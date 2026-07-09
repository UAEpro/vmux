use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

pub fn runtime_dir() -> Result<PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/tmp/vmux-{}", unsafe { libc_getuid() })))
        .join("vmux");
    fs::create_dir_all(&dir).with_context(|| format!("create runtime dir {}", dir.display()))?;
    Ok(dir)
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
    fs::create_dir_all(&dir).with_context(|| format!("create state dir {}", dir.display()))?;
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
    fs::create_dir_all(&dir).with_context(|| format!("create config dir {}", dir.display()))?;
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
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

pub fn process_exists(pid: u32) -> bool {
    PathBuf::from(format!("/proc/{pid}")).exists()
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
