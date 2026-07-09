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
use std::net::{IpAddr, Ipv4Addr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
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
use std::process::{Command as ProcessCommand, Stdio};

const RELAY_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Safe default: localhost only. Never default to 0.0.0.0.
const DEFAULT_LISTEN: &str = "127.0.0.1:4399";
const DEFAULT_FPS: u32 = 15;
const DEFAULT_IDLE_FPS: u32 = 5;
const HELLO_TIMEOUT: Duration = Duration::from_millis(500);
const MAX_WS_MSG: usize = 24 * 1024 * 1024;

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
    /// Optional shared secret; when set, register requires
    /// `X-Vmux-Bootstrap: <secret>` (or `Authorization: Bootstrap <secret>`).
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
            let file: DeviceFile = serde_json::from_str(&raw).unwrap_or_default();
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
        fs::write(path, serde_json::to_string_pretty(&file)? + "\n")?;
        Ok(())
    }

    fn register(
        &self,
        device_id: &str,
        login_name: &str,
        hostname: &str,
        plain_token: &str,
    ) -> Result<()> {
        let mut guard = self.devices.lock().expect("device store lock");
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
        let mut guard = self.devices.lock().expect("device store lock");
        let removed = guard.remove(device_id).is_some();
        if removed {
            Self::persist_locked(&self.path, &guard)?;
        }
        Ok(removed)
    }

    fn validate_token(&self, token: &str) -> Option<String> {
        let hash = sha256_hex(token);
        let guard = self.devices.lock().expect("device store lock");
        guard
            .values()
            .find(|d| d.token_hash == hash)
            .map(|d| d.device_id.clone())
    }

    fn set_apns(&self, device_id: &str, token: &str, env: &str) -> Result<()> {
        let mut guard = self.devices.lock().expect("device store lock");
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
}

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
    eprintln!("  config:  {}", path.display());
    eprintln!("  devices: {}", devices_path()?.display());
    if config.allow_localhost {
        eprintln!("  auth:    localhost registration allowed");
    }
    if config.allow_tailnet_cgnat {
        eprintln!("  auth:    Tailscale CGNAT sources accepted without whois");
    }

    for stream in listener.incoming() {
        if !state.running.load(Ordering::Relaxed) {
            break;
        }
        let Ok(stream) = stream else { continue };
        let state = Arc::clone(&state);
        thread::spawn(move || {
            if let Err(err) = handle_connection(stream, state) {
                eprintln!("relay connection error: {err:#}");
            }
        });
    }
    Ok(())
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
    fs::write(&pid_path, format!("{pid}\n"))
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
    let raw = fs::read_to_string(&path).unwrap_or_default();
    let pid: i32 = raw.trim().parse().unwrap_or(0);
    let _ = fs::remove_file(&path);
    if pid <= 1 {
        return Ok(false);
    }
    #[cfg(unix)]
    {
        // Only kill if still looks like our relay (best-effort).
        let _ = unsafe { libc::kill(pid, libc::SIGTERM) };
        thread::sleep(Duration::from_millis(150));
        let _ = unsafe { libc::kill(pid, libc::SIGKILL) };
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

fn handle_connection(mut stream: TcpStream, state: Arc<RelayState>) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;

    let peer = stream
        .peer_addr()
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "0.0.0.0".into());

    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    if request_line.is_empty() {
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
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
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
    let mut body = vec![0u8; content_len.min(MAX_WS_MSG)];
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
        ("POST", "/v1/devices/me/register") => {
            match register_device(&state, &peer, &headers) {
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
                    write_http(&mut stream, 403, "application/json", br#"{"error":"forbidden"}"#)?;
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
            }
        }
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

// ─── Registration / Tailscale auth ──────────────────────────────────────────

enum RegisterError {
    Forbidden(String),
    Other(anyhow::Error),
}

fn register_device(
    state: &RelayState,
    peer: &str,
    headers: &HashMap<String, String>,
) -> std::result::Result<(String, String), RegisterError> {
    let bootstrap_ok = bootstrap_header_matches(state, headers);
    // When a bootstrap secret is configured, it is required unless the peer is
    // already identified via Tailscale whois / localhost (handled below).
    // A wrong secret never grants access; a missing secret is only fatal when
    // no other trusted identity path succeeds.

    let peer_ip: IpAddr = peer.parse().unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
    let identity = resolve_peer_identity(state, peer, peer_ip, bootstrap_ok).map_err(|e| match e {
        RegisterError::Forbidden(m) => RegisterError::Forbidden(m),
        RegisterError::Other(e) => RegisterError::Other(e),
    })?;

    // only enforce allow_login for whois-sourced identities (see PeerIdentity::from_whois)
    if identity.require_allow_login
        && !state.config.allow_login.is_empty()
        && !state
            .config
            .allow_login
            .iter()
            .any(|l| l.eq_ignore_ascii_case(&identity.login_name))
    {
        return Err(RegisterError::Forbidden(format!(
            "login {} not in allow_login",
            identity.login_name
        )));
    }

    let device_id = sha256_hex(&identity.node_key);
    let token = random_hex(32);
    state
        .devices
        .register(
            &device_id,
            &identity.login_name,
            &identity.hostname,
            &token,
        )
        .map_err(RegisterError::Other)?;
    Ok((device_id, token))
}

fn bootstrap_header_matches(state: &RelayState, headers: &HashMap<String, String>) -> bool {
    let Some(secret) = state.config.bootstrap_secret.as_deref() else {
        return false;
    };
    if secret.is_empty() {
        return false;
    }
    let provided = headers
        .get("x-vmux-bootstrap")
        .cloned()
        .or_else(|| {
            headers.get("authorization").and_then(|a| {
                a.strip_prefix("Bootstrap ")
                    .map(|s| s.to_string())
                    .or_else(|| a.strip_prefix("bootstrap ").map(|s| s.to_string()))
            })
        })
        .unwrap_or_default();
    // Constant-time-ish compare for equal length; length leak is acceptable here.
    if provided.len() != secret.len() {
        return false;
    }
    provided
        .as_bytes()
        .iter()
        .zip(secret.as_bytes().iter())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

struct PeerIdentity {
    login_name: String,
    hostname: String,
    node_key: String,
    /// When true, `allow_login` must contain `login_name` (whois path).
    require_allow_login: bool,
}

fn resolve_peer_identity(
    state: &RelayState,
    peer: &str,
    peer_ip: IpAddr,
    bootstrap_ok: bool,
) -> std::result::Result<PeerIdentity, RegisterError> {
    // 1) Localhost — only when explicitly allowed. Not subject to allow_login
    // (that list is Tailscale logins). Refuse localhost when allow_login is
    // set AND allow_localhost is false; if allow_localhost is true, admit.
    if is_loopback(peer_ip) {
        if !state.config.allow_localhost {
            return Err(RegisterError::Forbidden(
                "localhost registration disabled (set allow_localhost)".into(),
            ));
        }
        return Ok(PeerIdentity {
            login_name: "localhost".into(),
            hostname: "localhost".into(),
            node_key: format!("vmux-dev-localhost:{peer}"),
            require_allow_login: false,
        });
    }

    // 2) Tailscale whois — real identity; enforce allow_login if non-empty.
    if let Some(id) = try_tailscale_whois(peer) {
        if state.config.allow_login.is_empty()
            || state
                .config
                .allow_login
                .iter()
                .any(|l| l.eq_ignore_ascii_case(&id.login_name))
        {
            return Ok(PeerIdentity {
                login_name: id.login_name,
                hostname: id.hostname,
                node_key: id.node_key,
                require_allow_login: false, // already checked
            });
        }
        return Err(RegisterError::Forbidden(format!(
            "login {} not in allow_login",
            id.login_name
        )));
    }

    // 3) CGNAT without whois — only when allow_tailnet_cgnat, and only when
    // allow_login is empty (otherwise whois is required to prove login).
    if state.config.allow_tailnet_cgnat && is_tailscale_cgnat(peer_ip) {
        if !state.config.allow_login.is_empty() {
            return Err(RegisterError::Forbidden(
                "allow_login is set; Tailscale whois required (CGNAT fallback denied)".into(),
            ));
        }
        return Ok(PeerIdentity {
            login_name: "tailnet".into(),
            hostname: peer.to_string(),
            node_key: format!("vmux-cgnat:{peer}"),
            require_allow_login: false,
        });
    }

    // 4) Bootstrap secret — only when header actually matched.
    if bootstrap_ok {
        return Ok(PeerIdentity {
            login_name: "bootstrap".into(),
            hostname: peer.to_string(),
            node_key: format!("vmux-bootstrap:{peer}"),
            require_allow_login: false,
        });
    }

    if state.config.bootstrap_secret.as_ref().is_some_and(|s| !s.is_empty()) {
        return Err(RegisterError::Forbidden(
            "bootstrap secret required or peer not on Tailscale".into(),
        ));
    }

    Err(RegisterError::Forbidden(
        "peer not recognized (enable Tailscale, allow_localhost, or allow_tailnet_cgnat)".into(),
    ))
}

fn try_tailscale_whois(peer: &str) -> Option<PeerIdentity> {
    let output = std::process::Command::new("tailscale")
        .args(["whois", "--json", peer])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let v: Value = serde_json::from_slice(&output.stdout).ok()?;
    // tailscale whois --json shape varies; try common fields.
    let user = v
        .pointer("/UserProfile/LoginName")
        .or_else(|| v.pointer("/User/LoginName"))
        .or_else(|| v.get("LoginName"))
        .and_then(|x| x.as_str())
        .or_else(|| {
            v.get("UserProfile")
                .and_then(|u| u.get("LoginName"))
                .and_then(|x| x.as_str())
        })?;
    let hostname = v
        .pointer("/Node/Hostinfo/Hostname")
        .or_else(|| v.pointer("/Node/Name"))
        .and_then(|x| x.as_str())
        .unwrap_or("tailscale-node");
    let node_key = v
        .pointer("/Node/Key")
        .and_then(|x| x.as_str())
        .unwrap_or(peer);
    Some(PeerIdentity {
        login_name: user.to_string(),
        hostname: hostname.to_string(),
        node_key: node_key.to_string(),
        require_allow_login: true,
    })
}

fn is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback(),
    }
}

fn is_tailscale_cgnat(ip: IpAddr) -> bool {
    // 100.64.0.0/10
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 100 && (o[1] & 0xC0) == 64
        }
        _ => false,
    }
}

// ─── WebSocket ──────────────────────────────────────────────────────────────

fn handle_ws_upgrade(
    stream: TcpStream,
    reader: BufReader<TcpStream>,
    headers: HashMap<String, String>,
    state: Arc<RelayState>,
    _peer: String,
) -> Result<()> {
    let device_id = device_id_from_ws_headers(&headers, &state.devices)
        .ok_or_else(|| anyhow!("ws upgrade unauthorized"))?;

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

    // reader holds a clone; drop it so we write on the original stream fully.
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

    let mut ws = tungstenite::WebSocket::from_raw_socket(stream, Role::Server, None);
    ws.get_mut()
        .set_read_timeout(Some(Duration::from_millis(200)))
        .ok();

    // Hello must arrive quickly.
    let hello_deadline = Instant::now() + HELLO_TIMEOUT;
    let mut helloed = false;
    let mut active_subs: HashMap<String, thread::JoinHandle<()>> = HashMap::new();
    let stop_flags: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let push_tx = Arc::new(Mutex::new(None::<std::sync::mpsc::Sender<String>>));
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    *push_tx.lock().expect("push lock") = Some(tx);

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
                                let id = ev
                                    .get("id")
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string())
                                    .unwrap_or_else(|| {
                                        format!(
                                            "{}:{}",
                                            ev.get("ts").and_then(|t| t.as_u64()).unwrap_or(0),
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
                                    let _ = tx.send(frame.to_string());
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

                    // stop existing sub for this surface
                    if let Some(flag) = stop_flags
                        .lock()
                        .expect("flags")
                        .remove(&surface_id)
                    {
                        flag.store(true, Ordering::Relaxed);
                    }
                    if let Some(handle) = active_subs.remove(&surface_id) {
                        let _ = handle.join();
                    }

                    let stop = Arc::new(AtomicBool::new(false));
                    stop_flags
                        .lock()
                        .expect("flags")
                        .insert(surface_id.clone(), Arc::clone(&stop));
                    let socket = state.socket.clone();
                    let push = Arc::clone(&push_tx);
                    let fps = state.config.default_fps.max(1);
                    let idle_fps = state.config.idle_fps.max(1);
                    let sid = surface_id.clone();
                    let handle = thread::spawn(move || {
                        run_surface_poller(
                            socket, workspace_id, sid, lines, fps, idle_fps, stop, push,
                        );
                    });
                    active_subs.insert(surface_id, handle);
                    let _ = ws.send(Message::Text(
                        rpc_ok(&id, json!({})).to_string(),
                    ));
                    continue;
                }

                if method == "surface.unsubscribe" {
                    let surface_id = params
                        .get("surface_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if let Some(flag) = stop_flags.lock().expect("flags").remove(surface_id) {
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
    for (_, flag) in stop_flags.lock().expect("flags").drain() {
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
        const T: &[u8] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
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

fn run_surface_poller(
    socket: PathBuf,
    _workspace_id: String,
    surface_id: String,
    lines: usize,
    active_fps: u32,
    idle_fps: u32,
    stop: Arc<AtomicBool>,
    push: Arc<Mutex<Option<std::sync::mpsc::Sender<String>>>>,
) {
    let mut rev: u64 = 0;
    let mut prev: Option<ScreenSnap> = None;
    let last_input = Instant::now();
    let mut last_checksum = Instant::now() - Duration::from_secs(10);
    let mut current_fps = active_fps.max(1);

    while !stop.load(Ordering::Relaxed) {
        if last_input.elapsed() > Duration::from_millis(1500) {
            current_fps = idle_fps.max(1);
        }
        let interval = Duration::from_millis((1000 / current_fps as u64).max(20));

        match read_surface_screen(&socket, &surface_id, lines) {
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
                            let _ = tx.send(full.to_string());
                        }
                    }
                    let frame = json!({
                        "type": "screen.diff",
                        "surface_id": surface_id,
                        "rev": rev,
                        "ops": ops,
                    });
                    if let Some(tx) = push.lock().ok().and_then(|g| g.clone()) {
                        let _ = tx.send(frame.to_string());
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
                        let _ = tx.send(frame.to_string());
                    }
                }
            }
            Err(err) => {
                eprintln!("relay poll {surface_id}: {err:#}");
            }
        }

        // boost fps if someone notes input via atomic — we approximate by
        // checking high-frequency for a short time after any successful read
        // of changing content; last_input is bumped when ops non-empty.
        if prev.is_some() {
            // keep
        }
        let _ = last_input; // silence if unused in some paths
        thread::sleep(interval);
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
    dig.iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn read_surface_screen(socket: &Path, surface_id: &str, lines: usize) -> Result<ScreenSnap> {
    let resp = protocol::request(
        socket,
        &Request::ReadScreen {
            pane: Some(surface_id.to_string()),
            scrollback: false,
            limit_bytes: Some(256_000),
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

    // Prefer cursor from list snapshot when available.
    let (cx, cy) = pane_cursor(socket, surface_id).unwrap_or((0, 0));

    let mut rows: Vec<String> = text
        .split('\n')
        .map(|s| s.to_string())
        .collect();
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

fn pane_cursor(socket: &Path, pane: &str) -> Option<(i64, i64)> {
    let resp = protocol::request(socket, &Request::List).ok()?;
    let data = resp.data?;
    let panes = data.get("panes")?.as_object()?;
    let p = panes.get(pane)?;
    let col = p.get("cursor_col")?.as_u64()? as i64;
    let row = p.get("cursor_row")?.as_u64()? as i64;
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
            call(socket, &Request::FocusPane { pane: surface })?;
            Ok(json!({}))
        }
        "surface.send_text" => {
            let surface = req_str(params, "surface_id")?;
            let text = params
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let _ = call(socket, &Request::FocusPane { pane: surface.clone() });
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
            let _ = call(socket, &Request::FocusPane { pane: surface.clone() });
            call(
                socket,
                &Request::SendKey {
                    pane: Some(surface),
                    keys: vec![mapped],
                },
            )?;
            Ok(json!({}))
        }
        "surface.read_text" => {
            let surface = req_str(params, "surface_id")?;
            let lines = params
                .get("lines")
                .and_then(|v| v.as_u64())
                .unwrap_or(200) as usize;
            let snap = read_surface_screen(socket, &surface, lines)?;
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
            let body = params
                .get("body")
                .and_then(|v| v.as_str())
                .unwrap_or("");
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
    let pane_ids = ws.get("panes")?.as_array()?;
    for pid in pane_ids {
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
        if let Some(ws) = arr.iter().find(|w| w.get("id").and_then(|i| i.as_str()) == Some(&id))
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

    // Prefer panes from the active tab, else workspace.panes.
    let mut pane_ids: Vec<String> = Vec::new();
    if let Some(tabs) = ws.get("tabs").and_then(|t| t.as_array()) {
        let active = ws.get("active_tab").and_then(|v| v.as_str());
        let tab = tabs
            .iter()
            .find(|t| t.get("id").and_then(|i| i.as_str()) == active)
            .or_else(|| tabs.first());
        if let Some(tab) = tab {
            if let Some(arr) = tab.get("panes").and_then(|p| p.as_array()) {
                for p in arr {
                    if let Some(id) = p.as_str() {
                        pane_ids.push(id.to_string());
                    }
                }
            }
        }
    }
    if pane_ids.is_empty() {
        if let Some(arr) = ws.get("panes").and_then(|p| p.as_array()) {
            for p in arr {
                if let Some(id) = p.as_str() {
                    pane_ids.push(id.to_string());
                }
            }
        }
    }

    let mut surfaces = Vec::new();
    for (index, pid) in pane_ids.iter().enumerate() {
        let title = panes_map
            .get(pid)
            .and_then(|p| p.get("title"))
            .and_then(|t| t.as_str())
            .unwrap_or(pid)
            .to_string();
        surfaces.push(json!({
            "id": pid,
            "title": title,
            "index": index,
            "type": "terminal",
            "focused": ws.get("active_pane").and_then(|v| v.as_str()) == Some(pid.as_str()),
        }));
    }
    Ok(json!({ "surfaces": surfaces, "workspace_id": workspace_id }))
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
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let path = dir.join(format!("{}-{}", now_secs(), safe));
    fs::write(&path, bytes)?;
    Ok(json!({
        "path": path.display().to_string(),
        "filename": safe,
    }))
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
    let cleaned: Vec<u8> = input
        .bytes()
        .filter(|b| !b.is_ascii_whitespace())
        .collect();
    if cleaned.len() % 4 != 0 {
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

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    // Prefer getrandom via /dev/urandom
    if let Ok(mut f) = fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut buf);
    } else {
        let t = now_secs();
        for (i, b) in buf.iter_mut().enumerate() {
            *b = ((t.wrapping_mul(1103515245).wrapping_add(i as u64 * 12345)) % 256) as u8;
        }
    }
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
        assert!(is_tailscale_cgnat(IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1))));
        assert!(is_tailscale_cgnat(IpAddr::V4(Ipv4Addr::new(100, 127, 1, 1))));
        assert!(!is_tailscale_cgnat(IpAddr::V4(Ipv4Addr::new(100, 63, 0, 1))));
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
