//! Port detection, registry, and optional Tailscale forwarding.
//!
//! Detection prefers `/proc/net/tcp{,6}` on Linux (fast, no subprocess). The
//! registry diffs opens/closes so the daemon can notify and tear down proxies.
//! Forwarding binds only on a Tailscale IP (never `0.0.0.0`).

#[cfg(target_os = "linux")]
use std::collections::HashMap;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// A listening port attributed to a pane (when possible).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DetectedPort {
    pub port: u16,
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forward: Option<ForwardInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ForwardInfo {
    pub via: String,
    pub url: String,
}

#[derive(Debug, Clone, Default)]
pub struct PortRegistry {
    /// Keyed by port number (dual-stack collapsed).
    ports: BTreeMap<u16, DetectedPort>,
    /// Active Tailscale forwarders by local port.
    forwards: BTreeMap<u16, ForwardHandle>,
}

#[derive(Debug)]
struct ForwardHandle {
    stop: Arc<AtomicBool>,
    url: String,
}

impl Clone for ForwardHandle {
    fn clone(&self) -> Self {
        Self {
            stop: Arc::clone(&self.stop),
            url: self.url.clone(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct PortDiff {
    pub opened: Vec<DetectedPort>,
    pub closed: Vec<DetectedPort>,
}

impl PortRegistry {
    pub fn list(&self) -> Vec<DetectedPort> {
        self.ports
            .values()
            .cloned()
            .map(|mut p| {
                if let Some(fwd) = self.forwards.get(&p.port) {
                    p.forward = Some(ForwardInfo {
                        via: "tailscale".into(),
                        url: fwd.url.clone(),
                    });
                }
                p
            })
            .collect()
    }

    pub fn get(&self, port: u16) -> Option<DetectedPort> {
        let mut p = self.ports.get(&port).cloned()?;
        if let Some(fwd) = self.forwards.get(&port) {
            p.forward = Some(ForwardInfo {
                via: "tailscale".into(),
                url: fwd.url.clone(),
            });
        }
        Some(p)
    }

    /// Replace detected set; return open/close diffs (ignoring forward field).
    pub fn replace_detected(&mut self, next: BTreeMap<u16, DetectedPort>) -> PortDiff {
        let mut opened = Vec::new();
        let mut closed = Vec::new();
        for (port, det) in &next {
            if !self.ports.contains_key(port) {
                opened.push(det.clone());
            }
        }
        for (port, det) in &self.ports {
            if !next.contains_key(port) {
                closed.push(det.clone());
            }
        }
        // Preserve forward annotations on surviving ports.
        self.ports = next;
        PortDiff { opened, closed }
    }

    pub fn stop_forward(&mut self, port: u16) -> bool {
        if let Some(handle) = self.forwards.remove(&port) {
            handle.stop.store(true, Ordering::SeqCst);
            true
        } else {
            false
        }
    }

    pub fn stop_all_forwards(&mut self) {
        let keys: Vec<u16> = self.forwards.keys().copied().collect();
        for port in keys {
            self.stop_forward(port);
        }
    }

    pub fn has_forward(&self, port: u16) -> bool {
        self.forwards.contains_key(&port)
    }

    pub fn insert_forward(&mut self, port: u16, stop: Arc<AtomicBool>, url: String) {
        if let Some(old) = self.forwards.insert(port, ForwardHandle { stop, url }) {
            old.stop.store(true, Ordering::SeqCst);
        }
    }
}

/// Build `ssh -L local:127.0.0.1:port user@host` for clipboard/scripting.
pub fn ssh_forward_command(port: u16, ssh_host: &str) -> String {
    format!("ssh -L {port}:127.0.0.1:{port} {ssh_host}")
}

/// Resolve host for ssh-cmd: config override → USER@hostname → USER@SSH_CONNECTION peer.
pub fn resolve_ssh_host(config_override: &str) -> String {
    let override_host = config_override.trim();
    if !override_host.is_empty() {
        return override_host.to_string();
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "user".into());
    if let Ok(hostname) = fs::read_to_string("/etc/hostname") {
        let hostname = hostname.trim();
        if !hostname.is_empty() {
            return format!("{user}@{hostname}");
        }
    }
    if let Ok(conn) = std::env::var("SSH_CONNECTION") {
        // client_ip client_port server_ip server_port
        let parts: Vec<&str> = conn.split_whitespace().collect();
        if parts.len() >= 3 {
            return format!("{user}@{}", parts[2]);
        }
    }
    if let Some(host) = hostname_cmd() {
        return format!("{user}@{host}");
    }
    format!("{user}@localhost")
}

fn hostname_cmd() -> Option<String> {
    let out = std::process::Command::new("hostname")
        .arg("-f")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

#[derive(Debug, Clone)]
pub struct ScanInput {
    /// pane_id → root pid
    pub pane_pids: BTreeMap<String, u32>,
    /// pane_id → workspace_id
    pub pane_workspace: BTreeMap<String, String>,
    /// Ports to ignore (relay port, user list).
    pub ignore_ports: BTreeSet<u16>,
    pub ignore_processes: BTreeSet<String>,
    pub ignore_ephemeral: bool,
}

/// Scan listening sockets and attribute them to panes.
pub fn scan_ports(input: &ScanInput) -> BTreeMap<u16, DetectedPort> {
    #[cfg(target_os = "linux")]
    {
        scan_ports_linux(input)
    }
    #[cfg(not(target_os = "linux"))]
    {
        // Touch every field so macOS/clippy does not treat ScanInput as dead
        // when the Linux scanner is not compiled.
        let _ = (
            input.pane_pids.len(),
            input.pane_workspace.len(),
            input.ignore_ports.len(),
            input.ignore_processes.len(),
            input.ignore_ephemeral,
        );
        BTreeMap::new()
    }
}

#[cfg(target_os = "linux")]
fn scan_ports_linux(input: &ScanInput) -> BTreeMap<u16, DetectedPort> {
    let mut entries = Vec::new();
    if let Ok(text) = fs::read_to_string("/proc/net/tcp") {
        entries.extend(parse_proc_net_tcp(&text, false));
    }
    if let Ok(text) = fs::read_to_string("/proc/net/tcp6") {
        entries.extend(parse_proc_net_tcp(&text, true));
    }

    let ephemeral_floor = if input.ignore_ephemeral {
        ephemeral_port_floor()
    } else {
        u16::MAX
    };

    // pid → set of socket inodes
    let mut pid_inodes: HashMap<u32, BTreeSet<u64>> = HashMap::new();
    let mut all_pids: BTreeSet<u32> = input.pane_pids.values().copied().collect();
    // Include descendants cheaply: single pass ppid map + walk.
    let ppid_map = build_ppid_map();
    let root_set: BTreeSet<u32> = input.pane_pids.values().copied().collect();
    for &pid in ppid_map.keys() {
        if is_descendant_of(pid, &root_set, &ppid_map) {
            all_pids.insert(pid);
        }
    }
    for &pid in &all_pids {
        if let Some(inodes) = socket_inodes_for_pid(pid) {
            pid_inodes.insert(pid, inodes);
        }
    }

    // inode → pid
    let mut inode_pid: HashMap<u64, u32> = HashMap::new();
    for (pid, inodes) in &pid_inodes {
        for inode in inodes {
            inode_pid.entry(*inode).or_insert(*pid);
        }
    }

    // pid → pane
    let mut pid_to_pane: HashMap<u32, String> = HashMap::new();
    for (pane, &root) in &input.pane_pids {
        pid_to_pane.insert(root, pane.clone());
        for &pid in &all_pids {
            if pid != root && is_descendant_of(pid, &BTreeSet::from([root]), &ppid_map) {
                pid_to_pane.entry(pid).or_insert_with(|| pane.clone());
            }
        }
    }

    let mut out: BTreeMap<u16, DetectedPort> = BTreeMap::new();
    for ent in entries {
        if input.ignore_ports.contains(&ent.port) {
            continue;
        }
        if ent.port >= ephemeral_floor {
            continue;
        }
        let Some(&pid) = inode_pid.get(&ent.inode) else {
            continue;
        };
        let process = process_name(pid);
        if let Some(ref name) = process {
            if input.ignore_processes.contains(name) {
                continue;
            }
        }
        let pane = pid_to_pane.get(&pid).cloned();
        let workspace = pane
            .as_ref()
            .and_then(|p| input.pane_workspace.get(p).cloned());
        let host = ent.host;
        let prefer_wildcard = host == "0.0.0.0" || host == "::" || host == "*";
        match out.get_mut(&ent.port) {
            Some(existing) => {
                if prefer_wildcard {
                    existing.host = host;
                }
                if existing.pid.is_none() {
                    existing.pid = Some(pid);
                }
                if existing.process.is_none() {
                    existing.process = process;
                }
                if existing.pane.is_none() {
                    existing.pane = pane;
                }
                if existing.workspace.is_none() {
                    existing.workspace = workspace;
                }
            }
            None => {
                out.insert(
                    ent.port,
                    DetectedPort {
                        port: ent.port,
                        host,
                        pid: Some(pid),
                        process,
                        pane,
                        workspace,
                        forward: None,
                    },
                );
            }
        }
    }
    out
}

// Pure parsers: used by the Linux scanner and unit tests. On macOS release
// builds neither path is linked, so allow dead_code there.
#[derive(Debug, Clone)]
#[cfg_attr(not(any(test, target_os = "linux")), allow(dead_code))]
pub(crate) struct SockEntry {
    host: String,
    port: u16,
    inode: u64,
}

/// Parse `/proc/net/tcp` or `tcp6` LISTEN lines.
#[cfg_attr(not(any(test, target_os = "linux")), allow(dead_code))]
pub(crate) fn parse_proc_net_tcp(text: &str, v6: bool) -> Vec<SockEntry> {
    let mut out = Vec::new();
    for line in text.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        // sl local_address rem_address st ... inode
        if fields.len() < 10 {
            continue;
        }
        // LISTEN = 0A
        if fields[3] != "0A" {
            continue;
        }
        let Some((host, port)) = parse_hex_addr(fields[1], v6) else {
            continue;
        };
        let Ok(inode) = fields[9].parse::<u64>() else {
            continue;
        };
        out.push(SockEntry { host, port, inode });
    }
    out
}

#[cfg_attr(not(any(test, target_os = "linux")), allow(dead_code))]
fn parse_hex_addr(field: &str, v6: bool) -> Option<(String, u16)> {
    let (addr_hex, port_hex) = field.split_once(':')?;
    let port = u16::from_str_radix(port_hex, 16).ok()?;
    if !v6 {
        if addr_hex.len() != 8 {
            return None;
        }
        let ip = u32::from_str_radix(addr_hex, 16).ok()?;
        // little-endian words in /proc
        let b = ip.to_le_bytes();
        let host = if b == [0, 0, 0, 0] {
            "0.0.0.0".into()
        } else if b == [127, 0, 0, 1] {
            "127.0.0.1".into()
        } else {
            format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3])
        };
        return Some((host, port));
    }
    // IPv6: 32 hex chars, 4-byte little-endian groups
    if addr_hex.len() != 32 {
        return None;
    }
    let mut bytes = [0u8; 16];
    for i in 0..4 {
        let word = u32::from_str_radix(&addr_hex[i * 8..i * 8 + 8], 16).ok()?;
        let le = word.to_le_bytes();
        bytes[i * 4..i * 4 + 4].copy_from_slice(&le);
    }
    // v4-mapped ::ffff:a.b.c.d
    if bytes[..10] == [0, 0, 0, 0, 0, 0, 0, 0, 0, 0] && bytes[10] == 0xff && bytes[11] == 0xff {
        let b = &bytes[12..16];
        let host = if b == [0, 0, 0, 0] {
            "0.0.0.0".into()
        } else if b == [127, 0, 0, 1] {
            "127.0.0.1".into()
        } else {
            format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3])
        };
        return Some((host, port));
    }
    if bytes.iter().all(|&b| b == 0) {
        return Some(("::".into(), port));
    }
    let ip = std::net::Ipv6Addr::from(bytes);
    Some((ip.to_string(), port))
}

// /proc walk helpers — only used by `scan_ports_linux`.
#[cfg(target_os = "linux")]
fn ephemeral_port_floor() -> u16 {
    if let Ok(text) = fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range") {
        let mut parts = text.split_whitespace();
        if let Some(low) = parts.next().and_then(|s| s.parse().ok()) {
            return low;
        }
    }
    32768
}

#[cfg(target_os = "linux")]
fn build_ppid_map() -> HashMap<u32, u32> {
    let mut map = HashMap::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return map;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let stat = fs::read_to_string(entry.path().join("stat")).unwrap_or_default();
        if let Some(ppid) = proc_stat_ppid(&stat) {
            map.insert(pid, ppid);
        }
    }
    map
}

#[cfg(target_os = "linux")]
fn proc_stat_ppid(stat: &str) -> Option<u32> {
    let rest = stat.rsplit_once(") ")?.1;
    let mut fields = rest.split_whitespace();
    fields.next()?; // state
    fields.next()?.parse().ok()
}

#[cfg(target_os = "linux")]
fn is_descendant_of(pid: u32, roots: &BTreeSet<u32>, ppid_map: &HashMap<u32, u32>) -> bool {
    if roots.contains(&pid) {
        return true;
    }
    let mut current = pid;
    for _ in 0..64 {
        let Some(&ppid) = ppid_map.get(&current) else {
            return false;
        };
        if roots.contains(&ppid) {
            return true;
        }
        if ppid == 0 || ppid == current {
            return false;
        }
        current = ppid;
    }
    false
}

#[cfg(target_os = "linux")]
fn socket_inodes_for_pid(pid: u32) -> Option<BTreeSet<u64>> {
    let fd_dir = format!("/proc/{pid}/fd");
    let entries = fs::read_dir(fd_dir).ok()?;
    let mut set = BTreeSet::new();
    for entry in entries.flatten() {
        let Ok(link) = fs::read_link(entry.path()) else {
            continue;
        };
        let s = link.to_string_lossy();
        if let Some(rest) = s.strip_prefix("socket:[") {
            if let Some(num) = rest.strip_suffix(']') {
                if let Ok(inode) = num.parse::<u64>() {
                    set.insert(inode);
                }
            }
        }
    }
    Some(set)
}

#[cfg(target_os = "linux")]
fn process_name(pid: u32) -> Option<String> {
    let comm = fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    let name = comm.trim().to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// Start a Tailscale-facing TCP proxy: `ts_ip:port` → `127.0.0.1:port`.
pub fn start_tailscale_forward(
    port: u16,
    ts_ip: Ipv4Addr,
    stop: Arc<AtomicBool>,
) -> std::io::Result<String> {
    let listen_addr = SocketAddr::new(IpAddr::V4(ts_ip), port);
    let listener = TcpListener::bind(listen_addr)?;
    listener.set_nonblocking(true)?;
    let url = format!("http://{ts_ip}:{port}");
    let stop_flag = Arc::clone(&stop);
    thread::spawn(move || {
        while !stop_flag.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((client, _)) => {
                    if stop_flag.load(Ordering::SeqCst) {
                        break;
                    }
                    let stop_c = Arc::clone(&stop_flag);
                    thread::spawn(move || {
                        if let Ok(upstream) = TcpStream::connect(SocketAddr::new(
                            IpAddr::V4(Ipv4Addr::LOCALHOST),
                            port,
                        )) {
                            proxy_bidirectional(client, upstream, stop_c);
                        }
                    });
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(100));
                }
                Err(_) => break,
            }
        }
    });
    Ok(url)
}

fn proxy_bidirectional(a: TcpStream, b: TcpStream, stop: Arc<AtomicBool>) {
    let Ok(a2) = a.try_clone() else {
        return;
    };
    let Ok(b2) = b.try_clone() else {
        return;
    };
    let stop1 = Arc::clone(&stop);
    let h1 = thread::spawn(move || copy_until_stop(a, b, stop1));
    let stop2 = stop;
    let h2 = thread::spawn(move || copy_until_stop(b2, a2, stop2));
    let _ = h1.join();
    let _ = h2.join();
}

fn copy_until_stop(mut from: TcpStream, mut to: TcpStream, stop: Arc<AtomicBool>) {
    let _ = from.set_read_timeout(Some(Duration::from_millis(200)));
    let _ = to.set_write_timeout(Some(Duration::from_secs(5)));
    let mut buf = [0u8; 16 * 1024];
    while !stop.load(Ordering::SeqCst) {
        match from.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if to.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(err) if err.kind() == std::io::ErrorKind::TimedOut => continue,
            Err(_) => break,
        }
    }
}

/// Convert detected ports into the sidebar `ListeningPort` shape.
pub fn to_listening_ports(detected: &[DetectedPort]) -> Vec<crate::model::ListeningPort> {
    detected
        .iter()
        .map(|d| crate::model::ListeningPort {
            host: d.host.clone(),
            port: d.port,
            pids: d.pid.into_iter().collect(),
            process: d.process.clone(),
            pane: d.pane.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ipv4_listen_line() {
        // 0100007F:0BB8 = 127.0.0.1:3000, state 0A, inode 12345
        let text = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: 0100007F:0BB8 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 12345 1 0000000000000000 100 0 0 10 0\n";
        let entries = parse_proc_net_tcp(text, false);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].port, 3000);
        assert_eq!(entries[0].host, "127.0.0.1");
        assert_eq!(entries[0].inode, 12345);
    }

    #[test]
    fn parses_wildcard_ipv4() {
        let text = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: 00000000:1F40 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 9 1 0000000000000000 100 0 0 10 0\n";
        let entries = parse_proc_net_tcp(text, false);
        assert_eq!(entries[0].port, 8000);
        assert_eq!(entries[0].host, "0.0.0.0");
    }

    #[test]
    fn ignores_non_listen() {
        let text = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: 0100007F:0BB8 00000000:0000 01 00000000:00000000 00:00000000 00000000     0        0 1 1 0000000000000000 100 0 0 10 0\n";
        assert!(parse_proc_net_tcp(text, false).is_empty());
    }

    #[test]
    fn registry_diffs_open_close() {
        let mut reg = PortRegistry::default();
        let mut a = BTreeMap::new();
        a.insert(
            3000,
            DetectedPort {
                port: 3000,
                host: "127.0.0.1".into(),
                pid: Some(1),
                process: Some("node".into()),
                pane: Some("pane-1".into()),
                workspace: Some("ws-1".into()),
                forward: None,
            },
        );
        let d1 = reg.replace_detected(a.clone());
        assert_eq!(d1.opened.len(), 1);
        assert!(d1.closed.is_empty());
        let d2 = reg.replace_detected(BTreeMap::new());
        assert!(d2.opened.is_empty());
        assert_eq!(d2.closed.len(), 1);
    }

    #[test]
    fn ssh_cmd_uses_override() {
        assert_eq!(
            ssh_forward_command(3000, "me@box"),
            "ssh -L 3000:127.0.0.1:3000 me@box"
        );
    }
}
