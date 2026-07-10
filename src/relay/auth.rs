//! Device registration and Tailscale peer identity.
use super::{random_hex, sha256_hex, RelayState};
use serde_json::Value;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};

// ─── Registration / Tailscale auth ──────────────────────────────────────────

pub(crate) enum RegisterError {
    Forbidden(String),
    Other(anyhow::Error),
}

pub(crate) fn register_device(
    state: &RelayState,
    peer: &str,
    headers: &HashMap<String, String>,
) -> std::result::Result<(String, String), RegisterError> {
    let bootstrap_ok = bootstrap_header_matches(state, headers);
    // Policy (bugs.md P1#3): when bootstrap_secret is configured non-empty, it
    // is required for *every* registration path (whois, localhost, CGNAT).
    // Identity checks still run after the secret gate.
    let secret_required = state
        .config
        .bootstrap_secret
        .as_ref()
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    if secret_required && !bootstrap_ok {
        return Err(RegisterError::Forbidden(
            "bootstrap secret required (X-Vmux-Bootstrap or Authorization: Bootstrap …)".into(),
        ));
    }

    let peer_ip: IpAddr = peer.parse().unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
    let identity =
        resolve_peer_identity(state, peer, peer_ip, bootstrap_ok).map_err(|e| match e {
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
        .register(&device_id, &identity.login_name, &identity.hostname, &token)
        .map_err(RegisterError::Other)?;
    Ok((device_id, token))
}

pub(crate) fn bootstrap_header_matches(
    state: &RelayState,
    headers: &HashMap<String, String>,
) -> bool {
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

pub(crate) struct PeerIdentity {
    login_name: String,
    hostname: String,
    node_key: String,
    /// When true, `allow_login` must contain `login_name` (whois path).
    require_allow_login: bool,
}

pub(crate) fn resolve_peer_identity(
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

    if state
        .config
        .bootstrap_secret
        .as_ref()
        .is_some_and(|s| !s.is_empty())
    {
        return Err(RegisterError::Forbidden(
            "bootstrap secret required or peer not on Tailscale".into(),
        ));
    }

    Err(RegisterError::Forbidden(
        "peer not recognized (enable Tailscale, allow_localhost, or allow_tailnet_cgnat)".into(),
    ))
}

pub(crate) fn try_tailscale_whois(peer: &str) -> Option<PeerIdentity> {
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

pub(crate) fn is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback(),
    }
}

pub(crate) fn is_tailscale_cgnat(ip: IpAddr) -> bool {
    // 100.64.0.0/10
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 100 && (o[1] & 0xC0) == 64
        }
        _ => false,
    }
}
