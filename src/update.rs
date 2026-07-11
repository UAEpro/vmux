//! Stage-A update *notification* (no self-update): a best-effort, cached check
//! against the GitHub Releases API so the daemon can tell the user when a newer
//! `vmux` is published. It never blocks a command, fails silently on any error,
//! and can be turned off with `VMUX_NO_UPDATE_CHECK=1`.
//!
//! The daemon keeps the cache fresh in the background ([`refresh_if_stale`]);
//! any client reads it cheaply ([`available_update`]) to show a notice.

use crate::paths;
use serde::{Deserialize, Serialize};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

/// Re-check the network at most this often.
const TTL_SECS: u64 = 24 * 60 * 60;
/// Hard bound on the network probe so a slow/hung endpoint never stalls a tick.
const CHECK_TIMEOUT_SECS: u64 = 5;

/// The version this binary was built as.
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// `owner/repo` parsed from Cargo.toml's `repository` (a github.com URL). Returns
/// `None` when unset or not a GitHub URL, which disables the check entirely.
fn repo_slug() -> Option<String> {
    let url = option_env!("CARGO_PKG_REPOSITORY").filter(|s| !s.is_empty())?;
    parse_github_slug(url)
}

fn parse_github_slug(url: &str) -> Option<String> {
    // Accept https://github.com/owner/repo(.git) and git@github.com:owner/repo.git
    let rest = url.split("github.com").nth(1)?;
    let rest = rest.trim_start_matches([':', '/']);
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let mut parts = rest.split('/').filter(|s| !s.is_empty());
    let owner = parts.next()?;
    let repo = parts.next()?;
    Some(format!("{owner}/{repo}"))
}

/// Explicit opt-out. Any non-empty, non-"0" value disables the check.
fn disabled() -> bool {
    matches!(
        std::env::var("VMUX_NO_UPDATE_CHECK").ok().as_deref(),
        Some(v) if !v.is_empty() && v != "0"
    )
}

#[derive(Serialize, Deserialize, Default)]
struct Cache {
    /// Unix seconds of the last network *attempt* (success or not).
    last_checked: u64,
    /// Latest release version seen, without a leading `v` (e.g. "0.2.0").
    latest: String,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn read_cache() -> Cache {
    paths::update_cache_path()
        .ok()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn write_cache(cache: &Cache) {
    let Ok(path) = paths::update_cache_path() else {
        return;
    };
    if let Ok(bytes) = serde_json::to_vec(cache) {
        let _ = std::fs::write(path, bytes);
    }
}

/// Refresh the cached latest version if it is older than the TTL. Best-effort:
/// network-bounded, silent on error, and self-rate-limited so it is safe to call
/// on a frequent loop. Intended to run on a daemon background thread.
pub fn refresh_if_stale() {
    if disabled() {
        return;
    }
    let Some(slug) = repo_slug() else {
        return;
    };
    let mut cache = read_cache();
    if now_secs().saturating_sub(cache.last_checked) < TTL_SECS {
        return;
    }
    // Stamp the attempt *before* fetching so a persistent failure (offline, rate
    // limit) backs off for the full TTL instead of retrying every tick.
    cache.last_checked = now_secs();
    if let Some(latest) = fetch_latest_release(&slug) {
        cache.latest = latest;
    }
    write_cache(&cache);
}

/// Query the GitHub Releases API for the latest published release tag. Uses the
/// already-required `curl` (see `daemon::browser`) rather than pulling in an HTTP
/// crate. Returns the version with any leading `v` stripped.
fn fetch_latest_release(slug: &str) -> Option<String> {
    let url = format!("https://api.github.com/repos/{slug}/releases/latest");
    let output = Command::new("curl")
        .args([
            "-sS",
            "--max-time",
            &CHECK_TIMEOUT_SECS.to_string(),
            "-H",
            "Accept: application/vnd.github+json",
            &url,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let tag = value.get("tag_name")?.as_str()?;
    let version = tag.trim().trim_start_matches('v');
    if version.is_empty() {
        None
    } else {
        Some(version.to_string())
    }
}

/// The newer version available, if the cached latest release is greater than the
/// running version. `None` when up to date, disabled, or not yet known. Cheap
/// (one small file read) — safe for clients to call at startup.
pub fn available_update() -> Option<String> {
    if disabled() {
        return None;
    }
    let cache = read_cache();
    if !cache.latest.is_empty() && version_gt(&cache.latest, current_version()) {
        Some(cache.latest)
    } else {
        None
    }
}

/// `true` when `a` is a strictly newer dotted-numeric version than `b`.
/// Pre-release / build suffixes are ignored — sufficient for a notification.
fn version_gt(a: &str, b: &str) -> bool {
    parse_version(a) > parse_version(b)
}

fn parse_version(s: &str) -> Vec<u64> {
    s.split(['-', '+'])
        .next()
        .unwrap_or(s)
        .split('.')
        .map(|part| {
            part.chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .unwrap_or(0)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_ordering_is_numeric_not_lexical() {
        assert!(version_gt("0.10.0", "0.9.0"));
        assert!(version_gt("1.0.0", "0.99.99"));
        assert!(version_gt("0.2.0", "0.1.9"));
        assert!(!version_gt("0.1.0", "0.1.0"));
        assert!(!version_gt("0.1.0", "0.2.0"));
    }

    #[test]
    fn version_ignores_prerelease_and_v_prefix_upstream() {
        // Leading `v` is stripped before storage; suffixes are ignored.
        assert!(!version_gt("0.1.0-rc1", "0.1.0"));
        assert!(version_gt("0.2.0", "0.1.0-rc1"));
    }

    #[test]
    fn github_slug_parses_https_and_ssh_forms() {
        assert_eq!(
            parse_github_slug("https://github.com/octo/vmux"),
            Some("octo/vmux".to_string())
        );
        assert_eq!(
            parse_github_slug("https://github.com/octo/vmux.git"),
            Some("octo/vmux".to_string())
        );
        assert_eq!(
            parse_github_slug("git@github.com:octo/vmux.git"),
            Some("octo/vmux".to_string())
        );
        assert_eq!(parse_github_slug("https://gitlab.com/octo/vmux"), None);
    }
}
