//! Settings panel: rows, values, and draw.

use super::theme::{UiLayout, UiTheme, UiWorkspaceSecondLine};
use super::{panel_block, selected_row_style};
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingsEntryId {
    /// Screen structure (classic / compact / minimal / flat / zen).
    Layout,
    /// Color palette only.
    Colors,
    WorkspaceLine,
    Sidebar,
    SidebarResponsive,
    SidebarFit,
    SidebarWidth,
    PrefixKey,
    ScrollStep,
    CursorBlink,
    CursorBlinkMs,
    StatusMarkers,
    DefaultShell,
    DefaultCwd,
    Mouse,
    TabCloseButton,
    BellOnAttention,
    ResumeAgents,
    MobileRelay,
    MobileRelayBind,
    MobileRelayPort,
    MobileRelayLocalhost,
    MobileRelayCgnat,
    MobileRelayPaste,
    MobileRelayViewResize,
    PortsEnabled,
    PortsNotify,
    PortsAutoForward,
    PortsPollSecs,
    HookShell,
    HookClaude,
    HookCodex,
    HookGrok,
    HookInstallAll,
}

/// One tab of the settings page. Replaces the old single flat list with
/// `── section ──` header rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingsTab {
    Ui,
    Relay,
    Ports,
    Hooks,
}

impl SettingsTab {
    pub(crate) const ALL: [SettingsTab; 4] = [
        SettingsTab::Ui,
        SettingsTab::Relay,
        SettingsTab::Ports,
        SettingsTab::Hooks,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            SettingsTab::Ui => "ui",
            SettingsTab::Relay => "relay",
            SettingsTab::Ports => "ports",
            SettingsTab::Hooks => "hooks",
        }
    }

    /// A one-line description shown under the tab strip.
    pub(crate) fn blurb(self) -> &'static str {
        match self {
            SettingsTab::Ui => "appearance and input",
            SettingsTab::Relay => {
                "one phone relay for all sessions · edits apply when you leave a row"
            }
            SettingsTab::Ports => "detect listeners in panes · vmux ports",
            SettingsTab::Hooks => "agent status in the sidebar",
        }
    }

    pub(crate) fn next(self) -> SettingsTab {
        let index = Self::ALL.iter().position(|tab| *tab == self).unwrap_or(0);
        Self::ALL[(index + 1) % Self::ALL.len()]
    }

    pub(crate) fn prev(self) -> SettingsTab {
        let index = Self::ALL.iter().position(|tab| *tab == self).unwrap_or(0);
        Self::ALL[(index + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

pub(crate) struct SettingsEntry {
    pub(crate) id: SettingsEntryId,
    pub(crate) name: &'static str,
}

pub(crate) fn settings_entries_for_tab(tab: SettingsTab) -> Vec<SettingsEntry> {
    let rows: &[(SettingsEntryId, &'static str)] = match tab {
        SettingsTab::Ui => &[
            (SettingsEntryId::Layout, "layout"),
            (SettingsEntryId::Colors, "colors"),
            (SettingsEntryId::WorkspaceLine, "workspace line"),
            (SettingsEntryId::Sidebar, "sidebar"),
            (SettingsEntryId::SidebarResponsive, "responsive layout"),
            (SettingsEntryId::SidebarFit, "sidebar fit text"),
            (SettingsEntryId::SidebarWidth, "sidebar width"),
            (SettingsEntryId::PrefixKey, "prefix key"),
            (SettingsEntryId::ScrollStep, "scroll step"),
            (SettingsEntryId::CursorBlink, "cursor blink"),
            (SettingsEntryId::CursorBlinkMs, "blink period"),
            (SettingsEntryId::StatusMarkers, "status markers"),
            (SettingsEntryId::DefaultShell, "default shell"),
            (SettingsEntryId::DefaultCwd, "default cwd"),
            (SettingsEntryId::Mouse, "mouse"),
            (SettingsEntryId::TabCloseButton, "tab close ×"),
            (SettingsEntryId::BellOnAttention, "bell on attention"),
            (SettingsEntryId::ResumeAgents, "resume agents"),
        ],
        SettingsTab::Relay => &[
            (SettingsEntryId::MobileRelay, "mobile relay"),
            (SettingsEntryId::MobileRelayBind, "relay bind"),
            (SettingsEntryId::MobileRelayPort, "relay port"),
            (SettingsEntryId::MobileRelayLocalhost, "relay localhost"),
            (SettingsEntryId::MobileRelayCgnat, "relay CGNAT"),
            (SettingsEntryId::MobileRelayPaste, "paste page"),
            (SettingsEntryId::MobileRelayViewResize, "phone-fit resize"),
        ],
        SettingsTab::Ports => &[
            (SettingsEntryId::PortsEnabled, "port detection"),
            (SettingsEntryId::PortsNotify, "port notify"),
            (SettingsEntryId::PortsAutoForward, "auto-forward"),
            (SettingsEntryId::PortsPollSecs, "port scan interval"),
        ],
        SettingsTab::Hooks => &[
            (SettingsEntryId::HookShell, "shell hooks"),
            (SettingsEntryId::HookClaude, "claude code"),
            (SettingsEntryId::HookCodex, "codex"),
            (SettingsEntryId::HookGrok, "grok skill"),
            (SettingsEntryId::HookInstallAll, "install all hooks"),
        ],
    };
    rows.iter()
        .map(|(id, name)| SettingsEntry { id: *id, name })
        .collect()
}

pub(crate) struct SettingsView<'a> {
    pub(crate) theme: UiTheme,
    pub(crate) layout: UiLayout,
    pub(crate) workspace_second_line: UiWorkspaceSecondLine,
    pub(crate) sidebar_collapsed: bool,
    pub(crate) sidebar_responsive: bool,
    pub(crate) sidebar_fit: bool,
    pub(crate) sidebar_width: u16,
    pub(crate) prefix_label: &'a str,
    pub(crate) scroll_step: usize,
    pub(crate) cursor_blink: bool,
    pub(crate) cursor_blink_ms: u64,
    pub(crate) status_markers: &'a str,
    pub(crate) default_shell: &'a str,
    pub(crate) default_cwd: &'a str,
    pub(crate) mouse: bool,
    pub(crate) tab_close_button: bool,
    pub(crate) bell_on_attention: bool,
    pub(crate) resume_agents: bool,
    pub(crate) mobile_relay_enabled: bool,
    pub(crate) mobile_relay_bind: &'a str,
    pub(crate) mobile_relay_port: u16,
    pub(crate) mobile_relay_allow_localhost: bool,
    pub(crate) mobile_relay_allow_cgnat: bool,
    pub(crate) mobile_relay_allow_paste: bool,
    pub(crate) mobile_relay_allow_view_resize: bool,
    /// True while relay edits are pending (not yet saved / applied).
    pub(crate) settings_relay_dirty: bool,
    pub(crate) ports_enabled: bool,
    pub(crate) ports_notify: &'a str,
    pub(crate) ports_auto_forward: bool,
    pub(crate) ports_poll_secs: u64,
    pub(crate) active_tab: SettingsTab,
    pub(crate) selected: usize,
}

pub(crate) fn settings_panel_lines(view: SettingsView<'_>) -> Vec<Line<'static>> {
    let hook_status = crate::agent_hooks::status_report();
    let theme = view.theme;
    settings_entries_for_tab(view.active_tab)
        .into_iter()
        .enumerate()
        .map(|(index, entry)| {
            let active = index == view.selected;
            let value = match entry.id {
                SettingsEntryId::Layout => {
                    format!("{} · {}", view.layout.label(), view.layout.blurb())
                }
                SettingsEntryId::Colors => theme.label().to_string(),
                SettingsEntryId::WorkspaceLine => view.workspace_second_line.label().to_string(),
                SettingsEntryId::Sidebar => {
                    if view.sidebar_collapsed {
                        "collapsed".to_string()
                    } else {
                        "expanded".to_string()
                    }
                }
                SettingsEntryId::SidebarResponsive => {
                    if view.sidebar_responsive {
                        "on · hide sidebar when narrow (<90)".to_string()
                    } else {
                        "off · always show sidebar".to_string()
                    }
                }
                SettingsEntryId::SidebarFit => {
                    if view.sidebar_fit {
                        "on · width follows workspace names".to_string()
                    } else {
                        "off · fixed width".to_string()
                    }
                }
                SettingsEntryId::SidebarWidth => {
                    if view.sidebar_fit {
                        format!("max {} cols (drag edge)", view.sidebar_width)
                    } else {
                        format!("{} cols (drag edge)", view.sidebar_width)
                    }
                }
                SettingsEntryId::PrefixKey => view.prefix_label.to_string(),
                SettingsEntryId::ScrollStep => format!("{} lines", view.scroll_step),
                SettingsEntryId::CursorBlink => {
                    if view.cursor_blink {
                        "on".to_string()
                    } else {
                        "off (solid)".to_string()
                    }
                }
                SettingsEntryId::CursorBlinkMs => {
                    format!("{} ms half-period", view.cursor_blink_ms)
                }
                SettingsEntryId::StatusMarkers => view.status_markers.to_string(),
                SettingsEntryId::DefaultShell => {
                    if view.default_shell.is_empty() {
                        "system ($SHELL)".to_string()
                    } else {
                        view.default_shell.to_string()
                    }
                }
                SettingsEntryId::DefaultCwd => match view.default_cwd {
                    "home" => "home directory".to_string(),
                    _ => "launch directory".to_string(),
                },
                SettingsEntryId::Mouse => {
                    if view.mouse {
                        "on".to_string()
                    } else {
                        "off".to_string()
                    }
                }
                SettingsEntryId::TabCloseButton => {
                    if view.tab_close_button {
                        "show when multi-tab".to_string()
                    } else {
                        "hidden".to_string()
                    }
                }
                SettingsEntryId::BellOnAttention => {
                    if view.bell_on_attention {
                        "on".to_string()
                    } else {
                        "off".to_string()
                    }
                }
                SettingsEntryId::ResumeAgents => {
                    if view.resume_agents {
                        "on · claude --resume on restart (next daemon start)".to_string()
                    } else {
                        "off · restart opens a fresh conversation".to_string()
                    }
                }
                SettingsEntryId::MobileRelay => {
                    let settings = crate::config::RelaySettings {
                        enabled: view.mobile_relay_enabled,
                        bind: view.mobile_relay_bind.to_string(),
                        port: view.mobile_relay_port,
                        allow_localhost: view.mobile_relay_allow_localhost,
                        allow_tailnet_cgnat: view.mobile_relay_allow_cgnat,
                        allow_paste: view.mobile_relay_allow_paste,
                        // Display-only: runtime_status_line reads enabled/bind/port.
                        allow_view_resize: false,
                    };
                    let mut line = crate::relay::runtime_status_line(&settings);
                    if view.settings_relay_dirty {
                        line.push_str(" · pending save");
                    }
                    line
                }
                SettingsEntryId::MobileRelayBind => {
                    let base = match view.mobile_relay_bind {
                        "tailscale" => "tailscale only",
                        "local" => "localhost only",
                        _ => "auto (Tailscale → localhost)",
                    };
                    if view.settings_relay_dirty {
                        format!("{base} · pending")
                    } else {
                        base.to_string()
                    }
                }
                SettingsEntryId::MobileRelayPort => {
                    if view.settings_relay_dirty {
                        format!(
                            "{} · pending · leave row or Esc to apply (shared by all sessions)",
                            view.mobile_relay_port
                        )
                    } else {
                        format!(
                            "{} · h/l to cycle · applies on leave · one port for all sessions",
                            view.mobile_relay_port
                        )
                    }
                }
                SettingsEntryId::MobileRelayLocalhost => {
                    if view.mobile_relay_allow_localhost {
                        "allow register from 127.0.0.1".to_string()
                    } else {
                        "deny localhost register".to_string()
                    }
                }
                SettingsEntryId::MobileRelayCgnat => {
                    if view.mobile_relay_allow_cgnat {
                        "allow Tailscale CGNAT peers without whois".to_string()
                    } else {
                        "require whois / bootstrap for CGNAT".to_string()
                    }
                }
                SettingsEntryId::MobileRelayPaste => {
                    if view.mobile_relay_allow_paste {
                        "on · browser screenshot paste at /paste".to_string()
                    } else {
                        "off · /paste returns 404".to_string()
                    }
                }
                SettingsEntryId::MobileRelayViewResize => {
                    if view.mobile_relay_allow_view_resize {
                        "on · phone can shrink PTY to fit".to_string()
                    } else {
                        "off · phone wraps only".to_string()
                    }
                }
                SettingsEntryId::PortsEnabled => {
                    if view.ports_enabled {
                        "on · scan pane process trees (next daemon start)".to_string()
                    } else {
                        "off (next daemon start)".to_string()
                    }
                }
                SettingsEntryId::PortsNotify => match view.ports_notify {
                    "banner" => "banner".to_string(),
                    "off" => "off · silent".to_string(),
                    _ => "toast · notification feed".to_string(),
                },
                SettingsEntryId::PortsAutoForward => {
                    if view.ports_auto_forward {
                        "on · Tailscale-forward new ports (next daemon start)".to_string()
                    } else {
                        "off (next daemon start)".to_string()
                    }
                }
                SettingsEntryId::PortsPollSecs => {
                    format!(
                        "{}s between scans (next daemon start)",
                        view.ports_poll_secs
                    )
                }
                SettingsEntryId::HookShell => {
                    hook_status_value(&hook_status, crate::agent_hooks::IntegrationKind::Shell)
                }
                SettingsEntryId::HookClaude => {
                    hook_status_value(&hook_status, crate::agent_hooks::IntegrationKind::Claude)
                }
                SettingsEntryId::HookCodex => {
                    hook_status_value(&hook_status, crate::agent_hooks::IntegrationKind::Codex)
                }
                SettingsEntryId::HookGrok => {
                    hook_status_value(&hook_status, crate::agent_hooks::IntegrationKind::Grok)
                }
                SettingsEntryId::HookInstallAll => {
                    let missing = hook_status
                        .iter()
                        .filter(|s| matches!(s.state, crate::agent_hooks::InstallState::Missing))
                        .count();
                    if missing == 0 {
                        "all ready (Enter reinstall)".to_string()
                    } else {
                        format!("{missing} missing — Enter to install")
                    }
                }
            };
            let marker = if active { "›" } else { " " };
            let style = if active {
                selected_row_style(theme.palette())
            } else {
                Style::default().fg(theme.palette().text)
            };
            Line::from(vec![Span::styled(
                format!("{marker} {name:<18} {value}", name = entry.name),
                style,
            )])
        })
        .collect()
}

/// Which tab sits at `column` (0-based, relative to the panel content area)
/// in the strip drawn by `settings_tab_strip`. Must mirror its span layout:
/// a leading space, then ` {label} ` per tab joined by ` │ `.
pub(crate) fn settings_tab_at(column: usize) -> Option<SettingsTab> {
    let mut cursor = 1usize;
    for (index, tab) in SettingsTab::ALL.into_iter().enumerate() {
        if index > 0 {
            cursor += 3;
        }
        let width = tab.label().len() + 2;
        if (cursor..cursor + width).contains(&column) {
            return Some(tab);
        }
        cursor += width;
    }
    None
}

/// Rows rendered above the settings entries by `draw_settings` (tab strip,
/// blurb, spacer). Keep in sync with `settings_tab_strip`'s line count.
pub(crate) const SETTINGS_HEADER_ROWS: usize = 3;

/// The `ui │ relay │ ports │ hooks` strip above the rows, plus the active
/// tab's blurb.
pub(crate) fn settings_tab_strip(active: SettingsTab, theme: UiTheme) -> Vec<Line<'static>> {
    let palette = theme.palette();
    let mut spans: Vec<Span<'static>> = vec![Span::raw(" ")];
    for (index, tab) in SettingsTab::ALL.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" │ ", Style::default().fg(palette.muted)));
        }
        let style = if tab == active {
            selected_row_style(palette)
        } else {
            Style::default().fg(palette.muted)
        };
        spans.push(Span::styled(format!(" {} ", tab.label()), style));
    }
    vec![
        Line::from(spans),
        Line::from(Span::styled(
            format!(" {}", active.blurb()),
            Style::default().fg(palette.muted),
        )),
        Line::from(Span::raw("")),
    ]
}

pub(crate) fn draw_settings(frame: &mut ratatui::Frame, area: Rect, view: SettingsView<'_>) {
    let palette = view.theme.palette();
    let mut lines = settings_tab_strip(view.active_tab, view.theme);
    lines.extend(settings_panel_lines(view));
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel_block(" settings · Tab switches page ", palette))
            .style(Style::default().fg(palette.text).bg(palette.surface))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn hook_status_value(
    statuses: &[crate::agent_hooks::IntegrationStatus],
    kind: crate::agent_hooks::IntegrationKind,
) -> String {
    let Some(status) = statuses.iter().find(|s| s.kind == kind) else {
        return "unknown".to_string();
    };
    let icon = match status.state {
        crate::agent_hooks::InstallState::Installed => "✅",
        crate::agent_hooks::InstallState::Missing => "○",
        crate::agent_hooks::InstallState::NotDetected => "·",
    };
    let action = match status.state {
        crate::agent_hooks::InstallState::Installed => "ok",
        crate::agent_hooks::InstallState::Missing => "Enter install",
        crate::agent_hooks::InstallState::NotDetected => "Enter setup",
    };
    format!("{icon} {}  {action}", status.state.label())
}
