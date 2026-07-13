//! Install and detect coding-agent sidebar status hooks (shell, Claude Code, Codex, Grok).
//!
//! Each integration reports status into vmux via `vmux hooks event` so the workspace
//! sidebar can show 🔄 busy / 🙋 needs input / ✅ done / ❌ error.

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

/// Substring present in every vmux-managed agent hook command (for detection).
/// Stable even when the binary path is shell-quoted (`'/path/vmux' hooks event`).
pub const VMUX_HOOK_MARKER: &str = "hooks event --pane";

/// Build the shell command installed into Claude/Codex hooks.
///
/// Prefers an absolute path to this `vmux` binary so hooks work even when the
/// agent's PATH does not include `vmux` (common with cargo-built binaries).
pub fn agent_hook_command() -> String {
    let vmux = resolve_vmux_bin();
    // Quote for shell; fall back to bare `vmux` if resolution fails.
    let bin = shell_words::quote(&vmux);
    // Prefer VMUX_*; fall back to legacy LMUX_* so old Claude settings still
    // route Stop → ✅. Empty pane is handled by the daemon as "active pane".
    format!(
        "cat | {bin} hooks event --pane \"${{VMUX_PANE_ID:-${{LMUX_PANE_ID:-}}}}\" --session \"${{VMUX_SESSION:-${{LMUX_SESSION:-default}}}}\" >/dev/null 2>&1 || cat >/dev/null; printf '%s\\n' '{{}}'"
    )
}

/// True when installed hook commands still use pre-rename env/binary names and
/// will fail to mark Claude/Codex as done (🔄 stuck after Stop).
pub fn hooks_look_stale(content: &str) -> bool {
    if !content.contains(VMUX_HOOK_MARKER) {
        return false;
    }
    // Fresh hooks may still mention LMUX_* as a fallback after VMUX_*.
    // Stale = legacy only (no VMUX_PANE_ID) or still invoking the old `lmux` binary.
    let uses_legacy_pane_only =
        content.contains("LMUX_PANE_ID") && !content.contains("VMUX_PANE_ID");
    let uses_legacy_bin = (content.contains("/lmux ")
        || content.contains("/lmux\"")
        || content.contains("| lmux ")
        || content.contains("|lmux "))
        && !content.contains("/vmux ")
        && !content.contains("/vmux\"")
        && !content.contains("| vmux ")
        && !content.contains("|vmux ");
    uses_legacy_pane_only || uses_legacy_bin
}

fn resolve_vmux_bin() -> String {
    if let Ok(exe) = std::env::current_exe() {
        if exe.is_file() {
            return exe.display().to_string();
        }
    }
    "vmux".to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntegrationKind {
    Shell,
    Claude,
    Codex,
    Grok,
}

impl IntegrationKind {
    pub fn all() -> &'static [IntegrationKind] {
        &[
            IntegrationKind::Shell,
            IntegrationKind::Claude,
            IntegrationKind::Codex,
            IntegrationKind::Grok,
        ]
    }

    pub fn id(self) -> &'static str {
        match self {
            IntegrationKind::Shell => "shell",
            IntegrationKind::Claude => "claude",
            IntegrationKind::Codex => "codex",
            IntegrationKind::Grok => "grok",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            IntegrationKind::Shell => "shell hooks",
            IntegrationKind::Claude => "claude code",
            IntegrationKind::Codex => "codex",
            IntegrationKind::Grok => "grok skill",
        }
    }

    pub fn parse(name: &str) -> Result<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "shell" | "bash" | "zsh" => Ok(Self::Shell),
            "claude" | "claude-code" | "claudecode" => Ok(Self::Claude),
            "codex" | "openai" => Ok(Self::Codex),
            "grok" | "grok-build" | "skill" => Ok(Self::Grok),
            "all" => Err(anyhow!("use install without --agent for all integrations")),
            other => Err(anyhow!(
                "unknown agent integration '{other}' (shell, claude, codex, grok)"
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallState {
    /// vmux hook is present and looks complete.
    Installed,
    /// Config/tool exists but vmux hook is missing or incomplete.
    Missing,
    /// Tool config directory not found (optional agent not used yet).
    NotDetected,
}

impl InstallState {
    pub fn label(&self) -> &'static str {
        match self {
            InstallState::Installed => "installed",
            InstallState::Missing => "missing",
            InstallState::NotDetected => "not detected",
        }
    }
}

#[derive(Debug, Clone)]
pub struct IntegrationStatus {
    pub kind: IntegrationKind,
    pub state: InstallState,
    pub path: PathBuf,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct InstallResult {
    pub kind: IntegrationKind,
    pub path: PathBuf,
    pub changed: bool,
    pub detail: String,
}

pub fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

pub fn shell_hooks_path(home: &Path) -> PathBuf {
    config_home(home).join("vmux").join("hooks.sh")
}

pub fn claude_settings_path(home: &Path) -> PathBuf {
    home.join(".claude").join("settings.json")
}

pub fn codex_hooks_path(home: &Path) -> PathBuf {
    home.join(".codex").join("hooks.json")
}

pub fn grok_skill_path(home: &Path) -> PathBuf {
    home.join(".grok")
        .join("skills")
        .join("vmux-control")
        .join("SKILL.md")
}

fn config_home(home: &Path) -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"))
}

/// Status for every supported coding-agent integration.
pub fn status_report() -> Vec<IntegrationStatus> {
    status_report_in(&home_dir())
}

pub fn status_report_in(home: &Path) -> Vec<IntegrationStatus> {
    IntegrationKind::all()
        .iter()
        .copied()
        .map(|kind| status_for(kind, home))
        .collect()
}

pub fn status_for(kind: IntegrationKind, home: &Path) -> IntegrationStatus {
    match kind {
        IntegrationKind::Shell => shell_status(home),
        IntegrationKind::Claude => claude_status(home),
        IntegrationKind::Codex => codex_status(home),
        IntegrationKind::Grok => grok_status(home),
    }
}

fn shell_status(home: &Path) -> IntegrationStatus {
    let path = shell_hooks_path(home);
    let legacy_path = config_home(home).join("lmux").join("hooks.sh");
    let (state, detail) = if path.is_file() {
        let content = fs::read_to_string(&path).unwrap_or_default();
        if content.contains("lmux_hook_status")
            && !content.contains("vmux_hook_status")
            && !content.contains("_vmux_pane_id")
        {
            (
                InstallState::Missing,
                "stale lmux shell hooks — run: vmux hooks install --agent shell".to_string(),
            )
        } else if content.contains("vmux_hook_status")
            && (content.contains("vmux set-status")
                || content.contains("_vmux_bin")
                || content.contains("set-status"))
        {
            (
                InstallState::Installed,
                "eval \"$(vmux hooks shell)\" helpers present".to_string(),
            )
        } else {
            (
                InstallState::Missing,
                "hooks.sh exists but is incomplete".to_string(),
            )
        }
    } else if legacy_path.is_file() {
        (
            InstallState::Missing,
            format!(
                "legacy {} found — run: vmux hooks install --agent shell",
                legacy_path.display()
            ),
        )
    } else {
        (
            InstallState::Missing,
            "run: vmux hooks install --agent shell".to_string(),
        )
    };
    IntegrationStatus {
        kind: IntegrationKind::Shell,
        state,
        path,
        detail,
    }
}

fn claude_status(home: &Path) -> IntegrationStatus {
    let path = claude_settings_path(home);
    let claude_dir = home.join(".claude");
    if !claude_dir.is_dir() && !path.is_file() {
        return IntegrationStatus {
            kind: IntegrationKind::Claude,
            state: InstallState::NotDetected,
            path,
            detail: "~/.claude not found (Claude Code not installed yet)".to_string(),
        };
    }
    if !path.is_file() {
        return IntegrationStatus {
            kind: IntegrationKind::Claude,
            state: InstallState::Missing,
            path,
            detail: "settings.json missing vmux Stop/Notification hooks".to_string(),
        };
    }
    let content = fs::read_to_string(&path).unwrap_or_default();
    let has_marker = content.contains(VMUX_HOOK_MARKER);
    let has_stop = content.contains("\"Stop\"") && has_marker;
    let has_notification = content.contains("\"Notification\"") && has_marker;
    let has_prompt = content.contains("\"UserPromptSubmit\"") && has_marker;
    let (state, detail) = if hooks_look_stale(&content) {
        (
            InstallState::Missing,
            "stale lmux hooks (LMUX_PANE_ID / old binary) — run: vmux hooks install --agent claude"
                .to_string(),
        )
    } else if has_stop && has_notification && has_prompt {
        (
            InstallState::Installed,
            "UserPromptSubmit→🔄 Stop→✅ Notification→🙋".to_string(),
        )
    } else if has_marker {
        (
            InstallState::Missing,
            "partial vmux hooks (need Stop + Notification + UserPromptSubmit)".to_string(),
        )
    } else {
        (
            InstallState::Missing,
            "no vmux hooks in settings.json".to_string(),
        )
    };
    IntegrationStatus {
        kind: IntegrationKind::Claude,
        state,
        path,
        detail,
    }
}

fn codex_status(home: &Path) -> IntegrationStatus {
    let path = codex_hooks_path(home);
    let codex_dir = home.join(".codex");
    if !codex_dir.is_dir() && !path.is_file() {
        return IntegrationStatus {
            kind: IntegrationKind::Codex,
            state: InstallState::NotDetected,
            path,
            detail: "~/.codex not found (Codex not installed yet)".to_string(),
        };
    }
    if !path.is_file() {
        return IntegrationStatus {
            kind: IntegrationKind::Codex,
            state: InstallState::Missing,
            path,
            detail: "hooks.json missing vmux Stop/PermissionRequest hooks".to_string(),
        };
    }
    let content = fs::read_to_string(&path).unwrap_or_default();
    let has_marker = content.contains(VMUX_HOOK_MARKER);
    let has_stop = content.contains("\"Stop\"") && has_marker;
    let has_permission = content.contains("\"PermissionRequest\"") && has_marker;
    let has_busy = content.contains("\"PreToolUse\"") && has_marker;
    let (state, detail) = if hooks_look_stale(&content) {
        (
            InstallState::Missing,
            "stale lmux hooks — run: vmux hooks install --agent codex".to_string(),
        )
    } else if has_stop && has_permission && has_busy {
        (
            InstallState::Installed,
            "UserPromptSubmit/PreToolUse→🔄 PermissionRequest→🙋 Stop→✅".to_string(),
        )
    } else if has_marker {
        (
            InstallState::Missing,
            "partial vmux hooks (need Stop + PermissionRequest + PreToolUse)".to_string(),
        )
    } else {
        (
            InstallState::Missing,
            "no vmux hooks in hooks.json".to_string(),
        )
    };
    IntegrationStatus {
        kind: IntegrationKind::Codex,
        state,
        path,
        detail,
    }
}

fn grok_status(home: &Path) -> IntegrationStatus {
    let path = grok_skill_path(home);
    let grok_dir = home.join(".grok");
    if !grok_dir.is_dir() && !path.is_file() {
        return IntegrationStatus {
            kind: IntegrationKind::Grok,
            state: InstallState::NotDetected,
            path,
            detail: "~/.grok not found (Grok Build not installed yet)".to_string(),
        };
    }
    if path.is_file() {
        let content = fs::read_to_string(&path).unwrap_or_default();
        if content.contains("lmux ") && !content.contains("vmux ") {
            IntegrationStatus {
                kind: IntegrationKind::Grok,
                state: InstallState::Missing,
                path,
                detail: "stale lmux skill — run: vmux hooks install --agent grok".to_string(),
            }
        } else if content.contains("set-status") && content.contains("vmux") {
            IntegrationStatus {
                kind: IntegrationKind::Grok,
                state: InstallState::Installed,
                path,
                detail: "vmux-control skill installed".to_string(),
            }
        } else {
            IntegrationStatus {
                kind: IntegrationKind::Grok,
                state: InstallState::Missing,
                path,
                detail: "skill file incomplete — run: vmux hooks install --agent grok".to_string(),
            }
        }
    } else {
        IntegrationStatus {
            kind: IntegrationKind::Grok,
            state: InstallState::Missing,
            path,
            detail: "vmux-control skill not installed".to_string(),
        }
    }
}

/// Install one integration under `home` (tests pass a temp dir).
pub fn install_one(kind: IntegrationKind, home: &Path) -> Result<InstallResult> {
    match kind {
        IntegrationKind::Shell => install_shell(home),
        IntegrationKind::Claude => install_claude(home),
        IntegrationKind::Codex => install_codex(home),
        IntegrationKind::Grok => install_grok(home),
    }
}

pub fn install_all(home: &Path) -> Result<Vec<InstallResult>> {
    let mut results = Vec::new();
    for kind in IntegrationKind::all() {
        results.push(install_one(*kind, home)?);
    }
    Ok(results)
}

fn install_shell(home: &Path) -> Result<InstallResult> {
    let path = shell_hooks_path(home);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = crate::shell_hooks_bash();
    let previous = fs::read_to_string(&path).unwrap_or_default();
    let changed = previous != content;
    if changed {
        write_atomic(&path, content)?;
    }

    // Best-effort: append source line to bashrc/zshrc when they exist.
    let source_line = format!(". {}", shell_words::quote(&path.display().to_string()));
    let legacy_source = format!(
        ". {}",
        shell_words::quote(
            &config_home(home)
                .join("lmux")
                .join("hooks.sh")
                .display()
                .to_string()
        )
    );
    for rc_name in [".bashrc", ".zshrc"] {
        let rc = home.join(rc_name);
        if rc.is_file() || rc_name == ".bashrc" {
            // Prefer the new vmux path; comment out a legacy lmux source line if present.
            rewrite_legacy_shell_source(&rc, &legacy_source, &source_line)?;
            append_source_once(&rc, &source_line)?;
        }
    }

    Ok(InstallResult {
        kind: IntegrationKind::Shell,
        path,
        changed,
        detail: "shell helpers written; sourced from bashrc/zshrc when present".to_string(),
    })
}

/// Comment out a legacy `lmux` hooks source line so it does not fight with vmux.
fn rewrite_legacy_shell_source(path: &Path, legacy_line: &str, new_line: &str) -> Result<()> {
    let Ok(existing) = fs::read_to_string(path) else {
        return Ok(());
    };
    if !existing.lines().any(|line| line.trim() == legacy_line) {
        return Ok(());
    }
    if existing.lines().any(|line| line.trim() == new_line) {
        // New line already present: just comment the old one.
    }
    let mut updated = String::new();
    for line in existing.lines() {
        if line.trim() == legacy_line {
            updated.push_str("# migrated to vmux — was: ");
            updated.push_str(line);
            updated.push('\n');
        } else {
            updated.push_str(line);
            updated.push('\n');
        }
    }
    if !existing.ends_with('\n') && updated.ends_with('\n') {
        // keep trailing newline style
    }
    write_atomic(path, &updated)?;
    Ok(())
}

pub(crate) fn append_source_once(path: &Path, source_line: &str) -> Result<()> {
    // Only treat missing file as empty; fail closed on other I/O.
    let existing = match fs::read(path) {
        Ok(bytes) => String::from_utf8(bytes).with_context(|| {
            format!(
                "shell rc {} is not valid UTF-8; refusing to rewrite",
                path.display()
            )
        })?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => {
            return Err(err).with_context(|| format!("read shell rc {}", path.display()));
        }
    };
    if existing.lines().any(|line| line.trim() == source_line) {
        return Ok(());
    }
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    // Byte-for-byte backup before first mutation.
    if path.is_file() {
        let backup = path.with_extension(format!(
            "{}vmux-bak",
            path.extension()
                .and_then(|e| e.to_str())
                .map(|e| format!("{e}."))
                .unwrap_or_default()
        ));
        if !backup.exists() {
            fs::copy(path, &backup)
                .with_context(|| format!("backup {} → {}", path.display(), backup.display()))?;
        }
    }
    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str("\n# vmux shell hooks\n");
    updated.push_str(source_line);
    updated.push('\n');
    write_atomic(path, &updated)?;
    Ok(())
}

fn read_json_object_file(path: &Path) -> Result<(String, Value)> {
    match fs::read(path) {
        Ok(bytes) => {
            let previous = String::from_utf8(bytes).with_context(|| {
                format!("{} is not valid UTF-8; refusing to rewrite", path.display())
            })?;
            let root: Value = serde_json::from_str(&previous).with_context(|| {
                format!(
                    "parse {} failed; refusing to replace with defaults (fix the file first)",
                    path.display()
                )
            })?;
            if !root.is_object() {
                anyhow::bail!(
                    "{} root must be a JSON object; refusing to rewrite",
                    path.display()
                );
            }
            Ok((previous, root))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(("{}".to_string(), json!({}))),
        Err(err) => Err(err).with_context(|| format!("read {}", path.display())),
    }
}

/// Write `contents` to `path` atomically: write a sibling temp file, then
/// rename it over the target. A crash / full disk / power loss can then only
/// ever leave the old file or the new file intact, never a truncated one
/// (fs::write truncates in place and could otherwise destroy ~/.claude settings).
fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!("{e}."))
        .unwrap_or_default();
    let tmp = path.with_extension(format!("{ext}vmux-tmp.{}", std::process::id()));
    fs::write(&tmp, contents).with_context(|| format!("write {}", tmp.display()))?;
    if let Err(err) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(err).with_context(|| format!("rename {} → {}", tmp.display(), path.display()));
    }
    Ok(())
}

fn backup_file_bytes(path: &Path, previous: &str) -> Result<()> {
    if !path.is_file() || previous.contains(VMUX_HOOK_MARKER) {
        return Ok(());
    }
    let backup = path.with_extension("json.vmux-bak");
    if backup.exists() {
        return Ok(());
    }
    fs::write(&backup, previous)
        .with_context(|| format!("backup {} → {}", path.display(), backup.display()))
}

fn install_claude(home: &Path) -> Result<InstallResult> {
    let path = claude_settings_path(home);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let (previous, mut root) = read_json_object_file(&path)?;
    let hooks = root
        .as_object_mut()
        .unwrap()
        .entry("hooks")
        .or_insert_with(|| json!({}));
    if !hooks.is_object() {
        anyhow::bail!(
            "{} has a non-object \"hooks\" value; refusing to overwrite without confirmation",
            path.display()
        );
    }
    let hooks_obj = hooks.as_object_mut().unwrap();
    ensure_event_hook(hooks_obj, "Notification");
    ensure_event_hook(hooks_obj, "UserPromptSubmit");
    ensure_event_hook(hooks_obj, "PreToolUse");
    ensure_event_hook(hooks_obj, "PostToolUse");
    ensure_event_hook(hooks_obj, "Stop");
    ensure_event_hook(hooks_obj, "StopFailure");
    ensure_event_hook(hooks_obj, "PermissionRequest");

    let next = serde_json::to_string_pretty(&root)?;
    let next = format!("{next}\n");
    let changed = previous.trim() != next.trim();
    if changed {
        backup_file_bytes(&path, &previous)?;
        write_atomic(&path, &next)?;
    }
    Ok(InstallResult {
        kind: IntegrationKind::Claude,
        path,
        changed,
        detail: "UserPromptSubmit/PreToolUse→🔄 Notification/Permission→🙋 Stop→✅".to_string(),
    })
}

fn install_codex(home: &Path) -> Result<InstallResult> {
    let path = codex_hooks_path(home);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let (previous, mut root) = read_json_object_file(&path)?;
    let hooks = root
        .as_object_mut()
        .unwrap()
        .entry("hooks")
        .or_insert_with(|| json!({}));
    if !hooks.is_object() {
        anyhow::bail!(
            "{} has a non-object \"hooks\" value; refusing to overwrite without confirmation",
            path.display()
        );
    }
    let hooks_obj = hooks.as_object_mut().unwrap();
    // Lifecycle → sidebar emoji:
    //   UserPromptSubmit / PreToolUse / PostToolUse → 🔄 busy
    //   PermissionRequest → 🙋 needs input (then PreToolUse restores 🔄)
    //   Stop → ✅ done
    ensure_event_hook(hooks_obj, "UserPromptSubmit");
    ensure_event_hook(hooks_obj, "PreToolUse");
    ensure_event_hook(hooks_obj, "PostToolUse");
    ensure_event_hook(hooks_obj, "PermissionRequest");
    ensure_event_hook(hooks_obj, "Stop");

    let next = serde_json::to_string_pretty(&root)?;
    let next = format!("{next}\n");
    let changed = previous.trim() != next.trim();
    if changed {
        backup_file_bytes(&path, &previous)?;
        write_atomic(&path, &next)?;
    }
    Ok(InstallResult {
        kind: IntegrationKind::Codex,
        path,
        changed,
        detail: "UserPromptSubmit/PreToolUse→🔄 PermissionRequest→🙋 Stop→✅".to_string(),
    })
}

fn ensure_event_hook(hooks_obj: &mut serde_json::Map<String, Value>, event: &str) {
    let entry = hooks_obj
        .entry(event.to_string())
        .or_insert_with(|| json!([]));
    if !entry.is_array() {
        *entry = json!([]);
    }
    let groups = entry.as_array_mut().unwrap();
    let command = agent_hook_command();
    // Refresh an existing vmux handler command only when the path/command changed.
    if let Some(group) = groups.iter_mut().find(|g| group_has_vmux_hook(g)) {
        if let Some(hooks) = group.get_mut("hooks").and_then(|h| h.as_array_mut()) {
            for hook in hooks.iter_mut() {
                if hook
                    .get("command")
                    .and_then(|c| c.as_str())
                    .map(|c| c.contains(VMUX_HOOK_MARKER))
                    .unwrap_or(false)
                {
                    let current = hook.get("command").and_then(|c| c.as_str()).unwrap_or("");
                    if current != command {
                        hook["command"] = json!(command);
                    }
                    // Refresh legacy statusMessage branding.
                    if hook
                        .get("statusMessage")
                        .and_then(|s| s.as_str())
                        .map(|s| s.contains("lmux"))
                        .unwrap_or(false)
                    {
                        hook["statusMessage"] = json!("vmux status");
                    }
                    return;
                }
            }
        }
        return;
    }
    groups.push(json!({
        "matcher": "",
        "hooks": [{
            "type": "command",
            "command": command,
            "timeout": 10,
            "statusMessage": "vmux status"
        }]
    }));
}

fn group_has_vmux_hook(group: &Value) -> bool {
    group
        .get("hooks")
        .and_then(|h| h.as_array())
        .map(|hooks| {
            hooks.iter().any(|hook| {
                hook.get("command")
                    .and_then(|c| c.as_str())
                    .map(|c| c.contains(VMUX_HOOK_MARKER))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

fn install_grok(home: &Path) -> Result<InstallResult> {
    let path = grok_skill_path(home);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let content = crate::vmux_control_skill_markdown();
    let previous = fs::read_to_string(&path).unwrap_or_default();
    let changed = previous != content;
    if changed {
        write_atomic(&path, content)?;
    }
    Ok(InstallResult {
        kind: IntegrationKind::Grok,
        path,
        changed,
        detail: "vmux-control skill for Grok Build / agent workflows".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_home() -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("vmux-agent-hooks-{}-{}", std::process::id(), stamp));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn hooks_look_stale_detects_legacy_lmux_env() {
        let stale = r#"{"hooks":{"Stop":[{"hooks":[{"command":"cat | /tmp/lmux hooks event --pane \"${LMUX_PANE_ID:-}\""}]}]}}"#;
        assert!(hooks_look_stale(stale));
        let fresh = agent_hook_command();
        assert!(fresh.contains("VMUX_PANE_ID"));
        assert!(!hooks_look_stale(&format!(
            r#"{{"cmd":"{}"}}"#,
            fresh.replace('\\', "\\\\").replace('"', "\\\"")
        )));
    }

    #[test]
    fn install_all_is_idempotent_and_detectable() {
        let home = temp_home();
        // Pretend agents exist so status is Missing → Installed, not NotDetected.
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::create_dir_all(home.join(".codex")).unwrap();
        fs::create_dir_all(home.join(".grok")).unwrap();

        let first = install_all(&home).unwrap();
        assert_eq!(first.len(), 4);
        assert!(first.iter().all(|r| r.changed));

        let second = install_all(&home).unwrap();
        assert!(second.iter().all(|r| !r.changed));

        let status = status_report_in(&home);
        assert!(status.iter().all(|s| s.state == InstallState::Installed));

        let claude = fs::read_to_string(claude_settings_path(&home)).unwrap();
        assert!(claude.contains(VMUX_HOOK_MARKER));
        assert!(claude.contains("\"Stop\""));
        assert!(claude.contains("\"Notification\""));

        let codex = fs::read_to_string(codex_hooks_path(&home)).unwrap();
        assert!(codex.contains(VMUX_HOOK_MARKER));
        assert!(codex.contains("\"PermissionRequest\""));

        let skill = fs::read_to_string(grok_skill_path(&home)).unwrap();
        assert!(skill.contains("set-status"));

        fs::remove_dir_all(home).ok();
    }

    #[test]
    fn claude_merge_preserves_existing_hooks() {
        let home = temp_home();
        let path = claude_settings_path(&home);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [{ "type": "command", "command": "echo keep-me" }]
      }
    ]
  }
}
"#,
        )
        .unwrap();

        install_claude(&home).unwrap();
        let root: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let pre = root["hooks"]["PreToolUse"].as_array().unwrap();
        // Existing Bash matcher is kept; vmux adds its own matcher group.
        assert!(pre.len() >= 2);
        assert!(pre.iter().any(|g| g["hooks"][0]["command"]
            .as_str()
            .unwrap_or("")
            .contains("keep-me")));
        assert!(pre.iter().any(group_has_vmux_hook));
        assert!(!root["hooks"]["Stop"].as_array().unwrap().is_empty());
        assert!(group_has_vmux_hook(&root["hooks"]["Stop"][0]));

        fs::remove_dir_all(home).ok();
    }

    #[test]
    fn not_detected_when_agent_dirs_missing() {
        let home = temp_home();
        let status = status_report_in(&home);
        let claude = status
            .iter()
            .find(|s| s.kind == IntegrationKind::Claude)
            .unwrap();
        assert_eq!(claude.state, InstallState::NotDetected);
        let shell = status
            .iter()
            .find(|s| s.kind == IntegrationKind::Shell)
            .unwrap();
        assert_eq!(shell.state, InstallState::Missing);
        fs::remove_dir_all(home).ok();
    }
}
