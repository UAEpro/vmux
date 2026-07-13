//! Opt-in cmux-remote compatible relay.
//!
//! Exposes HTTP + WebSocket endpoints that the community **Cmux Remote**
//! iPhone app speaks, and maps them onto the existing vmux Unix-socket API.
//!
//! Isolation guarantees:
//! - Never started unless the user runs `vmux relay serve`.
//! - Talks to the daemon only as a normal client (same as CLI).
//! - Does not change attach/CLI/daemon behaviour when inactive.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tungstenite::protocol::Role;
use tungstenite::Message;

use crate::cli::SplitDirection;
use crate::config::{LmuxConfig, RelaySettings};
use crate::daemon;
use crate::paths;
use crate::protocol::{self, Request, Response};
use crate::sync::MutexExt;
use std::process::{Command as ProcessCommand, Stdio};

const RELAY_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Safe default: localhost only. Never default to 0.0.0.0.
const DEFAULT_LISTEN: &str = "127.0.0.1:4399";
const DEFAULT_FPS: u32 = 15;
const DEFAULT_IDLE_FPS: u32 = 5;
const HELLO_TIMEOUT: Duration = Duration::from_millis(500);

// ─── Config ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RelayConfig {
    /// `host:port` bind address (default `127.0.0.1:4399`; never `0.0.0.0`).
    pub listen: String,
    /// Tailscale login names allowed to register devices. Empty = any
    /// successful `tailscale whois` peer (or localhost when allowed).
    pub allow_login: Vec<String>,
    /// Allow loopback peers to register without Tailscale (dev / local).
    pub allow_localhost: bool,
    /// Accept any source in the Tailscale CGNAT range without whois
    /// (weaker; useful when `tailscale whois` is unavailable).
    pub allow_tailnet_cgnat: bool,
    pub default_fps: u32,
    pub idle_fps: u32,
    /// vmux session the relay attaches to.
    pub session: String,
    /// Serve the browser paste page (GET /paste, POST /v1/paste). Defaults to
    /// on — uploads still require a paired device token. `vmux config set
    /// relay.allow_paste false` (or the Settings panel) turns it off.
    pub allow_paste: bool,
    /// Honor `view_cols`/`view_rows` on `surface.subscribe` (phone-fit pane
    /// sizing). Off by default — a phone glance must not resize a pane the
    /// desktop user is looking at unless the host opted in:
    /// `vmux config set relay.allow_view_resize true`.
    pub allow_view_resize: bool,
    /// When set (non-empty), **every** device registration must present this
    /// secret via `X-Vmux-Bootstrap` or `Authorization: Bootstrap <secret>`.
    /// Whois/localhost/CGNAT identity still applies after the secret check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bootstrap_secret: Option<String>,
    #[serde(default)]
    pub snippets: Vec<Snippet>,
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            listen: DEFAULT_LISTEN.to_string(),
            allow_login: Vec::new(),
            allow_localhost: false,
            allow_tailnet_cgnat: false,
            allow_paste: true,
            allow_view_resize: false,
            default_fps: DEFAULT_FPS,
            idle_fps: DEFAULT_IDLE_FPS,
            session: "default".to_string(),
            bootstrap_secret: None,
            snippets: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snippet {
    pub label: String,
    pub text: String,
}

pub fn default_config_path() -> Result<PathBuf> {
    Ok(paths::config_dir()?.join("relay.json"))
}

pub fn devices_path() -> Result<PathBuf> {
    Ok(paths::state_dir()?.join("relay-devices.json"))
}

pub fn load_or_init_config(path: &Path) -> Result<RelayConfig> {
    if path.exists() {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("read relay config {}", path.display()))?;
        let mut cfg: RelayConfig = serde_json::from_str(&raw)
            .with_context(|| format!("parse relay config {}", path.display()))?;
        if cfg.default_fps == 0 {
            cfg.default_fps = DEFAULT_FPS;
        }
        if cfg.idle_fps == 0 {
            cfg.idle_fps = DEFAULT_IDLE_FPS;
        }
        if std::env::var_os("VMUX_RELAY_ALLOW_LOCALHOST").as_deref() == Some("1".as_ref()) {
            cfg.allow_localhost = true;
        }
        return Ok(cfg);
    }
    let cfg = RelayConfig {
        allow_localhost: std::env::var_os("VMUX_RELAY_ALLOW_LOCALHOST").as_deref()
            == Some("1".as_ref()),
        ..RelayConfig::default()
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_string_pretty(&cfg)? + "\n")
        .with_context(|| format!("write default relay config {}", path.display()))?;
    // This file carries `bootstrap_secret`. The config dir is already 0700, but
    // the device store next to it sets an explicit mode and the one file with a
    // shared secret in it should not be the exception.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    Ok(cfg)
}

// ─── Device store ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DeviceRecord {
    device_id: String,
    token_hash: String,
    login_name: String,
    hostname: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    apns_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    apns_env: Option<String>,
    created_at: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct DeviceFile {
    devices: Vec<DeviceRecord>,
}

#[derive(Debug)]
struct DeviceStore {
    path: PathBuf,
    devices: Mutex<HashMap<String, DeviceRecord>>,
}

impl DeviceStore {
    fn load(path: PathBuf) -> Result<Self> {
        let devices = if path.exists() {
            let raw = fs::read_to_string(&path)?;
            // Fail closed on corrupt storage — never treat as empty.
            let file: DeviceFile = serde_json::from_str(&raw).with_context(|| {
                format!(
                    "parse device store {} failed; refusing to wipe (repair or delete the file)",
                    path.display()
                )
            })?;
            file.devices
                .into_iter()
                .map(|d| (d.device_id.clone(), d))
                .collect()
        } else {
            HashMap::new()
        };
        Ok(Self {
            path,
            devices: Mutex::new(devices),
        })
    }

    fn persist_locked(path: &Path, map: &HashMap<String, DeviceRecord>) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = DeviceFile {
            devices: map.values().cloned().collect(),
        };
        let payload = serde_json::to_string_pretty(&file)? + "\n";
        // Atomic write: temp in same dir + rename.
        let tmp = path.with_extension(format!("tmp-{}-{}", std::process::id(), now_secs()));
        fs::write(&tmp, &payload)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
        }
        fs::rename(&tmp, path)?;
        Ok(())
    }

    fn register(
        &self,
        device_id: &str,
        login_name: &str,
        hostname: &str,
        plain_token: &str,
    ) -> Result<()> {
        let mut guard = self.devices.lock_or_recover();
        guard.insert(
            device_id.to_string(),
            DeviceRecord {
                device_id: device_id.to_string(),
                token_hash: sha256_hex(plain_token),
                login_name: login_name.to_string(),
                hostname: hostname.to_string(),
                apns_token: None,
                apns_env: None,
                created_at: now_secs(),
            },
        );
        Self::persist_locked(&self.path, &guard)
    }

    fn revoke(&self, device_id: &str) -> Result<bool> {
        let mut guard = self.devices.lock_or_recover();
        let removed = guard.remove(device_id).is_some();
        if removed {
            Self::persist_locked(&self.path, &guard)?;
        }
        Ok(removed)
    }

    fn validate_token(&self, token: &str) -> Option<String> {
        let hash = sha256_hex(token);
        let guard = self.devices.lock_or_recover();
        let mut found = None;
        for d in guard.values() {
            // Constant-time-ish compare on the hex hash (equal length SHA-256 hex).
            if ct_eq_str(&d.token_hash, &hash) {
                found = Some(d.device_id.clone());
            }
        }
        found
    }

    fn set_apns(&self, device_id: &str, token: &str, env: &str) -> Result<()> {
        let mut guard = self.devices.lock_or_recover();
        let Some(dev) = guard.get_mut(device_id) else {
            bail!("unknown device");
        };
        dev.apns_token = Some(token.to_string());
        dev.apns_env = Some(env.to_string());
        Self::persist_locked(&self.path, &guard)
    }

    fn list(&self) -> Vec<DeviceRecord> {
        self.devices
            .lock()
            .expect("device store lock")
            .values()
            .cloned()
            .collect()
    }
}

// ─── Shared server state ────────────────────────────────────────────────────

struct RelayState {
    config: RelayConfig,
    devices: DeviceStore,
    socket: PathBuf,
    boot_id: String,
    started_at: u64,
    running: AtomicBool,
    /// Active TCP handlers (capped to limit thread/memory DoS).
    active_connections: AtomicUsize,
}

/// Max simultaneous TCP connections accepted by the relay.
const MAX_RELAY_CONNECTIONS: usize = 64;

// ─── Public CLI entry points ────────────────────────────────────────────────

pub fn serve(
    session: &str,
    config_path: Option<PathBuf>,
    listen_override: Option<String>,
    allow_localhost: bool,
) -> Result<()> {
    let path = match config_path {
        Some(p) => p,
        None => default_config_path()?,
    };
    let mut config = load_or_init_config(&path)?;
    if let Some(listen) = listen_override {
        config.listen = listen;
    }
    if allow_localhost {
        config.allow_localhost = true;
    }
    if !session.is_empty() && session != "default" {
        config.session = session.to_string();
    }
    // Harden legacy configs that still say 0.0.0.0.
    if assert_safe_listen(&config.listen).is_err() {
        eprintln!(
            "vmux relay: refusing listen {} — remapping to {}",
            config.listen, DEFAULT_LISTEN
        );
        config.listen = DEFAULT_LISTEN.to_string();
    }
    assert_safe_listen(&config.listen)?;

    daemon::ensure_running(&config.session)?;
    let socket = paths::socket_path(&config.session)?;

    let state = Arc::new(RelayState {
        config: config.clone(),
        devices: DeviceStore::load(devices_path()?)?,
        socket,
        boot_id: random_hex(16),
        started_at: now_secs(),
        running: AtomicBool::new(true),
        active_connections: AtomicUsize::new(0),
    });

    let listener = TcpListener::bind(&config.listen)
        .with_context(|| format!("bind relay on {}", config.listen))?;
    eprintln!(
        "vmux relay listening on {} (session={}, socket={})",
        config.listen,
        config.session,
        state.socket.display()
    );
    eprintln!(
        "  health:  curl -s http://127.0.0.1:{}/v1/health",
        port_of(&config.listen).unwrap_or(4399)
    );
    if config.allow_paste {
        eprintln!(
            "  paste:   http://<this-host>:{}/paste — paste screenshots from any browser into the active pane",
            port_of(&config.listen).unwrap_or(4399)
        );
    }
    eprintln!("  config:  {}", path.display());
    eprintln!("  devices: {}", devices_path()?.display());
    eprintln!("  max conns: {MAX_RELAY_CONNECTIONS}");
    if config.allow_localhost {
        eprintln!("  auth:    localhost registration allowed");
    }
    if config.allow_tailnet_cgnat {
        eprintln!("  auth:    Tailscale CGNAT sources accepted without whois");
    }
    if config
        .bootstrap_secret
        .as_ref()
        .map(|s| !s.is_empty())
        .unwrap_or(false)
    {
        eprintln!("  auth:    bootstrap secret REQUIRED for all registration");
    }

    for stream in listener.incoming() {
        if !state.running.load(Ordering::Relaxed) {
            break;
        }
        let Ok(stream) = stream else { continue };
        // Cap concurrent handlers before spawning a thread.
        let active = state.active_connections.load(Ordering::Relaxed);
        if active >= MAX_RELAY_CONNECTIONS {
            let _ = stream.shutdown(std::net::Shutdown::Both);
            eprintln!("relay: rejecting connection (at capacity {MAX_RELAY_CONNECTIONS})");
            continue;
        }
        state.active_connections.fetch_add(1, Ordering::Relaxed);
        let state = Arc::clone(&state);
        thread::spawn(move || {
            let conns = Arc::clone(&state);
            let _guard = ConnectionGuard(conns);
            if let Err(err) = handle_connection(stream, state) {
                eprintln!("relay connection error: {err:#}");
            }
        });
    }
    Ok(())
}

struct ConnectionGuard(Arc<RelayState>);
impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.0.active_connections.fetch_sub(1, Ordering::Relaxed);
    }
}

pub fn status(config_path: Option<PathBuf>) -> Result<()> {
    let path = match config_path {
        Some(p) => p,
        None => default_config_path()?,
    };
    let config = if path.exists() {
        load_or_init_config(&path)?
    } else {
        RelayConfig::default()
    };
    let devices = DeviceStore::load(devices_path()?)?.list();
    let port = port_of(&config.listen).unwrap_or(4399);
    let health_url = format!("http://127.0.0.1:{port}/v1/health");
    let health = match ureq_get_health(&health_url) {
        Ok(body) => body,
        Err(err) => format!("unreachable ({err})"),
    };
    println!("config:   {}", path.display());
    println!("listen:   {}", config.listen);
    println!("session:  {}", config.session);
    println!("devices:  {}", devices.len());
    for d in &devices {
        println!(
            "  - {}  login={} host={} created={}",
            &d.device_id[..d.device_id.len().min(12)],
            d.login_name,
            d.hostname,
            d.created_at
        );
    }
    println!("health:   {health}");
    Ok(())
}

pub fn devices_list() -> Result<()> {
    let devices = DeviceStore::load(devices_path()?)?.list();
    if devices.is_empty() {
        println!("(no paired devices)");
        return Ok(());
    }
    for d in devices {
        println!(
            "{}\tlogin={}\thost={}\tcreated={}",
            d.device_id, d.login_name, d.hostname, d.created_at
        );
    }
    Ok(())
}

pub fn devices_revoke(device_id: &str) -> Result<()> {
    let store = DeviceStore::load(devices_path()?)?;
    if store.revoke(device_id)? {
        println!("revoked {device_id}");
    } else {
        bail!("device not found: {device_id}");
    }
    Ok(())
}

// ─── Managed lifecycle (settings / attach auto-start) ───────────────────────

pub fn managed_pid_path() -> Result<PathBuf> {
    Ok(paths::state_dir()?.join("relay.pid"))
}

pub fn managed_log_path() -> Result<PathBuf> {
    Ok(paths::runtime_dir()?.join("relay.log"))
}

/// Resolve listen address from settings bind mode + port.
///
/// **Never returns `0.0.0.0` / `::`.** Phone access is Tailscale CGNAT or
/// localhost only, so the host is not exposed on every interface.
pub fn resolve_listen(settings: &RelaySettings) -> String {
    let port = if settings.port == 0 {
        4399
    } else {
        settings.port
    };
    match settings.bind.as_str() {
        "local" => format!("127.0.0.1:{port}"),
        "tailscale" => match tailscale_ipv4() {
            Some(ip) => format!("{ip}:{port}"),
            // Offline: stay local rather than opening all interfaces.
            None => format!("127.0.0.1:{port}"),
        },
        // auto (and any unknown / migrated "all")
        _ => match tailscale_ipv4() {
            Some(ip) => format!("{ip}:{port}"),
            None => format!("127.0.0.1:{port}"),
        },
    }
}

/// Reject binds that would listen on every interface (public exposure risk).
pub fn assert_safe_listen(listen: &str) -> Result<()> {
    let host = listen
        .rsplit_once(':')
        .map(|(h, _)| h.trim())
        .unwrap_or(listen.trim());
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host == "0.0.0.0" || host == "::" || host == "*" {
        bail!(
            "refusing to bind relay on {host} (all interfaces). \
             Use Tailscale IP, 127.0.0.1, or relay.bind=auto|tailscale|local"
        );
    }
    Ok(())
}

fn tailscale_ipv4() -> Option<String> {
    let output = ProcessCommand::new("tailscale")
        .args(["ip", "-4"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let ip = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    if ip.is_empty() || !ip.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return None;
    }
    Some(ip)
}

/// Probe whether something answers on the relay health endpoint for `listen`.
pub fn is_healthy(listen: &str) -> bool {
    let port = port_of(listen).unwrap_or(4399);
    // Prefer loopback for health when bound to 0.0.0.0 or a remote interface IP.
    let host = if listen.starts_with("127.") {
        "127.0.0.1"
    } else if let Some((h, _)) = listen.rsplit_once(':') {
        if h == "0.0.0.0" || h == "[::]" || h == "::" {
            "127.0.0.1"
        } else {
            h.trim_start_matches('[').trim_end_matches(']')
        }
    } else {
        "127.0.0.1"
    };
    let url = format!("http://{host}:{port}/v1/health");
    ureq_get_health(&url)
        .map(|body| body.contains("\"ok\":true") || body.contains("\"ok\": true"))
        .unwrap_or(false)
}

/// Human-readable status for the settings panel.
/// Cached briefly so Settings redraws do not spawn `tailscale` / TCP on every frame.
pub fn runtime_status_line(settings: &RelaySettings) -> String {
    use std::sync::Mutex;
    static CACHE: Mutex<Option<(Instant, String, bool, String)>> = Mutex::new(None);

    let listen = resolve_listen(settings);
    let enabled = settings.enabled;
    if let Ok(guard) = CACHE.lock() {
        if let Some((at, cached_listen, cached_enabled, line)) = guard.as_ref() {
            if cached_listen == &listen
                && *cached_enabled == enabled
                && at.elapsed() < Duration::from_secs(2)
            {
                return line.clone();
            }
        }
    }

    let line = if is_healthy(&listen) {
        format!("running · {listen}")
    } else if enabled {
        format!("enabled · not running · {listen}")
    } else {
        format!("off · {listen}")
    };
    if let Ok(mut guard) = CACHE.lock() {
        *guard = Some((Instant::now(), listen, enabled, line.clone()));
    }
    line
}

/// Sync main config relay section into `~/.config/vmux/relay.json` used by serve.
pub fn sync_relay_json_from_settings(session: &str, settings: &RelaySettings) -> Result<PathBuf> {
    let path = default_config_path()?;
    let listen = resolve_listen(settings);
    let mut file_cfg = if path.exists() {
        load_or_init_config(&path)?
    } else {
        RelayConfig::default()
    };
    file_cfg.listen = listen;
    file_cfg.session = session.to_string();
    file_cfg.allow_localhost = settings.allow_localhost;
    file_cfg.allow_tailnet_cgnat = settings.allow_tailnet_cgnat;
    file_cfg.allow_paste = settings.allow_paste;
    file_cfg.allow_view_resize = settings.allow_view_resize;
    // Keep empty allow_login = any successful whois / CGNAT policy.
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, serde_json::to_string_pretty(&file_cfg)? + "\n")?;
    Ok(path)
}

/// If settings say enabled, start a managed relay process when not already healthy.
/// Safe to call from attach: no-op when disabled or already running.
pub fn ensure_from_config(session: &str, config: &LmuxConfig) -> Result<Option<String>> {
    if !config.relay.enabled {
        return Ok(None);
    }
    ensure_started(session, &config.relay)
}

/// Start managed relay (or return existing). Returns a short status message.
pub fn ensure_started(session: &str, settings: &RelaySettings) -> Result<Option<String>> {
    let listen = resolve_listen(settings);
    assert_safe_listen(&listen)?;
    if is_healthy(&listen) {
        return Ok(Some(format!("mobile relay already running on {listen}")));
    }
    // Clean stale pid
    let _ = stop_managed();

    let cfg_path = sync_relay_json_from_settings(session, settings)?;
    let log_path = managed_log_path()?;
    let pid_path = managed_pid_path()?;
    let exe = std::env::current_exe().context("resolve vmux executable")?;

    let log_file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("open relay log {}", log_path.display()))?;
    let log_err = log_file.try_clone()?;

    let mut cmd = ProcessCommand::new(exe);
    cmd.arg("--session")
        .arg(session)
        .arg("relay")
        .arg("serve")
        .arg("--config")
        .arg(&cfg_path)
        .arg("--listen")
        .arg(&listen);
    if settings.allow_localhost {
        cmd.arg("--allow-localhost");
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_err));

    // Detach from controlling terminal / process group so attach exit doesn't kill relay.
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            // setsid so SIGHUP from the attach TTY does not stop the relay.
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = cmd.spawn().context("spawn vmux relay serve")?;
    let pid = child.id();
    crate::paths::write_pid_record(&pid_path, pid)
        .with_context(|| format!("write {}", pid_path.display()))?;

    // Wait briefly for health
    for _ in 0..20 {
        thread::sleep(Duration::from_millis(100));
        if is_healthy(&listen) {
            return Ok(Some(format!(
                "mobile relay started on {listen} (pid {pid})"
            )));
        }
    }
    Ok(Some(format!(
        "mobile relay spawning on {listen} (pid {pid}; check {})",
        log_path.display()
    )))
}

/// Stop the managed relay process if we started it (pid file).
pub fn stop_managed() -> Result<bool> {
    let path = managed_pid_path()?;
    if !path.exists() {
        return Ok(false);
    }
    let record = crate::paths::read_pid_record(&path);
    let _ = fs::remove_file(&path);
    let Some(record) = record else {
        return Ok(false);
    };
    if record.pid <= 1 {
        return Ok(false);
    }
    // Never signal a recycled PID or a process that is not our relay.
    if !crate::paths::process_matches_record(record)
        || !crate::paths::process_cmdline_contains(record.pid, "vmux")
    {
        return Ok(false);
    }
    #[cfg(unix)]
    {
        let pid = record.pid as i32;
        let _ = unsafe { libc::kill(pid, libc::SIGTERM) };
        thread::sleep(Duration::from_millis(200));
        // SIGKILL only if the same process (starttime) is still alive.
        if crate::paths::process_matches_record(record) {
            let _ = unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
    Ok(true)
}

/// Apply settings change: enable starts, disable stops.
pub fn apply_enabled(session: &str, settings: &RelaySettings) -> Result<String> {
    if settings.enabled {
        Ok(ensure_started(session, settings)?.unwrap_or_else(|| "mobile relay on".into()))
    } else {
        let stopped = stop_managed()?;
        // Also try to free port if someone else left a process — only if healthy
        // and we don't know the pid; leave foreign processes alone.
        if stopped {
            Ok("mobile relay stopped".into())
        } else if is_healthy(&resolve_listen(settings)) {
            Ok("relay still reachable (not managed by this pid file)".into())
        } else {
            Ok("mobile relay off".into())
        }
    }
}

fn ureq_get_health(url: &str) -> Result<String> {
    // Avoid adding ureq — tiny blocking GET with TcpStream.
    let url = url
        .strip_prefix("http://")
        .ok_or_else(|| anyhow!("only http supported"))?;
    let (hostport, path) = url
        .split_once('/')
        .map(|(h, p)| (h, format!("/{p}")))
        .unwrap_or((url, "/".into()));
    let mut stream = TcpStream::connect(hostport)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {hostport}\r\nConnection: close\r\n\r\n"
    )?;
    let mut buf = String::new();
    stream.read_to_string(&mut buf)?;
    let body = buf.split("\r\n\r\n").nth(1).unwrap_or("").trim();
    Ok(body.to_string())
}

// ─── HTTP connection handling ───────────────────────────────────────────────

/// Request-line / header limits for unauthenticated peers.
const MAX_REQUEST_LINE: usize = 8 * 1024;
const MAX_HEADER_LINE: usize = 8 * 1024;
const MAX_HEADER_BYTES: usize = 64 * 1024;
/// HTTP request bodies are only small JSON payloads (register/apns); file
/// uploads go over the WebSocket channel, not here. Cap well below MAX_WS_MSG so
/// an unauthenticated peer can't force a 24 MiB allocation per connection.
/// Exception: POST /v1/paste carries raw screenshot bytes and is allowed
/// MAX_PASTE_BODY — but only after the device token is validated, so the
/// pre-auth allocation bound stays at this cap.
const MAX_HTTP_BODY: usize = 256 * 1024;
/// Body cap for authenticated POST /v1/paste (browser screenshot paste).
const MAX_PASTE_BODY: usize = 16 * 1024 * 1024;
/// Served at GET /paste: browser page that pastes clipboard images into panes.
const PASTE_PAGE: &str = include_str!("paste_page.html");
/// Bound on the per-connection outbound push queue. A stalled client blocks
/// `ws.send` up to the 30s write timeout; with an unbounded channel the surface
/// pollers would buffer ~30s of full-snapshot frames in memory. When the queue
/// is full we drop frames (the next diff/full frame re-syncs the client).
const PUSH_CHANNEL_CAP: usize = 128;

/// Read a line into `buf` without ever buffering more than `max` bytes. Returns
/// `Ok(false)` if the line exceeded `max` (or the peer hit EOF without a
/// newline). `BufRead::read_line` is otherwise unbounded, so the size caps that
/// run *after* it can't prevent a pre-auth OOM from a peer that never sends
/// `\n`.
fn read_line_capped<R: BufRead>(
    reader: &mut R,
    buf: &mut String,
    max: usize,
) -> std::io::Result<bool> {
    let read = (&mut *reader).take(max as u64).read_line(buf)?;
    Ok(read > 0 && buf.ends_with('\n'))
}
const MAX_HEADER_COUNT: usize = 64;

fn handle_connection(mut stream: TcpStream, state: Arc<RelayState>) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;

    let peer = stream
        .peer_addr()
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "0.0.0.0".into());

    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    let full = read_line_capped(&mut reader, &mut request_line, MAX_REQUEST_LINE)?;
    if request_line.is_empty() {
        return Ok(());
    }
    if !full {
        write_http(&mut stream, 414, "text/plain", b"request line too long")?;
        return Ok(());
    }
    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() < 2 {
        write_http(&mut stream, 400, "text/plain", b"bad request")?;
        return Ok(());
    }
    let method = parts[0].to_uppercase();
    let target = parts[1].to_string();
    let path = target.split('?').next().unwrap_or(&target).to_string();

    let mut headers: HashMap<String, String> = HashMap::new();
    let mut header_bytes = 0usize;
    loop {
        if headers.len() >= MAX_HEADER_COUNT {
            write_http(&mut stream, 431, "text/plain", b"too many headers")?;
            return Ok(());
        }
        let mut line = String::new();
        if !read_line_capped(&mut reader, &mut line, MAX_HEADER_LINE)? {
            write_http(&mut stream, 431, "text/plain", b"header line too long")?;
            return Ok(());
        }
        header_bytes = header_bytes.saturating_add(line.len());
        if header_bytes > MAX_HEADER_BYTES {
            write_http(&mut stream, 431, "text/plain", b"headers too large")?;
            return Ok(());
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }

    let content_len: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    // /v1/paste is the one route allowed a large body, and only once the
    // Bearer token checks out — unauthenticated peers keep the small cap.
    let body_cap = if method == "POST" && path == "/v1/paste" {
        if !state.config.allow_paste {
            write_http(&mut stream, 404, "text/plain", b"paste disabled")?;
            return Ok(());
        }
        if auth_device_from_headers(&headers, &state.devices).is_none() {
            write_http(&mut stream, 401, "text/plain", b"unauthorized")?;
            return Ok(());
        }
        MAX_PASTE_BODY
    } else {
        MAX_HTTP_BODY
    };
    if content_len > body_cap {
        write_http(&mut stream, 413, "text/plain", b"payload too large")?;
        return Ok(());
    }
    let mut body = vec![0u8; content_len];
    if !body.is_empty() {
        reader.read_exact(&mut body)?;
    }

    // WebSocket upgrade
    if method == "GET"
        && path == "/v1/ws"
        && headers
            .get("upgrade")
            .map(|v| v.eq_ignore_ascii_case("websocket"))
            .unwrap_or(false)
    {
        return handle_ws_upgrade(stream, reader, headers, state, peer);
    }

    // Re-assemble full stream ownership for HTTP responses (reader held clone).
    drop(reader);

    let device_id = auth_device_from_headers(&headers, &state.devices);

    match (method.as_str(), path.as_str()) {
        ("GET", "/paste") => {
            if !state.config.allow_paste {
                write_http(&mut stream, 404, "text/plain", b"paste disabled")?;
                return Ok(());
            }
            // Static page, no secrets: pairing/auth happens from its JS via
            // the same registration flow the phone app uses.
            let page = PASTE_PAGE.replace("{{SESSION}}", &state.config.session);
            write_http(
                &mut stream,
                200,
                "text/html; charset=utf-8",
                page.as_bytes(),
            )?;
        }
        ("POST", "/v1/paste") => {
            if device_id.is_none() {
                write_http(&mut stream, 401, "text/plain", b"unauthorized")?;
                return Ok(());
            }
            match paste_upload(&state, &target, &body) {
                Ok(v) => {
                    write_http(
                        &mut stream,
                        200,
                        "application/json",
                        v.to_string().as_bytes(),
                    )?;
                }
                Err(err) => {
                    let body = json!({ "error": err.to_string() });
                    write_http(
                        &mut stream,
                        400,
                        "application/json",
                        body.to_string().as_bytes(),
                    )?;
                }
            }
        }
        ("GET", "/v1/health") => {
            let body = json!({
                "ok": true,
                "version": RELAY_VERSION,
                "backend": "vmux",
                "session": state.config.session,
                "boot_id": state.boot_id,
            });
            write_http(
                &mut stream,
                200,
                "application/json",
                body.to_string().as_bytes(),
            )?;
        }
        ("GET", "/v1/state") => {
            // Requires auth: snippets may embed command text / paths the user
            // would not want an unauthenticated (e.g. tailnet) peer to read.
            if device_id.is_none() {
                write_http(&mut stream, 401, "text/plain", b"unauthorized")?;
                return Ok(());
            }
            let body = json!({
                "snippets": state.config.snippets,
                "default_fps": state.config.default_fps,
                "idle_fps": state.config.idle_fps,
                "backend": "vmux",
                "session": state.config.session,
                "boot_id": state.boot_id,
                "started_at": state.started_at,
            });
            write_http(
                &mut stream,
                200,
                "application/json",
                body.to_string().as_bytes(),
            )?;
        }
        ("POST", "/v1/devices/me/register") => match register_device(&state, &peer, &headers) {
            Ok((device_id, token)) => {
                let body = json!({ "device_id": device_id, "token": token });
                write_http(
                    &mut stream,
                    200,
                    "application/json",
                    body.to_string().as_bytes(),
                )?;
            }
            Err(RegisterError::Forbidden(msg)) => {
                eprintln!("register forbidden from {peer}: {msg}");
                // Throttle failed registrations: this holds the connection (and
                // one of the MAX_RELAY_CONNECTIONS slots) so a network peer can't
                // rapidly brute-force a weak bootstrap_secret.
                thread::sleep(Duration::from_millis(750));
                write_http(
                    &mut stream,
                    403,
                    "application/json",
                    br#"{"error":"forbidden"}"#,
                )?;
            }
            Err(RegisterError::Other(err)) => {
                eprintln!("register error from {peer}: {err:#}");
                write_http(
                    &mut stream,
                    500,
                    "application/json",
                    br#"{"error":"internal"}"#,
                )?;
            }
        },
        ("POST", "/v1/devices/me/apns") => {
            let Some(did) = device_id else {
                write_http(&mut stream, 401, "text/plain", b"unauthorized")?;
                return Ok(());
            };
            let payload: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let token = payload
                .get("apns_token")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let env = payload.get("env").and_then(|v| v.as_str()).unwrap_or("");
            if token.is_empty() || (env != "prod" && env != "sandbox") {
                write_http(&mut stream, 400, "text/plain", b"bad request")?;
            } else {
                let _ = state.devices.set_apns(&did, token, env);
                write_http(&mut stream, 204, "text/plain", b"")?;
            }
        }
        ("DELETE", "/v1/devices/me") => {
            let Some(did) = device_id else {
                write_http(&mut stream, 401, "text/plain", b"unauthorized")?;
                return Ok(());
            };
            let _ = state.devices.revoke(&did);
            write_http(&mut stream, 204, "text/plain", b"")?;
        }
        _ => {
            write_http(&mut stream, 404, "text/plain", b"not found")?;
        }
    }
    Ok(())
}

fn write_http(stream: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) -> Result<()> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "OK",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    if !body.is_empty() {
        stream.write_all(body)?;
    }
    stream.flush()?;
    Ok(())
}

fn auth_device_from_headers(
    headers: &HashMap<String, String>,
    devices: &DeviceStore,
) -> Option<String> {
    if let Some(auth) = headers.get("authorization") {
        if let Some(token) = auth
            .strip_prefix("Bearer ")
            .or_else(|| auth.strip_prefix("bearer "))
        {
            if let Some(id) = devices.validate_token(token.trim()) {
                return Some(id);
            }
        }
    }
    None
}

mod auth;
use auth::*;

// ─── WebSocket ──────────────────────────────────────────────────────────────

/// A stream that replays a prefix of already-buffered bytes before delegating to
/// the socket. During the HTTP read the BufReader can consume bytes past the
/// header terminator (a WS frame the client pipelined with the upgrade request);
/// those bytes are stuck in the BufReader and lost if we hand tungstenite the
/// raw socket. Replaying them here preserves the client's first frame.
struct PrefixedStream {
    prefix: Vec<u8>,
    pos: usize,
    inner: TcpStream,
}

impl PrefixedStream {
    fn set_read_timeout(&self, dur: Option<Duration>) -> std::io::Result<()> {
        self.inner.set_read_timeout(dur)
    }
}

impl Read for PrefixedStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pos < self.prefix.len() {
            let n = std::cmp::min(buf.len(), self.prefix.len() - self.pos);
            buf[..n].copy_from_slice(&self.prefix[self.pos..self.pos + n]);
            self.pos += n;
            return Ok(n);
        }
        self.inner.read(buf)
    }
}

impl Write for PrefixedStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.inner.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn handle_ws_upgrade(
    stream: TcpStream,
    reader: BufReader<TcpStream>,
    headers: HashMap<String, String>,
    state: Arc<RelayState>,
    _peer: String,
) -> Result<()> {
    let device_id = device_id_from_ws_headers(&headers, &state.devices)
        .ok_or_else(|| anyhow!("ws upgrade unauthorized"))?;

    // CSWSH guard: browsers always attach an Origin header to a WebSocket
    // handshake; the native phone client does not. A present Origin therefore
    // means a web page is trying to open the terminal-control channel — refuse
    // it even though a valid token is separately required.
    if headers
        .get("origin")
        .map(|o| !o.trim().is_empty())
        .unwrap_or(false)
    {
        bail!("ws upgrade rejected: unexpected Origin header (cross-site)");
    }

    // Reconstruct: tungstenite needs the raw stream before body was read.
    // We already consumed the request via a cloned stream — use the original
    // `stream` which still has the same socket; but request was read on the
    // clone. For HTTP upgrade, we must feed the request into the accept
    // handshake. Use accept_hdr with a callback after re-sending is hard.
    //
    // Simpler approach: perform the WS handshake manually with the headers
    // we already parsed, then wrap the stream with tungstenite::WebSocket.

    let key = headers
        .get("sec-websocket-key")
        .ok_or_else(|| anyhow!("missing Sec-WebSocket-Key"))?;
    let accept = ws_accept_key(key);

    // Echo first offered subprotocol (required by iOS URLSessionWebSocketTask).
    let proto_echo = headers
        .get("sec-websocket-protocol")
        .and_then(|p| p.split(',').next())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    // Capture any bytes the BufReader read past the header terminator (a WS
    // frame the client pipelined with the upgrade) BEFORE dropping it, then
    // replay them to tungstenite so the client's first frame is not lost.
    let buffered = reader.buffer().to_vec();
    drop(reader);
    let mut stream = stream;
    write!(
        stream,
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n"
    )?;
    if let Some(proto) = proto_echo {
        write!(stream, "Sec-WebSocket-Protocol: {proto}\r\n")?;
    }
    write!(stream, "\r\n")?;
    stream.flush()?;

    let socket = PrefixedStream {
        prefix: buffered,
        pos: 0,
        inner: stream,
    };
    let mut ws = tungstenite::WebSocket::from_raw_socket(socket, Role::Server, None);
    ws.get_mut()
        .set_read_timeout(Some(Duration::from_millis(200)))
        .ok();

    // Hello must arrive quickly.
    let hello_deadline = Instant::now() + HELLO_TIMEOUT;
    let mut helloed = false;
    let mut active_subs: HashMap<String, thread::JoinHandle<()>> = HashMap::new();
    let stop_flags: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let push_tx = Arc::new(Mutex::new(None::<std::sync::mpsc::SyncSender<String>>));
    let (tx, rx) = std::sync::mpsc::sync_channel::<String>(PUSH_CHANNEL_CAP);
    *push_tx.lock_or_recover() = Some(tx);

    // Event poller for notifications
    let events_stop = Arc::new(AtomicBool::new(false));
    {
        let stop = Arc::clone(&events_stop);
        let socket = state.socket.clone();
        let push = Arc::clone(&push_tx);
        thread::spawn(move || {
            let mut last_ids: HashSet<String> = HashSet::new();
            while !stop.load(Ordering::Relaxed) {
                if let Ok(resp) = protocol::request(&socket, &Request::Events { limit: 50 }) {
                    if let Some(data) = resp.data {
                        if let Some(arr) = data
                            .get("events")
                            .and_then(|e| e.as_array())
                            .or_else(|| data.as_array())
                        {
                            for ev in arr {
                                // Prefer monotonic numeric id (daemon EventRecord.id).
                                let id = ev
                                    .get("id")
                                    .and_then(|v| {
                                        v.as_u64()
                                            .map(|n| n.to_string())
                                            .or_else(|| v.as_str().map(|s| s.to_string()))
                                    })
                                    .unwrap_or_else(|| {
                                        format!(
                                            "{}:{}:{}",
                                            ev.get("time").and_then(|t| t.as_u64()).unwrap_or(0),
                                            ev.get("kind").and_then(|k| k.as_str()).unwrap_or(""),
                                            ev.get("message")
                                                .and_then(|m| m.as_str())
                                                .unwrap_or("")
                                        )
                                    });
                                if !last_ids.insert(id.clone()) {
                                    continue;
                                }
                                let frame = json!({
                                    "type": "event",
                                    "category": "notification",
                                    "name": ev.get("kind").and_then(|k| k.as_str()).unwrap_or("notification"),
                                    "payload": ev,
                                });
                                if let Some(tx) = push.lock().ok().and_then(|g| g.clone()) {
                                    let _ = tx.try_send(frame.to_string());
                                }
                            }
                            if last_ids.len() > 500 {
                                last_ids.clear();
                            }
                        }
                    }
                }
                thread::sleep(Duration::from_millis(500));
            }
        });
    }

    loop {
        // Drain outbound pushes
        while let Ok(msg) = rx.try_recv() {
            if ws.send(Message::Text(msg)).is_err() {
                break;
            }
        }

        match ws.read() {
            Ok(Message::Text(text)) => {
                if !helloed {
                    if Instant::now() > hello_deadline {
                        let _ = ws.close(None);
                        break;
                    }
                    match serde_json::from_str::<Value>(&text) {
                        Ok(v) if v.get("deviceId").is_some() || v.get("device_id").is_some() => {
                            helloed = true;
                            // attach session — nothing more needed
                            continue;
                        }
                        _ => {
                            let _ = ws.close(None);
                            break;
                        }
                    }
                }

                let req: Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let id = req
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let method = req
                    .get("method")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let params = req.get("params").cloned().unwrap_or(json!({}));

                if method == "surface.subscribe" {
                    let workspace_id = params
                        .get("workspace_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let surface_id = params
                        .get("surface_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let lines = params
                        .get("lines")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(200)
                        .max(1) as usize;
                    // Colour is opt-in per subscription: clients that predate
                    // it (the cmux-remote app) keep receiving plain text.
                    let ansi = params
                        .get("ansi")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    let view_size = subscribe_view_size(&params, state.config.allow_view_resize);

                    // stop existing sub for this surface
                    if let Some(flag) = stop_flags.lock_or_recover().remove(&surface_id) {
                        flag.store(true, Ordering::Relaxed);
                    }
                    if let Some(handle) = active_subs.remove(&surface_id) {
                        let _ = handle.join();
                    }

                    let stop = Arc::new(AtomicBool::new(false));
                    stop_flags
                        .lock_or_recover()
                        .insert(surface_id.clone(), Arc::clone(&stop));
                    let socket = state.socket.clone();
                    let push = Arc::clone(&push_tx);
                    let fps = state.config.default_fps.max(1);
                    let idle_fps = state.config.idle_fps.max(1);
                    let sid = surface_id.clone();
                    let handle = thread::spawn(move || {
                        run_surface_poller(
                            socket,
                            workspace_id,
                            sid,
                            lines,
                            ansi,
                            view_size,
                            fps,
                            idle_fps,
                            stop,
                            push,
                        );
                    });
                    active_subs.insert(surface_id, handle);
                    let _ = ws.send(Message::Text(rpc_ok(&id, json!({})).to_string()));
                    continue;
                }

                if method == "surface.unsubscribe" {
                    let surface_id = params
                        .get("surface_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if let Some(flag) = stop_flags.lock_or_recover().remove(surface_id) {
                        flag.store(true, Ordering::Relaxed);
                    }
                    if let Some(handle) = active_subs.remove(surface_id) {
                        let _ = handle.join();
                    }
                    let _ = ws.send(Message::Text(rpc_ok(&id, json!({})).to_string()));
                    continue;
                }

                match dispatch_rpc(&state, &method, &params) {
                    Ok(result) => {
                        let _ = ws.send(Message::Text(rpc_ok(&id, result).to_string()));
                    }
                    Err(err) => {
                        let _ = ws.send(Message::Text(
                            rpc_err(&id, "internal_error", &err.to_string()).to_string(),
                        ));
                    }
                }
            }
            Ok(Message::Ping(data)) => {
                let _ = ws.send(Message::Pong(data));
            }
            Ok(Message::Close(_)) | Err(tungstenite::Error::ConnectionClosed) => break,
            Ok(Message::Binary(_)) | Ok(Message::Pong(_)) | Ok(Message::Frame(_)) => {}
            Err(tungstenite::Error::Io(ref e))
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                if !helloed && Instant::now() > hello_deadline {
                    let _ = ws.close(None);
                    break;
                }
                continue;
            }
            Err(_) => break,
        }
    }

    events_stop.store(true, Ordering::Relaxed);
    for (_, flag) in stop_flags.lock_or_recover().drain() {
        flag.store(true, Ordering::Relaxed);
    }
    for (_, handle) in active_subs.drain() {
        let _ = handle.join();
    }
    let _ = device_id;
    Ok(())
}

fn device_id_from_ws_headers(
    headers: &HashMap<String, String>,
    devices: &DeviceStore,
) -> Option<String> {
    if let Some(id) = auth_device_from_headers(headers, devices) {
        return Some(id);
    }
    if let Some(proto) = headers.get("sec-websocket-protocol") {
        for part in proto.split(',') {
            let part = part.trim();
            if let Some(token) = part.strip_prefix("bearer.") {
                if let Some(id) = devices.validate_token(token) {
                    return Some(id);
                }
            }
        }
    }
    None
}

fn ws_accept_key(key: &str) -> String {
    // RFC6455: base64(sha1(key + magic))
    // tungstenite uses sha1; we don't have sha1 crate. Implement via
    // a tiny pure approach: use tungstenite's handshake utility if available.
    // Fallback: compute with a minimal SHA-1 — pull from tungstenite internals.
    use sha1_compat::sha1_base64;
    sha1_base64(&(key.to_string() + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"))
}

/// Minimal SHA-1 + base64 for the WebSocket accept header only.
mod sha1_compat {
    pub fn sha1_base64(input: &str) -> String {
        let digest = sha1(input.as_bytes());
        base64_encode(&digest)
    }

    fn sha1(message: &[u8]) -> [u8; 20] {
        // Compact SHA-1 (public domain style).
        let mut h0: u32 = 0x67452301;
        let mut h1: u32 = 0xEFCDAB89;
        let mut h2: u32 = 0x98BADCFE;
        let mut h3: u32 = 0x10325476;
        let mut h4: u32 = 0xC3D2E1F0;

        let bit_len = (message.len() as u64) * 8;
        let mut msg = message.to_vec();
        msg.push(0x80);
        while (msg.len() % 64) != 56 {
            msg.push(0);
        }
        msg.extend_from_slice(&bit_len.to_be_bytes());

        for chunk in msg.chunks(64) {
            let mut w = [0u32; 80];
            for i in 0..16 {
                w[i] = u32::from_be_bytes([
                    chunk[i * 4],
                    chunk[i * 4 + 1],
                    chunk[i * 4 + 2],
                    chunk[i * 4 + 3],
                ]);
            }
            for i in 16..80 {
                w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
            }
            let (mut a, mut b, mut c, mut d, mut e) = (h0, h1, h2, h3, h4);
            #[allow(clippy::needless_range_loop)]
            for i in 0..80 {
                let (f, k) = match i {
                    0..=19 => ((b & c) | ((!b) & d), 0x5A827999),
                    20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                    40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                    _ => (b ^ c ^ d, 0xCA62C1D6),
                };
                let temp = a
                    .rotate_left(5)
                    .wrapping_add(f)
                    .wrapping_add(e)
                    .wrapping_add(k)
                    .wrapping_add(w[i]);
                e = d;
                d = c;
                c = b.rotate_left(30);
                b = a;
                a = temp;
            }
            h0 = h0.wrapping_add(a);
            h1 = h1.wrapping_add(b);
            h2 = h2.wrapping_add(c);
            h3 = h3.wrapping_add(d);
            h4 = h4.wrapping_add(e);
        }
        let mut out = [0u8; 20];
        out[0..4].copy_from_slice(&h0.to_be_bytes());
        out[4..8].copy_from_slice(&h1.to_be_bytes());
        out[8..12].copy_from_slice(&h2.to_be_bytes());
        out[12..16].copy_from_slice(&h3.to_be_bytes());
        out[16..20].copy_from_slice(&h4.to_be_bytes());
        out
    }

    fn base64_encode(data: &[u8]) -> String {
        const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in data.chunks(3) {
            let b0 = chunk[0] as u32;
            let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
            let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
            let n = (b0 << 16) | (b1 << 8) | b2;
            out.push(T[((n >> 18) & 63) as usize] as char);
            out.push(T[((n >> 12) & 63) as usize] as char);
            if chunk.len() > 1 {
                out.push(T[((n >> 6) & 63) as usize] as char);
            } else {
                out.push('=');
            }
            if chunk.len() > 2 {
                out.push(T[(n & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
        out
    }
}

fn rpc_ok(id: &str, result: Value) -> Value {
    json!({ "id": id, "ok": true, "result": result })
}

fn rpc_err(id: &str, code: &str, message: &str) -> Value {
    json!({
        "id": id,
        "ok": false,
        "error": { "code": code, "message": message }
    })
}

// ─── Diff poller ────────────────────────────────────────────────────────────

#[derive(Clone)]
struct ScreenSnap {
    rows: Vec<String>,
    cols: usize,
    cursor_x: i64,
    cursor_y: i64,
}

/// How long the daemon holds a phone-fit view override without hearing from
/// us again. Re-signed by the poller every `VIEW_RELEASE_EVERY`, so the pane
/// restores within seconds of the phone vanishing, while surviving ordinary
/// poll jitter comfortably.
const VIEW_LEASE_MS: u64 = 10_000;
const VIEW_RELEASE_EVERY: Duration = Duration::from_secs(2);

/// Phone-fit view size from `surface.subscribe` params (both `view_cols` and
/// `view_rows` must be present and non-zero) — but only when the host gate
/// `relay.allow_view_resize` is on. The gate defaults off: a phone opening a
/// pane must not resize it under the desktop user unless the host opted in.
/// With the gate off (or params absent) the client gets the previous
/// behaviour: full-width rows, wrapped client-side.
fn subscribe_view_size(params: &Value, allow_view_resize: bool) -> Option<(u16, u16)> {
    if !allow_view_resize {
        return None;
    }
    match (
        params.get("view_cols").and_then(|v| v.as_u64()),
        params.get("view_rows").and_then(|v| v.as_u64()),
    ) {
        (Some(cols), Some(rows)) if cols > 0 && rows > 0 => Some((
            cols.min(u16::MAX as u64) as u16,
            rows.min(u16::MAX as u64) as u16,
        )),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn run_surface_poller(
    socket: PathBuf,
    _workspace_id: String,
    surface_id: String,
    lines: usize,
    ansi: bool,
    view_size: Option<(u16, u16)>,
    active_fps: u32,
    idle_fps: u32,
    stop: Arc<AtomicBool>,
    push: Arc<Mutex<Option<std::sync::mpsc::SyncSender<String>>>>,
) {
    let mut rev: u64 = 0;
    let mut prev: Option<ScreenSnap> = None;
    let mut last_activity = Instant::now();
    let mut last_checksum = Instant::now() - Duration::from_secs(10);
    // Force an immediate first lease (before the first frame, so the phone's
    // first paint is already phone-sized).
    let mut last_lease = Instant::now() - 2 * VIEW_RELEASE_EVERY;

    while !stop.load(Ordering::Relaxed) {
        if let Some((cols, rows)) = view_size {
            if last_lease.elapsed() >= VIEW_RELEASE_EVERY {
                last_lease = Instant::now();
                // Best-effort: a zoomed pane refuses the override, and the
                // phone then simply sees the wrapped desktop-size screen.
                let _ = protocol::request(
                    &socket,
                    &Request::SetPaneViewSize {
                        pane: surface_id.clone(),
                        cols,
                        rows,
                        lease_ms: VIEW_LEASE_MS,
                    },
                );
            }
        }
        // Non-empty screen diffs bump last_activity so FPS returns to active.
        let current_fps = if last_activity.elapsed() > Duration::from_millis(1500) {
            idle_fps.max(1)
        } else {
            active_fps.max(1)
        };
        let interval = Duration::from_millis((1000 / current_fps as u64).max(20));

        match read_surface_screen(&socket, &surface_id, lines, ansi) {
            Ok(snap) => {
                let ops = if let Some(ref old) = prev {
                    diff_ops(old, &snap)
                } else {
                    let mut ops = vec![json!({"op":"clear"})];
                    for (i, row) in snap.rows.iter().enumerate() {
                        ops.push(json!({"op":"row","y": i, "text": row}));
                    }
                    ops.push(json!({
                        "op": "cursor",
                        "x": snap.cursor_x,
                        "y": snap.cursor_y
                    }));
                    ops
                };
                if !ops.is_empty() {
                    last_activity = Instant::now();
                    rev = rev.wrapping_add(1);
                    // First frame after subscribe: also send screen.full for clients
                    // that prefer a full snapshot.
                    if prev.is_none() {
                        let full = json!({
                            "type": "screen.full",
                            "surface_id": surface_id,
                            "rev": rev,
                            "rows": snap.rows,
                            "cols": snap.cols,
                            "rowsCount": snap.rows.len(),
                            "cursor": { "x": snap.cursor_x, "y": snap.cursor_y },
                        });
                        if let Some(tx) = push.lock().ok().and_then(|g| g.clone()) {
                            let _ = tx.try_send(full.to_string());
                        }
                    }
                    let frame = json!({
                        "type": "screen.diff",
                        "surface_id": surface_id,
                        "rev": rev,
                        "ops": ops,
                    });
                    if let Some(tx) = push.lock().ok().and_then(|g| g.clone()) {
                        let _ = tx.try_send(frame.to_string());
                    }
                    prev = Some(snap.clone());
                }

                if last_checksum.elapsed() >= Duration::from_secs(5) {
                    last_checksum = Instant::now();
                    let hash = screen_hash(&snap);
                    let frame = json!({
                        "type": "screen.checksum",
                        "surface_id": surface_id,
                        "rev": rev,
                        "hash": hash,
                    });
                    if let Some(tx) = push.lock().ok().and_then(|g| g.clone()) {
                        let _ = tx.try_send(frame.to_string());
                    }
                }
            }
            Err(err) => {
                eprintln!("relay poll {surface_id}: {err:#}");
            }
        }

        thread::sleep(interval);
    }

    // Explicit restore on the way out (unsubscribe and WS teardown both join
    // this thread). The lease above is the backstop for paths that never get
    // here — a killed relay process, a panicking poller.
    if view_size.is_some() {
        let _ = protocol::request(
            &socket,
            &Request::ClearPaneViewSize {
                pane: surface_id.clone(),
            },
        );
    }
}

fn diff_ops(old: &ScreenSnap, new: &ScreenSnap) -> Vec<Value> {
    let mut ops = Vec::new();
    if old.rows.len() != new.rows.len() {
        ops.push(json!({"op":"clear"}));
        for (i, row) in new.rows.iter().enumerate() {
            ops.push(json!({"op":"row","y": i, "text": row}));
        }
    } else {
        for (i, (a, b)) in old.rows.iter().zip(new.rows.iter()).enumerate() {
            if a != b {
                ops.push(json!({"op":"row","y": i, "text": b}));
            }
        }
    }
    if old.cursor_x != new.cursor_x || old.cursor_y != new.cursor_y {
        ops.push(json!({
            "op": "cursor",
            "x": new.cursor_x,
            "y": new.cursor_y
        }));
    }
    ops
}

fn screen_hash(screen: &ScreenSnap) -> String {
    let mut hasher = Sha256::new();
    for row in &screen.rows {
        hasher.update(row.as_bytes());
        hasher.update([0x0A]);
    }
    hasher.update([0xFF]);
    hasher.update(screen.cursor_x.to_le_bytes());
    hasher.update(screen.cursor_y.to_le_bytes());
    let dig = hasher.finalize();
    dig.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

fn read_surface_screen(
    socket: &Path,
    surface_id: &str,
    lines: usize,
    ansi: bool,
) -> Result<ScreenSnap> {
    let resp = protocol::request(
        socket,
        &Request::ReadScreen {
            pane: Some(surface_id.to_string()),
            scrollback: false,
            limit_bytes: Some(256_000),
            ansi,
        },
    )?;
    if !resp.ok {
        bail!(resp.error.unwrap_or_else(|| "read-screen failed".into()));
    }
    let data = resp.data.unwrap_or(Value::Null);
    let text = data
        .get("screen")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let cols = data.get("cols").and_then(|v| v.as_u64()).unwrap_or(80) as usize;
    let rows_n = data.get("rows").and_then(|v| v.as_u64()).unwrap_or(24) as usize;

    // Cursor comes from ReadScreen itself (no second full List snapshot).
    let (cx, cy) = pane_cursor_from_read(&data).unwrap_or((0, 0));

    let mut rows: Vec<String> = text.split('\n').map(|s| s.to_string()).collect();
    if rows.len() > lines {
        rows = rows.split_off(rows.len() - lines);
    }
    // Pad to terminal rows so mobile view is stable.
    while rows.len() < rows_n.min(lines) {
        rows.push(String::new());
    }
    Ok(ScreenSnap {
        rows,
        cols,
        cursor_x: cx,
        cursor_y: cy,
    })
}

/// Replay a pane's raw output ring through a fresh vt100 parser and return
/// the history rows that have scrolled off the top of the live screen,
/// oldest first. The raw ring contains the pane's original escape stream, so
/// with `ansi` the rows come back coloured — even when the daemon serving the
/// ring predates colour support (the replay happens here, in the relay).
///
/// The ring is byte-capped, so its start may land mid-escape; vt100 discards
/// partial sequences, costing at most one garbled leading row.
fn replay_scrollback(raw: &str, rows: u16, cols: u16, lines: usize, ansi: bool) -> Vec<String> {
    let mut parser = vt100::Parser::new(rows.max(1), cols.max(1), lines);
    parser.process(raw.as_bytes());

    // set_scrollback clamps to what is actually retained, which is how we
    // learn how much history exists.
    parser.screen_mut().set_scrollback(usize::MAX);
    let available = parser.screen().scrollback();

    let mut out = Vec::with_capacity(available);
    // Offset N puts the N-th line above the live screen at visible row 0.
    for offset in (1..=available).rev() {
        parser.screen_mut().set_scrollback(offset);
        let row = if ansi {
            crate::daemon::row_contents_ansi(parser.screen(), 0)
        } else {
            let mut plain = String::new();
            let (_, c) = parser.screen().size();
            for col in 0..c {
                let Some(cell) = parser.screen().cell(0, col) else {
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
        out.push(row.trim_end().to_string());
    }
    out
}

/// `surface.scrollback` — the pane history above the live screen.
fn surface_scrollback(socket: &Path, surface: &str, lines: usize, ansi: bool) -> Result<Value> {
    let resp = call(
        socket,
        &Request::ReadScreen {
            pane: Some(surface.to_string()),
            scrollback: true,
            limit_bytes: Some(512_000),
            ansi: false,
        },
    )?;
    let data = resp.data.unwrap_or(Value::Null);
    let raw = data
        .get("scrollback")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let cols = data.get("cols").and_then(|v| v.as_u64()).unwrap_or(80) as u16;
    let rows_n = data.get("rows").and_then(|v| v.as_u64()).unwrap_or(24) as u16;
    let history = replay_scrollback(raw, rows_n, cols, lines, ansi);
    Ok(json!({
        "surface_id": surface,
        "count": history.len(),
        "rows": history,
    }))
}

fn pane_cursor_from_read(data: &Value) -> Option<(i64, i64)> {
    let col = data.get("cursor_col")?.as_u64()? as i64;
    let row = data.get("cursor_row")?.as_u64()? as i64;
    Some((col, row))
}

// ─── RPC dispatch (cmux-remote methods → vmux) ──────────────────────────────

fn dispatch_rpc(state: &RelayState, method: &str, params: &Value) -> Result<Value> {
    let socket = &state.socket;
    match method {
        "workspace.list" => workspace_list(socket),
        "workspace.create" => {
            let title = params
                .get("title")
                .or_else(|| params.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("workspace");
            workspace_create(socket, title)
        }
        "workspace.rename" => {
            let id = req_str(params, "workspace_id")?;
            let title = params
                .get("title")
                .or_else(|| params.get("name"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("title required"))?;
            call(
                socket,
                &Request::RenameWorkspace {
                    workspace: id,
                    name: title.to_string(),
                },
            )?;
            Ok(json!({}))
        }
        "workspace.close" => {
            let id = req_str(params, "workspace_id")?;
            call(
                socket,
                &Request::CloseWorkspace {
                    workspace: Some(id),
                },
            )?;
            Ok(json!({}))
        }
        "workspace.select" => {
            let id = req_str(params, "workspace_id")?;
            call(socket, &Request::SwitchWorkspace { workspace: id })?;
            Ok(json!({}))
        }
        "surface.list" => {
            let ws = req_str(params, "workspace_id")?;
            surface_list(socket, &ws)
        }
        "surface.create" => {
            let ws = req_str(params, "workspace_id")?;
            surface_create(socket, &ws)
        }
        "surface.close" => {
            let surface = req_str(params, "surface_id")?;
            call(
                socket,
                &Request::KillPane {
                    pane: Some(surface),
                },
            )?;
            Ok(json!({}))
        }
        "surface.focus" => {
            let surface = req_str(params, "surface_id")?;
            focus_surface(socket, &surface)?;
            Ok(json!({}))
        }
        "surface.send_text" => {
            let surface = req_str(params, "surface_id")?;
            let text = params
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let _ = call(
                socket,
                &Request::FocusPane {
                    pane: surface.clone(),
                },
            );
            call(
                socket,
                &Request::Input {
                    pane: Some(surface),
                    data: text,
                },
            )?;
            Ok(json!({}))
        }
        "surface.send_key" => {
            let surface = req_str(params, "surface_id")?;
            let key = params
                .get("key")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let mapped = map_cmux_key(&key);
            let _ = call(
                socket,
                &Request::FocusPane {
                    pane: surface.clone(),
                },
            );
            call(
                socket,
                &Request::SendKey {
                    pane: Some(surface),
                    keys: vec![mapped],
                },
            )?;
            Ok(json!({}))
        }
        "surface.scrollback" => {
            let surface = req_str(params, "surface_id")?;
            let lines = params
                .get("lines")
                .and_then(|v| v.as_u64())
                .unwrap_or(500)
                .clamp(1, 2000) as usize;
            let ansi = params
                .get("ansi")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            surface_scrollback(socket, &surface, lines, ansi)
        }
        "surface.read_text" => {
            let surface = req_str(params, "surface_id")?;
            let lines = params.get("lines").and_then(|v| v.as_u64()).unwrap_or(200) as usize;
            let snap = read_surface_screen(socket, &surface, lines, false)?;
            Ok(json!({
                "text": snap.rows.join("\n"),
                "surface_id": surface,
            }))
        }
        "host.battery" => Ok(json!({
            "available": false,
            "level": null,
            "charging": false,
            "source": "vmux"
        })),
        "file.upload" => file_upload(params),
        "notification.create" => {
            let workspace = params
                .get("workspace_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let surface = params
                .get("surface_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let title = params
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("notification");
            let body = params.get("body").and_then(|v| v.as_str()).unwrap_or("");
            let message = if body.is_empty() {
                title.to_string()
            } else {
                format!("{title}: {body}")
            };
            call(
                socket,
                &Request::Notify {
                    pane: surface,
                    workspace,
                    status: Some("attention".into()),
                    color: None,
                    clear: false,
                    message,
                },
            )?;
            Ok(json!({}))
        }
        other => bail!("unsupported method {other}"),
    }
}

fn call(socket: &Path, req: &Request) -> Result<Response> {
    let resp = protocol::request(socket, req)?;
    if !resp.ok {
        bail!(resp.error.unwrap_or_else(|| "request failed".into()));
    }
    Ok(resp)
}

fn req_str(params: &Value, key: &str) -> Result<String> {
    params
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("{key} required"))
}

fn workspace_list(socket: &Path) -> Result<Value> {
    let resp = call(socket, &Request::List)?;
    let data = resp.data.unwrap_or(Value::Null);
    let workspaces = data
        .get("workspaces")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for (index, ws) in workspaces.iter().enumerate() {
        let id = ws
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let title = ws
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or(&id)
            .to_string();
        // Surface agent needs-input on workspace for Inbox promotion.
        let mut obj = json!({
            "id": id,
            "title": title,
            "name": title,
            "index": index,
        });
        if let Some(map) = obj.as_object_mut() {
            if let Some(active) = ws.get("active_pane").and_then(|v| v.as_str()) {
                map.insert("active_surface_id".into(), json!(active));
            }
            // Promote agent status from panes
            if let Some(status) = workspace_attention_status(&data, ws) {
                map.insert("agent_status".into(), json!(status));
                if status == "attention" || status == "needs-input" {
                    map.insert(
                        "needs_input_message".into(),
                        json!(format!("agent in {title} needs input")),
                    );
                }
            }
        }
        out.push(obj);
    }
    Ok(json!({ "workspaces": out }))
}

fn workspace_attention_status(snapshot: &Value, ws: &Value) -> Option<String> {
    let panes = snapshot.get("panes")?.as_object()?;
    // workspace.panes mirrors only the ACTIVE tab; an agent on another tab
    // must still be able to summon a human, so scan every tab's panes.
    let mut pane_ids: Vec<Value> = Vec::new();
    if let Some(tabs) = ws.get("tabs").and_then(|t| t.as_array()) {
        for tab in tabs {
            if let Some(arr) = tab.get("panes").and_then(|p| p.as_array()) {
                pane_ids.extend(arr.iter().cloned());
            }
        }
    }
    if pane_ids.is_empty() {
        pane_ids = ws.get("panes")?.as_array()?.clone();
    }
    for pid in &pane_ids {
        let id = pid.as_str()?;
        if let Some(p) = panes.get(id) {
            let status = p.get("agent_status").and_then(|v| v.as_str()).unwrap_or("");
            if status == "attention" || status == "needs-input" || status == "needs_input" {
                return Some("attention".into());
            }
        }
    }
    None
}

fn workspace_create(socket: &Path, title: &str) -> Result<Value> {
    let resp = call(
        socket,
        &Request::NewWorkspace {
            name: title.to_string(),
            cwd: None,
        },
    )?;
    // Response may be the workspace object or wrap it.
    let data = resp.data.unwrap_or(Value::Null);
    let id = data
        .get("id")
        .or_else(|| data.get("workspace").and_then(|w| w.get("id")))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| title.to_string());
    // Ensure at least one shell surface.
    let _ = call(
        socket,
        &Request::NewPane {
            direction: SplitDirection::Right,
            command: String::new(),
            title: Some("shell".into()),
            workspace: Some(id.clone()),
            surface_kind: None,
        },
    );
    // Re-list to get index
    let list = workspace_list(socket)?;
    if let Some(arr) = list.get("workspaces").and_then(|v| v.as_array()) {
        if let Some(ws) = arr
            .iter()
            .find(|w| w.get("id").and_then(|i| i.as_str()) == Some(&id))
        {
            return Ok(ws.clone());
        }
    }
    Ok(json!({ "id": id, "title": title, "index": 0 }))
}

fn surface_list(socket: &Path, workspace_id: &str) -> Result<Value> {
    let resp = call(socket, &Request::List)?;
    let data = resp.data.unwrap_or(Value::Null);
    let workspaces = data
        .get("workspaces")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let panes_map = data
        .get("panes")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();

    let ws = workspaces
        .iter()
        .find(|w| w.get("id").and_then(|i| i.as_str()) == Some(workspace_id))
        .ok_or_else(|| anyhow!("workspace not found"))?;

    // Every tab's panes, in tab order — a phone must be able to reach all of a
    // workspace, not just its active tab. Each surface carries `tab`/`tab_id`
    // (additive: clients that predate them ignore unknown fields).
    let mut pane_ids: Vec<(String, Option<(String, String)>)> = Vec::new();
    if let Some(tabs) = ws.get("tabs").and_then(|t| t.as_array()) {
        for tab in tabs {
            let tab_meta = tab.get("id").and_then(|i| i.as_str()).map(|id| {
                (
                    id.to_string(),
                    tab.get("title")
                        .and_then(|t| t.as_str())
                        .unwrap_or(id)
                        .to_string(),
                )
            });
            if let Some(arr) = tab.get("panes").and_then(|p| p.as_array()) {
                for p in arr {
                    if let Some(id) = p.as_str() {
                        pane_ids.push((id.to_string(), tab_meta.clone()));
                    }
                }
            }
        }
    }
    if pane_ids.is_empty() {
        if let Some(arr) = ws.get("panes").and_then(|p| p.as_array()) {
            for p in arr {
                if let Some(id) = p.as_str() {
                    pane_ids.push((id.to_string(), None));
                }
            }
        }
    }

    let mut surfaces = Vec::new();
    for (index, (pid, tab_meta)) in pane_ids.iter().enumerate() {
        let title = panes_map
            .get(pid)
            .and_then(|p| p.get("title"))
            .and_then(|t| t.as_str())
            .unwrap_or(pid)
            .to_string();
        let mut surface = json!({
            "id": pid,
            "title": title,
            "index": index,
            "type": "terminal",
            "focused": ws.get("active_pane").and_then(|v| v.as_str()) == Some(pid.as_str()),
        });
        if let (Some((tab_id, tab_title)), Some(map)) = (tab_meta, surface.as_object_mut()) {
            map.insert("tab_id".into(), json!(tab_id));
            map.insert("tab".into(), json!(tab_title));
        }
        surfaces.push(surface);
    }
    Ok(json!({ "surfaces": surfaces, "workspace_id": workspace_id }))
}

/// Focus a pane wherever it lives: FocusPane only accepts panes on the active
/// tab (workspace.panes mirrors just that tab), so find the pane's tab and
/// switch to it first. Tapping a pane on the phone means "show me this pane",
/// which is a tab switch anyway.
fn focus_surface(socket: &Path, surface: &str) -> Result<()> {
    let resp = call(socket, &Request::List)?;
    let data = resp.data.unwrap_or(Value::Null);
    if let Some(workspaces) = data.get("workspaces").and_then(|v| v.as_array()) {
        'search: for ws in workspaces {
            let Some(tabs) = ws.get("tabs").and_then(|t| t.as_array()) else {
                continue;
            };
            for tab in tabs {
                let holds_pane = tab
                    .get("panes")
                    .and_then(|p| p.as_array())
                    .map(|arr| arr.iter().any(|p| p.as_str() == Some(surface)))
                    .unwrap_or(false);
                if !holds_pane {
                    continue;
                }
                let ws_id = ws.get("id").and_then(|i| i.as_str()).map(String::from);
                let active_tab = ws.get("active_tab").and_then(|v| v.as_str());
                if let Some(tab_id) = tab.get("id").and_then(|i| i.as_str()) {
                    if active_tab != Some(tab_id) {
                        call(
                            socket,
                            &Request::SwitchTab {
                                workspace: ws_id,
                                tab: tab_id.to_string(),
                            },
                        )?;
                    }
                }
                break 'search;
            }
        }
    }
    call(
        socket,
        &Request::FocusPane {
            pane: surface.to_string(),
        },
    )?;
    Ok(())
}

fn surface_create(socket: &Path, workspace_id: &str) -> Result<Value> {
    let resp = call(
        socket,
        &Request::NewPane {
            direction: SplitDirection::Right,
            command: String::new(),
            title: Some("shell".into()),
            workspace: Some(workspace_id.to_string()),
            surface_kind: None,
        },
    )?;
    let data = resp.data.unwrap_or(Value::Null);
    // NewPane may return pane id in various shapes.
    let surface_id = data
        .get("id")
        .or_else(|| data.get("pane"))
        .or_else(|| data.get("pane_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            // Fall back: list and pick newest in workspace.
            surface_list(socket, workspace_id).ok().and_then(|list| {
                list.get("surfaces")
                    .and_then(|s| s.as_array())
                    .and_then(|a| a.last())
                    .and_then(|s| s.get("id"))
                    .and_then(|i| i.as_str())
                    .map(|s| s.to_string())
            })
        })
        .unwrap_or_else(|| "pane-unknown".into());
    Ok(json!({ "surface_id": surface_id, "id": surface_id }))
}

fn map_cmux_key(key: &str) -> String {
    // cmux KeyEncoder: "ctrl+c", "enter", "pgup", …
    // vmux accepts the same style with + or -.
    let lower = key.to_ascii_lowercase();
    match lower.as_str() {
        "return" => "enter".into(),
        "escape" => "esc".into(),
        "pgup" => "pgup".into(),
        "pgdn" => "pgdn".into(),
        "cmd+c" => "ctrl+c".into(), // mac-ism
        other => other.to_string(),
    }
}

fn file_upload(params: &Value) -> Result<Value> {
    let name = params
        .get("filename")
        .or_else(|| params.get("name"))
        .and_then(|v| v.as_str())
        .unwrap_or("upload.bin");
    let b64 = params
        .get("data")
        .or_else(|| params.get("base64"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("data required"))?;
    let bytes = decode_base64(b64)?;
    let (path, safe) = save_upload(name, &bytes)?;
    Ok(json!({
        "path": path.display().to_string(),
        "filename": safe,
    }))
}

/// Save uploaded bytes under ~/Downloads/vmux-remote with a sanitized,
/// collision-proof name. Returns (path, sanitized filename).
fn save_upload(name: &str, bytes: &[u8]) -> Result<(PathBuf, String)> {
    let dir = dirs::download_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("vmux-remote");
    fs::create_dir_all(&dir)?;
    // Sanitize filename
    let safe: String = Path::new(name)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("upload.bin")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    // Unique name + create_new so same-second uploads never clobber.
    for _ in 0..8 {
        let path = dir.join(format!("{}-{}-{}", now_secs(), random_hex(8), safe));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut f) => {
                use std::io::Write;
                f.write_all(bytes)?;
                return Ok((path, safe));
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err.into()),
        }
    }
    anyhow::bail!("could not create unique upload path");
}

/// POST /v1/paste — raw image bytes in the body (from the /paste browser
/// page). Saves the image and types its path into the target pane (the
/// active pane unless `?pane=` is given), which agents like Claude Code pick
/// up as an image attachment. `?enter=1` submits with a carriage return.
fn paste_upload(state: &RelayState, target: &str, body: &[u8]) -> Result<Value> {
    let ext = crate::image_extension(body)
        .ok_or_else(|| anyhow!("body is not an image (expected png, jpeg, gif, webp, or bmp)"))?;
    let query = parse_query(target);
    let pane = query.get("pane").cloned();
    let enter = matches!(
        query.get("enter").map(String::as_str),
        Some("1") | Some("true")
    );
    let (path, _) = save_upload(&format!("paste.{ext}"), body)?;
    let mut data = format!("{} ", path.display());
    if enter {
        data.push('\r');
    }
    call(&state.socket, &Request::Input { pane, data })?;
    Ok(json!({
        "path": path.display().to_string(),
        "bytes": body.len(),
        "enter": enter,
    }))
}

/// Minimal query-string parser: `/v1/paste?pane=pane-2&enter=1` →
/// {pane: "pane-2", enter: "1"}. Values are used as-is (pane ids and flags
/// are plain ASCII; no percent-decoding needed).
fn parse_query(target: &str) -> HashMap<String, String> {
    target
        .split_once('?')
        .map(|(_, q)| q)
        .unwrap_or("")
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn decode_base64(input: &str) -> Result<Vec<u8>> {
    // Minimal base64 decoder (std only).
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            b'=' => Some(0),
            _ => None,
        }
    }
    let cleaned: Vec<u8> = input.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if !cleaned.len().is_multiple_of(4) {
        bail!("invalid base64 length");
    }
    let mut out = Vec::with_capacity(cleaned.len() / 4 * 3);
    for chunk in cleaned.chunks(4) {
        let (a, b, c, d) = (
            val(chunk[0]).ok_or_else(|| anyhow!("bad base64"))?,
            val(chunk[1]).ok_or_else(|| anyhow!("bad base64"))?,
            val(chunk[2]).ok_or_else(|| anyhow!("bad base64"))?,
            val(chunk[3]).ok_or_else(|| anyhow!("bad base64"))?,
        );
        let n = ((a as u32) << 18) | ((b as u32) << 12) | ((c as u32) << 6) | (d as u32);
        out.push(((n >> 16) & 0xFF) as u8);
        if chunk[2] != b'=' {
            out.push(((n >> 8) & 0xFF) as u8);
        }
        if chunk[3] != b'=' {
            out.push((n & 0xFF) as u8);
        }
    }
    Ok(out)
}

// ─── Helpers ────────────────────────────────────────────────────────────────

fn sha256_hex(s: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn ct_eq_str(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.as_bytes()
        .iter()
        .zip(b.as_bytes().iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

fn random_hex(bytes: usize) -> String {
    // These bytes back device tokens and boot_id, so they MUST be
    // cryptographically random. getrandom draws from the OS CSPRNG
    // (getrandom(2) / /dev/urandom) and fails only in a broken environment; a
    // predictable fallback (the old time-seeded LCG) would make tokens
    // guessable, so fail closed instead of issuing weak secrets.
    let mut buf = vec![0u8; bytes];
    getrandom::getrandom(&mut buf).expect("OS CSPRNG unavailable; refusing to issue weak tokens");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn port_of(listen: &str) -> Option<u16> {
    listen
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse().ok())
        .or_else(|| {
            // bare port
            listen.parse().ok()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_scrollback_returns_history_above_the_screen_oldest_first() {
        // 30 numbered lines into a 5-row screen: 25 scroll off the top.
        let mut raw = String::new();
        for i in 1..=30 {
            raw.push_str(&format!("line-{i}\r\n"));
        }
        let history = replay_scrollback(&raw, 5, 20, 100, false);
        assert!(!history.is_empty(), "no history came back");
        assert_eq!(history.first().map(String::as_str), Some("line-1"));
        // The live screen's contents must NOT be in the history.
        assert!(
            !history.iter().any(|r| r == "line-30"),
            "history leaked the live screen"
        );
        // Contiguous, ordered.
        // The trailing newline parks the cursor on a fresh row, so the screen
        // holds lines 27-30 plus the cursor row — 26 lines are history.
        assert_eq!(history.last().map(String::as_str), Some("line-26"));
        assert_eq!(history.len(), 26);
    }

    #[test]
    fn replay_scrollback_preserves_colour_when_asked() {
        let raw = format!(
            "{}coloured-history\r\n{}",
            "\x1b[31m",
            "filler\r\n".repeat(10)
        );
        let history = replay_scrollback(&raw, 3, 30, 100, true);
        let hit = history
            .iter()
            .find(|r| r.contains("coloured-history"))
            .expect("history line missing");
        assert!(hit.contains("\x1b[0;31m"), "colour lost: {hit:?}");
        // And plain mode strips it.
        let plain = replay_scrollback(&raw, 3, 30, 100, false);
        let hit = plain
            .iter()
            .find(|r| r.contains("coloured-history"))
            .expect("history line missing");
        assert!(!hit.contains('\x1b'), "plain mode leaked escapes: {hit:?}");
    }

    #[test]
    fn replay_scrollback_caps_at_requested_lines() {
        let mut raw = String::new();
        for i in 1..=200 {
            raw.push_str(&format!("l{i}\r\n"));
        }
        let history = replay_scrollback(&raw, 5, 10, 50, false);
        assert!(history.len() <= 50, "cap ignored: {}", history.len());
        // The retained window is the NEWEST history, ending just above screen.
        assert_eq!(history.last().map(String::as_str), Some("l196"));
    }

    // ─── Device registration policy ─────────────────────────────────────────
    //
    // `relay/auth.rs` is the relay's only network-facing security surface, and
    // it had no tests at all. Both of the behaviours pinned below are *already
    // fixed bugs* (the bootstrap-secret gate, and the localhost device-id
    // collision) that nothing stopped from silently regressing.

    fn auth_state(mutate: impl FnOnce(&mut RelayConfig)) -> RelayState {
        let mut config = RelayConfig {
            allow_localhost: true,
            ..RelayConfig::default()
        };
        mutate(&mut config);
        let devices = std::env::temp_dir().join(format!(
            "vmux-authtest-{}-{}.json",
            std::process::id(),
            random_hex(8)
        ));
        RelayState {
            config,
            devices: DeviceStore::load(devices).unwrap(),
            socket: PathBuf::from("/nonexistent/vmux-authtest.sock"),
            boot_id: "test-boot".into(),
            started_at: 0,
            running: AtomicBool::new(true),
            active_connections: AtomicUsize::new(0),
        }
    }

    fn headers(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn is_forbidden<T>(result: &std::result::Result<T, RegisterError>) -> bool {
        matches!(result, Err(RegisterError::Forbidden(_)))
    }

    #[test]
    fn bootstrap_secret_is_required_on_every_registration_path() {
        // The policy: a configured non-empty bootstrap secret
        // gates *every* path — including localhost, which is otherwise admitted.
        let state = auth_state(|c| c.bootstrap_secret = Some("s3cret".into()));

        assert!(
            is_forbidden(&register_device(&state, "127.0.0.1", &headers(&[]))),
            "localhost must not bypass a configured bootstrap secret"
        );
        assert!(
            is_forbidden(&register_device(
                &state,
                "127.0.0.1",
                &headers(&[("x-vmux-bootstrap", "wrong")])
            )),
            "a wrong secret must be refused"
        );
        assert!(
            register_device(
                &state,
                "127.0.0.1",
                &headers(&[("x-vmux-bootstrap", "s3cret")])
            )
            .is_ok(),
            "the correct secret must be accepted"
        );
    }

    #[test]
    fn bootstrap_header_accepts_both_header_forms_and_rejects_near_misses() {
        let state = auth_state(|c| c.bootstrap_secret = Some("s3cret".into()));
        assert!(bootstrap_header_matches(
            &state,
            &headers(&[("x-vmux-bootstrap", "s3cret")])
        ));
        assert!(bootstrap_header_matches(
            &state,
            &headers(&[("authorization", "Bootstrap s3cret")])
        ));
        // A prefix must not pass: the digest compare exists precisely so length
        // and timing leak nothing.
        assert!(!bootstrap_header_matches(
            &state,
            &headers(&[("x-vmux-bootstrap", "s3cre")])
        ));
        assert!(!bootstrap_header_matches(&state, &headers(&[])));

        // An *empty* configured secret is not a secret — it must not admit a
        // caller who also sends nothing.
        let empty = auth_state(|c| c.bootstrap_secret = Some(String::new()));
        assert!(!bootstrap_header_matches(&empty, &headers(&[])));
        assert!(!bootstrap_header_matches(
            &empty,
            &headers(&[("x-vmux-bootstrap", "")])
        ));
    }

    #[test]
    fn localhost_registration_honours_allow_localhost() {
        let denied = auth_state(|c| c.allow_localhost = false);
        assert!(is_forbidden(&register_device(
            &denied,
            "127.0.0.1",
            &headers(&[])
        )));

        let allowed = auth_state(|c| c.allow_localhost = true);
        assert!(register_device(&allowed, "127.0.0.1", &headers(&[])).is_ok());
    }

    #[test]
    fn each_localhost_pairing_is_a_distinct_device() {
        // Loopback has no stable node identity, so a fixed node_key would hash
        // to ONE device_id for every local client and each new pairing would
        // overwrite the previous device's token. Two pairings, two devices.
        let state = auth_state(|c| c.allow_localhost = true);
        let (first_id, first_token) = register_device(&state, "127.0.0.1", &headers(&[])).unwrap();
        let (second_id, second_token) =
            register_device(&state, "127.0.0.1", &headers(&[])).unwrap();

        assert_ne!(first_id, second_id, "each pairing must be its own device");
        assert_ne!(first_token, second_token);
        // The first device's token must still resolve to the first device — the
        // second pairing did not clobber it.
        assert_eq!(
            state.devices.validate_token(&first_token).as_deref(),
            Some(first_id.as_str())
        );
        assert_eq!(
            state.devices.validate_token(&second_token).as_deref(),
            Some(second_id.as_str())
        );
    }

    #[test]
    fn cgnat_peer_needs_the_opt_in_and_cannot_skip_allow_login() {
        // A CGNAT peer with no whois identity: refused unless explicitly opted
        // in, and refused even then when allow_login is set (whois is the only
        // thing that can prove a login, so the fallback must not stand in).
        let off = auth_state(|c| c.allow_tailnet_cgnat = false);
        assert!(is_forbidden(&register_device(
            &off,
            "100.64.0.1",
            &headers(&[])
        )));

        let on_with_allowlist = auth_state(|c| {
            c.allow_tailnet_cgnat = true;
            c.allow_login = vec!["someone@example.com".into()];
        });
        assert!(is_forbidden(&register_device(
            &on_with_allowlist,
            "100.64.0.1",
            &headers(&[])
        )));
    }

    #[test]
    fn view_resize_is_gated_off_by_default() {
        // The host gate defaults off — including for relay.json files written
        // before the field existed.
        assert!(!RelayConfig::default().allow_view_resize);
        let old: RelayConfig = serde_json::from_str(r#"{"listen":"127.0.0.1:4399"}"#).unwrap();
        assert!(!old.allow_view_resize);

        // A subscribe carrying view params must NOT produce an override while
        // the gate is off; the phone falls back to client-side wrapping. The
        // poller only leases SetPaneViewSize when this returns Some, so None
        // here is what guarantees the daemon never sees an override.
        let params = json!({"surface_id": "pane-1", "view_cols": 46, "view_rows": 22});
        assert_eq!(subscribe_view_size(&params, false), None);

        // Flipping the gate (vmux config set relay.allow_view_resize true)
        // makes the same subscribe take effect.
        assert_eq!(subscribe_view_size(&params, true), Some((46, 22)));

        // Gate on but params absent/degenerate: still no override.
        assert_eq!(subscribe_view_size(&json!({"surface_id": "p"}), true), None);
        assert_eq!(
            subscribe_view_size(&json!({"view_cols": 0, "view_rows": 22}), true),
            None
        );
    }

    #[test]
    fn relay_config_without_allow_paste_defaults_on() {
        // Configs written before the paste page existed must keep serving it
        // (serde default), and an explicit false must stick.
        let old: RelayConfig = serde_json::from_str(r#"{"listen":"127.0.0.1:4399"}"#).unwrap();
        assert!(old.allow_paste);
        let off: RelayConfig =
            serde_json::from_str(r#"{"listen":"127.0.0.1:4399","allow_paste":false}"#).unwrap();
        assert!(!off.allow_paste);
    }

    #[test]
    fn parse_query_extracts_pane_and_enter() {
        let q = parse_query("/v1/paste?pane=pane-2&enter=1");
        assert_eq!(q.get("pane").map(String::as_str), Some("pane-2"));
        assert_eq!(q.get("enter").map(String::as_str), Some("1"));
        assert!(parse_query("/v1/paste").is_empty());
        assert!(parse_query("/v1/paste?").is_empty());
    }

    #[test]
    fn paste_page_is_self_contained_and_templated() {
        assert!(PASTE_PAGE.contains("{{SESSION}}"));
        assert!(PASTE_PAGE.contains("/v1/paste"));
        assert!(PASTE_PAGE.contains("/v1/devices/me/register"));
        // CSP-free page must not pull remote assets.
        assert!(!PASTE_PAGE.contains("https://"));
        assert!(!PASTE_PAGE.contains("http://"));
    }

    #[test]
    fn save_upload_sanitizes_and_never_clobbers() {
        let (a, safe) = save_upload("../../etc/pass wd.png", b"x").unwrap();
        assert_eq!(safe, "pass_wd.png");
        let (b, _) = save_upload("../../etc/pass wd.png", b"y").unwrap();
        assert_ne!(a, b);
        assert_eq!(fs::read(&a).unwrap(), b"x");
        fs::remove_file(&a).ok();
        fs::remove_file(&b).ok();
    }

    #[test]
    fn screen_hash_stable() {
        let s = ScreenSnap {
            rows: vec!["hello".into(), "world".into()],
            cols: 80,
            cursor_x: 1,
            cursor_y: 2,
        };
        let a = screen_hash(&s);
        let b = screen_hash(&s);
        assert_eq!(a, b);
        assert_eq!(a.len(), 16);
    }

    #[test]
    fn diff_detects_row_change() {
        let a = ScreenSnap {
            rows: vec!["a".into(), "b".into()],
            cols: 10,
            cursor_x: 0,
            cursor_y: 0,
        };
        let b = ScreenSnap {
            rows: vec!["a".into(), "c".into()],
            cols: 10,
            cursor_x: 0,
            cursor_y: 0,
        };
        let ops = diff_ops(&a, &b);
        assert!(ops.iter().any(|o| o.get("op") == Some(&json!("row"))));
    }

    #[test]
    fn map_keys() {
        assert_eq!(map_cmux_key("enter"), "enter");
        assert_eq!(map_cmux_key("ctrl+c"), "ctrl+c");
        assert_eq!(map_cmux_key("pgup"), "pgup");
    }

    #[test]
    fn tailscale_cgnat() {
        use std::net::{IpAddr, Ipv4Addr};
        assert!(is_tailscale_cgnat(IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1))));
        assert!(is_tailscale_cgnat(IpAddr::V4(Ipv4Addr::new(
            100, 127, 1, 1
        ))));
        assert!(!is_tailscale_cgnat(IpAddr::V4(Ipv4Addr::new(
            100, 63, 0, 1
        ))));
        assert!(!is_tailscale_cgnat(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
    }

    #[test]
    fn refuses_all_interfaces_listen() {
        assert!(assert_safe_listen("0.0.0.0:4399").is_err());
        assert!(assert_safe_listen("[::]:4399").is_err());
        assert!(assert_safe_listen("127.0.0.1:4399").is_ok());
        assert!(assert_safe_listen("100.92.56.118:4399").is_ok());
    }

    #[test]
    fn resolve_listen_never_all_interfaces() {
        let local = RelaySettings {
            enabled: true,
            bind: "local".into(),
            port: 4399,
            allow_localhost: false,
            allow_tailnet_cgnat: true,
            allow_paste: true,
            allow_view_resize: false,
        };
        assert_eq!(resolve_listen(&local), "127.0.0.1:4399");
        let all_migrated = RelaySettings {
            bind: "all".into(),
            ..local.clone()
        };
        // Unknown/removed modes fall through to auto → never 0.0.0.0
        assert!(!resolve_listen(&all_migrated).starts_with("0.0.0.0"));
    }

    #[test]
    fn base64_roundtrip_small() {
        // "hi" = aGk=
        let decoded = decode_base64("aGk=").unwrap();
        assert_eq!(decoded, b"hi");
    }
}
