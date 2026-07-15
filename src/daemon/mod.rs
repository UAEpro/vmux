use anyhow::{anyhow, Context, Result};
use daemonize::Daemonize;
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::cli::{BroadcastScope, SplitDirection};
use crate::model::{
    acknowledge_done_status, condense_agent_title, decay_stale_busy, direction_axis,
    infer_agent_status, insert_pane_in_layout, is_coding_agent_command, merge_agent_status,
    next_pane_in_layout, resize_layout, title_from_status_message, touch_agent_status, unix_time,
    AgentStatus, ClipboardItem, DaemonInfo, EventRecord, ListeningPort, Notification, Pane,
    PaneStatus, PaneTab, Session, SurfaceKind,
};
use crate::paths;
use crate::protocol::{self, PaneSize, Request, Response};
use crate::sync::MutexExt;

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

/// Entry point for the (non-foreground) `vmux daemon` subcommand.
///
/// `VMUX_DAEMONIZE=1` marks the helper process that `start_detached` spawns:
/// it means "become the daemon yourself" rather than "spawn a helper". Only
/// this subcommand may honor it. It used to be honored inside
/// `start_detached` itself, which every `ensure_running` call reaches — and
/// since panes inherited the daemon's environment (including this variable),
/// any command that started a new session from inside a vmux pane silently
/// daemonized *itself*: the CLI forked into a detached daemon nobody asked
/// for and the parent exited 0 having run nothing. `vmux smoke` inside a
/// pane produced a zombie daemon per run this way.
pub fn start_detached_or_daemonize(session: &str) -> Result<()> {
    if std::env::var_os("VMUX_DAEMONIZE").is_some() {
        return daemonize_current_process(session);
    }
    start_detached(session)
}

pub fn start_detached(session: &str) -> Result<()> {
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
    // We are the daemon now; the marker has done its job. Scrub it so pane
    // children don't inherit it — a shell inside a pane carrying
    // VMUX_DAEMONIZE=1 would make any `vmux` command that starts a new
    // session daemonize itself instead of spawning a proper helper.
    std::env::remove_var("VMUX_DAEMONIZE");
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
    if is_running(session) {
        anyhow::bail!(
            "session {session:?} is already running (socket reachable). \
             Use `vmux --session {session} stop` first, or attach instead of daemon --foreground"
        );
    }
    let server = Arc::new(Server::load(session)?);
    server.reap_orphan_panes()?;
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

/// How often the idle-decay sweep runs.
const DECAY_TICK_SECS: u64 = 4;
/// An unpinned (heuristic) Busy spinner with no output for this long is demoted
/// to Idle. Pinned (hook/CLI) Busy is exempt, so this does not affect a properly
/// hook-driven agent that is thinking silently.
const BUSY_IDLE_DECAY_SECS: u64 = 20;

struct PaneRuntime {
    generation: u64,
    pane: Pane,
    // PTY OS handles. Kept in `Option` so they can be dropped the moment the
    // child exits: a short-lived pane would otherwise pin its
    // master/writer/killer file descriptors in the map until an explicit
    // kill/prune, leaking FDs. The captured `parser`/`output` below survive so
    // the UI can still read an exited pane's final screen and scrollback.
    master: Option<Box<dyn MasterPty + Send>>,
    // Wrapped in its own lock so blocking PTY writes never hold the global
    // `panes` mutex (a stalled child draining stdin would otherwise wedge
    // append_output/snapshot for every pane).
    writer: Option<Arc<Mutex<Box<dyn Write + Send>>>>,
    killer: Option<Box<dyn ChildKiller + Send + Sync>>,
    /// Byte budget for `output`, from `ui.scrollback_bytes`.
    scrollback_cap: usize,
    // Recent decoded output chunks. Bounded by `output_bytes` to `scrollback_cap`
    // so materializing the joined scrollback stays O(cap) rather than O(total).
    output: VecDeque<String>,
    output_bytes: usize,
    // Bytes of an incomplete trailing UTF-8 sequence carried into the next read.
    pending: Vec<u8>,
    // Accumulated decoded text used to detect OSC sequences that straddle reads.
    osc_tail: String,
    // vt100 does not expose DEC private mode 1007 (alternate scroll), so keep
    // the one unsupported input mode the attach UI needs in a tiny sidecar
    // scanner. This lets fullscreen TUIs such as Codex receive wheel-as-arrow
    // input just as they do in xterm-compatible terminal emulators.
    terminal_modes: TerminalModeTracker,
    parser: vt100::Parser,
    /// Effective PTY size actually applied to the master and vt100 parser:
    /// `min(layout_size, view_override)` per axis while an override is live,
    /// otherwise `layout_size`.
    size: PaneSize,
    /// The size the attach UI's layout last asked for. Kept separately from
    /// `size` so dropping a view override can restore the desktop layout
    /// without waiting for the next client resize.
    layout_size: PaneSize,
    /// Phone-fit override (`SetPaneViewSize`): a small remote viewer is
    /// watching, hold the PTY to at most this size until the lease runs out.
    /// One slot per pane — a second viewer replaces the first (last writer
    /// wins; with one slot a true min() across viewers isn't representable).
    view_override: Option<ViewOverride>,
    // Bumped by append_output whenever new bytes are processed. Used to skip the
    // scrollback-formatted walk in snapshot() when a pane produced no output
    // since the last snapshot.
    output_generation: u64,
    // Cached styled scrollback (see screen_scrollback_formatted) plus the
    // output_generation it was built from. `u64::MAX` forces the first build.
    scrollback_formatted_cache: String,
    scrollback_formatted_generation: u64,
    // Auto tab titles (agent panes only). `auto_title` is the last title this
    // pane pushed onto its tab, so an agent rewriting the same title does not
    // re-rename. `llm_title_state` tracks the one-shot LLM fallback.
    auto_title: Option<String>,
    llm_title_state: LlmTitleState,
    // When this pane's process started, used to time the LLM fallback out.
    started_at: u64,
    // Whether a coding agent is running *inside* this pane, and when that was
    // last determined. A pane's command is normally the user's shell and the
    // agent is a child of it, so this is answered from the process tree — and
    // cached, because that walk must not run on every chunk of output.
    agent_inside: bool,
    agent_inside_at: u64,
}

#[derive(Debug, Default)]
struct TerminalModeTracker {
    alternate_scroll: bool,
    state: TerminalModeScanState,
    csi: Vec<u8>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum TerminalModeScanState {
    #[default]
    Ground,
    Escape,
    Csi,
}

impl TerminalModeTracker {
    fn process(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            match self.state {
                TerminalModeScanState::Ground => {
                    if byte == b'\x1b' {
                        self.state = TerminalModeScanState::Escape;
                    }
                }
                TerminalModeScanState::Escape => match byte {
                    b'[' => {
                        self.csi.clear();
                        self.state = TerminalModeScanState::Csi;
                    }
                    b'\x1b' => {}
                    _ => self.state = TerminalModeScanState::Ground,
                },
                TerminalModeScanState::Csi => {
                    if (0x40..=0x7e).contains(&byte) {
                        self.apply_csi(byte);
                        self.csi.clear();
                        self.state = TerminalModeScanState::Ground;
                    } else if byte == b'\x1b' {
                        self.csi.clear();
                        self.state = TerminalModeScanState::Escape;
                    } else if self.csi.len() < 64 {
                        self.csi.push(byte);
                    } else {
                        self.csi.clear();
                        self.state = TerminalModeScanState::Ground;
                    }
                }
            }
        }
    }

    fn apply_csi(&mut self, final_byte: u8) {
        if !matches!(final_byte, b'h' | b'l') || !self.csi.starts_with(b"?") {
            return;
        }
        let has_alternate_scroll = self.csi[1..]
            .split(|byte| *byte == b';')
            .any(|param| param == b"1007");
        if has_alternate_scroll {
            self.alternate_scroll = final_byte == b'h';
        }
    }
}

fn alternate_scroll_active(
    status: &PaneStatus,
    modes: &TerminalModeTracker,
    screen: &vt100::Screen,
) -> bool {
    matches!(status, PaneStatus::Running) && modes.alternate_scroll && screen.alternate_screen()
}

/// Start with an empty screen/history while retaining the live child's input
/// contract. Fullscreen TUIs negotiate these modes once at startup and do not
/// resend them after `ClearPane`, because that RPC is invisible to the child.
fn cleared_parser_preserving_terminal_modes(
    parser: &vt100::Parser,
    rows: u16,
    cols: u16,
) -> vt100::Parser {
    let screen = parser.screen();
    let alternate_screen = screen.alternate_screen();
    let hide_cursor = screen.hide_cursor();
    let input_modes = screen.input_mode_formatted();

    let mut cleared = vt100::Parser::new(rows, cols, 2000);
    if alternate_screen {
        cleared.process(b"\x1b[?1049h");
    }
    cleared.process(&input_modes);
    if hide_cursor {
        cleared.process(b"\x1b[?25l");
    }
    cleared
}

/// How long a pane's "is an agent running in here" answer is trusted before the
/// process tree is walked again.
const AGENT_INSIDE_TTL_SECS: u64 = 5;

/// A leased view-size override (see `Request::SetPaneViewSize`).
#[derive(Debug, Clone, Copy)]
struct ViewOverride {
    size: PaneSize,
    /// The override silently expires at this instant unless re-leased, so a
    /// viewer that dies without unsubscribing cannot pin the pane small.
    expires_at: Instant,
}

/// "Smallest client wins", per axis: what the PTY should actually be when a
/// view override is active on top of the layout size.
fn effective_pane_size(layout: PaneSize, view: Option<PaneSize>) -> PaneSize {
    match view {
        Some(view) => sanitize_pane_size(PaneSize {
            rows: layout.rows.min(view.rows),
            cols: layout.cols.min(view.cols),
        }),
        None => layout,
    }
}

/// Bounds for `SetPaneViewSize.lease_ms`. The floor keeps a buggy client from
/// thrashing resize; the ceiling keeps a stuck client from pinning a pane for
/// more than a minute even if it leased generously.
const VIEW_LEASE_MS_MIN: u64 = 500;
const VIEW_LEASE_MS_MAX: u64 = 60_000;

/// One-shot lifecycle of the LLM tab-title fallback for a pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LlmTitleState {
    /// No title from the agent yet; still eligible for the fallback.
    Pending,
    /// A summarizer is running (or the pane got a title another way).
    Done,
}

impl PaneRuntime {
    /// Join the retained output chunks (already bounded to ~`scrollback_cap`).
    fn joined_output(&self) -> String {
        self.output.iter().cloned().collect()
    }

    /// Push a decoded chunk, tracking the running byte total and evicting the
    /// oldest chunks once the cap is exceeded.
    fn push_output(&mut self, text: String) {
        push_bounded_output(
            &mut self.output,
            &mut self.output_bytes,
            text,
            self.scrollback_cap,
        );
    }
}

/// Append `text` to a byte-bounded output deque, evicting the oldest chunks
/// while the running total exceeds `cap` (the most recent chunk is
/// always kept). Keeps materializing the joined scrollback O(cap).
fn push_bounded_output(
    output: &mut VecDeque<String>,
    output_bytes: &mut usize,
    text: String,
    cap: usize,
) {
    if text.is_empty() {
        return;
    }
    *output_bytes += text.len();
    output.push_back(text);
    while *output_bytes > cap && output.len() > 1 {
        if let Some(front) = output.pop_front() {
            *output_bytes -= front.len();
        }
    }
}

/// Line count (as `str::lines` reports it) of the retained output chunks,
/// without joining them into one allocation. ANSI escapes never contain
/// newlines, so this matches counting the stripped text.
fn chunked_line_count(output: &VecDeque<String>) -> usize {
    let newlines: usize = output
        .iter()
        .map(|chunk| chunk.bytes().filter(|b| *b == b'\n').count())
        .sum();
    match output.iter().rev().find(|chunk| !chunk.is_empty()) {
        None => 0,
        Some(last) => newlines + usize::from(!last.ends_with('\n')),
    }
}

/// Cached per-workspace metadata populated by a background thread so the
/// snapshot hot path never spawns git/ss subprocesses.
#[derive(Clone, Default, PartialEq, Eq)]
struct WorkspaceMeta {
    git_branch: Option<String>,
    ports: Vec<ListeningPort>,
    /// Live working directory of the workspace's active pane shell, so the
    /// sidebar path follows `cd` instead of pinning the spawn cwd. `None` when
    /// it can't be read (pane gone, non-Linux) — the persisted cwd then shows.
    cwd: Option<String>,
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
    // writes into a shared temp file or race the rename.
    save_lock: Mutex<u64>,
    /// Debounced persistence: PTY reader threads set this; a background writer
    /// flushes within ~500ms instead of blocking on every OSC notification.
    save_dirty: std::sync::atomic::AtomicBool,
    /// Monotonic generation bumped on any session/runtime change. Clients pass
    /// this as Snapshot.since to skip full reserializations when idle.
    generation: std::sync::atomic::AtomicU64,
    /// Set by `Shutdown` so the debounced save loop stops before the process
    /// exits and cannot re-create a state file a caller just cleaned up.
    shutting_down: std::sync::atomic::AtomicBool,
    // Only one attached UI should drive PTY dimensions at a time. Without this,
    // a small phone terminal and a large desktop terminal can resize the same
    // panes back and forth on every refresh.
    pane_size_owner: Mutex<Option<String>>,
    /// Wakes `wait` when a pane exits (instead of 50 ms polling only).
    exit_notify: (Mutex<()>, std::sync::Condvar),
    /// Automatic agent tab naming. Read once at start: changing it takes effect
    /// on the next daemon start (`vmux kill && vmux attach`).
    agent_titles: crate::config::AgentTitleSettings,
    /// Per-pane scrollback byte budget (`ui.scrollback_bytes`). Read once at
    /// start, like `agent_titles`.
    scrollback_cap: usize,
    /// Session lock file fd held for the daemon lifetime (single-instance).
    #[cfg(unix)]
    _session_lock: Option<std::fs::File>,
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
                        // View overrides are leases held by live viewers; no
                        // viewer survives a daemon restart. (save() also strips
                        // them — this is the belt to that brace.)
                        pane.view_size = None;
                        // DECSET 1007 belongs to the old child terminal. A
                        // restored pane starts a new PTY and must negotiate it
                        // again before the UI forwards wheel events.
                        pane.alternate_scroll_mode = false;
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

        #[cfg(unix)]
        let session_lock = paths::try_lock_session(session_name)?;

        // Read once at start; `normalized()` clamps scrollback_bytes.
        let config = crate::config::load().unwrap_or_default().normalized();

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
            save_dirty: std::sync::atomic::AtomicBool::new(false),
            generation: std::sync::atomic::AtomicU64::new(1),
            shutting_down: std::sync::atomic::AtomicBool::new(false),
            pane_size_owner: Mutex::new(None),
            exit_notify: (Mutex::new(()), std::sync::Condvar::new()),
            agent_titles: config.agent_titles,
            scrollback_cap: config.ui.scrollback_bytes,
            #[cfg(unix)]
            _session_lock: session_lock,
        })
    }

    fn touch(&self) {
        self.generation
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    fn generation(&self) -> u64 {
        self.generation.load(std::sync::atomic::Ordering::Relaxed)
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
        {
            // Debounced state writer.
            let server = Arc::clone(&self);
            thread::spawn(move || server.save_loop());
        }
        {
            // Background update-availability check (Stage A: notify only).
            let server = Arc::clone(&self);
            thread::spawn(move || server.update_check_loop());
        }
        {
            // Self-heal stale/false 🔄 spinners (unpinned Busy → Idle when quiet).
            let server = Arc::clone(&self);
            thread::spawn(move || server.agent_status_decay_loop());
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

    /// Recompute cached git/ss metadata for every workspace. Runs only on
    /// the background thread and never holds `panes`/`session` while spawning
    /// subprocesses. Everything here is local and cheap — vmux deliberately
    /// makes no network calls for sidebar metadata (querying GitHub for PR
    /// state from a background loop once drained the account's API quota).
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
                        workspace
                            .active_pane
                            .clone()
                            .or_else(|| workspace.first_pane()),
                        workspace
                            .all_pane_ids()
                            .into_iter()
                            .map(|id| id.to_string())
                            .collect::<Vec<_>>(),
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
        for (id, cwd, active_pane, pane_ids) in workspaces {
            let live_cwd = active_pane
                .and_then(|pane| pane_pids.get(&pane).copied())
                .and_then(pane_live_cwd);
            // Git metadata follows the live cwd so branch and path (both shown
            // on the sidebar second line) never describe different directories.
            let path = Path::new(live_cwd.as_deref().unwrap_or(&cwd));
            let roots = pane_ids
                .iter()
                .filter_map(|pane| pane_pids.get(pane).copied())
                .collect::<Vec<_>>();
            updated.insert(
                id,
                WorkspaceMeta {
                    git_branch: git_branch(path),
                    ports: listening_ports_for_roots(&roots),
                    cwd: live_cwd,
                },
            );
        }
        let mut cache = self.workspace_meta.lock_or_recover();
        if *cache != updated {
            *cache = updated;
            // Metadata changes must invalidate Snapshot.since.
            self.touch();
        }
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
            Request::Snapshot {
                since,
                full,
                lean,
                scrollback_panes,
            } => {
                let gen = self.generation();
                if since == Some(gen) {
                    return Ok(Response::ok(serde_json::json!({
                        "unchanged": true,
                        "generation": gen,
                    })));
                }
                let session = if lean {
                    let keep: BTreeSet<String> = scrollback_panes.into_iter().collect();
                    self.snapshot_opts(full, Some(&keep))?
                } else {
                    self.snapshot(full)?
                };
                Ok(Response::ok(serde_json::json!({
                    "generation": gen,
                    "session": session,
                })))
            }
            Request::List => {
                // List stays a flat Session for scripts; still bumps nothing.
                Ok(Response::ok(self.snapshot(false)?))
            }
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
                // Accept a workspace name OR id.
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
                // Accept a workspace name OR id.
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
                // Tab-aware: a pane may live on a background tab (or even another
                // workspace). Switch there first so "focus this pane" always means
                // "show me this pane" — same semantics as the phone relay path.
                {
                    let mut session = self.session.lock_or_recover();
                    let Some(location) = session.find_pane_location(&pane) else {
                        return Err(anyhow!("pane {pane} is not attached to any workspace"));
                    };
                    session.active_workspace = location.workspace_id.clone();
                    let workspace = session.active_workspace_mut();
                    if let Some(tab_id) = location.tab_id.as_deref() {
                        if workspace.active_tab.as_deref() != Some(tab_id) {
                            workspace.switch_tab(tab_id).map_err(anyhow::Error::msg)?;
                        }
                    }
                    if !workspace.panes.iter().any(|item| item == &pane) {
                        return Err(anyhow!("pane {pane} is not in active workspace"));
                    }
                    workspace.active_pane = Some(pane.clone());
                    workspace.flush_active_tab();
                }
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
                title,
            } => {
                let note = self.notify(pane, workspace, status, color, clear, message, title)?;
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
                ansi,
                history_lines,
            } => {
                let data = self.read_screen(pane, scrollback, limit_bytes, ansi, history_lines)?;
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
            Request::SetPaneViewSize {
                pane,
                cols,
                rows,
                lease_ms,
            } => {
                self.set_pane_view_size(Some(pane), PaneSize { rows, cols }, lease_ms)?;
                Ok(Response::empty())
            }
            Request::ClearPaneViewSize { pane } => {
                self.clear_pane_view_size(Some(pane))?;
                Ok(Response::empty())
            }
            Request::Shutdown => {
                self.shutting_down
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                self.save()?;
                self.log("daemon shutting down").ok();
                self.cleanup_runtime_files();
                self.release_session_lock();
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

    /// Drop saved panes that no workspace tab references. They can appear
    /// when a save races a tab close, and once loaded they are pure zombies:
    /// nothing on screen can ever show them, but the status bar counts them
    /// ("panes:8" with four visible) and their stale agent statuses pollute
    /// the busy/done/error tallies.
    fn reap_orphan_panes(self: &Arc<Self>) -> Result<()> {
        let removed = {
            let mut session = self.session.lock_or_recover();
            let referenced: std::collections::HashSet<String> = session
                .workspaces
                .iter()
                .flat_map(|w| w.all_pane_ids().into_iter().map(|p| p.to_string()))
                .collect();
            let orphans: Vec<String> = session
                .panes
                .keys()
                .filter(|id| !referenced.contains(*id))
                .cloned()
                .collect();
            for id in &orphans {
                session.panes.remove(id);
            }
            orphans
        };
        if !removed.is_empty() {
            self.log(&format!(
                "reaped {} orphan pane(s) not referenced by any tab: {}",
                removed.len(),
                removed.join(", ")
            ))
            .ok();
            // save() touches the generation, so attached UIs repaint.
            self.save()?;
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
                    // Restore panes on every tab, not only the active-tab live view.
                    workspace
                        .all_pane_ids()
                        .into_iter()
                        .filter_map(|pane_id| {
                            let pane = session.panes.get(pane_id)?;
                            if matches!(pane.status, PaneStatus::Restored) {
                                Some((
                                    pane_id.to_string(),
                                    workspace.id.clone(),
                                    PathBuf::from(&workspace.cwd),
                                ))
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<_>>()
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
            // Seed runtime scrollback so the first PTY append does not wipe history.
            let seed = if !old_scrollback.is_empty() {
                old_scrollback.clone()
            } else {
                old_output.clone()
            };
            if !seed.is_empty() {
                let runtime_key = active_runtime_key_for_pane(&pane);
                let mut runtimes = self.panes.lock_or_recover();
                let key = if runtimes.contains_key(&runtime_key) {
                    runtime_key
                } else {
                    pane_id.clone()
                };
                if let Some(runtime) = runtimes.get_mut(&key) {
                    runtime.push_output(seed.clone());
                    runtime.parser.process(seed.as_bytes());
                    runtime.pane.output = seed.clone();
                    runtime.pane.scrollback = seed;
                    runtime.pane.scrollback_formatted = old_scrollback_formatted.clone();
                }
            }
            pane.output = old_output;
            pane.scrollback = old_scrollback;
            pane.scrollback_formatted = old_scrollback_formatted;
            {
                let mut session = self.session.lock_or_recover();
                // start_pane_runtime already published a Running record; refresh
                // it with the restored history only if the child has not already
                // exited, otherwise mark_exited holds the final output.
                if let Some(existing) = session.panes.get(&pane_id) {
                    if existing.status == PaneStatus::Running {
                        session.panes.insert(pane_id.clone(), pane);
                    }
                }
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
            // start_pane_runtime already published the session record (before
            // spawning the reader thread); only link it into the workspace here
            // so we never clobber an exit that raced ahead of us.
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
                .find(|workspace| workspace.contains_pane(&pane_id))
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
        // Legacy lmux hook scripts still expand these names.
        builder.env("LMUX_SESSION", &self.session_name);
        builder.env("LMUX_WORKSPACE_ID", workspace);
        builder.env("LMUX_PANE_ID", &pane.id);
        builder.env("LMUX_SURFACE_ID", &pane.id);
        builder.env("LMUX_SOCKET_PATH", self.socket_path.display().to_string());
        let mut child = pair.slave.spawn_command(builder)?;
        let child_pid = child.process_id();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let killer = child.clone_killer();
        let master = pair.master;
        pane.status = PaneStatus::Running;
        // Do NOT pin Busy at launch: a freshly spawned agent is waiting for your
        // first prompt, not working, and a pinned 🔄 would stick as a false
        // spinner until a Stop hook or process exit. Start from inference
        // (coding agent → Idle, else Unknown), unpinned. Real work drives Busy
        // via hooks (pinned, authoritative) or output inference (which the idle
        // decay self-heals once the pane goes quiet).
        touch_agent_status(pane, infer_agent_status("", &pane.command), false);
        pane.exit_code = None;
        pane.pid = child_pid;
        pane.progress = None;
        pane.output.clear();
        pane.scrollback.clear();
        pane.scrollback_formatted.clear();
        pane.alternate_scroll_mode = false;
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
                scrollback_cap: self.scrollback_cap,
                output: VecDeque::with_capacity(2048),
                output_bytes: 0,
                pending: Vec::new(),
                osc_tail: String::new(),
                terminal_modes: TerminalModeTracker::default(),
                parser: vt100::Parser::new(24, 80, 2000),
                size: PaneSize { rows: 24, cols: 80 },
                layout_size: PaneSize { rows: 24, cols: 80 },
                view_override: None,
                output_generation: 0,
                scrollback_formatted_cache: String::new(),
                scrollback_formatted_generation: u64::MAX,
                auto_title: None,
                llm_title_state: LlmTitleState::Pending,
                agent_inside: false,
                agent_inside_at: 0,
                started_at: unix_time(),
            },
        );

        // Publish the session record BEFORE spawning the reader thread. A
        // fast-exiting child's mark_exited runs on that thread and looks the
        // pane up in session.panes; if the record is not there yet the exit is
        // dropped and the pane is stuck Running forever (vmux wait hangs).
        // Preserve tab/metadata ownership when a prior record exists (restart).
        {
            let mut session = self.session.lock_or_recover();
            if let Some(existing) = session.panes.get_mut(&pane_id) {
                let tabs = existing.tabs.clone();
                let active_tab = existing.active_tab.clone();
                let metadata = existing.metadata.clone();
                *existing = pane.clone();
                existing.tabs = tabs;
                existing.active_tab = active_tab;
                existing.metadata = metadata;
            } else {
                session.panes.insert(pane_id.clone(), pane.clone());
            }
        }

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
        // Accept a workspace name OR id, matching switch/rename.
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
            // Accept a workspace name OR id.
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

        // Close every pane in every tab of the workspace — not only the active
        // tab's live `panes` view.
        for pane in closed.all_pane_ids() {
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
            // Accept a workspace name OR id.
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
        // Accept a workspace name OR id.
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
                .find(|workspace| workspace.contains_pane(pane_id))
                .ok_or_else(|| anyhow!("pane {pane_id} is not attached to a workspace"))?;
            (
                pane,
                workspace.id.clone(),
                normalize_cwd(Some(workspace.cwd.clone()))?,
            )
        };

        self.start_pane_runtime(&mut pane, cwd, &workspace_id)?;
        // start_pane_runtime republished the session record before spawning the
        // reader thread; re-inserting here would clobber an instant exit.
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
            // The client reports its layout box; a live view override
            // (phone-fit) clamps what actually reaches the PTY.
            runtime.layout_size = sanitize_pane_size(size);
            let next =
                effective_pane_size(runtime.layout_size, runtime.view_override.map(|v| v.size));
            apply_pty_size(runtime, next)?;
        }
        Ok(())
    }

    /// Set or refresh a pane's phone-fit view override (see the request docs).
    fn set_pane_view_size(
        &self,
        pane: Option<String>,
        view: PaneSize,
        lease_ms: u64,
    ) -> Result<()> {
        let pane_id = self.resolve_pane(pane)?;
        let runtime_key = self.active_runtime_key(&pane_id)?;
        let view = sanitize_pane_size(view);

        // LOCK ORDER: session first, released before panes. Mirror the marker
        // into the session pane (snapshots read it from there) and refuse
        // zoomed panes — the user zoomed for a full-area view; a phone glance
        // must not shrink it under them.
        {
            let mut session = self.session.lock_or_recover();
            let zoomed = session.workspaces.iter().any(|ws| {
                ws.zoomed_pane.as_deref() == Some(pane_id.as_str())
                    || ws
                        .tabs
                        .iter()
                        .any(|tab| tab.zoomed_pane.as_deref() == Some(pane_id.as_str()))
            });
            if zoomed {
                return Err(anyhow!(
                    "pane {pane_id} is zoomed; not applying a view size"
                ));
            }
            let Some(pane) = session.panes.get_mut(&pane_id) else {
                return Err(anyhow!("unknown pane {pane_id}"));
            };
            pane.view_size = Some(crate::model::PaneViewSize {
                cols: view.cols,
                rows: view.rows,
            });
        }

        let lease = Duration::from_millis(lease_ms.clamp(VIEW_LEASE_MS_MIN, VIEW_LEASE_MS_MAX));
        {
            let mut panes = self.panes.lock_or_recover();
            let key = if panes.contains_key(&runtime_key) {
                runtime_key
            } else {
                legacy_runtime_key(&pane_id)
            };
            if let Some(runtime) = panes.get_mut(&key) {
                runtime.view_override = Some(ViewOverride {
                    size: view,
                    expires_at: Instant::now() + lease,
                });
                runtime.pane.view_size = Some(crate::model::PaneViewSize {
                    cols: view.cols,
                    rows: view.rows,
                });
                let next = effective_pane_size(runtime.layout_size, Some(view));
                apply_pty_size(runtime, next)?;
            }
        }
        self.touch();
        Ok(())
    }

    /// Drop a pane's view override and return it to its layout size now.
    fn clear_pane_view_size(&self, pane: Option<String>) -> Result<()> {
        let pane_id = self.resolve_pane(pane)?;
        let runtime_key = self.active_runtime_key(&pane_id)?;
        {
            let mut session = self.session.lock_or_recover();
            if let Some(pane) = session.panes.get_mut(&pane_id) {
                pane.view_size = None;
            }
        }
        {
            let mut panes = self.panes.lock_or_recover();
            let key = if panes.contains_key(&runtime_key) {
                runtime_key
            } else {
                legacy_runtime_key(&pane_id)
            };
            if let Some(runtime) = panes.get_mut(&key) {
                runtime.view_override = None;
                runtime.pane.view_size = None;
                apply_pty_size(runtime, runtime.layout_size)?;
            }
        }
        self.touch();
        Ok(())
    }

    /// Drop view overrides whose lease ran out, restoring layout sizes.
    /// Returns true when anything changed (callers bump the generation).
    /// Runs from the housekeeping tick so a vanished phone restores the pane
    /// within seconds even though it never said goodbye.
    fn expire_view_overrides(&self, now: Instant) -> bool {
        let mut restored: Vec<String> = Vec::new();
        {
            let mut panes = self.panes.lock_or_recover();
            for runtime in panes.values_mut() {
                let expired = runtime
                    .view_override
                    .map(|v| v.expires_at <= now)
                    .unwrap_or(false);
                if expired {
                    runtime.view_override = None;
                    runtime.pane.view_size = None;
                    apply_pty_size(runtime, runtime.layout_size).ok();
                    restored.push(runtime.pane.id.clone());
                }
            }
        }
        if restored.is_empty() {
            return false;
        }
        let mut session = self.session.lock_or_recover();
        for pane_id in &restored {
            if let Some(pane) = session.panes.get_mut(pane_id) {
                pane.view_size = None;
            }
        }
        true
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
        // final scrollback before the process is killed.
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
        // Kill removes the pane from every tab and the live view (not just the
        // active-tab view), then drops the session record. Keeping an Exited
        // orphan that is in no layout would leak: all_pane_ids() can't reach it
        // so prune_exited_panes can never collect it (finding: kill_pane leak).
        for workspace in &mut session.workspaces {
            workspace.remove_pane_anywhere(&pane_id);
        }
        let Some(mut pane) = session.panes.remove(&pane_id) else {
            return Err(anyhow!("unknown pane {pane_id}"));
        };
        pane.status = PaneStatus::Exited;
        touch_agent_status(&mut pane, AgentStatus::Done, true);
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
                id: 0,
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
        // No explicit pane title: let it auto-title from its process, like any
        // other pane. Passing the tab's title froze a copy of the default
        // ("tab") on the pane forever, even after the tab auto-renamed.
        let pane = if let Some(command) = command.filter(|c| !c.trim().is_empty()) {
            Some(self.new_pane(
                SplitDirection::Right,
                command,
                None,
                Some(workspace_id.clone()),
                None,
            )?)
        } else {
            // Empty shell so the tab isn't blank.
            Some(self.new_pane(
                SplitDirection::Right,
                String::new(),
                None,
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
            // Sleep on the exit condvar (woken by mark_exited) with a short
            // timeout so we still re-check if a notification is missed.
            let (lock, cvar) = &self.exit_notify;
            let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
            let wait = timeout_ms
                .map(|t| {
                    let remaining = Duration::from_millis(t).saturating_sub(started.elapsed());
                    remaining.min(Duration::from_millis(200))
                })
                .unwrap_or(Duration::from_millis(200));
            let _ = cvar.wait_timeout(guard, wait);
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
            // Accept a workspace name OR id.
            let workspace = session
                .resolve_workspace_selector(&workspace)
                .map_err(anyhow::Error::msg)?;
            let targets = session
                .workspaces
                .iter()
                .find(|item| item.id == workspace)
                .map(|workspace| {
                    workspace
                        .all_pane_ids()
                        .into_iter()
                        .map(|id| id.to_string())
                        .collect::<Vec<_>>()
                })
                .ok_or_else(|| anyhow!("unknown workspace {workspace}"))?;
            if targets.is_empty() {
                return Err(anyhow!("workspace has no panes"));
            }
            return Ok(targets);
        }
        Ok(vec![self.resolve_pane(pane)?])
    }

    // Mirrors the Notify request fields; grouping would obscure the 1:1 map.
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        pane: Option<String>,
        workspace: Option<String>,
        status: Option<String>,
        color: Option<String>,
        clear: bool,
        mut message: String,
        title: Option<String>,
    ) -> Result<Notification> {
        // Bound the stored message like events are (see push_event); the raw
        // request body can be up to the 16 MB request cap otherwise.
        const MAX_NOTIFY_MSG: usize = 4_096;
        truncate_on_char_boundary(&mut message, MAX_NOTIFY_MSG);
        let target_workspace = self.resolve_workspace(workspace)?;
        let target_pane = if target_workspace.is_some() {
            pane
        } else {
            Some(self.resolve_pane(pane.clone())?)
        };
        // Agent-agnostic free title path (any coding agent, not just Grok):
        //   1. explicit `title` from hooks (UserPromptSubmit prompt condensed)
        //   2. a meaningful busy message (`set-status busy --message "fix auth"`)
        // OSC titles are handled on the PTY reader path; LLM is last-resort.
        let auto_title = if self.agent_titles.enabled && !clear {
            title.as_deref().and_then(condense_agent_title).or_else(|| {
                let busy = status
                    .as_deref()
                    .map(|s| matches!(parse_agent_status(s), AgentStatus::Busy))
                    .unwrap_or(false);
                if busy {
                    title_from_status_message(&message)
                } else {
                    None
                }
            })
        } else {
            None
        };
        // Reclassify completion-flavoured "attention" messages (e.g. Grok
        // Notification "Turn complete") as done so they don't raise 🙋.
        let (status, color) = if !clear
            && status
                .as_deref()
                .map(|s| matches!(parse_agent_status(s), AgentStatus::Attention))
                .unwrap_or(false)
            && is_completion_noise_notification(&message)
        {
            (Some("done".to_string()), Some("green".to_string()))
        } else {
            (status, color)
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
        // Claude Code's Notification hook fires ~60s after every finished turn
        // with "Claude is waiting for your input". That idle notice carries no
        // new request, so it must not overwrite the Stop hook's ✅ with 🙋.
        // Same for completion noise that we didn't reclassify above.
        let skip_status = is_non_actionable_attention(&message)
            && status
                .as_deref()
                .map(|status| matches!(parse_agent_status(status), AgentStatus::Attention))
                .unwrap_or(true);
        // UserPromptSubmit condenses the prompt into `title` / auto_title — that
        // is the signal a *new* turn started, vs late PreToolUse after Stop.
        let has_new_turn_title = auto_title.is_some()
            || title
                .as_deref()
                .map(|t| !t.trim().is_empty())
                .unwrap_or(false);
        let mut session = self.session.lock_or_recover();
        let mut runtime_target = None;
        if let Some(pane_id) = &target_pane {
            if let Some(target) = session.panes.get_mut(pane_id) {
                runtime_target = Some(active_runtime_key_for_pane(target));
                if clear {
                    target.notification_color = None;
                    target.notification_message = None;
                } else if skip_status {
                    // keep current status/banner
                } else if let Some(status) = &status {
                    if !should_skip_busy_after_settled(target, status, &message, has_new_turn_title)
                    {
                        apply_explicit_agent_status(target, status, color.as_deref(), &message);
                    }
                } else if !message.is_empty() {
                    // Bare notify without status = needs attention.
                    touch_agent_status(target, AgentStatus::Attention, true);
                    target.notification_color = color.clone().or_else(|| Some("blue".to_string()));
                    target.notification_message = Some(message.clone());
                }
                target.updated_at = unix_time();
            }
        }
        // Agent hooks fire on every tool call, so a working agent repeats the
        // identical "busy / agent working" notify many times per minute. A
        // notify that matches the latest recorded state for the same target is
        // not news — keep the live pane status fresh (above/below) but don't
        // append another feed entry, which the relay would forward as yet
        // another push notification. Routine busy/done boilerplate is also
        // skipped entirely: the sidebar already shows 🔄/✅.
        let duplicate = !clear
            && session
                .notifications
                .iter()
                .rev()
                .find(|prev| prev.pane == note.pane && prev.workspace == note.workspace)
                .is_some_and(|prev| {
                    !prev.clear
                        && prev.status == note.status
                        && prev.color == note.color
                        && prev.message == note.message
                });
        if !duplicate && should_record_in_notification_feed(&note) {
            session.notifications.push(note.clone());
            let len = session.notifications.len();
            if len > 100 {
                session.notifications.drain(0..len - 100);
            }
            push_event(
                &mut session,
                EventRecord {
                    id: 0,
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
        }
        drop(session);
        if let Some(runtime_key) = runtime_target {
            if let Some(runtime) = self.panes.lock_or_recover().get_mut(&runtime_key) {
                if clear {
                    runtime.pane.notification_color = None;
                    runtime.pane.notification_message = None;
                } else if skip_status {
                    // keep current status/banner
                } else if let Some(status) = &status {
                    if !should_skip_busy_after_settled(
                        &runtime.pane,
                        status,
                        &message,
                        has_new_turn_title,
                    ) {
                        apply_explicit_agent_status(
                            &mut runtime.pane,
                            status,
                            color.as_deref(),
                            &message,
                        );
                    }
                } else if !message.is_empty() {
                    touch_agent_status(&mut runtime.pane, AgentStatus::Attention, true);
                    runtime.pane.notification_color =
                        color.clone().or_else(|| Some("blue".to_string()));
                    runtime.pane.notification_message = Some(message.clone());
                }
                if let Some(title) = auto_title.as_ref() {
                    // Same bookkeeping as OSC titles: skip the LLM fallback and
                    // remember what we already put on the tab.
                    runtime.llm_title_state = LlmTitleState::Done;
                    runtime.auto_title = Some(title.clone());
                }
                runtime.pane.updated_at = unix_time();
            }
        }
        if let (Some(pane_id), Some(title)) = (target_pane.as_deref(), auto_title.as_deref()) {
            self.apply_auto_tab_title(pane_id, title);
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
                            snapshot.find_pane_location(pane_id).and_then(|loc| {
                                snapshot
                                    .workspaces
                                    .iter()
                                    .find(|workspace| workspace.id == loc.workspace_id)
                            })
                        })
                    });
                let pane = event
                    .pane
                    .as_ref()
                    .and_then(|pane_id| snapshot.panes.get(pane_id));
                serde_json::json!({
                    "id": event.id,
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
            // Prefer attention / bare "needs you" notes over routine busy/done
            // feed noise so Ctrl-b u lands on something that wants input.
            let visible: Vec<_> = session
                .notifications
                .iter()
                .rev()
                .filter(|note| !note.clear && !note.message.is_empty())
                .collect();
            visible
                .iter()
                .find(|note| {
                    matches!(
                        note.status.as_deref(),
                        Some("attention") | Some("error") | None
                    )
                })
                .or_else(|| visible.first())
                .copied()
                .cloned()
                .ok_or_else(|| anyhow!("no notifications"))?
        };

        let mut session = self.session.lock_or_recover();
        let (workspace_id, tab_id, pane_id) = if let Some(pane_id) = target.pane.clone() {
            let location = session
                .find_pane_location(&pane_id)
                .ok_or_else(|| anyhow!("notification pane {pane_id} is no longer attached"))?;
            (
                location.workspace_id,
                location.tab_id,
                Some(location.pane_id),
            )
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
            (workspace_id, None, None)
        };

        session.active_workspace = workspace_id.clone();
        {
            let workspace = session.active_workspace_mut();
            if let Some(tab_id) = tab_id.as_deref() {
                if workspace.active_tab.as_deref() != Some(tab_id) {
                    workspace.switch_tab(tab_id).map_err(anyhow::Error::msg)?;
                }
            }
            if let Some(pane_id) = pane_id.clone() {
                workspace.active_pane = Some(pane_id);
                workspace.flush_active_tab();
            }
        }
        drop(session);
        if let Some(ref pane_id) = pane_id {
            self.acknowledge_done_panes(std::slice::from_ref(pane_id));
        }
        self.save()?;

        Ok(serde_json::json!({
            "workspace": workspace_id,
            "tab": tab_id,
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
                id: 0,
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
        ansi: bool,
        history_lines: usize,
    ) -> Result<serde_json::Value> {
        let pane_id = self.resolve_pane(pane)?;
        let runtime_key = self
            .active_runtime_key(&pane_id)
            .unwrap_or_else(|_| pane_id.clone());
        let limit = scrollback_limit(limit_bytes);
        let output = {
            let mut panes = self.panes.lock_or_recover();
            let key = if panes.contains_key(&runtime_key) {
                Some(runtime_key.clone())
            } else if panes.contains_key(&pane_id) {
                Some(pane_id.clone())
            } else {
                None
            };
            key.and_then(|k| panes.get_mut(&k)).map(|runtime| {
                let (cursor_row, cursor_col) = runtime.parser.screen().cursor_position();
                let screen_text = if ansi {
                    screen_contents_ansi(runtime.parser.screen())
                } else {
                    runtime.parser.screen().contents()
                };
                let mut value = serde_json::json!({
                    "pane": pane_id,
                    "screen": screen_text,
                    "rows": runtime.size.rows,
                    "cols": runtime.size.cols,
                    "cursor_row": cursor_row,
                    "cursor_col": cursor_col,
                });
                if include_scrollback {
                    // joined_output() concatenates the whole ring (~16KB); only
                    // pay for it when the caller actually wants scrollback.
                    let raw = runtime.joined_output();
                    value["scrollback"] = serde_json::json!(trim_output(raw, limit));
                }
                if history_lines > 0 {
                    value["history"] = serde_json::json!(parser_history_rows(
                        &mut runtime.parser,
                        history_lines,
                        ansi
                    ));
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
        // LOCK ORDER: release `panes` before taking `session` (see the field
        // comments). Reading the persisted fallback inside the `panes` block
        // would nest session-under-panes, against the documented order.
        let from_runtime = {
            let panes = self.panes.lock_or_recover();
            panes
                .get(&runtime_key)
                .or_else(|| panes.get(&pane_id))
                .map(|runtime| (runtime.parser.screen().contents(), runtime.joined_output()))
        };
        let (screen, scrollback) = match from_runtime {
            Some(pair) => pair,
            None => {
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
        // LOCK ORDER: release `panes` before falling back to `session`.
        let from_runtime = {
            let panes = self.panes.lock_or_recover();
            panes
                .get(&runtime_key)
                .or_else(|| panes.get(&pane_id))
                .map(|runtime| {
                    if scrollback {
                        trim_output(runtime.joined_output(), limit)
                    } else {
                        runtime.parser.screen().contents()
                    }
                })
        };
        let text = match from_runtime {
            Some(text) => text,
            None => {
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
        let runtime_state = {
            let mut panes = self.panes.lock_or_recover();
            if let Some(runtime) = panes.get_mut(&runtime_key) {
                runtime.output.clear();
                runtime.output_bytes = 0;
                runtime.pending.clear();
                runtime.osc_tail.clear();
                runtime.parser = cleared_parser_preserving_terminal_modes(
                    &runtime.parser,
                    runtime.size.rows,
                    runtime.size.cols,
                );
                runtime.pane.output.clear();
                runtime.pane.output_formatted.clear();
                runtime.pane.scrollback.clear();
                runtime.pane.scrollback_formatted.clear();
                runtime.pane.alternate_scroll_mode = alternate_scroll_active(
                    &runtime.pane.status,
                    &runtime.terminal_modes,
                    runtime.parser.screen(),
                );
                // Force the styled-scrollback cache to rebuild from the fresh
                // parser on the next snapshot.
                runtime.scrollback_formatted_cache.clear();
                runtime.scrollback_formatted_generation = u64::MAX;
                Some((runtime.size, runtime.pane.alternate_scroll_mode))
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
        // active tab. sync_active_pane_tab then propagates the
        // cleared state into the active tab's captured record.
        pane.scrollback.clear();
        pane.scrollback_formatted.clear();
        pane.alternate_scroll_mode = runtime_state
            .map(|(_, alternate_scroll_mode)| alternate_scroll_mode)
            .unwrap_or(false);
        pane.updated_at = unix_time();
        sync_active_pane_tab(pane);
        let mut pane = pane.clone();
        if let Some((size, _)) = runtime_state {
            pane.output = format!("cleared capture at {}x{}", size.rows, size.cols);
        }
        drop(session);
        self.save()?;
        Ok(pane)
    }

    fn resolve_pane(&self, pane: Option<String>) -> Result<String> {
        let mut session = self.session.lock_or_recover();
        // Treat missing / blank / whitespace as "use active pane". Claude Stop
        // hooks that expand an empty LMUX_PANE_ID used to send `--pane ""` and
        // fail with `unknown pane `, leaving 🔄 stuck forever.
        let pane_id = match pane.map(|p| p.trim().to_string()).filter(|p| !p.is_empty()) {
            Some(pane) => pane,
            None => session
                .active_workspace_mut()
                .active_pane
                .clone()
                .ok_or_else(|| anyhow!("no active pane"))?,
        };
        // Validate the pane actually exists so callers fail with a clear
        // "unknown pane" here rather than a confusing downstream "not running"
        // error.
        if !session.panes.contains_key(&pane_id) {
            return Err(anyhow!("unknown pane {pane_id}"));
        }
        Ok(pane_id)
    }

    fn resolve_workspace(&self, workspace: Option<String>) -> Result<Option<String>> {
        let Some(workspace) = workspace else {
            return Ok(None);
        };
        // Accept a workspace name OR id, matching switch/rename.
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

    fn append_output(
        self: &Arc<Self>,
        runtime_key: &str,
        pane_id: &str,
        generation: u64,
        bytes: &[u8],
    ) {
        // Hot path: update the vt100 parser + light metadata only. Heavy screen
        // strings are materialized lazily in snapshot().
        let Some((light, notifications, auto_title, llm_screen)) = ({
            let mut panes = self.panes.lock_or_recover();
            let runtime = panes.get_mut(runtime_key);
            runtime.and_then(|runtime| {
                if runtime.generation != generation {
                    return None;
                }
                runtime.parser.process(bytes);
                runtime.terminal_modes.process(bytes);
                runtime.output_generation = runtime.output_generation.wrapping_add(1);
                let text = decode_utf8_stream(&mut runtime.pending, bytes);
                runtime.osc_tail.push_str(&text);
                let (osc, retain_from) = scan_osc_events(&runtime.osc_tail);
                let notifications = osc.notifications;
                if let Some(progress) = osc.progress {
                    runtime.pane.progress = progress;
                }
                if retain_from > 0 {
                    runtime.osc_tail.drain(..retain_from);
                }
                cap_osc_tail(&mut runtime.osc_tail);
                // Name the tab after what the agent says it is doing. Whether a
                // pane counts as an agent pane is decided inside, from the
                // process tree — the pane command is just the shell.
                let (auto_title, llm_screen) = if self.agent_titles.enabled {
                    self.agent_title_update(runtime, osc.title)
                } else {
                    (None, None)
                };
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
                    if !is_non_actionable_attention(message) {
                        touch_agent_status(&mut runtime.pane, AgentStatus::Attention, true);
                        runtime.pane.notification_color = Some("blue".to_string());
                        runtime.pane.notification_message = Some(message.clone());
                    }
                }
                // Light fields only — no contents()/formatted/scrollback join here.
                let (cursor_row, cursor_col) = runtime.parser.screen().cursor_position();
                runtime.pane.cursor_row = Some(cursor_row);
                runtime.pane.cursor_col = Some(cursor_col);
                let (screen_rows, screen_cols) = runtime.parser.screen().size();
                runtime.pane.screen_rows = Some(screen_rows);
                runtime.pane.screen_cols = Some(screen_cols);
                update_pane_terminal_modes(&mut runtime.pane, runtime.parser.screen());
                runtime.pane.alternate_scroll_mode = alternate_scroll_active(
                    &runtime.pane.status,
                    &runtime.terminal_modes,
                    runtime.parser.screen(),
                );
                // Clone light metadata for session merge (empty heavy strings).
                // The last snapshot may have cached the heavy screen strings on
                // runtime.pane; lift them out before cloning so we don't copy
                // (then discard) up to ~16KB × 4 on every PTY chunk, then put
                // them back (moves, no copy).
                let output = std::mem::take(&mut runtime.pane.output);
                let output_formatted = std::mem::take(&mut runtime.pane.output_formatted);
                let scrollback = std::mem::take(&mut runtime.pane.scrollback);
                let scrollback_formatted = std::mem::take(&mut runtime.pane.scrollback_formatted);
                let light = runtime.pane.clone();
                runtime.pane.output = output;
                runtime.pane.output_formatted = output_formatted;
                runtime.pane.scrollback = scrollback;
                runtime.pane.scrollback_formatted = scrollback_formatted;
                Some((light, notifications, auto_title, llm_screen))
            })
        }) else {
            return;
        };
        self.touch();
        // Both run outside the `panes` lock: renaming takes `session`, and the
        // summarizer shells out to another process.
        if let Some(title) = auto_title {
            self.apply_auto_tab_title(pane_id, &title);
        }
        if let Some(screen) = llm_screen {
            self.spawn_llm_tab_title(pane_id.to_string(), screen);
        }
        let mut should_save = false;
        {
            let mut session = self.session.lock_or_recover();
            if let Some(pane) = session.panes.get_mut(pane_id) {
                if runtime_key_is_active_for_pane(pane, runtime_key) {
                    // Merge light fields only; keep any previously snapshotted
                    // screen buffers until the next full snapshot refreshes them.
                    pane.updated_at = light.updated_at;
                    pane.agent_status = light.agent_status.clone();
                    pane.agent_status_pinned = light.agent_status_pinned;
                    pane.agent_status_at = light.agent_status_at;
                    pane.notification_color = light.notification_color.clone();
                    pane.notification_message = light.notification_message.clone();
                    pane.cursor_row = light.cursor_row;
                    pane.cursor_col = light.cursor_col;
                    pane.screen_rows = light.screen_rows;
                    pane.screen_cols = light.screen_cols;
                    pane.mouse_protocol_mode = light.mouse_protocol_mode.clone();
                    pane.mouse_protocol_encoding = light.mouse_protocol_encoding.clone();
                    pane.alternate_scroll_mode = light.alternate_scroll_mode;
                    pane.status = light.status.clone();
                    pane.pid = light.pid;
                    pane.progress = light.progress;
                } else if let Some(tab_id) = runtime_key_tab(pane_id, runtime_key) {
                    sync_tab_from_runtime_pane(pane, tab_id, &light);
                }
            }
            for message in notifications {
                if is_non_actionable_attention(&message) {
                    continue;
                }
                let note = Notification {
                    time: unix_time(),
                    pane: Some(pane_id.to_string()),
                    workspace: None,
                    status: Some("attention".to_string()),
                    color: Some("blue".to_string()),
                    clear: false,
                    message: message.clone(),
                };
                if !should_record_in_notification_feed(&note) {
                    continue;
                }
                let event_message = message;
                session.notifications.push(note);
                push_event(
                    &mut session,
                    EventRecord {
                        id: 0,
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
            self.schedule_save();
        }
    }

    /// Fold a fresh terminal title into a pane's auto-title state.
    ///
    /// Returns the title to put on the tab (when it changed) and, when the agent
    /// has gone `llm_delay_ms` without ever setting a usable title, the screen
    /// text to hand the summarizer. Caller must hold the `panes` lock.
    fn agent_title_update(
        &self,
        runtime: &mut PaneRuntime,
        osc_title: Option<String>,
    ) -> (Option<String>, Option<String>) {
        // Condense first: it is pure string work, and it rejects the titles a
        // plain shell sets (`~/code/vmux`, `user@host`) without touching /proc.
        if let Some(condensed) = osc_title.as_deref().and_then(condense_agent_title) {
            if !self.pane_runs_agent(runtime) {
                return (None, None);
            }
            // The agent names its own tab; the summarizer is not needed.
            runtime.llm_title_state = LlmTitleState::Done;
            if runtime.auto_title.as_deref() == Some(condensed.as_str()) {
                return (None, None);
            }
            runtime.auto_title = Some(condensed.clone());
            return (Some(condensed), None);
        }
        if runtime.llm_title_state != LlmTitleState::Pending || !self.agent_titles.llm_fallback {
            return (None, None);
        }
        // Only name a pane that is actually on a task. An agent parked at its
        // banner has nothing to name, and summarizing it would spend a model
        // call to produce a title about nothing.
        if !matches!(
            runtime.pane.agent_status,
            AgentStatus::Busy | AgentStatus::Attention
        ) {
            return (None, None);
        }
        let elapsed_ms = unix_time()
            .saturating_sub(runtime.started_at)
            .saturating_mul(1000);
        if elapsed_ms < self.agent_titles.llm_delay_ms {
            return (None, None);
        }
        if !self.pane_runs_agent(runtime) {
            return (None, None);
        }
        // One shot per pane: mark Done before the call so a slow summarizer
        // cannot be started twice by the next chunk of output.
        runtime.llm_title_state = LlmTitleState::Done;
        let screen = runtime.parser.screen().contents();
        if screen.trim().is_empty() {
            return (None, None);
        }
        (None, Some(screen))
    }

    /// Whether a coding agent is running in this pane — as the pane command
    /// (`vmux new-pane --command claude`) or, far more commonly, as a process
    /// the user started inside the pane's shell.
    ///
    /// The process-tree walk is cached for `AGENT_INSIDE_TTL_SECS`: it is only
    /// reached when a pane emits a title worth acting on or is due for the
    /// summarizer, but a shell that retitles on every prompt would otherwise
    /// walk /proc on every command.
    fn pane_runs_agent(&self, runtime: &mut PaneRuntime) -> bool {
        if is_coding_agent_command(&runtime.pane.command) {
            return true;
        }
        let Some(pid) = runtime.pane.pid else {
            return false;
        };
        let now = unix_time();
        if now.saturating_sub(runtime.agent_inside_at) < AGENT_INSIDE_TTL_SECS
            && runtime.agent_inside_at != 0
        {
            return runtime.agent_inside;
        }
        runtime.agent_inside = agent_running_in_pane(pid);
        runtime.agent_inside_at = now;
        runtime.agent_inside
    }

    /// Put an agent-derived title on the tab owning `pane_id`. Tabs the user has
    /// renamed by hand are left alone (`WorkspaceTab::title_locked`).
    fn apply_auto_tab_title(&self, pane_id: &str, title: &str) {
        let renamed = {
            let mut session = self.session.lock_or_recover();
            let Some(location) = session.find_pane_location(pane_id) else {
                return;
            };
            let Some(tab_id) = location.tab_id else {
                return;
            };
            let Some(workspace) = session
                .workspaces
                .iter_mut()
                .find(|workspace| workspace.id == location.workspace_id)
            else {
                return;
            };
            workspace.auto_rename_tab(&tab_id, title).is_some()
        };
        if renamed {
            self.schedule_save();
        }
    }

    /// Ask the configured headless agent to name a tab from what is on screen.
    /// Runs detached: a slow or missing summarizer must never stall a PTY reader.
    fn spawn_llm_tab_title(self: &Arc<Self>, pane_id: String, screen: String) {
        let server = Arc::clone(self);
        let command = self.agent_titles.llm_command.clone();
        thread::spawn(move || match llm_tab_title(&command, &screen) {
            Ok(Some(title)) => server.apply_auto_tab_title(&pane_id, &title),
            Ok(None) => {}
            Err(err) => {
                eprintln!("vmux: agent tab title via `{command}` failed: {err}");
            }
        });
    }

    fn schedule_save(&self) {
        self.touch();
        self.save_dirty
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    fn save_loop(self: Arc<Self>) {
        loop {
            thread::sleep(Duration::from_millis(400));
            // Stop before writing once shutdown has begun. `Shutdown` does its
            // own final save and then removes the socket/pid, which makes
            // `is_running()` false while this process is still alive — so a
            // debounced write landing here would re-create the state file after
            // a caller (e.g. `vmux smoke`) had already cleaned it up.
            if self
                .shutting_down
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                return;
            }
            if self
                .save_dirty
                .swap(false, std::sync::atomic::Ordering::Relaxed)
            {
                if let Err(err) = self.save() {
                    self.log(&format!("debounced save failed: {err:#}")).ok();
                }
            }
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
                runtime.terminal_modes = TerminalModeTracker::default();
                runtime.pane.alternate_scroll_mode = false;
                runtime.pane.updated_at = unix_time();
                // Materialize final screen/scrollback into the pane record so
                // save/restart keep history even when no full snapshot runs.
                let contents = runtime.parser.screen().contents();
                let raw = runtime.joined_output();
                runtime.pane.output = contents;
                runtime.pane.scrollback = trim_output(raw, self.scrollback_cap);
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
                    id: 0,
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
        self.exit_notify.1.notify_all();
        self.save().ok();
    }

    fn snapshot(&self, include_output: bool) -> Result<Session> {
        self.snapshot_opts(include_output, None)
    }

    /// `lean_scrollback: Some(keep)` enables the attach-UI lean payload:
    /// `events` and per-pane/tab scrollback strings are stripped (except for
    /// panes in `keep`, which the client is scrolled back in) and every pane
    /// carries `scrollback_lines` so the client can clamp scrolling. Lean
    /// snapshots never feed persistence — `save()` always uses `full`.
    fn snapshot_opts(
        &self,
        include_output: bool,
        lean_scrollback: Option<&BTreeSet<String>>,
    ) -> Result<Session> {
        let mut session = self.session.lock_or_recover().clone();
        session.daemon = Some(self.daemon_info());

        // Read cached git/gh/ss metadata (refreshed by the background thread).
        // On a cache miss keep the persisted values so fields stay populated.
        {
            let meta = self.workspace_meta.lock_or_recover();
            for workspace in &mut session.workspaces {
                if let Some(cached) = meta.get(&workspace.id) {
                    workspace.git_branch = cached.git_branch.clone();
                    // No longer populated: vmux stopped querying GitHub for PR
                    // state (background `gh pr view` polling billed the user's
                    // API quota). Clear rather than skip, so a value persisted
                    // by an older daemon cannot linger as a stale chip.
                    workspace.pull_request = None;
                    workspace.ports = cached.ports.clone();
                    // Display-only overlay: the persisted workspace cwd (used
                    // to spawn new panes) is left untouched.
                    if let Some(cwd) = &cached.cwd {
                        workspace.cwd = cwd.clone();
                    }
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
                    pane.alternate_scroll_mode = alternate_scroll_active(
                        &runtime.pane.status,
                        &runtime.terminal_modes,
                        runtime.parser.screen(),
                    );
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
                        let lean_skip = lean_scrollback
                            .map(|keep| !keep.contains(&pane.id))
                            .unwrap_or(false);
                        if lean_skip {
                            // Lean poll: the client only needs the live screen
                            // plus a line count for scroll clamping — skip the
                            // joined-output copy and styled-scrollback rebuild.
                            pane.scrollback_lines = Some(chunked_line_count(&runtime.output));
                            Some((contents, formatted, String::new(), String::new()))
                        } else {
                            // Rebuild styled scrollback only when output changed.
                            if runtime.scrollback_formatted_generation != runtime.output_generation
                            {
                                runtime.scrollback_formatted_cache = screen_scrollback_formatted(
                                    &mut runtime.parser,
                                    SCROLLBACK_FORMATTED_ROW_CAP,
                                );
                                runtime.scrollback_formatted_generation = runtime.output_generation;
                            }
                            // Also keep plain scrollback string in runtime for persist/save.
                            let raw = runtime.joined_output();
                            runtime.pane.scrollback = trim_output(raw.clone(), self.scrollback_cap);
                            runtime.pane.output = contents.clone();
                            runtime.pane.output_formatted = formatted.clone();
                            runtime.pane.scrollback_formatted =
                                runtime.scrollback_formatted_cache.clone();
                            Some((
                                contents,
                                formatted,
                                raw,
                                runtime.scrollback_formatted_cache.clone(),
                            ))
                        }
                    } else {
                        // Light snapshot: cursor/status only, no heavy strings.
                        let (cursor_row, cursor_col) = runtime.parser.screen().cursor_position();
                        pane.cursor_row = Some(cursor_row);
                        pane.cursor_col = Some(cursor_col);
                        let (screen_rows, screen_cols) = runtime.parser.screen().size();
                        pane.screen_rows = Some(screen_rows);
                        pane.screen_cols = Some(screen_cols);
                        update_pane_terminal_modes(&mut pane, runtime.parser.screen());
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
                pane.scrollback = trim_output(raw, self.scrollback_cap);
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
        if let Some(keep) = lean_scrollback {
            // Event history has its own RPC (Request::Events); the attach UI
            // never reads it from snapshots, and it dominates payload size.
            session.events.clear();
            for pane in session.panes.values_mut() {
                // Panes that skipped the runtime fast path (scrolled, dead, or
                // tab-synced) still need a line count for scroll clamping.
                if pane.scrollback_lines.is_none() {
                    pane.scrollback_lines = Some(pane.scrollback.lines().count());
                }
                if !keep.contains(&pane.id) {
                    pane.scrollback.clear();
                    pane.scrollback_formatted.clear();
                }
                // Tab content strings are never rendered by the attach UI (the
                // active tab's content is mirrored into the pane fields).
                for tab in &mut pane.tabs {
                    tab.output.clear();
                    tab.output_formatted.clear();
                    tab.scrollback.clear();
                    tab.scrollback_formatted.clear();
                }
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
                // Include panes on every tab, not only the active-tab live view.
                workspace.all_pane_ids().into_iter().filter_map(|pane_id| {
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

        let location = snapshot
            .find_pane_location(&target_pane)
            .ok_or_else(|| anyhow!("pane {target_pane} is not attached to a workspace"))?;
        let workspace = snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.id == location.workspace_id)
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
            "tab": location.tab_id,
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
            // Read from the cache the background thread keeps fresh (cheap).
            update_available: crate::update::available_update(),
        }
    }

    /// Background thread: keep the update-check cache fresh. `refresh_if_stale`
    /// is a no-op until the TTL elapses and fails silently, so a cheap hourly
    /// tick is enough to notice a new release within a day.
    fn update_check_loop(self: Arc<Self>) {
        loop {
            crate::update::refresh_if_stale();
            thread::sleep(Duration::from_secs(3600));
        }
    }

    /// Background thread: self-heal stale 🔄. Demotes any *unpinned* (heuristic)
    /// Busy pane that has produced no output for `BUSY_IDLE_DECAY_SECS` back to
    /// Idle. Pinned Busy (a real hook/CLI signal) is left alone, so a silently
    /// thinking agent keeps its spinner. Sweeps both the runtime panes (so the
    /// next output frame merges from Idle, not sticky Busy) and the session copy
    /// (what the UI shows) — each judged by its own last-output time.
    fn agent_status_decay_loop(self: Arc<Self>) {
        loop {
            thread::sleep(Duration::from_secs(DECAY_TICK_SECS));
            let now = unix_time();
            let mut changed = false;
            {
                let mut panes = self.panes.lock_or_recover();
                for runtime in panes.values_mut() {
                    changed |= decay_stale_busy(&mut runtime.pane, now, BUSY_IDLE_DECAY_SECS);
                }
            }
            {
                let mut session = self.session.lock_or_recover();
                for pane in session.panes.values_mut() {
                    changed |= decay_stale_busy(pane, now, BUSY_IDLE_DECAY_SECS);
                }
            }
            // Same tick also reaps expired phone-fit view leases, so a viewer
            // that vanished restores the pane within DECAY_TICK_SECS + lease.
            changed |= self.expire_view_overrides(Instant::now());
            if changed {
                self.touch();
                self.save().ok();
            }
        }
    }

    fn write_pid_file(&self) -> Result<()> {
        paths::write_pid_record(&self.pid_path, std::process::id())
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

    /// Release the session flock so a successor daemon can start immediately.
    ///
    /// `Shutdown` unlinks the socket before this process exits, so callers see
    /// "not running" while the flock is still held for the response-flush grace
    /// period. Without an explicit unlock, an `ensure_running` issued in that
    /// window (the restore phase of `vmux smoke` does exactly this) spawns a
    /// daemon that fails `try_lock_session` and dies — "vmux daemon helper
    /// exited with exit status: 0".
    #[cfg(unix)]
    fn release_session_lock(&self) {
        use std::os::unix::io::AsRawFd;
        if let Some(lock) = &self._session_lock {
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) };
        }
    }

    #[cfg(not(unix))]
    fn release_session_lock(&self) {}

    fn save(&self) -> Result<()> {
        self.touch();
        // Hold save_lock across the ENTIRE save — snapshot capture through
        // rename — so two concurrent saves can't build snapshots out of order
        // and rename an older payload over a newer one (which after a crash
        // would resurrect killed panes / drop new ones). The lock also gives
        // each save a unique temp name via the per-save counter below.
        let mut counter = self.save_lock.lock_or_recover();
        // Keep active-tab records in sync with live layout fields before
        // persisting (inactive tabs already hold their own layout).
        {
            let mut session = self.session.lock_or_recover();
            session.flush_tabs();
        }
        // Persist must materialize runtime screen/scrollback.
        // Light `snapshot(false)` leaves empty strings from append_output's
        // hot path and loses history across daemon restart.
        let mut snapshot = self.snapshot(true)?;
        snapshot.daemon = None;
        // View overrides are live-viewer leases, not layout: persisting one
        // would resurrect a phone-sized pane after restart with no phone
        // attached. Strip them; load() clears any that slip through.
        for pane in snapshot.panes.values_mut() {
            pane.view_size = None;
            pane.alternate_scroll_mode = false;
        }
        let payload = serde_json::to_vec_pretty(&snapshot)?;
        *counter = counter.wrapping_add(1);
        let tmp =
            self.state_path
                .with_extension(format!("json.tmp.{}.{}", std::process::id(), *counter));
        fs::write(&tmp, &payload)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
        }
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

/// Drive a pane's PTY and vt100 parser to `next`, recording it as the
/// effective size. No-op when already there. An exited pane has no master;
/// its parser is still resized so restored output reflows correctly.
fn apply_pty_size(runtime: &mut PaneRuntime, next: PaneSize) -> Result<()> {
    if runtime.size == next {
        return Ok(());
    }
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
    Ok(())
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

/// Truncate `s` to at most `max` bytes without splitting a UTF-8 character.
/// `String::truncate` panics when the byte index is not a char boundary, and
/// these messages come straight from arbitrary pane output / user input.
fn truncate_on_char_boundary(s: &mut String, max: usize) {
    if s.len() <= max {
        return;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    s.push('…');
}

fn push_event(session: &mut Session, mut event: EventRecord) {
    session.next_event_id = session.next_event_id.saturating_add(1).max(1);
    event.id = session.next_event_id;
    // Bound message length at the protocol boundary.
    const MAX_EVENT_MSG: usize = 4_096;
    truncate_on_char_boundary(&mut event.message, MAX_EVENT_MSG);
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

mod browser;
pub(crate) use browser::*;

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

/// True for informational "the prompt is idle" notices. Claude Code fires its
/// Notification hook (and, when configured, an OSC 9 terminal notification)
/// with "Claude is waiting for your input" 60s after a turn ends. Unlike a
/// permission request ("Claude needs your permission to use Bash"), it asks
/// nothing new of the user, so it must not flip a finished-turn ✅ into 🙋.
fn is_idle_prompt_notification(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("waiting for your input")
        || message.contains("waiting for input")
        || message.contains("finished responding")
}

/// Completion-flavoured Notification messages (Grok "Turn complete", etc.) that
/// must not raise 🙋 — they are done-turn noise, not a request for input.
fn is_completion_noise_notification(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message == "turn complete"
        || message == "end_turn"
        || message == "end turn"
        || message.starts_with("turn complete")
        || message.contains("response complete")
}

/// Attention-class messages that must not change agent_status / banner.
fn is_non_actionable_attention(message: &str) -> bool {
    is_idle_prompt_notification(message) || is_completion_noise_notification(message)
}

/// Default lifecycle messages from `hooks event` — sidebar already shows 🔄/✅.
fn is_boilerplate_lifecycle_message(message: &str) -> bool {
    matches!(
        message.trim().to_ascii_lowercase().as_str(),
        "agent working"
            | "agent hook completed"
            | "agent hook event"
            | "agent hook failed"
            | "end_turn"
            | "turn complete"
    )
}

/// Whether a notify should appear in the session notification feed / panel.
/// Routine busy↔done hook chatter is status, not a notification the user jumps to.
fn should_record_in_notification_feed(note: &Notification) -> bool {
    if note.clear || note.message.is_empty() {
        return false;
    }
    // Leftover progress-misparse strings from older daemons / partial OSC.
    if note
        .message
        .chars()
        .all(|c| c.is_ascii_digit() || c == ':' || c.is_whitespace())
        && note.message.contains(':')
    {
        return false;
    }
    match note
        .status
        .as_deref()
        .map(parse_agent_status)
        .unwrap_or(AgentStatus::Unknown)
    {
        AgentStatus::Busy | AgentStatus::Done | AgentStatus::Idle => {
            !is_boilerplate_lifecycle_message(&note.message)
        }
        AgentStatus::Attention if is_non_actionable_attention(&note.message) => false,
        _ => true,
    }
}

/// Pane is finished (✅) or user-acknowledged idle — late tool hooks must not
/// flip it back to 🔄 without a real new-turn signal.
fn is_settled_after_turn(pane: &Pane) -> bool {
    matches!(pane.agent_status, AgentStatus::Done)
        || (matches!(pane.agent_status, AgentStatus::Idle | AgentStatus::Unknown)
            && pane.agent_status_pinned)
}

/// Grok (and others) often fire PreToolUse/PostToolUse with "agent working"
/// after Stop has already set ✅. That late boilerplate must not demote a
/// finished turn. A real new turn carries a UserPromptSubmit title and/or a
/// custom busy message (`set-status busy --message "fix auth"`).
fn should_skip_busy_after_settled(
    pane: &Pane,
    status: &str,
    message: &str,
    has_new_turn_title: bool,
) -> bool {
    if !matches!(parse_agent_status(status), AgentStatus::Busy) {
        return false;
    }
    if !is_settled_after_turn(pane) {
        return false;
    }
    if has_new_turn_title {
        return false;
    }
    is_boilerplate_lifecycle_message(message)
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
        .find(|workspace| workspace.contains_pane(pane_id))
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
    // Stop at $HOME so a workspace under /tmp cannot pick up /tmp/vmux.json
    // planted by another user.
    let home = dirs::home_dir();
    for dir in cwd.ancestors() {
        for name in ["vmux.json", ".vmux.json"] {
            let path = dir.join(name);
            if path.is_file() {
                return Some(path);
            }
        }
        if home.as_ref().is_some_and(|h| dir == h) {
            break;
        }
        if dir.parent().is_none() {
            break;
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

/// Live working directory of a pane's shell via `/proc/<pid>/cwd`, so the
/// sidebar path follows `cd`. Best-effort: `None` when the process is gone,
/// the cwd was deleted, or `/proc` is unavailable (non-Linux).
/// The pane process's current working directory.
///
/// Linux only: this reads `/proc/<pid>/cwd`, which macOS does not have, so it
/// returns `None` there and the sidebar falls back to the workspace's stored
/// cwd rather than the pane's live one. Reading it on macOS needs
/// `proc_pidinfo(PROC_PIDVNODEPATHINFO)` via libproc.
fn pane_live_cwd(pid: u32) -> Option<String> {
    let path = std::fs::read_link(format!("/proc/{pid}/cwd")).ok()?;
    path.is_absolute().then(|| path.display().to_string())
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

/// True when a coding agent is running inside the pane rooted at `pid`.
///
/// The pane's own command is almost always the user's shell — you open a pane
/// and *then* type `claude` — so the agent shows up as a descendant process, not
/// as the pane command. Walks the pane's process tree and matches each command
/// line, which also catches agents launched through an interpreter
/// (`node .../bin/claude`).
fn agent_running_in_pane(pid: u32) -> bool {
    descendant_pids(&[pid])
        .into_iter()
        .filter(|child| *child != pid)
        .any(|child| {
            // `comm` is the kernel's name for the process, which names the agent
            // even when it is a wrapper script (`~/bin/claude` → comm `claude`).
            let comm = fs::read_to_string(format!("/proc/{child}/comm")).unwrap_or_default();
            if is_coding_agent_command(comm.trim()) {
                return true;
            }
            // `cmdline` catches agents run through an interpreter, where comm is
            // just `node` (`node .../claude-code/cli.js`). It is NUL-separated.
            let cmdline = fs::read(format!("/proc/{child}/cmdline")).unwrap_or_default();
            let cmdline = String::from_utf8_lossy(&cmdline).replace('\0', " ");
            is_coding_agent_command(cmdline.trim())
        })
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

/// Everything the OSC scanner pulls out of one read.
#[derive(Debug, Default)]
struct OscEvents {
    /// Desktop notifications (OSC 9 / 99 / 777).
    notifications: Vec<String>,
    /// The window title the program last set (OSC 0 / 2). Only the final one in
    /// a read matters — agents rewrite the title as they work.
    title: Option<String>,
    /// ConEmu / Windows Terminal / Ghostty progress bar (OSC 9;4;…).
    /// `Some(None)` clears the bar; `Some(Some(n))` sets 0–100.
    progress: Option<Option<u8>>,
}

/// Scan `buf` for the OSC sequences vmux acts on. Returns the events and the
/// byte offset from which `buf` should be retained: the start of a trailing
/// unterminated OSC sequence (so it can be completed by a later read), or
/// `buf.len()` when everything up to the end was consumed.
fn scan_osc_events(buf: &str) -> (OscEvents, usize) {
    let mut events = OscEvents::default();
    let mut idx = 0;
    loop {
        let Some(rel) = buf[idx..].find("\x1b]") else {
            return (events, buf.len());
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
            return (events, start);
        };
        let payload = &buf[after..after + end];
        // Progress first: OSC 9;4 is *not* a notification. Misparsing it as one
        // produced feed spam ("4: 1" / "4: 0") and flipped panes to 🙋.
        if let Some(progress) = osc_progress_update(payload) {
            events.progress = Some(progress);
        } else if let Some(message) = osc_notification_message(payload) {
            events.notifications.push(message);
        } else if let Some(title) = osc_window_title(payload) {
            events.title = Some(title);
        }
        idx = if st == Some(end) {
            after + end + 2
        } else {
            after + end + 1
        };
    }
}

/// OSC 9;4 progress report (ConEmu / Windows Terminal / Ghostty / many CLIs).
///
/// `OSC 9 ; 4 ; <state> [; <progress>] ST`
/// - state 0 → remove progress bar
/// - state 1 → set progress 0–100
/// - states 2/3/4 → error / indeterminate / paused (we clear the numeric bar)
fn osc_progress_update(payload: &str) -> Option<Option<u8>> {
    let rest = payload.strip_prefix("9;4")?;
    // Accept bare "9;4" / "9;4;" as clear.
    let rest = rest.strip_prefix(';').unwrap_or(rest).trim();
    if rest.is_empty() {
        return Some(None);
    }
    let mut parts = rest.split(';').map(str::trim).filter(|p| !p.is_empty());
    let state = parts.next()?;
    match state {
        "0" => Some(None),
        "1" => {
            let pct = parts
                .next()
                .and_then(|p| p.parse::<u16>().ok())
                .map(|n| n.min(100) as u8)
                .unwrap_or(0);
            Some(Some(pct))
        }
        // Error / indeterminate / paused: drop the numeric bar rather than
        // inventing a fake percentage.
        "2" | "3" | "4" => Some(None),
        _ => None,
    }
}

/// How long the tab-title summarizer may run before it is killed.
const LLM_TITLE_TIMEOUT_SECS: u64 = 30;
/// How much of the agent's screen the summarizer is shown.
const LLM_TITLE_CONTEXT_CHARS: usize = 2000;

/// Build the summarizer prompt from the tail of an agent's screen.
fn llm_tab_title_prompt(screen: &str) -> String {
    let screen = screen.trim();
    // The tail is where the current task is; the top of the screen is banner.
    let start = screen
        .char_indices()
        .nth(
            screen
                .chars()
                .count()
                .saturating_sub(LLM_TITLE_CONTEXT_CHARS),
        )
        .map(|(index, _)| index)
        .unwrap_or(0);
    let context = &screen[start..];
    format!(
        "Below is the current screen of a coding agent working in a terminal.\n\
         Reply with a one or two word lowercase label naming the task it is working on \
         — the kind of name you would give a tab. Reply with the words only: no \
         punctuation, no quotes, no explanation.\n\n\
         SCREEN:\n{context}"
    )
}

/// Run the configured summarizer over `screen` and condense its answer into a
/// tab title. `Ok(None)` when it declines to answer or answers with nothing
/// usable — the tab then simply keeps its current name.
fn llm_tab_title(command: &str, screen: &str) -> Result<Option<String>> {
    let argv = shell_words::split(command)
        .with_context(|| format!("invalid agent_titles.llm_command: {command}"))?;
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| anyhow!("agent_titles.llm_command is empty"))?;
    let mut child = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("spawn {program}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        // Dropped right after, which closes the pipe so the agent sees EOF.
        stdin
            .write_all(llm_tab_title_prompt(screen).as_bytes())
            .ok();
    }
    // A summarizer that never exits would otherwise leak this thread forever.
    let pid = child.id() as i32;
    let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watchdog = Arc::clone(&finished);
    thread::spawn(move || {
        for _ in 0..LLM_TITLE_TIMEOUT_SECS * 10 {
            thread::sleep(Duration::from_millis(100));
            if watchdog.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
        }
        #[cfg(unix)]
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    });
    let output = child
        .wait_with_output()
        .with_context(|| format!("run {program}"))?;
    finished.store(true, std::sync::atomic::Ordering::Relaxed);
    if !output.status.success() {
        return Ok(None);
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Agents sometimes preface the answer; the label is the last non-empty line.
    let answer = stdout.lines().rev().find(|line| !line.trim().is_empty());
    Ok(answer.and_then(condense_agent_title))
}

/// Window title from `OSC 0` (icon + title) or `OSC 2` (title). `OSC 1` sets the
/// icon name only and is ignored.
fn osc_window_title(payload: &str) -> Option<String> {
    let rest = payload
        .strip_prefix("0;")
        .or_else(|| payload.strip_prefix("2;"))?;
    let title = rest.trim();
    if title.is_empty() {
        return None;
    }
    // Bound what an untrusted PTY can hand us before it reaches the condenser.
    Some(title.chars().take(256).collect())
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
    scan_osc_events(text).0.notifications
}

#[cfg(test)]
fn osc_title(text: &str) -> Option<String> {
    scan_osc_events(text).0.title
}

fn osc_notification_message(payload: &str) -> Option<String> {
    // OSC 9;4 is progress (handled separately) — never a desktop notification.
    if payload.starts_with("9;4") {
        return None;
    }
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
        // Lone numeric "4" would be a partial progress sequence we didn't
        // recognize; never surface it as a user-visible notification.
        [message] if message.chars().all(|c| c.is_ascii_digit()) => None,
        [message] => Some((*message).to_string()),
        // "4: 1" / "4: 0" was the classic misparse of OSC 9;4 progress.
        [title, body, ..]
            if title.chars().all(|c| c.is_ascii_digit())
                && body.chars().all(|c| c.is_ascii_digit()) =>
        {
            None
        }
        [title, body, ..] => Some(format!("{title}: {body}")),
    }
}

pub(crate) fn trim_output(output: String, max_len: usize) -> String {
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

/// Attribute set of one cell, compared cell-to-cell to emit minimal SGR runs.
#[derive(PartialEq, Clone, Copy, Default)]
struct SgrStyle {
    fg: vt100::Color,
    bg: vt100::Color,
    bold: bool,
    italic: bool,
    underline: bool,
    inverse: bool,
}

fn push_sgr(out: &mut String, style: &SgrStyle) {
    use std::fmt::Write;
    out.push_str("\x1b[0");
    if style.bold {
        out.push_str(";1");
    }
    if style.italic {
        out.push_str(";3");
    }
    if style.underline {
        out.push_str(";4");
    }
    if style.inverse {
        out.push_str(";7");
    }
    match style.fg {
        vt100::Color::Default => {}
        vt100::Color::Idx(i) if i < 8 => {
            let _ = write!(out, ";{}", 30 + u16::from(i));
        }
        vt100::Color::Idx(i) if i < 16 => {
            let _ = write!(out, ";{}", 82 + u16::from(i));
        }
        vt100::Color::Idx(i) => {
            let _ = write!(out, ";38;5;{i}");
        }
        vt100::Color::Rgb(r, g, b) => {
            let _ = write!(out, ";38;2;{r};{g};{b}");
        }
    }
    match style.bg {
        vt100::Color::Default => {}
        vt100::Color::Idx(i) if i < 8 => {
            let _ = write!(out, ";{}", 40 + u16::from(i));
        }
        vt100::Color::Idx(i) if i < 16 => {
            let _ = write!(out, ";{}", 92 + u16::from(i));
        }
        vt100::Color::Idx(i) => {
            let _ = write!(out, ";48;5;{i}");
        }
        vt100::Color::Rgb(r, g, b) => {
            let _ = write!(out, ";48;2;{r};{g};{b}");
        }
    }
    out.push('m');
}

/// One visible row of `screen` as text with self-contained SGR colour codes
/// (reset re-established mid-row on attribute changes, reset at row end).
/// Respects the screen's scrollback offset, which is what lets the relay
/// replay pane history row by row. Used by the whole-screen serializer below.
pub(crate) fn row_contents_ansi(screen: &vt100::Screen, row: u16) -> String {
    let (_, cols) = screen.size();
    let mut out = String::new();
    let mut current = SgrStyle::default();
    for col in 0..cols {
        let Some(cell) = screen.cell(row, col) else {
            continue;
        };
        // A wide glyph's continuation cell repeats nothing.
        if cell.is_wide_continuation() {
            continue;
        }
        let style = SgrStyle {
            fg: cell.fgcolor(),
            bg: cell.bgcolor(),
            bold: cell.bold(),
            italic: cell.italic(),
            underline: cell.underline(),
            inverse: cell.inverse(),
        };
        if style != current {
            push_sgr(&mut out, &style);
            current = style;
        }
        let contents = cell.contents();
        if contents.is_empty() {
            out.push(' ');
        } else {
            out.push_str(contents);
        }
    }
    if current != SgrStyle::default() {
        out.push_str("\x1b[0m");
    }
    out
}

/// The pane's true history: the live parser's scrollback rows, oldest first.
///
/// Loss-free where raw-ring replay is not — diff-drawing TUIs skip cells that
/// have not changed, so a replay elsewhere reconstructs fragments, but this
/// grid has been fed every byte since the pane started. Walks scrollback
/// offsets (each puts one older row at visible row 0) and restores the view.
fn parser_history_rows(parser: &mut vt100::Parser, lines: usize, ansi: bool) -> Vec<String> {
    parser.screen_mut().set_scrollback(usize::MAX);
    let available = parser.screen().scrollback().min(lines);
    let mut rows = Vec::with_capacity(available);
    for offset in (1..=available).rev() {
        parser.screen_mut().set_scrollback(offset);
        let row = if ansi {
            row_contents_ansi(parser.screen(), 0)
        } else {
            let screen = parser.screen();
            let (_, cols) = screen.size();
            let mut plain = String::new();
            for col in 0..cols {
                let Some(cell) = screen.cell(0, col) else {
                    continue;
                };
                if cell.is_wide_continuation() {
                    continue;
                }
                let contents = cell.contents();
                if contents.is_empty() {
                    plain.push(' ');
                } else {
                    plain.push_str(contents);
                }
            }
            plain
        };
        rows.push(row.trim_end().to_string());
    }
    parser.screen_mut().set_scrollback(0);
    rows
}

/// Serialize a vt100 screen to text with SGR colour codes, one line per row.
/// Only SGR is emitted — no cursor movement, no OSC — so a client needs
/// nothing beyond a small colour-code parser.
fn screen_contents_ansi(screen: &vt100::Screen) -> String {
    let (rows, _) = screen.size();
    let mut out = String::new();
    for row in 0..rows {
        if row > 0 {
            out.push('\n');
        }
        out.push_str(&row_contents_ansi(screen, row));
    }
    out
}

fn scrollback_limit(limit_bytes: Option<usize>) -> usize {
    limit_bytes.unwrap_or(16_000).min(1_000_000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn terminal_mode_tracker_handles_split_alternate_scroll_sequences() {
        let mut modes = TerminalModeTracker::default();

        modes.process(b"plain\x1b[?100");
        assert!(!modes.alternate_scroll);
        modes.process(b"7h");
        assert!(modes.alternate_scroll);

        // DECSET/DECRST may carry more than one private mode parameter.
        modes.process(b"\x1b[?25;1007l");
        assert!(!modes.alternate_scroll);
        modes.process(b"\x1b[?1000;1007h");
        assert!(modes.alternate_scroll);

        // Similar text and unrelated CSI sequences must not change the mode.
        modes.process(b"?1007l\x1b[31m\x1b[?1006l");
        assert!(modes.alternate_scroll);
    }

    #[test]
    fn alternate_scroll_is_effective_only_on_the_alternate_screen() {
        let mut modes = TerminalModeTracker::default();
        let mut parser = vt100::Parser::new(24, 80, 100);

        modes.process(b"\x1b[?1007h");
        assert!(!alternate_scroll_active(
            &PaneStatus::Running,
            &modes,
            parser.screen()
        ));

        parser.process(b"\x1b[?1049h");
        assert!(alternate_scroll_active(
            &PaneStatus::Running,
            &modes,
            parser.screen()
        ));
        // A retained parser may still report the alternate screen after a
        // forced exit; pane status must prevent snapshots re-enabling input.
        assert!(!alternate_scroll_active(
            &PaneStatus::Exited,
            &modes,
            parser.screen()
        ));

        parser.process(b"\x1b[?1049l");
        assert!(!alternate_scroll_active(
            &PaneStatus::Running,
            &modes,
            parser.screen()
        ));
    }

    #[test]
    fn clearing_parser_preserves_fullscreen_input_modes() {
        let mut parser = vt100::Parser::new(24, 80, 100);
        parser.process(b"\x1b[?1049h\x1b[?1h\x1b[?1000h\x1b[?1006h\x1b[?2004h\x1b[?25lvisible");
        let mut modes = TerminalModeTracker::default();
        modes.process(b"\x1b[?1007h");

        let cleared = cleared_parser_preserving_terminal_modes(&parser, 24, 80);
        let screen = cleared.screen();
        assert!(screen.contents().is_empty());
        assert!(screen.alternate_screen());
        assert!(screen.application_cursor());
        assert!(screen.bracketed_paste());
        assert!(screen.hide_cursor());
        assert_eq!(
            screen.mouse_protocol_mode(),
            vt100::MouseProtocolMode::PressRelease
        );
        assert_eq!(
            screen.mouse_protocol_encoding(),
            vt100::MouseProtocolEncoding::Sgr
        );
        assert!(alternate_scroll_active(
            &PaneStatus::Running,
            &modes,
            screen
        ));
    }

    #[test]
    fn parser_history_rows_returns_complete_lines() {
        let mut parser = vt100::Parser::new(4, 30, 100);
        for i in 1..=20 {
            parser.process(format!("full-line-{i}\r\n").as_bytes());
        }
        let history = parser_history_rows(&mut parser, 50, false);
        assert!(history.iter().any(|r| r == "full-line-1"), "{history:?}");
        // Complete rows — never a suffix fragment.
        assert!(history
            .iter()
            .all(|r| r.is_empty() || r.starts_with("full-line-")));
        // The view offset is restored so live reads are unaffected.
        assert_eq!(parser.screen().scrollback(), 0);
    }

    #[test]
    fn parser_history_rows_caps_and_orders() {
        let mut parser = vt100::Parser::new(4, 30, 100);
        for i in 1..=60 {
            parser.process(format!("l{i}\r\n").as_bytes());
        }
        let history = parser_history_rows(&mut parser, 10, false);
        assert_eq!(history.len(), 10);
        // Newest window of history, oldest first within it.
        let nums: Vec<i32> = history
            .iter()
            .map(|r| r.trim_start_matches('l').parse().unwrap())
            .collect();
        let mut sorted = nums.clone();
        sorted.sort_unstable();
        assert_eq!(nums, sorted);
    }

    #[test]
    fn screen_contents_ansi_preserves_colors_and_resets_per_row() {
        let mut parser = vt100::Parser::new(4, 20, 0);
        parser.process(b"\x1b[31mred\x1b[0m plain\r\n\x1b[1;38;5;46mbold-green\x1b[0m\r\nno-style");
        let out = screen_contents_ansi(parser.screen());
        let rows: Vec<&str> = out.split('\n').collect();
        assert_eq!(rows.len(), 4);

        // Row 0: red fg for "red", reset back to default for " plain".
        assert!(rows[0].contains("\x1b[0;31mred"), "row0 = {:?}", rows[0]);
        assert!(rows[0].contains("\x1b[0m plain"), "row0 = {:?}", rows[0]);
        // Rows are self-contained: any styled row ends in a reset.
        assert!(rows[0].trim_end().ends_with('n') || rows[0].ends_with("\x1b[0m"));

        // Row 1: bold + 256-colour foreground survive.
        assert!(
            rows[1].contains("\x1b[0;1;38;5;46mbold-green"),
            "row1 = {:?}",
            rows[1]
        );

        // Row 2: no escapes at all when nothing is styled.
        assert!(!rows[2].contains('\x1b'), "row2 = {:?}", rows[2]);
        assert!(rows[2].starts_with("no-style"));
    }

    #[test]
    fn screen_contents_ansi_plain_screen_matches_contents() {
        let mut parser = vt100::Parser::new(2, 10, 0);
        parser.process(b"hello");
        let ansi = screen_contents_ansi(parser.screen());
        // With no styling anywhere, the ANSI form differs from contents() only
        // by preserved cell padding (spaces), never by escapes.
        assert!(!ansi.contains('\x1b'));
        assert!(ansi.starts_with("hello"));
    }

    /// A disposable session name that removes its own runtime/state files.
    ///
    /// Daemon tests write to the *real* XDG dirs, so they must not collide with
    /// each other or leave anything behind. Two things went wrong before this
    /// existed:
    ///
    /// 1. Names were built from `unix_time()` alone — one-second granularity and
    ///    no PID, so two tests in the same second (or two `cargo test` runs in
    ///    two worktrees, which the repo's own workflow encourages) collided on
    ///    the same session and the same flock.
    /// 2. Cleanup was a bare statement at the end of the test body, so any
    ///    failing assertion panicked straight past it. The leaked `*.json` files
    ///    then showed up as phantom sessions in the developer's real `vmux ls`.
    ///
    /// Cleanup lives in `Drop` so it runs on the panicking path too.
    struct TestSession {
        name: String,
    }

    impl TestSession {
        fn new(label: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            Self {
                name: format!(
                    "vmux-test-{label}-{}-{}-{unique}",
                    std::process::id(),
                    unix_time()
                ),
            }
        }

        fn as_str(&self) -> &str {
            &self.name
        }
    }

    impl Drop for TestSession {
        fn drop(&mut self) {
            for path in [
                paths::state_path(&self.name).ok(),
                paths::lock_path(&self.name).ok(),
                paths::pid_path(&self.name).ok(),
                paths::socket_path(&self.name).ok(),
                paths::log_path(&self.name).ok(),
            ]
            .into_iter()
            .flatten()
            {
                fs::remove_file(path).ok();
            }
        }
    }

    #[test]
    fn handle_stream_returns_json_error_for_unknown_request() {
        let session = TestSession::new("decode");
        let server = Arc::new(Server::load(session.as_str()).unwrap());
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
    fn idle_prompt_notification_does_not_demote_done() {
        let session = TestSession::new("idle-note");
        let server = Arc::new(Server::load(session.as_str()).unwrap());
        {
            let mut session = server.session.lock_or_recover();
            let mut pane = Pane::new(
                "pane-1".to_string(),
                "claude".to_string(),
                SplitDirection::Right,
            );
            touch_agent_status(&mut pane, AgentStatus::Done, true);
            session.panes.insert("pane-1".to_string(), pane);
        }

        // Claude's 60s-idle Notification hook must not flip ✅ → 🙋 …
        server
            .notify(
                Some("pane-1".to_string()),
                None,
                Some("attention".to_string()),
                None,
                false,
                "Claude is waiting for your input".to_string(),
                None,
            )
            .unwrap();
        {
            let session = server.session.lock_or_recover();
            let pane = &session.panes["pane-1"];
            assert_eq!(pane.agent_status, AgentStatus::Done);
            assert_eq!(pane.notification_message, None);
        }

        // … but a real permission request still does.
        server
            .notify(
                Some("pane-1".to_string()),
                None,
                Some("attention".to_string()),
                None,
                false,
                "Claude needs your permission to use Bash".to_string(),
                None,
            )
            .unwrap();
        {
            let session = server.session.lock_or_recover();
            let pane = &session.panes["pane-1"];
            assert_eq!(pane.agent_status, AgentStatus::Attention);
        }

        fs::remove_file(server.state_path.clone()).ok();
    }

    #[test]
    fn late_busy_after_done_does_not_resurrect_spinner() {
        // Repro: Grok Stop → ✅, then a late PreToolUse/PostToolUse with
        // "agent working" (no UserPromptSubmit title) was flipping ✅ → 🔄.
        let session = TestSession::new("late-busy");
        let server = Arc::new(Server::load(session.as_str()).unwrap());
        {
            let mut session = server.session.lock_or_recover();
            let mut pane = Pane::new(
                "pane-1".to_string(),
                "grok".to_string(),
                SplitDirection::Right,
            );
            touch_agent_status(&mut pane, AgentStatus::Done, true);
            session.panes.insert("pane-1".to_string(), pane);
        }

        server
            .notify(
                Some("pane-1".to_string()),
                None,
                Some("busy".to_string()),
                Some("yellow".to_string()),
                false,
                "agent working".to_string(),
                None, // no new-turn title
            )
            .unwrap();
        {
            let session = server.session.lock_or_recover();
            assert_eq!(
                session.panes["pane-1"].agent_status,
                AgentStatus::Done,
                "boilerplate busy after Stop must not demote ✅"
            );
        }

        // User dismissed ✅ (click away) → settled Idle; late busy still ignored.
        {
            let mut session = server.session.lock_or_recover();
            assert!(acknowledge_done_status(
                session.panes.get_mut("pane-1").unwrap()
            ));
            assert_eq!(session.panes["pane-1"].agent_status, AgentStatus::Idle);
            assert!(session.panes["pane-1"].agent_status_pinned);
        }
        server
            .notify(
                Some("pane-1".to_string()),
                None,
                Some("busy".to_string()),
                Some("yellow".to_string()),
                false,
                "agent working".to_string(),
                None,
            )
            .unwrap();
        {
            let session = server.session.lock_or_recover();
            assert_eq!(
                session.panes["pane-1"].agent_status,
                AgentStatus::Idle,
                "boilerplate busy after acknowledge must not re-raise 🔄"
            );
        }

        // A real new turn (UserPromptSubmit title) still wins.
        server
            .notify(
                Some("pane-1".to_string()),
                None,
                Some("busy".to_string()),
                Some("yellow".to_string()),
                false,
                "agent working".to_string(),
                Some("fix parser".to_string()),
            )
            .unwrap();
        {
            let session = server.session.lock_or_recover();
            assert_eq!(session.panes["pane-1"].agent_status, AgentStatus::Busy);
        }

        fs::remove_file(server.state_path.clone()).ok();
    }

    #[test]
    fn notify_dedupes_repeated_same_state_notifications() {
        let session = TestSession::new("notify-dedupe");
        let server = Arc::new(Server::load(session.as_str()).unwrap());
        {
            let mut session = server.session.lock_or_recover();
            for id in ["pane-1", "pane-2"] {
                session.panes.insert(
                    id.to_string(),
                    Pane::new(id.to_string(), "claude".to_string(), SplitDirection::Right),
                );
            }
        }
        let notify = |pane: &str, status: &str, color: &str, message: &str| {
            server
                .notify(
                    Some(pane.to_string()),
                    None,
                    Some(status.to_string()),
                    Some(color.to_string()),
                    false,
                    message.to_string(),
                    None,
                )
                .unwrap();
        };

        // Routine busy/done boilerplate must NOT enter the notification feed —
        // the sidebar already shows 🔄/✅. Live status still updates.
        notify("pane-1", "busy", "yellow", "agent working");
        notify("pane-2", "busy", "yellow", "agent working");
        notify("pane-1", "busy", "yellow", "agent working");
        notify("pane-1", "done", "green", "agent hook completed");
        {
            let session = server.session.lock_or_recover();
            assert!(
                session.notifications.is_empty(),
                "boilerplate lifecycle must not spam the feed: {:?}",
                session.notifications
            );
            assert_eq!(session.panes["pane-1"].agent_status, AgentStatus::Done);
            assert_eq!(session.panes["pane-2"].agent_status, AgentStatus::Busy);
        }

        // Attention / permission requests *do* land in the feed (and dedupe).
        notify("pane-1", "attention", "blue", "approve edit to foo.rs");
        notify("pane-1", "attention", "blue", "approve edit to foo.rs");
        notify("pane-1", "attention", "blue", "approve edit to bar.rs");
        {
            let session = server.session.lock_or_recover();
            let messages: Vec<&str> = session
                .notifications
                .iter()
                .filter(|note| note.pane.as_deref() == Some("pane-1"))
                .map(|note| note.message.as_str())
                .collect();
            assert_eq!(
                messages,
                ["approve edit to foo.rs", "approve edit to bar.rs"]
            );
            assert_eq!(session.panes["pane-1"].agent_status, AgentStatus::Attention);
        }

        fs::remove_file(server.state_path.clone()).ok();
    }

    #[test]
    fn jump_notification_switches_to_background_tab() {
        // A notify on a pane living only on a background tab must switch that
        // tab into the live view; setting active_pane alone leaves the pane
        // invisible (workspace.panes is the active tab's layout).
        let session = TestSession::new("jump-bg-tab");
        let server = Arc::new(Server::load(session.as_str()).unwrap());
        {
            let mut session = server.session.lock_or_recover();
            let ws = &mut session.workspaces[0];
            // Active tab-1 holds pane-1; background tab-2 holds pane-2.
            ws.tabs = vec![
                {
                    let mut tab = crate::model::WorkspaceTab::new("tab-1", "front");
                    tab.panes = vec!["pane-1".into()];
                    tab.active_pane = Some("pane-1".into());
                    tab
                },
                {
                    let mut tab = crate::model::WorkspaceTab::new("tab-2", "back");
                    tab.panes = vec!["pane-2".into()];
                    tab.active_pane = Some("pane-2".into());
                    tab
                },
            ];
            ws.active_tab = Some("tab-1".into());
            ws.panes = vec!["pane-1".into()];
            ws.active_pane = Some("pane-1".into());
            for id in ["pane-1", "pane-2"] {
                session.panes.insert(
                    id.to_string(),
                    Pane::new(id.to_string(), "claude".to_string(), SplitDirection::Right),
                );
            }
            session.notifications.push(Notification {
                time: 1,
                pane: Some("pane-2".into()),
                workspace: None,
                status: Some("attention".into()),
                color: Some("blue".into()),
                clear: false,
                message: "needs input on background tab".into(),
            });
        }

        let result = server.jump_notification().unwrap();
        assert_eq!(result["pane"], "pane-2");
        assert_eq!(result["tab"], "tab-2");
        {
            let session = server.session.lock_or_recover();
            let ws = &session.workspaces[0];
            assert_eq!(ws.active_tab.as_deref(), Some("tab-2"));
            assert_eq!(ws.active_pane.as_deref(), Some("pane-2"));
            assert_eq!(ws.panes, vec!["pane-2".to_string()]);
        }

        fs::remove_file(server.state_path.clone()).ok();
    }

    #[test]
    fn focus_pane_switches_workspace_and_tab() {
        let session = TestSession::new("focus-tab");
        let server = Arc::new(Server::load(session.as_str()).unwrap());
        {
            let mut session = server.session.lock_or_recover();
            // Second workspace with a single tab holding pane-9.
            let mut ws2 = crate::model::Workspace::new("ws-2", "other");
            ws2.tabs[0].panes = vec!["pane-9".into()];
            ws2.tabs[0].active_pane = Some("pane-9".into());
            ws2.panes = vec!["pane-9".into()];
            ws2.active_pane = Some("pane-9".into());
            session.workspaces.push(ws2);
            session.panes.insert(
                "pane-9".into(),
                Pane::new("pane-9".into(), "claude".into(), SplitDirection::Right),
            );
            // Active workspace stays ws-1.
            session.active_workspace = "ws-1".into();
        }

        // Drive through the same Request path the UI uses.
        let response = server
            .dispatch(Request::FocusPane {
                pane: "pane-9".into(),
            })
            .unwrap();
        assert!(response.ok, "{response:?}");
        {
            let session = server.session.lock_or_recover();
            assert_eq!(session.active_workspace, "ws-2");
            let ws = session
                .workspaces
                .iter()
                .find(|w| w.id == "ws-2")
                .expect("ws-2");
            assert_eq!(ws.active_pane.as_deref(), Some("pane-9"));
        }

        fs::remove_file(server.state_path.clone()).ok();
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
    fn reap_orphan_panes_drops_panes_no_tab_references() {
        let _guard = TestSession::new("orphans");
        let session_name = _guard.as_str().to_string();
        let state_path = paths::state_path(&session_name).unwrap();
        let cwd = std::env::current_dir().unwrap();
        let mut session = Session::new(&session_name);
        session.workspaces[0].cwd = cwd.display().to_string();
        session.workspaces[0].panes = vec!["pane-1".to_string()];
        session.workspaces[0].active_pane = Some("pane-1".to_string());
        session.workspaces[0].layout = Some(crate::model::LayoutNode::Pane {
            pane: "pane-1".to_string(),
        });
        let pane = Pane::new("pane-1".to_string(), String::new(), SplitDirection::Right);
        session.panes.insert("pane-1".to_string(), pane);
        // The zombie: saved in the map, referenced by no workspace or tab.
        let orphan = Pane::new("pane-9".to_string(), String::new(), SplitDirection::Right);
        session.panes.insert("pane-9".to_string(), orphan);
        fs::write(&state_path, serde_json::to_vec_pretty(&session).unwrap()).unwrap();

        let server = Arc::new(Server::load(&session_name).unwrap());
        server.reap_orphan_panes().unwrap();

        let loaded = server.session.lock_or_recover();
        assert!(
            loaded.panes.contains_key("pane-1"),
            "referenced pane must stay"
        );
        assert!(
            !loaded.panes.contains_key("pane-9"),
            "orphan pane must be reaped — it is invisible but pollutes the status counts"
        );
    }

    #[test]
    fn new_tab_pane_does_not_freeze_the_tab_title() {
        let _guard = TestSession::new("tabtitle");
        let server = Arc::new(Server::load(_guard.as_str()).unwrap());
        let data = server.new_tab(None, None, None).unwrap();
        let pane_id = data
            .get("pane")
            .and_then(|p| p.get("id"))
            .and_then(|v| v.as_str())
            .expect("new tab should open a pane")
            .to_string();
        let session = server.session.lock_or_recover();
        let title = &session.panes.get(&pane_id).unwrap().title;
        assert_ne!(
            title, "tab",
            "a tab's first pane must auto-title from its process, not freeze the default tab title"
        );
    }

    #[test]
    fn load_marks_saved_panes_restored_and_relaunches_them() {
        let _guard = TestSession::new("restore");
        let session_name = _guard.as_str().to_string();
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
        // The relaunched pane must be a *live* process, not the stale saved pid.
        assert_ne!(pane.pid, Some(999_999));
        assert!(pane.pid.is_some(), "relaunched pane must have a real pid");
        assert_eq!(pane.output, "old output");
        assert_eq!(pane.scrollback, "old scrollback");

        // `printf` exits immediately. Wait for the reaper rather than accepting
        // `Running | Exited`: that OR made the assertion unfalsifiable, and the
        // reader thread's `mark_exited` calls `save()`, so ending the test while
        // it is still in flight re-creates the state file after cleanup.
        let exited = (0..200).any(|_| {
            let done = matches!(
                server.snapshot(false).unwrap().panes["pane-1"].status,
                PaneStatus::Exited
            );
            if !done {
                thread::sleep(Duration::from_millis(10));
            }
            done
        });
        assert!(
            exited,
            "relaunched pane should be reaped after printf exits"
        );

        if let Some(mut runtime) = server.panes.lock_or_recover().remove("pane-1") {
            if let Some(mut killer) = runtime.killer.take() {
                killer.kill().ok();
            }
        }
        server.cleanup_runtime_files();
        let _ = state_path;
    }

    /// A successor daemon must be able to take the session lock as soon as the
    /// outgoing daemon has said "shutting down", not only after its process
    /// exits. `Shutdown` unlinks the socket immediately (so `is_running` goes
    /// false) but the process lives another ~50ms to flush the response — and
    /// the flock used to live with it, so a restart issued in that window
    /// failed with "vmux daemon helper exited with exit status: 0".
    #[test]
    fn shutdown_releases_the_session_lock_for_a_successor() {
        let session = TestSession::new("lock-release");
        let server = Server::load(session.as_str()).unwrap();

        // While the daemon holds the lock, a successor must be refused.
        assert!(
            paths::try_lock_session(session.as_str()).is_err(),
            "second lock while daemon holds it should fail"
        );

        // What Shutdown does, minus the process::exit.
        server.cleanup_runtime_files();
        server.release_session_lock();

        assert!(
            paths::try_lock_session(session.as_str()).is_ok(),
            "successor must acquire the lock as soon as shutdown released it"
        );
    }

    #[test]
    fn effective_pane_size_is_min_per_axis() {
        let layout = PaneSize { cols: 80, rows: 24 };
        // Phone narrower but taller than the layout: each axis clamps alone.
        let out = effective_pane_size(layout, Some(PaneSize { cols: 46, rows: 60 }));
        assert_eq!(out, PaneSize { cols: 46, rows: 24 });
        // No override → layout untouched.
        assert_eq!(effective_pane_size(layout, None), layout);
        // Degenerate override still yields a usable grid.
        let out = effective_pane_size(layout, Some(PaneSize { cols: 0, rows: 0 }));
        assert_eq!(out, PaneSize { cols: 2, rows: 2 });
    }

    /// The full phone-fit lifecycle against one pane: set clamps the PTY and
    /// bumps the generation, layout resizes keep respecting the override,
    /// persistence never records it, the lease expiring restores the layout
    /// size, and a zoomed pane refuses the override outright.
    #[test]
    fn view_override_clamps_leases_and_never_persists() {
        let guard = TestSession::new("view-size");
        let server = Server::load(guard.as_str()).unwrap();
        {
            let mut session = server.session.lock_or_recover();
            let mut pane = Pane::new("pane-1".into(), "sh".into(), SplitDirection::Right);
            pane.status = PaneStatus::Running;
            session.workspaces[0].panes = vec!["pane-1".into()];
            session.panes.insert("pane-1".into(), pane.clone());
            drop(session);
            let mut panes = server.panes.lock_or_recover();
            panes.insert(
                "pane-1".into(),
                PaneRuntime {
                    generation: 1,
                    pane,
                    master: None,
                    writer: None,
                    killer: None,
                    output: std::collections::VecDeque::new(),
                    output_bytes: 0,
                    scrollback_cap: crate::config::default_scrollback_bytes(),
                    pending: Vec::new(),
                    osc_tail: String::new(),
                    terminal_modes: TerminalModeTracker::default(),
                    parser: vt100::Parser::new(24, 80, 1000),
                    size: PaneSize { cols: 80, rows: 24 },
                    layout_size: PaneSize { cols: 80, rows: 24 },
                    view_override: None,
                    output_generation: 1,
                    scrollback_formatted_cache: String::new(),
                    scrollback_formatted_generation: u64::MAX,
                    auto_title: None,
                    llm_title_state: LlmTitleState::Pending,
                    agent_inside: false,
                    agent_inside_at: 0,
                    started_at: unix_time(),
                },
            );
        }

        // Set: PTY clamps to min(layout, view) per axis, snapshot advertises
        // the override, and the generation moves so the UI repaints.
        let before = server.generation();
        server
            .set_pane_view_size(
                Some("pane-1".into()),
                PaneSize { cols: 46, rows: 60 },
                10_000,
            )
            .unwrap();
        assert!(server.generation() > before, "set must bump the generation");
        {
            let panes = server.panes.lock_or_recover();
            assert_eq!(panes["pane-1"].size, PaneSize { cols: 46, rows: 24 });
        }
        let snap = server.snapshot(false).unwrap();
        let view = snap.panes["pane-1"]
            .view_size
            .expect("snapshot carries view_size");
        assert_eq!((view.cols, view.rows), (46, 60));

        // A layout resize while the override is live keeps the clamp.
        let mut sizes = BTreeMap::new();
        sizes.insert(
            "pane-1".to_string(),
            PaneSize {
                cols: 120,
                rows: 30,
            },
        );
        server.resize_ptys(sizes, None, false).unwrap();
        {
            let panes = server.panes.lock_or_recover();
            assert_eq!(panes["pane-1"].size, PaneSize { cols: 46, rows: 30 });
            assert_eq!(
                panes["pane-1"].layout_size,
                PaneSize {
                    cols: 120,
                    rows: 30
                }
            );
        }

        // Persistence must not record the override: a restart with no phone
        // attached must come back desktop-sized.
        server.save().unwrap();
        let on_disk: Session =
            serde_json::from_str(&fs::read_to_string(&server.state_path).unwrap()).unwrap();
        assert!(
            on_disk.panes["pane-1"].view_size.is_none(),
            "view_size leaked into the state file"
        );

        // Not expired yet: a sweep now is a no-op.
        assert!(!server.expire_view_overrides(Instant::now()));
        // Lease runs out: the pane returns to its layout size on its own.
        assert!(server.expire_view_overrides(Instant::now() + Duration::from_secs(60)));
        {
            let panes = server.panes.lock_or_recover();
            assert_eq!(
                panes["pane-1"].size,
                PaneSize {
                    cols: 120,
                    rows: 30
                }
            );
            assert!(panes["pane-1"].view_override.is_none());
        }
        let snap = server.snapshot(false).unwrap();
        assert!(snap.panes["pane-1"].view_size.is_none());

        // A zoomed pane refuses the override: the user zoomed for a full-area
        // view; a phone glance must not shrink it under them.
        server.session.lock_or_recover().workspaces[0].zoomed_pane = Some("pane-1".into());
        let err = server
            .set_pane_view_size(
                Some("pane-1".into()),
                PaneSize { cols: 46, rows: 22 },
                5_000,
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("zoomed"),
            "unexpected error: {err}"
        );
    }

    /// The generation counter is what makes the UI repaint: clients poll with
    /// `Snapshot { since }` and the daemon short-circuits to `unchanged` when the
    /// counter has not moved. A mutation that forgets to bump it is invisible —
    /// the screen simply never updates — which makes this the cheapest possible
    /// bug to introduce and the hardest to notice. Nothing guarded it before.
    #[test]
    fn mutating_requests_bump_the_generation_counter() {
        let session = TestSession::new("generation");
        let server = Arc::new(Server::load(session.as_str()).unwrap());

        let mutations: Vec<(&str, Request)> = vec![
            (
                "new-workspace",
                Request::NewWorkspace {
                    name: "gen-ws".into(),
                    cwd: None,
                },
            ),
            (
                "new-tab",
                Request::NewTab {
                    workspace: None,
                    title: Some("gen-tab".into()),
                    command: None,
                },
            ),
            (
                "rename-workspace",
                Request::RenameWorkspace {
                    workspace: "gen-ws".into(),
                    name: "gen-ws-renamed".into(),
                },
            ),
        ];

        for (label, request) in mutations {
            let before = server.generation();
            server.dispatch(request).unwrap();
            let after = server.generation();
            assert!(
                after > before,
                "{label} mutated state but did not bump the generation \
                 ({before} -> {after}); the UI would never repaint"
            );
        }

        // The converse: a read must NOT bump it, or `since` never short-circuits
        // and every client re-fetches the whole session on every poll.
        let before = server.generation();
        server.dispatch(Request::Agents).unwrap();
        assert_eq!(
            server.generation(),
            before,
            "a read-only request must not bump the generation"
        );
    }

    #[test]
    fn activating_base_tab_migrates_legacy_runtime_without_orphaning() {
        // Regression: a pane created before it had tabs keeps its
        // runtime under the bare `pane-N` key. Adding a tab and switching back to
        // the base tab must reuse that runtime (re-keyed to `pane-N::tab-1`)
        // rather than orphaning it and spawning a duplicate shell.
        let _guard = TestSession::new("tabkey");
        let session_name = _guard.as_str().to_string();
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

    /// `/proc` is Linux-only, so this asserted `Some(cwd)` on a platform where
    /// `pane_live_cwd` can only ever return `None`. It failed on every macOS
    /// run — which nobody saw, because the workflow was invalid and CI had never
    /// executed a single job.
    #[test]
    #[cfg(target_os = "linux")]
    fn pane_live_cwd_reads_proc_cwd() {
        let expected = std::env::current_dir().unwrap().display().to_string();
        assert_eq!(pane_live_cwd(std::process::id()), Some(expected));
        assert_eq!(pane_live_cwd(u32::MAX), None);
    }

    /// The macOS half of the same contract: no `/proc`, so no live cwd. Pins the
    /// graceful degradation rather than leaving the platform untested.
    #[test]
    #[cfg(not(target_os = "linux"))]
    fn pane_live_cwd_is_unavailable_without_proc() {
        assert_eq!(pane_live_cwd(std::process::id()), None);
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
        let session = TestSession::new("size-owner");
        let server = Server::load(session.as_str()).unwrap();
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
    fn osc_progress_is_not_a_notification() {
        // ConEmu / Windows Terminal / Ghostty progress: OSC 9;4;state;pct
        // Used to be misparsed as notification message "4: 1" / "4: 0" and
        // flipped the pane to 🙋 attention.
        let set = "\x1b]9;4;1;50\x07";
        let clear = "\x1b]9;4;0\x07";
        let err = "\x1b]9;4;2\x07";
        assert!(
            osc_notifications(set).is_empty(),
            "progress set must not become a notification"
        );
        assert!(osc_notifications(clear).is_empty());
        assert!(osc_notifications(err).is_empty());

        let (events, _) = scan_osc_events(set);
        assert_eq!(events.progress, Some(Some(50)));
        let (events, _) = scan_osc_events(clear);
        assert_eq!(events.progress, Some(None));
        // Real notifications still work next to progress.
        let mixed = "\x1b]9;4;1;10\x07\x1b]9;needs review\x07";
        let (events, _) = scan_osc_events(mixed);
        assert_eq!(events.progress, Some(Some(10)));
        assert_eq!(events.notifications, vec!["needs review".to_string()]);
    }

    #[test]
    fn turn_complete_notification_does_not_raise_attention() {
        let session = TestSession::new("turn-complete");
        let server = Arc::new(Server::load(session.as_str()).unwrap());
        {
            let mut session = server.session.lock_or_recover();
            let mut pane = Pane::new(
                "pane-1".to_string(),
                "grok".to_string(),
                SplitDirection::Right,
            );
            touch_agent_status(&mut pane, AgentStatus::Busy, true);
            session.panes.insert("pane-1".to_string(), pane);
        }
        // Grok's Notification hook fires "Turn complete" as attention — that
        // must become done, not 🙋, and must not spam the feed.
        server
            .notify(
                Some("pane-1".to_string()),
                None,
                Some("attention".to_string()),
                Some("blue".to_string()),
                false,
                "Turn complete".to_string(),
                None,
            )
            .unwrap();
        {
            let session = server.session.lock_or_recover();
            assert_eq!(session.panes["pane-1"].agent_status, AgentStatus::Done);
            assert!(
                session.notifications.is_empty(),
                "turn-complete noise must not land in the feed"
            );
        }
        fs::remove_file(server.state_path.clone()).ok();
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
        // Numeric-only OSC 9 payloads are never user notifications.
        assert!(osc_notifications("\x1b]9;4\x07").is_empty());
        assert!(osc_notifications("\x1b]9;4;1\x07").is_empty());
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
    fn osc_title_reads_window_title_sequences() {
        // OSC 2 (title) and OSC 0 (icon + title), both terminators.
        assert_eq!(
            osc_title("\x1b]2;✳ Fixing the parser bug\x07"),
            Some("✳ Fixing the parser bug".to_string())
        );
        assert_eq!(
            osc_title("\x1b]0;reviewing auth\x1b\\"),
            Some("reviewing auth".to_string())
        );
        // The last title in a read wins: agents rewrite it as they work.
        assert_eq!(
            osc_title("\x1b]2;first\x07output\x1b]2;second\x07"),
            Some("second".to_string())
        );
        // OSC 1 sets the icon name only, and a notification is not a title.
        assert_eq!(osc_title("\x1b]1;icon\x07"), None);
        assert_eq!(osc_title("\x1b]9;needs input\x07"), None);
        assert_eq!(osc_title("plain output"), None);
    }

    #[test]
    fn osc_title_and_notification_survive_the_same_read() {
        let (events, _) = scan_osc_events("\x1b]2;fixing parser\x07\x1b]9;needs input\x07");
        assert_eq!(events.title.as_deref(), Some("fixing parser"));
        assert_eq!(events.notifications, vec!["needs input".to_string()]);
    }

    #[test]
    fn llm_tab_title_condenses_the_summarizer_answer() {
        // `cat` stands in for the summarizer: it echoes the prompt back, whose
        // last line is the screen tail — proving the answer is condensed, and
        // that a summarizer printing prose cannot blow out the tab strip.
        let title = llm_tab_title("cat", "agent is Refactoring the OSC parser").unwrap();
        assert_eq!(title.as_deref(), Some("refactoring osc"));
        // A summarizer that fails or is missing leaves the tab title alone.
        assert_eq!(llm_tab_title("false", "screen").unwrap(), None);
        assert!(llm_tab_title("vmux-no-such-summarizer-binary", "screen").is_err());
    }

    #[test]
    fn scan_osc_notifications_detects_sequences_split_across_reads() {
        let mut tail = String::new();
        tail.push_str("before\x1b]9;needs ");
        let (events, retain_from) = scan_osc_events(&tail);
        assert!(events.notifications.is_empty());
        // Retain from the unterminated OSC start.
        assert_eq!(&tail[retain_from..], "\x1b]9;needs ");
        tail.drain(..retain_from);
        tail.push_str("input\x07done");
        let (events, retain_from) = scan_osc_events(&tail);
        assert_eq!(events.notifications, vec!["needs input".to_string()]);
        assert_eq!(retain_from, tail.len());
    }

    #[test]
    fn push_output_bounds_scrollback_by_bytes() {
        // An explicit cap, not the default: this asserts the budget is honoured,
        // whatever `ui.scrollback_bytes` happens to be set to.
        const CAP: usize = 16_000;
        let mut output: VecDeque<String> = VecDeque::new();
        let mut output_bytes = 0;
        for _ in 0..1000 {
            push_bounded_output(&mut output, &mut output_bytes, "x".repeat(1000), CAP);
        }
        assert!(output_bytes <= CAP);
        let joined: String = output.iter().cloned().collect();
        assert_eq!(joined.len(), output_bytes);
        assert!(joined.len() <= CAP);
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
        let _guard = TestSession::new("corrupt");
        let session_name = _guard.as_str().to_string();
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

    #[test]
    fn push_event_assigns_monotonic_ids() {
        let mut session = Session::new("evt");
        push_event(
            &mut session,
            EventRecord {
                id: 0,
                time: 1,
                kind: "a".into(),
                pane: None,
                workspace: None,
                status: None,
                key: None,
                value: None,
                message: String::new(),
            },
        );
        push_event(
            &mut session,
            EventRecord {
                id: 0,
                time: 2,
                kind: "b".into(),
                pane: None,
                workspace: None,
                status: None,
                key: None,
                value: None,
                message: String::new(),
            },
        );
        assert_eq!(session.events[0].id, 1);
        assert_eq!(session.events[1].id, 2);
        assert_eq!(session.next_event_id, 2);
    }

    #[test]
    fn full_snapshot_materializes_runtime_output_for_persist() {
        // Light path clears heavy strings; persist/full path must refill them.
        let _guard = TestSession::new("persist");
        let session_name = _guard.as_str().to_string();
        let server = Server::load(&session_name).unwrap();
        {
            let mut panes = server.panes.lock_or_recover();
            let mut runtime = PaneRuntime {
                generation: 1,
                pane: Pane::new("pane-1".into(), "echo".into(), SplitDirection::Right),
                master: None,
                writer: None,
                killer: None,
                output: std::collections::VecDeque::new(),
                output_bytes: 0,
                scrollback_cap: crate::config::default_scrollback_bytes(),
                pending: Vec::new(),
                osc_tail: String::new(),
                terminal_modes: TerminalModeTracker::default(),
                parser: vt100::Parser::new(24, 80, 1000),
                size: PaneSize { cols: 80, rows: 24 },
                layout_size: PaneSize { cols: 80, rows: 24 },
                view_override: None,
                output_generation: 1,
                scrollback_formatted_cache: String::new(),
                scrollback_formatted_generation: u64::MAX,
                auto_title: None,
                llm_title_state: LlmTitleState::Pending,
                agent_inside: false,
                agent_inside_at: 0,
                started_at: unix_time(),
            };
            runtime.parser.process(b"hello-persist-marker\r\n");
            runtime.push_output("hello-persist-marker\n".into());
            runtime.pane.output.clear();
            runtime.pane.scrollback.clear();
            panes.insert("pane-1".into(), runtime);
        }
        {
            let mut session = server.session.lock_or_recover();
            session.panes.insert(
                "pane-1".into(),
                Pane::new("pane-1".into(), "echo".into(), SplitDirection::Right),
            );
            if let Some(ws) = session.workspaces.first_mut() {
                ws.panes = vec!["pane-1".into()];
                ws.active_pane = Some("pane-1".into());
                ws.ensure_layout();
            }
        }
        let light = server.snapshot(false).unwrap();
        // Light snapshot may still copy empty stored strings.
        let full = server.snapshot(true).unwrap();
        let pane = full.panes.get("pane-1").expect("pane present");
        assert!(
            pane.output.contains("hello-persist-marker")
                || pane.scrollback.contains("hello-persist-marker"),
            "full snapshot must materialize output; got out={:?} sb={:?}",
            pane.output,
            pane.scrollback
        );
        let _ = light;
        // cleanup lock/state
        drop(server);
        let _ = fs::remove_file(paths::state_path(&session_name).unwrap());
        let _ = fs::remove_file(paths::lock_path(&session_name).unwrap());
    }

    /// End-to-end: live runtime output → save → drop server → reload → history intact.
    #[test]
    fn e2e_restart_preserves_scrollback_across_save_reload() {
        let _guard = TestSession::new("e2e-restart");
        let session_name = _guard.as_str().to_string();
        let state_path = paths::state_path(&session_name).unwrap();
        let lock_path = paths::lock_path(&session_name).unwrap();
        let marker = "vmux-e2e-restart-marker-42";

        // Phase 1: live daemon with output only in the runtime deque.
        {
            let server = Server::load(&session_name).unwrap();
            {
                let mut session = server.session.lock_or_recover();
                let mut pane =
                    Pane::new("pane-1".into(), "printf e2e".into(), SplitDirection::Right);
                pane.status = PaneStatus::Running;
                session.panes.insert("pane-1".into(), pane);
                if let Some(ws) = session.workspaces.first_mut() {
                    ws.cwd = std::env::current_dir()
                        .unwrap_or_else(|_| PathBuf::from("."))
                        .display()
                        .to_string();
                    ws.panes = vec!["pane-1".into()];
                    ws.active_pane = Some("pane-1".into());
                    ws.ensure_layout();
                }
            }
            {
                let mut panes = server.panes.lock_or_recover();
                let mut runtime = PaneRuntime {
                    generation: 1,
                    pane: Pane::new("pane-1".into(), "printf e2e".into(), SplitDirection::Right),
                    master: None,
                    writer: None,
                    killer: None,
                    output: VecDeque::new(),
                    output_bytes: 0,
                    scrollback_cap: crate::config::default_scrollback_bytes(),
                    pending: Vec::new(),
                    osc_tail: String::new(),
                    terminal_modes: TerminalModeTracker::default(),
                    parser: vt100::Parser::new(24, 80, 1000),
                    size: PaneSize { cols: 80, rows: 24 },
                    layout_size: PaneSize { cols: 80, rows: 24 },
                    view_override: None,
                    output_generation: 1,
                    scrollback_formatted_cache: String::new(),
                    scrollback_formatted_generation: u64::MAX,
                    auto_title: None,
                    llm_title_state: LlmTitleState::Pending,
                    agent_inside: false,
                    agent_inside_at: 0,
                    started_at: unix_time(),
                };
                runtime.pane.status = PaneStatus::Running;
                runtime.push_output(format!("{marker}\n"));
                runtime.parser.process(format!("{marker}\r\n").as_bytes());
                runtime.pane.output.clear();
                runtime.pane.scrollback.clear();
                panes.insert("pane-1".into(), runtime);
            }
            server.save().unwrap();
        }

        assert!(state_path.exists(), "state file should exist after save");
        let on_disk: Session =
            serde_json::from_str(&fs::read_to_string(&state_path).unwrap()).unwrap();
        let disk_pane = on_disk.panes.get("pane-1").expect("pane on disk");
        // Assert both fields independently, not `output || scrollback`. The P0
        // this test guards was scrollback being dropped from the state file
        // *while `output` survived*, so an OR here cannot see its own regression.
        assert!(
            disk_pane.scrollback.contains(marker),
            "state file must persist scrollback; sb={:?}",
            disk_pane.scrollback
        );
        assert!(
            disk_pane.output.contains(marker),
            "state file must persist screen output; out={:?}",
            disk_pane.output
        );

        // Phase 2: cold start from disk (lock re-acquired after drop).
        {
            // Concurrent tests that spawn PTY children can briefly inherit
            // this session's flock fd (flock lives until every duplicated fd
            // closes), so the re-acquire may need a few retries under a
            // parallel test run.
            let server = (0..50)
                .find_map(|attempt| {
                    if attempt > 0 {
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                    Server::load(&session_name).ok()
                })
                .expect("re-acquire session lock after drop");
            let loaded = server.session.lock_or_recover();
            let pane = loaded.panes.get("pane-1").expect("pane after reload");
            assert!(
                pane.scrollback.contains(marker),
                "reload must keep scrollback; sb={:?}",
                pane.scrollback
            );
            assert!(
                pane.output.contains(marker),
                "reload must keep screen output; out={:?}",
                pane.output
            );
            // A pane loaded from disk has not been relaunched yet.
            assert_eq!(pane.status, PaneStatus::Restored);
        }

        fs::remove_file(state_path).ok();
        fs::remove_file(lock_path).ok();
        fs::remove_file(paths::pid_path(&session_name).unwrap()).ok();
        fs::remove_file(paths::socket_path(&session_name).unwrap()).ok();
    }
}
