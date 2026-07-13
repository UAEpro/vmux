use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

use crate::paths;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct LmuxConfig {
    #[serde(default)]
    pub ui: UiConfig,
    /// Opt-in mobile / Cmux Remote relay (started on attach when enabled).
    #[serde(default)]
    pub relay: RelaySettings,
    /// Name tabs after what the coding agent running in them is doing.
    #[serde(default)]
    pub agent_titles: AgentTitleSettings,
}

/// Automatic tab naming for panes running a coding agent.
///
/// The daemon reads the title the agent sets on the terminal (OSC 0/2) and
/// condenses it to one or two words. Agents that never set a title fall back to
/// `llm_command`, which is asked to name the session from what is on screen.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct AgentTitleSettings {
    /// Master switch. Off leaves every tab title exactly as the user set it.
    pub enabled: bool,
    /// Ask `llm_command` to name the tab when the agent sets no terminal title.
    /// Costs one short model call per pane; the OSC path is free.
    pub llm_fallback: bool,
    /// Headless command that reads a prompt on stdin and prints a short title.
    pub llm_command: String,
    /// How long an agent pane may run without a usable title before falling
    /// back to `llm_command`.
    pub llm_delay_ms: u64,
}

impl Default for AgentTitleSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            llm_fallback: true,
            llm_command: "claude -p".to_string(),
            llm_delay_ms: 20_000,
        }
    }
}

/// Mobile relay preferences (settings UI + `vmux config set relay.*`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct RelaySettings {
    /// When true, `vmux attach` starts the phone relay if it is not already up.
    pub enabled: bool,
    /// Where the relay listens: `auto` | `tailscale` | `local`.
    /// Never binds `0.0.0.0` (public/all interfaces) — phone access is
    /// Tailscale or localhost only.
    /// - auto: Tailscale IP if online, else localhost
    /// - tailscale: Tailscale IP only (errors / falls back to local if offline)
    /// - local: 127.0.0.1 only
    pub bind: String,
    /// TCP port (default 4399, Cmux Remote default).
    pub port: u16,
    /// Allow localhost device registration (dev / same machine).
    pub allow_localhost: bool,
    /// Accept Tailscale CGNAT peers without whois (practical on Linux).
    pub allow_tailnet_cgnat: bool,
    /// Serve the browser paste page (GET /paste, POST /v1/paste) for
    /// pasting screenshots into panes from other devices.
    pub allow_paste: bool,
}

impl Default for RelaySettings {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: "auto".to_string(),
            port: 4399,
            allow_localhost: false,
            // Safer default: require Tailscale whois (or bootstrap) rather than
            // trusting any CGNAT peer (newimp §8).
            allow_tailnet_cgnat: false,
            // On by default: uploads still require a paired device token, and
            // the relay itself is opt-in.
            allow_paste: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UiConfig {
    #[serde(default)]
    pub sidebar_collapsed: bool,
    /// Expanded sidebar width in terminal columns (clamped on load).
    /// When `sidebar_fit` is true this is the **maximum** width.
    #[serde(default = "default_sidebar_width")]
    pub sidebar_width: u16,
    /// Fit sidebar width to workspace name text (up to `sidebar_width` max).
    #[serde(default)]
    pub sidebar_fit: bool,
    #[serde(default = "default_prefix_key")]
    pub prefix_key: String,
    #[serde(default = "default_scroll_step")]
    pub scroll_step: usize,
    #[serde(default = "default_theme")]
    pub theme: String,
    #[serde(default = "default_workspace_second_line")]
    pub workspace_second_line: String,
    /// Soft-blink the active pane caret while idle.
    #[serde(default = "default_true")]
    pub cursor_blink: bool,
    /// Half-period of the caret blink in milliseconds (on or off duration).
    #[serde(default = "default_cursor_blink_ms")]
    pub cursor_blink_ms: u64,
    /// Sidebar / tab status markers: `emoji`, `ascii`, or `off`.
    #[serde(default = "default_status_markers")]
    pub status_markers: String,
    /// Empty = use `$SHELL`. Otherwise a shell binary name or path.
    #[serde(default)]
    pub default_shell: String,
    /// `launch` = directory where the user started vmux; `home` = $HOME.
    #[serde(default = "default_default_cwd")]
    pub default_cwd: String,
    /// Capture mouse in the attach UI (click, drag, wheel).
    #[serde(default = "default_true")]
    pub mouse: bool,
    /// Show × on workspace tabs when more than one tab exists.
    #[serde(default = "default_true")]
    pub tab_close_button: bool,
    /// Terminal bell when a pane enters attention / needs-input.
    #[serde(default)]
    pub bell_on_attention: bool,
    /// Auto-hide the workspace sidebar on narrow terminals (burger + picker).
    #[serde(default = "default_true")]
    pub sidebar_responsive: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            sidebar_collapsed: false,
            sidebar_width: default_sidebar_width(),
            sidebar_fit: false,
            prefix_key: default_prefix_key(),
            scroll_step: default_scroll_step(),
            theme: default_theme(),
            workspace_second_line: default_workspace_second_line(),
            cursor_blink: true,
            cursor_blink_ms: default_cursor_blink_ms(),
            status_markers: default_status_markers(),
            default_shell: String::new(),
            default_cwd: default_default_cwd(),
            mouse: true,
            tab_close_button: true,
            bell_on_attention: false,
            sidebar_responsive: true,
        }
    }
}

impl LmuxConfig {
    pub fn normalized(mut self) -> Self {
        if self.ui.prefix_key.trim().is_empty() {
            self.ui.prefix_key = default_prefix_key();
        } else {
            self.ui.prefix_key = self.ui.prefix_key.trim().to_string();
        }
        self.ui.scroll_step = self.ui.scroll_step.clamp(1, 50);
        self.ui.sidebar_width = clamp_sidebar_width(self.ui.sidebar_width);
        self.ui.theme = normalize_theme(&self.ui.theme);
        self.ui.workspace_second_line =
            normalize_workspace_second_line(&self.ui.workspace_second_line);
        self.ui.cursor_blink_ms = self.ui.cursor_blink_ms.clamp(200, 5000);
        self.ui.status_markers = normalize_status_markers(&self.ui.status_markers);
        self.ui.default_shell = self.ui.default_shell.trim().to_string();
        self.ui.default_cwd = normalize_default_cwd(&self.ui.default_cwd);
        self.relay.bind = normalize_relay_bind(&self.relay.bind);
        if self.relay.port == 0 {
            self.relay.port = 4399;
        }
        self
    }
}

pub const SIDEBAR_MIN_WIDTH: u16 = 12;
pub const SIDEBAR_MAX_WIDTH: u16 = 60;
pub const SIDEBAR_COLLAPSED_WIDTH: u16 = 6;

fn default_sidebar_width() -> u16 {
    24
}

fn default_true() -> bool {
    true
}

fn default_cursor_blink_ms() -> u64 {
    1000
}

fn default_status_markers() -> String {
    "emoji".to_string()
}

fn default_default_cwd() -> String {
    "launch".to_string()
}

pub fn clamp_sidebar_width(width: u16) -> u16 {
    width.clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH)
}

pub fn set_value(config: &mut LmuxConfig, key: &str, value: &str) -> Result<()> {
    match key {
        "ui.sidebar_collapsed" => {
            config.ui.sidebar_collapsed = parse_bool(value)?;
        }
        "ui.sidebar_width" => {
            let width = value
                .parse::<u16>()
                .map_err(|_| anyhow!("ui.sidebar_width must be an integer"))?;
            config.ui.sidebar_width = clamp_sidebar_width(width);
        }
        "ui.sidebar_fit" => {
            config.ui.sidebar_fit = parse_bool(value)?;
        }
        "ui.prefix_key" => {
            crate::input::parse_key_binding(value)
                .with_context(|| format!("invalid prefix key {value}"))?;
            config.ui.prefix_key = value.trim().to_string();
        }
        "ui.scroll_step" => {
            config.ui.scroll_step = value
                .parse::<usize>()
                .map_err(|_| anyhow!("ui.scroll_step must be a positive integer"))?;
        }
        "ui.theme" => {
            let normalized = value.trim().to_ascii_lowercase();
            if !supported_themes().contains(&normalized.as_str()) {
                return Err(anyhow!(
                    "ui.theme must be one of {}",
                    supported_themes().join(", ")
                ));
            }
            config.ui.theme = normalized;
        }
        "ui.workspace_second_line" => {
            let normalized = value.trim().to_ascii_lowercase();
            if !supported_workspace_second_lines().contains(&normalized.as_str()) {
                return Err(anyhow!(
                    "ui.workspace_second_line must be one of {}",
                    supported_workspace_second_lines().join(", ")
                ));
            }
            config.ui.workspace_second_line = normalized;
        }
        "ui.cursor_blink" => {
            config.ui.cursor_blink = parse_bool(value)?;
        }
        "ui.cursor_blink_ms" => {
            config.ui.cursor_blink_ms = value
                .parse::<u64>()
                .map_err(|_| anyhow!("ui.cursor_blink_ms must be an integer"))?;
        }
        "ui.status_markers" => {
            let normalized = value.trim().to_ascii_lowercase();
            if !supported_status_markers().contains(&normalized.as_str()) {
                return Err(anyhow!(
                    "ui.status_markers must be one of {}",
                    supported_status_markers().join(", ")
                ));
            }
            config.ui.status_markers = normalized;
        }
        "ui.default_shell" => {
            config.ui.default_shell = value.trim().to_string();
        }
        "ui.default_cwd" => {
            let normalized = value.trim().to_ascii_lowercase();
            if !supported_default_cwds().contains(&normalized.as_str()) {
                return Err(anyhow!(
                    "ui.default_cwd must be one of {}",
                    supported_default_cwds().join(", ")
                ));
            }
            config.ui.default_cwd = normalized;
        }
        "ui.mouse" => {
            config.ui.mouse = parse_bool(value)?;
        }
        "ui.tab_close_button" => {
            config.ui.tab_close_button = parse_bool(value)?;
        }
        "ui.bell_on_attention" => {
            config.ui.bell_on_attention = parse_bool(value)?;
        }
        "ui.sidebar_responsive" => {
            config.ui.sidebar_responsive = parse_bool(value)?;
        }
        "relay.enabled" => {
            config.relay.enabled = parse_bool(value)?;
        }
        "relay.bind" => {
            let normalized = value.trim().to_ascii_lowercase();
            if !supported_relay_binds().contains(&normalized.as_str()) {
                return Err(anyhow!(
                    "relay.bind must be one of {}",
                    supported_relay_binds().join(", ")
                ));
            }
            config.relay.bind = normalized;
        }
        "relay.port" => {
            config.relay.port = value
                .parse::<u16>()
                .map_err(|_| anyhow!("relay.port must be a port number"))?;
            if config.relay.port == 0 {
                return Err(anyhow!("relay.port must be non-zero"));
            }
        }
        "relay.allow_localhost" => {
            config.relay.allow_localhost = parse_bool(value)?;
        }
        "relay.allow_tailnet_cgnat" => {
            config.relay.allow_tailnet_cgnat = parse_bool(value)?;
        }
        "relay.allow_paste" => {
            config.relay.allow_paste = parse_bool(value)?;
        }
        "agent_titles.enabled" => {
            config.agent_titles.enabled = parse_bool(value)?;
        }
        "agent_titles.llm_fallback" => {
            config.agent_titles.llm_fallback = parse_bool(value)?;
        }
        "agent_titles.llm_command" => {
            let command = value.trim();
            shell_words::split(command).with_context(|| {
                format!("agent_titles.llm_command is not a valid command: {command}")
            })?;
            config.agent_titles.llm_command = command.to_string();
        }
        "agent_titles.llm_delay_ms" => {
            config.agent_titles.llm_delay_ms = value
                .parse::<u64>()
                .map_err(|_| anyhow!("agent_titles.llm_delay_ms must be an integer"))?;
        }
        other => return Err(anyhow!("unknown config key {other}")),
    }
    *config = config.clone().normalized();
    Ok(())
}

pub fn save_to_path(path: &Path, config: &LmuxConfig) -> Result<()> {
    // If the config path is a symlink (e.g. dotfiles-managed), write THROUGH it
    // to the real target. Renaming over the link itself would replace it with a
    // regular file and silently detach it from the user's dotfiles repo.
    let target = match fs::read_link(path) {
        Ok(link) if link.is_absolute() => link,
        Ok(link) => path.parent().map(|p| p.join(&link)).unwrap_or(link),
        Err(_) => path.to_path_buf(),
    };
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    // Atomic write (tmp + rename in the target's dir) so a crash mid-write cannot
    // leave empty config.
    let tmp = target.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(config)?;
    fs::write(&tmp, &bytes).with_context(|| format!("write {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
    }
    fs::rename(&tmp, &target).with_context(|| format!("rename to {}", target.display()))?;
    Ok(())
}

fn parse_bool(value: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        _ => Err(anyhow!("boolean value must be true or false")),
    }
}

pub fn default_prefix_key() -> String {
    "Ctrl-b".to_string()
}

pub fn default_scroll_step() -> usize {
    5
}

pub fn default_theme() -> String {
    "midnight".to_string()
}

pub fn default_workspace_second_line() -> String {
    "path".to_string()
}

pub fn supported_themes() -> Vec<&'static str> {
    vec![
        "midnight",
        "daylight",
        "contrast",
        "nord",
        "dracula",
        "gruvbox",
        "catppuccin",
        "solarized-dark",
        "solarized-light",
        "tokyo-night",
        "forest",
        "rose-pine",
        "ocean",
        "ember",
        "monokai",
    ]
}

pub fn supported_workspace_second_lines() -> Vec<&'static str> {
    vec![
        "path", "details", "branch", "id", "status", "cursor", "none",
    ]
}

pub fn supported_status_markers() -> Vec<&'static str> {
    vec!["emoji", "ascii", "off"]
}

pub fn supported_default_cwds() -> Vec<&'static str> {
    vec!["launch", "home"]
}

pub fn supported_relay_binds() -> Vec<&'static str> {
    // Intentionally no "all" / 0.0.0.0 — keeps the relay off the public LAN.
    vec!["auto", "tailscale", "local"]
}

fn normalize_relay_bind(value: &str) -> String {
    let normalized = value.trim().to_ascii_lowercase();
    // Migrate removed "all" (insecure all-interfaces) to safe auto.
    if normalized == "all" {
        return "auto".to_string();
    }
    if supported_relay_binds().contains(&normalized.as_str()) {
        normalized
    } else {
        "auto".to_string()
    }
}

/// Common prefix keys the settings UI can cycle (must parse via `parse_key_binding`).
pub fn prefix_key_choices() -> Vec<&'static str> {
    vec!["Ctrl-b", "Ctrl-a", "Ctrl-Space", "Alt-a", "Alt-b"]
}

pub fn scroll_step_choices() -> Vec<usize> {
    vec![1, 3, 5, 10, 15, 20]
}

pub fn cursor_blink_ms_choices() -> Vec<u64> {
    vec![500, 1000, 1500, 2000]
}

pub fn default_shell_choices() -> Vec<&'static str> {
    // Empty string means system $SHELL.
    vec!["", "bash", "zsh", "fish", "sh"]
}

fn normalize_theme(value: &str) -> String {
    let normalized = value.trim().to_ascii_lowercase();
    if supported_themes().contains(&normalized.as_str()) {
        normalized
    } else {
        default_theme()
    }
}

fn normalize_workspace_second_line(value: &str) -> String {
    let normalized = value.trim().to_ascii_lowercase();
    if supported_workspace_second_lines().contains(&normalized.as_str()) {
        normalized
    } else {
        default_workspace_second_line()
    }
}

fn normalize_status_markers(value: &str) -> String {
    let normalized = value.trim().to_ascii_lowercase();
    if supported_status_markers().contains(&normalized.as_str()) {
        normalized
    } else {
        default_status_markers()
    }
}

fn normalize_default_cwd(value: &str) -> String {
    let normalized = value.trim().to_ascii_lowercase();
    if supported_default_cwds().contains(&normalized.as_str()) {
        normalized
    } else {
        default_default_cwd()
    }
}

/// Resolve the shell used when a pane command is empty.
pub fn resolve_default_shell() -> String {
    if let Ok(config) = load() {
        let shell = config.ui.default_shell.trim();
        if !shell.is_empty() {
            return shell.to_string();
        }
    }
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string())
}

/// Resolve workspace/pane default directory from config + launch env.
pub fn resolve_default_cwd_path() -> PathBuf {
    let mode = load()
        .map(|c| c.ui.default_cwd)
        .unwrap_or_else(|_| default_default_cwd());
    if mode == "home" {
        if let Some(home) = dirs::home_dir() {
            if home.is_dir() {
                return home;
            }
        }
    }
    crate::model::launch_cwd()
}

pub fn load() -> Result<LmuxConfig> {
    let path = paths::config_path()?;
    if !path.exists() {
        return Ok(LmuxConfig::default());
    }
    load_from_path(&path)
}

/// Load config for read-only use. Malformed files fall back to defaults with a
/// warning so doctor/status still work.
pub fn load_from_path(path: &Path) -> Result<LmuxConfig> {
    match load_from_path_strict(path) {
        Ok(config) => Ok(config),
        Err(err) => {
            eprintln!(
                "warning: ignoring malformed config at {} ({err}); using defaults",
                path.display()
            );
            Ok(LmuxConfig::default())
        }
    }
}

/// Load config for mutating commands (`config set`, Settings panel). Fails closed
/// on parse errors so a typo cannot be overwritten with defaults (bugs.md P1#5).
pub fn load_for_mutation() -> Result<(std::path::PathBuf, LmuxConfig)> {
    let path = paths::config_path()?;
    // Distinguish "genuinely absent" from "broken symlink". `exists()` follows
    // symlinks, so a dotfiles link whose target is temporarily missing would
    // read as absent → defaults → the save then destroys the link. Detect the
    // link with symlink_metadata and fail closed instead.
    match fs::symlink_metadata(&path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok((path, LmuxConfig::default()));
        }
        Err(err) => {
            return Err(err).with_context(|| format!("stat config {}", path.display()));
        }
        Ok(_) => {}
    }
    if !path.exists() {
        return Err(anyhow!(
            "config path {} is a broken symlink; refusing to overwrite with defaults \
             (fix or remove the link target first)",
            path.display()
        ));
    }
    let config = load_from_path_strict(&path)?;
    Ok((path, config))
}

pub fn load_from_path_strict(path: &Path) -> Result<LmuxConfig> {
    let contents = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let config: LmuxConfig = serde_json::from_str(&contents).with_context(|| {
        format!(
            "parse config at {} (refusing to overwrite malformed file; fix or delete it first)",
            path.display()
        )
    })?;
    Ok(config.normalized())
}

pub fn write_default(path: &Path, force: bool) -> Result<()> {
    if path.exists() && !force {
        anyhow::bail!("config already exists at {}", path.display());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    save_to_path(path, &LmuxConfig::default())
}

/// Cycle through a list of string options by delta (+1 / -1).
pub fn cycle_choice(choices: &[&str], current: &str, delta: isize) -> String {
    if choices.is_empty() {
        return current.to_string();
    }
    let cur = current.trim();
    let idx = choices
        .iter()
        .position(|c| c.eq_ignore_ascii_case(cur))
        .unwrap_or(0);
    let next = (idx as isize + delta).rem_euclid(choices.len() as isize) as usize;
    choices[next].to_string()
}

pub fn cycle_usize(choices: &[usize], current: usize, delta: isize) -> usize {
    if choices.is_empty() {
        return current;
    }
    let idx = choices.iter().position(|c| *c == current).unwrap_or(0);
    let next = (idx as isize + delta).rem_euclid(choices.len() as isize) as usize;
    choices[next]
}

pub fn cycle_u64(choices: &[u64], current: u64, delta: isize) -> u64 {
    if choices.is_empty() {
        return current;
    }
    let idx = choices.iter().position(|c| *c == current).unwrap_or(0);
    let next = (idx as isize + delta).rem_euclid(choices.len() as isize) as usize;
    choices[next]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_scroll_step_bounds() {
        let mut low = LmuxConfig::default();
        low.ui.scroll_step = 0;
        assert_eq!(low.normalized().ui.scroll_step, 1);
        let mut high = LmuxConfig::default();
        high.ui.scroll_step = 500;
        assert_eq!(high.normalized().ui.scroll_step, 50);
    }

    #[test]
    fn loads_partial_config_with_defaults() {
        let dir = std::env::temp_dir().join(format!("vmux-config-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        fs::write(&path, r#"{"ui":{"sidebar_collapsed":true}}"#).unwrap();
        let config = load_from_path(&path).unwrap();
        fs::remove_dir_all(dir).ok();
        assert!(config.ui.sidebar_collapsed);
        assert_eq!(config.ui.prefix_key, "Ctrl-b");
        assert_eq!(config.ui.scroll_step, 5);
        assert_eq!(config.ui.theme, "midnight");
        assert_eq!(config.ui.workspace_second_line, "path");
        assert!(config.ui.cursor_blink);
        assert_eq!(config.ui.cursor_blink_ms, 1000);
        assert_eq!(config.ui.status_markers, "emoji");
        assert_eq!(config.ui.default_cwd, "launch");
        assert!(config.ui.mouse);
        assert!(config.ui.tab_close_button);
        assert!(!config.ui.bell_on_attention);
    }

    #[test]
    fn malformed_config_falls_back_to_defaults() {
        let dir = std::env::temp_dir().join(format!("vmux-config-bad-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        fs::write(&path, "{ this is not valid json ").unwrap();
        let config = load_from_path(&path).unwrap();
        fs::remove_dir_all(dir).ok();
        assert_eq!(config, LmuxConfig::default());
    }

    #[test]
    fn malformed_config_strict_load_fails() {
        let dir = std::env::temp_dir().join(format!("vmux-config-strict-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        fs::write(&path, "{ this is not valid json ").unwrap();
        let err = load_from_path_strict(&path).unwrap_err().to_string();
        fs::remove_dir_all(dir).ok();
        assert!(
            err.contains("refusing to overwrite") || err.contains("parse config"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn trims_or_defaults_prefix_key() {
        let mut config = LmuxConfig::default();
        config.ui.prefix_key = " Alt-x ".to_string();
        config.ui.theme = "MIDNIGHT".to_string();
        config.ui.workspace_second_line = "DETAILS".to_string();
        let n = config.normalized();
        assert_eq!(n.ui.prefix_key, "Alt-x");
        assert_eq!(n.ui.theme, "midnight");
        assert_eq!(n.ui.workspace_second_line, "details");

        let mut empty = LmuxConfig::default();
        empty.ui.prefix_key = " ".to_string();
        empty.ui.theme = "unknown".to_string();
        let empty_n = empty.normalized();
        assert_eq!(empty_n.ui.prefix_key, "Ctrl-b");
        assert_eq!(empty_n.ui.theme, "midnight");
    }

    #[test]
    fn set_value_updates_known_keys_and_normalizes() {
        let mut config = LmuxConfig::default();
        set_value(&mut config, "ui.sidebar_collapsed", "yes").unwrap();
        set_value(&mut config, "ui.prefix_key", " Alt-x ").unwrap();
        set_value(&mut config, "ui.scroll_step", "500").unwrap();
        set_value(&mut config, "ui.theme", "contrast").unwrap();
        set_value(&mut config, "ui.workspace_second_line", "details").unwrap();
        set_value(&mut config, "ui.cursor_blink", "off").unwrap();
        set_value(&mut config, "ui.cursor_blink_ms", "500").unwrap();
        set_value(&mut config, "ui.status_markers", "ascii").unwrap();
        set_value(&mut config, "ui.default_shell", "zsh").unwrap();
        set_value(&mut config, "ui.default_cwd", "home").unwrap();
        set_value(&mut config, "ui.mouse", "false").unwrap();
        set_value(&mut config, "ui.tab_close_button", "0").unwrap();
        set_value(&mut config, "ui.bell_on_attention", "on").unwrap();
        assert!(config.relay.allow_paste); // default on
        set_value(&mut config, "relay.allow_paste", "false").unwrap();
        assert!(!config.relay.allow_paste);
        assert!(config.ui.sidebar_collapsed);
        assert_eq!(config.ui.prefix_key, "Alt-x");
        assert_eq!(config.ui.scroll_step, 50);
        assert_eq!(config.ui.theme, "contrast");
        assert_eq!(config.ui.workspace_second_line, "details");
        assert!(!config.ui.cursor_blink);
        assert_eq!(config.ui.cursor_blink_ms, 500);
        assert_eq!(config.ui.status_markers, "ascii");
        assert_eq!(config.ui.default_shell, "zsh");
        assert_eq!(config.ui.default_cwd, "home");
        assert!(!config.ui.mouse);
        assert!(!config.ui.tab_close_button);
        assert!(config.ui.bell_on_attention);
    }

    #[test]
    fn set_value_rejects_unknown_or_invalid_values() {
        let mut config = LmuxConfig::default();
        assert!(set_value(&mut config, "ui.sidebar_collapsed", "maybe").is_err());
        assert!(set_value(&mut config, "ui.prefix_key", "Ctrl-UnknownKey").is_err());
        assert!(set_value(&mut config, "ui.theme", "neon").is_err());
        assert!(set_value(&mut config, "ui.status_markers", "icons").is_err());
        assert!(set_value(&mut config, "ui.default_cwd", "root").is_err());
        assert!(set_value(&mut config, "missing", "value").is_err());
    }

    #[test]
    fn cycle_choice_wraps() {
        assert_eq!(cycle_choice(&["a", "b", "c"], "b", 1), "c");
        assert_eq!(cycle_choice(&["a", "b", "c"], "c", 1), "a");
        assert_eq!(cycle_choice(&["a", "b", "c"], "a", -1), "c");
    }
}
