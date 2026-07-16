//! Settings panel: rows, values, and draw.

use super::theme::{UiTheme, UiWorkspaceSecondLine};
use super::{panel_block, selected_row_style};
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingsEntryId {
    Theme,
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
    SectionRelay,
    MobileRelay,
    MobileRelayBind,
    MobileRelayPort,
    MobileRelayLocalhost,
    MobileRelayCgnat,
    MobileRelayPaste,
    MobileRelayViewResize,
    SectionPorts,
    PortsEnabled,
    PortsNotify,
    PortsAutoForward,
    PortsPollSecs,
    Section,
    HookShell,
    HookClaude,
    HookCodex,
    HookGrok,
    HookInstallAll,
}

pub(crate) struct SettingsEntry {
    pub(crate) id: SettingsEntryId,
    pub(crate) name: &'static str,
}

pub(crate) fn settings_entries() -> Vec<SettingsEntry> {
    vec![
        SettingsEntry {
            id: SettingsEntryId::Theme,
            name: "theme",
        },
        SettingsEntry {
            id: SettingsEntryId::WorkspaceLine,
            name: "workspace line",
        },
        SettingsEntry {
            id: SettingsEntryId::Sidebar,
            name: "sidebar",
        },
        SettingsEntry {
            id: SettingsEntryId::SidebarResponsive,
            name: "responsive layout",
        },
        SettingsEntry {
            id: SettingsEntryId::SidebarFit,
            name: "sidebar fit text",
        },
        SettingsEntry {
            id: SettingsEntryId::SidebarWidth,
            name: "sidebar width",
        },
        SettingsEntry {
            id: SettingsEntryId::PrefixKey,
            name: "prefix key",
        },
        SettingsEntry {
            id: SettingsEntryId::ScrollStep,
            name: "scroll step",
        },
        SettingsEntry {
            id: SettingsEntryId::CursorBlink,
            name: "cursor blink",
        },
        SettingsEntry {
            id: SettingsEntryId::CursorBlinkMs,
            name: "blink period",
        },
        SettingsEntry {
            id: SettingsEntryId::StatusMarkers,
            name: "status markers",
        },
        SettingsEntry {
            id: SettingsEntryId::DefaultShell,
            name: "default shell",
        },
        SettingsEntry {
            id: SettingsEntryId::DefaultCwd,
            name: "default cwd",
        },
        SettingsEntry {
            id: SettingsEntryId::Mouse,
            name: "mouse",
        },
        SettingsEntry {
            id: SettingsEntryId::TabCloseButton,
            name: "tab close ×",
        },
        SettingsEntry {
            id: SettingsEntryId::BellOnAttention,
            name: "bell on attention",
        },
        SettingsEntry {
            id: SettingsEntryId::SectionRelay,
            name: "── mobile relay ──",
        },
        SettingsEntry {
            id: SettingsEntryId::MobileRelay,
            name: "mobile relay",
        },
        SettingsEntry {
            id: SettingsEntryId::MobileRelayBind,
            name: "relay bind",
        },
        SettingsEntry {
            id: SettingsEntryId::MobileRelayPort,
            name: "relay port",
        },
        SettingsEntry {
            id: SettingsEntryId::MobileRelayLocalhost,
            name: "relay localhost",
        },
        SettingsEntry {
            id: SettingsEntryId::MobileRelayCgnat,
            name: "relay CGNAT",
        },
        SettingsEntry {
            id: SettingsEntryId::MobileRelayPaste,
            name: "paste page",
        },
        SettingsEntry {
            id: SettingsEntryId::MobileRelayViewResize,
            name: "phone-fit resize",
        },
        SettingsEntry {
            id: SettingsEntryId::SectionPorts,
            name: "── ports ──",
        },
        SettingsEntry {
            id: SettingsEntryId::PortsEnabled,
            name: "port detection",
        },
        SettingsEntry {
            id: SettingsEntryId::PortsNotify,
            name: "port notify",
        },
        SettingsEntry {
            id: SettingsEntryId::PortsAutoForward,
            name: "auto-forward",
        },
        SettingsEntry {
            id: SettingsEntryId::PortsPollSecs,
            name: "port scan interval",
        },
        SettingsEntry {
            id: SettingsEntryId::Section,
            name: "── agent hooks ──",
        },
        SettingsEntry {
            id: SettingsEntryId::HookShell,
            name: "shell hooks",
        },
        SettingsEntry {
            id: SettingsEntryId::HookClaude,
            name: "claude code",
        },
        SettingsEntry {
            id: SettingsEntryId::HookCodex,
            name: "codex",
        },
        SettingsEntry {
            id: SettingsEntryId::HookGrok,
            name: "grok skill",
        },
        SettingsEntry {
            id: SettingsEntryId::HookInstallAll,
            name: "install all hooks",
        },
    ]
}

pub(crate) struct SettingsView<'a> {
    pub(crate) theme: UiTheme,
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
    pub(crate) selected: usize,
}

pub(crate) fn settings_panel_lines(view: SettingsView<'_>) -> Vec<Line<'static>> {
    let hook_status = crate::agent_hooks::status_report();
    let theme = view.theme;
    settings_entries()
        .into_iter()
        .enumerate()
        .map(|(index, entry)| {
            let active = index == view.selected;
            let value = match entry.id {
                SettingsEntryId::Theme => theme.label().to_string(),
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
                SettingsEntryId::SectionRelay => {
                    "one relay for all sessions · change applies when you leave the row".to_string()
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
                SettingsEntryId::SectionPorts => {
                    "detect listeners in panes · vmux ports".to_string()
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
                SettingsEntryId::Section => "sidebar emoji for agents".to_string(),
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
            let style = if matches!(
                entry.id,
                SettingsEntryId::Section | SettingsEntryId::SectionRelay
            ) {
                Style::default().fg(theme.palette().muted)
            } else if active {
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

pub(crate) fn draw_settings(frame: &mut ratatui::Frame, area: Rect, view: SettingsView<'_>) {
    let palette = view.theme.palette();
    let lines = settings_panel_lines(view);
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel_block(" settings ", palette))
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
