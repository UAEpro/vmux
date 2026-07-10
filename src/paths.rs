use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};

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
    Ok(())
}

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

/// PID file record: `pid` on line 1, optional process starttime (jiffies) on line 2.
/// Starttime prevents signalling a recycled PID after a crash (bugs.md P1#2).
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
    PathBuf::from(format!("/proc/{pid}")).exists()
}

/// Linux `/proc/<pid>/stat` field 22 (starttime in clock ticks).
pub fn process_starttime(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm can contain spaces/parens: parse after the last `) `.
    let after_comm = stat.rsplit_once(") ").map(|(_, rest)| rest)?;
    let field = after_comm.split_whitespace().nth(19)?; // 22nd field overall → index 19 after comm
    field.parse().ok()
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

/// Best-effort: does `/proc/<pid>/cmdline` look like a vmux daemon/relay?
pub fn process_cmdline_contains(pid: u32, needle: &str) -> bool {
    fs::read(format!("/proc/{pid}/cmdline"))
        .ok()
        .map(|bytes| {
            let text = String::from_utf8_lossy(&bytes);
            text.contains(needle)
        })
        .unwrap_or(false)
}

/// Path of the exclusive session lock file.
pub fn lock_path(session: &str) -> Result<PathBuf> {
    validate_session_name(session)?;
    Ok(runtime_dir()?.join(format!("{session}.lock")))
}

/// Acquire an exclusive non-blocking lock for this session (single-instance).
/// Returns the held file so the OS releases the lock when the process exits.
#[cfg(unix)]
pub fn try_lock_session(session: &str) -> Result<Option<std::fs::File>> {
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
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        bail!(
            "session {session:?} is already locked by another vmux daemon (lock {})",
            path.display()
        );
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
}
