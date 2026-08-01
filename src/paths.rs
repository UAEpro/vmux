use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub fn runtime_dir() -> Result<PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/tmp/vmux-{}", unsafe { libc_getuid() })))
        .join("vmux");
    ensure_private_dir(&dir)?;
    Ok(dir)
}

/// Create `dir` mode 0700 if missing; if it exists, require a real directory
/// owned by the current uid with no group/other access (tmux-style).
fn ensure_private_dir(dir: &Path) -> Result<()> {
    if !dir.exists() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("create private dir {}", dir.display()))?;
    }
    let meta =
        fs::symlink_metadata(dir).with_context(|| format!("stat runtime dir {}", dir.display()))?;
    if meta.file_type().is_symlink() {
        bail!(
            "refusing to use runtime dir {}: path is a symlink",
            dir.display()
        );
    }
    if !meta.is_dir() {
        bail!(
            "refusing to use runtime dir {}: not a directory",
            dir.display()
        );
    }
    let uid = unsafe { libc_getuid() };
    if meta.uid() != uid {
        bail!(
            "refusing to use runtime dir {}: owned by uid {}, expected {}",
            dir.display(),
            meta.uid(),
            uid
        );
    }
    let mode = meta.mode() & 0o777;
    if mode & 0o077 != 0 {
        // Tighten in place when safe (we own it).
        let mut perms = meta.permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o700);
        fs::set_permissions(dir, perms).with_context(|| format!("chmod 0700 {}", dir.display()))?;
    }
    Ok(())
}

/// Validate a session name before it is turned into a filesystem path.
///
/// A session name becomes the stem of socket/pid/log/state files, so a name
/// containing a path separator or `..` could escape the intended runtime/state
/// directories (e.g. `../../etc/foo`). Reject such names outright rather than
/// silently rewriting them, so callers never end up with aliased sessions.
pub fn validate_session_name(session: &str) -> Result<()> {
    if session.is_empty() {
        anyhow::bail!("invalid session name: must not be empty");
    }
    if session.contains('/')
        || session.contains('\\')
        || session.contains("..")
        || session.contains('\0')
    {
        anyhow::bail!(
            "invalid session name {session:?}: must not contain '/', '\\', '..', or NUL bytes"
        );
    }
    if RESERVED_STATE_STEMS.contains(&session) {
        anyhow::bail!(
            "invalid session name {session:?}: reserved for vmux's own state file of that name"
        );
    }
    Ok(())
}

/// File stems the daemon writes into the state dir that are *not* sessions.
///
/// These share the state dir with `<session>.json`, so they matter twice over.
/// `list_sessions` enumerates `*.json` there and filters through
/// `validate_session_name`, so without this list they surface as phantom
/// sessions in `vmux sessions` — `update-check` appears on every install once
/// the daily check has run. And a session actually *named* one of these would
/// have its state file collide with the real one, letting a session clobber the
/// relay's device store (which holds auth token hashes).
const RESERVED_STATE_STEMS: &[&str] = &["update-check", "relay-devices"];

pub fn socket_path(session: &str) -> Result<PathBuf> {
    validate_session_name(session)?;
    Ok(runtime_dir()?.join(format!("{session}.sock")))
}

pub fn pid_path(session: &str) -> Result<PathBuf> {
    validate_session_name(session)?;
    Ok(runtime_dir()?.join(format!("{session}.pid")))
}

pub fn log_path(session: &str) -> Result<PathBuf> {
    validate_session_name(session)?;
    Ok(runtime_dir()?.join(format!("{session}.log")))
}

/// Where `vmux relay serve` records that a relay is wanted for this session
/// (listen address, config path, pid). The daemon reads it to bring the relay
/// back after a restart, and stops it on shutdown.
///
/// Lives in the runtime dir, not the state dir: it is per-boot, and
/// `list_sessions` only enumerates `*.json` under the state dir, so this
/// cannot masquerade as a session.
pub fn relay_autostart_path(session: &str) -> Result<PathBuf> {
    validate_session_name(session)?;
    Ok(runtime_dir()?.join(format!("{session}.relay.json")))
}

/// The relay's log when the daemon (re)starts it unattended.
pub fn relay_log_path(session: &str) -> Result<PathBuf> {
    validate_session_name(session)?;
    Ok(runtime_dir()?.join(format!("{session}.relay.log")))
}

pub fn state_dir() -> Result<PathBuf> {
    let dir = dirs::state_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")))
        .join("vmux");
    ensure_private_dir(&dir)?;
    Ok(dir)
}

pub fn state_path(session: &str) -> Result<PathBuf> {
    validate_session_name(session)?;
    Ok(state_dir()?.join(format!("{session}.json")))
}

/// Cache for the background update check. Not session-scoped — the running
/// version is global, so all sessions share one cache.
pub fn update_cache_path() -> Result<PathBuf> {
    Ok(state_dir()?.join("update-check.json"))
}

pub fn config_dir() -> Result<PathBuf> {
    let dir = dirs::config_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")))
        .join("vmux");
    ensure_private_dir(&dir)?;
    Ok(dir)
}

pub fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.json"))
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionArtifact {
    pub name: String,
    pub running: bool,
    pub socket_path: String,
    pub pid_path: String,
    pub log_path: String,
    pub state_path: String,
    pub pid: Option<u32>,
}

pub fn list_sessions() -> Result<Vec<SessionArtifact>> {
    let runtime = runtime_dir()?;
    let state = state_dir()?;
    let mut names = BTreeSet::new();
    collect_session_names(&runtime, "sock", &mut names)?;
    collect_session_names(&runtime, "pid", &mut names)?;
    collect_session_names(&state, "json", &mut names)?;

    let mut sessions = Vec::new();
    for name in names {
        let socket = socket_path(&name)?;
        let pid = pid_path(&name)?;
        let log = log_path(&name)?;
        let state = state_path(&name)?;
        let pid_value = read_pid_file(&pid);
        sessions.push(SessionArtifact {
            name: name.clone(),
            running: socket.exists() && pid_value.map(process_exists).unwrap_or(false),
            socket_path: socket.display().to_string(),
            pid_path: pid.display().to_string(),
            log_path: log.display().to_string(),
            state_path: state.display().to_string(),
            pid: pid_value,
        });
    }
    Ok(sessions)
}

pub fn read_pid_file(path: &Path) -> Option<u32> {
    read_pid_record(path).map(|r| r.pid)
}

/// PID file record: `pid` on line 1, optional platform process start time on
/// line 2. Start time prevents signalling a recycled PID after a crash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PidRecord {
    pub pid: u32,
    pub starttime: Option<u64>,
}

pub fn read_pid_record(path: &Path) -> Option<PidRecord> {
    let raw = fs::read_to_string(path).ok()?;
    let mut lines = raw.lines().filter(|l| !l.trim().is_empty());
    let pid = lines.next()?.trim().parse().ok()?;
    let starttime = lines.next().and_then(|l| l.trim().parse().ok());
    Some(PidRecord { pid, starttime })
}

pub fn write_pid_record(path: &Path, pid: u32) -> Result<()> {
    let starttime = process_starttime(pid).unwrap_or(0);
    fs::write(path, format!("{pid}\n{starttime}\n"))
        .with_context(|| format!("write pid file {}", path.display()))
}

pub fn process_exists(pid: u32) -> bool {
    // `/proc` is not mounted on macOS.  `kill(pid, 0)` is the portable Unix
    // liveness probe: it sends no signal and succeeds when the process exists
    // and is visible to us.  EPERM also means the process exists, just that we
    // are not allowed to signal it.
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    let rc = unsafe { libc::kill(pid as i32, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Linux `/proc/<pid>/stat` field 22 (starttime in clock ticks).
#[cfg(target_os = "linux")]
pub fn process_starttime(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm can contain spaces/parens: parse after the last `) `.
    let after_comm = stat.rsplit_once(") ").map(|(_, rest)| rest)?;
    let field = after_comm.split_whitespace().nth(19)?; // 22nd field overall → index 19 after comm
    field.parse().ok()
}

/// macOS process start time, in microseconds since the Unix epoch.
///
/// Keeping a start time in the pid record prevents a recycled pid from being
/// signalled.  `proc_pidinfo` provides the same identity check on macOS that
/// `/proc/<pid>/stat` provides on Linux.
#[cfg(target_os = "macos")]
pub fn process_starttime(pid: u32) -> Option<u64> {
    if pid == 0 || pid > i32::MAX as u32 {
        return None;
    }
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let expected = std::mem::size_of::<libc::proc_bsdinfo>();
    let read = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            expected as i32,
        )
    };
    if read != expected as i32 {
        return None;
    }
    let info = unsafe { info.assume_init() };
    info.pbi_start_tvsec
        .checked_mul(1_000_000)?
        .checked_add(info.pbi_start_tvusec)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn process_starttime(_pid: u32) -> Option<u64> {
    None
}

/// True when `pid` is alive and matches the recorded starttime (if present).
pub fn process_matches_record(record: PidRecord) -> bool {
    if !process_exists(record.pid) {
        return false;
    }
    match record.starttime {
        None | Some(0) => true, // legacy pid files without starttime
        Some(expected) => process_starttime(record.pid) == Some(expected),
    }
}

/// Best-effort: does a process argument look like a vmux daemon/relay?
#[cfg(target_os = "linux")]
pub fn process_cmdline_contains(pid: u32, needle: &str) -> bool {
    fs::read(format!("/proc/{pid}/cmdline"))
        .ok()
        .map(|bytes| {
            let text = String::from_utf8_lossy(&bytes);
            text.contains(needle)
        })
        .unwrap_or(false)
}

/// Read argv through macOS's native `KERN_PROCARGS2` interface.
///
/// Only the executable path and the declared argv entries are searched.  The
/// buffer also contains the process environment; searching the whole buffer
/// could mistake an unrelated process carrying a `VMUX_*` variable for vmux
/// and make the stop path signal the wrong pid.
#[cfg(target_os = "macos")]
pub fn process_cmdline_contains(pid: u32, needle: &str) -> bool {
    if pid == 0 || pid > i32::MAX as u32 || needle.is_empty() {
        return false;
    }
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as i32];
    let mut size = 0usize;
    let size_rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if size_rc != 0 || size < std::mem::size_of::<i32>() {
        return false;
    }
    let mut bytes = vec![0u8; size];
    let read_rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            bytes.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if read_rc != 0 || size < std::mem::size_of::<i32>() {
        return false;
    }
    bytes.truncate(size);

    let argc = i32::from_ne_bytes(bytes[..4].try_into().unwrap_or([0; 4]));
    if argc <= 0 {
        return false;
    }
    let mut offset = 4usize;
    let Some(executable_end) = bytes[offset..].iter().position(|byte| *byte == 0) else {
        return false;
    };
    if String::from_utf8_lossy(&bytes[offset..offset + executable_end]).contains(needle) {
        return true;
    }
    offset += executable_end + 1;

    // KERN_PROCARGS2 pads with NUL bytes between the executable path and argv.
    while offset < bytes.len() && bytes[offset] == 0 {
        offset += 1;
    }
    for _ in 0..argc {
        if offset >= bytes.len() {
            return false;
        }
        let Some(end) = bytes[offset..].iter().position(|byte| *byte == 0) else {
            return false;
        };
        if String::from_utf8_lossy(&bytes[offset..offset + end]).contains(needle) {
            return true;
        }
        offset += end + 1;
    }
    false
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn process_cmdline_contains(_pid: u32, _needle: &str) -> bool {
    false
}

/// Path of the exclusive session lock file.
pub fn lock_path(session: &str) -> Result<PathBuf> {
    validate_session_name(session)?;
    Ok(runtime_dir()?.join(format!("{session}.lock")))
}

/// How long a starting daemon retries the session lock before calling the
/// session taken. The window it covers is a fork/exec gap of a few ms; 250ms is
/// slack, not a poll budget. Pass `Duration::ZERO` to probe without waiting.
#[cfg(unix)]
pub const LOCK_WAIT: Duration = Duration::from_millis(250);
#[cfg(unix)]
const LOCK_RETRY_SLEEP: Duration = Duration::from_millis(5);

/// Acquire the exclusive session lock (single-instance), retrying for `wait`.
///
/// `flock` is held by the *open file description*, so any child forked while we
/// hold the lock inherits it and keeps it alive until it execs (the fd is
/// CLOEXEC, so exec is what drops it). A daemon that forks pane shells, `git`,
/// or `ss` therefore leaves a few-millisecond window in which a lock it has
/// already released still reads as held. Failing fast there turns a free
/// session into "already locked by another vmux daemon" — so a daemon taking
/// the lock for real passes `LOCK_WAIT`, and only callers probing whether the
/// lock is held right now pass `Duration::ZERO`.
///
/// Returns the held file so the OS releases the lock when the process exits.
#[cfg(unix)]
pub fn lock_session(session: &str, wait: Duration) -> Result<Option<std::fs::File>> {
    use std::os::unix::io::AsRawFd;
    let path = lock_path(session)?;
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("open lock {}", path.display()))?;
    let deadline = Instant::now() + wait;
    loop {
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            break;
        }
        // Only EWOULDBLOCK/EAGAIN mean "held by someone else". Any other errno
        // (permissions, bad fd, …) is permanent — spinning for LOCK_WAIT would
        // just delay a real failure and report it as "already locked".
        let err = std::io::Error::last_os_error();
        let retryable = matches!(
            err.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
        );
        if !retryable {
            return Err(err)
                .with_context(|| format!("flock exclusive on session lock {}", path.display()));
        }
        // Duration::ZERO must fail on the first contention without sleeping.
        if wait.is_zero() || Instant::now() >= deadline {
            bail!(
                "session {session:?} is already locked by another vmux daemon (lock {})",
                path.display()
            );
        }
        std::thread::sleep(LOCK_RETRY_SLEEP);
    }
    // Record our pid inside the lock for doctor/debug.
    let _ = fs::write(&path, format!("{}\n", std::process::id()));
    // Keep flock: rewriting path content doesn't drop the lock on the open fd.
    Ok(Some(file))
}

fn collect_session_names(dir: &Path, extension: &str, names: &mut BTreeSet<String>) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(dir).with_context(|| format!("read dir {}", dir.display()))? {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some(extension) {
            if let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) {
                if validate_session_name(stem).is_ok() {
                    names.insert(stem.to_string());
                }
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
unsafe fn libc_getuid() -> u32 {
    unsafe extern "C" {
        fn getuid() -> u32;
    }
    getuid()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_process_checks_recognize_the_current_process() {
        let pid = std::process::id();
        assert!(process_exists(pid));
        assert!(process_starttime(pid).is_some());
        assert!(process_matches_record(PidRecord {
            pid,
            starttime: process_starttime(pid),
        }));
        assert!(process_cmdline_contains(pid, "vmux"));
        assert!(!process_cmdline_contains(pid, "vmux-no-such-argument"));
    }

    #[test]
    fn native_process_checks_reject_an_impossible_pid() {
        let pid = i32::MAX as u32;
        assert!(!process_exists(pid));
        assert_eq!(process_starttime(pid), None);
        assert!(!process_cmdline_contains(pid, "vmux"));
    }

    #[test]
    fn process_record_rejects_a_mismatched_start_time() {
        let pid = std::process::id();
        let starttime = process_starttime(pid).expect("current process start time");
        assert!(!process_matches_record(PidRecord {
            pid,
            starttime: Some(starttime.saturating_add(1)),
        }));
    }

    #[test]
    fn native_cmdline_check_reads_process_arguments() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();
        assert!(process_cmdline_contains(pid, "sleep"));
        assert!(process_cmdline_contains(pid, "30"));
        assert!(!process_cmdline_contains(pid, "vmux-no-such-argument"));
        child.kill().ok();
        child.wait().ok();
    }

    #[test]
    fn runtime_artifact_paths_share_session_stem() {
        let socket = socket_path("abc").unwrap();
        let pid = pid_path("abc").unwrap();
        let log = log_path("abc").unwrap();
        assert_eq!(socket.file_name().unwrap(), "abc.sock");
        assert_eq!(pid.file_name().unwrap(), "abc.pid");
        assert_eq!(log.file_name().unwrap(), "abc.log");
    }

    #[test]
    fn config_path_lives_under_vmux_config_dir() {
        let path = config_path().unwrap();
        assert_eq!(path.file_name().unwrap(), "config.json");
        assert_eq!(path.parent().unwrap().file_name().unwrap(), "vmux");
    }

    #[test]
    fn validate_session_name_accepts_plain_names() {
        assert!(validate_session_name("default").is_ok());
        assert!(validate_session_name("my-session_1").is_ok());
    }

    #[test]
    fn validate_session_name_rejects_empty() {
        assert!(validate_session_name("").is_err());
    }

    #[test]
    fn validate_session_name_rejects_traversal_and_separators() {
        for bad in [
            "..",
            "../evil",
            "foo/bar",
            "foo\\bar",
            "a..b",
            "/abs",
            "with\0nul",
        ] {
            assert!(
                validate_session_name(bad).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn path_builders_reject_unsafe_session_names() {
        assert!(socket_path("../escape").is_err());
        assert!(pid_path("../escape").is_err());
        assert!(log_path("foo/bar").is_err());
        assert!(state_path("..").is_err());
    }

    #[test]
    fn list_sessions_includes_state_only_session() {
        let name = format!("vmux-test-{}", std::process::id());
        let path = state_path(&name).unwrap();
        fs::write(&path, "{}").unwrap();
        let sessions = list_sessions().unwrap();
        fs::remove_file(&path).ok();
        assert!(sessions
            .iter()
            .any(|session| session.name == name && !session.running));
    }

    #[test]
    fn reserved_state_stems_are_not_sessions() {
        // The daemon's own state files live beside `<session>.json`. Before this
        // was reserved, the daily update check made `update-check` show up in
        // `vmux sessions` on every install.
        for reserved in RESERVED_STATE_STEMS {
            assert!(
                validate_session_name(reserved).is_err(),
                "expected {reserved:?} to be rejected as a session name"
            );
            assert!(state_path(reserved).is_err());
        }
    }

    #[test]
    fn list_sessions_skips_the_daemons_own_state_files() {
        // Materialize the real files list_sessions would otherwise report.
        let update = update_cache_path().unwrap();
        let had_update = update.exists();
        if !had_update {
            fs::write(&update, "{}").unwrap();
        }
        let sessions = list_sessions().unwrap();
        if !had_update {
            fs::remove_file(&update).ok();
        }
        for reserved in RESERVED_STATE_STEMS {
            assert!(
                !sessions.iter().any(|s| s.name == *reserved),
                "{reserved:?} is a daemon state file, not a session, but was listed"
            );
        }
    }

    /// A child forked while we hold the lock inherits the fd and keeps the
    /// flock alive until it execs, so a session that is free can read as held
    /// for a few milliseconds. A starting daemon must wait that out.
    #[cfg(unix)]
    #[test]
    fn lock_session_waits_out_a_holder_that_is_letting_go() {
        let session = format!("vmux-test-lock-wait-{}", std::process::id());
        let held = lock_session(&session, Duration::ZERO)
            .unwrap()
            .expect("first lock");
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            drop(held);
        });

        let started = Instant::now();
        let taken = lock_session(&session, LOCK_WAIT)
            .expect("a lock released inside the wait window must be acquired, not refused");
        let waited = started.elapsed();
        assert!(taken.is_some());
        assert!(
            waited >= Duration::from_millis(40),
            "should have waited for the holder, returned after {waited:?}"
        );

        releaser.join().unwrap();
        drop(taken);
        fs::remove_file(lock_path(&session).unwrap()).ok();
    }

    /// The waiting is opt-in: callers probing whether a daemon is alive (and
    /// the shutdown test that asserts a held lock is refused) must not spin.
    #[cfg(unix)]
    #[test]
    fn a_zero_wait_lock_refuses_a_held_lock_immediately() {
        let session = format!("vmux-test-lock-nowait-{}", std::process::id());
        let held = lock_session(&session, Duration::ZERO)
            .unwrap()
            .expect("first lock");

        let started = Instant::now();
        assert!(
            lock_session(&session, Duration::ZERO).is_err(),
            "a held lock must be refused"
        );
        assert!(
            started.elapsed() < LOCK_WAIT,
            "a zero wait must return immediately, not retry"
        );

        drop(held);
        fs::remove_file(lock_path(&session).unwrap()).ok();
    }
}
