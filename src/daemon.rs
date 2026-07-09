use anyhow::{anyhow, Context, Result};
use daemonize::Daemonize;
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use crate::cli::{BroadcastScope, SplitDirection};
use crate::model::{
    acknowledge_done_status, direction_axis, infer_agent_status, insert_pane_in_layout,
    merge_agent_status, next_pane_in_layout, remove_pane_from_layout, resize_layout,
    touch_agent_status, unix_time, AgentStatus, ClipboardItem, DaemonInfo, EventRecord,
    ListeningPort, Notification, Pane, PaneStatus, PaneTab, PullRequestInfo, Session, SurfaceKind,
};
use crate::paths;
use crate::protocol::{self, PaneSize, Request, Response};

/// Extension trait for locking a [`Mutex`] while tolerating poisoning.
trait MutexExt<T> {
    fn lock_or_recover(&self) -> MutexGuard<'_, T>;
}

impl<T> MutexExt<T> for Mutex<T> {
    /// Lock the mutex, recovering the guard even if a previous holder panicked
    /// and poisoned it (`unwrap_or_else(|p| p.into_inner())`).
    ///
    /// Recovering is correct here because the guarded structures (session
    /// state, the pane runtime map, workspace/metadata caches, etc.) remain
    /// structurally valid after a panic: a panic mid-update can at worst leave
    /// stale or partial data. That is strictly better than the alternative of
    /// `lock().unwrap()`, where a single panicking connection/pane thread would
    /// poison the mutex and cascade into every later lock panicking too,
    /// crashing the daemon and killing every user's session.
    fn lock_or_recover(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

pub fn ensure_running(session: &str) -> Result<()> {
    if is_running(session) {
        return Ok(());
    }

    if let Err(err) = start_detached(session) {
        if wait_until_running(session, Duration::from_secs(3)) {
            return Ok(());
        }
        return Err(err);
    }

    if wait_until_running(session, Duration::from_secs(3)) {
        return Ok(());
    }

    Err(anyhow!("vmux daemon did not start"))
}

pub fn start_detached(session: &str) -> Result<()> {
    if std::env::var_os("VMUX_DAEMONIZE").is_some() {
        return daemonize_current_process(session);
    }

    if is_running(session) {
        println!(
            "vmux daemon already running at {}",
            paths::socket_path(session)?.display()
        );
        return Ok(());
    }

    cleanup_stale_runtime(session)?;
    // Capture the client's cwd *before* the daemon chdirs to `/` so new panes
    // open in the directory the user was in when they ran `vmux`.
    let launch_cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut child = Command::new(std::env::current_exe().context("resolve vmux executable")?)
        .arg("--session")
        .arg(session)
        .arg("daemon")
        .env("VMUX_DAEMONIZE", "1")
        .env("VMUX_LAUNCH_CWD", &launch_cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn vmux daemon helper")?;

    if wait_until_running(session, Duration::from_secs(3)) {
        return Ok(());
    }

    if let Some(status) = child.try_wait().ok().flatten() {
        return Err(anyhow!("vmux daemon helper exited with {status}"));
    }

    Err(anyhow!("vmux daemon did not start"))
}

fn daemonize_current_process(session: &str) -> Result<()> {
    let pid_path = paths::pid_path(session)?;
    cleanup_stale_runtime(session)?;
    // Preserve launch cwd across daemonize (which chdirs to `/`).
    if std::env::var_os("VMUX_LAUNCH_CWD").is_none() {
        if let Ok(cwd) = std::env::current_dir() {
            std::env::set_var("VMUX_LAUNCH_CWD", cwd);
        }
    }
    let log_path = paths::log_path(session)?;
    let stdout = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("open daemon log {}", log_path.display()))?;
    let stderr = stdout
        .try_clone()
        .with_context(|| format!("clone daemon log {}", log_path.display()))?;
    let daemon = Daemonize::new()
        .working_directory("/")
        .umask(0o077)
        .pid_file(&pid_path)
        .stdout(stdout)
        .stderr(stderr);
    if let Err(err) = daemon.start() {
        if wait_until_running(session, Duration::from_secs(3)) {
            return Ok(());
        }
        return Err(err).context("daemonize vmux");
    }
    ignore_hangup_signal()?;
    let err = serve_foreground(session).err();
    if let Some(err) = err {
        eprintln!("{err:#}");
    }
    std::process::exit(0);

    #[allow(unreachable_code)]
    {
        Ok(())
    }
}

#[cfg(unix)]
fn ignore_hangup_signal() -> Result<()> {
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = libc::SIG_IGN;
        action.sa_flags = 0;
        libc::sigemptyset(&mut action.sa_mask);
        if libc::sigaction(libc::SIGHUP, &action, std::ptr::null_mut()) != 0 {
            return Err(std::io::Error::last_os_error()).context("ignore SIGHUP");
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn ignore_hangup_signal() -> Result<()> {
    Ok(())
}

pub fn serve_foreground(session: &str) -> Result<()> {
    let server = Arc::new(Server::load(session)?);
    server.restore_saved_panes()?;
    server.ensure_initial_pane()?;
    server.save()?;
    server.serve()
}

pub fn is_running(session: &str) -> bool {
    let Ok(path) = paths::socket_path(session) else {
        return false;
    };
    if !path.exists() {
        return false;
    }
    protocol::request(&path, &Request::Ping)
        .map(|response| response.ok)
        .unwrap_or(false)
}

fn wait_until_running(session: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if is_running(session) {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    is_running(session)
}

pub fn terminate_pid(pid: u32) -> Result<()> {
    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        const SIGTERM: i32 = 15;
        let rc = unsafe { kill(pid as i32, SIGTERM) };
        if rc == 0 {
            Ok(())
        } else {
            Err(anyhow!("failed to signal pid {pid}"))
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        Err(anyhow!("pid signalling is only supported on Unix"))
    }
}

fn cleanup_stale_runtime(session: &str) -> Result<()> {
    let pid_path = paths::pid_path(session)?;
    let socket_path = paths::socket_path(session)?;
    let stale_pid = paths::read_pid_file(&pid_path)
        .map(|pid| !paths::process_exists(pid))
        .unwrap_or(pid_path.exists());
    if stale_pid {
        fs::remove_file(&pid_path).ok();
    }
    if socket_path.exists() && !is_running(session) {
        fs::remove_file(&socket_path).ok();
    }
    Ok(())
}

/// Maximum bytes of decoded terminal output retained per pane for scrollback.
const SCROLLBACK_CAP: usize = 16_000;

struct PaneRuntime {
    generation: u64,
    pane: Pane,
    // PTY OS handles. Kept in `Option` so they can be dropped the moment the
    // child exits (finding 4): a short-lived pane would otherwise pin its
    // master/writer/killer file descriptors in the map until an explicit
    // kill/prune, leaking FDs. The captured `parser`/`output` below survive so
    // the UI can still read an exited pane's final screen and scrollback.
    master: Option<Box<dyn MasterPty + Send>>,
    // Wrapped in its own lock so blocking PTY writes never hold the global
    // `panes` mutex (a stalled child draining stdin would otherwise wedge
    // append_output/snapshot for every pane).
    writer: Option<Arc<Mutex<Box<dyn Write + Send>>>>,
    killer: Option<Box<dyn ChildKiller + Send + Sync>>,
    // Recent decoded output chunks. Bounded by `output_bytes` to SCROLLBACK_CAP
    // so materializing the joined scrollback stays O(cap) rather than O(total).
    output: VecDeque<String>,
    output_bytes: usize,
    // Bytes of an incomplete trailing UTF-8 sequence carried into the next read.
    pending: Vec<u8>,
    // Accumulated decoded text used to detect OSC sequences that straddle reads.
    osc_tail: String,
    parser: vt100::Parser,
    size: PaneSize,
    // Bumped by append_output whenever new bytes are processed. Used to skip the
    // scrollback-formatted walk in snapshot() when a pane produced no output
    // since the last snapshot.
    output_generation: u64,
    // Cached styled scrollback (see screen_scrollback_formatted) plus the
    // output_generation it was built from. `u64::MAX` forces the first build.
    scrollback_formatted_cache: String,
    scrollback_formatted_generation: u64,
}

impl PaneRuntime {
    /// Join the retained output chunks (already bounded to ~SCROLLBACK_CAP).
    fn joined_output(&self) -> String {
        self.output.iter().cloned().collect()
    }

    /// Push a decoded chunk, tracking the running byte total and evicting the
    /// oldest chunks once the cap is exceeded.
    fn push_output(&mut self, text: String) {
        push_bounded_output(&mut self.output, &mut self.output_bytes, text);
    }
}

/// Append `text` to a byte-bounded output deque, evicting the oldest chunks
/// while the running total exceeds SCROLLBACK_CAP (the most recent chunk is
/// always kept). Keeps materializing the joined scrollback O(cap).
fn push_bounded_output(output: &mut VecDeque<String>, output_bytes: &mut usize, text: String) {
    if text.is_empty() {
        return;
    }
    *output_bytes += text.len();
    output.push_back(text);
    while *output_bytes > SCROLLBACK_CAP && output.len() > 1 {
        if let Some(front) = output.pop_front() {
            *output_bytes -= front.len();
        }
    }
}

/// Cached per-workspace metadata populated by a background thread so the
/// snapshot hot path never spawns git/gh/ss subprocesses.
#[derive(Clone, Default)]
struct WorkspaceMeta {
    git_branch: Option<String>,
    pull_request: Option<PullRequestInfo>,
    ports: Vec<ListeningPort>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
struct CustomAction {
    name: String,
    command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    direction: Option<SplitDirection>,
}

#[derive(Debug, serde::Deserialize)]
struct LmuxConfig {
    #[serde(default)]
    commands: Vec<CustomAction>,
}

struct Server {
    session_name: String,
    socket_path: std::path::PathBuf,
    state_path: std::path::PathBuf,
    pid_path: std::path::PathBuf,
    log_path: std::path::PathBuf,
    started_at: u64,
    // LOCK ORDER: never hold `session` and `panes` at the same time. When a
    // handler needs both, take one, collect what it needs, release it, then take
    // the other (PTY reader threads do panes->release->session; request handlers
    // do session->release->panes). Nesting the two locks in opposite orders on
    // different threads would deadlock, and the `save`/`snapshot` mutex below
    // must likewise only be entered while no data lock is held.
    session: Mutex<Session>,
    panes: Mutex<BTreeMap<String, PaneRuntime>>,
    workspace_meta: Mutex<BTreeMap<String, WorkspaceMeta>>,
    next_pane: Mutex<u64>,
    next_workspace: Mutex<u64>,
    next_runtime: Mutex<u64>,
    // Serializes state-file writes so concurrent handlers can't interleave
    // writes into a shared temp file or race the rename (finding 11).
    save_lock: Mutex<u64>,
    // Only one attached UI should drive PTY dimensions at a time. Without this,
    // a small phone terminal and a large desktop terminal can resize the same
    // panes back and forth on every refresh.
    pane_size_owner: Mutex<Option<String>>,
}

impl Server {
    fn load(session_name: &str) -> Result<Self> {
        let state_path = paths::state_path(session_name)?;
        let session = if state_path.exists() {
            let contents = fs::read_to_string(&state_path)
                .with_context(|| format!("read state {}", state_path.display()))?;
            match serde_json::from_str::<Session>(&contents) {
                Ok(mut session) => {
                    for pane in session.panes.values_mut() {
                        if !matches!(pane.status, PaneStatus::Exited) {
                            pane.status = PaneStatus::Restored;
                            pane.pid = None;
                        }
                    }
                    session.ensure_workspace();
                    // Workspace → Tab → Pane hierarchy (and collapse legacy
                    // per-pane tabs onto the pane surface).
                    session.migrate_hierarchy();
                    // Older daemons stored cwd `/` after chdir-on-daemonize;
                    // re-home those to the user's launch directory.
                    repair_workspace_cwds(&mut session);
                    for workspace in &mut session.workspaces {
                        workspace.ensure_layout();
                    }
                    session
                }
                Err(err) => {
                    // A corrupt or hand-edited state file must not brick the
                    // daemon. Preserve it for debugging and start fresh.
                    let backup = state_path.with_extension(format!("json.corrupt.{}", unix_time()));
                    eprintln!(
                        "vmux: failed to parse state {}: {err}; backing up to {} and starting fresh",
                        state_path.display(),
                        backup.display()
                    );
                    if let Err(rename_err) = fs::rename(&state_path, &backup) {
                        eprintln!(
                            "vmux: could not back up corrupt state {}: {rename_err}",
                            state_path.display()
                        );
                    }
                    Session::new(session_name)
                }
            }
        } else {
            Session::new(session_name)
        };

        let next_pane = next_number("pane-", session.panes.keys().map(String::as_str));
        let next_workspace = next_number(
            "ws-",
            session
                .workspaces
                .iter()
                .map(|workspace| workspace.id.as_str()),
        );

        Ok(Self {
            session_name: session_name.to_string(),
            socket_path: paths::socket_path(session_name)?,
            state_path,
            pid_path: paths::pid_path(session_name)?,
            log_path: paths::log_path(session_name)?,
            started_at: unix_time(),
            session: Mutex::new(session),
            panes: Mutex::new(BTreeMap::new()),
            workspace_meta: Mutex::new(BTreeMap::new()),
            next_pane: Mutex::new(next_pane),
            next_workspace: Mutex::new(next_workspace),
            next_runtime: Mutex::new(1),
            save_lock: Mutex::new(0),
            pane_size_owner: Mutex::new(None),
        })
    }

    fn serve(self: Arc<Self>) -> Result<()> {
        self.write_pid_file().ok();
        self.log("daemon starting").ok();
        {
            // Refresh git/gh/ss-derived workspace metadata off the request path
            // so the snapshot hot loop only ever reads cached values.
            let server = Arc::clone(&self);
            thread::spawn(move || server.refresh_workspace_meta_loop());
        }
        if self.socket_path.exists() {
            fs::remove_file(&self.socket_path).ok();
        }
        let listener = UnixListener::bind(&self.socket_path)
            .with_context(|| format!("bind {}", self.socket_path.display()))?;
        fs::set_permissions(
            &self.socket_path,
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        )
        .ok();

        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let server = Arc::clone(&self);
                    thread::spawn(move || {
                        let response = server.handle_stream(stream);
                        if let Err(err) = response {
                            eprintln!("vmux connection error: {err:#}");
                        }
                    });
                }
                Err(err) => eprintln!("vmux accept error: {err}"),
            }
        }
        Ok(())
    }

    fn refresh_workspace_meta_loop(self: Arc<Self>) {
        loop {
            self.refresh_workspace_meta();
            thread::sleep(Duration::from_secs(5));
        }
    }

    /// Recompute cached git/gh/ss metadata for every workspace. Runs only on
    /// the background thread and never holds `panes`/`session` while spawning
    /// subprocesses.
    fn refresh_workspace_meta(&self) {
        let workspaces = {
            let session = self.session.lock_or_recover();
            session
                .workspaces
                .iter()
                .map(|workspace| {
                    (
                        workspace.id.clone(),
                        workspace.cwd.clone(),
                        workspace.panes.clone(),
                    )
                })
                .collect::<Vec<_>>()
        };
        let pane_pids = {
            let panes = self.panes.lock_or_recover();
            panes
                .values()
                .filter_map(|runtime| runtime.pane.pid.map(|pid| (runtime.pane.id.clone(), pid)))
                .collect::<BTreeMap<_, _>>()
        };
        let mut updated = BTreeMap::new();
        for (id, cwd, pane_ids) in workspaces {
            let path = Path::new(&cwd);
            let roots = pane_ids
                .iter()
                .filter_map(|pane| pane_pids.get(pane).copied())
                .collect::<Vec<_>>();
            updated.insert(
                id,
                WorkspaceMeta {
                    git_branch: git_branch(path),
                    pull_request: pull_request_info(path),
                    ports: listening_ports_for_roots(&roots),
                },
            );
        }
        *self.workspace_meta.lock_or_recover() = updated;
    }

    /// Kick off a one-shot metadata refresh on a detached thread so a freshly
    /// created or re-homed workspace shows its git branch / PR without waiting
    /// for the next 5s background tick. Runs off the request thread and holds no
    /// session/panes lock while spawning git/gh/ss (which are timeout-bounded).
    fn nudge_workspace_meta(self: &Arc<Self>) {
        let server = Arc::clone(self);
        thread::spawn(move || server.refresh_workspace_meta());
    }

    fn handle_stream(self: Arc<Self>, stream: UnixStream) -> Result<()> {
        // Cap the request line so a misbehaving or hostile client cannot make
        // the daemon buffer an unbounded amount of memory.
        const MAX_REQUEST_BYTES: u64 = 16 * 1024 * 1024;
        let mut limited = stream.try_clone()?.take(MAX_REQUEST_BYTES);
        let mut line = String::new();
        BufReader::new(&mut limited).read_line(&mut line)?;
        let over_limit = limited.limit() == 0 && !line.ends_with('\n');
        let response = if over_limit {
            Response::err("decode request: request exceeds maximum size")
        } else {
            match serde_json::from_str::<Request>(&line) {
                Ok(request) => self.dispatch(request).unwrap_or_else(Response::err),
                Err(err) => Response::err(format!("decode request: {err}")),
            }
        };
        protocol::write_response(&stream, &response)
    }

    fn dispatch(self: &Arc<Self>, request: Request) -> Result<Response> {
        match request {
            Request::Ping => Ok(Response::ok(
                serde_json::json!({ "session": self.session_name, "daemon": self.daemon_info() }),
            )),
            Request::Snapshot => Ok(Response::ok(self.snapshot(true)?)),
            Request::List => Ok(Response::ok(self.snapshot(false)?)),
            Request::Agents => Ok(Response::ok(self.agent_summary()?)),
            Request::Identify { pane } => {
                let data = self.identify(pane)?;
                Ok(Response::ok(data))
            }
            Request::NewPane {
                direction,
                command,
                title,
                workspace,
                surface_kind,
            } => {
                let pane = self.new_pane(direction, command, title, workspace, surface_kind)?;
                Ok(Response::ok(pane))
            }
            Request::DuplicatePane { pane, direction } => {
                let pane = self.duplicate_pane(pane, direction)?;
                Ok(Response::ok(pane))
            }
            Request::OpenUrl {
                url,
                direction,
                title,
                workspace,
            } => {
                let pane = self.open_url(url, direction, title, workspace)?;
                Ok(Response::ok(pane))
            }
            Request::UrlSnapshot { url } => {
                let data = url_snapshot(&url)?;
                Ok(Response::ok(data))
            }
            Request::UrlLinks { url } => {
                let data = url_links(&url)?;
                Ok(Response::ok(data))
            }
            Request::UrlForms { url } => {
                let data = url_forms(&url)?;
                Ok(Response::ok(data))
            }
            Request::UrlEvaluate { url, expression } => {
                let data = url_evaluate(&url, &expression)?;
                Ok(Response::ok(data))
            }
            Request::UrlConsole { url } => {
                let data = url_console(&url)?;
                Ok(Response::ok(data))
            }
            Request::UrlNetwork { url } => {
                let data = url_network(&url)?;
                Ok(Response::ok(data))
            }
            Request::OpenUrlLink {
                url,
                index,
                direction,
                title,
                workspace,
            } => {
                let data = self.open_url_link(url, index, direction, title, workspace)?;
                Ok(Response::ok(data))
            }
            Request::SubmitForm {
                url,
                index,
                fields,
                direction,
                title,
                workspace,
            } => {
                let data = self.submit_form(url, index, fields, direction, title, workspace)?;
                Ok(Response::ok(data))
            }
            Request::CustomActions { workspace } => {
                let data = self.custom_actions(workspace)?;
                Ok(Response::ok(data))
            }
            Request::RunCustomAction { name, workspace } => {
                let data = self.run_custom_action(name, workspace)?;
                Ok(Response::ok(data))
            }
            Request::KillPane { pane } => {
                let pane = self.kill_pane(pane)?;
                Ok(Response::ok(pane))
            }
            Request::Prune { workspace, all } => {
                let data = self.prune_exited(workspace, all)?;
                Ok(Response::ok(data))
            }
            Request::RestartPane {
                pane,
                workspace,
                all,
                command,
            } => {
                let data = self.restart_panes(pane, workspace, all, command)?;
                Ok(Response::ok(data))
            }
            Request::MovePane {
                pane,
                workspace,
                direction,
            } => {
                let workspace = self.move_pane(pane, workspace, direction)?;
                Ok(Response::ok(workspace))
            }
            Request::SwapPanes { first, second } => {
                let mut session = self.session.lock_or_recover();
                let workspace = session
                    .swap_panes(&first, &second)
                    .map_err(anyhow::Error::msg)?;
                drop(session);
                self.save()?;
                Ok(Response::ok(workspace))
            }
            Request::SetPaneTitle { pane, title } => {
                let pane = self.set_pane_title(pane, title)?;
                Ok(Response::ok(pane))
            }
            Request::SetPaneMetadata { pane, key, value } => {
                let data = self.set_pane_metadata(pane, key, value)?;
                Ok(Response::ok(data))
            }
            Request::ListTabs { workspace } => {
                let data = self.list_tabs(workspace)?;
                Ok(Response::ok(data))
            }
            Request::NewTab {
                workspace,
                title,
                command,
            } => {
                let data = self.new_tab(workspace, title, command)?;
                Ok(Response::ok(data))
            }
            Request::SwitchTab { workspace, tab } => {
                let data = self.switch_tab(workspace, tab)?;
                Ok(Response::ok(data))
            }
            Request::RenameTab {
                workspace,
                tab,
                title,
            } => {
                let data = self.rename_tab(workspace, tab, title)?;
                Ok(Response::ok(data))
            }
            Request::CloseTab { workspace, tab } => {
                let data = self.close_tab(workspace, tab)?;
                Ok(Response::ok(data))
            }
            Request::MovePaneInLayout { pane, direction } => {
                let data = self.move_pane_in_layout(pane, direction)?;
                Ok(Response::ok(data))
            }
            Request::PaneTabs { .. }
            | Request::AddPaneTab { .. }
            | Request::SwitchPaneTab { .. }
            | Request::RenamePaneTab { .. }
            | Request::ClosePaneTab { .. } => Err(anyhow!(
                "per-pane tabs were removed; use workspace tabs \
                 (vmux tab list|new|switch|rename|close)"
            )),
            Request::WaitPane {
                pane,
                workspace,
                all,
                timeout_ms,
            } => {
                let data = self.wait_panes(pane, workspace, all, timeout_ms)?;
                Ok(Response::ok(data))
            }
            Request::NewWorkspace { name, cwd } => {
                let cwd = normalize_cwd(cwd)?;
                let workspace = {
                    let mut session = self.session.lock_or_recover();
                    let mut next = self.next_workspace.lock_or_recover();
                    // git_branch/pull_request are populated by the background
                    // meta loop (see nudge below). Computing them here would run
                    // `git`/`gh` (an unbounded network call) while holding the
                    // session mutex, freezing the attached UI (finding: creation
                    // lag). Start empty; the UI renders empty branch/PR fine.
                    let mut workspace = crate::model::Workspace::new(format!("ws-{next}"), name);
                    workspace.cwd = cwd.display().to_string();
                    *next += 1;
                    session.active_workspace = workspace.id.clone();
                    session.workspaces.push(workspace.clone());
                    workspace
                };
                self.save()?;
                // Refresh branch/PR/ports off the request thread so they appear
                // promptly instead of waiting up to 5s for the next meta tick.
                self.nudge_workspace_meta();
                Ok(Response::ok(workspace))
            }
            Request::SwitchWorkspace { workspace } => {
                let pane_ids = {
                    let mut session = self.session.lock_or_recover();
                    let workspace = session
                        .resolve_workspace_selector(&workspace)
                        .map_err(anyhow::Error::msg)?;
                    session.active_workspace = workspace.clone();
                    // User looked at this workspace → clear finished ✅ markers.
                    session
                        .workspaces
                        .iter()
                        .find(|w| w.id == workspace)
                        .map(|w| {
                            w.all_pane_ids()
                                .into_iter()
                                .map(|id| id.to_string())
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default()
                };
                self.acknowledge_done_panes(&pane_ids);
                self.save()?;
                Ok(Response::empty())
            }
            Request::RenameWorkspace { workspace, name } => {
                let mut session = self.session.lock_or_recover();
                let workspace = session
                    .resolve_workspace_selector(&workspace)
                    .map_err(anyhow::Error::msg)?;
                let Some(target) = session
                    .workspaces
                    .iter_mut()
                    .find(|item| item.id == workspace)
                else {
                    return Err(anyhow!("unknown workspace {workspace}"));
                };
                target.name = name;
                let target = target.clone();
                drop(session);
                self.save()?;
                Ok(Response::ok(target))
            }
            Request::CloseWorkspace { workspace } => {
                let closed = self.close_workspace(workspace)?;
                Ok(Response::ok(closed))
            }
            Request::SetWorkspaceCwd { workspace, cwd } => {
                let cwd = normalize_cwd(Some(cwd))?;
                let mut session = self.session.lock_or_recover();
                let workspace = session
                    .resolve_workspace_selector(&workspace)
                    .map_err(anyhow::Error::msg)?;
                let Some(target) = session
                    .workspaces
                    .iter_mut()
                    .find(|item| item.id == workspace)
                else {
                    return Err(anyhow!("unknown workspace {workspace}"));
                };
                target.cwd = cwd.display().to_string();
                // Drop the stale branch/PR immediately; the background meta loop
                // (nudged below) recomputes them without running git/gh under the
                // session mutex (which would freeze the attached UI).
                target.git_branch = None;
                target.pull_request = None;
                let target = target.clone();
                drop(session);
                self.save()?;
                self.nudge_workspace_meta();
                Ok(Response::ok(target))
            }
            Request::SetWorkspacePinned { workspace, pinned } => {
                let mut session = self.session.lock_or_recover();
                // Accept a workspace name OR id (finding 10).
                let id = session
                    .resolve_workspace_selector(&workspace)
                    .map_err(anyhow::Error::msg)?;
                let target = session
                    .set_workspace_pinned(&id, pinned)
                    .map_err(anyhow::Error::msg)?;
                drop(session);
                self.save()?;
                Ok(Response::ok(target))
            }
            Request::MoveWorkspace {
                workspace,
                position,
            } => {
                let mut session = self.session.lock_or_recover();
                // Accept a workspace name OR id (finding 10).
                let id = session
                    .resolve_workspace_selector(&workspace)
                    .map_err(anyhow::Error::msg)?;
                let target = session
                    .move_workspace(&id, position)
                    .map_err(anyhow::Error::msg)?;
                drop(session);
                self.save()?;
                Ok(Response::ok(target))
            }
            Request::FocusPane { pane } => {
                let mut session = self.session.lock_or_recover();
                let workspace = session.active_workspace_mut();
                if !workspace.panes.iter().any(|item| item == &pane) {
                    return Err(anyhow!("pane {pane} is not in active workspace"));
                }
                workspace.active_pane = Some(pane.clone());
                drop(session);
                // Clicking a pane acknowledges its finished ✅.
                self.acknowledge_done_panes(&[pane]);
                self.save()?;
                Ok(Response::empty())
            }
            Request::FocusDirection { direction } => {
                let pane = self.focus_direction(direction)?;
                self.acknowledge_done_panes(std::slice::from_ref(&pane));
                Ok(Response::ok(pane))
            }
            Request::ToggleZoom { pane } => {
                let data = self.toggle_zoom(pane)?;
                Ok(Response::ok(data))
            }
            Request::Resize { direction, amount } => {
                self.resize_active(direction, amount)?;
                Ok(Response::empty())
            }
            Request::Input { pane, data } => {
                self.write_input(pane, data)?;
                Ok(Response::empty())
            }
            Request::SendKey { pane, keys } => {
                let data = crate::input::encode_key_specs(&keys)?;
                self.write_input(pane, data)?;
                Ok(Response::empty())
            }
            Request::Broadcast { scope, data } => {
                let data = self.broadcast_input(scope, data)?;
                Ok(Response::ok(data))
            }
            Request::Notify {
                pane,
                workspace,
                status,
                color,
                clear,
                message,
            } => {
                let note = self.notify(pane, workspace, status, color, clear, message)?;
                Ok(Response::ok(note))
            }
            Request::Notifications { limit } => {
                let data = self.notifications(limit)?;
                Ok(Response::ok(data))
            }
            Request::Events { limit } => {
                let data = self.events(limit)?;
                Ok(Response::ok(data))
            }
            Request::ClearNotifications => {
                let data = self.clear_notifications()?;
                Ok(Response::ok(data))
            }
            Request::JumpNotification => {
                let data = self.jump_notification()?;
                Ok(Response::ok(data))
            }
            Request::Progress { pane, value } => {
                let pane = self.set_progress(pane, value)?;
                Ok(Response::ok(pane))
            }
            Request::ReadScreen {
                pane,
                scrollback,
                limit_bytes,
            } => {
                let data = self.read_screen(pane, scrollback, limit_bytes)?;
                Ok(Response::ok(data))
            }
            Request::Search { pane, query } => {
                let data = self.search_pane(pane, query)?;
                Ok(Response::ok(data))
            }
            Request::ClearPane { pane } => {
                let data = self.clear_pane_capture(pane)?;
                Ok(Response::ok(data))
            }
            Request::CopyPane {
                pane,
                scrollback,
                limit_bytes,
            } => {
                let data = self.copy_pane(pane, scrollback, limit_bytes)?;
                Ok(Response::ok(data))
            }
            Request::Paste { pane, enter } => {
                let data = self.paste_clipboard(pane, enter)?;
                Ok(Response::ok(data))
            }
            Request::Clipboard => {
                let data = self.clipboard()?;
                Ok(Response::ok(data))
            }
            Request::SetClipboard {
                text,
                source_pane,
                source,
            } => {
                let data = self.set_clipboard(text, source_pane, source)?;
                Ok(Response::ok(data))
            }
            Request::PaneSizes {
                panes,
                client_id,
                take_control,
            } => {
                self.resize_ptys(panes, client_id, take_control)?;
                Ok(Response::empty())
            }
            Request::Shutdown => {
                self.save()?;
                self.log("daemon shutting down").ok();
                self.cleanup_runtime_files();
                thread::spawn(|| {
                    thread::sleep(Duration::from_millis(50));
                    std::process::exit(0);
                });
                Ok(Response::empty())
            }
        }
    }

    fn ensure_initial_pane(self: &Arc<Self>) -> Result<()> {
        let needs_pane = self
            .session
            .lock_or_recover()
            .active_workspace_mut()
            .panes
            .is_empty();
        if needs_pane {
            self.new_pane(SplitDirection::Right, default_shell(), None, None, None)?;
        }
        Ok(())
    }

    fn restore_saved_panes(self: &Arc<Self>) -> Result<Vec<String>> {
        let targets = {
            let session = self.session.lock_or_recover();
            session
                .workspaces
                .iter()
                .flat_map(|workspace| {
                    workspace.panes.iter().filter_map(|pane_id| {
                        let pane = session.panes.get(pane_id)?;
                        if matches!(pane.status, PaneStatus::Restored) {
                            Some((
                                pane_id.clone(),
                                workspace.id.clone(),
                                PathBuf::from(&workspace.cwd),
                            ))
                        } else {
                            None
                        }
                    })
                })
                .collect::<Vec<_>>()
        };

        let mut restored = Vec::new();
        for (pane_id, workspace_id, cwd) in targets {
            if self.panes.lock_or_recover().contains_key(&pane_id) {
                continue;
            }
            let mut pane = {
                let session = self.session.lock_or_recover();
                session
                    .panes
                    .get(&pane_id)
                    .cloned()
                    .ok_or_else(|| anyhow!("unknown restored pane {pane_id}"))?
            };
            let old_output = pane.output.clone();
            let old_scrollback = pane.scrollback.clone();
            let old_scrollback_formatted = pane.scrollback_formatted.clone();
            let cwd = match normalize_cwd(Some(cwd.display().to_string())) {
                Ok(cwd) => cwd,
                Err(err) => {
                    self.log(&format!("restore pane {pane_id} skipped: {err:#}"))
                        .ok();
                    continue;
                }
            };
            if let Err(err) = self.start_pane_runtime(&mut pane, cwd, &workspace_id) {
                self.log(&format!("restore pane {pane_id} failed: {err:#}"))
                    .ok();
                continue;
            }
            pane.output = old_output;
            pane.scrollback = old_scrollback;
            pane.scrollback_formatted = old_scrollback_formatted;
            {
                let mut session = self.session.lock_or_recover();
                session.panes.insert(pane_id.clone(), pane);
            }
            restored.push(pane_id);
        }
        Ok(restored)
    }

    fn new_pane(
        self: &Arc<Self>,
        direction: SplitDirection,
        command: String,
        title: Option<String>,
        workspace: Option<String>,
        surface_kind: Option<SurfaceKind>,
    ) -> Result<Pane> {
        let command = if command.trim().is_empty() {
            default_shell()
        } else {
            command
        };
        let (target_workspace, cwd) = self.target_workspace_and_cwd(workspace)?;
        let mut next = self.next_pane.lock_or_recover();
        let id = format!("pane-{next}");
        *next += 1;
        drop(next);

        let mut pane = Pane::new(id.clone(), command.clone(), direction);
        if let Some(surface_kind) = surface_kind {
            pane.surface_kind = surface_kind;
        }
        if let Some(title) = normalize_pane_title(title)? {
            pane.title = title;
        }
        self.start_pane_runtime(&mut pane, cwd, &target_workspace)?;

        {
            let mut session = self.session.lock_or_recover();
            session.panes.insert(id.clone(), pane.clone());
            let workspace = session
                .workspaces
                .iter_mut()
                .find(|workspace| workspace.id == target_workspace)
                .ok_or_else(|| anyhow!("unknown workspace {target_workspace}"))?;
            workspace.ensure_layout();
            let active_before = workspace.active_pane.clone();
            workspace.panes.push(id.clone());
            workspace.layout = Some(insert_pane_in_layout(
                workspace.layout.take(),
                active_before.as_deref(),
                id.clone(),
                direction,
            ));
            workspace.active_pane = Some(id);
        }
        self.save()?;
        Ok(pane)
    }

    fn duplicate_pane(
        self: &Arc<Self>,
        pane: Option<String>,
        direction: SplitDirection,
    ) -> Result<Pane> {
        let pane_id = self.resolve_pane(pane)?;
        let (command, title, workspace, surface_kind) = {
            let session = self.session.lock_or_recover();
            let pane = session
                .panes
                .get(&pane_id)
                .ok_or_else(|| anyhow!("unknown pane {pane_id}"))?;
            let workspace = session
                .workspaces
                .iter()
                .find(|workspace| workspace.panes.iter().any(|item| item == &pane_id))
                .ok_or_else(|| anyhow!("pane {pane_id} is not attached to a workspace"))?;
            (
                pane.command.clone(),
                Some(pane.title.clone()),
                workspace.id.clone(),
                pane.surface_kind.clone(),
            )
        };
        self.new_pane(
            direction,
            command,
            title,
            Some(workspace),
            Some(surface_kind),
        )
    }

    fn open_url(
        self: &Arc<Self>,
        url: String,
        direction: SplitDirection,
        title: Option<String>,
        workspace: Option<String>,
    ) -> Result<Pane> {
        validate_url(&url)?;
        let command = url_open_command(&url);
        let title = title.or_else(|| Some(format!("url:{}", compact_url_title(&url))));
        self.new_pane(
            direction,
            command,
            title,
            workspace,
            Some(SurfaceKind::Browser),
        )
    }

    fn open_url_link(
        self: &Arc<Self>,
        url: String,
        index: usize,
        direction: SplitDirection,
        title: Option<String>,
        workspace: Option<String>,
    ) -> Result<serde_json::Value> {
        if index == 0 {
            return Err(anyhow!("link index is 1-based"));
        }
        let snapshot = url_links(&url)?;
        let links = snapshot
            .get("links")
            .and_then(|links| links.as_array())
            .ok_or_else(|| anyhow!("url links response did not include links"))?;
        let link = links
            .get(index - 1)
            .cloned()
            .ok_or_else(|| anyhow!("link index {index} out of range"))?;
        let href = link
            .get("href")
            .and_then(|href| href.as_str())
            .ok_or_else(|| anyhow!("link {index} has no href"))?
            .to_string();
        let pane = self.open_url(href.clone(), direction, title, workspace)?;
        Ok(serde_json::json!({
            "source_url": url,
            "index": index,
            "href": href,
            "link": link,
            "pane": pane,
        }))
    }

    fn submit_form(
        self: &Arc<Self>,
        url: String,
        index: usize,
        fields: BTreeMap<String, String>,
        direction: SplitDirection,
        title: Option<String>,
        workspace: Option<String>,
    ) -> Result<serde_json::Value> {
        if index == 0 {
            return Err(anyhow!("form index is 1-based"));
        }
        let forms_data = url_forms(&url)?;
        let forms = forms_data
            .get("forms")
            .and_then(|forms| forms.as_array())
            .ok_or_else(|| anyhow!("url forms response did not include forms"))?;
        let form = forms
            .get(index - 1)
            .cloned()
            .ok_or_else(|| anyhow!("form index {index} out of range"))?;
        let action = form
            .get("action")
            .and_then(|action| action.as_str())
            .ok_or_else(|| anyhow!("form {index} has no action"))?
            .to_string();
        let method = form
            .get("method")
            .and_then(|method| method.as_str())
            .unwrap_or("get")
            .to_ascii_lowercase();
        let mut values = form_default_fields(&form);
        values.extend(fields);
        let command_or_url = form_submission_target(&action, &method, &values)?;
        let title = title.or_else(|| Some(format!("form:{}", compact_url_title(&action))));
        let pane = if method == "get" {
            self.open_url(command_or_url.clone(), direction, title, workspace)?
        } else {
            self.new_pane(
                direction,
                command_or_url.clone(),
                title,
                workspace,
                Some(SurfaceKind::Browser),
            )?
        };
        Ok(serde_json::json!({
            "source_url": url,
            "index": index,
            "form": form,
            "method": method,
            "fields": values,
            "target": command_or_url,
            "pane": pane,
        }))
    }

    fn custom_actions(&self, workspace: Option<String>) -> Result<serde_json::Value> {
        let (workspace, cwd) = self.target_workspace_and_cwd(workspace)?;
        let loaded = load_custom_actions(&cwd)?;
        let (config_path, commands) = loaded
            .map(|(path, commands)| (Some(path.display().to_string()), commands))
            .unwrap_or_else(|| (None, Vec::new()));
        Ok(serde_json::json!({
            "workspace": workspace,
            "cwd": cwd,
            "config_path": config_path,
            "commands": commands,
        }))
    }

    fn run_custom_action(
        self: &Arc<Self>,
        name: String,
        workspace: Option<String>,
    ) -> Result<serde_json::Value> {
        if name.trim().is_empty() {
            return Err(anyhow!("action name cannot be empty"));
        }
        let (workspace, cwd) = self.target_workspace_and_cwd(workspace)?;
        let (config_path, commands) = load_custom_actions(&cwd)?
            .ok_or_else(|| anyhow!("no vmux.json or .vmux.json found from {}", cwd.display()))?;
        let action = commands
            .into_iter()
            .find(|action| action.name == name)
            .ok_or_else(|| anyhow!("unknown action {name}"))?;
        let pane = self.new_pane(
            action.direction.unwrap_or(SplitDirection::Right),
            action.command.clone(),
            action.title.clone(),
            Some(workspace.clone()),
            None,
        )?;
        Ok(serde_json::json!({
            "workspace": workspace,
            "cwd": cwd,
            "config_path": config_path,
            "action": action,
            "pane": pane,
        }))
    }

    fn start_pane_runtime(
        self: &Arc<Self>,
        pane: &mut Pane,
        cwd: PathBuf,
        workspace: &str,
    ) -> Result<()> {
        let generation = {
            let mut next = self.next_runtime.lock_or_recover();
            let generation = *next;
            *next += 1;
            generation
        };
        let mut argv =
            shell_words::split(&pane.command).unwrap_or_else(|_| vec![pane.command.clone()]);
        if argv.is_empty() {
            argv.push(default_shell());
        }

        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let mut builder = CommandBuilder::new(&argv[0]);
        for arg in argv.iter().skip(1) {
            builder.arg(arg);
        }
        builder.cwd(cwd);
        builder.env("VMUX_SESSION", &self.session_name);
        builder.env("VMUX_WORKSPACE_ID", workspace);
        builder.env("VMUX_PANE_ID", &pane.id);
        builder.env("VMUX_SURFACE_ID", &pane.id);
        builder.env("VMUX_SOCKET_PATH", self.socket_path.display().to_string());
        builder.env("VMUX_PID_PATH", self.pid_path.display().to_string());
        builder.env("VMUX_LOG_PATH", self.log_path.display().to_string());
        builder.env("VMUX_STATE_PATH", self.state_path.display().to_string());
        let mut child = pair.slave.spawn_command(builder)?;
        let child_pid = child.process_id();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let killer = child.clone_killer();
        let master = pair.master;
        pane.status = PaneStatus::Running;
        // Coding agents start as Busy (pinned) so the workspace sidebar shows
        // 🔄 until a Stop hook/CLI marks Done, or the process exits.
        if crate::model::is_coding_agent_command(&pane.command) {
            touch_agent_status(pane, AgentStatus::Busy, true);
        } else {
            touch_agent_status(pane, infer_agent_status("", &pane.command), false);
        }
        pane.exit_code = None;
        pane.pid = child_pid;
        pane.progress = None;
        pane.output.clear();
        pane.scrollback.clear();
        pane.scrollback_formatted.clear();
        pane.updated_at = unix_time();

        let pane_id = pane.id.clone();
        let runtime_key = active_runtime_key_for_pane(pane);

        // Insert the runtime BEFORE spawning the reader thread so early output
        // always finds a runtime in append_output (otherwise the first bytes a
        // fast-starting child emits are silently dropped).
        self.panes.lock_or_recover().insert(
            active_runtime_key_for_pane(pane),
            PaneRuntime {
                generation,
                pane: pane.clone(),
                master: Some(master),
                writer: Some(Arc::new(Mutex::new(writer))),
                killer: Some(killer),
                output: VecDeque::with_capacity(2048),
                output_bytes: 0,
                pending: Vec::new(),
                osc_tail: String::new(),
                parser: vt100::Parser::new(24, 80, 2000),
                size: PaneSize { rows: 24, cols: 80 },
                output_generation: 0,
                scrollback_formatted_cache: String::new(),
                scrollback_formatted_generation: u64::MAX,
            },
        );

        let output_server = Arc::clone(self);
        thread::spawn(move || {
            let mut buf = [0_u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        output_server.append_output(&runtime_key, &pane_id, generation, &buf[..n])
                    }
                    Err(_) => break,
                }
            }
            let exit_code = child.wait().ok().map(|status| status.exit_code() as i32);
            output_server.mark_exited(&runtime_key, &pane_id, generation, exit_code);
        });

        Ok(())
    }

    fn target_workspace_and_cwd(&self, workspace: Option<String>) -> Result<(String, PathBuf)> {
        let session = self.session.lock_or_recover();
        // Accept a workspace name OR id, matching switch/rename (finding 10).
        let target = match workspace {
            Some(selector) => session
                .resolve_workspace_selector(&selector)
                .map_err(anyhow::Error::msg)?,
            None => session.active_workspace.clone(),
        };
        let cwd = session
            .workspaces
            .iter()
            .find(|workspace| workspace.id == target)
            .map(|workspace| workspace.cwd.clone())
            .ok_or_else(|| anyhow!("unknown workspace {target}"))?;
        drop(session);
        Ok((target, normalize_cwd(Some(cwd))?))
    }

    fn resize_active(&self, direction: SplitDirection, amount: u16) -> Result<()> {
        let delta = match direction {
            SplitDirection::Right | SplitDirection::Down => amount.min(50) as i16,
            SplitDirection::Left | SplitDirection::Up => -(amount.min(50) as i16),
        };
        let axis = direction_axis(direction);
        let mut session = self.session.lock_or_recover();
        let workspace = session.active_workspace_mut();
        workspace.ensure_layout();
        if !resize_layout(&mut workspace.layout, axis, delta) {
            return Err(anyhow!("active workspace has no split to resize"));
        }
        drop(session);
        self.save()?;
        Ok(())
    }

    fn focus_direction(&self, direction: SplitDirection) -> Result<String> {
        let mut session = self.session.lock_or_recover();
        let workspace = session.active_workspace_mut();
        workspace.ensure_layout();
        let Some(next) = next_pane_in_layout(
            workspace.layout.as_ref(),
            &workspace.panes,
            workspace.active_pane.as_deref(),
            direction,
        ) else {
            return Err(anyhow!("active workspace has no panes"));
        };
        workspace.active_pane = Some(next.clone());
        drop(session);
        self.save()?;
        Ok(next)
    }

    fn toggle_zoom(&self, pane: Option<String>) -> Result<serde_json::Value> {
        let mut session = self.session.lock_or_recover();
        let workspace = session.active_workspace_mut();
        workspace.ensure_layout();
        let pane = pane
            .or_else(|| workspace.active_pane.clone())
            .ok_or_else(|| anyhow!("active workspace has no panes"))?;
        if !workspace.panes.iter().any(|item| item == &pane) {
            return Err(anyhow!("pane {pane} is not in active workspace"));
        }
        let zoomed = if workspace.zoomed_pane.as_deref() == Some(&pane) {
            workspace.zoomed_pane = None;
            false
        } else {
            workspace.active_pane = Some(pane.clone());
            workspace.zoomed_pane = Some(pane.clone());
            true
        };
        let workspace_id = workspace.id.clone();
        drop(session);
        self.save()?;
        Ok(serde_json::json!({
            "workspace": workspace_id,
            "pane": pane,
            "zoomed": zoomed,
        }))
    }

    fn close_workspace(&self, workspace: Option<String>) -> Result<crate::model::Workspace> {
        let closed = {
            let mut session = self.session.lock_or_recover();
            // Accept a workspace name OR id (finding 10).
            let target = match workspace {
                Some(selector) => Some(
                    session
                        .resolve_workspace_selector(&selector)
                        .map_err(anyhow::Error::msg)?,
                ),
                None => None,
            };
            session
                .close_workspace(target.as_deref())
                .map_err(anyhow::Error::msg)?
        };

        for pane in &closed.panes {
            self.remove_pane_runtimes(pane);
        }

        self.save()?;
        Ok(closed)
    }

    fn move_pane(
        &self,
        pane: Option<String>,
        workspace: String,
        direction: SplitDirection,
    ) -> Result<crate::model::Workspace> {
        let pane = self.resolve_pane(pane)?;
        let moved = {
            let mut session = self.session.lock_or_recover();
            // Accept a workspace name OR id (finding 10).
            let target = session
                .resolve_workspace_selector(&workspace)
                .map_err(anyhow::Error::msg)?;
            session
                .move_pane(&pane, &target, direction)
                .map_err(anyhow::Error::msg)?
        };
        self.save()?;
        Ok(moved)
    }

    fn prune_exited(&self, workspace: Option<String>, all: bool) -> Result<serde_json::Value> {
        // Accept a workspace name OR id (finding 10).
        let workspace = if all {
            None
        } else if let Some(selector) = workspace {
            let session = self.session.lock_or_recover();
            Some(
                session
                    .resolve_workspace_selector(&selector)
                    .map_err(anyhow::Error::msg)?,
            )
        } else {
            Some(self.session.lock_or_recover().active_workspace.clone())
        };
        let removed = {
            let mut session = self.session.lock_or_recover();
            session
                .prune_exited_panes(workspace.as_deref())
                .map_err(anyhow::Error::msg)?
        };
        for pane in &removed {
            self.remove_pane_runtimes(&pane.id);
        }
        self.save()?;
        Ok(serde_json::json!({
            "removed": removed,
        }))
    }

    fn restart_panes(
        self: &Arc<Self>,
        pane: Option<String>,
        workspace: Option<String>,
        all: bool,
        command: Option<String>,
    ) -> Result<serde_json::Value> {
        if (workspace.is_some() || all) && command.is_some() {
            return Err(anyhow!(
                "--command can only be used when restarting one pane"
            ));
        }
        let scoped = all || workspace.is_some();
        let targets = self.wait_targets(pane, workspace, all)?;
        let mut restarted = Vec::new();
        for pane_id in targets {
            restarted.push(self.restart_pane_by_id(&pane_id, command.clone())?);
        }
        if restarted.len() == 1 && !scoped {
            Ok(serde_json::to_value(&restarted[0])?)
        } else {
            Ok(serde_json::json!({ "panes": restarted }))
        }
    }

    fn restart_pane_by_id(
        self: &Arc<Self>,
        pane_id: &str,
        command: Option<String>,
    ) -> Result<Pane> {
        self.remove_pane_runtimes(pane_id);

        let (mut pane, workspace_id, cwd) = {
            let session = self.session.lock_or_recover();
            let mut pane = session
                .panes
                .get(pane_id)
                .cloned()
                .ok_or_else(|| anyhow!("unknown pane {pane_id}"))?;
            if let Some(command) = command {
                pane.command = if command.trim().is_empty() {
                    default_shell()
                } else {
                    command
                };
            }
            let workspace = session
                .workspaces
                .iter()
                .find(|workspace| workspace.panes.iter().any(|item| item == pane_id))
                .ok_or_else(|| anyhow!("pane {pane_id} is not attached to a workspace"))?;
            (
                pane,
                workspace.id.clone(),
                normalize_cwd(Some(workspace.cwd.clone()))?,
            )
        };

        self.start_pane_runtime(&mut pane, cwd, &workspace_id)?;
        {
            let mut session = self.session.lock_or_recover();
            session.panes.insert(pane_id.to_string(), pane.clone());
        }
        self.save()?;
        Ok(pane)
    }

    fn resize_ptys(
        &self,
        pane_sizes: BTreeMap<String, PaneSize>,
        client_id: Option<String>,
        take_control: bool,
    ) -> Result<()> {
        if let Some(client_id) = client_id.as_deref() {
            let mut owner = self.pane_size_owner.lock_or_recover();
            if take_control || owner.is_none() {
                *owner = Some(client_id.to_string());
            }
            if owner.as_deref() != Some(client_id) {
                return Ok(());
            }
        }
        let keyed_sizes = {
            let session = self.session.lock_or_recover();
            pane_sizes
                .into_iter()
                .map(|(pane_id, size)| {
                    let runtime_key = session
                        .panes
                        .get(&pane_id)
                        .map(active_runtime_key_for_pane)
                        .unwrap_or_else(|| pane_id.clone());
                    (pane_id, runtime_key, size)
                })
                .collect::<Vec<_>>()
        };
        let mut panes = self.panes.lock_or_recover();
        for (pane_id, runtime_key, size) in keyed_sizes {
            let key = if panes.contains_key(&runtime_key) {
                runtime_key
            } else {
                legacy_runtime_key(&pane_id)
            };
            let Some(runtime) = panes.get_mut(&key) else {
                continue;
            };
            let next = sanitize_pane_size(size);
            if runtime.size == next {
                continue;
            }
            // An exited pane has no master; skip resizing it (nothing to drive).
            if let Some(master) = runtime.master.as_ref() {
                master.resize(PtySize {
                    rows: next.rows,
                    cols: next.cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })?;
            }
            runtime.parser.screen_mut().set_size(next.rows, next.cols);
            runtime.size = next;
        }
        Ok(())
    }

    fn write_input(&self, pane: Option<String>, data: String) -> Result<()> {
        let pane_id = self.resolve_pane(pane)?;
        let runtime_key = self.active_runtime_key(&pane_id)?;
        // User-typed input starts a new agent turn → sticky Busy until Stop.
        if looks_like_user_turn_input(&data) {
            self.mark_coding_agent_busy(&pane_id, &runtime_key);
        }
        // Only hold the global panes lock long enough to clone the per-pane
        // writer handle; the potentially blocking write happens without it.
        let writer = {
            let panes = self.panes.lock_or_recover();
            let key = if panes.contains_key(&runtime_key) {
                runtime_key.clone()
            } else {
                legacy_runtime_key(&pane_id)
            };
            panes
                .get(&key)
                .and_then(|runtime| runtime.writer.as_ref().map(Arc::clone))
                .ok_or_else(|| anyhow!("pane {pane_id} is not running"))?
        };
        let mut writer = writer.lock_or_recover();
        writer.write_all(data.as_bytes())?;
        writer.flush()?;
        Ok(())
    }

    fn mark_coding_agent_busy(&self, pane_id: &str, runtime_key: &str) {
        {
            let mut session = self.session.lock_or_recover();
            if let Some(pane) = session.panes.get_mut(pane_id) {
                if crate::model::is_coding_agent_command(&pane.command)
                    || matches!(pane.surface_kind, crate::model::SurfaceKind::Agent)
                {
                    touch_agent_status(pane, AgentStatus::Busy, true);
                    pane.notification_message = None;
                    pane.notification_color = None;
                    pane.updated_at = unix_time();
                }
            }
        }
        if let Some(runtime) = self.panes.lock_or_recover().get_mut(runtime_key) {
            if crate::model::is_coding_agent_command(&runtime.pane.command)
                || matches!(runtime.pane.surface_kind, crate::model::SurfaceKind::Agent)
            {
                touch_agent_status(&mut runtime.pane, AgentStatus::Busy, true);
                runtime.pane.notification_message = None;
                runtime.pane.notification_color = None;
                runtime.pane.updated_at = unix_time();
            }
        }
    }

    fn broadcast_input(&self, scope: BroadcastScope, data: String) -> Result<serde_json::Value> {
        let targets = {
            let session = self.session.lock_or_recover();
            match scope {
                BroadcastScope::Workspace => session
                    .workspaces
                    .iter()
                    .find(|workspace| workspace.id == session.active_workspace)
                    .map(|workspace| workspace.panes.clone())
                    .unwrap_or_default(),
                BroadcastScope::Session => session.panes.keys().cloned().collect(),
            }
        };
        let target_keys = {
            let session = self.session.lock_or_recover();
            targets
                .into_iter()
                .map(|pane_id| {
                    let runtime_key = session
                        .panes
                        .get(&pane_id)
                        .map(active_runtime_key_for_pane)
                        .unwrap_or_else(|| pane_id.clone());
                    (pane_id, runtime_key)
                })
                .collect::<Vec<_>>()
        };
        // Clone each target's writer handle under the lock, then release the
        // global lock before performing any (potentially blocking) writes.
        let writers = {
            let panes = self.panes.lock_or_recover();
            target_keys
                .into_iter()
                .map(|(pane_id, runtime_key)| {
                    let key = if panes.contains_key(&runtime_key) {
                        runtime_key
                    } else {
                        legacy_runtime_key(&pane_id)
                    };
                    let writer = panes
                        .get(&key)
                        .and_then(|runtime| runtime.writer.as_ref().map(Arc::clone));
                    (pane_id, writer)
                })
                .collect::<Vec<_>>()
        };
        let mut delivered = Vec::new();
        let mut skipped = Vec::new();
        for (pane_id, writer) in writers {
            let Some(writer) = writer else {
                skipped.push(pane_id);
                continue;
            };
            let mut writer = writer.lock_or_recover();
            writer.write_all(data.as_bytes())?;
            writer.flush()?;
            delivered.push(pane_id);
        }
        Ok(serde_json::json!({
            "scope": scope,
            "delivered": delivered,
            "skipped": skipped,
        }))
    }

    fn kill_pane(&self, pane: Option<String>) -> Result<Pane> {
        let pane_id = self.resolve_pane(pane)?;
        // With tabs the live runtime is keyed `pane-N::tab-M`; resolve the active
        // tab's key (falling back to the bare key) so we capture the correct
        // final scrollback before the process is killed (finding 5).
        let runtime_key = self
            .active_runtime_key(&pane_id)
            .unwrap_or_else(|_| pane_id.clone());
        let output = {
            let panes = self.panes.lock_or_recover();
            panes
                .get(&runtime_key)
                .or_else(|| panes.get(&pane_id))
                .map(|runtime| runtime.joined_output())
        };
        self.remove_pane_runtimes(&pane_id);

        let mut session = self.session.lock_or_recover();
        for workspace in &mut session.workspaces {
            workspace.panes.retain(|item| item != &pane_id);
            workspace.layout = remove_pane_from_layout(workspace.layout.take(), &pane_id);
            if workspace.active_pane.as_deref() == Some(&pane_id) {
                workspace.active_pane = workspace.first_pane();
            }
            if workspace.zoomed_pane.as_deref() == Some(&pane_id) {
                workspace.zoomed_pane = None;
            }
        }
        let Some(pane) = session.panes.get_mut(&pane_id) else {
            return Err(anyhow!("unknown pane {pane_id}"));
        };
        pane.status = PaneStatus::Exited;
        touch_agent_status(pane, AgentStatus::Done, true);
        pane.notification_message = None;
        pane.notification_color = None;
        if let Some(output) = output {
            pane.scrollback = trim_output(output.clone(), 16_000);
            pane.output = trim_output(output, 16_000);
            pane.output_formatted.clear();
            // The runtime (and its styled history) is gone; drop the formatted
            // scrollback so the UI falls back to the plain scrollback above.
            pane.scrollback_formatted.clear();
        }
        pane.updated_at = unix_time();
        sync_active_pane_tab(pane);
        let pane = pane.clone();
        drop(session);
        self.save()?;
        Ok(pane)
    }

    fn set_pane_title(&self, pane: Option<String>, title: String) -> Result<Pane> {
        let title = normalize_required_pane_title(title)?;
        let pane_id = self.resolve_pane(pane)?;
        let runtime_key = self
            .active_runtime_key(&pane_id)
            .unwrap_or_else(|_| pane_id.clone());
        {
            let mut panes = self.panes.lock_or_recover();
            if let Some(runtime) = panes.get_mut(&runtime_key) {
                runtime.pane.title = title.clone();
                runtime.pane.updated_at = unix_time();
            }
        }

        let mut session = self.session.lock_or_recover();
        let Some(pane) = session.panes.get_mut(&pane_id) else {
            return Err(anyhow!("unknown pane {pane_id}"));
        };
        pane.title = title;
        pane.updated_at = unix_time();
        sync_active_pane_tab(pane);
        let pane = pane.clone();
        drop(session);
        self.save()?;
        Ok(pane)
    }

    fn set_pane_metadata(
        &self,
        pane: Option<String>,
        key: String,
        value: Option<String>,
    ) -> Result<serde_json::Value> {
        let key = normalize_metadata_key(&key)?;
        let value = value.map(|value| value.trim().to_string());
        let pane_id = self.resolve_pane(pane)?;
        let runtime_key = self
            .active_runtime_key(&pane_id)
            .unwrap_or_else(|_| pane_id.clone());
        let updated_at = unix_time();
        {
            let mut panes = self.panes.lock_or_recover();
            if let Some(runtime) = panes.get_mut(&runtime_key) {
                if let Some(value) = value.as_ref().filter(|value| !value.is_empty()) {
                    runtime.pane.metadata.insert(key.clone(), value.clone());
                } else {
                    runtime.pane.metadata.remove(&key);
                }
                runtime.pane.updated_at = updated_at;
            }
        }

        let mut session = self.session.lock_or_recover();
        let Some(pane) = session.panes.get_mut(&pane_id) else {
            return Err(anyhow!("unknown pane {pane_id}"));
        };
        let event_value = value.clone().filter(|value| !value.is_empty());
        if let Some(value) = event_value.clone() {
            pane.metadata.insert(key.clone(), value);
        } else {
            pane.metadata.remove(&key);
        }
        pane.updated_at = updated_at;
        let metadata = pane.metadata.clone();
        push_event(
            &mut session,
            EventRecord {
                time: updated_at,
                kind: "metadata".to_string(),
                pane: Some(pane_id.clone()),
                workspace: None,
                status: None,
                key: Some(key),
                value: event_value,
                message: String::new(),
            },
        );
        drop(session);
        self.save()?;
        Ok(serde_json::json!({
            "pane": pane_id,
            "metadata": metadata,
        }))
    }

    fn resolve_workspace_id(&self, workspace: Option<String>) -> Result<String> {
        let session = self.session.lock_or_recover();
        match workspace {
            Some(selector) => session
                .resolve_workspace_selector(&selector)
                .map_err(anyhow::Error::msg),
            None => Ok(session.active_workspace.clone()),
        }
    }

    fn list_tabs(&self, workspace: Option<String>) -> Result<serde_json::Value> {
        let workspace_id = self.resolve_workspace_id(workspace)?;
        let session = self.session.lock_or_recover();
        let ws = session
            .workspaces
            .iter()
            .find(|w| w.id == workspace_id)
            .ok_or_else(|| anyhow!("unknown workspace {workspace_id}"))?;
        Ok(serde_json::json!({
            "workspace": workspace_id,
            "active_tab": ws.active_tab,
            "tabs": ws.tabs,
        }))
    }

    fn new_tab(
        self: &Arc<Self>,
        workspace: Option<String>,
        title: Option<String>,
        command: Option<String>,
    ) -> Result<serde_json::Value> {
        let workspace_id = self.resolve_workspace_id(workspace)?;
        let title = title
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| "tab".to_string());
        let tab = {
            let mut session = self.session.lock_or_recover();
            let ws = session
                .workspaces
                .iter_mut()
                .find(|w| w.id == workspace_id)
                .ok_or_else(|| anyhow!("unknown workspace {workspace_id}"))?;
            ws.add_tab(title)
        };
        // Optional first pane in the new tab.
        let pane = if let Some(command) = command.filter(|c| !c.trim().is_empty()) {
            Some(self.new_pane(
                SplitDirection::Right,
                command,
                Some(tab.title.clone()),
                Some(workspace_id.clone()),
                None,
            )?)
        } else {
            // Empty shell so the tab isn't blank.
            Some(self.new_pane(
                SplitDirection::Right,
                String::new(),
                Some(tab.title.clone()),
                Some(workspace_id.clone()),
                None,
            )?)
        };
        let session = self.session.lock_or_recover();
        let ws = session
            .workspaces
            .iter()
            .find(|w| w.id == workspace_id)
            .ok_or_else(|| anyhow!("unknown workspace {workspace_id}"))?;
        Ok(serde_json::json!({
            "workspace": workspace_id,
            "active_tab": ws.active_tab,
            "tab": ws.tabs.iter().find(|t| t.id == tab.id),
            "pane": pane,
            "tabs": ws.tabs,
        }))
    }

    fn switch_tab(&self, workspace: Option<String>, tab: String) -> Result<serde_json::Value> {
        let workspace_id = self.resolve_workspace_id(workspace)?;
        let (active_tab, tabs, panes, active_pane) = {
            let mut session = self.session.lock_or_recover();
            let ws = session
                .workspaces
                .iter_mut()
                .find(|w| w.id == workspace_id)
                .ok_or_else(|| anyhow!("unknown workspace {workspace_id}"))?;
            let tab_id = resolve_workspace_tab_id(ws, &tab)?;
            ws.switch_tab(&tab_id).map_err(anyhow::Error::msg)?;
            session.active_workspace = workspace_id.clone();
            let ws = session
                .workspaces
                .iter()
                .find(|w| w.id == workspace_id)
                .expect("workspace still present");
            (
                ws.active_tab.clone(),
                ws.tabs.clone(),
                ws.panes.clone(),
                ws.active_pane.clone(),
            )
        };
        // Opening a tab acknowledges ✅ on that tab's panes (user is looking).
        self.acknowledge_done_panes(&panes);
        self.save()?;
        Ok(serde_json::json!({
            "workspace": workspace_id,
            "active_tab": active_tab,
            "active_pane": active_pane,
            "panes": panes,
            "tabs": tabs,
        }))
    }

    /// Clear finished-turn ✅ on panes the user has just focused / opened.
    fn acknowledge_done_panes(&self, pane_ids: &[String]) {
        if pane_ids.is_empty() {
            return;
        }
        {
            let mut session = self.session.lock_or_recover();
            for pane_id in pane_ids {
                if let Some(pane) = session.panes.get_mut(pane_id) {
                    acknowledge_done_status(pane);
                }
            }
        }
        let mut panes = self.panes.lock_or_recover();
        for runtime in panes.values_mut() {
            if pane_ids.iter().any(|id| id == &runtime.pane.id) {
                acknowledge_done_status(&mut runtime.pane);
            }
        }
    }

    fn rename_tab(
        &self,
        workspace: Option<String>,
        tab: String,
        title: String,
    ) -> Result<serde_json::Value> {
        let title = normalize_required_pane_title(title)?;
        let workspace_id = self.resolve_workspace_id(workspace)?;
        let mut session = self.session.lock_or_recover();
        let ws = session
            .workspaces
            .iter_mut()
            .find(|w| w.id == workspace_id)
            .ok_or_else(|| anyhow!("unknown workspace {workspace_id}"))?;
        let tab_id = resolve_workspace_tab_id(ws, &tab)?;
        let renamed = ws.rename_tab(&tab_id, title).map_err(anyhow::Error::msg)?;
        drop(session);
        self.save()?;
        Ok(serde_json::json!({
            "workspace": workspace_id,
            "tab": renamed,
        }))
    }

    fn close_tab(
        self: &Arc<Self>,
        workspace: Option<String>,
        tab: String,
    ) -> Result<serde_json::Value> {
        let workspace_id = self.resolve_workspace_id(workspace)?;
        let (pane_ids, next_tab, tabs, active_tab) = {
            let mut session = self.session.lock_or_recover();
            let ws = session
                .workspaces
                .iter_mut()
                .find(|w| w.id == workspace_id)
                .ok_or_else(|| anyhow!("unknown workspace {workspace_id}"))?;
            let tab_id = resolve_workspace_tab_id(ws, &tab)?;
            let (pane_ids, next_tab) = ws.close_tab(&tab_id).map_err(anyhow::Error::msg)?;
            // Drop closed panes from session map after kill.
            (pane_ids, next_tab, ws.tabs.clone(), ws.active_tab.clone())
        };
        for pane_id in &pane_ids {
            self.remove_pane_runtimes(pane_id);
            let mut session = self.session.lock_or_recover();
            session.panes.remove(pane_id);
        }
        self.save()?;
        Ok(serde_json::json!({
            "workspace": workspace_id,
            "closed_panes": pane_ids,
            "active_tab": active_tab,
            "tab": next_tab,
            "tabs": tabs,
        }))
    }

    fn move_pane_in_layout(
        &self,
        pane: Option<String>,
        direction: SplitDirection,
    ) -> Result<serde_json::Value> {
        let pane_id = self.resolve_pane(pane)?;
        let neighbor = {
            let mut session = self.session.lock_or_recover();
            let ws = session
                .workspaces
                .iter_mut()
                .find(|w| w.panes.iter().any(|p| p == &pane_id))
                .ok_or_else(|| anyhow!("pane {pane_id} is not in the active tab layout"))?;
            ws.ensure_layout();
            crate::model::adjacent_pane_in_layout(ws.layout.as_ref(), &pane_id, direction)
                .ok_or_else(|| {
                    anyhow!("pane {pane_id} has no neighbor to the {direction:?} (edge of layout)")
                })?
        };
        if neighbor == pane_id {
            return Err(anyhow!("cannot move pane onto itself"));
        }
        let mut session = self.session.lock_or_recover();
        let workspace = session
            .swap_panes(&pane_id, &neighbor)
            .map_err(anyhow::Error::msg)?;
        drop(session);
        self.save()?;
        Ok(serde_json::json!({
            "pane": pane_id,
            "swapped_with": neighbor,
            "direction": direction,
            "workspace": workspace,
        }))
    }

    fn wait_panes(
        &self,
        pane: Option<String>,
        workspace: Option<String>,
        all: bool,
        timeout_ms: Option<u64>,
    ) -> Result<serde_json::Value> {
        let scoped = all || workspace.is_some();
        let targets = self.wait_targets(pane, workspace, all)?;
        let started = Instant::now();
        loop {
            let panes = {
                let session = self.session.lock_or_recover();
                targets
                    .iter()
                    .map(|pane_id| {
                        session
                            .panes
                            .get(pane_id)
                            .cloned()
                            .ok_or_else(|| anyhow!("unknown pane {pane_id}"))
                    })
                    .collect::<Result<Vec<_>>>()?
            };
            if panes
                .iter()
                .all(|pane| matches!(pane.status, PaneStatus::Exited))
            {
                if panes.len() == 1 && !scoped {
                    return Ok(serde_json::to_value(&panes[0])?);
                }
                return Ok(serde_json::json!({
                    "panes": panes,
                }));
            }
            if timeout_ms
                .map(|timeout| started.elapsed() >= Duration::from_millis(timeout))
                .unwrap_or(false)
            {
                return Err(anyhow!("timed out waiting for panes {}", targets.join(",")));
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn wait_targets(
        &self,
        pane: Option<String>,
        workspace: Option<String>,
        all: bool,
    ) -> Result<Vec<String>> {
        if all {
            let targets = self
                .session
                .lock_or_recover()
                .panes
                .keys()
                .cloned()
                .collect();
            return Ok(targets);
        }
        if let Some(workspace) = workspace {
            let session = self.session.lock_or_recover();
            // Accept a workspace name OR id (finding 10).
            let workspace = session
                .resolve_workspace_selector(&workspace)
                .map_err(anyhow::Error::msg)?;
            let targets = session
                .workspaces
                .iter()
                .find(|item| item.id == workspace)
                .map(|workspace| workspace.panes.clone())
                .ok_or_else(|| anyhow!("unknown workspace {workspace}"))?;
            if targets.is_empty() {
                return Err(anyhow!("workspace has no panes"));
            }
            return Ok(targets);
        }
        Ok(vec![self.resolve_pane(pane)?])
    }

    fn notify(
        &self,
        pane: Option<String>,
        workspace: Option<String>,
        status: Option<String>,
        color: Option<String>,
        clear: bool,
        message: String,
    ) -> Result<Notification> {
        let target_workspace = self.resolve_workspace(workspace)?;
        let target_pane = if target_workspace.is_some() {
            pane
        } else {
            Some(self.resolve_pane(pane.clone())?)
        };
        let note = Notification {
            time: unix_time(),
            pane: target_pane.clone(),
            workspace: target_workspace,
            status: status.clone(),
            color: color.clone(),
            clear,
            message: message.clone(),
        };
        let mut session = self.session.lock_or_recover();
        let mut runtime_target = None;
        if let Some(pane_id) = &target_pane {
            if let Some(target) = session.panes.get_mut(pane_id) {
                runtime_target = Some(active_runtime_key_for_pane(target));
                if clear {
                    target.notification_color = None;
                    target.notification_message = None;
                } else if let Some(status) = &status {
                    apply_explicit_agent_status(target, status, color.as_deref(), &message);
                } else if !message.is_empty() {
                    // Bare notify without status = needs attention.
                    touch_agent_status(target, AgentStatus::Attention, true);
                    target.notification_color = color.clone().or_else(|| Some("blue".to_string()));
                    target.notification_message = Some(message.clone());
                }
                target.updated_at = unix_time();
            }
        }
        session.notifications.push(note.clone());
        let len = session.notifications.len();
        if len > 100 {
            session.notifications.drain(0..len - 100);
        }
        push_event(
            &mut session,
            EventRecord {
                time: note.time,
                kind: "notification".to_string(),
                pane: target_pane.clone(),
                workspace: note.workspace.clone(),
                status: status.clone(),
                key: None,
                value: color.clone(),
                message: message.clone(),
            },
        );
        drop(session);
        if let Some(runtime_key) = runtime_target {
            if let Some(runtime) = self.panes.lock_or_recover().get_mut(&runtime_key) {
                if clear {
                    runtime.pane.notification_color = None;
                    runtime.pane.notification_message = None;
                } else if let Some(status) = &status {
                    apply_explicit_agent_status(
                        &mut runtime.pane,
                        status,
                        color.as_deref(),
                        &message,
                    );
                } else if !message.is_empty() {
                    touch_agent_status(&mut runtime.pane, AgentStatus::Attention, true);
                    runtime.pane.notification_color =
                        color.clone().or_else(|| Some("blue".to_string()));
                    runtime.pane.notification_message = Some(message.clone());
                }
                runtime.pane.updated_at = unix_time();
            }
        }
        self.save()?;
        Ok(note)
    }

    fn clear_notifications(&self) -> Result<serde_json::Value> {
        let cleared = {
            let mut session = self.session.lock_or_recover();
            let cleared = session.notifications.len();
            session.notifications.clear();
            cleared
        };
        self.save()?;
        Ok(serde_json::json!({ "cleared": cleared }))
    }

    fn notifications(&self, limit: usize) -> Result<serde_json::Value> {
        let snapshot = self.snapshot(false)?;
        let limit = limit.clamp(1, 100);
        let notifications = snapshot
            .notifications
            .iter()
            .rev()
            .take(limit)
            .map(|note| {
                let workspace = notification_workspace(&snapshot, note);
                let pane = note
                    .pane
                    .as_ref()
                    .and_then(|pane_id| snapshot.panes.get(pane_id));
                serde_json::json!({
                    "time": note.time,
                    "pane": note.pane,
                    "pane_title": pane.map(|pane| pane.title.clone()),
                    "workspace": workspace.as_ref().map(|workspace| workspace.id.clone()),
                    "workspace_name": workspace.as_ref().map(|workspace| workspace.name.clone()),
                    "status": note.status,
                    "color": note.color,
                    "clear": note.clear,
                    "message": note.message,
                })
            })
            .collect::<Vec<_>>();
        Ok(serde_json::json!({
            "session": snapshot.name,
            "count": notifications.len(),
            "notifications": notifications,
        }))
    }

    fn events(&self, limit: usize) -> Result<serde_json::Value> {
        let snapshot = self.snapshot(false)?;
        let limit = limit.clamp(1, 500);
        let events = snapshot
            .events
            .iter()
            .rev()
            .take(limit)
            .map(|event| {
                let workspace = event
                    .workspace
                    .as_ref()
                    .and_then(|workspace_id| {
                        snapshot
                            .workspaces
                            .iter()
                            .find(|workspace| &workspace.id == workspace_id)
                    })
                    .or_else(|| {
                        event.pane.as_ref().and_then(|pane_id| {
                            snapshot.workspaces.iter().find(|workspace| {
                                workspace.panes.iter().any(|item| item == pane_id)
                            })
                        })
                    });
                let pane = event
                    .pane
                    .as_ref()
                    .and_then(|pane_id| snapshot.panes.get(pane_id));
                serde_json::json!({
                    "time": event.time,
                    "kind": event.kind,
                    "pane": event.pane,
                    "pane_title": pane.map(|pane| pane.title.clone()),
                    "workspace": workspace.as_ref().map(|workspace| workspace.id.clone()),
                    "workspace_name": workspace.as_ref().map(|workspace| workspace.name.clone()),
                    "status": event.status,
                    "key": event.key,
                    "value": event.value,
                    "message": event.message,
                })
            })
            .collect::<Vec<_>>();
        Ok(serde_json::json!({
            "session": snapshot.name,
            "count": events.len(),
            "events": events,
        }))
    }

    fn jump_notification(&self) -> Result<serde_json::Value> {
        let target = {
            let session = self.session.lock_or_recover();
            session
                .notifications
                .iter()
                .rev()
                .find(|note| !note.clear && (!note.message.is_empty() || note.status.is_some()))
                .cloned()
                .ok_or_else(|| anyhow!("no notifications"))?
        };

        let mut session = self.session.lock_or_recover();
        let (workspace_id, pane_id) = if let Some(pane_id) = target.pane.clone() {
            let workspace_id = session
                .workspaces
                .iter()
                .find(|workspace| workspace.panes.iter().any(|item| item == &pane_id))
                .map(|workspace| workspace.id.clone())
                .ok_or_else(|| anyhow!("notification pane {pane_id} is no longer attached"))?;
            (workspace_id, Some(pane_id))
        } else {
            let workspace_id = target
                .workspace
                .clone()
                .ok_or_else(|| anyhow!("latest notification has no target workspace or pane"))?;
            if !session
                .workspaces
                .iter()
                .any(|workspace| workspace.id == workspace_id)
            {
                return Err(anyhow!(
                    "notification workspace {workspace_id} no longer exists"
                ));
            }
            (workspace_id, None)
        };

        session.active_workspace = workspace_id.clone();
        if let Some(pane_id) = pane_id.clone() {
            let workspace = session.active_workspace_mut();
            workspace.active_pane = Some(pane_id.clone());
        }
        drop(session);
        self.save()?;

        Ok(serde_json::json!({
            "workspace": workspace_id,
            "pane": pane_id,
            "notification": target,
        }))
    }

    fn set_progress(&self, pane: Option<String>, value: Option<u8>) -> Result<Pane> {
        let pane_id = self.resolve_pane(pane)?;
        let value = value.map(|item| item.min(100));
        let mut session = self.session.lock_or_recover();
        let Some(pane) = session.panes.get_mut(&pane_id) else {
            return Err(anyhow!("unknown pane {pane_id}"));
        };
        pane.progress = value;
        pane.updated_at = unix_time();
        let updated_at = pane.updated_at;
        let pane = pane.clone();
        push_event(
            &mut session,
            EventRecord {
                time: updated_at,
                kind: "progress".to_string(),
                pane: Some(pane_id.clone()),
                workspace: None,
                status: value.map(|value| value.to_string()),
                key: None,
                value: value.map(|value| value.to_string()),
                message: String::new(),
            },
        );
        drop(session);
        let runtime_key = self
            .active_runtime_key(&pane_id)
            .unwrap_or_else(|_| pane_id.clone());
        if let Some(runtime) = self.panes.lock_or_recover().get_mut(&runtime_key) {
            runtime.pane.progress = value;
            runtime.pane.updated_at = unix_time();
        }
        self.save()?;
        Ok(pane)
    }

    fn read_screen(
        &self,
        pane: Option<String>,
        include_scrollback: bool,
        limit_bytes: Option<usize>,
    ) -> Result<serde_json::Value> {
        let pane_id = self.resolve_pane(pane)?;
        let runtime_key = self
            .active_runtime_key(&pane_id)
            .unwrap_or_else(|_| pane_id.clone());
        let limit = scrollback_limit(limit_bytes);
        let output = {
            let panes = self.panes.lock_or_recover();
            panes
                .get(&runtime_key)
                .or_else(|| panes.get(&pane_id))
                .map(|runtime| {
                    let raw = runtime.joined_output();
                    let mut value = serde_json::json!({
                        "pane": pane_id,
                        "screen": runtime.parser.screen().contents(),
                        "rows": runtime.size.rows,
                        "cols": runtime.size.cols,
                    });
                    if include_scrollback {
                        value["scrollback"] = serde_json::json!(trim_output(raw, limit));
                    }
                    value
                })
        };
        if let Some(output) = output {
            return Ok(output);
        }
        let output = self
            .session
            .lock_or_recover()
            .panes
            .get(&pane_id)
            .map(|pane| pane.output.clone())
            .ok_or_else(|| anyhow!("unknown pane {pane_id}"))?;
        let mut value = serde_json::json!({
            "pane": pane_id,
            "screen": trim_output(output.clone(), 16_000),
        });
        if include_scrollback {
            value["scrollback"] = serde_json::json!(trim_output(output, limit));
        }
        Ok(value)
    }

    fn search_pane(&self, pane: Option<String>, query: String) -> Result<serde_json::Value> {
        let pane_id = self.resolve_pane(pane)?;
        let runtime_key = self
            .active_runtime_key(&pane_id)
            .unwrap_or_else(|_| pane_id.clone());
        if query.is_empty() {
            return Err(anyhow!("search query cannot be empty"));
        }
        let (screen, scrollback) = {
            let panes = self.panes.lock_or_recover();
            if let Some(runtime) = panes.get(&runtime_key).or_else(|| panes.get(&pane_id)) {
                (runtime.parser.screen().contents(), runtime.joined_output())
            } else {
                let output = self
                    .session
                    .lock_or_recover()
                    .panes
                    .get(&pane_id)
                    .map(|pane| pane.output.clone())
                    .ok_or_else(|| anyhow!("unknown pane {pane_id}"))?;
                (output.clone(), output)
            }
        };
        let mut matches = Vec::new();
        for (source, text) in [
            ("screen", screen.as_str()),
            ("scrollback", scrollback.as_str()),
        ] {
            for (line_number, line) in text.lines().enumerate() {
                if line.contains(&query) {
                    matches.push(serde_json::json!({
                        "source": source,
                        "line": line_number + 1,
                        "text": line,
                    }));
                }
            }
        }
        Ok(serde_json::json!({
            "pane": pane_id,
            "query": query,
            "matches": matches,
        }))
    }

    fn copy_pane(
        &self,
        pane: Option<String>,
        scrollback: bool,
        limit_bytes: Option<usize>,
    ) -> Result<ClipboardItem> {
        let pane_id = self.resolve_pane(pane)?;
        let runtime_key = self
            .active_runtime_key(&pane_id)
            .unwrap_or_else(|_| pane_id.clone());
        let limit = scrollback_limit(limit_bytes);
        let text = {
            let panes = self.panes.lock_or_recover();
            if let Some(runtime) = panes.get(&runtime_key).or_else(|| panes.get(&pane_id)) {
                if scrollback {
                    trim_output(runtime.joined_output(), limit)
                } else {
                    runtime.parser.screen().contents()
                }
            } else {
                let pane = self
                    .session
                    .lock_or_recover()
                    .panes
                    .get(&pane_id)
                    .cloned()
                    .ok_or_else(|| anyhow!("unknown pane {pane_id}"))?;
                if scrollback {
                    trim_output(pane.scrollback, limit)
                } else {
                    pane.output
                }
            }
        };
        let item = ClipboardItem {
            text,
            source_pane: Some(pane_id),
            source: if scrollback {
                "scrollback".to_string()
            } else {
                "screen".to_string()
            },
            copied_at: unix_time(),
        };
        let mut session = self.session.lock_or_recover();
        session.clipboard = Some(item.clone());
        drop(session);
        self.save()?;
        Ok(item)
    }

    fn paste_clipboard(&self, pane: Option<String>, enter: bool) -> Result<serde_json::Value> {
        let item = self
            .session
            .lock_or_recover()
            .clipboard
            .clone()
            .ok_or_else(|| anyhow!("clipboard is empty"))?;
        let mut data = item.text.clone();
        if enter {
            data.push('\n');
        }
        let bytes = data.len();
        let pane_id = self.resolve_pane(pane)?;
        self.write_input(Some(pane_id.clone()), data)?;
        Ok(serde_json::json!({
            "pane": pane_id,
            "bytes": bytes,
            "clipboard": item,
        }))
    }

    fn clipboard(&self) -> Result<serde_json::Value> {
        let item = self.session.lock_or_recover().clipboard.clone();
        Ok(serde_json::json!({ "clipboard": item }))
    }

    fn set_clipboard(
        &self,
        text: String,
        source_pane: Option<String>,
        source: String,
    ) -> Result<ClipboardItem> {
        let item = ClipboardItem {
            text,
            source_pane,
            source,
            copied_at: unix_time(),
        };
        let mut session = self.session.lock_or_recover();
        session.clipboard = Some(item.clone());
        drop(session);
        self.save()?;
        Ok(item)
    }

    fn clear_pane_capture(&self, pane: Option<String>) -> Result<Pane> {
        let pane_id = self.resolve_pane(pane)?;
        let runtime_key = self
            .active_runtime_key(&pane_id)
            .unwrap_or_else(|_| pane_id.clone());
        let size = {
            let mut panes = self.panes.lock_or_recover();
            if let Some(runtime) = panes.get_mut(&runtime_key) {
                runtime.output.clear();
                runtime.output_bytes = 0;
                runtime.pending.clear();
                runtime.osc_tail.clear();
                runtime.parser = vt100::Parser::new(runtime.size.rows, runtime.size.cols, 2000);
                runtime.pane.output.clear();
                runtime.pane.output_formatted.clear();
                runtime.pane.scrollback.clear();
                runtime.pane.scrollback_formatted.clear();
                // Force the styled-scrollback cache to rebuild from the fresh
                // parser on the next snapshot.
                runtime.scrollback_formatted_cache.clear();
                runtime.scrollback_formatted_generation = u64::MAX;
                Some(runtime.size)
            } else {
                None
            }
        };
        let mut session = self.session.lock_or_recover();
        let Some(pane) = session.panes.get_mut(&pane_id) else {
            return Err(anyhow!("unknown pane {pane_id}"));
        };
        pane.output.clear();
        pane.output_formatted.clear();
        // Also clear the persisted scrollback (not just output); otherwise a
        // later snapshot/sync copies the old history back into the pane and its
        // active tab (finding 6). sync_active_pane_tab then propagates the
        // cleared state into the active tab's captured record.
        pane.scrollback.clear();
        pane.scrollback_formatted.clear();
        pane.updated_at = unix_time();
        sync_active_pane_tab(pane);
        let mut pane = pane.clone();
        if let Some(size) = size {
            pane.output = format!("cleared capture at {}x{}", size.rows, size.cols);
        }
        drop(session);
        self.save()?;
        Ok(pane)
    }

    fn resolve_pane(&self, pane: Option<String>) -> Result<String> {
        let mut session = self.session.lock_or_recover();
        let pane_id = match pane {
            Some(pane) => pane,
            None => session
                .active_workspace_mut()
                .active_pane
                .clone()
                .ok_or_else(|| anyhow!("no active pane"))?,
        };
        // Validate the pane actually exists so callers fail with a clear
        // "unknown pane" here rather than a confusing downstream "not running"
        // error (finding 13).
        if !session.panes.contains_key(&pane_id) {
            return Err(anyhow!("unknown pane {pane_id}"));
        }
        Ok(pane_id)
    }

    fn resolve_workspace(&self, workspace: Option<String>) -> Result<Option<String>> {
        let Some(workspace) = workspace else {
            return Ok(None);
        };
        // Accept a workspace name OR id, matching switch/rename (finding 10).
        let id = self
            .session
            .lock_or_recover()
            .resolve_workspace_selector(&workspace)
            .map_err(anyhow::Error::msg)?;
        Ok(Some(id))
    }

    fn active_runtime_key(&self, pane_id: &str) -> Result<String> {
        let session = self.session.lock_or_recover();
        let pane = session
            .panes
            .get(pane_id)
            .ok_or_else(|| anyhow!("unknown pane {pane_id}"))?;
        Ok(active_runtime_key_for_pane(pane))
    }

    fn remove_pane_runtimes(&self, pane_id: &str) {
        let mut panes = self.panes.lock_or_recover();
        if let Some(mut runtime) = panes.remove(pane_id) {
            if let Some(mut killer) = runtime.killer.take() {
                killer.kill().ok();
            }
        }
        let prefix = format!("{pane_id}::");
        let keys = panes
            .keys()
            .filter(|key| key.starts_with(&prefix))
            .cloned()
            .collect::<Vec<_>>();
        for key in keys {
            if let Some(mut runtime) = panes.remove(&key) {
                if let Some(mut killer) = runtime.killer.take() {
                    killer.kill().ok();
                }
            }
        }
    }

    #[allow(dead_code)]
    fn remove_runtime(&self, runtime_key: &str) {
        if let Some(mut runtime) = self.panes.lock_or_recover().remove(runtime_key) {
            if let Some(mut killer) = runtime.killer.take() {
                killer.kill().ok();
            }
        }
    }

    fn append_output(&self, runtime_key: &str, pane_id: &str, generation: u64, bytes: &[u8]) {
        let Some((runtime_pane, notifications)) = ({
            let mut panes = self.panes.lock_or_recover();
            let runtime = panes.get_mut(runtime_key);
            runtime.and_then(|runtime| {
                if runtime.generation != generation {
                    return None;
                }
                // The vt100 parser buffers partial escape sequences itself, so
                // it always gets the raw bytes.
                runtime.parser.process(bytes);
                // Mark the pane dirty so snapshot() rebuilds the styled
                // scrollback lazily (never on this hot path).
                runtime.output_generation = runtime.output_generation.wrapping_add(1);
                // Decode only the valid UTF-8 prefix, carrying an incomplete
                // trailing multibyte sequence into the next read so multibyte
                // characters split across chunk boundaries are never corrupted.
                let text = decode_utf8_stream(&mut runtime.pending, bytes);
                // Scan for OSC notifications over an accumulated tail so
                // sequences that straddle reads are still detected.
                runtime.osc_tail.push_str(&text);
                let (notifications, retain_from) = scan_osc_notifications(&runtime.osc_tail);
                if retain_from > 0 {
                    runtime.osc_tail.drain(..retain_from);
                }
                cap_osc_tail(&mut runtime.osc_tail);
                runtime.push_output(text.clone());
                runtime.pane.updated_at = unix_time();
                let inferred = infer_agent_status(&text, &runtime.pane.command);
                let prev = runtime.pane.agent_status.clone();
                let (next, pinned) =
                    merge_agent_status(prev.clone(), runtime.pane.agent_status_pinned, inferred);
                if next != prev || pinned != runtime.pane.agent_status_pinned {
                    touch_agent_status(&mut runtime.pane, next, pinned);
                }
                if let Some(message) = notifications.last() {
                    // OSC notifications are explicit attention signals.
                    touch_agent_status(&mut runtime.pane, AgentStatus::Attention, true);
                    runtime.pane.notification_color = Some("blue".to_string());
                    runtime.pane.notification_message = Some(message.clone());
                }
                runtime.pane.output = runtime.parser.screen().contents();
                runtime.pane.output_formatted = screen_contents_formatted(runtime.parser.screen());
                let (cursor_row, cursor_col) = runtime.parser.screen().cursor_position();
                runtime.pane.cursor_row = Some(cursor_row);
                runtime.pane.cursor_col = Some(cursor_col);
                let (screen_rows, screen_cols) = runtime.parser.screen().size();
                runtime.pane.screen_rows = Some(screen_rows);
                runtime.pane.screen_cols = Some(screen_cols);
                update_pane_terminal_modes(&mut runtime.pane, runtime.parser.screen());
                // The deque is byte-bounded to SCROLLBACK_CAP, so this join is
                // O(cap) rather than O(total output) on every read.
                runtime.pane.scrollback = trim_output(runtime.joined_output(), SCROLLBACK_CAP);
                Some((runtime.pane.clone(), notifications))
            })
        }) else {
            return;
        };
        let mut should_save = false;
        {
            let mut session = self.session.lock_or_recover();
            if let Some(pane) = session.panes.get_mut(pane_id) {
                if runtime_key_is_active_for_pane(pane, runtime_key) {
                    let tabs = pane.tabs.clone();
                    let active_tab = pane.active_tab.clone();
                    let metadata = pane.metadata.clone();
                    *pane = runtime_pane.clone();
                    pane.tabs = tabs;
                    pane.active_tab = active_tab;
                    pane.metadata = metadata;
                    sync_active_pane_tab(pane);
                } else if let Some(tab_id) = runtime_key_tab(pane_id, runtime_key) {
                    sync_tab_from_runtime_pane(pane, tab_id, &runtime_pane);
                }
            }
            for message in notifications {
                let event_message = message.clone();
                session.notifications.push(Notification {
                    time: unix_time(),
                    pane: Some(pane_id.to_string()),
                    workspace: None,
                    status: Some("attention".to_string()),
                    color: Some("blue".to_string()),
                    clear: false,
                    message,
                });
                push_event(
                    &mut session,
                    EventRecord {
                        time: unix_time(),
                        kind: "notification".to_string(),
                        pane: Some(pane_id.to_string()),
                        workspace: None,
                        status: Some("attention".to_string()),
                        key: None,
                        value: Some("blue".to_string()),
                        message: event_message,
                    },
                );
                should_save = true;
            }
            let len = session.notifications.len();
            if len > 100 {
                session.notifications.drain(0..len - 100);
            }
        }
        if should_save {
            self.save().ok();
        }
    }

    fn mark_exited(
        &self,
        runtime_key: &str,
        pane_id: &str,
        generation: u64,
        exit_code: Option<i32>,
    ) {
        let agent_status = if exit_code == Some(0) {
            AgentStatus::Done
        } else {
            AgentStatus::Error
        };
        let mut runtime_pane = None;
        {
            let mut panes = self.panes.lock_or_recover();
            if let Some(runtime) = panes.get_mut(runtime_key) {
                if runtime.generation != generation {
                    return;
                }
                runtime.pane.status = PaneStatus::Exited;
                runtime.pane.exit_code = exit_code;
                runtime.pane.pid = None;
                touch_agent_status(&mut runtime.pane, agent_status.clone(), true);
                runtime.pane.progress = Some(100);
                runtime.pane.notification_message = None;
                runtime.pane.notification_color = None;
                runtime.pane.updated_at = unix_time();
                runtime_pane = Some(runtime.pane.clone());
                // Release the PTY OS handles now that the child is gone; the
                // parser/output stay so the UI keeps the final screen state.
                runtime.master = None;
                runtime.writer = None;
                runtime.killer = None;
            }
        }
        let Some(runtime_pane) = runtime_pane else {
            return;
        };
        {
            let mut session = self.session.lock_or_recover();
            if let Some(pane) = session.panes.get_mut(pane_id) {
                if runtime_key_is_active_for_pane(pane, runtime_key) {
                    let tabs = pane.tabs.clone();
                    let active_tab = pane.active_tab.clone();
                    let metadata = pane.metadata.clone();
                    *pane = runtime_pane.clone();
                    pane.tabs = tabs;
                    pane.active_tab = active_tab;
                    pane.metadata = metadata;
                    sync_active_pane_tab(pane);
                } else if let Some(tab_id) = runtime_key_tab(pane_id, runtime_key) {
                    sync_tab_from_runtime_pane(pane, tab_id, &runtime_pane);
                }
            }
            push_event(
                &mut session,
                EventRecord {
                    time: unix_time(),
                    kind: "pane-exit".to_string(),
                    pane: Some(pane_id.to_string()),
                    workspace: None,
                    status: Some(agent_status_name(&agent_status).to_string()),
                    key: None,
                    value: exit_code.map(|code| code.to_string()),
                    message: String::new(),
                },
            );
        }
        self.save().ok();
    }

    fn snapshot(&self, include_output: bool) -> Result<Session> {
        let mut session = self.session.lock_or_recover().clone();
        session.daemon = Some(self.daemon_info());

        // Read cached git/gh/ss metadata (refreshed by the background thread).
        // On a cache miss keep the persisted values so fields stay populated.
        {
            let meta = self.workspace_meta.lock_or_recover();
            for workspace in &mut session.workspaces {
                if let Some(cached) = meta.get(&workspace.id) {
                    workspace.git_branch = cached.git_branch.clone();
                    workspace.pull_request = cached.pull_request.clone();
                    workspace.ports = cached.ports.clone();
                }
            }
        }

        // The vt100 parser lives in the runtime, so screen access must happen
        // under the panes lock; collect the minimal per-pane data here and
        // perform the (allocation-heavy) session merge after releasing it.
        struct Collected {
            runtime_key: String,
            pane: Pane,
            // (screen contents, formatted screen, raw retained output, styled scrollback)
            output: Option<(String, String, String, String)>,
        }
        let collected = {
            // iter_mut: rebuilding the styled scrollback drives the parser's
            // scrollback offset (set_scrollback needs &mut). The panes lock
            // already serializes this against append_output.
            let mut panes = self.panes.lock_or_recover();
            panes
                .iter_mut()
                .map(|(runtime_key, runtime)| {
                    let mut pane = runtime.pane.clone();
                    let output = if include_output {
                        let contents = runtime.parser.screen().contents();
                        let formatted = screen_contents_formatted(runtime.parser.screen());
                        let (cursor_row, cursor_col) = runtime.parser.screen().cursor_position();
                        pane.cursor_row = Some(cursor_row);
                        pane.cursor_col = Some(cursor_col);
                        let (screen_rows, screen_cols) = runtime.parser.screen().size();
                        pane.screen_rows = Some(screen_rows);
                        pane.screen_cols = Some(screen_cols);
                        update_pane_terminal_modes(&mut pane, runtime.parser.screen());
                        // Rebuild the styled scrollback only when the pane
                        // produced new output since the last snapshot; otherwise
                        // reuse the cached string and skip the offset walk.
                        if runtime.scrollback_formatted_generation != runtime.output_generation {
                            runtime.scrollback_formatted_cache = screen_scrollback_formatted(
                                &mut runtime.parser,
                                SCROLLBACK_FORMATTED_ROW_CAP,
                            );
                            runtime.scrollback_formatted_generation = runtime.output_generation;
                        }
                        Some((
                            contents,
                            formatted,
                            runtime.joined_output(),
                            runtime.scrollback_formatted_cache.clone(),
                        ))
                    } else {
                        None
                    };
                    Collected {
                        runtime_key: runtime_key.clone(),
                        pane,
                        output,
                    }
                })
                .collect::<Vec<_>>()
        };

        for Collected {
            runtime_key,
            mut pane,
            output,
        } in collected
        {
            if let Some((contents, formatted, raw, scrollback_formatted)) = output {
                pane.output = contents;
                pane.output_formatted = formatted;
                pane.scrollback = trim_output(raw, SCROLLBACK_CAP);
                pane.scrollback_formatted = scrollback_formatted;
            } else if let Some(stored) = session.panes.get(&pane.id) {
                pane.output = stored.output.clone();
                pane.output_formatted = stored.output_formatted.clone();
                pane.scrollback = stored.scrollback.clone();
                pane.scrollback_formatted = stored.scrollback_formatted.clone();
            }
            if let Some(stored) = session.panes.get_mut(&pane.id) {
                if runtime_key_is_active_for_pane(stored, &runtime_key) {
                    let tabs = stored.tabs.clone();
                    let active_tab = stored.active_tab.clone();
                    let metadata = stored.metadata.clone();
                    pane.tabs = tabs;
                    pane.active_tab = active_tab;
                    pane.metadata = metadata;
                    sync_active_pane_tab(&mut pane);
                    session.panes.insert(pane.id.clone(), pane);
                } else if let Some(tab_id) = runtime_key_tab(&pane.id, &runtime_key) {
                    sync_tab_from_runtime_pane(stored, tab_id, &pane);
                }
            } else {
                session.panes.insert(pane.id.clone(), pane);
            }
        }
        Ok(session)
    }

    fn agent_summary(&self) -> Result<serde_json::Value> {
        let snapshot = self.snapshot(false)?;
        let panes = snapshot
            .workspaces
            .iter()
            .flat_map(|workspace| {
                workspace.panes.iter().filter_map(|pane_id| {
                    let pane = snapshot.panes.get(pane_id)?;
                    Some(serde_json::json!({
                        "workspace": workspace.id,
                        "workspace_name": workspace.name,
                        "pane": pane.id,
                        "surface_kind": pane.surface_kind,
                        "title": pane.title,
                        "command": pane.command,
                        "status": pane.status,
                        "agent_status": pane.agent_status,
                        "progress": pane.progress,
                        "metadata": pane.metadata.clone(),
                        "exit_code": pane.exit_code,
                        "pid": pane.pid,
                        "notification": pane.notification_message,
                        "updated_at": pane.updated_at,
                    }))
                })
            })
            .collect::<Vec<_>>();
        Ok(serde_json::json!({
            "session": snapshot.name,
            "active_workspace": snapshot.active_workspace,
            "panes": panes,
        }))
    }

    fn identify(&self, pane: Option<String>) -> Result<serde_json::Value> {
        let snapshot = self.snapshot(false)?;
        let target_pane = pane
            .or_else(|| {
                snapshot
                    .workspaces
                    .iter()
                    .find(|workspace| workspace.id == snapshot.active_workspace)
                    .and_then(|workspace| workspace.active_pane.clone())
            })
            .ok_or_else(|| anyhow!("no active pane"))?;

        let workspace = snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.panes.iter().any(|item| item == &target_pane))
            .ok_or_else(|| anyhow!("pane {target_pane} is not attached to a workspace"))?;
        let pane = snapshot
            .panes
            .get(&target_pane)
            .ok_or_else(|| anyhow!("unknown pane {target_pane}"))?;
        let daemon = self.daemon_info();

        Ok(serde_json::json!({
            "session": snapshot.name,
            "workspace": workspace.id,
            "workspace_name": workspace.name,
            "pane": pane.id,
            "surface_kind": pane.surface_kind,
            "title": pane.title,
            "command": pane.command,
            "cwd": workspace.cwd,
            "git_branch": workspace.git_branch,
            "pull_request": workspace.pull_request,
            "ports": workspace.ports,
            "status": pane.status,
            "agent_status": pane.agent_status,
            "progress": pane.progress,
            "metadata": pane.metadata.clone(),
            "socket_path": daemon.socket_path,
            "pid_path": daemon.pid_path,
            "log_path": daemon.log_path,
            "state_path": daemon.state_path,
            "env": {
                "VMUX_SESSION": snapshot.name,
                "VMUX_WORKSPACE_ID": workspace.id,
                "VMUX_PANE_ID": pane.id,
                "VMUX_SURFACE_ID": pane.id,
                "VMUX_SOCKET_PATH": daemon.socket_path,
                "VMUX_PID_PATH": daemon.pid_path,
                "VMUX_LOG_PATH": daemon.log_path,
                "VMUX_STATE_PATH": daemon.state_path,
            }
        }))
    }

    fn daemon_info(&self) -> DaemonInfo {
        DaemonInfo {
            pid: std::process::id(),
            socket_path: self.socket_path.display().to_string(),
            pid_path: self.pid_path.display().to_string(),
            log_path: self.log_path.display().to_string(),
            state_path: self.state_path.display().to_string(),
            started_at: self.started_at,
        }
    }

    fn write_pid_file(&self) -> Result<()> {
        fs::write(&self.pid_path, format!("{}\n", std::process::id()))
            .with_context(|| format!("write pid file {}", self.pid_path.display()))
    }

    fn log(&self, message: &str) -> Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
            .with_context(|| format!("open log {}", self.log_path.display()))?;
        writeln!(file, "{} {}", unix_time(), message)?;
        Ok(())
    }

    fn cleanup_runtime_files(&self) {
        fs::remove_file(&self.socket_path).ok();
        fs::remove_file(&self.pid_path).ok();
    }

    fn save(&self) -> Result<()> {
        // Keep active-tab records in sync with live layout fields before
        // persisting (inactive tabs already hold their own layout).
        {
            let mut session = self.session.lock_or_recover();
            session.flush_tabs();
        }
        let mut snapshot = self.snapshot(false)?;
        snapshot.daemon = None;
        let payload = serde_json::to_vec_pretty(&snapshot)?;
        // Serialize writes so concurrent handlers can't interleave into a shared
        // temp file or race the rename. Each save also uses a unique temp name
        // (per-save counter + pid) so a stray writer never clobbers ours.
        let mut counter = self.save_lock.lock_or_recover();
        *counter = counter.wrapping_add(1);
        let tmp =
            self.state_path
                .with_extension(format!("json.tmp.{}.{}", std::process::id(), *counter));
        fs::write(&tmp, &payload)?;
        if let Err(err) = fs::rename(&tmp, &self.state_path) {
            fs::remove_file(&tmp).ok();
            return Err(err.into());
        }
        Ok(())
    }
}

fn next_number<'a>(prefix: &str, ids: impl Iterator<Item = &'a str>) -> u64 {
    ids.filter_map(|id| id.strip_prefix(prefix))
        .filter_map(|rest| rest.parse::<u64>().ok())
        .max()
        .unwrap_or(0)
        + 1
}

fn pane_runtime_key(pane_id: &str, tab_id: Option<&str>) -> String {
    tab_id
        .map(|tab_id| format!("{pane_id}::{tab_id}"))
        .unwrap_or_else(|| pane_id.to_string())
}

fn active_runtime_key_for_pane(pane: &Pane) -> String {
    pane_runtime_key(&pane.id, pane.active_tab.as_deref())
}

fn legacy_runtime_key(pane_id: &str) -> String {
    pane_id.to_string()
}

fn sanitize_pane_size(size: PaneSize) -> PaneSize {
    PaneSize {
        rows: size.rows.max(2),
        cols: size.cols.max(2),
    }
}

fn runtime_key_tab<'a>(pane_id: &str, runtime_key: &'a str) -> Option<&'a str> {
    runtime_key
        .strip_prefix(pane_id)
        .and_then(|rest| rest.strip_prefix("::"))
}

fn runtime_key_is_active_for_pane(pane: &Pane, runtime_key: &str) -> bool {
    active_runtime_key_for_pane(pane) == runtime_key
        || (pane.active_tab.is_none() && legacy_runtime_key(&pane.id) == runtime_key)
}

fn push_event(session: &mut Session, event: EventRecord) {
    session.events.push(event);
    let len = session.events.len();
    if len > 500 {
        session.events.drain(0..len - 500);
    }
}

fn agent_status_name(status: &AgentStatus) -> &'static str {
    match status {
        AgentStatus::Unknown => "unknown",
        AgentStatus::Idle => "idle",
        AgentStatus::Busy => "busy",
        AgentStatus::Attention => "attention",
        AgentStatus::Done => "done",
        AgentStatus::Error => "error",
    }
}

#[allow(dead_code)]
fn ensure_base_pane_tab(pane: &mut Pane) {
    if pane.tabs.is_empty() {
        let tab = PaneTab::from_pane("tab-1".to_string(), pane);
        pane.active_tab = Some(tab.id.clone());
        pane.tabs.push(tab);
    } else if pane
        .active_tab
        .as_ref()
        .map(|active| !pane.tabs.iter().any(|tab| &tab.id == active))
        .unwrap_or(true)
    {
        pane.active_tab = pane.tabs.first().map(|tab| tab.id.clone());
    }
}

#[allow(dead_code)]
fn sync_tab_from_runtime_pane(pane: &mut Pane, tab_id: &str, runtime_pane: &Pane) {
    if let Some(tab) = pane.tabs.iter_mut().find(|tab| tab.id == tab_id) {
        tab.title = runtime_pane.title.clone();
        tab.command = runtime_pane.command.clone();
        tab.surface_kind = runtime_pane.surface_kind.clone();
        tab.status = Some(runtime_pane.status.clone());
        tab.agent_status = Some(runtime_pane.agent_status.clone());
        tab.progress = runtime_pane.progress;
        tab.notification_color = runtime_pane.notification_color.clone();
        tab.notification_message = runtime_pane.notification_message.clone();
        tab.exit_code = runtime_pane.exit_code;
        tab.output = runtime_pane.output.clone();
        tab.output_formatted = runtime_pane.output_formatted.clone();
        tab.scrollback = runtime_pane.scrollback.clone();
        tab.scrollback_formatted = runtime_pane.scrollback_formatted.clone();
        tab.updated_at = runtime_pane.updated_at;
    }
}

#[allow(dead_code)]
fn sync_active_pane_tab(pane: &mut Pane) {
    let Some(active) = pane.active_tab.clone() else {
        return;
    };
    if let Some(tab) = pane.tabs.iter_mut().find(|tab| tab.id == active) {
        tab.title = pane.title.clone();
        tab.command = pane.command.clone();
        tab.surface_kind = pane.surface_kind.clone();
        tab.status = Some(pane.status.clone());
        tab.agent_status = Some(pane.agent_status.clone());
        tab.progress = pane.progress;
        tab.notification_color = pane.notification_color.clone();
        tab.notification_message = pane.notification_message.clone();
        tab.exit_code = pane.exit_code;
        tab.output = pane.output.clone();
        tab.output_formatted = pane.output_formatted.clone();
        tab.scrollback = pane.scrollback.clone();
        tab.scrollback_formatted = pane.scrollback_formatted.clone();
        tab.updated_at = unix_time();
    }
}

#[allow(dead_code)]
fn apply_pane_tab(pane: &mut Pane, selector: &str) -> Result<PaneTab> {
    let tab = resolve_pane_tab(pane, selector)?.clone();
    pane.active_tab = Some(tab.id.clone());
    pane.title = tab.title.clone();
    pane.command = normalize_tab_command(tab.command.clone());
    pane.surface_kind = tab.surface_kind.clone();
    if let Some(status) = tab.status.clone() {
        pane.status = status;
    }
    if let Some(agent_status) = tab.agent_status.clone() {
        pane.agent_status = agent_status;
    } else {
        pane.agent_status = infer_agent_status(&tab.output, &tab.command);
    }
    pane.progress = tab.progress;
    pane.notification_color = tab.notification_color.clone();
    pane.notification_message = tab.notification_message.clone();
    pane.exit_code = tab.exit_code;
    pane.output = tab.output.clone();
    pane.output_formatted = tab.output_formatted.clone();
    pane.scrollback = tab.scrollback.clone();
    pane.scrollback_formatted = tab.scrollback_formatted.clone();
    pane.updated_at = unix_time();
    Ok(tab)
}

#[allow(dead_code)]
fn normalize_tab_command(command: String) -> String {
    if command.trim().is_empty() {
        default_shell()
    } else {
        command
    }
}

fn normalize_metadata_key(key: &str) -> Result<String> {
    let key = key.trim();
    if key.is_empty() {
        return Err(anyhow!("metadata key cannot be empty"));
    }
    if key.chars().any(char::is_whitespace) {
        return Err(anyhow!("metadata key cannot contain whitespace"));
    }
    Ok(key.to_string())
}

fn resolve_workspace_tab_id(workspace: &crate::model::Workspace, selector: &str) -> Result<String> {
    let matches = workspace
        .tabs
        .iter()
        .filter(|tab| tab.id == selector || tab.title == selector)
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [tab] => Ok(tab.id.clone()),
        [] => Err(anyhow!("unknown tab {selector}")),
        _ => Err(anyhow!("tab selector {selector} is ambiguous")),
    }
}

#[allow(dead_code)]
fn resolve_pane_tab<'a>(pane: &'a Pane, selector: &str) -> Result<&'a PaneTab> {
    let matches = pane
        .tabs
        .iter()
        .filter(|tab| tab.id == selector || tab.title == selector)
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [tab] => Ok(tab),
        [] => Err(anyhow!("unknown tab {selector}")),
        _ => Err(anyhow!("pane tab selector {selector} is ambiguous")),
    }
}

#[allow(dead_code)]
fn resolve_pane_tab_mut<'a>(pane: &'a mut Pane, selector: &str) -> Result<&'a mut PaneTab> {
    let matches = pane
        .tabs
        .iter()
        .enumerate()
        .filter(|(_, tab)| tab.id == selector || tab.title == selector)
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let index = match matches.as_slice() {
        [index] => *index,
        [] => return Err(anyhow!("unknown tab {selector}")),
        _ => return Err(anyhow!("pane tab selector {selector} is ambiguous")),
    };
    Ok(&mut pane.tabs[index])
}

fn screen_contents_formatted(screen: &vt100::Screen) -> String {
    let (_, cols) = screen.size();
    screen
        .rows_formatted(0, cols)
        .map(|row| String::from_utf8_lossy(&row).into_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Build the styled scrollback history for a pane, oldest→newest, one grid row
/// per line (the same convention as [`screen_contents_formatted`]).
///
/// vt100 keeps styled scrollback history, but only the visible screen is exposed
/// with inline SGR by default. Each scrollback offset shifts the visible window
/// up by exactly one history row, so we walk offsets from the oldest retained
/// window (`available`) down to `1`, taking the new top row at each. The live
/// visible screen is then appended so the returned string spans the same
/// oldest→newest line window the plain `pane.scrollback` stream indexes into —
/// this keeps the UI's scroll math (and the offset 0↔1 boundary) aligned while
/// carrying color. History rows are capped at `row_cap` to bound CPU/payload.
///
/// The parser's current scrollback position is saved up front and
/// unconditionally restored before returning (no early returns in between).
fn screen_scrollback_formatted(parser: &mut vt100::Parser, row_cap: usize) -> String {
    let saved = parser.screen().scrollback();
    let (_, cols) = parser.screen().size();
    // set_scrollback clamps to the history length, so requesting a huge offset
    // and reading it back reports how many history rows are actually retained.
    parser.screen_mut().set_scrollback(usize::MAX);
    let available = parser.screen().scrollback();
    let take = available.min(row_cap);
    let mut rows: Vec<String> = Vec::with_capacity(take.saturating_add(1));
    for offset in (1..=take).rev() {
        parser.screen_mut().set_scrollback(offset);
        if let Some(row) = parser.screen().rows_formatted(0, cols).next() {
            rows.push(String::from_utf8_lossy(&row).into_owned());
        }
    }
    parser.screen_mut().set_scrollback(saved);
    let screen = screen_contents_formatted(parser.screen());
    if rows.is_empty() {
        screen
    } else if screen.is_empty() {
        rows.join("\n")
    } else {
        format!("{}\n{}", rows.join("\n"), screen)
    }
}

/// Row cap for [`screen_scrollback_formatted`]; bounds the per-snapshot walk and
/// the serialized payload while still covering deep scrolled-back views.
const SCROLLBACK_FORMATTED_ROW_CAP: usize = 500;

fn update_pane_terminal_modes(pane: &mut Pane, screen: &vt100::Screen) {
    pane.mouse_protocol_mode = match screen.mouse_protocol_mode() {
        vt100::MouseProtocolMode::None => String::new(),
        vt100::MouseProtocolMode::Press => "press".to_string(),
        vt100::MouseProtocolMode::PressRelease => "press-release".to_string(),
        vt100::MouseProtocolMode::ButtonMotion => "button-motion".to_string(),
        vt100::MouseProtocolMode::AnyMotion => "any-motion".to_string(),
    };
    pane.mouse_protocol_encoding = match screen.mouse_protocol_encoding() {
        vt100::MouseProtocolEncoding::Default => String::new(),
        vt100::MouseProtocolEncoding::Utf8 => "utf8".to_string(),
        vt100::MouseProtocolEncoding::Sgr => "sgr".to_string(),
    };
}

fn default_shell() -> String {
    crate::config::resolve_default_shell()
}

fn validate_url(url: &str) -> Result<()> {
    // This is a local dev tool: opening localhost/private previews is a core use
    // case, so we deliberately do NOT block private or loopback hosts. We only
    // harden parsing: reject empty input, whitespace/control characters, an
    // unsupported scheme, and a missing host (finding 15).
    if url.is_empty() {
        return Err(anyhow!("open-url requires a URL"));
    }
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(anyhow!(
            "URL must not contain whitespace or control characters"
        ));
    }
    let lower = url.to_ascii_lowercase();
    let rest = if let Some(rest) = lower.strip_prefix("http://") {
        rest
    } else if let Some(rest) = lower.strip_prefix("https://") {
        rest
    } else {
        return Err(anyhow!("open-url only supports http:// and https:// URLs"));
    };
    // Host is everything up to the first path/query/fragment separator, minus
    // any userinfo and port.
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host = host.split(':').next().unwrap_or(host);
    if host.is_empty() {
        return Err(anyhow!("URL is missing a host"));
    }
    Ok(())
}

fn url_open_command(url: &str) -> String {
    let browser = ["w3m", "lynx", "links", "elinks", "browsh"]
        .into_iter()
        .find(|candidate| command_exists(candidate));
    let argv = if let Some(browser) = browser {
        vec![browser.to_string(), url.to_string()]
    } else {
        vec![
            "curl".to_string(),
            "-L".to_string(),
            "--max-time".to_string(),
            "30".to_string(),
            url.to_string(),
        ]
    };
    shell_words::join(argv)
}

fn url_snapshot(url: &str) -> Result<serde_json::Value> {
    validate_url(url)?;
    let body = fetch_url_body(url, "url snapshot")?;
    let title = html_title(&body);
    let links = html_links(&body, url);
    let text = html_to_text(&body);
    Ok(serde_json::json!({
        "url": url,
        "title": title,
        "text": trim_output(text, 32_000),
        "links": links,
    }))
}

fn url_links(url: &str) -> Result<serde_json::Value> {
    let snapshot = url_snapshot(url)?;
    let links = snapshot
        .get("links")
        .cloned()
        .unwrap_or_else(|| serde_json::json!([]));
    Ok(serde_json::json!({
        "url": url,
        "title": snapshot.get("title").cloned().unwrap_or(serde_json::Value::Null),
        "links": links,
    }))
}

fn url_forms(url: &str) -> Result<serde_json::Value> {
    validate_url(url)?;
    let body = fetch_url_body(url, "url forms")?;
    Ok(serde_json::json!({
        "url": url,
        "title": html_title(&body),
        "forms": html_forms(&body, url),
    }))
}

fn url_evaluate(url: &str, expression: &str) -> Result<serde_json::Value> {
    validate_url(url)?;
    let body = fetch_url_body(url, "url evaluate")?;
    let expression = expression.trim();
    if expression.is_empty() {
        return Err(anyhow!("browser evaluate expression cannot be empty"));
    }
    let links = html_links(&body, url);
    let forms = html_forms(&body, url);
    let value = evaluate_static_expression(expression, &body, &links, &forms)?;
    Ok(serde_json::json!({
        "url": url,
        "engine": "static-html",
        "expression": expression,
        "value": value,
    }))
}

fn url_console(url: &str) -> Result<serde_json::Value> {
    validate_url(url)?;
    let body = fetch_url_body(url, "url console")?;
    let scripts = html_scripts(&body, url);
    Ok(serde_json::json!({
        "url": url,
        "engine": "static-html",
        "scripts": scripts,
        "console_calls": html_console_calls(&body),
        "noscript": html_noscript_blocks(&body),
    }))
}

fn url_network(url: &str) -> Result<serde_json::Value> {
    validate_url(url)?;
    let started = Instant::now();
    let output = Command::new("curl")
        .arg("-L")
        .arg("--max-time")
        .arg("30")
        .arg("-sS")
        .arg("-D")
        .arg("-")
        .arg("-o")
        .arg("/dev/null")
        .arg("-w")
        .arg("\nVMUX_CURL_META\t%{http_code}\t%{url_effective}\t%{content_type}\t%{size_download}\t%{time_total}\t%{num_redirects}\n")
        .arg(url)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("run curl for url network")?;
    let elapsed_ms = started.elapsed().as_millis();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let (headers, meta) = parse_curl_network_output(&stdout);
    if !output.status.success() {
        return Err(anyhow!(
            "url network failed: {}",
            if stderr.is_empty() {
                "curl failed"
            } else {
                &stderr
            }
        ));
    }
    Ok(serde_json::json!({
        "url": url,
        "elapsed_ms": elapsed_ms,
        "status": meta.status,
        "effective_url": meta.effective_url,
        "content_type": meta.content_type,
        "bytes": meta.bytes,
        "curl_time_total": meta.time_total,
        "redirects": meta.redirects,
        "headers": headers,
    }))
}

fn fetch_url_body(url: &str, label: &str) -> Result<String> {
    let output = Command::new("curl")
        .arg("-L")
        .arg("--max-time")
        .arg("30")
        .arg("-sS")
        .arg(url)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("run curl for {label}"))?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(anyhow!(
            "{label} failed: {}",
            if error.is_empty() {
                "curl failed"
            } else {
                &error
            }
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[derive(Debug, Default, PartialEq)]
struct CurlNetworkMeta {
    status: Option<u16>,
    effective_url: Option<String>,
    content_type: Option<String>,
    bytes: Option<u64>,
    time_total: Option<f64>,
    redirects: Option<u64>,
}

fn parse_curl_network_output(output: &str) -> (Vec<serde_json::Value>, CurlNetworkMeta) {
    let mut headers = Vec::new();
    let mut meta = CurlNetworkMeta::default();
    for line in output.lines() {
        if let Some(rest) = line.strip_prefix("VMUX_CURL_META\t") {
            let parts = rest.split('\t').collect::<Vec<_>>();
            meta.status = parts.first().and_then(|value| value.parse().ok());
            meta.effective_url = parts
                .get(1)
                .filter(|value| !value.is_empty())
                .map(|value| (*value).to_string());
            meta.content_type = parts
                .get(2)
                .filter(|value| !value.is_empty())
                .map(|value| (*value).to_string());
            meta.bytes = parts.get(3).and_then(|value| value.parse().ok());
            meta.time_total = parts.get(4).and_then(|value| value.parse().ok());
            meta.redirects = parts.get(5).and_then(|value| value.parse().ok());
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim();
            if !name.is_empty() {
                headers.push(serde_json::json!({
                    "name": name,
                    "value": value.trim(),
                }));
            }
        }
    }
    (headers, meta)
}

fn evaluate_static_expression(
    expression: &str,
    html: &str,
    links: &[serde_json::Value],
    forms: &[serde_json::Value],
) -> Result<serde_json::Value> {
    match expression {
        "title" | "document.title" => Ok(html_title(html)
            .map(serde_json::Value::String)
            .unwrap_or(serde_json::Value::Null)),
        "text" | "document.body.innerText" | "body.innerText" => Ok(serde_json::Value::String(
            trim_output(html_to_text(html), 32_000),
        )),
        "links" => Ok(serde_json::Value::Array(links.to_vec())),
        "forms" => Ok(serde_json::Value::Array(forms.to_vec())),
        expression => {
            if let Some(index) = indexed_expression(expression, "links") {
                return links
                    .get(index)
                    .cloned()
                    .ok_or_else(|| anyhow!("link index {} out of range", index + 1));
            }
            if let Some(index) = indexed_expression(expression, "forms") {
                return forms
                    .get(index)
                    .cloned()
                    .ok_or_else(|| anyhow!("form index {} out of range", index + 1));
            }
            if let Some((index, field)) = indexed_field_expression(expression, "links") {
                return links
                    .get(index)
                    .and_then(|link| link.get(field))
                    .cloned()
                    .ok_or_else(|| anyhow!("link index {} has no field {field}", index + 1));
            }
            if let Some((index, field)) = indexed_field_expression(expression, "forms") {
                return forms
                    .get(index)
                    .and_then(|form| form.get(field))
                    .cloned()
                    .ok_or_else(|| anyhow!("form index {} has no field {field}", index + 1));
            }
            if let Some(selector) = expression.strip_prefix("text:") {
                return Ok(serde_json::Value::String(trim_output(
                    html_elements_text(html, selector).join("\n"),
                    32_000,
                )));
            }
            if let Some(selector) = expression.strip_prefix("selector:") {
                return Ok(serde_json::Value::Array(
                    html_elements_text(html, selector)
                        .into_iter()
                        .map(serde_json::Value::String)
                        .collect(),
                ));
            }
            Err(anyhow!(
                "unsupported static browser expression {expression}; try title, text, links, forms, links[1], links[1].href, text:h1, or selector:p"
            ))
        }
    }
}

fn indexed_expression(expression: &str, name: &str) -> Option<usize> {
    let rest = expression.strip_prefix(name)?.strip_prefix('[')?;
    let (index, suffix) = rest.split_once(']')?;
    if !suffix.is_empty() {
        return None;
    }
    one_based_index(index)
}

fn indexed_field_expression<'a>(expression: &'a str, name: &str) -> Option<(usize, &'a str)> {
    let rest = expression.strip_prefix(name)?.strip_prefix('[')?;
    let (index, suffix) = rest.split_once("].")?;
    let field = suffix.trim();
    if field.is_empty() {
        return None;
    }
    Some((one_based_index(index)?, field))
}

fn one_based_index(value: &str) -> Option<usize> {
    value.trim().parse::<usize>().ok()?.checked_sub(1)
}

fn html_scripts(html: &str, base_url: &str) -> Vec<serde_json::Value> {
    html_tag_blocks(html, "script")
        .into_iter()
        .enumerate()
        .map(|(index, (attrs, body))| {
            let src = html_attr(attrs, "src").map(|src| absolutize_url(base_url, &src));
            let inline = src.is_none();
            let script_type = html_attr(attrs, "type").unwrap_or_else(|| "text/javascript".into());
            serde_json::json!({
                "index": index + 1,
                "src": src,
                "type": script_type,
                "inline": inline,
                "bytes": body.len(),
                "preview": trim_output(body.trim().to_string(), 500),
            })
        })
        .collect()
}

fn html_console_calls(html: &str) -> Vec<serde_json::Value> {
    let mut calls = Vec::new();
    for (_, body) in html_tag_blocks(html, "script") {
        let mut rest = body.as_str();
        while let Some(index) = rest.find("console.") {
            rest = &rest[index + "console.".len()..];
            let method = rest
                .chars()
                .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
                .collect::<String>();
            if method.is_empty() {
                continue;
            }
            let preview = rest
                .find(';')
                .map(|end| &rest[..end])
                .unwrap_or(rest)
                .trim();
            calls.push(serde_json::json!({
                "method": method,
                "preview": trim_output(preview.to_string(), 500),
            }));
            rest = preview
                .len()
                .checked_add(1)
                .and_then(|offset| rest.get(offset..))
                .unwrap_or("");
        }
    }
    calls
}

fn html_noscript_blocks(html: &str) -> Vec<String> {
    html_tag_blocks(html, "noscript")
        .into_iter()
        .map(|(_, body)| html_to_text(&body))
        .filter(|text| !text.trim().is_empty())
        .collect()
}

fn html_tag_blocks<'a>(html: &'a str, tag: &str) -> Vec<(&'a str, String)> {
    let mut blocks = Vec::new();
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut rest = html;
    while let Some(index) = rest.to_ascii_lowercase().find(&open) {
        rest = &rest[index + open.len()..];
        let Some(tag_end) = rest.find('>') else {
            break;
        };
        let attrs = &rest[..tag_end];
        let after = &rest[tag_end + 1..];
        let lower_after = after.to_ascii_lowercase();
        let Some(body_end) = lower_after.find(&close) else {
            break;
        };
        blocks.push((attrs, after[..body_end].to_string()));
        rest = &after[body_end + close.len()..];
    }
    blocks
}

fn html_elements_text(html: &str, selector: &str) -> Vec<String> {
    let selector = selector.trim().trim_start_matches('.');
    if selector.is_empty()
        || !selector
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
    {
        return Vec::new();
    }
    html_tag_blocks(html, selector)
        .into_iter()
        .map(|(_, body)| html_to_text(&body))
        .filter(|text| !text.trim().is_empty())
        .collect()
}

fn form_default_fields(form: &serde_json::Value) -> BTreeMap<String, String> {
    let mut values = BTreeMap::new();
    if let Some(fields) = form.get("fields").and_then(|fields| fields.as_array()) {
        for field in fields {
            let Some(name) = field.get("name").and_then(|name| name.as_str()) else {
                continue;
            };
            let value = field
                .get("value")
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            values.insert(name.to_string(), value.to_string());
        }
    }
    values
}

fn form_submission_target(
    action: &str,
    method: &str,
    values: &BTreeMap<String, String>,
) -> Result<String> {
    match method {
        "get" => Ok(url_with_query(action, values)),
        "post" => {
            let mut argv = vec![
                "curl".to_string(),
                "-L".to_string(),
                "-X".to_string(),
                "POST".to_string(),
            ];
            for (name, value) in values {
                argv.push("--data-urlencode".to_string());
                argv.push(format!("{name}={value}"));
            }
            argv.push(action.to_string());
            Ok(shell_words::join(argv))
        }
        other => Err(anyhow!("unsupported form method {other}")),
    }
}

fn url_with_query(url: &str, values: &BTreeMap<String, String>) -> String {
    if values.is_empty() {
        return url.to_string();
    }
    let query = values
        .iter()
        .map(|(name, value)| format!("{}={}", url_encode(name), url_encode(value)))
        .collect::<Vec<_>>()
        .join("&");
    let separator = if url.contains('?') { "&" } else { "?" };
    format!("{url}{separator}{query}")
}

fn url_encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else if byte == b' ' {
            out.push('+');
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn compact_url_title(url: &str) -> String {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    rest.split('/').next().unwrap_or(rest).to_string()
}

fn command_exists(command: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(command).is_file())
}

fn html_title(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let start = lower.find("<title")?;
    let after_tag = html[start..].find('>')? + start + 1;
    let end = lower[after_tag..].find("</title>")? + after_tag;
    let title = html_unescape(&html[after_tag..end]).trim().to_string();
    if title.is_empty() {
        None
    } else {
        Some(title)
    }
}

fn html_links(html: &str, base_url: &str) -> Vec<serde_json::Value> {
    let mut links = Vec::new();
    let mut rest = html;
    while let Some(index) = rest.to_ascii_lowercase().find("<a ") {
        rest = &rest[index + 3..];
        let Some(end) = rest.find('>') else {
            break;
        };
        let attrs = &rest[..end];
        if let Some(href) = html_attr(attrs, "href") {
            let after = &rest[end + 1..];
            let lower_after = after.to_ascii_lowercase();
            let text = lower_after
                .find("</a>")
                .map(|end| html_to_text(&after[..end]))
                .unwrap_or_default();
            links.push(serde_json::json!({
                "href": absolutize_url(base_url, &href),
                "text": text.trim(),
            }));
        }
        rest = &rest[end + 1..];
    }
    links
}

fn html_forms(html: &str, base_url: &str) -> Vec<serde_json::Value> {
    let mut forms = Vec::new();
    let mut rest = html;
    while let Some(index) = rest.to_ascii_lowercase().find("<form") {
        rest = &rest[index + 5..];
        let Some(tag_end) = rest.find('>') else {
            break;
        };
        let attrs = &rest[..tag_end];
        let after = &rest[tag_end + 1..];
        let lower_after = after.to_ascii_lowercase();
        let body_end = lower_after.find("</form>").unwrap_or(after.len());
        let body = &after[..body_end];
        let method = html_attr(attrs, "method")
            .unwrap_or_else(|| "get".to_string())
            .to_ascii_lowercase();
        let action = html_attr(attrs, "action")
            .map(|action| absolutize_url(base_url, &action))
            .unwrap_or_else(|| base_url.to_string());
        let label = html_attr(attrs, "aria-label")
            .or_else(|| html_attr(attrs, "name"))
            .or_else(|| html_attr(attrs, "id"));
        forms.push(serde_json::json!({
            "index": forms.len() + 1,
            "method": method,
            "action": action,
            "label": label,
            "fields": html_form_fields(body),
        }));
        rest = if body_end < after.len() {
            &after[body_end + "</form>".len()..]
        } else {
            ""
        };
    }
    forms
}

fn html_form_fields(html: &str) -> Vec<serde_json::Value> {
    let mut fields = Vec::new();
    let mut rest = html;
    while let Some(index) = rest.to_ascii_lowercase().find("<input") {
        rest = &rest[index + 6..];
        let Some(end) = rest.find('>') else {
            break;
        };
        let attrs = &rest[..end];
        if let Some(name) = html_attr(attrs, "name") {
            let field_type = html_attr(attrs, "type").unwrap_or_else(|| "text".to_string());
            if !matches!(field_type.as_str(), "submit" | "button" | "reset" | "image") {
                fields.push(serde_json::json!({
                    "name": name,
                    "type": field_type,
                    "value": html_attr(attrs, "value").unwrap_or_default(),
                }));
            }
        }
        rest = &rest[end + 1..];
    }

    let mut rest = html;
    while let Some(index) = rest.to_ascii_lowercase().find("<textarea") {
        rest = &rest[index + 9..];
        let Some(end) = rest.find('>') else {
            break;
        };
        let attrs = &rest[..end];
        let after = &rest[end + 1..];
        let lower_after = after.to_ascii_lowercase();
        let value_end = lower_after.find("</textarea>").unwrap_or(0);
        if let Some(name) = html_attr(attrs, "name") {
            fields.push(serde_json::json!({
                "name": name,
                "type": "textarea",
                "value": html_unescape(&after[..value_end]).trim(),
            }));
        }
        rest = if value_end < after.len() {
            &after[value_end + "</textarea>".len()..]
        } else {
            ""
        };
    }

    let mut rest = html;
    while let Some(index) = rest.to_ascii_lowercase().find("<select") {
        rest = &rest[index + 7..];
        let Some(end) = rest.find('>') else {
            break;
        };
        let attrs = &rest[..end];
        let after = &rest[end + 1..];
        let lower_after = after.to_ascii_lowercase();
        let body_end = lower_after.find("</select>").unwrap_or(0);
        if let Some(name) = html_attr(attrs, "name") {
            fields.push(serde_json::json!({
                "name": name,
                "type": "select",
                "value": selected_option_value(&after[..body_end]).unwrap_or_default(),
            }));
        }
        rest = if body_end < after.len() {
            &after[body_end + "</select>".len()..]
        } else {
            ""
        };
    }

    fields
}

fn selected_option_value(html: &str) -> Option<String> {
    let mut first = None;
    let mut rest = html;
    while let Some(index) = rest.to_ascii_lowercase().find("<option") {
        rest = &rest[index + 7..];
        let Some(end) = rest.find('>') else {
            break;
        };
        let attrs = &rest[..end];
        let value = html_attr(attrs, "value").unwrap_or_else(|| {
            let after = &rest[end + 1..];
            let lower_after = after.to_ascii_lowercase();
            lower_after
                .find("</option>")
                .map(|end| html_to_text(&after[..end]))
                .unwrap_or_default()
                .trim()
                .to_string()
        });
        if first.is_none() {
            first = Some(value.clone());
        }
        if attrs.to_ascii_lowercase().contains("selected") {
            return Some(value);
        }
        rest = &rest[end + 1..];
    }
    first
}

fn html_attr(attrs: &str, name: &str) -> Option<String> {
    for quote in ['"', '\''] {
        let needle = format!("{name}={quote}");
        if let Some(start) = attrs.to_ascii_lowercase().find(&needle) {
            let value_start = start + needle.len();
            let value_end = attrs[value_start..].find(quote)? + value_start;
            return Some(html_unescape(&attrs[value_start..value_end]));
        }
    }
    None
}

fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut entity = String::new();
    let mut in_entity = false;
    for ch in html.chars() {
        if in_tag {
            if ch == '>' {
                in_tag = false;
                out.push(' ');
            }
            continue;
        }
        if in_entity {
            if ch == ';' {
                out.push_str(&html_unescape_entity(&entity));
                entity.clear();
                in_entity = false;
            } else if entity.len() < 16 {
                entity.push(ch);
            } else {
                out.push('&');
                out.push_str(&entity);
                entity.clear();
                in_entity = false;
            }
            continue;
        }
        match ch {
            '<' => in_tag = true,
            '&' => in_entity = true,
            ch => out.push(ch),
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn html_unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '&' {
            out.push(ch);
            continue;
        }
        let mut entity = String::new();
        while let Some(next) = chars.peek().copied() {
            if next == ';' {
                chars.next();
                out.push_str(&html_unescape_entity(&entity));
                entity.clear();
                break;
            }
            if entity.len() >= 16 || !(next.is_ascii_alphanumeric() || next == '#') {
                out.push('&');
                out.push_str(&entity);
                entity.clear();
                break;
            }
            entity.push(next);
            chars.next();
        }
        if !entity.is_empty() {
            out.push('&');
            out.push_str(&entity);
        }
    }
    out
}

fn html_unescape_entity(entity: &str) -> String {
    match entity {
        "amp" => "&".to_string(),
        "lt" => "<".to_string(),
        "gt" => ">".to_string(),
        "quot" => "\"".to_string(),
        "apos" => "'".to_string(),
        _ => format!("&{entity};"),
    }
}

fn absolutize_url(base_url: &str, href: &str) -> String {
    if href.starts_with("http://") || href.starts_with("https://") {
        return href.to_string();
    }
    let Some((scheme, rest)) = base_url.split_once("://") else {
        return href.to_string();
    };
    let host = rest.split('/').next().unwrap_or(rest);
    if href.starts_with('/') {
        format!("{scheme}://{host}{href}")
    } else {
        let base_dir = rest
            .rsplit_once('/')
            .map(|(dir, _)| dir)
            .filter(|dir| dir.contains('/'))
            .unwrap_or(host);
        format!("{scheme}://{base_dir}/{href}")
    }
}

fn parse_agent_status(status: &str) -> AgentStatus {
    match status {
        "idle" => AgentStatus::Idle,
        "busy" | "running" | "working" => AgentStatus::Busy,
        "attention" | "needs-input" | "needs_input" | "waiting" | "blocked" | "approval" => {
            AgentStatus::Attention
        }
        "done" | "complete" => AgentStatus::Done,
        "error" | "failed" => AgentStatus::Error,
        _ => AgentStatus::Unknown,
    }
}

/// Apply a hook/CLI status so it sticks in the sidebar (pinned).
///
/// Done/busy/error clear the pane notification banner so workspace_status shows
/// ✅/🔄/❌ instead of treating every notify as 🙋 attention.
fn apply_explicit_agent_status(pane: &mut Pane, status: &str, color: Option<&str>, message: &str) {
    let parsed = parse_agent_status(status);
    let pinned = !matches!(parsed, AgentStatus::Unknown);
    touch_agent_status(pane, parsed.clone(), pinned);
    match parsed {
        AgentStatus::Attention => {
            pane.notification_color = color
                .map(str::to_string)
                .or_else(|| Some("blue".to_string()));
            if !message.is_empty() {
                pane.notification_message = Some(message.to_string());
            }
        }
        AgentStatus::Done | AgentStatus::Busy | AgentStatus::Error | AgentStatus::Idle => {
            // Status emoji is driven by agent_status, not the notification banner.
            pane.notification_message = None;
            pane.notification_color = color.map(str::to_string);
        }
        AgentStatus::Unknown => {
            if !message.is_empty() {
                pane.notification_message = Some(message.to_string());
            }
            pane.notification_color = color.map(str::to_string);
        }
    }
}

/// Heuristic: keystrokes that start a new agent turn (not pure mouse/CSI noise).
fn looks_like_user_turn_input(data: &str) -> bool {
    if data.is_empty() {
        return false;
    }
    // Mouse reports / focus / paste bracket — do not flip agent status.
    if data.contains("\u{1b}[<")
        || data.contains("\u{1b}[M")
        || data.contains("\u{1b}[I")
        || data.contains("\u{1b}[O")
        || data.contains("\u{1b}[200~")
        || data.contains("\u{1b}[201~")
    {
        return false;
    }
    // Enter alone submits a prompt.
    if data == "\r" || data == "\n" || data == "\r\n" {
        return true;
    }
    // Pure CSI / SS3 (arrows, function keys) without printable text.
    if data.starts_with('\u{1b}') && !data.chars().any(|c| c.is_ascii_graphic() && c != '[') {
        return false;
    }
    if data.starts_with("\u{1b}[") || data.starts_with("\u{1b}O") {
        // Arrow keys etc.
        if !data
            .chars()
            .any(|c| c.is_ascii_alphanumeric() && data.len() > 8)
        {
            return data.contains('\r') || data.contains('\n');
        }
    }
    // Printable text or paste payloads.
    data.contains('\r')
        || data.contains('\n')
        || data.chars().any(|c| c.is_ascii_graphic() || c == ' ')
}

fn notification_workspace<'a>(
    session: &'a Session,
    note: &Notification,
) -> Option<&'a crate::model::Workspace> {
    if let Some(workspace_id) = &note.workspace {
        return session
            .workspaces
            .iter()
            .find(|workspace| &workspace.id == workspace_id);
    }
    let pane_id = note.pane.as_ref()?;
    session
        .workspaces
        .iter()
        .find(|workspace| workspace.panes.iter().any(|item| item == pane_id))
}

fn normalize_pane_title(title: Option<String>) -> Result<Option<String>> {
    title.map(normalize_required_pane_title).transpose()
}

/// Trim and validate a title that is always supplied. Callers that already have
/// a `String` use this instead of `normalize_pane_title(Some(..))?.expect(..)`
/// so a future change to the optional variant can never panic the daemon.
fn normalize_required_pane_title(title: String) -> Result<String> {
    let title = title.trim();
    if title.is_empty() {
        return Err(anyhow!("pane title cannot be empty"));
    }
    Ok(title.to_string())
}

fn normalize_cwd(cwd: Option<String>) -> Result<PathBuf> {
    // Prefer the user's launch directory over the daemon process cwd (`/`).
    let base = crate::model::launch_cwd();
    let cwd = cwd.map(PathBuf::from).unwrap_or_else(|| base.clone());
    let cwd = if cwd.is_absolute() {
        cwd
    } else {
        base.join(cwd)
    };
    if !cwd.is_dir() {
        return Err(anyhow!(
            "workspace cwd is not a directory: {}",
            cwd.display()
        ));
    }
    Ok(cwd)
}

/// Fix workspaces that still have the daemon's chdir target (`/`) as cwd.
fn repair_workspace_cwds(session: &mut Session) {
    let launch = crate::model::default_cwd();
    if launch == "/" {
        return;
    }
    for workspace in &mut session.workspaces {
        if workspace.cwd.is_empty() || workspace.cwd == "/" || workspace.cwd == "." {
            workspace.cwd = launch.clone();
        }
    }
}

fn load_custom_actions(cwd: &Path) -> Result<Option<(PathBuf, Vec<CustomAction>)>> {
    let Some(path) = find_vmux_config(cwd) else {
        return Ok(None);
    };
    let config: LmuxConfig = serde_json::from_str(
        &fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?,
    )
    .with_context(|| format!("parse {}", path.display()))?;
    for action in &config.commands {
        if action.name.trim().is_empty() {
            return Err(anyhow!(
                "custom action in {} has empty name",
                path.display()
            ));
        }
        if action.command.trim().is_empty() {
            return Err(anyhow!(
                "custom action {} in {} has empty command",
                action.name,
                path.display()
            ));
        }
    }
    Ok(Some((path, config.commands)))
}

fn find_vmux_config(cwd: &Path) -> Option<PathBuf> {
    for dir in cwd.ancestors() {
        for name in ["vmux.json", ".vmux.json"] {
            let path = dir.join(name);
            if path.is_file() {
                return Some(path);
            }
        }
    }
    None
}

/// Hard deadline for metadata subprocesses (git/gh/ss). `gh pr view` in
/// particular makes an unbounded network call; without a timeout a slow or
/// hung network would stall the background meta loop indefinitely.
const METADATA_SUBPROCESS_TIMEOUT: Duration = Duration::from_secs(3);

/// Run `command` with piped output and a hard deadline. Returns the captured
/// output if the child exits in time, or `None` if it errors or is killed for
/// exceeding `timeout`. std-only: spawn, then poll `try_wait`, killing on
/// expiry. Suitable for the small-output metadata commands here.
fn run_with_timeout(mut command: Command, timeout: Duration) -> Option<std::process::Output> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().ok()?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return None,
        }
    }
}

fn git_branch(cwd: &Path) -> Option<String> {
    for args in [
        ["rev-parse", "--abbrev-ref", "HEAD"],
        ["symbolic-ref", "--short", "HEAD"],
    ] {
        let mut command = Command::new("git");
        command.arg("-C").arg(cwd).args(args);
        let output = run_with_timeout(command, METADATA_SUBPROCESS_TIMEOUT)?;
        if output.status.success() {
            let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !branch.is_empty() && branch != "HEAD" {
                return Some(branch);
            }
        }
    }
    None
}

fn pull_request_info(cwd: &Path) -> Option<PullRequestInfo> {
    let mut command = Command::new("gh");
    command
        .arg("pr")
        .arg("view")
        .arg("--json")
        .arg("number,state,title,url,isDraft")
        .current_dir(cwd);
    let output = run_with_timeout(command, METADATA_SUBPROCESS_TIMEOUT)?;
    if !output.status.success() {
        return None;
    }
    parse_pull_request_info(&String::from_utf8_lossy(&output.stdout))
}

fn parse_pull_request_info(output: &str) -> Option<PullRequestInfo> {
    let value: serde_json::Value = serde_json::from_str(output).ok()?;
    Some(PullRequestInfo {
        number: value.get("number")?.as_u64()?,
        state: value
            .get("state")
            .and_then(|item| item.as_str())
            .unwrap_or("UNKNOWN")
            .to_string(),
        title: value
            .get("title")
            .and_then(|item| item.as_str())
            .map(ToOwned::to_owned),
        url: value
            .get("url")
            .and_then(|item| item.as_str())
            .map(ToOwned::to_owned),
        draft: value
            .get("isDraft")
            .and_then(|item| item.as_bool())
            .unwrap_or(false),
    })
}

fn listening_ports_for_roots(roots: &[u32]) -> Vec<ListeningPort> {
    if roots.is_empty() {
        return Vec::new();
    }
    let owned = descendant_pids(roots);
    if owned.is_empty() {
        return Vec::new();
    }
    let mut command = Command::new("ss");
    command.args(["-H", "-ltnp"]);
    let Some(output) = run_with_timeout(command, METADATA_SUBPROCESS_TIMEOUT) else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    parse_listening_ports(&String::from_utf8_lossy(&output.stdout), &owned)
}

fn descendant_pids(roots: &[u32]) -> BTreeSet<u32> {
    let mut owned = roots.iter().copied().collect::<BTreeSet<_>>();
    let mut changed = true;
    while changed {
        changed = false;
        let Ok(entries) = fs::read_dir("/proc") else {
            break;
        };
        for entry in entries.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue;
            };
            if owned.contains(&pid) {
                continue;
            }
            let stat = fs::read_to_string(entry.path().join("stat")).unwrap_or_default();
            if let Some(ppid) = proc_stat_ppid(&stat) {
                if owned.contains(&ppid) {
                    owned.insert(pid);
                    changed = true;
                }
            }
        }
    }
    owned
}

fn proc_stat_ppid(stat: &str) -> Option<u32> {
    let rest = stat.rsplit_once(") ")?.1;
    let mut fields = rest.split_whitespace();
    fields.next()?;
    fields.next()?.parse().ok()
}

fn parse_listening_ports(output: &str, owned: &BTreeSet<u32>) -> Vec<ListeningPort> {
    let mut ports = BTreeMap::<u16, ListeningPort>::new();
    for line in output.lines() {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.len() < 4 {
            continue;
        }
        let Some((host, port)) = parse_local_port(fields[3]) else {
            continue;
        };
        let pids = extract_pids(line)
            .into_iter()
            .filter(|pid| owned.contains(pid))
            .collect::<Vec<_>>();
        if pids.is_empty() {
            continue;
        }
        ports
            .entry(port)
            .and_modify(|item| {
                for pid in &pids {
                    if !item.pids.contains(pid) {
                        item.pids.push(*pid);
                    }
                }
            })
            .or_insert(ListeningPort { host, port, pids });
    }
    ports.into_values().collect()
}

fn parse_local_port(value: &str) -> Option<(String, u16)> {
    let (host, port) = value.rsplit_once(':')?;
    let port = port.parse::<u16>().ok()?;
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    Some((host, port))
}

fn extract_pids(line: &str) -> Vec<u32> {
    let mut pids = Vec::new();
    let mut rest = line;
    while let Some(index) = rest.find("pid=") {
        rest = &rest[index + 4..];
        let digits = rest
            .chars()
            .take_while(|item| item.is_ascii_digit())
            .collect::<String>();
        if let Ok(pid) = digits.parse::<u32>() {
            pids.push(pid);
        }
    }
    pids
}

/// Maximum bytes of an unterminated OSC sequence retained while waiting for a
/// terminator to arrive in a later read.
const OSC_TAIL_CAP: usize = 8192;

/// Decode the valid UTF-8 prefix of `pending + bytes`, returning the decoded
/// text and carrying any incomplete trailing multibyte sequence in `pending`
/// for the next call. Genuinely invalid bytes are replaced (lossy) rather than
/// carried, so `pending` cannot grow without bound.
fn decode_utf8_stream(pending: &mut Vec<u8>, bytes: &[u8]) -> String {
    pending.extend_from_slice(bytes);
    match std::str::from_utf8(pending) {
        Ok(text) => {
            let text = text.to_string();
            pending.clear();
            text
        }
        Err(err) => {
            let valid_up_to = err.valid_up_to();
            // SAFETY: `valid_up_to` is a valid UTF-8 boundary by definition.
            let text =
                unsafe { std::str::from_utf8_unchecked(&pending[..valid_up_to]) }.to_string();
            match err.error_len() {
                // Incomplete trailing sequence: carry it into the next read.
                None => {
                    pending.drain(..valid_up_to);
                    text
                }
                // Genuine invalid bytes: emit the remainder lossily and reset.
                Some(_) => {
                    let mut text = text;
                    text.push_str(&String::from_utf8_lossy(&pending[valid_up_to..]));
                    pending.clear();
                    text
                }
            }
        }
    }
}

/// Scan `buf` for OSC notification sequences. Returns the extracted messages
/// and the byte offset from which `buf` should be retained: the start of a
/// trailing unterminated OSC sequence (so it can be completed by a later read),
/// or `buf.len()` when everything up to the end was consumed.
fn scan_osc_notifications(buf: &str) -> (Vec<String>, usize) {
    let mut messages = Vec::new();
    let mut idx = 0;
    loop {
        let Some(rel) = buf[idx..].find("\x1b]") else {
            return (messages, buf.len());
        };
        let start = idx + rel;
        let after = start + 2;
        let bell = buf[after..].find('\x07');
        let st = buf[after..].find("\x1b\\");
        let end = match (bell, st) {
            (Some(bell), Some(st)) => Some(bell.min(st)),
            (Some(bell), None) => Some(bell),
            (None, Some(st)) => Some(st),
            (None, None) => None,
        };
        let Some(end) = end else {
            // Unterminated OSC sequence; retain it for the next read.
            return (messages, start);
        };
        if let Some(message) = osc_notification_message(&buf[after..after + end]) {
            messages.push(message);
        }
        idx = if st == Some(end) {
            after + end + 2
        } else {
            after + end + 1
        };
    }
}

/// Bound the retained OSC tail so a stray, never-terminated `ESC ]` cannot grow
/// it without limit. Drops from the front on a char boundary.
fn cap_osc_tail(tail: &mut String) {
    if tail.len() <= OSC_TAIL_CAP {
        return;
    }
    let target = tail.len() - OSC_TAIL_CAP;
    let cut = (target..=tail.len())
        .find(|index| tail.is_char_boundary(*index))
        .unwrap_or(tail.len());
    tail.drain(..cut);
}

#[cfg(test)]
fn osc_notifications(text: &str) -> Vec<String> {
    scan_osc_notifications(text).0
}

fn osc_notification_message(payload: &str) -> Option<String> {
    for prefix in ["9;", "99;", "777;"] {
        if let Some(rest) = payload.strip_prefix(prefix) {
            return format_osc_notification(rest);
        }
    }
    None
}

fn format_osc_notification(payload: &str) -> Option<String> {
    let parts = payload
        .split(';')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    let parts = if matches!(
        parts.first().map(|part| part.to_ascii_lowercase()),
        Some(kind) if kind == "notify" || kind == "notification"
    ) {
        &parts[1..]
    } else {
        parts.as_slice()
    };
    match parts {
        [] => None,
        [message] => Some((*message).to_string()),
        [title, body, ..] => Some(format!("{title}: {body}")),
    }
}

fn trim_output(output: String, max_len: usize) -> String {
    if output.len() <= max_len {
        output
    } else {
        let start = output.len() - max_len;
        let start = output
            .char_indices()
            .map(|(index, _)| index)
            .find(|index| *index >= start)
            .unwrap_or(output.len());
        output[start..].to_string()
    }
}

fn scrollback_limit(limit_bytes: Option<usize>) -> usize {
    limit_bytes.unwrap_or(16_000).min(1_000_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handle_stream_returns_json_error_for_unknown_request() {
        let server = Arc::new(Server::load(&format!("vmux-decode-test-{}", unix_time())).unwrap());
        let (mut client, daemon) = UnixStream::pair().unwrap();
        let worker = {
            let server = Arc::clone(&server);
            thread::spawn(move || server.handle_stream(daemon).unwrap())
        };

        client
            .write_all(br#"{"action":"missing-protocol-action"}"#)
            .unwrap();
        client.write_all(b"\n").unwrap();
        client.flush().unwrap();
        let mut line = String::new();
        BufReader::new(client).read_line(&mut line).unwrap();
        worker.join().unwrap();
        fs::remove_file(server.state_path.clone()).ok();

        let response: Response = serde_json::from_str(&line).unwrap();
        assert!(!response.ok);
        assert!(response
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("decode request"));
    }

    #[test]
    fn normalize_cwd_rejects_missing_directory() {
        let missing = format!("/tmp/vmux-missing-{}", unix_time());
        let err = normalize_cwd(Some(missing)).unwrap_err().to_string();
        assert!(err.contains("workspace cwd is not a directory"));
    }

    #[test]
    fn custom_actions_load_from_workspace_ancestor() {
        let root = std::env::temp_dir().join(format!("vmux-actions-test-{}", unix_time()));
        let nested = root.join("repo").join("crates").join("app");
        fs::create_dir_all(&nested).unwrap();
        fs::write(
            root.join("repo").join("vmux.json"),
            r#"{"commands":[{"name":"test","command":"cargo test","title":"tests","direction":"down"}]}"#,
        )
        .unwrap();

        let (path, actions) = load_custom_actions(&nested).unwrap().unwrap();

        assert_eq!(path, root.join("repo").join("vmux.json"));
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].name, "test");
        assert_eq!(actions[0].command, "cargo test");
        assert_eq!(actions[0].title.as_deref(), Some("tests"));
        assert_eq!(actions[0].direction, Some(SplitDirection::Down));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn load_marks_saved_panes_restored_and_relaunches_them() {
        let session_name = format!("vmux-restore-test-{}-{}", std::process::id(), unix_time());
        let state_path = paths::state_path(&session_name).unwrap();
        let cwd = std::env::current_dir().unwrap();
        let mut session = Session::new(&session_name);
        session.workspaces[0].cwd = cwd.display().to_string();
        session.workspaces[0].panes = vec!["pane-1".to_string()];
        session.workspaces[0].active_pane = Some("pane-1".to_string());
        session.workspaces[0].layout = Some(crate::model::LayoutNode::Pane {
            pane: "pane-1".to_string(),
        });
        let mut pane = Pane::new(
            "pane-1".to_string(),
            "printf vmux-restored".to_string(),
            SplitDirection::Right,
        );
        pane.status = PaneStatus::Running;
        pane.pid = Some(999_999);
        pane.output = "old output".to_string();
        pane.scrollback = "old scrollback".to_string();
        session.panes.insert("pane-1".to_string(), pane);
        fs::write(&state_path, serde_json::to_vec_pretty(&session).unwrap()).unwrap();

        let server = Server::load(&session_name).unwrap();
        {
            let loaded = server.session.lock_or_recover();
            let pane = loaded.panes.get("pane-1").unwrap();
            assert!(matches!(pane.status, PaneStatus::Restored));
            assert_eq!(pane.pid, None);
            assert_eq!(pane.output, "old output");
            assert_eq!(pane.scrollback, "old scrollback");
        }

        let server = Arc::new(server);
        let restored = server.restore_saved_panes().unwrap();
        assert_eq!(restored, vec!["pane-1".to_string()]);
        assert!(server.panes.lock_or_recover().contains_key("pane-1"));
        let snapshot = server.snapshot(false).unwrap();
        let pane = snapshot.panes.get("pane-1").unwrap();
        assert!(matches!(
            pane.status,
            PaneStatus::Running | PaneStatus::Exited
        ));
        assert_ne!(pane.pid, Some(999_999));
        assert_eq!(pane.output, "old output");
        assert_eq!(pane.scrollback, "old scrollback");

        if let Some(mut runtime) = server.panes.lock_or_recover().remove("pane-1") {
            if let Some(mut killer) = runtime.killer.take() {
                killer.kill().ok();
            }
        }
        server.cleanup_runtime_files();
        fs::remove_file(state_path).ok();
    }

    #[test]
    fn activating_base_tab_migrates_legacy_runtime_without_orphaning() {
        // Regression for finding 2: a pane created before it had tabs keeps its
        // runtime under the bare `pane-N` key. Adding a tab and switching back to
        // the base tab must reuse that runtime (re-keyed to `pane-N::tab-1`)
        // rather than orphaning it and spawning a duplicate shell.
        let session_name = format!("vmux-tabkey-test-{}-{}", std::process::id(), unix_time());
        let state_path = paths::state_path(&session_name).unwrap();
        let cwd = std::env::current_dir().unwrap();
        let mut session = Session::new(&session_name);
        session.workspaces[0].cwd = cwd.display().to_string();
        session.workspaces[0].panes = vec!["pane-1".to_string()];
        session.workspaces[0].active_pane = Some("pane-1".to_string());
        session.workspaces[0].layout = Some(crate::model::LayoutNode::Pane {
            pane: "pane-1".to_string(),
        });
        let mut pane = Pane::new(
            "pane-1".to_string(),
            "sleep 30".to_string(),
            SplitDirection::Right,
        );
        pane.status = PaneStatus::Running;
        session.panes.insert("pane-1".to_string(), pane);
        fs::write(&state_path, serde_json::to_vec_pretty(&session).unwrap()).unwrap();

        let server = Arc::new(Server::load(&session_name).unwrap());
        server.restore_saved_panes().unwrap();
        // The restored runtime is keyed bare (no tabs yet).
        assert!(server.panes.lock_or_recover().contains_key("pane-1"));

        // Workspace tabs: open a second tab, then switch back to the first.
        server
            .new_tab(
                None,
                Some("second".to_string()),
                Some("sleep 30".to_string()),
            )
            .unwrap();
        server.switch_tab(None, "tab-1".to_string()).unwrap();

        let keys = server
            .panes
            .lock_or_recover()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        // Runtimes stay keyed by bare pane id (no pane-N::tab-M).
        assert!(
            keys.iter().any(|k| k.starts_with("pane-")),
            "expected pane runtimes: {keys:?}"
        );
        assert!(
            !keys.iter().any(|k| k.contains("::")),
            "legacy pane-tab runtime keys must not appear: {keys:?}"
        );
        // At least the original restored pane runtime should still exist.
        assert!(
            !keys.is_empty(),
            "expected pane runtimes after tab switch, got {keys:?}"
        );

        for key in keys {
            let pane_id = key.split("::").next().unwrap_or(&key);
            server.remove_pane_runtimes(pane_id);
        }
        server.cleanup_runtime_files();
        fs::remove_file(state_path).ok();
    }

    #[test]
    fn custom_actions_reject_empty_command() {
        let root = std::env::temp_dir().join(format!("vmux-actions-bad-{}", unix_time()));
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join(".vmux.json"),
            r#"{"commands":[{"name":"bad","command":""}]}"#,
        )
        .unwrap();

        let err = load_custom_actions(&root).unwrap_err();

        assert!(err.to_string().contains("empty command"));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn git_branch_handles_unborn_branch() {
        let dir = std::env::temp_dir().join(format!("vmux-git-test-{}", unix_time()));
        fs::create_dir_all(&dir).unwrap();
        Command::new("git")
            .arg("-C")
            .arg(&dir)
            .arg("init")
            .output()
            .unwrap();
        Command::new("git")
            .arg("-C")
            .arg(&dir)
            .arg("checkout")
            .arg("-b")
            .arg("feature/test")
            .output()
            .unwrap();
        assert_eq!(git_branch(&dir).as_deref(), Some("feature/test"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn validates_supported_url_schemes() {
        assert!(validate_url("https://example.com").is_ok());
        assert!(validate_url("http://example.com").is_ok());
        assert!(validate_url("file:///etc/passwd").is_err());
        // localhost/private hosts stay allowed (core local-dev use case).
        assert!(validate_url("http://localhost:3000/preview").is_ok());
        assert!(validate_url("http://127.0.0.1:8080").is_ok());
        assert!(validate_url("http://user@example.com:8443/x").is_ok());
        // Hardened parsing rejects malformed URLs.
        assert!(validate_url("").is_err());
        assert!(validate_url("http://").is_err());
        assert!(validate_url("https:// example.com").is_err());
        assert!(validate_url("http://exa\nmple.com").is_err());
        assert!(validate_url("http://:8080/path").is_err());
    }

    #[test]
    fn compact_url_title_uses_host() {
        assert_eq!(compact_url_title("https://example.com/path"), "example.com");
    }

    #[test]
    fn pane_tab_helpers_create_switch_and_reject_ambiguous_titles() {
        let mut pane = Pane::new(
            "pane-1".to_string(),
            "bash".to_string(),
            SplitDirection::Right,
        );
        ensure_base_pane_tab(&mut pane);

        assert_eq!(pane.tabs.len(), 1);
        assert_eq!(pane.active_tab.as_deref(), Some("tab-1"));
        pane.tabs.push(PaneTab {
            id: "tab-2".to_string(),
            title: "tests".to_string(),
            command: "cargo test".to_string(),
            surface_kind: SurfaceKind::Terminal,
            status: Some(PaneStatus::Running),
            agent_status: Some(AgentStatus::Busy),
            progress: Some(42),
            notification_color: Some("yellow".to_string()),
            notification_message: Some("running tests".to_string()),
            exit_code: None,
            output: "test output".to_string(),
            output_formatted: String::new(),
            scrollback: "test history".to_string(),
            scrollback_formatted: String::new(),
            created_at: 1,
            updated_at: 1,
        });
        assert_eq!(
            resolve_pane_tab(&pane, "tests").unwrap().command,
            "cargo test"
        );
        apply_pane_tab(&mut pane, "tab-2").unwrap();
        assert_eq!(pane.active_tab.as_deref(), Some("tab-2"));
        assert_eq!(pane.title, "tests");
        assert_eq!(pane.command, "cargo test");
        assert_eq!(pane.output, "test output");
        assert_eq!(pane.scrollback, "test history");
        assert_eq!(pane.progress, Some(42));
        pane.output = "new test output".to_string();
        sync_active_pane_tab(&mut pane);
        assert_eq!(
            resolve_pane_tab(&pane, "tab-2").unwrap().output,
            "new test output"
        );
        pane.tabs.push(PaneTab {
            id: "tab-3".to_string(),
            title: "tests".to_string(),
            command: "cargo clippy".to_string(),
            surface_kind: SurfaceKind::Terminal,
            status: None,
            agent_status: None,
            progress: None,
            notification_color: None,
            notification_message: None,
            exit_code: None,
            output: String::new(),
            output_formatted: String::new(),
            scrollback: String::new(),
            scrollback_formatted: String::new(),
            created_at: 1,
            updated_at: 1,
        });
        assert!(resolve_pane_tab(&pane, "tests").is_err());
    }

    #[test]
    fn runtime_keys_include_active_tab_when_present() {
        let mut pane = Pane::new(
            "pane-1".to_string(),
            "bash".to_string(),
            SplitDirection::Right,
        );
        assert_eq!(active_runtime_key_for_pane(&pane), "pane-1");
        pane.active_tab = Some("tab-2".to_string());
        assert_eq!(active_runtime_key_for_pane(&pane), "pane-1::tab-2");
        assert_eq!(pane_runtime_key("pane-1", Some("tab-2")), "pane-1::tab-2");
        assert_eq!(pane_runtime_key("pane-1", None), "pane-1");
        assert_eq!(runtime_key_tab("pane-1", "pane-1::tab-2"), Some("tab-2"));
        assert_eq!(runtime_key_tab("pane-1", "pane-1"), None);
        assert_eq!(runtime_key_tab("pane-1", "pane-10::tab-2"), None);
        assert!(runtime_key_is_active_for_pane(&pane, "pane-1::tab-2"));
        assert!(!runtime_key_is_active_for_pane(&pane, "pane-1::tab-3"));
    }

    #[test]
    fn pane_size_sanitizer_keeps_vt100_dimensions_valid() {
        assert_eq!(
            sanitize_pane_size(PaneSize { rows: 0, cols: 0 }),
            PaneSize { rows: 2, cols: 2 }
        );
        assert_eq!(
            sanitize_pane_size(PaneSize { rows: 1, cols: 1 }),
            PaneSize { rows: 2, cols: 2 }
        );
        assert_eq!(
            sanitize_pane_size(PaneSize { rows: 24, cols: 80 }),
            PaneSize { rows: 24, cols: 80 }
        );
    }

    #[test]
    fn pane_size_sync_uses_single_attach_owner() {
        let server = Server::load(&format!("vmux-size-owner-test-{}", unix_time())).unwrap();
        server
            .resize_ptys(BTreeMap::new(), Some("desktop".to_string()), true)
            .unwrap();
        assert_eq!(
            server.pane_size_owner.lock_or_recover().as_deref(),
            Some("desktop")
        );

        server
            .resize_ptys(BTreeMap::new(), Some("phone".to_string()), false)
            .unwrap();
        assert_eq!(
            server.pane_size_owner.lock_or_recover().as_deref(),
            Some("desktop")
        );

        server
            .resize_ptys(BTreeMap::new(), Some("phone".to_string()), true)
            .unwrap();
        assert_eq!(
            server.pane_size_owner.lock_or_recover().as_deref(),
            Some("phone")
        );
    }

    #[test]
    fn metadata_keys_are_trimmed_and_reject_empty_or_whitespace() {
        assert_eq!(normalize_metadata_key(" task ").unwrap(), "task");
        assert!(normalize_metadata_key("").is_err());
        assert!(normalize_metadata_key("needs review").is_err());
    }

    #[test]
    fn inactive_runtime_sync_updates_tab_without_replacing_active_pane() {
        let mut pane = Pane::new(
            "pane-1".to_string(),
            "bash".to_string(),
            SplitDirection::Right,
        );
        ensure_base_pane_tab(&mut pane);
        pane.tabs.push(PaneTab {
            id: "tab-2".to_string(),
            title: "worker".to_string(),
            command: "sleep 10".to_string(),
            surface_kind: SurfaceKind::Terminal,
            status: Some(PaneStatus::Running),
            agent_status: Some(AgentStatus::Idle),
            progress: None,
            notification_color: None,
            notification_message: None,
            exit_code: None,
            output: String::new(),
            output_formatted: String::new(),
            scrollback: String::new(),
            scrollback_formatted: String::new(),
            created_at: 1,
            updated_at: 1,
        });
        pane.active_tab = Some("tab-1".to_string());
        pane.title = "active".to_string();
        pane.output = "active output".to_string();

        let mut runtime_pane = pane.clone();
        apply_pane_tab(&mut runtime_pane, "tab-2").unwrap();
        runtime_pane.output = "inactive output".to_string();
        runtime_pane.scrollback = "inactive history".to_string();
        runtime_pane.agent_status = AgentStatus::Attention;
        runtime_pane.notification_message = Some("done".to_string());
        runtime_pane.updated_at = 42;

        sync_tab_from_runtime_pane(&mut pane, "tab-2", &runtime_pane);

        assert_eq!(pane.active_tab.as_deref(), Some("tab-1"));
        assert_eq!(pane.title, "active");
        assert_eq!(pane.output, "active output");
        let tab = resolve_pane_tab(&pane, "tab-2").unwrap();
        assert_eq!(tab.output, "inactive output");
        assert_eq!(tab.scrollback, "inactive history");
        assert_eq!(tab.agent_status, Some(AgentStatus::Attention));
        assert_eq!(tab.notification_message.as_deref(), Some("done"));
    }

    #[test]
    fn extracts_text_title_and_links_from_html() {
        let html = r#"
            <html><head><title>Docs &amp; API</title></head>
            <body><h1>Hello</h1><p>Read <a href="/guide">the guide</a>
            or <a href="https://example.com?q=1&x=2">external</a>.</p></body></html>
        "#;

        assert_eq!(html_title(html).as_deref(), Some("Docs & API"));
        assert_eq!(
            html_to_text(html),
            "Docs & API Hello Read the guide or external ."
        );
        let links = html_links(html, "https://docs.example.test/index.html");
        assert_eq!(links.len(), 2);
        assert_eq!(links[0]["href"], "https://docs.example.test/guide");
        assert_eq!(links[0]["text"], "the guide");
        assert_eq!(links[1]["href"], "https://example.com?q=1&x=2");
    }

    #[test]
    fn extracts_forms_and_builds_submission_targets() {
        let html = r#"
            <form method="POST" action="/search" aria-label="Search">
              <input type="text" name="q" value="vmux">
              <input type="hidden" name="token" value="abc">
              <textarea name="notes">hello &amp; bye</textarea>
              <select name="kind"><option value="all">All</option><option value="code" selected>Code</option></select>
              <input type="submit" value="Go">
            </form>
        "#;
        let forms = html_forms(html, "https://example.test/docs/index.html");
        assert_eq!(forms.len(), 1);
        assert_eq!(forms[0]["method"], "post");
        assert_eq!(forms[0]["action"], "https://example.test/search");
        assert_eq!(forms[0]["label"], "Search");
        assert_eq!(forms[0]["fields"].as_array().unwrap().len(), 4);

        let values = form_default_fields(&forms[0]);
        assert_eq!(values.get("q").map(String::as_str), Some("vmux"));
        assert_eq!(values.get("notes").map(String::as_str), Some("hello & bye"));
        assert_eq!(values.get("kind").map(String::as_str), Some("code"));
        assert_eq!(
            form_submission_target("https://example.test/search", "post", &values).unwrap(),
            "curl -L -X POST --data-urlencode 'kind=code' --data-urlencode 'notes=hello & bye' --data-urlencode 'q=vmux' --data-urlencode 'token=abc' https://example.test/search"
        );
    }

    #[test]
    fn get_form_submission_encodes_query_values() {
        let mut values = BTreeMap::new();
        values.insert("q".to_string(), "vmux terminal".to_string());
        values.insert("tag".to_string(), "rust/pty".to_string());
        assert_eq!(
            form_submission_target("https://example.test/search", "get", &values).unwrap(),
            "https://example.test/search?q=vmux+terminal&tag=rust%2Fpty"
        );
    }

    #[test]
    fn evaluates_static_browser_expressions() {
        let html = r#"
            <html><head><title>Docs</title></head>
            <body>
              <h1>Welcome</h1>
              <p>Terminal native browser controls.</p>
              <a href="/guide">Guide</a>
              <form action="/search"><input name="q" value="vmux"></form>
            </body></html>
        "#;
        let links = html_links(html, "https://example.test/docs/");
        let forms = html_forms(html, "https://example.test/docs/");

        assert_eq!(
            evaluate_static_expression("title", html, &links, &forms).unwrap(),
            serde_json::json!("Docs")
        );
        assert_eq!(
            evaluate_static_expression("links[1].href", html, &links, &forms).unwrap(),
            serde_json::json!("https://example.test/guide")
        );
        assert_eq!(
            evaluate_static_expression("text:h1", html, &links, &forms).unwrap(),
            serde_json::json!("Welcome")
        );
        assert!(evaluate_static_expression("window.location", html, &links, &forms).is_err());
    }

    #[test]
    fn extracts_console_script_metadata() {
        let html = r#"
            <script src="/app.js"></script>
            <script>
              console.log("ready");
              console.error("bad");
            </script>
            <noscript>JavaScript is disabled</noscript>
        "#;

        let scripts = html_scripts(html, "https://example.test/index.html");
        assert_eq!(scripts.len(), 2);
        assert_eq!(scripts[0]["src"], "https://example.test/app.js");
        assert_eq!(scripts[1]["inline"], true);

        let calls = html_console_calls(html);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["method"], "log");
        assert_eq!(calls[1]["method"], "error");
        assert_eq!(
            html_noscript_blocks(html),
            vec!["JavaScript is disabled".to_string()]
        );
    }

    #[test]
    fn parses_curl_network_output() {
        let output = "HTTP/2 200\r\ncontent-type: text/html; charset=UTF-8\r\ncache-control: max-age=60\r\n\nVMUX_CURL_META\t200\thttps://example.test/\ttext/html; charset=UTF-8\t1234\t0.052\t1\n";
        let (headers, meta) = parse_curl_network_output(output);

        assert_eq!(headers.len(), 2);
        assert_eq!(headers[0]["name"], "content-type");
        assert_eq!(headers[0]["value"], "text/html; charset=UTF-8");
        assert_eq!(meta.status, Some(200));
        assert_eq!(meta.effective_url.as_deref(), Some("https://example.test/"));
        assert_eq!(
            meta.content_type.as_deref(),
            Some("text/html; charset=UTF-8")
        );
        assert_eq!(meta.bytes, Some(1234));
        assert_eq!(meta.redirects, Some(1));
    }

    #[test]
    fn parses_pull_request_info_from_gh_json() {
        let pr = parse_pull_request_info(
            r#"{"number":42,"state":"OPEN","title":"Add vmux","url":"https://example.test/pull/42","isDraft":true}"#,
        )
        .unwrap();

        assert_eq!(pr.number, 42);
        assert_eq!(pr.state, "OPEN");
        assert_eq!(pr.title.as_deref(), Some("Add vmux"));
        assert_eq!(pr.url.as_deref(), Some("https://example.test/pull/42"));
        assert!(pr.draft);
    }

    #[test]
    fn ignores_invalid_pull_request_json() {
        assert!(parse_pull_request_info(r#"{"state":"OPEN"}"#).is_none());
    }

    #[test]
    fn parses_listening_ports_for_owned_processes() {
        let mut owned = BTreeSet::new();
        owned.insert(1234);
        let output = r#"
LISTEN 0 511 127.0.0.1:5173 0.0.0.0:* users:(("node",pid=1234,fd=21))
LISTEN 0 511 127.0.0.1:8080 0.0.0.0:* users:(("node",pid=9999,fd=21))
LISTEN 0 128 [::1]:3000 [::]:* users:(("python",pid=1234,fd=4))
"#;
        let ports = parse_listening_ports(output, &owned);

        assert_eq!(ports.len(), 2);
        assert_eq!(ports[0].host, "::1");
        assert_eq!(ports[0].port, 3000);
        assert_eq!(ports[0].pids, vec![1234]);
        assert_eq!(ports[1].host, "127.0.0.1");
        assert_eq!(ports[1].port, 5173);
    }

    #[test]
    fn parses_proc_stat_ppid_when_command_has_spaces() {
        assert_eq!(proc_stat_ppid("123 (my shell) S 42 1 1 0").unwrap(), 42);
    }

    #[test]
    fn parses_osc_notification_sequences() {
        let text = "before\x1b]9;needs input\x07middle\x1b]777;notify;Claude;waiting\x1b\\after";
        assert_eq!(
            osc_notifications(text),
            vec!["needs input".to_string(), "Claude: waiting".to_string()]
        );
    }

    #[test]
    fn parse_agent_status_supports_attention_aliases() {
        assert_eq!(parse_agent_status("attention"), AgentStatus::Attention);
        assert_eq!(parse_agent_status("needs-input"), AgentStatus::Attention);
        assert_eq!(parse_agent_status("approval"), AgentStatus::Attention);
    }

    #[test]
    fn trim_output_does_not_split_utf8() {
        assert_eq!(trim_output("ab😀cd".to_string(), 4), "cd");
        assert_eq!(trim_output("ab😀cd".to_string(), 6), "😀cd");
    }

    #[test]
    fn scrollback_limit_defaults_and_caps() {
        assert_eq!(scrollback_limit(None), 16_000);
        assert_eq!(scrollback_limit(Some(4096)), 4096);
        assert_eq!(scrollback_limit(Some(2_000_000)), 1_000_000);
    }

    #[test]
    fn ignores_non_notification_osc_sequences() {
        assert!(osc_notifications("\x1b]0;terminal title\x07").is_empty());
    }

    #[test]
    fn terminate_pid_reports_missing_process() {
        let err = terminate_pid(999_999_999).unwrap_err().to_string();
        assert!(err.contains("failed to signal pid"));
    }

    #[test]
    fn decode_utf8_stream_carries_split_multibyte_chars() {
        // "é" is 0xC3 0xA9; split it across two reads.
        let mut pending = Vec::new();
        let first = decode_utf8_stream(&mut pending, b"ab\xc3");
        assert_eq!(first, "ab");
        assert_eq!(pending, vec![0xc3]);
        let second = decode_utf8_stream(&mut pending, b"\xa9cd");
        assert_eq!(second, "\u{e9}cd");
        assert!(pending.is_empty());
    }

    #[test]
    fn decode_utf8_stream_handles_emoji_across_three_chunks() {
        // "😀" is 0xF0 0x9F 0x98 0x80.
        let mut pending = Vec::new();
        assert_eq!(decode_utf8_stream(&mut pending, b"\xf0\x9f"), "");
        assert_eq!(decode_utf8_stream(&mut pending, b"\x98"), "");
        assert_eq!(decode_utf8_stream(&mut pending, b"\x80!"), "\u{1f600}!");
        assert!(pending.is_empty());
    }

    #[test]
    fn decode_utf8_stream_replaces_genuinely_invalid_bytes() {
        let mut pending = Vec::new();
        let text = decode_utf8_stream(&mut pending, b"a\xffb");
        assert!(text.starts_with('a'));
        assert!(text.ends_with('b'));
        assert!(pending.is_empty());
    }

    #[test]
    fn scan_osc_notifications_detects_sequences_split_across_reads() {
        let mut tail = String::new();
        tail.push_str("before\x1b]9;needs ");
        let (messages, retain_from) = scan_osc_notifications(&tail);
        assert!(messages.is_empty());
        // Retain from the unterminated OSC start.
        assert_eq!(&tail[retain_from..], "\x1b]9;needs ");
        tail.drain(..retain_from);
        tail.push_str("input\x07done");
        let (messages, retain_from) = scan_osc_notifications(&tail);
        assert_eq!(messages, vec!["needs input".to_string()]);
        assert_eq!(retain_from, tail.len());
    }

    #[test]
    fn push_output_bounds_scrollback_by_bytes() {
        let mut output: VecDeque<String> = VecDeque::new();
        let mut output_bytes = 0;
        for _ in 0..1000 {
            push_bounded_output(&mut output, &mut output_bytes, "x".repeat(1000));
        }
        assert!(output_bytes <= SCROLLBACK_CAP);
        let joined: String = output.iter().cloned().collect();
        assert_eq!(joined.len(), output_bytes);
        assert!(joined.len() <= SCROLLBACK_CAP);
        // The most recent chunk is always retained.
        assert!(output.back().is_some());
    }

    #[test]
    fn screen_scrollback_formatted_captures_styled_history_in_order() {
        // Small screen so most lines scroll off into history.
        let mut parser = vt100::Parser::new(2, 40, 100);
        parser.process(b"\x1b[31mred-oldest\x1b[m\r\nmid-one\r\nmid-two\r\nnewest\r\n");

        let formatted = screen_scrollback_formatted(&mut parser, 500);

        // Inline SGR escapes are preserved (the whole point of the fix).
        assert!(
            formatted.contains('\u{1b}'),
            "expected SGR escapes: {formatted:?}"
        );
        assert!(formatted.contains("red-oldest"));
        // Oldest → newest ordering, matching the plain scrollback stream.
        let red = formatted.find("red-oldest").unwrap();
        let newest = formatted.find("newest").unwrap();
        assert!(
            red < newest,
            "oldest row must precede newest: {formatted:?}"
        );
        // The scrollback offset is restored to the live view.
        assert_eq!(parser.screen().scrollback(), 0);
    }

    #[test]
    fn screen_scrollback_formatted_caps_history_rows() {
        let mut parser = vt100::Parser::new(2, 40, 100);
        for i in 0..40 {
            parser.process(format!("line-{i}\r\n").as_bytes());
        }

        let formatted = screen_scrollback_formatted(&mut parser, 3);

        // At most `row_cap` history rows plus the two visible screen rows.
        assert!(
            formatted.lines().count() <= 3 + 2,
            "row cap not honored: {} lines",
            formatted.lines().count()
        );
    }

    #[test]
    fn run_with_timeout_kills_slow_command() {
        let mut command = Command::new("sleep");
        command.arg("5");
        let start = Instant::now();
        let result = run_with_timeout(command, Duration::from_millis(200));
        let elapsed = start.elapsed();

        assert!(result.is_none(), "a command past its deadline is killed");
        assert!(
            elapsed < Duration::from_secs(2),
            "should return near the deadline, took {elapsed:?}"
        );
    }

    #[test]
    fn run_with_timeout_returns_fast_command_output() {
        let mut command = Command::new("printf");
        command.arg("hi");
        let output =
            run_with_timeout(command, Duration::from_secs(2)).expect("fast command succeeds");

        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout), "hi");
    }

    #[test]
    fn load_recovers_from_corrupt_state_file() {
        let session_name = format!("vmux-corrupt-test-{}-{}", std::process::id(), unix_time());
        let state_path = paths::state_path(&session_name).unwrap();
        fs::write(&state_path, b"{ this is not valid json").unwrap();

        let server = Server::load(&session_name).unwrap();
        {
            let session = server.session.lock_or_recover();
            assert_eq!(session.name, session_name);
            assert!(!session.workspaces.is_empty());
        }
        // Original corrupt file was moved aside, not left in place.
        assert!(!state_path.exists());

        // Clean up any generated backups.
        if let Some(dir) = state_path.parent() {
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    if name.to_string_lossy().contains(&session_name) {
                        fs::remove_file(entry.path()).ok();
                    }
                }
            }
        }
    }
}
