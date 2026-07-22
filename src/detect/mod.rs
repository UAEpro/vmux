//! Herdr-style agent state detection from screen content + OSC title/progress.
//!
//! Authority model (matches herdr for Claude/Codex-class agents):
//! 1. Identify the agent from the pane command (or `VMUX_AGENT` / `HERDR_AGENT`).
//! 2. If a screen manifest exists, evaluate it against the live screen + OSC
//!    strings — that is the **primary** status source.
//! 3. Hooks / PTY keyword heuristics only apply when no manifest agent is
//!    detected (or as session-id plumbing for resume).

pub mod manifest;

use manifest::{detect_with_osc, DetectionInput};

/// Semantic state from screen manifests (herdr vocabulary).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectedState {
    Idle,
    Working,
    Blocked,
    Unknown,
}

/// Result of evaluating a pane's screen against an agent manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detection {
    pub agent: Option<&'static str>,
    pub state: DetectedState,
    pub skip_state_update: bool,
    pub visible_idle: bool,
    pub visible_blocker: bool,
    pub visible_working: bool,
    pub matched_rule: Option<String>,
    pub fallback_reason: Option<&'static str>,
}

/// Agents that ship a screen-content manifest (primary status authority).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ManifestAgent {
    Claude,
    Codex,
    Grok,
    Cursor,
    Gemini,
    OpenCode,
    Amp,
}

impl ManifestAgent {
    pub fn label(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Grok => "grok",
            Self::Cursor => "cursor",
            Self::Gemini => "gemini",
            Self::OpenCode => "opencode",
            Self::Amp => "amp",
        }
    }

    pub fn all() -> &'static [ManifestAgent] {
        &[
            Self::Claude,
            Self::Codex,
            Self::Grok,
            Self::Cursor,
            Self::Gemini,
            Self::OpenCode,
            Self::Amp,
        ]
    }

    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "claude" | "claude-code" | "claudecode" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            "grok" | "grok-build" => Some(Self::Grok),
            "cursor" | "cursor-agent" => Some(Self::Cursor),
            "gemini" => Some(Self::Gemini),
            "opencode" | "open-code" => Some(Self::OpenCode),
            "amp" | "amp-local" => Some(Self::Amp),
            _ => None,
        }
    }
}

/// Resolve which agent is running in a pane from command line / env hint.
pub fn agent_from_command(command: &str) -> Option<ManifestAgent> {
    // Wrapper hint (herdr-compatible): VMUX_AGENT=claude fence -- claude
    if let Ok(hint) = std::env::var("VMUX_AGENT").or_else(|_| std::env::var("HERDR_AGENT")) {
        if let Some(agent) = ManifestAgent::parse(&hint) {
            return Some(agent);
        }
    }
    let mut tokens = command.split_whitespace();
    let first = tokens.next()?;
    let base = token_basename(first);
    if let Some(agent) = ManifestAgent::parse(base) {
        return Some(agent);
    }
    // node /usr/.../bin/claude
    if matches!(
        base,
        "node" | "nodejs" | "bun" | "deno" | "python" | "python3"
    ) {
        if let Some(script) = tokens.find(|t| !t.starts_with('-')) {
            let script_base = token_basename(script);
            if let Some(agent) = ManifestAgent::parse(script_base) {
                return Some(agent);
            }
            let lower = script.to_ascii_lowercase();
            for agent in ManifestAgent::all() {
                if lower.contains(agent.label()) {
                    return Some(*agent);
                }
            }
        }
    }
    // `npx @anthropic-ai/claude-code` etc.
    let lower = command.to_ascii_lowercase();
    for agent in ManifestAgent::all() {
        if lower.split_whitespace().any(|t| {
            let b = token_basename(t);
            ManifestAgent::parse(b) == Some(*agent) || t.contains(agent.label())
        }) {
            return Some(*agent);
        }
    }
    None
}

fn token_basename(token: &str) -> &str {
    token
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(token)
        .split('.')
        .next()
        .unwrap_or(token)
}

/// True when this agent should use screen manifests as primary status authority
/// (hooks still used for session resume / optional notify feed).
pub fn screen_is_status_authority(command: &str) -> bool {
    agent_from_command(command).is_some()
}

/// Run detection for a known agent against live screen + OSC strings.
pub fn detect_agent(
    agent: ManifestAgent,
    screen: &str,
    osc_title: &str,
    osc_progress: &str,
) -> Detection {
    let result = detect_with_osc(
        agent,
        DetectionInput {
            screen,
            osc_title,
            osc_progress,
        },
    );
    Detection {
        agent: Some(agent.label()),
        state: result.state,
        skip_state_update: result.skip_state_update,
        visible_idle: result.visible_idle,
        visible_blocker: result.visible_blocker,
        visible_working: result.visible_working,
        matched_rule: result.matched_rule,
        fallback_reason: result.fallback_reason,
    }
}

/// Detect from pane command + screen. Returns `None` when the command is not a
/// known screen-manifest agent (caller should use hooks/keywords).
pub fn detect_for_command(
    command: &str,
    screen: &str,
    osc_title: &str,
    osc_progress: &str,
) -> Option<Detection> {
    let agent = agent_from_command(command)?;
    Some(detect_agent(agent, screen, osc_title, osc_progress))
}

/// Map herdr-style detected state onto vmux [`crate::model::AgentStatus`].
pub fn to_agent_status(state: DetectedState) -> crate::model::AgentStatus {
    match state {
        DetectedState::Working => crate::model::AgentStatus::Busy,
        DetectedState::Blocked => crate::model::AgentStatus::Attention,
        DetectedState::Idle => crate::model::AgentStatus::Idle,
        DetectedState::Unknown => crate::model::AgentStatus::Unknown,
    }
}

/// Merge a screen-detection result into the current pane status.
///
/// Screen is authoritative for busy/idle/attention. `Done` (from Stop) is kept
/// through plain idle until the user acknowledges; screen `working`/`blocked`
/// always wins. `Error` is sticky until a non-idle screen state appears.
pub fn merge_screen_status(
    current: crate::model::AgentStatus,
    detection: &Detection,
) -> Option<(crate::model::AgentStatus, bool)> {
    use crate::model::AgentStatus;
    if detection.skip_state_update {
        return None;
    }
    let screen = to_agent_status(detection.state);
    let next = match (&current, &screen) {
        (AgentStatus::Error, AgentStatus::Idle | AgentStatus::Unknown) => AgentStatus::Error,
        (AgentStatus::Done, AgentStatus::Idle | AgentStatus::Unknown) => AgentStatus::Done,
        (AgentStatus::Done, AgentStatus::Busy) => AgentStatus::Busy,
        (AgentStatus::Done, AgentStatus::Attention) => AgentStatus::Attention,
        (_, status) => status.clone(),
    };
    // Screen results are unpinned so a later frame can correct them freely.
    // Done stays pinned so quiet redraws cannot clear ✅.
    let pinned = matches!(next, AgentStatus::Done | AgentStatus::Error);
    if next == current {
        return None;
    }
    Some((next, pinned))
}

/// Human message for the notification feed/banner when screen detection
/// elevates a pane to Attention (blocked / needs input).
pub fn screen_attention_message(detection: &Detection) -> String {
    let agent = detection.agent.unwrap_or("agent");
    match detection.matched_rule.as_deref() {
        Some(rule)
            if rule.contains("permission")
                || rule.contains("bash_permission")
                || rule.contains("allow") =>
        {
            format!("{agent} needs permission")
        }
        Some(rule) if rule.contains("blocked") || rule.contains("prompt") => {
            format!("{agent} needs input")
        }
        Some(rule) if rule.contains("workflow") => {
            format!("{agent} needs a decision")
        }
        _ => format!("{agent} needs input"),
    }
}

/// Keep the pane banner in sync with screen-derived status: set a short
/// message when entering Attention, clear it when leaving.
pub fn apply_screen_notification_banner(
    pane: &mut crate::model::Pane,
    previous: &crate::model::AgentStatus,
    next: &crate::model::AgentStatus,
    detection: &Detection,
) {
    use crate::model::AgentStatus;
    match next {
        AgentStatus::Attention => {
            pane.notification_color = Some("blue".to_string());
            pane.notification_message = Some(screen_attention_message(detection));
        }
        // Leaving Attention (or clearing a leftover banner on any non-attention
        // status): drop so the sidebar / bell cannot re-fire on stale messages.
        _ if matches!(previous, AgentStatus::Attention) || pane.notification_message.is_some() => {
            pane.notification_message = None;
            pane.notification_color = None;
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_from_command_detects_common_agents() {
        assert_eq!(agent_from_command("claude"), Some(ManifestAgent::Claude));
        assert_eq!(
            agent_from_command("/usr/bin/codex resume abc"),
            Some(ManifestAgent::Codex)
        );
        assert_eq!(
            agent_from_command("node /home/x/.npm/bin/claude"),
            Some(ManifestAgent::Claude)
        );
        assert_eq!(agent_from_command("bash"), None);
        assert_eq!(agent_from_command("git status"), None);
    }

    #[test]
    fn claude_prompt_box_is_idle() {
        let screen = "\
─────────────────────
 ❯  
─────────────────────
";
        let d = detect_agent(ManifestAgent::Claude, screen, "✳ claude", "");
        assert_eq!(d.state, DetectedState::Idle, "{d:?}");
        assert!(d.visible_idle || d.matched_rule.is_some() || d.fallback_reason.is_some());
    }

    #[test]
    fn claude_permission_is_blocked() {
        let screen = "\
Do you want to proceed?
 Bash command
 ❯ 1. Yes
 2. No
 esc to cancel
";
        let d = detect_agent(ManifestAgent::Claude, screen, "", "");
        assert_eq!(d.state, DetectedState::Blocked, "{d:?}");
        assert!(d.visible_blocker || d.matched_rule.is_some());
    }

    #[test]
    fn claude_osc_title_braille_is_working() {
        // Braille pattern block U+2800..=U+28FF then space — herdr osc_title_working.
        let title = "\u{28FF} thinking";
        let d = detect_agent(ManifestAgent::Claude, "anything on screen", title, "");
        assert_eq!(d.state, DetectedState::Working, "{d:?}");
        assert!(d.visible_working);
    }

    #[test]
    fn merge_screen_keeps_done_through_idle() {
        use crate::model::AgentStatus;
        let d = Detection {
            agent: Some("claude"),
            state: DetectedState::Idle,
            skip_state_update: false,
            visible_idle: true,
            visible_blocker: false,
            visible_working: false,
            matched_rule: Some("live_prompt_box".into()),
            fallback_reason: None,
        };
        // Done + idle → stay Done (None because status unchanged after merge logic
        // actually returns Done which equals current... wait current is Done, next is Done, None)
        assert_eq!(merge_screen_status(AgentStatus::Done, &d), None);
        let working = Detection {
            state: DetectedState::Working,
            visible_working: true,
            matched_rule: Some("osc".into()),
            ..d.clone()
        };
        assert_eq!(
            merge_screen_status(AgentStatus::Done, &working),
            Some((AgentStatus::Busy, false))
        );
    }

    #[test]
    fn screen_attention_banner_set_and_cleared() {
        use crate::cli::SplitDirection;
        use crate::model::{AgentStatus, Pane};
        let mut pane = Pane::new("p".into(), "claude".into(), SplitDirection::Right);
        let blocked = Detection {
            agent: Some("claude"),
            state: DetectedState::Blocked,
            skip_state_update: false,
            visible_idle: false,
            visible_blocker: true,
            visible_working: false,
            matched_rule: Some("bash_permission_prompt".into()),
            fallback_reason: None,
        };
        apply_screen_notification_banner(
            &mut pane,
            &AgentStatus::Busy,
            &AgentStatus::Attention,
            &blocked,
        );
        assert_eq!(
            pane.notification_message.as_deref(),
            Some("claude needs permission")
        );
        apply_screen_notification_banner(
            &mut pane,
            &AgentStatus::Attention,
            &AgentStatus::Idle,
            &Detection {
                state: DetectedState::Idle,
                matched_rule: Some("live_prompt_box".into()),
                visible_idle: true,
                visible_blocker: false,
                ..blocked
            },
        );
        assert!(pane.notification_message.is_none());
        assert!(pane.notification_color.is_none());
    }
}
