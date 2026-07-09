use anyhow::{Context, Result};
use crossterm::cursor::SetCursorStyle;
use crossterm::event::{
    self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, size as terminal_size, EnterAlternateScreen,
    LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};
use ratatui::Terminal;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Stdout, Write};
use std::time::{Duration, Instant};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::cli::SplitDirection;
use crate::config::LmuxConfig;
use crate::input::{key_to_input, parse_key_binding};
use crate::model::{LayoutNode, Session, SplitAxis};
use crate::paths;
use crate::protocol::{self, PaneSize, Request};

const CONTROL_BAR_HEIGHT: u16 = 2;
/// Workspace tab strip above the pane grid (Workspace → Tab → Pane).
const TAB_BAR_HEIGHT: u16 = 1;

pub fn attach(session: &str) -> Result<()> {
    // Load config before entering raw mode / the alternate screen so a config
    // error returns cleanly without leaving the user's shell in a broken state.
    let config = crate::config::load()?;
    // Opt-in mobile relay: only when Settings → mobile relay is enabled.
    // Failures are non-fatal so attach still works offline / without Tailscale.
    if config.relay.enabled {
        match crate::relay::ensure_from_config(session, &config) {
            Ok(Some(msg)) => eprintln!("vmux: {msg}"),
            Ok(None) => {}
            Err(err) => eprintln!("vmux: mobile relay not started ({err:#})"),
        }
    }
    // Enable raw mode + the alternate screen behind an RAII guard so the TTY is
    // always restored, even if `Ui::new`, terminal init, an early RPC, or a
    // panic inside `run` bails out between here and normal teardown.
    let _guard = TerminalGuard::new(config.ui.mouse)?;
    let backend = CrosstermBackend::new(io::stdout());
    let terminal = Terminal::new(backend)?;
    let mut app = Ui::new(session.to_string(), terminal, config);
    app.run()
}

/// RAII guard that owns the terminal's raw mode + alternate screen state. Its
/// `Drop` disables raw mode and leaves the alternate screen (and disables mouse
/// capture), so every early-return and unwind path restores the shell. Drop is
/// the only cleanup, so there is no double-teardown on the happy path.
struct TerminalGuard;

impl TerminalGuard {
    fn new(mouse: bool) -> io::Result<Self> {
        enable_raw_mode()?;
        // Construct the guard immediately after enabling raw mode so that if
        // entering the alternate screen fails, Drop still restores raw mode.
        let guard = Self;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            // Steady block; vmux software-blinks only the active pane caret.
            SetCursorStyle::SteadyBlock,
        )?;
        execute!(io::stdout(), event::EnableBracketedPaste)?;
        if mouse {
            execute!(io::stdout(), event::EnableMouseCapture)?;
        }
        Ok(guard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Best-effort restore; ignore errors since we may be unwinding. Leaving
        // the alternate screen / disabling mouse capture is harmless even if
        // entering it never succeeded. Always disable mouse/paste — Settings
        // may have toggled capture after construction (improve.md #20).
        let _ = execute!(io::stdout(), SetCursorStyle::DefaultUserShape);
        let _ = execute!(io::stdout(), event::DisableMouseCapture);
        let _ = execute!(io::stdout(), event::DisableBracketedPaste);
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

/// Maps a daemon response to a user-facing error message. Returns `None` when
/// the request succeeded (`ok:true`); otherwise the daemon's `error` string, or
/// a generic fallback when the daemon reported a failure with no message.
fn response_error(response: &protocol::Response) -> Option<String> {
    if response.ok {
        None
    } else {
        Some(
            response
                .error
                .clone()
                .unwrap_or_else(|| "daemon reported an error".to_string()),
        )
    }
}

struct Ui {
    session: String,
    terminal: Terminal<CrosstermBackend<Stdout>>,
    snapshot: Option<Session>,
    prefix: bool,
    prefix_key: UiKeyBinding,
    /// Human-readable label for the configured prefix key (e.g. "Ctrl-b"), shown
    /// in the command palette's shortcut column. Sourced from the config value.
    prefix_label: String,
    mode: UiMode,
    sidebar_collapsed: bool,
    /// Expanded sidebar width in columns (when not collapsed).
    sidebar_width: u16,
    /// True while dragging the sidebar right edge to resize.
    sidebar_resize_drag: bool,
    sidebar_drag_workspace: Option<String>,
    pane_resize_drag: Option<PaneResizeDrag>,
    context_pane: Option<String>,
    notification_selected: usize,
    action_selected: usize,
    command_selected: usize,
    /// Current text typed into the command-palette filter box. Empty means the
    /// full palette is shown.
    command_filter: String,
    settings_selected: usize,
    context_selected: usize,
    hover_control: Option<ControlAction>,
    hover_pane_control: Option<PaneControlHit>,
    hover_workspace: Option<String>,
    /// True while the pointer is over the empty-workspace "[ + create pane ]"
    /// button, so it renders with a hover highlight like the control bar.
    hover_empty_create: bool,
    /// Action awaiting confirmation (modal alert overlay; pane grid stays visible).
    pending_confirm: Option<PendingConfirm>,
    /// Inline rename dialog for workspace / tab / pane titles (double-click).
    rename_dialog: Option<RenameDialog>,
    /// Previous left-click used to detect double-clicks for rename.
    last_click: Option<ClickStamp>,
    selection: Option<TextSelection>,
    theme: UiTheme,
    workspace_second_line: UiWorkspaceSecondLine,
    scroll_step: usize,
    cursor_blink: bool,
    cursor_blink_ms: u64,
    /// `emoji` | `ascii` | `off`
    status_markers: String,
    /// Empty = system `$SHELL`
    default_shell: String,
    /// `launch` | `home`
    default_cwd: String,
    mouse: bool,
    tab_close_button: bool,
    bell_on_attention: bool,
    /// Auto-hide sidebar on narrow terminals (burger + workspace picker).
    sidebar_responsive: bool,
    /// Selection index inside the ☰ workspace picker.
    workspace_picker_selected: usize,
    /// Settings: auto-start phone relay on attach.
    mobile_relay_enabled: bool,
    /// Settings: `auto` | `tailscale` | `local` (never all-interfaces).
    mobile_relay_bind: String,
    mobile_relay_port: u16,
    mobile_relay_allow_localhost: bool,
    mobile_relay_allow_cgnat: bool,
    /// Pane ids last seen in Attention (for one-shot bell).
    prev_attention_panes: BTreeSet<String>,
    actions: Vec<UiAction>,
    /// Latest non-fatal error to surface to the user (daemon `ok:false`
    /// responses routed through `rpc`, plus action-palette / selection failures).
    /// Interior-mutable so `rpc(&self)` can record daemon errors without
    /// threading `&mut self` through every RPC helper.
    action_error: std::cell::RefCell<Option<String>>,
    client_id: String,
    pane_size_claimed: bool,
    pane_size_control_requested: bool,
    last_pane_sizes: BTreeMap<String, PaneSize>,
    scroll_offsets: BTreeMap<String, usize>,
    /// First visible workspace index in the sidebar (vertical scroll offset).
    sidebar_scroll: usize,
    /// Active workspace id observed at the last auto-scroll, so the sidebar only
    /// auto-scrolls to reveal the active workspace when it actually changes
    /// (otherwise wheel scrolling would snap straight back).
    sidebar_active_seen: Option<String>,
    /// Raw JSON of the last snapshot received, used to detect whether a refresh
    /// actually changed anything (drives dirty-flag rendering).
    last_snapshot_data: Option<serde_json::Value>,
    /// Set by any state-changing RPC (via `rpc()`) so the run loop refreshes
    /// immediately instead of waiting for the periodic tick. This covers
    /// keystroke echo latency as well as pane/workspace creation and layout
    /// changes, which would otherwise appear only after the next 150ms tick.
    /// `Cell` so the `&self` `rpc()` helper can set it.
    pending_refresh: std::cell::Cell<bool>,
    /// Last keystroke/paste into a pane — keeps the caret solid (no blink) while typing.
    last_typing_at: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UiMode {
    Panes,
    Notifications,
    Actions,
    Commands,
    Settings,
    /// Mobile-style workspace list opened from the ☰ burger control.
    WorkspacePicker,
    ContextMenu,
}

/// Terminal width (columns) below which the sidebar auto-hides and the burger menu is used.
const COMPACT_TERM_WIDTH: u16 = 90;

/// A pending action awaiting confirmation. The pane grid stays visible; a modal
/// alert overlays it and (for pane kills) the target pane is highlighted.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingConfirm {
    /// Title shown in the alert box border, e.g. " ⚠ close pane ".
    title: String,
    /// Short question lines (no key-binding prose — buttons show shortcuts).
    body: String,
    /// The action performed when the user confirms.
    action: ConfirmAction,
}

impl PendingConfirm {
    /// Pane id to paint with a danger border while the alert is open.
    fn highlight_pane(&self) -> Option<&str> {
        match &self.action {
            ConfirmAction::KillPane(pane) => Some(pane.as_str()),
            ConfirmAction::CloseWorkspace | ConfirmAction::CloseWorkspaceTab { .. } => None,
        }
    }
}

/// What is being renamed via the modal dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RenameTarget {
    Workspace { id: String },
    Tab { id: String },
    Pane { id: String },
}

impl RenameTarget {
    fn kind_label(&self) -> &'static str {
        match self {
            Self::Workspace { .. } => "workspace",
            Self::Tab { .. } => "tab",
            Self::Pane { .. } => "pane",
        }
    }

    fn id(&self) -> &str {
        match self {
            Self::Workspace { id } | Self::Tab { id } | Self::Pane { id } => id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RenameDialog {
    target: RenameTarget,
    /// Editable name buffer.
    draft: String,
}

/// First click of a potential double-click rename.
#[derive(Debug, Clone)]
struct ClickStamp {
    at: Instant,
    column: u16,
    row: u16,
    target: RenameTarget,
}

const DOUBLE_CLICK_MS: u128 = 450;

#[derive(Debug, Clone, PartialEq, Eq)]
enum ConfirmAction {
    CloseWorkspace,
    KillPane(String),
    #[allow(dead_code)]
    CloseWorkspaceTab {
        tab: String,
    },
}

#[derive(Debug, Clone, Deserialize)]
struct UiAction {
    name: String,
    command: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    direction: Option<SplitDirection>,
}

#[derive(Debug, Deserialize)]
struct UiActionResponse {
    #[serde(default)]
    commands: Vec<UiAction>,
}

#[cfg(test)]
#[derive(Debug, Clone)]
struct AgentPanelEntry {
    workspace_id: String,
    workspace_name: String,
    pane_id: String,
    title: String,
    command: String,
    surface: crate::model::SurfaceKind,
    status: crate::model::PaneStatus,
    agent_status: crate::model::AgentStatus,
    progress: Option<u8>,
    metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandPaletteAction {
    SplitRight,
    SplitDown,
    NewWorkspace,
    KillPane,
    DuplicatePane,
    RestartPane,
    ClearPane,
    CopyPane,
    PastePane,
    StatusBusy,
    StatusAttention,
    StatusDone,
    StatusIdle,
    CloseWorkspace,
    NextWorkspace,
    PreviousWorkspace,
    ToggleNotifications,
    ToggleActions,
    ToggleZoom,
    NextTab,
    PreviousTab,
    NewTab,
    Settings,
    FocusLeft,
    FocusRight,
    FocusUp,
    FocusDown,
    ResizeLeft,
    ResizeRight,
    ResizeUp,
    ResizeDown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControlAction {
    /// ☰ open/close workspace picker (responsive / mobile-friendly).
    Workspaces,
    NewWorkspace,
    NewTab,
    SplitRight,
    SplitDown,
    DeleteWorkspace,
    Commands,
    Notifications,
    Settings,
    Detach,
    KillPane,
}

#[derive(Debug, Clone, Copy)]
struct ControlButton {
    /// Prefer double-width emoji (or emoji + VS16) so every button lines up.
    icon: &'static str,
    label: &'static str,
    action: ControlAction,
}

impl ControlButton {
    /// Rendered text: `" {icon} {label} "` with consistent spacing.
    fn text(self) -> String {
        format!(" {} {} ", self.icon, self.label)
    }

    /// Display columns occupied by [`Self::text`] (emoji-aware).
    fn width(self) -> u16 {
        UnicodeWidthStr::width(self.text().as_str()) as u16
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaneControlAction {
    SplitRight,
    SplitDown,
    MoveLeft,
    MoveRight,
    MoveUp,
    MoveDown,
    Close,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PaneControlHit {
    pane: String,
    action: PaneControlAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TextSelection {
    pane: String,
    start_col: u16,
    start_row: u16,
    end_col: u16,
    end_row: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MouseButtonCode {
    Left,
    LeftDrag,
    WheelUp,
    WheelDown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UiTheme {
    Midnight,
    Daylight,
    Contrast,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UiWorkspaceSecondLine {
    Path,
    Details,
    Branch,
    Id,
    Status,
    Cursor,
    None,
}

#[derive(Debug, Clone, Copy)]
struct ThemePalette {
    background: Color,
    surface: Color,
    surface_alt: Color,
    text: Color,
    muted: Color,
    border: Color,
    active: Color,
    hover: Color,
    danger: Color,
    success: Color,
    warning: Color,
    command: Color,
    /// Readable foreground for text drawn on an `active`-accent fill (selected
    /// sidebar row, active tab, active control button).
    on_accent: Color,
    /// Readable foreground for text drawn on a bright fill (hover, success,
    /// danger, warning) — black reads well on every theme's bright colors.
    on_bright: Color,
    /// Background used to highlight a text selection in a pane.
    selection: Color,
    /// Block-cursor fill (bright accent — never pure black on dark panes).
    cursor: Color,
    /// Glyph color drawn on top of [`Self::cursor`].
    on_cursor: Color,
}

#[derive(Debug, Clone, Copy)]
struct CommandPaletteEntry {
    name: &'static str,
    description: &'static str,
    action: CommandPaletteAction,
}

#[derive(Debug, Clone, Copy)]
enum ContextMenuAction {
    CopyPane,
    PastePane,
    SplitRight,
    SplitDown,
    ClearPane,
}

#[derive(Debug, Clone, Copy)]
struct ContextMenuEntry {
    name: &'static str,
    description: &'static str,
    action: ContextMenuAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PaneResizeDrag {
    axis: SplitAxis,
    column: u16,
    row: u16,
}

impl UiTheme {
    fn from_name(name: &str) -> Self {
        match name.trim().to_ascii_lowercase().as_str() {
            "daylight" => Self::Daylight,
            "contrast" => Self::Contrast,
            _ => Self::Midnight,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Midnight => "midnight",
            Self::Daylight => "daylight",
            Self::Contrast => "contrast",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Midnight => "Midnight",
            Self::Daylight => "Daylight",
            Self::Contrast => "Contrast",
        }
    }

    fn all() -> &'static [Self] {
        &[Self::Midnight, Self::Daylight, Self::Contrast]
    }

    fn relative(self, delta: isize) -> Self {
        let themes = Self::all();
        let current = themes.iter().position(|item| *item == self).unwrap_or(0);
        let next = (current as isize + delta).rem_euclid(themes.len() as isize) as usize;
        themes[next]
    }

    fn palette(self) -> ThemePalette {
        match self {
            Self::Midnight => ThemePalette {
                background: Color::Rgb(8, 12, 18),
                surface: Color::Rgb(18, 18, 22),
                surface_alt: Color::Rgb(26, 30, 38),
                text: Color::Gray,
                muted: Color::DarkGray,
                border: Color::DarkGray,
                active: Color::Cyan,
                hover: Color::LightYellow,
                danger: Color::LightRed,
                success: Color::LightGreen,
                warning: Color::LightYellow,
                command: Color::LightBlue,
                on_accent: Color::Black,
                on_bright: Color::Black,
                selection: Color::LightYellow,
                cursor: Color::Rgb(120, 220, 255),
                on_cursor: Color::Rgb(8, 12, 18),
            },
            Self::Daylight => ThemePalette {
                background: Color::Rgb(238, 241, 245),
                surface: Color::Rgb(224, 229, 236),
                surface_alt: Color::Rgb(210, 218, 229),
                text: Color::Black,
                muted: Color::Gray,
                border: Color::Gray,
                active: Color::Blue,
                hover: Color::Yellow,
                danger: Color::Red,
                success: Color::Green,
                warning: Color::Yellow,
                command: Color::Blue,
                on_accent: Color::White,
                on_bright: Color::Black,
                selection: Color::Yellow,
                cursor: Color::Rgb(30, 100, 220),
                on_cursor: Color::White,
            },
            Self::Contrast => ThemePalette {
                background: Color::Black,
                surface: Color::Black,
                surface_alt: Color::DarkGray,
                text: Color::White,
                muted: Color::Gray,
                border: Color::White,
                active: Color::LightCyan,
                hover: Color::White,
                danger: Color::LightRed,
                success: Color::LightGreen,
                warning: Color::LightYellow,
                command: Color::LightBlue,
                on_accent: Color::Black,
                on_bright: Color::Black,
                cursor: Color::LightCyan,
                on_cursor: Color::Black,
                selection: Color::LightYellow,
            },
        }
    }
}

impl UiWorkspaceSecondLine {
    fn from_name(name: &str) -> Self {
        match name.trim().to_ascii_lowercase().as_str() {
            "details" => Self::Details,
            "branch" => Self::Branch,
            "id" => Self::Id,
            "status" => Self::Status,
            "cursor" => Self::Cursor,
            "none" => Self::None,
            _ => Self::Path,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Path => "path",
            Self::Details => "details",
            Self::Branch => "branch",
            Self::Id => "id",
            Self::Status => "status",
            Self::Cursor => "cursor",
            Self::None => "none",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Path => "Path",
            Self::Details => "Details",
            Self::Branch => "Branch",
            Self::Id => "ID",
            Self::Status => "Status",
            Self::Cursor => "Cursor",
            Self::None => "None",
        }
    }

    fn all() -> &'static [Self] {
        &[
            Self::Path,
            Self::Details,
            Self::Branch,
            Self::Id,
            Self::Status,
            Self::Cursor,
            Self::None,
        ]
    }

    fn relative(self, delta: isize) -> Self {
        let items = Self::all();
        let current = items.iter().position(|item| *item == self).unwrap_or(0);
        let next = (current as isize + delta).rem_euclid(items.len() as isize) as usize;
        items[next]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PrimaryMouseAction {
    None,
    SwitchWorkspace(String),
    StartResize(PaneResizeDrag),
    FocusPane(String),
    FocusWorkspaceTab {
        tab: String,
    },
    /// Close (×) on a workspace tab in the tab bar.
    CloseWorkspaceTab {
        tab: String,
    },
    NewWorkspaceTab,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UiKeyBinding {
    code: KeyCode,
    modifiers: KeyModifiers,
}

impl Ui {
    fn new(
        session: String,
        terminal: Terminal<CrosstermBackend<Stdout>>,
        config: LmuxConfig,
    ) -> Self {
        let (prefix_code, prefix_modifiers) = parse_key_binding(&config.ui.prefix_key)
            .unwrap_or((KeyCode::Char('b'), KeyModifiers::CONTROL));
        let prefix_label = config.ui.prefix_key.clone();
        let client_id = format!(
            "attach-{}-{}",
            std::process::id(),
            crate::model::unix_time()
        );
        Self {
            session,
            terminal,
            snapshot: None,
            prefix: false,
            prefix_key: UiKeyBinding {
                code: prefix_code,
                modifiers: prefix_modifiers,
            },
            prefix_label,
            mode: UiMode::Panes,
            sidebar_collapsed: config.ui.sidebar_collapsed,
            sidebar_width: crate::config::clamp_sidebar_width(config.ui.sidebar_width),
            sidebar_resize_drag: false,
            sidebar_drag_workspace: None,
            pane_resize_drag: None,
            context_pane: None,
            notification_selected: 0,
            action_selected: 0,
            command_selected: 0,
            command_filter: String::new(),
            settings_selected: 0,
            context_selected: 0,
            hover_control: None,
            hover_pane_control: None,
            hover_workspace: None,
            hover_empty_create: false,
            pending_confirm: None,
            rename_dialog: None,
            last_click: None,
            selection: None,
            theme: UiTheme::from_name(&config.ui.theme),
            workspace_second_line: UiWorkspaceSecondLine::from_name(
                &config.ui.workspace_second_line,
            ),
            scroll_step: config.ui.scroll_step,
            cursor_blink: config.ui.cursor_blink,
            cursor_blink_ms: config.ui.cursor_blink_ms,
            status_markers: config.ui.status_markers.clone(),
            default_shell: config.ui.default_shell.clone(),
            default_cwd: config.ui.default_cwd.clone(),
            mouse: config.ui.mouse,
            tab_close_button: config.ui.tab_close_button,
            bell_on_attention: config.ui.bell_on_attention,
            sidebar_responsive: config.ui.sidebar_responsive,
            workspace_picker_selected: 0,
            mobile_relay_enabled: config.relay.enabled,
            mobile_relay_bind: config.relay.bind.clone(),
            mobile_relay_port: config.relay.port,
            mobile_relay_allow_localhost: config.relay.allow_localhost,
            mobile_relay_allow_cgnat: config.relay.allow_tailnet_cgnat,
            prev_attention_panes: BTreeSet::new(),
            actions: Vec::new(),
            action_error: std::cell::RefCell::new(None),
            client_id,
            pane_size_claimed: false,
            pane_size_control_requested: false,
            last_pane_sizes: BTreeMap::new(),
            scroll_offsets: BTreeMap::new(),
            sidebar_scroll: 0,
            sidebar_active_seen: None,
            last_snapshot_data: None,
            pending_refresh: std::cell::Cell::new(false),
            last_typing_at: None,
        }
    }

    fn run(&mut self) -> Result<()> {
        let mut last_refresh = Instant::now() - Duration::from_secs(1);
        // Draw the first frame unconditionally; afterwards only redraw when
        // something actually changed (snapshot, an event, or a resize).
        let mut dirty = true;
        let started = Instant::now();
        let mut last_blink_phase = 0u8;
        let mut was_typing_solid = false;
        loop {
            // Active-pane cursor: solid while typing; otherwise blink at configured rate.
            // Inactive panes never show a caret.
            const TYPING_SOLID_MS: u128 = 1000;
            let blink_half_ms = self.cursor_blink_ms.max(200) as u128;
            let typing_solid = self
                .last_typing_at
                .map(|t| t.elapsed().as_millis() < TYPING_SOLID_MS)
                .unwrap_or(false);
            if typing_solid != was_typing_solid {
                was_typing_solid = typing_solid;
                dirty = true;
            }
            let blink_phase = ((started.elapsed().as_millis() / blink_half_ms) % 2) as u8;
            let cursor_blink_on = if !self.cursor_blink {
                true // solid caret when blink disabled
            } else {
                typing_solid || blink_phase == 0
            };
            if self.cursor_blink && blink_phase != last_blink_phase {
                last_blink_phase = blink_phase;
                // Only redraw for blink when not holding solid for typing.
                if !typing_solid {
                    dirty = true;
                }
            }

            // Periodic refresh so external changes (other clients, notifications)
            // still appear while idle.
            if last_refresh.elapsed() > Duration::from_millis(150) {
                if self.refresh()? {
                    dirty = true;
                }
                last_refresh = Instant::now();
            }

            if dirty {
                self.update_sidebar_scroll();
                // Borrow individual fields for the draw closure instead of
                // cloning the whole snapshot/actions/scroll state each frame.
                let mut pane_sizes = BTreeMap::new();
                {
                    // Scope the `action_error` borrow so its `Ref` is released
                    // before `sync_pane_sizes` (which calls `rpc`, i.e.
                    // `borrow_mut`) runs below.
                    let action_error = self.action_error.borrow();
                    // Hide pane corner chrome while a modal is open so it cannot
                    // draw under / through the alert box.
                    let hover_pane =
                        if self.pending_confirm.is_some() || self.rename_dialog.is_some() {
                            None
                        } else {
                            self.hover_pane_control.as_ref()
                        };
                    self.terminal.draw(|frame| {
                        draw(
                            frame,
                            self.snapshot.as_ref(),
                            &mut pane_sizes,
                            &self.scroll_offsets,
                            self.mode,
                            self.sidebar_collapsed,
                            self.sidebar_width,
                            self.sidebar_scroll,
                            self.notification_selected,
                            &self.actions,
                            self.action_selected,
                            action_error.as_deref(),
                            self.command_selected,
                            &self.command_filter,
                            &self.prefix_label,
                            self.settings_selected,
                            self.context_selected,
                            self.context_pane.as_deref(),
                            self.hover_control,
                            hover_pane,
                            self.hover_workspace.as_deref(),
                            self.hover_empty_create,
                            self.pending_confirm.as_ref(),
                            self.rename_dialog.as_ref(),
                            self.selection.as_ref(),
                            self.theme,
                            self.workspace_second_line,
                            cursor_blink_on,
                            self.status_markers.as_str(),
                            self.tab_close_button,
                            self.scroll_step,
                            self.cursor_blink,
                            self.cursor_blink_ms,
                            self.default_shell.as_str(),
                            self.default_cwd.as_str(),
                            self.mouse,
                            self.bell_on_attention,
                            self.mobile_relay_enabled,
                            self.mobile_relay_bind.as_str(),
                            self.mobile_relay_port,
                            self.mobile_relay_allow_localhost,
                            self.mobile_relay_allow_cgnat,
                            self.sidebar_responsive,
                            self.workspace_picker_selected,
                        )
                    })?;
                }
                self.sync_pane_sizes(pane_sizes)?;
                dirty = false;
            }

            // Block for the first event (so the loop still ticks for the
            // periodic refresh), then drain everything already queued and draw
            // at most once. Exit promptly if any event requests quit.
            if event::poll(Duration::from_millis(50))? {
                self.pending_refresh.set(false);
                loop {
                    let event = event::read()?;
                    if self.handle_event(event)? {
                        return Ok(());
                    }
                    // Any handled event can change what's on screen (including
                    // mouse-move hover highlighting and terminal resize).
                    dirty = true;
                    if !event::poll(Duration::ZERO)? {
                        break;
                    }
                }
                // If any drained event sent input to a pane, refresh now so the
                // echo appears immediately rather than at the next 150ms tick.
                // Draining first debounces a fast typing burst into one RPC.
                if self.pending_refresh.get() {
                    self.pending_refresh.set(false);
                    if self.refresh()? {
                        dirty = true;
                    }
                    last_refresh = Instant::now();
                }
            }
        }
    }

    /// Fetches a fresh snapshot. Returns `true` if the snapshot changed since
    /// the previous refresh (used to drive dirty-flag rendering). The raw JSON
    /// value is compared because `Session` is defined elsewhere and may not be
    /// cheap/available to compare directly.
    fn refresh(&mut self) -> Result<bool> {
        let response = protocol::request(&paths::socket_path(&self.session)?, &Request::Snapshot)?;
        if let Some(data) = response.data {
            if self.last_snapshot_data.as_ref() == Some(&data) {
                return Ok(false);
            }
            self.snapshot = Some(serde_json::from_value(data.clone())?);
            self.last_snapshot_data = Some(data);
            self.maybe_bell_on_attention();
            return Ok(true);
        }
        Ok(false)
    }

    fn maybe_bell_on_attention(&mut self) {
        if !self.bell_on_attention {
            return;
        }
        let Some(snapshot) = self.snapshot.as_ref() else {
            return;
        };
        let now: BTreeSet<String> = snapshot
            .panes
            .iter()
            .filter(|(_, pane)| {
                matches!(pane.agent_status, crate::model::AgentStatus::Attention)
                    || pane.notification_message.is_some()
            })
            .map(|(id, _)| id.clone())
            .collect();
        let new_attention = now.difference(&self.prev_attention_panes).next().is_some();
        self.prev_attention_panes = now;
        if new_attention {
            let _ = write!(io::stdout(), "\x07");
            let _ = io::stdout().flush();
        }
    }

    fn is_prefix_key(&self, code: KeyCode, modifiers: KeyModifiers) -> bool {
        code == self.prefix_key.code && modifiers == self.prefix_key.modifiers
    }

    fn handle_event(&mut self, event: Event) -> Result<bool> {
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                self.pane_size_control_requested = true;
                if self.prefix {
                    self.prefix = false;
                    // Prefix keys that map to a command-palette action are driven
                    // by the shared `prefix_action_bindings` table so the palette
                    // shortcut column can never drift from the real binding. The
                    // remaining keys drive behaviors with no palette action.
                    match key.code {
                        KeyCode::Char('q') => return Ok(true),
                        KeyCode::Char('B') => {
                            // Compact / mobile: open ☰ picker. Desktop: toggle collapse.
                            if self.is_compact_layout() {
                                self.toggle_workspace_picker();
                            } else {
                                self.sidebar_collapsed = !self.sidebar_collapsed;
                            }
                        }
                        KeyCode::Char('w') => self.toggle_workspace_picker(),
                        KeyCode::Char('P') => self.toggle_commands(),
                        KeyCode::Char('u') => self.jump_notification()?,
                        KeyCode::Tab => self.focus_direction(SplitDirection::Right)?,
                        KeyCode::PageUp => self.scroll_active(self.scroll_step as isize),
                        KeyCode::PageDown => self.scroll_active(-(self.scroll_step as isize)),
                        KeyCode::Home => self.reset_active_scroll(),
                        code => {
                            if let Some(action) = prefix_key_action(code) {
                                self.run_palette_action(action)?;
                            }
                        }
                    }
                    return Ok(false);
                }
                if self.mode == UiMode::Notifications {
                    match key.code {
                        KeyCode::Esc => self.mode = UiMode::Panes,
                        KeyCode::Up | KeyCode::Char('k') => self.move_notification_selection(-1),
                        KeyCode::Down | KeyCode::Char('j') => self.move_notification_selection(1),
                        KeyCode::Home => self.notification_selected = 0,
                        KeyCode::End => self.select_last_notification(),
                        KeyCode::Enter => self.jump_selected_notification()?,
                        _ => {}
                    }
                    return Ok(false);
                }
                if self.mode == UiMode::Actions {
                    match key.code {
                        KeyCode::Esc => self.mode = UiMode::Panes,
                        KeyCode::Up | KeyCode::Char('k') => self.move_action_selection(-1),
                        KeyCode::Down | KeyCode::Char('j') => self.move_action_selection(1),
                        KeyCode::Home => self.action_selected = 0,
                        KeyCode::End => self.select_last_action(),
                        KeyCode::Enter => self.run_selected_action()?,
                        _ => {}
                    }
                    return Ok(false);
                }
                if self.mode == UiMode::Commands {
                    // Letters now type into the filter, so navigation is limited
                    // to arrows/Home/End. Esc clears a non-empty filter first,
                    // then closes the palette on a second press.
                    match key.code {
                        KeyCode::Esc => {
                            if self.command_filter.is_empty() {
                                self.mode = UiMode::Panes;
                            } else {
                                self.command_filter.clear();
                                self.command_selected = 0;
                            }
                        }
                        KeyCode::Up => self.move_command_selection(-1),
                        KeyCode::Down => self.move_command_selection(1),
                        KeyCode::Home => self.command_selected = 0,
                        KeyCode::End => self.select_last_command(),
                        KeyCode::Enter => self.run_selected_command()?,
                        KeyCode::Backspace => {
                            self.command_filter.pop();
                            self.command_selected = 0;
                        }
                        KeyCode::Char(c)
                            if !key.modifiers.contains(KeyModifiers::CONTROL)
                                && !key.modifiers.contains(KeyModifiers::ALT) =>
                        {
                            self.command_filter.push(c);
                            self.command_selected = 0;
                        }
                        _ => {}
                    }
                    return Ok(false);
                }
                if self.mode == UiMode::Settings {
                    match key.code {
                        KeyCode::Esc => self.mode = UiMode::Panes,
                        KeyCode::Up | KeyCode::Char('k') => self.move_settings_selection(-1),
                        KeyCode::Down | KeyCode::Char('j') => self.move_settings_selection(1),
                        KeyCode::Left | KeyCode::Char('h') => self.adjust_selected_setting(-1)?,
                        KeyCode::Right | KeyCode::Char('l') | KeyCode::Enter => {
                            self.adjust_selected_setting(1)?
                        }
                        _ => {}
                    }
                    return Ok(false);
                }
                if self.mode == UiMode::WorkspacePicker {
                    match key.code {
                        KeyCode::Esc => self.mode = UiMode::Panes,
                        KeyCode::Up | KeyCode::Char('k') => self.move_workspace_picker_selection(-1),
                        KeyCode::Down | KeyCode::Char('j') => self.move_workspace_picker_selection(1),
                        KeyCode::Home => self.workspace_picker_selected = 0,
                        KeyCode::End => {
                            let n = self
                                .snapshot
                                .as_ref()
                                .map(|s| s.workspaces.len())
                                .unwrap_or(0);
                            self.workspace_picker_selected = n.saturating_sub(1);
                        }
                        KeyCode::Enter => self.activate_workspace_picker_selection()?,
                        _ => {}
                    }
                    return Ok(false);
                }
                if self.mode == UiMode::ContextMenu {
                    match key.code {
                        KeyCode::Esc => self.close_context_menu(),
                        KeyCode::Up | KeyCode::Char('k') => self.move_context_selection(-1),
                        KeyCode::Down | KeyCode::Char('j') => self.move_context_selection(1),
                        KeyCode::Home => self.context_selected = 0,
                        KeyCode::End => self.select_last_context_item(),
                        KeyCode::Enter => self.run_selected_context_item()?,
                        _ => {}
                    }
                    return Ok(false);
                }
                // Rename dialog captures typing while open.
                if self.rename_dialog.is_some() {
                    self.handle_rename_key(key.code, key.modifiers)?;
                    return Ok(false);
                }
                // Modal alert captures keys while open (overlay, not full-screen mode).
                if self.pending_confirm.is_some() {
                    match key.code {
                        KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                            self.cancel_confirm()
                        }
                        KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                            self.confirm_pending()?
                        }
                        _ => {}
                    }
                    return Ok(false);
                }
                if self.is_prefix_key(key.code, key.modifiers) {
                    self.prefix = true;
                    return Ok(false);
                }
                if let Some(data) = key_to_input(key.code, key.modifiers) {
                    if let Some(pane) = self.active_pane() {
                        self.note_typing();
                        self.rpc(&Request::Input {
                            pane: Some(pane),
                            data,
                        })?;
                    }
                }
            }
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    self.pane_size_control_requested = true;
                    if self.handle_click(mouse.column, mouse.row)? {
                        return Ok(true);
                    }
                }
                MouseEventKind::Drag(MouseButton::Left) => {
                    self.pane_size_control_requested = true;
                    if self.sidebar_resize_drag {
                        self.handle_sidebar_resize_drag(mouse.column)?;
                    } else if self.pane_resize_drag.is_some() {
                        self.handle_pane_resize_drag(mouse.column, mouse.row)?;
                    } else {
                        self.handle_drag(mouse.column, mouse.row)?;
                    }
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    if self.sidebar_resize_drag {
                        self.finish_sidebar_resize()?;
                    } else if !self.forward_mouse_to_pane(
                        mouse.column,
                        mouse.row,
                        MouseButtonCode::Left,
                        false,
                    )? {
                        self.finish_selection()?;
                    }
                    self.sidebar_drag_workspace = None;
                    self.pane_resize_drag = None;
                }
                MouseEventKind::Down(MouseButton::Right) => {
                    self.pane_size_control_requested = true;
                    self.handle_right_click(mouse.column, mouse.row)?;
                }
                MouseEventKind::Moved => self.update_hover(mouse.column, mouse.row)?,
                MouseEventKind::ScrollUp => {
                    if !matches!(self.mode, UiMode::Panes) {
                        // Don't scroll panes under overlays.
                    } else {
                        self.pane_size_control_requested = true;
                        if self.scroll_sidebar(mouse.column, -1) {
                        } else if !self.forward_mouse_to_pane(
                            mouse.column,
                            mouse.row,
                            MouseButtonCode::WheelUp,
                            true,
                        )? {
                            self.scroll_at(mouse.column, mouse.row, self.scroll_step as isize)?
                        }
                    }
                }
                MouseEventKind::ScrollDown => {
                    if !matches!(self.mode, UiMode::Panes) {
                    } else {
                        self.pane_size_control_requested = true;
                        if self.scroll_sidebar(mouse.column, 1) {
                        } else if !self.forward_mouse_to_pane(
                            mouse.column,
                            mouse.row,
                            MouseButtonCode::WheelDown,
                            true,
                        )? {
                            self.scroll_at(mouse.column, mouse.row, -(self.scroll_step as isize))?
                        }
                    }
                }
                _ => {}
            },
            Event::Paste(text) => {
                self.pane_size_control_requested = true;
                if let Some(pane) = self.active_pane() {
                    self.note_typing();
                    self.rpc(&Request::Input {
                        pane: Some(pane),
                        data: text,
                    })?;
                }
            }
            Event::Resize(_, _) => {
                self.pane_size_control_requested = true;
                self.last_pane_sizes.clear();
            }
            _ => {}
        }
        Ok(false)
    }

    fn note_typing(&mut self) {
        self.last_typing_at = Some(Instant::now());
    }

    fn request(&self, request: &Request) -> Result<protocol::Response> {
        protocol::request(&paths::socket_path(&self.session)?, request)
    }

    fn rpc(&self, request: &Request) -> Result<()> {
        let response = self.request(request)?;
        // Every state-changing RPC goes through here, so a single flag set makes
        // the run loop refresh immediately (pane/workspace creation, splits,
        // layout changes, input echo) rather than waiting for the 150ms tick.
        // Over-refreshing on a no-op RPC is harmless; the snapshot diff is cheap.
        self.pending_refresh.set(true);
        // Transport errors already surface via `?` above. Daemon-side failures
        // come back as `ok:false` (e.g. unknown pane, resize with no split, kill
        // on a missing pane); route them to the shared error display instead of
        // silently succeeding. Only the latest error is kept, so high-frequency
        // paths (per-keystroke input) never spam and never block typing.
        if let Some(message) = response_error(&response) {
            self.set_action_error(message);
        }
        Ok(())
    }

    /// Records a non-fatal error to surface in the UI. Interior-mutable so it is
    /// callable from `&self` RPC helpers.
    fn set_action_error(&self, message: String) {
        *self.action_error.borrow_mut() = Some(message);
    }

    fn sync_pane_sizes(&mut self, pane_sizes: BTreeMap<String, PaneSize>) -> Result<()> {
        if pane_sizes.is_empty() {
            self.pane_size_control_requested = false;
            return Ok(());
        }
        let take_control = !self.pane_size_claimed || self.pane_size_control_requested;
        if pane_sizes == self.last_pane_sizes && !take_control {
            return Ok(());
        }
        self.rpc(&Request::PaneSizes {
            panes: pane_sizes.clone(),
            client_id: Some(self.client_id.clone()),
            take_control,
        })?;
        self.pane_size_claimed = true;
        self.pane_size_control_requested = false;
        self.last_pane_sizes = pane_sizes;
        Ok(())
    }

    fn new_pane(&self, direction: SplitDirection) -> Result<()> {
        self.rpc(&Request::NewPane {
            direction,
            command: String::new(),
            title: None,
            workspace: None,
            surface_kind: None,
        })
    }

    fn new_workspace(&self) -> Result<()> {
        let response = self.request(&Request::NewWorkspace {
            name: "workspace".to_string(),
            cwd: std::env::current_dir()
                .ok()
                .map(|path| path.display().to_string()),
        })?;
        let workspace = response
            .data
            .as_ref()
            .and_then(|data| data.get("id"))
            .and_then(|id| id.as_str())
            .map(str::to_string);
        self.rpc(&Request::NewPane {
            direction: SplitDirection::Right,
            command: String::new(),
            title: None,
            workspace,
            surface_kind: None,
        })
    }

    /// Requests closing the active pane, prompting with an overlay alert.
    fn request_close_active_pane(&mut self) -> Result<()> {
        let Some(pane) = self.active_pane() else {
            return Ok(());
        };
        self.request_close_pane(pane)
    }

    /// Opens the close-pane alert overlay (keeps the pane grid visible and
    /// highlights the target pane). Does not switch the whole UI away from panes.
    fn request_close_pane(&mut self, pane_id: String) -> Result<()> {
        let Some(snapshot) = self.snapshot.as_ref() else {
            return Ok(());
        };
        let Some(pending) = pending_close_for_pane(snapshot, &pane_id) else {
            return Ok(());
        };
        self.pending_confirm = Some(pending);
        // Stay on Panes so the layout remains under the alert.
        self.mode = UiMode::Panes;
        Ok(())
    }

    /// Performs the pending confirmed action and dismisses the alert.
    fn confirm_pending(&mut self) -> Result<()> {
        let Some(pending) = self.pending_confirm.take() else {
            return Ok(());
        };
        self.rpc(&confirm_request(&pending.action))?;
        self.mode = UiMode::Panes;
        Ok(())
    }

    /// Dismisses a pending confirmation without acting.
    fn cancel_confirm(&mut self) {
        self.pending_confirm = None;
        self.mode = UiMode::Panes;
    }

    fn open_rename(&mut self, target: RenameTarget, current: String) {
        self.rename_dialog = Some(RenameDialog {
            target,
            draft: current,
        });
        self.last_click = None;
        self.pending_confirm = None;
        self.mode = UiMode::Panes;
    }

    fn cancel_rename(&mut self) {
        self.rename_dialog = None;
    }

    fn submit_rename(&mut self) -> Result<()> {
        let Some(dialog) = self.rename_dialog.take() else {
            return Ok(());
        };
        let name = dialog.draft.trim().to_string();
        if name.is_empty() {
            self.set_action_error("name cannot be empty".to_string());
            // Re-open so the user can fix it.
            self.rename_dialog = Some(dialog);
            return Ok(());
        }
        match dialog.target {
            RenameTarget::Workspace { id } => {
                self.rpc(&Request::RenameWorkspace {
                    workspace: id,
                    name,
                })?;
            }
            RenameTarget::Tab { id } => {
                self.rpc(&Request::RenameTab {
                    workspace: None,
                    tab: id,
                    title: name,
                })?;
            }
            RenameTarget::Pane { id } => {
                self.rpc(&Request::SetPaneTitle {
                    pane: Some(id),
                    title: name,
                })?;
            }
        }
        Ok(())
    }

    fn handle_rename_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> Result<()> {
        match code {
            KeyCode::Esc => self.cancel_rename(),
            KeyCode::Enter => self.submit_rename()?,
            KeyCode::Backspace => {
                if let Some(dialog) = self.rename_dialog.as_mut() {
                    dialog.draft.pop();
                }
            }
            KeyCode::Char(ch)
                if !modifiers.contains(KeyModifiers::CONTROL)
                    && !modifiers.contains(KeyModifiers::ALT) =>
            {
                if let Some(dialog) = self.rename_dialog.as_mut() {
                    if !ch.is_control() && dialog.draft.len() < 64 {
                        dialog.draft.push(ch);
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// True when this click completes a double-click on the same rename target.
    fn is_double_click(&self, column: u16, row: u16, target: &RenameTarget) -> bool {
        let Some(prev) = &self.last_click else {
            return false;
        };
        if &prev.target != target {
            return false;
        }
        if prev.at.elapsed().as_millis() > DOUBLE_CLICK_MS {
            return false;
        }
        // Allow a little pointer jitter between clicks.
        column.abs_diff(prev.column) <= 2 && row.abs_diff(prev.row) <= 1
    }

    fn duplicate_active_pane(&self) -> Result<()> {
        self.rpc(&Request::DuplicatePane {
            pane: None,
            direction: SplitDirection::Right,
        })
    }

    fn restart_active_pane(&self) -> Result<()> {
        self.rpc(&Request::RestartPane {
            pane: None,
            workspace: None,
            all: false,
            command: None,
        })
    }

    fn clear_active_pane(&self) -> Result<()> {
        self.rpc(&Request::ClearPane { pane: None })
    }

    fn copy_active_pane(&self) -> Result<()> {
        self.rpc(&Request::CopyPane {
            pane: None,
            scrollback: false,
            limit_bytes: None,
        })
    }

    fn paste_active_pane(&self) -> Result<()> {
        self.rpc(&Request::Paste {
            pane: None,
            enter: false,
        })
    }

    fn switch_active_workspace_tab(&self, delta: isize) -> Result<()> {
        let Some(snapshot) = &self.snapshot else {
            return Ok(());
        };
        let Some(workspace) = snapshot
            .workspaces
            .iter()
            .find(|w| w.id == snapshot.active_workspace)
        else {
            return Ok(());
        };
        if workspace.tabs.is_empty() {
            return Ok(());
        }
        let idx = workspace
            .active_tab
            .as_ref()
            .and_then(|id| workspace.tabs.iter().position(|t| &t.id == id))
            .unwrap_or(0);
        let next = if delta >= 0 {
            (idx + delta as usize) % workspace.tabs.len()
        } else {
            (idx + workspace.tabs.len() - ((-delta) as usize % workspace.tabs.len()))
                % workspace.tabs.len()
        };
        let tab = workspace.tabs[next].id.clone();
        self.rpc(&Request::SwitchTab {
            workspace: None,
            tab,
        })
    }

    fn new_workspace_tab(&self) -> Result<()> {
        self.rpc(&Request::NewTab {
            workspace: None,
            title: Some("tab".to_string()),
            command: None,
        })
    }

    fn set_active_agent_status(
        &self,
        status: &'static str,
        color: &'static str,
        message: &'static str,
    ) -> Result<()> {
        self.rpc(&Request::Notify {
            pane: None,
            workspace: None,
            status: Some(status.to_string()),
            color: Some(color.to_string()),
            clear: false,
            message: message.to_string(),
        })
    }

    fn close_active_workspace(&self) -> Result<()> {
        self.rpc(&Request::CloseWorkspace { workspace: None })
    }

    fn toggle_notifications(&mut self) {
        self.mode = if self.mode == UiMode::Notifications {
            UiMode::Panes
        } else {
            self.clamp_notification_selection();
            UiMode::Notifications
        };
    }

    fn toggle_actions(&mut self) -> Result<()> {
        if self.mode == UiMode::Actions {
            self.mode = UiMode::Panes;
            return Ok(());
        }
        self.load_actions()?;
        self.clamp_action_selection();
        self.mode = UiMode::Actions;
        Ok(())
    }

    fn toggle_commands(&mut self) {
        self.mode = if self.mode == UiMode::Commands {
            UiMode::Panes
        } else {
            self.command_filter.clear();
            self.command_selected = 0;
            UiMode::Commands
        };
    }

    fn toggle_settings(&mut self) {
        self.mode = if self.mode == UiMode::Settings {
            UiMode::Panes
        } else {
            self.settings_selected = self.settings_selected.min(settings_entries().len() - 1);
            UiMode::Settings
        };
    }

    fn toggle_workspace_picker(&mut self) {
        self.mode = if self.mode == UiMode::WorkspacePicker {
            UiMode::Panes
        } else {
            self.sync_workspace_picker_selection();
            UiMode::WorkspacePicker
        };
    }

    fn sync_workspace_picker_selection(&mut self) {
        let Some(snapshot) = self.snapshot.as_ref() else {
            self.workspace_picker_selected = 0;
            return;
        };
        self.workspace_picker_selected = snapshot
            .workspaces
            .iter()
            .position(|w| w.id == snapshot.active_workspace)
            .unwrap_or(0);
    }

    fn move_workspace_picker_selection(&mut self, delta: isize) {
        let count = self
            .snapshot
            .as_ref()
            .map(|s| s.workspaces.len())
            .unwrap_or(0);
        if count == 0 {
            self.workspace_picker_selected = 0;
            return;
        }
        let current = self.workspace_picker_selected.min(count - 1);
        self.workspace_picker_selected = if delta.is_negative() {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            current
                .saturating_add(delta as usize)
                .min(count.saturating_sub(1))
        };
    }

    fn activate_workspace_picker_selection(&mut self) -> Result<()> {
        let Some(snapshot) = self.snapshot.as_ref() else {
            return Ok(());
        };
        let Some(ws) = snapshot.workspaces.get(self.workspace_picker_selected) else {
            return Ok(());
        };
        let id = ws.id.clone();
        self.rpc(&Request::SwitchWorkspace { workspace: id })?;
        self.mode = UiMode::Panes;
        Ok(())
    }

    /// True when the terminal is narrow enough to use the mobile/burger layout.
    fn is_compact_layout(&self) -> bool {
        if !self.sidebar_responsive {
            return false;
        }
        terminal_size()
            .map(|(w, _)| w < COMPACT_TERM_WIDTH)
            .unwrap_or(false)
    }

    /// Sidebar column count after responsive rules (0 = fully hidden).
    fn layout_sidebar_cols(&self) -> u16 {
        let (term_w, _) = terminal_size().unwrap_or((80, 24));
        if self.sidebar_responsive && term_w < COMPACT_TERM_WIDTH {
            0
        } else {
            sidebar_width(self.sidebar_collapsed, self.sidebar_width)
        }
    }

    fn move_settings_selection(&mut self, delta: isize) {
        let entries = settings_entries();
        let count = entries.len();
        if count == 0 {
            return;
        }
        let mut current = self.settings_selected.min(count.saturating_sub(1));
        // Skip non-interactive section headers when moving.
        for _ in 0..count {
            current = if delta.is_negative() {
                current.saturating_sub(1)
            } else {
                current.saturating_add(1).min(count.saturating_sub(1))
            };
            if !matches!(
                entries[current].id,
                SettingsEntryId::Section | SettingsEntryId::SectionRelay
            ) {
                break;
            }
            if (delta.is_negative() && current == 0)
                || (!delta.is_negative() && current + 1 >= count)
            {
                break;
            }
        }
        // If we landed on a section (edge case), nudge off it.
        if matches!(
            entries[current].id,
            SettingsEntryId::Section | SettingsEntryId::SectionRelay
        ) {
            if current + 1 < count {
                current += 1;
            } else if current > 0 {
                current -= 1;
            }
        }
        self.settings_selected = current;
    }

    fn adjust_selected_setting(&mut self, delta: isize) -> Result<()> {
        let entries = settings_entries();
        let Some(entry) = entries.get(self.settings_selected) else {
            return Ok(());
        };
        match entry.id {
            SettingsEntryId::Theme => self.set_theme(self.theme.relative(delta))?,
            SettingsEntryId::WorkspaceLine => {
                self.set_workspace_second_line(self.workspace_second_line.relative(delta))?;
            }
            SettingsEntryId::Sidebar => {
                self.sidebar_collapsed = !self.sidebar_collapsed;
                self.save_ui_config("ui.sidebar_collapsed", &self.sidebar_collapsed.to_string())?;
            }
            SettingsEntryId::SidebarResponsive => {
                self.sidebar_responsive = !self.sidebar_responsive;
                self.save_ui_config(
                    "ui.sidebar_responsive",
                    &self.sidebar_responsive.to_string(),
                )?;
            }
            SettingsEntryId::SidebarWidth => {
                let next = if delta.is_negative() {
                    self.sidebar_width.saturating_sub(2)
                } else {
                    self.sidebar_width.saturating_add(2)
                };
                self.sidebar_width = crate::config::clamp_sidebar_width(next);
                if self.sidebar_collapsed && delta > 0 {
                    self.sidebar_collapsed = false;
                    self.save_ui_config("ui.sidebar_collapsed", "false")?;
                }
                self.save_ui_config("ui.sidebar_width", &self.sidebar_width.to_string())?;
            }
            SettingsEntryId::PrefixKey => {
                let next = crate::config::cycle_choice(
                    &crate::config::prefix_key_choices(),
                    &self.prefix_label,
                    delta,
                );
                self.apply_prefix_key(&next)?;
            }
            SettingsEntryId::ScrollStep => {
                let next = crate::config::cycle_usize(
                    &crate::config::scroll_step_choices(),
                    self.scroll_step,
                    delta,
                );
                self.scroll_step = next;
                self.save_ui_config("ui.scroll_step", &next.to_string())?;
            }
            SettingsEntryId::CursorBlink => {
                self.cursor_blink = !self.cursor_blink;
                self.save_ui_config("ui.cursor_blink", &self.cursor_blink.to_string())?;
            }
            SettingsEntryId::CursorBlinkMs => {
                let next = crate::config::cycle_u64(
                    &crate::config::cursor_blink_ms_choices(),
                    self.cursor_blink_ms,
                    delta,
                );
                self.cursor_blink_ms = next;
                self.save_ui_config("ui.cursor_blink_ms", &next.to_string())?;
            }
            SettingsEntryId::StatusMarkers => {
                let next = crate::config::cycle_choice(
                    &crate::config::supported_status_markers(),
                    &self.status_markers,
                    delta,
                );
                self.status_markers = next.clone();
                self.save_ui_config("ui.status_markers", &next)?;
            }
            SettingsEntryId::DefaultShell => {
                let next = crate::config::cycle_choice(
                    &crate::config::default_shell_choices(),
                    &self.default_shell,
                    delta,
                );
                self.default_shell = next.clone();
                self.save_ui_config("ui.default_shell", &next)?;
            }
            SettingsEntryId::DefaultCwd => {
                let next = crate::config::cycle_choice(
                    &crate::config::supported_default_cwds(),
                    &self.default_cwd,
                    delta,
                );
                self.default_cwd = next.clone();
                self.save_ui_config("ui.default_cwd", &next)?;
            }
            SettingsEntryId::Mouse => {
                self.mouse = !self.mouse;
                self.apply_mouse_capture(self.mouse)?;
                self.save_ui_config("ui.mouse", &self.mouse.to_string())?;
            }
            SettingsEntryId::TabCloseButton => {
                self.tab_close_button = !self.tab_close_button;
                self.save_ui_config("ui.tab_close_button", &self.tab_close_button.to_string())?;
            }
            SettingsEntryId::BellOnAttention => {
                self.bell_on_attention = !self.bell_on_attention;
                self.save_ui_config("ui.bell_on_attention", &self.bell_on_attention.to_string())?;
            }
            SettingsEntryId::MobileRelay => {
                self.mobile_relay_enabled = !self.mobile_relay_enabled;
                if let Err(err) =
                    self.save_ui_config("relay.enabled", &self.mobile_relay_enabled.to_string())
                {
                    *self.action_error.get_mut() = Some(format!("save config: {err:#}"));
                }
                // Never fail attach on relay start/stop (improve.md #14).
                let settings = self.relay_settings();
                let session = self.session.clone();
                match crate::relay::apply_enabled(&session, &settings) {
                    Ok(msg) => *self.action_error.get_mut() = Some(msg),
                    Err(err) => {
                        *self.action_error.get_mut() =
                            Some(format!("mobile relay error: {err:#}"));
                    }
                }
            }
            SettingsEntryId::MobileRelayBind => {
                let next = crate::config::cycle_choice(
                    &crate::config::supported_relay_binds(),
                    &self.mobile_relay_bind,
                    delta,
                );
                self.mobile_relay_bind = next.clone();
                let _ = self.save_ui_config("relay.bind", &next);
                if self.mobile_relay_enabled {
                    let settings = self.relay_settings();
                    let session = self.session.clone();
                    let _ = crate::relay::stop_managed();
                    match crate::relay::ensure_started(&session, &settings) {
                        Ok(Some(msg)) => *self.action_error.get_mut() = Some(msg),
                        Ok(None) => {}
                        Err(err) => {
                            *self.action_error.get_mut() =
                                Some(format!("mobile relay error: {err:#}"));
                        }
                    }
                }
            }
            SettingsEntryId::MobileRelayLocalhost => {
                self.mobile_relay_allow_localhost = !self.mobile_relay_allow_localhost;
                let _ = self.save_ui_config(
                    "relay.allow_localhost",
                    &self.mobile_relay_allow_localhost.to_string(),
                );
                if self.mobile_relay_enabled {
                    let settings = self.relay_settings();
                    let session = self.session.clone();
                    let _ = crate::relay::stop_managed();
                    if let Err(err) = crate::relay::ensure_started(&session, &settings) {
                        *self.action_error.get_mut() =
                            Some(format!("mobile relay error: {err:#}"));
                    }
                }
            }
            SettingsEntryId::HookShell
            | SettingsEntryId::HookClaude
            | SettingsEntryId::HookCodex
            | SettingsEntryId::HookGrok
            | SettingsEntryId::HookInstallAll => {
                // Install / re-install agent hooks (Enter / l / h all install).
                self.install_selected_agent_hooks(entry.id)?;
            }
            SettingsEntryId::Section | SettingsEntryId::SectionRelay => {}
        }
        Ok(())
    }

    fn relay_settings(&self) -> crate::config::RelaySettings {
        crate::config::RelaySettings {
            enabled: self.mobile_relay_enabled,
            bind: self.mobile_relay_bind.clone(),
            port: self.mobile_relay_port,
            allow_localhost: self.mobile_relay_allow_localhost,
            allow_tailnet_cgnat: self.mobile_relay_allow_cgnat,
        }
    }

    fn apply_prefix_key(&mut self, binding: &str) -> Result<()> {
        let (code, modifiers) =
            parse_key_binding(binding).with_context(|| format!("invalid prefix key {binding}"))?;
        self.prefix_key = UiKeyBinding { code, modifiers };
        self.prefix_label = binding.to_string();
        self.save_ui_config("ui.prefix_key", binding)
    }

    fn apply_mouse_capture(&mut self, enabled: bool) -> Result<()> {
        if enabled {
            execute!(io::stdout(), event::EnableMouseCapture)?;
        } else {
            execute!(io::stdout(), event::DisableMouseCapture)?;
        }
        Ok(())
    }

    fn install_selected_agent_hooks(&mut self, id: SettingsEntryId) -> Result<()> {
        let home = crate::agent_hooks::home_dir();
        match id {
            SettingsEntryId::HookInstallAll => {
                crate::agent_hooks::install_all(&home)?;
            }
            SettingsEntryId::HookShell => {
                crate::agent_hooks::install_one(crate::agent_hooks::IntegrationKind::Shell, &home)?;
            }
            SettingsEntryId::HookClaude => {
                crate::agent_hooks::install_one(
                    crate::agent_hooks::IntegrationKind::Claude,
                    &home,
                )?;
            }
            SettingsEntryId::HookCodex => {
                crate::agent_hooks::install_one(crate::agent_hooks::IntegrationKind::Codex, &home)?;
            }
            SettingsEntryId::HookGrok => {
                crate::agent_hooks::install_one(crate::agent_hooks::IntegrationKind::Grok, &home)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn set_theme(&mut self, theme: UiTheme) -> Result<()> {
        self.theme = theme;
        self.save_ui_config("ui.theme", theme.name())
    }

    fn set_workspace_second_line(&mut self, second_line: UiWorkspaceSecondLine) -> Result<()> {
        self.workspace_second_line = second_line;
        self.save_ui_config("ui.workspace_second_line", second_line.name())
    }

    fn handle_sidebar_resize_drag(&mut self, column: u16) -> Result<()> {
        // Dragging the edge sets the expanded width. If collapsed, expand first.
        if self.sidebar_collapsed {
            self.sidebar_collapsed = false;
        }
        let (term_w, _) = terminal_size()?;
        // Leave at least 20 columns for the main pane area.
        let max = crate::config::SIDEBAR_MAX_WIDTH.min(term_w.saturating_sub(20));
        let min = crate::config::SIDEBAR_MIN_WIDTH;
        // column is the pointer x; sidebar width is that many columns.
        self.sidebar_width = column.clamp(min, max.max(min));
        Ok(())
    }

    fn finish_sidebar_resize(&mut self) -> Result<()> {
        if !self.sidebar_resize_drag {
            return Ok(());
        }
        self.sidebar_resize_drag = false;
        self.save_ui_config("ui.sidebar_width", &self.sidebar_width.to_string())?;
        Ok(())
    }

    fn save_ui_config(&self, key: &str, value: &str) -> Result<()> {
        let path = paths::config_path()?;
        let mut config = crate::config::load()?;
        crate::config::set_value(&mut config, key, value)?;
        crate::config::save_to_path(&path, &config)?;
        // Keep in-memory config path linked: re-read normalized values for display.
        Ok(())
    }

    fn load_actions(&mut self) -> Result<()> {
        let response = protocol::request(
            &paths::socket_path(&self.session)?,
            &Request::CustomActions { workspace: None },
        )?;
        if response.ok {
            let actions = response
                .data
                .map(serde_json::from_value::<UiActionResponse>)
                .transpose()?
                .map(|data| data.commands)
                .unwrap_or_default();
            self.actions = actions;
            *self.action_error.get_mut() = None;
        } else {
            self.actions.clear();
            *self.action_error.get_mut() = response.error;
        }
        Ok(())
    }

    fn move_notification_selection(&mut self, delta: isize) {
        let count = self.notification_count();
        if count == 0 {
            self.notification_selected = 0;
            return;
        }
        let current = self.notification_selected.min(count - 1);
        self.notification_selected = if delta.is_negative() {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            current.saturating_add(delta as usize).min(count - 1)
        };
    }

    fn select_last_notification(&mut self) {
        let count = self.notification_count();
        self.notification_selected = count.saturating_sub(1);
    }

    fn clamp_notification_selection(&mut self) {
        let count = self.notification_count();
        if count == 0 {
            self.notification_selected = 0;
        } else {
            self.notification_selected = self.notification_selected.min(count - 1);
        }
    }

    fn notification_count(&self) -> usize {
        self.snapshot
            .as_ref()
            .map(|snapshot| snapshot.notifications.len().min(20))
            .unwrap_or(0)
    }

    fn move_action_selection(&mut self, delta: isize) {
        let count = self.actions.len();
        if count == 0 {
            self.action_selected = 0;
            return;
        }
        let current = self.action_selected.min(count - 1);
        self.action_selected = if delta.is_negative() {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            current.saturating_add(delta as usize).min(count - 1)
        };
    }

    fn select_last_action(&mut self) {
        self.action_selected = self.actions.len().saturating_sub(1);
    }

    fn clamp_action_selection(&mut self) {
        if self.actions.is_empty() {
            self.action_selected = 0;
        } else {
            self.action_selected = self.action_selected.min(self.actions.len() - 1);
        }
    }

    fn run_selected_action(&mut self) -> Result<()> {
        let Some(action) = self.actions.get(self.action_selected) else {
            return Ok(());
        };
        self.rpc(&Request::RunCustomAction {
            name: action.name.clone(),
            workspace: None,
        })?;
        self.mode = UiMode::Panes;
        Ok(())
    }

    fn move_command_selection(&mut self, delta: isize) {
        let count = self.filtered_command_entries().len();
        let current = self.command_selected.min(count.saturating_sub(1));
        self.command_selected = if delta.is_negative() {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            current
                .saturating_add(delta as usize)
                .min(count.saturating_sub(1))
        };
    }

    fn select_last_command(&mut self) {
        self.command_selected = self.filtered_command_entries().len().saturating_sub(1);
    }

    /// The command-palette entries currently matching `command_filter`, ranked
    /// best-first. With an empty filter this is the full palette in declaration
    /// order.
    fn filtered_command_entries(&self) -> Vec<CommandPaletteEntry> {
        filter_command_entries(&self.command_filter)
    }

    fn run_selected_command(&mut self) -> Result<()> {
        let entries = self.filtered_command_entries();
        let Some(entry) = entries.get(self.command_selected) else {
            return Ok(());
        };
        let action = entry.action;
        self.run_palette_action(action)?;
        if self.mode == UiMode::Commands {
            self.mode = UiMode::Panes;
        }
        Ok(())
    }

    fn run_palette_action(&mut self, action: CommandPaletteAction) -> Result<()> {
        match action {
            CommandPaletteAction::SplitRight => self.new_pane(SplitDirection::Right)?,
            CommandPaletteAction::SplitDown => self.new_pane(SplitDirection::Down)?,
            CommandPaletteAction::NewWorkspace => self.new_workspace()?,
            CommandPaletteAction::KillPane => self.request_close_active_pane()?,
            CommandPaletteAction::DuplicatePane => self.duplicate_active_pane()?,
            CommandPaletteAction::RestartPane => self.restart_active_pane()?,
            CommandPaletteAction::ClearPane => self.clear_active_pane()?,
            CommandPaletteAction::CopyPane => self.copy_active_pane()?,
            CommandPaletteAction::PastePane => self.paste_active_pane()?,
            CommandPaletteAction::StatusBusy => {
                self.set_active_agent_status("busy", "yellow", "working")?
            }
            CommandPaletteAction::StatusAttention => {
                self.set_active_agent_status("attention", "blue", "needs input")?
            }
            CommandPaletteAction::StatusDone => {
                self.set_active_agent_status("done", "green", "done")?
            }
            CommandPaletteAction::StatusIdle => {
                self.set_active_agent_status("idle", "gray", "idle")?
            }
            CommandPaletteAction::CloseWorkspace => self.close_active_workspace()?,
            CommandPaletteAction::NextWorkspace => self.switch_relative(1)?,
            CommandPaletteAction::PreviousWorkspace => self.switch_relative(-1)?,
            CommandPaletteAction::ToggleNotifications => self.toggle_notifications(),
            CommandPaletteAction::ToggleActions => self.toggle_actions()?,
            CommandPaletteAction::Settings => self.toggle_settings(),
            CommandPaletteAction::ToggleZoom => self.toggle_zoom()?,
            CommandPaletteAction::NextTab => self.switch_active_workspace_tab(1)?,
            CommandPaletteAction::PreviousTab => self.switch_active_workspace_tab(-1)?,
            CommandPaletteAction::NewTab => self.new_workspace_tab()?,
            CommandPaletteAction::FocusLeft => self.focus_direction(SplitDirection::Left)?,
            CommandPaletteAction::FocusRight => self.focus_direction(SplitDirection::Right)?,
            CommandPaletteAction::FocusUp => self.focus_direction(SplitDirection::Up)?,
            CommandPaletteAction::FocusDown => self.focus_direction(SplitDirection::Down)?,
            CommandPaletteAction::ResizeLeft => self.resize(SplitDirection::Left)?,
            CommandPaletteAction::ResizeRight => self.resize(SplitDirection::Right)?,
            CommandPaletteAction::ResizeUp => self.resize(SplitDirection::Up)?,
            CommandPaletteAction::ResizeDown => self.resize(SplitDirection::Down)?,
        }
        Ok(())
    }

    fn run_control_action(&mut self, action: ControlAction) -> Result<bool> {
        match action {
            ControlAction::Workspaces => self.toggle_workspace_picker(),
            ControlAction::NewWorkspace => self.new_workspace()?,
            ControlAction::NewTab => self.new_workspace_tab()?,
            ControlAction::SplitRight => self.new_pane(SplitDirection::Right)?,
            ControlAction::SplitDown => self.new_pane(SplitDirection::Down)?,
            ControlAction::DeleteWorkspace => self.request_close_workspace()?,
            ControlAction::Commands => self.toggle_commands(),
            ControlAction::Notifications => self.toggle_notifications(),
            ControlAction::Settings => self.toggle_settings(),
            ControlAction::Detach => return Ok(true),
            ControlAction::KillPane => self.request_close_active_pane()?,
        }
        Ok(false)
    }

    fn request_close_workspace(&mut self) -> Result<()> {
        if self.active_workspace_has_panes() {
            let body = self
                .snapshot
                .as_ref()
                .map(close_workspace_warning_text)
                .unwrap_or_default();
            self.pending_confirm = Some(PendingConfirm {
                title: " ⚠ close workspace ".to_string(),
                body,
                action: ConfirmAction::CloseWorkspace,
            });
            self.mode = UiMode::Panes;
        } else {
            self.close_active_workspace()?;
        }
        Ok(())
    }

    fn request_close_workspace_tab(&mut self, tab_id: String) -> Result<()> {
        let Some(snapshot) = self.snapshot.as_ref() else {
            return Ok(());
        };
        let Some(workspace) = snapshot
            .workspaces
            .iter()
            .find(|w| w.id == snapshot.active_workspace)
        else {
            return Ok(());
        };
        if workspace.tabs.len() <= 1 {
            self.set_action_error("cannot close the last tab".to_string());
            return Ok(());
        }
        let Some(tab) = workspace.tabs.iter().find(|t| t.id == tab_id) else {
            self.set_action_error(format!("unknown tab {tab_id}"));
            return Ok(());
        };
        let pane_count = tab.panes.len();
        let title = tab.title.clone();
        let body = if pane_count == 0 {
            format!("Close tab '{title}'?\n\nThis tab has no panes.")
        } else {
            format!("Close tab '{title}'?\n\nThis will stop {pane_count} pane(s) on that tab.")
        };
        self.pending_confirm = Some(PendingConfirm {
            title: " ⚠ close tab ".to_string(),
            body,
            action: ConfirmAction::CloseWorkspaceTab { tab: tab_id },
        });
        self.mode = UiMode::Panes;
        Ok(())
    }

    fn active_workspace_is_empty(&self) -> bool {
        self.snapshot
            .as_ref()
            .and_then(|snapshot| {
                snapshot
                    .workspaces
                    .iter()
                    .find(|workspace| workspace.id == snapshot.active_workspace)
            })
            .map(|workspace| workspace.panes.is_empty())
            .unwrap_or(false)
    }

    fn active_workspace_has_panes(&self) -> bool {
        self.snapshot
            .as_ref()
            .and_then(|snapshot| {
                snapshot
                    .workspaces
                    .iter()
                    .find(|workspace| workspace.id == snapshot.active_workspace)
            })
            .map(|workspace| !workspace.panes.is_empty())
            .unwrap_or(false)
    }

    fn run_pane_control(&mut self, hit: PaneControlHit) -> Result<()> {
        match hit.action {
            PaneControlAction::SplitRight => self.rpc(&Request::NewPane {
                direction: SplitDirection::Right,
                command: String::new(),
                title: None,
                workspace: None,
                surface_kind: None,
            })?,
            PaneControlAction::SplitDown => self.rpc(&Request::NewPane {
                direction: SplitDirection::Down,
                command: String::new(),
                title: None,
                workspace: None,
                surface_kind: None,
            })?,
            PaneControlAction::Close => self.request_close_pane(hit.pane)?,
            PaneControlAction::MoveLeft => {
                self.move_pane_in_layout(&hit.pane, SplitDirection::Left)?
            }
            PaneControlAction::MoveRight => {
                self.move_pane_in_layout(&hit.pane, SplitDirection::Right)?
            }
            PaneControlAction::MoveUp => self.move_pane_in_layout(&hit.pane, SplitDirection::Up)?,
            PaneControlAction::MoveDown => {
                self.move_pane_in_layout(&hit.pane, SplitDirection::Down)?
            }
        }
        Ok(())
    }

    fn move_pane_in_layout(&self, pane: &str, direction: SplitDirection) -> Result<()> {
        self.rpc(&Request::MovePaneInLayout {
            pane: Some(pane.to_string()),
            direction,
        })
    }

    fn move_context_selection(&mut self, delta: isize) {
        let count = context_menu_entries().len();
        let current = self.context_selected.min(count.saturating_sub(1));
        self.context_selected = if delta.is_negative() {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            current
                .saturating_add(delta as usize)
                .min(count.saturating_sub(1))
        };
    }

    fn select_last_context_item(&mut self) {
        self.context_selected = context_menu_entries().len().saturating_sub(1);
    }

    fn close_context_menu(&mut self) {
        self.mode = UiMode::Panes;
        self.context_pane = None;
        self.context_selected = 0;
    }

    fn run_selected_context_item(&mut self) -> Result<()> {
        let entries = context_menu_entries();
        let Some(entry) = entries.get(self.context_selected) else {
            return Ok(());
        };
        let Some(pane) = self.context_pane.clone() else {
            self.close_context_menu();
            return Ok(());
        };
        match entry.action {
            ContextMenuAction::CopyPane => self.rpc(&Request::CopyPane {
                pane: Some(pane),
                scrollback: false,
                limit_bytes: None,
            })?,
            ContextMenuAction::PastePane => self.rpc(&Request::Paste {
                pane: Some(pane),
                enter: false,
            })?,
            ContextMenuAction::SplitRight => self.new_pane(SplitDirection::Right)?,
            ContextMenuAction::SplitDown => self.new_pane(SplitDirection::Down)?,
            ContextMenuAction::ClearPane => self.rpc(&Request::ClearPane { pane: Some(pane) })?,
        }
        self.close_context_menu();
        Ok(())
    }

    fn jump_selected_notification(&mut self) -> Result<()> {
        let Some(snapshot) = &self.snapshot else {
            return Ok(());
        };
        let Some(note) = snapshot
            .notifications
            .iter()
            .rev()
            .take(20)
            .nth(self.notification_selected)
        else {
            return Ok(());
        };
        let Some(workspace_id) = notification_workspace(snapshot, note).map(|item| item.id.clone())
        else {
            return Ok(());
        };
        self.rpc(&Request::SwitchWorkspace {
            workspace: workspace_id,
        })?;
        if let Some(pane) = note.pane.clone() {
            self.rpc(&Request::FocusPane { pane })?;
        }
        self.mode = UiMode::Panes;
        Ok(())
    }

    fn jump_notification(&self) -> Result<()> {
        self.rpc(&Request::JumpNotification)
    }

    fn resize(&self, direction: SplitDirection) -> Result<()> {
        self.rpc(&Request::Resize {
            direction,
            amount: 5,
        })
    }

    fn toggle_zoom(&self) -> Result<()> {
        self.rpc(&Request::ToggleZoom { pane: None })
    }

    fn focus_direction(&self, direction: SplitDirection) -> Result<()> {
        self.rpc(&Request::FocusDirection { direction })
    }

    fn switch_relative(&self, delta: isize) -> Result<()> {
        let Some(snapshot) = &self.snapshot else {
            return Ok(());
        };
        if snapshot.workspaces.is_empty() {
            return Ok(());
        }
        let index = snapshot
            .workspaces
            .iter()
            .position(|workspace| workspace.id == snapshot.active_workspace)
            .unwrap_or(0);
        let next = (index as isize + delta).rem_euclid(snapshot.workspaces.len() as isize) as usize;
        self.rpc(&Request::SwitchWorkspace {
            workspace: snapshot.workspaces[next].id.clone(),
        })
    }

    fn active_pane(&self) -> Option<String> {
        let snapshot = self.snapshot.as_ref()?;
        snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.id == snapshot.active_workspace)
            .and_then(|workspace| workspace.active_pane.clone())
    }

    fn scroll_active(&mut self, delta: isize) {
        if let Some(pane) = self.active_pane() {
            adjust_scroll(&mut self.scroll_offsets, &pane, delta);
        }
    }

    fn reset_active_scroll(&mut self) {
        if let Some(pane) = self.active_pane() {
            self.scroll_offsets.remove(&pane);
        }
    }

    /// Keeps the sidebar scroll offset within bounds and auto-scrolls to reveal
    /// the active workspace, but only when the active workspace actually changed
    /// so wheel scrolling elsewhere is not immediately undone.
    fn update_sidebar_scroll(&mut self) {
        let Some(snapshot) = self.snapshot.as_ref() else {
            return;
        };
        let count = snapshot.workspaces.len();
        let list_height = terminal_size()
            .map(|(_, h)| h)
            .unwrap_or(0)
            .saturating_sub(1);
        let active = snapshot
            .workspaces
            .iter()
            .position(|workspace| workspace.id == snapshot.active_workspace)
            .unwrap_or(0);
        let active_id = snapshot
            .workspaces
            .get(active)
            .map(|workspace| workspace.id.clone());
        if self.sidebar_active_seen != active_id {
            self.sidebar_active_seen = active_id;
            self.sidebar_scroll = scroll_to_reveal(
                count,
                active,
                self.sidebar_scroll,
                list_height,
                self.sidebar_collapsed,
            );
        }
        self.sidebar_scroll = self.sidebar_scroll.min(max_sidebar_offset(
            count,
            list_height,
            self.sidebar_collapsed,
        ));
    }

    /// Scrolls the sidebar by wheel input. Returns `true` if the pointer was over
    /// the sidebar (and thus the wheel event was consumed).
    fn scroll_sidebar(&mut self, column: u16, delta: isize) -> bool {
        if column >= sidebar_width(self.sidebar_collapsed, self.sidebar_width) {
            return false;
        }
        let next = if delta.is_negative() {
            self.sidebar_scroll.saturating_sub(delta.unsigned_abs())
        } else {
            self.sidebar_scroll.saturating_add(delta as usize)
        };
        if let Some(snapshot) = self.snapshot.as_ref() {
            let count = snapshot.workspaces.len();
            let list_height = terminal_size()
                .map(|(_, h)| h)
                .unwrap_or(0)
                .saturating_sub(1);
            self.sidebar_scroll = next.min(max_sidebar_offset(
                count,
                list_height,
                self.sidebar_collapsed,
            ));
        } else {
            self.sidebar_scroll = next;
        }
        true
    }

    fn handle_click(&mut self, column: u16, row: u16) -> Result<bool> {
        self.update_hover(column, row)?;
        let (width, height) = terminal_size()?;
        let layout_cols = self.layout_sidebar_cols();
        // Start sidebar resize when grabbing the right edge (expanded only).
        let cols = layout_cols;
        if !self.sidebar_collapsed
            && layout_cols > 0
            && is_sidebar_resize_edge(column, cols)
            && row < height.saturating_sub(CONTROL_BAR_HEIGHT)
        {
            self.sidebar_resize_drag = true;
            self.sidebar_drag_workspace = None;
            self.pane_resize_drag = None;
            return Ok(false);
        }
        // ☰ workspace picker: click a row to switch.
        if self.mode == UiMode::WorkspacePicker {
            let main = confirm_main_area(self.sidebar_collapsed, self.sidebar_width, width, height);
            // List starts one row below the border title.
            if column > main.x
                && column < main.x.saturating_add(main.width)
                && row > main.y
                && row < main.y.saturating_add(main.height.saturating_sub(1))
            {
                let index = row.saturating_sub(main.y.saturating_add(1)) as usize;
                if let Some(snapshot) = self.snapshot.as_ref() {
                    if index < snapshot.workspaces.len() {
                        self.workspace_picker_selected = index;
                        self.activate_workspace_picker_selection()?;
                        return Ok(false);
                    }
                }
            }
            return Ok(false);
        }
        // Click the collapsed ☰ header to open the workspace picker.
        if layout_cols > 0 && self.sidebar_collapsed && column < layout_cols && row == 0 {
            self.toggle_workspace_picker();
            return Ok(false);
        }
        // Rename dialog: OK / Cancel / ignore outside.
        if self.rename_dialog.is_some() {
            let main = confirm_main_area(self.sidebar_collapsed, self.sidebar_width, width, height);
            if let Some((ok, cancel)) = rename_button_rects(main) {
                if point_in_rect(ok, column, row) {
                    self.submit_rename()?;
                    return Ok(false);
                }
                if point_in_rect(cancel, column, row) {
                    self.cancel_rename();
                    return Ok(false);
                }
            }
            return Ok(false);
        }
        // Modal alert captures clicks on Yes / Cancel (and swallows the rest).
        if self.pending_confirm.is_some() {
            let main = confirm_main_area(self.sidebar_collapsed, self.sidebar_width, width, height);
            if let Some((yes, no)) = confirm_button_rects(main) {
                if point_in_rect(yes, column, row) {
                    self.confirm_pending()?;
                    return Ok(false);
                }
                if point_in_rect(no, column, row) {
                    self.cancel_confirm();
                    return Ok(false);
                }
            }
            // Click outside buttons: ignore (keep dialog open).
            return Ok(false);
        }
        if let Some(action) = control_action_at(
            self.sidebar_collapsed,
            self.sidebar_width,
            width,
            height,
            column,
            row,
        ) {
            self.sidebar_drag_workspace = None;
            self.pane_resize_drag = None;
            self.last_click = None;
            return self.run_control_action(action);
        }
        // Overlays (settings/commands/picker/…) own the main area — do not
        // let clicks fall through to hidden pane × buttons (improve.md #19).
        if !matches!(self.mode, UiMode::Panes) {
            return Ok(false);
        }
        if self.mode == UiMode::Panes && self.active_workspace_is_empty() {
            let area = pane_area(self.sidebar_collapsed, self.sidebar_width, width, height);
            if let Some(rect) = empty_create_pane_rect(area) {
                if point_in_rect(rect, column, row) {
                    self.new_pane(SplitDirection::Right)?;
                    return Ok(false);
                }
            }
        }
        if self.mode == UiMode::ContextMenu {
            if self.handle_context_click(row)? {
                return Ok(false);
            }
            self.close_context_menu();
        }
        let Some(snapshot) = &self.snapshot else {
            return Ok(false);
        };
        // Double-click rename targets (workspace / tab / pane title).
        if let Some((target, current)) = rename_target_at(
            snapshot,
            self.sidebar_scroll,
            self.sidebar_collapsed,
            self.sidebar_width,
            width,
            height,
            column,
            row,
        ) {
            if self.is_double_click(column, row, &target) {
                self.open_rename(target, current);
                return Ok(false);
            }
            self.last_click = Some(ClickStamp {
                at: Instant::now(),
                column,
                row,
                target,
            });
        } else {
            self.last_click = None;
        }
        if self.forward_mouse_to_pane(column, row, MouseButtonCode::Left, true)? {
            return Ok(false);
        }
        if let Some(hit) = pane_control_at(
            snapshot,
            self.sidebar_collapsed,
            self.sidebar_width,
            width,
            height,
            column,
            row,
        ) {
            self.sidebar_drag_workspace = None;
            self.pane_resize_drag = None;
            self.run_pane_control(hit)?;
            return Ok(false);
        }
        match primary_mouse_action(
            snapshot,
            self.sidebar_scroll,
            self.sidebar_collapsed,
            self.sidebar_width,
            width,
            height,
            column,
            row,
            self.status_markers.as_str(),
            self.tab_close_button,
        ) {
            PrimaryMouseAction::SwitchWorkspace(workspace) => {
                self.sidebar_drag_workspace = Some(workspace.clone());
                self.rpc(&Request::SwitchWorkspace { workspace })?;
            }
            PrimaryMouseAction::StartResize(drag) => {
                self.sidebar_drag_workspace = None;
                self.pane_resize_drag = Some(drag);
            }
            PrimaryMouseAction::FocusWorkspaceTab { tab } => {
                self.sidebar_drag_workspace = None;
                self.pane_resize_drag = None;
                self.rpc(&Request::SwitchTab {
                    workspace: None,
                    tab,
                })?;
            }
            PrimaryMouseAction::CloseWorkspaceTab { tab } => {
                self.sidebar_drag_workspace = None;
                self.pane_resize_drag = None;
                self.request_close_workspace_tab(tab)?;
            }
            PrimaryMouseAction::NewWorkspaceTab => {
                self.sidebar_drag_workspace = None;
                self.pane_resize_drag = None;
                self.new_workspace_tab()?;
            }
            PrimaryMouseAction::FocusPane(pane) => {
                self.sidebar_drag_workspace = None;
                self.pane_resize_drag = None;
                self.start_selection(&pane, column, row)?;
                self.rpc(&Request::FocusPane { pane })?;
            }
            PrimaryMouseAction::None => {
                self.sidebar_drag_workspace = None;
                self.pane_resize_drag = None;
            }
        }
        Ok(false)
    }

    fn update_hover(&mut self, column: u16, row: u16) -> Result<()> {
        let (width, height) = terminal_size()?;
        self.hover_control = control_action_at(
            self.sidebar_collapsed,
            self.sidebar_width,
            width,
            height,
            column,
            row,
        );
        let sidebar_scroll = self.sidebar_scroll;
        let list_height = height.saturating_sub(1);
        self.hover_workspace = self.snapshot.as_ref().and_then(|snapshot| {
            (column < sidebar_width(self.sidebar_collapsed, self.sidebar_width))
                .then(|| {
                    workspace_at_sidebar_row(
                        snapshot,
                        sidebar_scroll,
                        list_height,
                        self.sidebar_collapsed,
                        row,
                    )
                    .map(|workspace| workspace.id.clone())
                })
                .flatten()
        });
        self.hover_pane_control = self.snapshot.as_ref().and_then(|snapshot| {
            pane_control_at(
                snapshot,
                self.sidebar_collapsed,
                self.sidebar_width,
                width,
                height,
                column,
                row,
            )
        });
        self.hover_empty_create = self.mode == UiMode::Panes
            && self.active_workspace_is_empty()
            && empty_create_pane_rect(pane_area(
                self.sidebar_collapsed,
                self.sidebar_width,
                width,
                height,
            ))
            .map(|rect| point_in_rect(rect, column, row))
            .unwrap_or(false);
        Ok(())
    }

    fn handle_drag(&mut self, column: u16, row: u16) -> Result<()> {
        if self.sidebar_drag_workspace.is_some() {
            return self.handle_sidebar_drag(column, row);
        }
        if self.forward_mouse_to_pane(column, row, MouseButtonCode::LeftDrag, true)? {
            return Ok(());
        }
        if let Some(selection) = self.selection.as_mut() {
            selection.end_col = column;
            selection.end_row = row;
        }
        Ok(())
    }

    fn handle_sidebar_drag(&mut self, column: u16, row: u16) -> Result<()> {
        let Some(workspace_id) = self.sidebar_drag_workspace.clone() else {
            return Ok(());
        };
        let sidebar_scroll = self.sidebar_scroll;
        let (_, height) = terminal_size()?;
        let list_height = height.saturating_sub(1);
        let Some(snapshot) = &self.snapshot else {
            return Ok(());
        };
        if column >= sidebar_width(self.sidebar_collapsed, self.sidebar_width) {
            return Ok(());
        }
        let Some(position) = sidebar_position_from_row(
            snapshot,
            sidebar_scroll,
            list_height,
            self.sidebar_collapsed,
            row,
        ) else {
            return Ok(());
        };
        let current = snapshot
            .workspaces
            .iter()
            .position(|workspace| workspace.id == workspace_id)
            .map(|index| index + 1);
        if current == Some(position) {
            return Ok(());
        }
        self.rpc(&Request::MoveWorkspace {
            workspace: workspace_id,
            position,
        })
    }

    fn handle_pane_resize_drag(&mut self, column: u16, row: u16) -> Result<()> {
        let Some(mut drag) = self.pane_resize_drag else {
            return Ok(());
        };
        let direction = resize_drag_direction(drag.axis, drag.column, drag.row, column, row);
        let amount = match drag.axis {
            SplitAxis::Horizontal => column.abs_diff(drag.column),
            SplitAxis::Vertical => row.abs_diff(drag.row),
        };
        if let Some(direction) = direction {
            self.rpc(&Request::Resize { direction, amount })?;
            drag.column = column;
            drag.row = row;
            self.pane_resize_drag = Some(drag);
        }
        Ok(())
    }

    fn handle_right_click(&mut self, column: u16, row: u16) -> Result<()> {
        let sidebar_scroll = self.sidebar_scroll;
        let (_, height) = terminal_size()?;
        let list_height = height.saturating_sub(1);
        let Some(snapshot) = &self.snapshot else {
            return Ok(());
        };
        let sidebar_width = sidebar_width(self.sidebar_collapsed, self.sidebar_width);
        if column < sidebar_width {
            if let Some(workspace) = workspace_at_sidebar_row(
                snapshot,
                sidebar_scroll,
                list_height,
                self.sidebar_collapsed,
                row,
            ) {
                self.rpc(&Request::SetWorkspacePinned {
                    workspace: workspace.id.clone(),
                    pinned: !workspace.pinned,
                })?;
            }
            return Ok(());
        }
        if let Some(pane) = self.pane_at_position(column, row)? {
            self.rpc(&Request::FocusPane { pane: pane.clone() })?;
            self.context_pane = Some(pane);
            self.context_selected = 0;
            self.mode = UiMode::ContextMenu;
        }
        Ok(())
    }

    fn handle_context_click(&mut self, row: u16) -> Result<bool> {
        if row == 0 {
            return Ok(false);
        }
        let index = usize::from(row.saturating_sub(1));
        if index >= context_menu_entries().len() {
            return Ok(false);
        }
        self.context_selected = index;
        self.run_selected_context_item()?;
        Ok(true)
    }

    fn scroll_at(&mut self, column: u16, row: u16, delta: isize) -> Result<()> {
        if let Some(pane) = self.pane_at_position(column, row)? {
            adjust_scroll(&mut self.scroll_offsets, &pane, delta);
        }
        Ok(())
    }

    fn start_selection(&mut self, pane: &str, column: u16, row: u16) -> Result<()> {
        let Some((_, area)) = self.pane_area_at_position(column, row)? else {
            return Ok(());
        };
        let has_strip = self
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.panes.get(pane))
            .map(pane_has_tab_strip)
            .unwrap_or(false);
        if !point_in_rect(pane_content_area(area, has_strip), column, row) {
            self.selection = None;
            return Ok(());
        }
        self.selection = Some(TextSelection {
            pane: pane.to_string(),
            start_col: column,
            start_row: row,
            end_col: column,
            end_row: row,
        });
        Ok(())
    }

    fn finish_selection(&mut self) -> Result<()> {
        let Some(selection) = self.selection.clone() else {
            return Ok(());
        };
        if selection.start_col == selection.end_col && selection.start_row == selection.end_row {
            self.selection = None;
            return Ok(());
        }
        let Some(text) = self.selected_text(&selection) else {
            self.selection = None;
            return Ok(());
        };
        if text.trim().is_empty() {
            self.selection = None;
            return Ok(());
        }
        let result = self.rpc(&Request::SetClipboard {
            text,
            source_pane: Some(selection.pane),
            source: "selection".to_string(),
        });
        if result.is_ok() {
            self.selection = None;
        } else if let Err(err) = result {
            self.selection = None;
            self.set_action_error(format!("selection copy failed: {err:#}"));
        }
        Ok(())
    }

    fn selected_text(&self, selection: &TextSelection) -> Option<String> {
        let snapshot = self.snapshot.as_ref()?;
        let pane = snapshot.panes.get(&selection.pane)?;
        let area = self.pane_area_by_id(&selection.pane).ok().flatten()?;
        selected_text_from_pane(
            pane,
            pane_content_area(area, pane_has_tab_strip(pane)),
            selection,
        )
    }

    fn forward_mouse_to_pane(
        &self,
        column: u16,
        row: u16,
        button: MouseButtonCode,
        pressed: bool,
    ) -> Result<bool> {
        let Some((pane_id, area)) = self.pane_area_at_position(column, row)? else {
            return Ok(false);
        };
        let Some(pane) = self
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.panes.get(&pane_id))
        else {
            return Ok(false);
        };
        let content = pane_content_area(area, pane_has_tab_strip(pane));
        if !point_in_rect(content, column, row) {
            return Ok(false);
        }
        if pane.mouse_protocol_mode.is_empty() {
            return Ok(false);
        }
        let x = column.saturating_sub(content.x).saturating_add(1);
        let y = row.saturating_sub(content.y).saturating_add(1);
        let data = sgr_mouse_sequence(button, x, y, pressed);
        self.rpc(&Request::Input {
            pane: Some(pane_id),
            data,
        })?;
        Ok(true)
    }

    fn pane_at_position(&self, column: u16, row: u16) -> Result<Option<String>> {
        Ok(self
            .pane_area_at_position(column, row)?
            .map(|(pane, _)| pane))
    }

    fn pane_area_at_position(&self, column: u16, row: u16) -> Result<Option<(String, Rect)>> {
        let sidebar_width = sidebar_width(self.sidebar_collapsed, self.sidebar_width);
        if column < sidebar_width {
            return Ok(None);
        }
        let Some(snapshot) = &self.snapshot else {
            return Ok(None);
        };
        let active = snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.id == snapshot.active_workspace);
        let Some(active) = active else {
            return Ok(None);
        };
        if active.panes.is_empty() {
            return Ok(None);
        }
        if let Some(pane) = active
            .zoomed_pane
            .as_ref()
            .filter(|pane| active.panes.iter().any(|item| item == *pane))
        {
            let (width, height) = terminal_size()?;
            return Ok(Some((
                pane.clone(),
                pane_area(self.sidebar_collapsed, self.sidebar_width, width, height),
            )));
        }
        let (width, height) = terminal_size()?;
        let area = pane_area(self.sidebar_collapsed, self.sidebar_width, width, height);
        Ok(pane_area_at(active.layout.as_ref(), area, column, row))
    }

    fn pane_area_by_id(&self, pane_id: &str) -> Result<Option<Rect>> {
        let Some(snapshot) = &self.snapshot else {
            return Ok(None);
        };
        let Some(active) = snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.id == snapshot.active_workspace)
        else {
            return Ok(None);
        };
        let (width, height) = terminal_size()?;
        let area = pane_area(self.sidebar_collapsed, self.sidebar_width, width, height);
        if active.zoomed_pane.as_deref() == Some(pane_id) {
            return Ok(Some(area));
        }
        Ok(pane_area_by_id(active.layout.as_ref(), area, pane_id))
    }
}

fn draw(
    frame: &mut ratatui::Frame,
    snapshot: Option<&Session>,
    pane_sizes: &mut BTreeMap<String, PaneSize>,
    scroll_offsets: &BTreeMap<String, usize>,
    mode: UiMode,
    sidebar_collapsed: bool,
    sidebar_expanded: u16,
    sidebar_scroll: usize,
    notification_selected: usize,
    actions: &[UiAction],
    action_selected: usize,
    action_error: Option<&str>,
    command_selected: usize,
    command_filter: &str,
    prefix_label: &str,
    settings_selected: usize,
    context_selected: usize,
    context_pane: Option<&str>,
    hover_control: Option<ControlAction>,
    hover_pane_control: Option<&PaneControlHit>,
    hover_workspace: Option<&str>,
    hover_empty_create: bool,
    confirm: Option<&PendingConfirm>,
    rename: Option<&RenameDialog>,
    selection: Option<&TextSelection>,
    theme: UiTheme,
    workspace_second_line: UiWorkspaceSecondLine,
    cursor_blink_on: bool,
    status_markers: &str,
    tab_close_button: bool,
    scroll_step: usize,
    cursor_blink: bool,
    cursor_blink_ms: u64,
    default_shell: &str,
    default_cwd: &str,
    mouse: bool,
    bell_on_attention: bool,
    mobile_relay_enabled: bool,
    mobile_relay_bind: &str,
    mobile_relay_port: u16,
    mobile_relay_allow_localhost: bool,
    mobile_relay_allow_cgnat: bool,
    sidebar_responsive: bool,
    workspace_picker_selected: usize,
) {
    let palette = theme.palette();
    let term_w = frame.size().width;
    let compact = sidebar_responsive && term_w < COMPACT_TERM_WIDTH;
    // Mobile / narrow: fully hide the rail; ☰ menu picks workspaces instead.
    let sidebar_width = if compact {
        0
    } else {
        sidebar_width(sidebar_collapsed, sidebar_expanded)
    };
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(sidebar_width), Constraint::Min(10)])
        .split(frame.size());

    let Some(snapshot) = snapshot else {
        frame.render_widget(Paragraph::new("connecting to vmux..."), frame.size());
        return;
    };

    if sidebar_width > 0 {
    let sidebar_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(chunks[0]);
    // Collapsed rail: show burger glyph so click-to-open is discoverable.
    let sidebar_title = if sidebar_collapsed {
        " ☰ "
    } else {
        " vmux "
    };
    frame.render_widget(
        Paragraph::new(sidebar_title).style(
            Style::default()
                .fg(palette.active)
                .bg(palette.background)
                .add_modifier(Modifier::BOLD),
        ),
        sidebar_chunks[0],
    );
    // Inner width available for labels (Borders::RIGHT steals one column).
    let sidebar_label_width = usize::from(sidebar_chunks[1].width.saturating_sub(1)).max(1);
    let workspace_items: Vec<ListItem> = sidebar_entries(
        snapshot.workspaces.len(),
        sidebar_scroll,
        sidebar_chunks[1].height,
        sidebar_collapsed,
    )
    .into_iter()
    .map(|entry| match entry {
        SidebarEntry::Above(hidden) => ListItem::new(Line::from(Span::styled(
            format!("  ▲ +{hidden}"),
            Style::default().fg(palette.muted).bg(palette.background),
        ))),
        SidebarEntry::Below(hidden) => ListItem::new(Line::from(Span::styled(
            format!("  ▼ +{hidden}"),
            Style::default().fg(palette.muted).bg(palette.background),
        ))),
        SidebarEntry::Workspace(index) => workspace_list_item(
            snapshot,
            index,
            sidebar_collapsed,
            workspace_second_line,
            hover_workspace,
            sidebar_label_width,
            palette,
            status_markers,
        ),
    })
    .collect();
    let sidebar = List::new(workspace_items)
        .block(
            Block::default()
                .borders(Borders::RIGHT)
                .border_style(Style::default().fg(palette.border)),
        )
        .style(Style::default().fg(palette.text).bg(palette.background));
    frame.render_widget(sidebar, sidebar_chunks[1]);
    } // end sidebar_width > 0

    let main_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(chunks[1]);

    let highlight_pane = confirm.and_then(|c| c.highlight_pane());
    match mode {
        UiMode::Panes => {
            let active = snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == snapshot.active_workspace);
            let pane_chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(TAB_BAR_HEIGHT), Constraint::Min(1)])
                .split(main_chunks[0]);
            if let Some(active) = active {
                draw_workspace_tab_bar(
                    frame,
                    pane_chunks[0],
                    snapshot,
                    active,
                    palette,
                    status_markers,
                    tab_close_button,
                );
                // Host caret follows the active pane PTY cursor (skipped under modals).
                let mut host_cursor: Option<(u16, u16)> = None;
                let host_cursor_slot = if confirm.is_none() && rename.is_none() {
                    Some(&mut host_cursor)
                } else {
                    None
                };
                if active.panes.is_empty() {
                    draw_empty_workspace(
                        frame,
                        pane_chunks[1],
                        theme,
                        prefix_label,
                        hover_empty_create,
                    );
                } else if let Some(zoomed) = active
                    .zoomed_pane
                    .as_deref()
                    .filter(|pane| active.panes.iter().any(|item| item == *pane))
                {
                    draw_single_pane(
                        frame,
                        pane_chunks[1],
                        snapshot,
                        zoomed,
                        Some(zoomed),
                        pane_sizes,
                        scroll_offsets,
                        hover_pane_control,
                        selection,
                        highlight_pane,
                        theme,
                        host_cursor_slot,
                        cursor_blink_on,
                    );
                } else {
                    draw_panes(
                        frame,
                        pane_chunks[1],
                        snapshot,
                        active.layout.as_ref(),
                        active.panes.as_slice(),
                        active.active_pane.as_deref(),
                        pane_sizes,
                        scroll_offsets,
                        hover_pane_control,
                        selection,
                        highlight_pane,
                        theme,
                        host_cursor_slot,
                        cursor_blink_on,
                    );
                }
                if let Some((x, y)) = host_cursor {
                    frame.set_cursor(x, y);
                }
            }
        }
        UiMode::Notifications => draw_notifications(
            frame,
            main_chunks[0],
            snapshot,
            notification_selected,
            palette,
        ),
        UiMode::Actions => draw_actions(
            frame,
            main_chunks[0],
            actions,
            action_selected,
            action_error,
            palette,
        ),
        UiMode::Commands => draw_commands(
            frame,
            main_chunks[0],
            command_selected,
            command_filter,
            prefix_label,
            theme,
        ),
        UiMode::Settings => draw_settings(
            frame,
            main_chunks[0],
            SettingsView {
                theme,
                workspace_second_line,
                sidebar_collapsed,
                sidebar_responsive,
                sidebar_width: sidebar_expanded,
                prefix_label,
                scroll_step,
                cursor_blink,
                cursor_blink_ms,
                status_markers,
                default_shell,
                default_cwd,
                mouse,
                tab_close_button,
                bell_on_attention,
                mobile_relay_enabled,
                mobile_relay_bind,
                mobile_relay_port,
                mobile_relay_allow_localhost,
                mobile_relay_allow_cgnat,
                selected: settings_selected,
            },
        ),
        UiMode::WorkspacePicker => draw_workspace_picker(
            frame,
            main_chunks[0],
            snapshot,
            workspace_picker_selected,
            theme,
            status_markers,
            compact,
        ),
        UiMode::ContextMenu => draw_context_menu(
            frame,
            main_chunks[0],
            context_selected,
            context_pane,
            palette,
        ),
    }
    // Modal alerts on top of the current view (panes stay visible underneath).
    if confirm.is_some() {
        draw_confirm_overlay(frame, main_chunks[0], confirm, theme);
    }
    if let Some(dialog) = rename {
        draw_rename_overlay(frame, main_chunks[0], dialog, theme);
    }
    draw_control_bar(frame, main_chunks[1], mode, confirm, hover_control, theme);
    frame.render_widget(
        Paragraph::new(session_footer_with_modals(
            snapshot,
            mode,
            notification_selected,
            confirm,
            rename,
        ))
        .style(Style::default().fg(palette.muted).bg(palette.background)),
        main_chunks[2],
    );
}

/// Shared block for every overlay panel so borders, title emphasis, and panel
/// background read identically across notifications, actions, agents, commands,
/// settings, and the context menu. `title` should include its own surrounding
/// spaces (e.g. `" commands "`).
fn panel_block(title: &str, palette: ThemePalette) -> Block<'static> {
    Block::default()
        .title(Span::styled(
            title.to_string(),
            Style::default()
                .fg(palette.active)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(palette.active))
        .style(Style::default().fg(palette.text).bg(palette.surface))
}

/// The single highlight style used for the selected row in every overlay panel
/// (and hovered sidebar rows), so keyboard selection looks the same everywhere.
fn selected_row_style(palette: ThemePalette) -> Style {
    Style::default()
        .fg(palette.on_bright)
        .bg(palette.hover)
        .add_modifier(Modifier::BOLD)
}

/// Renders a simple text-line overlay panel with a consistent block, panel
/// background, and shared selected-row highlight. `selected` is the row index to
/// highlight (or `None` when nothing is selectable, e.g. an error line).
fn draw_list_panel(
    frame: &mut ratatui::Frame,
    area: Rect,
    title: &str,
    lines: Vec<String>,
    selected: Option<usize>,
    empty: &str,
    palette: ThemePalette,
) {
    let block = panel_block(title, palette);
    if lines.is_empty() {
        frame.render_widget(
            Paragraph::new(empty.to_string())
                .block(block)
                .style(Style::default().fg(palette.muted).bg(palette.surface))
                .wrap(Wrap { trim: false }),
            area,
        );
        return;
    }
    let styled: Vec<Line> = lines
        .into_iter()
        .enumerate()
        .map(|(index, text)| {
            let style = if selected == Some(index) {
                selected_row_style(palette)
            } else {
                Style::default().fg(palette.text)
            };
            Line::from(Span::styled(text, style))
        })
        .collect();
    frame.render_widget(
        Paragraph::new(styled)
            .block(block)
            .style(Style::default().bg(palette.surface))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_notifications(
    frame: &mut ratatui::Frame,
    area: Rect,
    snapshot: &Session,
    selected: usize,
    palette: ThemePalette,
) {
    let lines = notification_panel_lines(snapshot, selected);
    draw_list_panel(
        frame,
        area,
        " notifications ",
        lines,
        Some(selected),
        "No notifications",
        palette,
    );
}

fn draw_actions(
    frame: &mut ratatui::Frame,
    area: Rect,
    actions: &[UiAction],
    selected: usize,
    error: Option<&str>,
    palette: ThemePalette,
) {
    let lines = action_panel_lines(actions, selected, error);
    // An error occupies the only row and is not a selectable action.
    let selected = if error.is_some() {
        None
    } else {
        Some(selected)
    };
    draw_list_panel(
        frame,
        area,
        " actions ",
        lines,
        selected,
        "No actions found",
        palette,
    );
}

fn draw_commands(
    frame: &mut ratatui::Frame,
    area: Rect,
    selected: usize,
    filter: &str,
    prefix_label: &str,
    theme: UiTheme,
) {
    let palette = theme.palette();
    let inner_width = area.width.saturating_sub(2);
    let entries = filter_command_entries(filter);
    let mut lines = Vec::new();
    // Search box showing what has been typed so far.
    lines.push(Line::from(vec![
        Span::styled("› ", Style::default().fg(palette.active)),
        Span::styled(filter.to_string(), Style::default().fg(palette.text)),
        Span::styled("▏", Style::default().fg(palette.muted)),
    ]));
    lines.extend(command_palette_lines(
        &entries,
        selected,
        prefix_label,
        inner_width,
        theme,
    ));
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel_block(" commands ", palette))
            .style(Style::default().bg(palette.surface))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_settings(frame: &mut ratatui::Frame, area: Rect, view: SettingsView<'_>) {
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

/// Full-screen (main area) workspace list — mobile-style ☰ menu.
fn draw_workspace_picker(
    frame: &mut ratatui::Frame,
    area: Rect,
    snapshot: &Session,
    selected: usize,
    theme: UiTheme,
    status_markers: &str,
    compact: bool,
) {
    let palette = theme.palette();
    let title = if compact {
        " ☰ workspaces · j/k · Enter · Esc "
    } else {
        " ☰ workspaces "
    };
    let mut lines: Vec<Line<'static>> = Vec::new();
    if snapshot.workspaces.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no workspaces)",
            Style::default().fg(palette.muted),
        )));
    }
    for (index, ws) in snapshot.workspaces.iter().enumerate() {
        let active = index == selected;
        let is_current = ws.id == snapshot.active_workspace;
        let marker = if is_current { "●" } else { "○" };
        let pin = if ws.pinned { "📌 " } else { "" };
        let status = workspace_status_summary(snapshot, &ws.id, status_markers);
        let label = format!(
            " {marker} {pin}{}  {}",
            ws.name,
            if status.is_empty() {
                String::new()
            } else {
                format!(" {status}")
            }
        );
        let style = if active {
            selected_row_style(palette)
        } else if is_current {
            Style::default()
                .fg(palette.active)
                .bg(palette.surface)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(palette.text).bg(palette.surface)
        };
        lines.push(Line::from(Span::styled(label, style)));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  ↑/↓ select · Enter open · Esc close · ☰ menu toggles",
        Style::default().fg(palette.muted),
    )));
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel_block(title, palette))
            .style(Style::default().fg(palette.text).bg(palette.surface)),
        area,
    );
}

fn workspace_status_summary(snapshot: &Session, workspace_id: &str, markers: &str) -> String {
    // Reuse sidebar agent tally for the workspace when possible.
    let Some(ws) = snapshot.workspaces.iter().find(|w| w.id == workspace_id) else {
        return String::new();
    };
    let mut busy = 0usize;
    let mut attention = 0usize;
    let mut done = 0usize;
    for pane_id in &ws.panes {
        let Some(pane) = snapshot.panes.get(pane_id) else {
            continue;
        };
        match status_label(&pane.agent_status) {
            "busy" => busy += 1,
            "attention" => attention += 1,
            "done" => done += 1,
            _ if pane.notification_message.is_some() => attention += 1,
            _ => {}
        }
    }
    let ascii = markers.eq_ignore_ascii_case("ascii");
    if attention > 0 {
        if ascii {
            format!("?{attention}")
        } else {
            format!("🙋{attention}")
        }
    } else if busy > 0 {
        if ascii {
            format!("*{busy}")
        } else {
            format!("🔄{busy}")
        }
    } else if done > 0 {
        if ascii {
            format!("+{done}")
        } else {
            format!("✅{done}")
        }
    } else {
        String::new()
    }
}

fn draw_context_menu(
    frame: &mut ratatui::Frame,
    area: Rect,
    selected: usize,
    pane: Option<&str>,
    palette: ThemePalette,
) {
    let lines = context_menu_lines(selected, pane);
    draw_list_panel(
        frame,
        area,
        " pane menu ",
        lines,
        Some(selected),
        "No pane actions",
        palette,
    );
}

const CONFIRM_BOX_WIDTH: u16 = 50;
const CONFIRM_BOX_HEIGHT: u16 = 10;

/// Solid dialog surface (opaque so underlying pane chrome cannot bleed through).
fn confirm_dialog_bg(theme: UiTheme) -> Color {
    match theme {
        UiTheme::Midnight => Color::Rgb(22, 24, 32),
        UiTheme::Daylight => Color::Rgb(250, 250, 252),
        UiTheme::Contrast => Color::Black,
    }
}

/// Centered modal alert over the main content area.
fn confirm_dialog_rect(area: Rect) -> Option<Rect> {
    if area.width < 30 || area.height < CONFIRM_BOX_HEIGHT {
        return None;
    }
    let width = CONFIRM_BOX_WIDTH.min(area.width.saturating_sub(2));
    let height = CONFIRM_BOX_HEIGHT.min(area.height);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Some(Rect::new(x, y, width, height))
}

/// Main content area used for the confirm overlay (matches draw() layout).
fn confirm_main_area(
    sidebar_collapsed: bool,
    sidebar_expanded: u16,
    terminal_width: u16,
    terminal_height: u16,
) -> Rect {
    let sw = sidebar_width(sidebar_collapsed, sidebar_expanded);
    Rect::new(
        sw,
        0,
        terminal_width.saturating_sub(sw),
        terminal_height.saturating_sub(CONTROL_BAR_HEIGHT),
    )
}

/// Clickable Yes / Cancel button rects inside the confirm dialog.
fn confirm_button_rects(area: Rect) -> Option<(Rect, Rect)> {
    let box_rect = confirm_dialog_rect(area)?;
    // Buttons sit on the row above the bottom border.
    let row = box_rect.y.saturating_add(box_rect.height.saturating_sub(3));
    let yes_w = 11_u16; // " [Y] Yes  "
    let no_w = 14_u16; // " [N] Cancel "
    let gap = 3_u16;
    let total = yes_w + gap + no_w;
    let start = box_rect.x + box_rect.width.saturating_sub(total).saturating_div(2);
    let yes = Rect::new(start, row, yes_w, 1);
    let no = Rect::new(
        start.saturating_add(yes_w).saturating_add(gap),
        row,
        no_w,
        1,
    );
    Some((yes, no))
}

const RENAME_BOX_WIDTH: u16 = 52;
const RENAME_BOX_HEIGHT: u16 = 9;

fn rename_dialog_rect(area: Rect) -> Option<Rect> {
    if area.width < 32 || area.height < RENAME_BOX_HEIGHT {
        return None;
    }
    let width = RENAME_BOX_WIDTH.min(area.width.saturating_sub(2));
    let height = RENAME_BOX_HEIGHT.min(area.height);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Some(Rect::new(x, y, width, height))
}

/// OK / Cancel button rects for the rename dialog.
fn rename_button_rects(area: Rect) -> Option<(Rect, Rect)> {
    let box_rect = rename_dialog_rect(area)?;
    let row = box_rect.y.saturating_add(box_rect.height.saturating_sub(3));
    let ok_w = 14_u16; // " [Enter] OK "
    let cancel_w = 15_u16; // " [Esc] Cancel "
    let gap = 2_u16;
    let total = ok_w + gap + cancel_w;
    let start = box_rect.x + box_rect.width.saturating_sub(total).saturating_div(2);
    let ok = Rect::new(start, row, ok_w, 1);
    let cancel = Rect::new(
        start.saturating_add(ok_w).saturating_add(gap),
        row,
        cancel_w,
        1,
    );
    Some((ok, cancel))
}

/// Opaque rename dialog with a text field and OK / Cancel.
fn draw_rename_overlay(
    frame: &mut ratatui::Frame,
    area: Rect,
    dialog: &RenameDialog,
    theme: UiTheme,
) {
    let palette = theme.palette();
    let bg = confirm_dialog_bg(theme);
    let Some(box_rect) = rename_dialog_rect(area) else {
        return;
    };
    let kind = dialog.target.kind_label();
    let title = format!(" rename {kind} ");

    frame.render_widget(Clear, box_rect);
    frame.render_widget(
        Block::default().style(Style::default().bg(bg).fg(palette.text)),
        box_rect,
    );
    let block = Block::default()
        .title(Span::styled(
            title,
            Style::default()
                .fg(palette.active)
                .bg(bg)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(
            Style::default()
                .fg(palette.active)
                .bg(bg)
                .add_modifier(Modifier::BOLD),
        )
        .style(Style::default().fg(palette.text).bg(bg));
    frame.render_widget(block, box_rect);

    // Hint line.
    let hint = Rect::new(
        box_rect.x.saturating_add(2),
        box_rect.y.saturating_add(1),
        box_rect.width.saturating_sub(4),
        1,
    );
    frame.render_widget(
        Paragraph::new(Span::styled(
            format!("Enter a new {kind} name for {}", dialog.target.id()),
            Style::default().fg(palette.muted).bg(bg),
        ))
        .alignment(Alignment::Center),
        hint,
    );

    // Input field with cursor.
    let field = Rect::new(
        box_rect.x.saturating_add(3),
        box_rect.y.saturating_add(3),
        box_rect.width.saturating_sub(6),
        1,
    );
    frame.render_widget(Clear, field);
    let mut field_text = dialog.draft.clone();
    field_text.push('▌');
    frame.render_widget(
        Paragraph::new(Span::styled(
            format!(" {field_text} "),
            Style::default()
                .fg(palette.text)
                .bg(palette.surface_alt)
                .add_modifier(Modifier::BOLD),
        )),
        field,
    );

    if let Some((ok, cancel)) = rename_button_rects(area) {
        frame.render_widget(Clear, ok);
        frame.render_widget(Clear, cancel);
        frame.render_widget(
            Paragraph::new(Span::styled(
                " [Enter] OK ",
                Style::default()
                    .fg(Color::White)
                    .bg(Color::Blue)
                    .add_modifier(Modifier::BOLD),
            ))
            .alignment(Alignment::Center),
            ok,
        );
        frame.render_widget(
            Paragraph::new(Span::styled(
                " [Esc] Cancel ",
                Style::default()
                    .fg(Color::White)
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            ))
            .alignment(Alignment::Center),
            cancel,
        );
    }
}

/// Fully opaque modal alert. Clears the box area first so pane borders / chrome
/// underneath never show through, then paints solid surface + Yes/Cancel.
fn draw_confirm_overlay(
    frame: &mut ratatui::Frame,
    area: Rect,
    confirm: Option<&PendingConfirm>,
    theme: UiTheme,
) {
    let palette = theme.palette();
    let bg = confirm_dialog_bg(theme);
    let Some(box_rect) = confirm_dialog_rect(area) else {
        return;
    };
    let (title, body) = match confirm {
        Some(pending) => (pending.title.as_str(), pending.body.clone()),
        None => (" ⚠ confirm ", "Nothing to confirm.".to_string()),
    };

    // 1) Erase everything under the dialog (critical for "above" look).
    frame.render_widget(Clear, box_rect);

    // 2) Solid fill so every cell has an opaque background.
    frame.render_widget(
        Block::default().style(Style::default().bg(bg).fg(palette.text)),
        box_rect,
    );

    // 3) Bordered frame with title.
    let block = Block::default()
        .title(Span::styled(
            title.to_string(),
            Style::default()
                .fg(palette.danger)
                .bg(bg)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(
            Style::default()
                .fg(palette.danger)
                .bg(bg)
                .add_modifier(Modifier::BOLD),
        )
        .style(Style::default().fg(palette.text).bg(bg));
    frame.render_widget(block, box_rect);

    // 4) Body text (centered), with solid bg on the paragraph so wrap lines stay opaque.
    let body_area = Rect::new(
        box_rect.x.saturating_add(2),
        box_rect.y.saturating_add(2),
        box_rect.width.saturating_sub(4),
        box_rect.height.saturating_sub(5),
    );
    let body_lines: Vec<Line> = body
        .lines()
        .map(|line| {
            Line::from(Span::styled(
                line.to_string(),
                Style::default().fg(palette.text).bg(bg),
            ))
        })
        .collect();
    frame.render_widget(
        Paragraph::new(body_lines)
            .style(Style::default().fg(palette.text).bg(bg))
            .alignment(Alignment::Center),
        body_area,
    );

    // 5) Buttons — high contrast so labels never disappear.
    if let Some((yes, no)) = confirm_button_rects(area) {
        // Clear button cells first so underlying chrome cannot peek through.
        frame.render_widget(Clear, yes);
        frame.render_widget(Clear, no);
        frame.render_widget(
            Paragraph::new(Span::styled(
                " [Y] Yes  ",
                Style::default()
                    .fg(Color::White)
                    .bg(Color::Red)
                    .add_modifier(Modifier::BOLD),
            ))
            .alignment(Alignment::Center),
            yes,
        );
        frame.render_widget(
            Paragraph::new(Span::styled(
                " [N] Cancel ",
                Style::default()
                    .fg(Color::White)
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            ))
            .alignment(Alignment::Center),
            no,
        );
    }
}

const EMPTY_BOX_WIDTH: u16 = 46;
const EMPTY_BOX_HEIGHT: u16 = 7;

/// The bordered placeholder box centered in `area`, or `None` when the pane area
/// is too small to hold it (callers fall back to a plain message / no click
/// target).
fn empty_placeholder_rect(area: Rect) -> Option<Rect> {
    if area.width < 24 || area.height < EMPTY_BOX_HEIGHT {
        return None;
    }
    let width = EMPTY_BOX_WIDTH.min(area.width);
    let height = EMPTY_BOX_HEIGHT;
    let x = area.x + (area.width - width) / 2;
    let y = area.y + (area.height - height) / 2;
    Some(Rect::new(x, y, width, height))
}

/// The clickable "[ + create pane ]" region inside the placeholder box. Kept in
/// sync with `draw_empty_workspace` so rendering and hit-testing agree; the y is
/// the button's row (border + title row + blank row) and the x centers the label
/// exactly as the centered Paragraph does.
fn empty_create_pane_rect(area: Rect) -> Option<Rect> {
    let box_rect = empty_placeholder_rect(area)?;
    let inner_x = box_rect.x + 1;
    let inner_width = box_rect.width.saturating_sub(2);
    let label_width = UnicodeWidthStr::width("[ + create pane ]") as u16;
    let width = label_width.min(inner_width);
    let x = inner_x + (inner_width - width) / 2;
    let y = box_rect.y + 3;
    Some(Rect::new(x, y, width, 1))
}

fn draw_empty_workspace(
    frame: &mut ratatui::Frame,
    area: Rect,
    theme: UiTheme,
    prefix_label: &str,
    hover_create: bool,
) {
    let palette = theme.palette();
    let Some(box_rect) = empty_placeholder_rect(area) else {
        frame.render_widget(
            Paragraph::new("no panes in this workspace")
                .alignment(Alignment::Center)
                .style(Style::default().fg(palette.muted).bg(palette.background)),
            area,
        );
        return;
    };
    let button_style = if hover_create {
        Style::default()
            .fg(palette.on_bright)
            .bg(palette.hover)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(palette.on_bright)
            .bg(palette.success)
            .add_modifier(Modifier::BOLD)
    };
    let lines = vec![
        Line::from(Span::styled(
            "no panes in this workspace",
            Style::default().fg(palette.text),
        )),
        Line::from(""),
        Line::from(Span::styled("[ + create pane ]", button_style)),
        Line::from(""),
        Line::from(Span::styled(
            format!("{prefix_label} %/\"  split    {prefix_label} c  new workspace"),
            Style::default().fg(palette.muted),
        )),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(palette.border))
        .style(Style::default().bg(palette.background));
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .alignment(Alignment::Center)
            .style(Style::default().bg(palette.background)),
        box_rect,
    );
}

/// Confirm killing a single pane (workspace tabs close from the tab bar).
fn pending_close_for_pane(snapshot: &Session, pane_id: &str) -> Option<PendingConfirm> {
    let pane = snapshot.panes.get(pane_id)?;
    Some(PendingConfirm {
        title: " ⚠ close pane ".to_string(),
        body: format!(
            "Close pane {pane_id} ({})?\n\nThis will stop the pane's process.",
            pane.title
        ),
        action: ConfirmAction::KillPane(pane_id.to_string()),
    })
}

/// The daemon request performed when a pending confirmation is accepted.
fn confirm_request(action: &ConfirmAction) -> Request {
    match action {
        ConfirmAction::CloseWorkspace => Request::CloseWorkspace { workspace: None },
        ConfirmAction::KillPane(pane) => Request::KillPane {
            pane: Some(pane.clone()),
        },
        ConfirmAction::CloseWorkspaceTab { tab } => Request::CloseTab {
            workspace: None,
            tab: tab.clone(),
        },
    }
}

fn close_workspace_warning_text(snapshot: &Session) -> String {
    snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.id == snapshot.active_workspace)
        .map(|workspace| {
            format!(
                "Close workspace '{}'?\n\nThis workspace has {} pane(s).\nClosing it will stop those panes.",
                workspace.name,
                workspace.panes.len()
            )
        })
        .unwrap_or_else(|| "No active workspace".to_string())
}

fn sidebar_width(collapsed: bool, expanded: u16) -> u16 {
    if collapsed {
        crate::config::SIDEBAR_COLLAPSED_WIDTH
    } else {
        crate::config::clamp_sidebar_width(expanded)
    }
}

/// True when `column` is on the draggable right edge of the expanded sidebar.
fn is_sidebar_resize_edge(column: u16, sidebar_cols: u16) -> bool {
    // Grab the border column and one column into the main area for easier hits.
    column + 1 == sidebar_cols || column == sidebar_cols
}

fn compact_workspace_id(id: &str) -> String {
    id.strip_prefix("ws-")
        .map(|suffix| suffix.chars().take(2).collect::<String>())
        .filter(|suffix| !suffix.is_empty())
        .unwrap_or_else(|| id.chars().take(2).collect())
}

/// Builds one workspace item for the sidebar list. Expanded mode uses a name
/// line plus a configurable detail line; collapsed mode stays one row.
///
/// Both lines are padded to `usable_width` so the row background fills the full
/// content column (no blank strip between text and the right border).
fn workspace_list_item(
    snapshot: &Session,
    index: usize,
    sidebar_collapsed: bool,
    second_line: UiWorkspaceSecondLine,
    hover_workspace: Option<&str>,
    usable_width: usize,
    palette: ThemePalette,
    status_markers: &str,
) -> ListItem<'static> {
    let workspace = &snapshot.workspaces[index];
    let active = workspace.id == snapshot.active_workspace;
    let hovered = hover_workspace == Some(workspace.id.as_str());
    // Fixed-width prefix so active/inactive rows align (avoids a ragged left edge).
    let prefix = if sidebar_collapsed {
        if active {
            "> "
        } else {
            "  "
        }
    } else if active {
        "> "
    } else {
        "  "
    };
    // Aggregate across every tab so ✅/🔄 still show if work finished on a
    // background tab while you were elsewhere.
    let all_panes: Vec<String> = workspace
        .all_pane_ids()
        .into_iter()
        .map(|id| id.to_string())
        .collect();
    let status = workspace_status(snapshot, &all_panes, status_markers);
    // Ordered segments so the pin and status marker can be tinted independently.
    // Concatenating the segment texts yields the exact row text hit-tests expect.
    let mut segments: Vec<(String, SidebarSeg)> = vec![(prefix.to_string(), SidebarSeg::Plain)];
    if sidebar_collapsed {
        let pin = if workspace.pinned { "*" } else { " " };
        let marker = if status.is_empty() {
            " ".to_string()
        } else {
            status.clone()
        };
        segments.push((pin.to_string(), SidebarSeg::Pin));
        segments.push((
            format!("{} ", compact_workspace_id(&workspace.id)),
            SidebarSeg::Plain,
        ));
        segments.push((marker, SidebarSeg::Status));
    } else {
        // Mandatory head keeps the status marker and index visible; the workspace
        // name is truncated, while the configured detail moves to line two.
        if workspace.pinned {
            segments.push(("*".to_string(), SidebarSeg::Pin));
            segments.push((" ".to_string(), SidebarSeg::Plain));
        }
        if !status.is_empty() {
            segments.push((status.clone(), SidebarSeg::Status));
            segments.push((" ".to_string(), SidebarSeg::Plain));
        }
        let head: String = segments.iter().map(|(text, _)| text.as_str()).collect();
        let rest = format!("{}:{}", index + 1, workspace.name);
        let budget = usable_width.saturating_sub(UnicodeWidthStr::width(head.as_str()));
        segments.push((truncate_to_width(&rest, budget), SidebarSeg::Plain));
    }
    let second = if sidebar_collapsed {
        None
    } else {
        workspace_second_line_text(snapshot, workspace, second_line)
            .filter(|line| !line.is_empty())
            .map(|line| {
                // Indent second line under the name (past the 2-char prefix).
                let indent = "  ";
                let budget = usable_width.saturating_sub(UnicodeWidthStr::width(indent));
                format!("{indent}{}", truncate_to_width(&line, budget))
            })
    };

    let row_bg = if active {
        palette.active
    } else if hovered {
        // selected_row_style uses hover bg — keep padding consistent
        palette.hover
    } else {
        palette.background
    };
    let active_style = Style::default()
        .fg(palette.on_accent)
        .bg(palette.active)
        .add_modifier(Modifier::BOLD);
    let hover_style = selected_row_style(palette);

    let spans: Vec<Span<'static>> = if active {
        let text: String = segments.into_iter().map(|(text, _)| text).collect();
        vec![Span::styled(
            pad_to_width(&text, usable_width),
            active_style,
        )]
    } else if hovered {
        let text: String = segments.into_iter().map(|(text, _)| text).collect();
        vec![Span::styled(pad_to_width(&text, usable_width), hover_style)]
    } else {
        // Ordinary row: tint the pin and status marker so they read at a glance.
        let base = Style::default().fg(palette.text).bg(palette.background);
        let mut spans: Vec<Span<'static>> = segments
            .into_iter()
            .filter(|(text, _)| !text.is_empty())
            .map(|(text, seg)| {
                let style = match seg {
                    SidebarSeg::Plain => base,
                    SidebarSeg::Pin if workspace.pinned => Style::default()
                        .fg(palette.active)
                        .bg(palette.background)
                        .add_modifier(Modifier::BOLD),
                    SidebarSeg::Pin => base,
                    SidebarSeg::Status => Style::default()
                        .fg(status_marker_color(&status, palette))
                        .bg(palette.background)
                        .add_modifier(Modifier::BOLD),
                };
                Span::styled(text, style)
            })
            .collect();
        pad_spans_to_width(&mut spans, usable_width, base);
        spans
    };
    let first = Line::from(spans);
    if let Some(second) = second {
        let second_style = if active {
            active_style
        } else if hovered {
            hover_style
        } else {
            Style::default().fg(palette.muted).bg(palette.background)
        };
        let second_padded = pad_to_width(&second, usable_width);
        ListItem::new(vec![
            first,
            Line::from(Span::styled(second_padded, second_style)),
        ])
    } else {
        // When expanded mode always reserves 2 rows per workspace, paint a solid
        // blank second line so no hole appears under the name.
        if !sidebar_collapsed {
            let blank_style = Style::default().fg(row_bg).bg(row_bg);
            ListItem::new(vec![
                first,
                Line::from(Span::styled(" ".repeat(usable_width), blank_style)),
            ])
        } else {
            ListItem::new(first)
        }
    }
}

/// Pad `value` with trailing spaces so its display width equals `width`.
fn pad_to_width(value: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let used = UnicodeWidthStr::width(value);
    if used >= width {
        return truncate_to_width(value, width);
    }
    format!("{value}{}", " ".repeat(width - used))
}

/// Append a trailing space span so multi-span rows fill `width` (row bg extends
/// to the sidebar border — no blank column on the right).
fn pad_spans_to_width(spans: &mut Vec<Span<'static>>, width: usize, pad_style: Style) {
    if width == 0 {
        return;
    }
    let used: usize = spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum();
    if used < width {
        spans.push(Span::styled(" ".repeat(width - used), pad_style));
    }
}

fn workspace_second_line_text(
    snapshot: &Session,
    workspace: &crate::model::Workspace,
    second_line: UiWorkspaceSecondLine,
) -> Option<String> {
    match second_line {
        UiWorkspaceSecondLine::Path => Some(short_path(&workspace.cwd)),
        UiWorkspaceSecondLine::Details => {
            let branch = workspace
                .git_branch
                .as_deref()
                .map(|branch| format!(" @{branch}"))
                .unwrap_or_default();
            let pr = pull_request_label(workspace.pull_request.as_ref());
            let ports = port_label(&workspace.ports);
            let note = workspace_notification(snapshot, &workspace.id, &workspace.panes)
                .map(|note| format!(" {}", trim_label(&note, 18)))
                .unwrap_or_default();
            Some(format!(
                "{}{}{}{}{}",
                compact_path(&workspace.cwd),
                branch,
                pr,
                ports,
                note
            ))
        }
        UiWorkspaceSecondLine::Branch => workspace
            .git_branch
            .as_deref()
            .map(|branch| format!("@{branch}")),
        UiWorkspaceSecondLine::Id => Some(workspace.id.clone()),
        UiWorkspaceSecondLine::Status => {
            let all_panes: Vec<String> = workspace
                .all_pane_ids()
                .into_iter()
                .map(|id| id.to_string())
                .collect();
            // Status second-line uses emoji markers (config applies in sidebar chip).
            let status = workspace_status(snapshot, &all_panes, "emoji");
            if status.is_empty() {
                Some("idle".to_string())
            } else {
                Some(status)
            }
        }
        UiWorkspaceSecondLine::Cursor => workspace
            .active_pane
            .as_deref()
            .and_then(|pane_id| snapshot.panes.get(pane_id))
            .and_then(|pane| match (pane.cursor_row, pane.cursor_col) {
                (Some(row), Some(col)) => Some(format!("cursor {}:{}", row + 1, col + 1)),
                _ => None,
            }),
        UiWorkspaceSecondLine::None => None,
    }
}

/// Segment role within a sidebar workspace row, used to tint the pin and status
/// marker independently of the surrounding text on ordinary rows.
#[derive(Debug, Clone, Copy)]
enum SidebarSeg {
    Plain,
    Pin,
    Status,
}

/// Color for a workspace status marker so severity reads at a glance.
/// Supports both emoji markers (❌🙋🔄✅) and legacy ASCII (!? * +).
fn status_marker_color(status: &str, palette: ThemePalette) -> Color {
    if status.contains('❌') || status.starts_with('!') {
        palette.danger
    } else if status.contains('🙋') || status.contains('⚠') || status.starts_with('?') {
        palette.warning
    } else if status.contains('🔄') || status.starts_with('*') {
        palette.command
    } else if status.contains('✅') || status.starts_with('+') {
        palette.success
    } else {
        palette.muted
    }
}

/// A single row in the scrollable sidebar list. Both rendering and hit-testing
/// walk the same entry sequence so clicks always line up with what is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SidebarEntry {
    /// Overflow marker at the top; carries the count of hidden workspaces above.
    Above(usize),
    /// A visible workspace at this index in `snapshot.workspaces`.
    Workspace(usize),
    /// Overflow marker at the bottom; carries the count of hidden workspaces below.
    Below(usize),
}

fn sidebar_workspace_height(collapsed: bool) -> usize {
    if collapsed {
        1
    } else {
        2
    }
}

/// Lays out the sidebar list for the given scroll `offset` and available
/// `list_height` terminal rows, inserting overflow markers when workspaces are hidden.
fn sidebar_entries(
    count: usize,
    offset: usize,
    list_height: u16,
    collapsed: bool,
) -> Vec<SidebarEntry> {
    let list_height = list_height as usize;
    if count == 0 || list_height == 0 {
        return Vec::new();
    }
    let workspace_height = sidebar_workspace_height(collapsed);
    if count.saturating_mul(workspace_height) <= list_height {
        return (0..count).map(SidebarEntry::Workspace).collect();
    }
    let offset = offset.min(count.saturating_sub(1));
    let has_above = offset > 0;
    let mut workspace_slots = list_height
        .saturating_sub(has_above as usize)
        .checked_div(workspace_height)
        .unwrap_or(0)
        .max(1);
    let mut has_below = offset + workspace_slots < count;
    if has_below {
        workspace_slots = list_height
            .saturating_sub(has_above as usize + 1)
            .checked_div(workspace_height)
            .unwrap_or(0)
            .max(1);
        has_below = offset + workspace_slots < count;
    }
    let end = (offset + workspace_slots).min(count);
    let mut entries = Vec::with_capacity(list_height);
    if has_above {
        entries.push(SidebarEntry::Above(offset));
    }
    entries.extend((offset..end).map(SidebarEntry::Workspace));
    if has_below {
        entries.push(SidebarEntry::Below(count - end));
    }
    entries
}

/// Largest scroll offset that still fills the list (tail window visible).
fn max_sidebar_offset(count: usize, list_height: u16, collapsed: bool) -> usize {
    let list_height = list_height as usize;
    let workspace_height = sidebar_workspace_height(collapsed);
    if list_height == 0 || count.saturating_mul(workspace_height) <= list_height {
        return 0;
    }
    // At the very bottom only the top overflow marker is shown, leaving
    // `list_height - 1` terminal rows for workspace entries.
    let visible_slots = list_height
        .saturating_sub(1)
        .checked_div(workspace_height)
        .unwrap_or(0)
        .max(1);
    count.saturating_sub(visible_slots)
}

/// Inclusive range of visible workspace indices for the given offset.
fn sidebar_visible_range(
    count: usize,
    offset: usize,
    list_height: u16,
    collapsed: bool,
) -> Option<(usize, usize)> {
    let visible: Vec<usize> = sidebar_entries(count, offset, list_height, collapsed)
        .into_iter()
        .filter_map(|entry| match entry {
            SidebarEntry::Workspace(index) => Some(index),
            _ => None,
        })
        .collect();
    Some((*visible.first()?, *visible.last()?))
}

/// Adjusts the scroll offset so `active` is visible, moving as little as needed.
fn scroll_to_reveal(
    count: usize,
    active: usize,
    offset: usize,
    list_height: u16,
    collapsed: bool,
) -> usize {
    let workspace_height = sidebar_workspace_height(collapsed);
    if list_height == 0 || count.saturating_mul(workspace_height) <= list_height as usize {
        return 0;
    }
    let mut offset = offset.min(max_sidebar_offset(count, list_height, collapsed));
    for _ in 0..=count {
        let Some((first, last)) = sidebar_visible_range(count, offset, list_height, collapsed)
        else {
            break;
        };
        if active < first && offset > 0 {
            offset -= 1;
        } else if active > last {
            offset += 1;
        } else {
            break;
        }
    }
    offset.min(max_sidebar_offset(count, list_height, collapsed))
}

fn workspace_at_sidebar_row(
    snapshot: &Session,
    offset: usize,
    list_height: u16,
    collapsed: bool,
    row: u16,
) -> Option<&crate::model::Workspace> {
    let index =
        sidebar_position_from_row(snapshot, offset, list_height, collapsed, row)?.saturating_sub(1);
    snapshot.workspaces.get(index)
}

fn sidebar_position_from_row(
    snapshot: &Session,
    offset: usize,
    list_height: u16,
    collapsed: bool,
    row: u16,
) -> Option<usize> {
    let list_row = row.checked_sub(1)? as usize;
    let mut cursor = 0usize;
    for entry in sidebar_entries(snapshot.workspaces.len(), offset, list_height, collapsed) {
        let height = match entry {
            SidebarEntry::Workspace(_) => sidebar_workspace_height(collapsed),
            _ => 1,
        };
        if list_row >= cursor && list_row < cursor + height {
            return match entry {
                SidebarEntry::Workspace(index) => Some(index + 1),
                _ => None,
            };
        }
        cursor += height;
    }
    None
}

fn primary_mouse_action(
    snapshot: &Session,
    sidebar_offset: usize,
    sidebar_collapsed: bool,
    sidebar_expanded: u16,
    terminal_width: u16,
    terminal_height: u16,
    column: u16,
    row: u16,
    status_markers: &str,
    tab_close_button: bool,
) -> PrimaryMouseAction {
    let sidebar_width = sidebar_width(sidebar_collapsed, sidebar_expanded);
    if column < sidebar_width {
        let list_height = terminal_height.saturating_sub(1);
        return workspace_at_sidebar_row(
            snapshot,
            sidebar_offset,
            list_height,
            sidebar_collapsed,
            row,
        )
        .map(|workspace| PrimaryMouseAction::SwitchWorkspace(workspace.id.clone()))
        .unwrap_or(PrimaryMouseAction::None);
    }
    let Some(active) = snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.id == snapshot.active_workspace)
    else {
        return PrimaryMouseAction::None;
    };
    if active.panes.is_empty() {
        return PrimaryMouseAction::None;
    }
    let tab_area = workspace_tab_bar_area(sidebar_collapsed, sidebar_expanded, terminal_width);
    if let Some(action) = workspace_tab_bar_hit(
        snapshot,
        active,
        tab_area,
        column,
        row,
        status_markers,
        tab_close_button,
    ) {
        return action;
    }
    let area = pane_area(
        sidebar_collapsed,
        sidebar_expanded,
        terminal_width,
        terminal_height,
    );
    if !point_in_rect(area, column, row) {
        return PrimaryMouseAction::None;
    }
    if active.zoomed_pane.is_none() {
        if let Some(axis) = split_axis_at(active.layout.as_ref(), area, column, row) {
            return PrimaryMouseAction::StartResize(PaneResizeDrag { axis, column, row });
        }
    }
    if let Some(pane_id) = active
        .zoomed_pane
        .as_ref()
        .filter(|pane| active.panes.iter().any(|item| item == *pane))
    {
        return PrimaryMouseAction::FocusPane(pane_id.clone());
    }
    pane_at(active.layout.as_ref(), area, column, row)
        .map(PrimaryMouseAction::FocusPane)
        .unwrap_or(PrimaryMouseAction::None)
}

fn pane_area(
    sidebar_collapsed: bool,
    sidebar_expanded: u16,
    terminal_width: u16,
    terminal_height: u16,
) -> Rect {
    let sidebar_width = sidebar_width(sidebar_collapsed, sidebar_expanded);
    Rect::new(
        sidebar_width,
        TAB_BAR_HEIGHT,
        terminal_width.saturating_sub(sidebar_width),
        terminal_height
            .saturating_sub(CONTROL_BAR_HEIGHT)
            .saturating_sub(TAB_BAR_HEIGHT),
    )
}

fn workspace_tab_bar_area(
    sidebar_collapsed: bool,
    sidebar_expanded: u16,
    terminal_width: u16,
) -> Rect {
    let sidebar_width = sidebar_width(sidebar_collapsed, sidebar_expanded);
    Rect::new(
        sidebar_width,
        0,
        terminal_width.saturating_sub(sidebar_width),
        TAB_BAR_HEIGHT,
    )
}

/// Resolve a double-click rename target under the pointer, with the current name.
fn rename_target_at(
    snapshot: &Session,
    sidebar_offset: usize,
    sidebar_collapsed: bool,
    sidebar_expanded: u16,
    terminal_width: u16,
    terminal_height: u16,
    column: u16,
    row: u16,
) -> Option<(RenameTarget, String)> {
    let sw = sidebar_width(sidebar_collapsed, sidebar_expanded);
    // Workspace name in the sidebar.
    if column < sw {
        let list_height = terminal_height.saturating_sub(1);
        let ws = workspace_at_sidebar_row(
            snapshot,
            sidebar_offset,
            list_height,
            sidebar_collapsed,
            row,
        )?;
        return Some((
            RenameTarget::Workspace { id: ws.id.clone() },
            ws.name.clone(),
        ));
    }
    let active = snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.id == snapshot.active_workspace)?;
    // Tab title in the tab bar (not the "+" control or close ×).
    let tab_area = workspace_tab_bar_area(sidebar_collapsed, sidebar_expanded, terminal_width);
    if point_in_rect(tab_area, column, row) {
        let mut x = tab_area.x;
        for chip in workspace_tab_chips(snapshot, active, "emoji", true) {
            let width = UnicodeWidthStr::width(chip.label.as_str()) as u16;
            if column >= x && column < x.saturating_add(width) {
                let rel = column.saturating_sub(x) as usize;
                // Clicking × is close, not rename.
                if chip.close_start.map(|start| rel >= start).unwrap_or(false) {
                    return None;
                }
                let title = active
                    .tabs
                    .iter()
                    .find(|t| t.id == chip.tab_id)
                    .map(|t| t.title.clone())
                    .unwrap_or_default();
                return Some((
                    RenameTarget::Tab {
                        id: chip.tab_id.clone(),
                    },
                    title,
                ));
            }
            x = x.saturating_add(width).saturating_add(1);
        }
        return None;
    }
    // Pane title on the top border row of a pane.
    let area = pane_area(
        sidebar_collapsed,
        sidebar_expanded,
        terminal_width,
        terminal_height,
    );
    if !point_in_rect(area, column, row) {
        return None;
    }
    if let Some(zoomed) = active
        .zoomed_pane
        .as_ref()
        .filter(|pane| active.panes.iter().any(|item| item == *pane))
    {
        if row == area.y {
            let title = snapshot.panes.get(zoomed).map(|p| p.title.clone())?;
            return Some((RenameTarget::Pane { id: zoomed.clone() }, title));
        }
        return None;
    }
    let pane_id = pane_title_at(active.layout.as_ref(), area, column, row)?;
    let title = snapshot.panes.get(&pane_id).map(|p| p.title.clone())?;
    Some((RenameTarget::Pane { id: pane_id }, title))
}

/// Pane whose top border (title row) contains the pointer.
fn pane_title_at(node: Option<&LayoutNode>, area: Rect, column: u16, row: u16) -> Option<String> {
    if !point_in_rect(area, column, row) {
        return None;
    }
    let node = node?;
    match node {
        LayoutNode::Pane { pane } => {
            if row == area.y {
                Some(pane.clone())
            } else {
                None
            }
        }
        LayoutNode::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            let ratio = (*ratio).clamp(15, 85) as u32;
            match axis {
                SplitAxis::Horizontal => {
                    let split = area.x + ((area.width as u32 * ratio) / 100) as u16;
                    if column < split {
                        let first_area =
                            Rect::new(area.x, area.y, split.saturating_sub(area.x), area.height);
                        pane_title_at(Some(first), first_area, column, row)
                    } else {
                        let second_area = Rect::new(
                            split,
                            area.y,
                            area.x.saturating_add(area.width).saturating_sub(split),
                            area.height,
                        );
                        pane_title_at(Some(second), second_area, column, row)
                    }
                }
                SplitAxis::Vertical => {
                    let split = area.y + ((area.height as u32 * ratio) / 100) as u16;
                    if row < split {
                        let first_area =
                            Rect::new(area.x, area.y, area.width, split.saturating_sub(area.y));
                        pane_title_at(Some(first), first_area, column, row)
                    } else {
                        let second_area = Rect::new(
                            area.x,
                            split,
                            area.width,
                            area.y.saturating_add(area.height).saturating_sub(split),
                        );
                        pane_title_at(Some(second), second_area, column, row)
                    }
                }
            }
        }
    }
}

/// One tab chip in the workspace tab bar (title + optional close control).
struct WorkspaceTabChip {
    tab_id: String,
    /// Full visible text including padding and optional ` ×`.
    label: String,
    /// Column offset within `label` where the close `×` starts, if closable.
    close_start: Option<usize>,
}

fn workspace_tab_chips(
    snapshot: &Session,
    workspace: &crate::model::Workspace,
    status_markers: &str,
    tab_close_button: bool,
) -> Vec<WorkspaceTabChip> {
    let can_close = tab_close_button && workspace.tabs.len() > 1;
    workspace
        .tabs
        .iter()
        .map(|tab| {
            let status = workspace_status(snapshot, &tab.panes, status_markers);
            let title = truncate_to_width(&tab.title, 14);
            let body = if status.is_empty() {
                format!(" {title}")
            } else {
                format!(" {status} {title}")
            };
            if can_close {
                // " title × " — close glyph is always the last non-space character.
                let head = format!("{body} ");
                let close_start = UnicodeWidthStr::width(head.as_str());
                WorkspaceTabChip {
                    tab_id: tab.id.clone(),
                    label: format!("{head}× "),
                    close_start: Some(close_start),
                }
            } else {
                WorkspaceTabChip {
                    tab_id: tab.id.clone(),
                    label: format!("{body} "),
                    close_start: None,
                }
            }
        })
        .collect()
}

fn draw_workspace_tab_bar(
    frame: &mut ratatui::Frame,
    area: Rect,
    snapshot: &Session,
    workspace: &crate::model::Workspace,
    palette: ThemePalette,
    status_markers: &str,
    tab_close_button: bool,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let mut spans = Vec::new();
    for chip in workspace_tab_chips(snapshot, workspace, status_markers, tab_close_button) {
        let active = workspace.active_tab.as_deref() == Some(chip.tab_id.as_str());
        let style = if active {
            Style::default()
                .fg(palette.on_accent)
                .bg(palette.active)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(palette.muted).bg(palette.surface)
        };
        // Paint close × in danger color when present so it's clickable-looking.
        if let Some(close_start) = chip.close_start {
            let head: String = chip
                .label
                .chars()
                .scan(0usize, |col, ch| {
                    let start = *col;
                    *col += UnicodeWidthChar::width(ch).unwrap_or(0).max(1);
                    Some((start, ch))
                })
                .take_while(|(start, _)| *start < close_start)
                .map(|(_, ch)| ch)
                .collect();
            let tail: String = chip
                .label
                .chars()
                .scan(0usize, |col, ch| {
                    let start = *col;
                    *col += UnicodeWidthChar::width(ch).unwrap_or(0).max(1);
                    Some((start, ch))
                })
                .skip_while(|(start, _)| *start < close_start)
                .map(|(_, ch)| ch)
                .collect();
            spans.push(Span::styled(head, style));
            let close_style = if active {
                Style::default()
                    .fg(palette.danger)
                    .bg(palette.active)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(palette.danger)
                    .bg(palette.surface)
                    .add_modifier(Modifier::BOLD)
            };
            spans.push(Span::styled(tail, close_style));
        } else {
            spans.push(Span::styled(chip.label, style));
        }
        spans.push(Span::raw(" "));
    }
    spans.push(Span::styled(
        " + ",
        Style::default()
            .fg(palette.success)
            .bg(palette.surface)
            .add_modifier(Modifier::BOLD),
    ));
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(palette.background)),
        area,
    );
}

/// Hit-test the workspace tab bar. Returns focus-tab, close-tab, new-tab, or None.
fn workspace_tab_bar_hit(
    snapshot: &Session,
    workspace: &crate::model::Workspace,
    area: Rect,
    column: u16,
    row: u16,
    status_markers: &str,
    tab_close_button: bool,
) -> Option<PrimaryMouseAction> {
    if !point_in_rect(area, column, row) {
        return None;
    }
    let mut x = area.x;
    for chip in workspace_tab_chips(snapshot, workspace, status_markers, tab_close_button) {
        let width = UnicodeWidthStr::width(chip.label.as_str()) as u16;
        if column >= x && column < x.saturating_add(width) {
            let rel = column.saturating_sub(x) as usize;
            if let Some(close_start) = chip.close_start {
                if rel >= close_start {
                    return Some(PrimaryMouseAction::CloseWorkspaceTab { tab: chip.tab_id });
                }
            }
            return Some(PrimaryMouseAction::FocusWorkspaceTab { tab: chip.tab_id });
        }
        x = x.saturating_add(width).saturating_add(1);
    }
    // trailing " + "
    let plus_width = 3_u16;
    if column >= x && column < x.saturating_add(plus_width) {
        return Some(PrimaryMouseAction::NewWorkspaceTab);
    }
    None
}

fn control_buttons() -> Vec<ControlButton> {
    // Icons use emoji presentation (VS16 where needed) so every glyph is
    // double-width — avoids the uneven gaps from mixed 1-cell / 2-cell symbols.
    // Note: ☰ is typically single-width in terminals; keep a short label.
    vec![
        ControlButton {
            // Burger / workspace menu (double-width emoji for control-bar alignment).
            icon: "📱",
            label: "menu",
            action: ControlAction::Workspaces,
        },
        ControlButton {
            icon: "📁",
            label: "workspace",
            action: ControlAction::NewWorkspace,
        },
        ControlButton {
            icon: "📑",
            label: "tab",
            action: ControlAction::NewTab,
        },
        ControlButton {
            icon: "➡️",
            label: "split→",
            action: ControlAction::SplitRight,
        },
        ControlButton {
            icon: "⬇️",
            label: "split↓",
            action: ControlAction::SplitDown,
        },
        ControlButton {
            icon: "🗑️",
            label: "del-ws",
            action: ControlAction::DeleteWorkspace,
        },
        ControlButton {
            icon: "⌨️",
            label: "commands",
            action: ControlAction::Commands,
        },
        ControlButton {
            icon: "🔔",
            label: "notes",
            action: ControlAction::Notifications,
        },
        ControlButton {
            icon: "⚙️",
            label: "settings",
            action: ControlAction::Settings,
        },
        ControlButton {
            icon: "❌",
            label: "close",
            action: ControlAction::KillPane,
        },
    ]
}

/// Detach is drawn and hit-tested on the far right of the control bar.
fn detach_control_button() -> ControlButton {
    ControlButton {
        icon: "⏏️",
        label: "detach",
        action: ControlAction::Detach,
    }
}

fn control_button_rects(area: Rect) -> Vec<(ControlAction, Rect)> {
    let detach = detach_control_button();
    let detach_w = detach.width().min(area.width);
    // Reserve the right edge for detach so left buttons never overlap it.
    let left_limit = area.x.saturating_add(area.width).saturating_sub(detach_w);

    let mut x = area.x;
    let mut rects = Vec::new();
    for button in control_buttons() {
        if x >= left_limit {
            break;
        }
        let remaining = left_limit.saturating_sub(x);
        if remaining < 4 {
            break;
        }
        let width = button.width().min(remaining);
        rects.push((
            button.action,
            Rect::new(x, area.y, width, area.height.max(1)),
        ));
        x = x.saturating_add(width);
    }
    if detach_w > 0 && area.width >= detach_w {
        let dx = area.x.saturating_add(area.width).saturating_sub(detach_w);
        rects.push((
            ControlAction::Detach,
            Rect::new(dx, area.y, detach_w, area.height.max(1)),
        ));
    }
    rects
}

fn control_action_at(
    sidebar_collapsed: bool,
    sidebar_expanded: u16,
    terminal_width: u16,
    terminal_height: u16,
    column: u16,
    row: u16,
) -> Option<ControlAction> {
    if terminal_height < CONTROL_BAR_HEIGHT || row != terminal_height.saturating_sub(2) {
        return None;
    }
    let sidebar_width = sidebar_width(sidebar_collapsed, sidebar_expanded);
    let area = Rect::new(
        sidebar_width,
        terminal_height.saturating_sub(2),
        terminal_width.saturating_sub(sidebar_width),
        1,
    );
    control_button_rects(area)
        .into_iter()
        .find_map(|(action, rect)| point_in_rect(rect, column, row).then_some(action))
}

fn control_button_style(
    button: ControlButton,
    mode: UiMode,
    confirm: Option<&PendingConfirm>,
    hover: Option<ControlAction>,
    palette: ThemePalette,
) -> Style {
    let active = matches!(
        (mode, button.action),
        (UiMode::Commands, ControlAction::Commands)
            | (UiMode::Notifications, ControlAction::Notifications)
            | (UiMode::Settings, ControlAction::Settings)
            | (UiMode::WorkspacePicker, ControlAction::Workspaces)
    ) || matches!(
        (confirm.map(|pending| &pending.action), button.action),
        (
            Some(ConfirmAction::CloseWorkspace),
            ControlAction::DeleteWorkspace
        ) | (Some(ConfirmAction::KillPane(_)), ControlAction::KillPane)
            | (
                Some(ConfirmAction::CloseWorkspaceTab { .. }),
                ControlAction::KillPane
            )
    );
    let hovered = hover == Some(button.action);
    if active || hovered {
        Style::default()
            .fg(Color::Black)
            .bg(if active {
                palette.active
            } else {
                palette.hover
            })
            .add_modifier(Modifier::BOLD)
    } else {
        match button.action {
            ControlAction::Detach | ControlAction::KillPane | ControlAction::DeleteWorkspace => {
                Style::default().fg(palette.danger).bg(palette.surface)
            }
            ControlAction::NewWorkspace
            | ControlAction::NewTab
            | ControlAction::SplitRight
            | ControlAction::SplitDown => Style::default().fg(palette.success).bg(palette.surface),
            ControlAction::Notifications => {
                Style::default().fg(palette.warning).bg(palette.surface)
            }
            ControlAction::Commands | ControlAction::Settings | ControlAction::Workspaces => {
                Style::default().fg(palette.command).bg(palette.surface)
            }
        }
    }
}

fn draw_control_bar(
    frame: &mut ratatui::Frame,
    area: Rect,
    mode: UiMode,
    confirm: Option<&PendingConfirm>,
    hover: Option<ControlAction>,
    theme: UiTheme,
) {
    let palette = theme.palette();
    let detach = detach_control_button();
    let detach_w = detach.width().min(area.width);
    let left_limit = area.width.saturating_sub(detach_w);

    let mut spans = Vec::new();
    let mut used: u16 = 0;
    for button in control_buttons() {
        let bw = button.width();
        if used.saturating_add(bw) > left_limit {
            break;
        }
        let style = control_button_style(button, mode, confirm, hover, palette);
        let text = button.text();
        let pad = usize::from(bw).saturating_sub(UnicodeWidthStr::width(text.as_str()));
        spans.push(Span::styled(text, style));
        if pad > 0 {
            spans.push(Span::styled(" ".repeat(pad), style));
        }
        used = used.saturating_add(bw);
    }
    // Flexible spacer pushes detach to the far right of the bar.
    let spacer = left_limit.saturating_sub(used) as usize;
    if spacer > 0 {
        spans.push(Span::styled(
            " ".repeat(spacer),
            Style::default().bg(palette.surface),
        ));
    }
    if detach_w > 0 {
        let style = control_button_style(detach, mode, confirm, hover, palette);
        let text = detach.text();
        let pad = usize::from(detach_w).saturating_sub(UnicodeWidthStr::width(text.as_str()));
        spans.push(Span::styled(text, style));
        if pad > 0 {
            spans.push(Span::styled(" ".repeat(pad), style));
        }
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans))
            .style(Style::default().fg(palette.text).bg(palette.surface)),
        area,
    );
}

fn draw_panes(
    frame: &mut ratatui::Frame,
    area: Rect,
    snapshot: &Session,
    layout: Option<&LayoutNode>,
    pane_ids: &[String],
    active_pane: Option<&str>,
    pane_sizes: &mut BTreeMap<String, PaneSize>,
    scroll_offsets: &BTreeMap<String, usize>,
    hover_pane_control: Option<&PaneControlHit>,
    selection: Option<&TextSelection>,
    highlight_pane: Option<&str>,
    theme: UiTheme,
    host_cursor: Option<&mut Option<(u16, u16)>>,
    cursor_blink_on: bool,
) {
    if pane_ids.is_empty() {
        frame.render_widget(Paragraph::new("No panes"), area);
        return;
    }
    if let Some(layout) = layout {
        draw_layout_node(
            frame,
            area,
            snapshot,
            layout,
            active_pane,
            pane_sizes,
            scroll_offsets,
            hover_pane_control,
            selection,
            highlight_pane,
            theme,
            host_cursor,
            cursor_blink_on,
        );
        return;
    }
    // Shared mutable slot: only the active leaf writes.
    let mut host_cursor = host_cursor;
    for (area, pane_id) in fallback_grid(area, pane_ids.len())
        .into_iter()
        .zip(pane_ids)
    {
        draw_single_pane(
            frame,
            area,
            snapshot,
            pane_id,
            active_pane,
            pane_sizes,
            scroll_offsets,
            hover_pane_control,
            selection,
            highlight_pane,
            theme,
            host_cursor.as_deref_mut(),
            cursor_blink_on,
        );
    }
}

fn draw_layout_node(
    frame: &mut ratatui::Frame,
    area: Rect,
    snapshot: &Session,
    node: &LayoutNode,
    active_pane: Option<&str>,
    pane_sizes: &mut BTreeMap<String, PaneSize>,
    scroll_offsets: &BTreeMap<String, usize>,
    hover_pane_control: Option<&PaneControlHit>,
    selection: Option<&TextSelection>,
    highlight_pane: Option<&str>,
    theme: UiTheme,
    host_cursor: Option<&mut Option<(u16, u16)>>,
    cursor_blink_on: bool,
) {
    match node {
        LayoutNode::Pane { pane } => draw_single_pane(
            frame,
            area,
            snapshot,
            pane,
            active_pane,
            pane_sizes,
            scroll_offsets,
            hover_pane_control,
            selection,
            highlight_pane,
            theme,
            host_cursor,
            cursor_blink_on,
        ),
        LayoutNode::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            let first_pct = (*ratio).clamp(15, 85) as u32;
            let second_pct = 100 - first_pct;
            let direction = match axis {
                SplitAxis::Horizontal => Direction::Horizontal,
                SplitAxis::Vertical => Direction::Vertical,
            };
            let chunks = Layout::default()
                .direction(direction)
                .constraints([
                    Constraint::Percentage(first_pct as u16),
                    Constraint::Percentage(second_pct as u16),
                ])
                .split(area);
            let mut host_cursor = host_cursor;
            draw_layout_node(
                frame,
                chunks[0],
                snapshot,
                first,
                active_pane,
                pane_sizes,
                scroll_offsets,
                hover_pane_control,
                selection,
                highlight_pane,
                theme,
                host_cursor.as_deref_mut(),
                cursor_blink_on,
            );
            draw_layout_node(
                frame,
                chunks[1],
                snapshot,
                second,
                active_pane,
                pane_sizes,
                scroll_offsets,
                hover_pane_control,
                selection,
                highlight_pane,
                theme,
                host_cursor,
                cursor_blink_on,
            );
        }
    }
}

fn draw_single_pane(
    frame: &mut ratatui::Frame,
    area: Rect,
    snapshot: &Session,
    pane_id: &str,
    active_pane: Option<&str>,
    pane_sizes: &mut BTreeMap<String, PaneSize>,
    scroll_offsets: &BTreeMap<String, usize>,
    hover_pane_control: Option<&PaneControlHit>,
    selection: Option<&TextSelection>,
    highlight_pane: Option<&str>,
    theme: UiTheme,
    mut host_cursor: Option<&mut Option<(u16, u16)>>,
    cursor_blink_on: bool,
) {
    if let Some(pane) = snapshot.panes.get(pane_id) {
        let palette = theme.palette();
        let has_strip = pane_has_tab_strip(pane) && area.width > 2 && area.height >= 3;
        let content = pane_content_area(area, has_strip);
        let inner_rows = content.height.max(1);
        let inner_cols = area.width.saturating_sub(2).max(2);
        pane_sizes.insert(
            pane_id.to_string(),
            PaneSize {
                rows: inner_rows,
                cols: inner_cols,
            },
        );
        let active = active_pane == Some(pane_id);
        let danger = highlight_pane == Some(pane_id);
        let scroll_offset = *scroll_offsets.get(pane_id).unwrap_or(&0);
        // Cap the title so it never runs under the top-right control buttons.
        let controls_reserved = if area.width >= 10 { 10 } else { 0 };
        let title_budget = usize::from(area.width)
            .saturating_sub(2)
            .saturating_sub(controls_reserved);
        let title = truncate_to_width(&pane_title_text(pane, scroll_offset), title_budget);
        // Danger: thick red border + red title only — keep pane body readable.
        let (title_style, border_style) = if danger {
            (
                Style::default()
                    .fg(Color::White)
                    .bg(Color::Red)
                    .add_modifier(Modifier::BOLD),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            )
        } else if active {
            (
                Style::default()
                    .fg(palette.active)
                    .add_modifier(Modifier::BOLD),
                Style::default()
                    .fg(palette.active)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            (
                Style::default().fg(palette.muted),
                Style::default().fg(palette.border),
            )
        };
        // Outer chrome uses the base background; pane content uses surface_alt so
        // the terminal body is slightly distinct from the surrounding chrome.
        let block = Block::default()
            .title(Span::styled(title, title_style))
            .borders(Borders::ALL)
            .border_style(border_style)
            .style(Style::default().fg(palette.text).bg(palette.background));
        // Cursor only on the active pane: solid while typing, otherwise blinks.
        let show_cursor = active && !danger && cursor_blink_on;
        let text = pane_lines_for_view(
            pane,
            inner_rows as usize,
            scroll_offset,
            selection.filter(|selection| selection.pane == pane_id),
            content,
            palette,
            show_cursor,
        );
        frame.render_widget(block, area);
        frame.render_widget(
            Paragraph::new(text).style(Style::default().fg(palette.text).bg(palette.surface_alt)),
            content,
        );
        // Host caret tracks the active pane only (hidden while blink is off).
        if show_cursor {
            if let Some((x, y)) = host_cursor_position(pane, content, scroll_offset) {
                if let Some(slot) = host_cursor.as_mut() {
                    **slot = Some((x, y));
                }
            }
        }
        // Workspace-level tabs render in the global tab bar; no per-pane strip.
        let _ = has_strip;
        // Hide corner controls on the pane marked for close (and when confirm
        // is open the caller already passes hover_pane_control as None).
        if !danger {
            let layout = snapshot
                .workspaces
                .iter()
                .find(|w| w.panes.iter().any(|p| p == pane_id))
                .and_then(|w| w.layout.as_ref());
            draw_pane_controls(frame, area, pane_id, layout, hover_pane_control, theme);
        }
    }
}

/// One laid-out cell in a pane's tab strip. The same layout drives rendering and
/// hit-testing so a click always resolves to exactly the tab under the cursor.
#[cfg(test)]
struct TabCell {
    /// Tab id to focus when this cell is clicked (for the overflow marker this is
    /// the next hidden tab to reveal).
    focus_id: String,
    text: String,
    active: bool,
    overflow: bool,
    /// Absolute start column (inclusive).
    start: u16,
    /// Absolute end column (exclusive).
    end: u16,
}

/// Lays out a pane's tab strip into positioned cells within `strip`. The active
/// tab is always kept visible; when tabs overflow the width a trailing `+N`
/// marker is appended (clicking it reveals the next hidden tab).
#[cfg(test)]
fn pane_tab_cells(pane: &crate::model::Pane, strip: Rect) -> Vec<TabCell> {
    let mut cells = Vec::new();
    if pane.tabs.len() <= 1 || strip.width == 0 {
        return cells;
    }
    let width = usize::from(strip.width);
    let active_index = pane
        .tabs
        .iter()
        .position(|tab| pane.active_tab.as_deref() == Some(tab.id.as_str()))
        .unwrap_or(0);
    let labels: Vec<String> = pane
        .tabs
        .iter()
        .enumerate()
        .map(|(index, tab)| format!(" {}:{} ", index + 1, trim_label(&tab.title, 12)))
        .collect();
    let widths: Vec<usize> = labels
        .iter()
        .map(|label| UnicodeWidthStr::width(label.as_str()))
        .collect();
    let total: usize = widths.iter().sum();

    let (start_idx, end_idx, overflow) = if total <= width {
        (0, pane.tabs.len(), false)
    } else {
        let reserve = UnicodeWidthStr::width(format!(" +{} ", pane.tabs.len()).as_str());
        let avail = width.saturating_sub(reserve).max(1);
        let mut start = active_index;
        let mut end = active_index + 1;
        let mut used = widths[active_index];
        loop {
            let mut grew = false;
            if end < pane.tabs.len() && used + widths[end] <= avail {
                used += widths[end];
                end += 1;
                grew = true;
            }
            if start > 0 && used + widths[start - 1] <= avail {
                start -= 1;
                used += widths[start - 1];
                grew = true;
            }
            if !grew {
                break;
            }
        }
        (start, end, true)
    };

    let strip_end = strip.x.saturating_add(strip.width);
    let mut col = strip.x;
    for index in start_idx..end_idx {
        if col >= strip_end {
            break;
        }
        let end = col.saturating_add(widths[index] as u16).min(strip_end);
        cells.push(TabCell {
            focus_id: pane.tabs[index].id.clone(),
            text: labels[index].clone(),
            active: index == active_index,
            overflow: false,
            start: col,
            end,
        });
        col = end;
    }
    if overflow && col < strip_end {
        let hidden = pane.tabs.len() - (end_idx - start_idx);
        // Reveal the first tab after the window, wrapping to the ones before it.
        let target = if end_idx < pane.tabs.len() {
            end_idx
        } else {
            start_idx.saturating_sub(1)
        };
        let text = format!(" +{hidden} ");
        let end = col
            .saturating_add(UnicodeWidthStr::width(text.as_str()) as u16)
            .min(strip_end);
        cells.push(TabCell {
            focus_id: pane.tabs[target].id.clone(),
            text,
            active: false,
            overflow: true,
            start: col,
            end,
        });
    }
    cells
}

#[cfg(test)]
fn draw_pane_tab_strip(
    frame: &mut ratatui::Frame,
    pane: &crate::model::Pane,
    strip: Rect,
    palette: ThemePalette,
) {
    let cells = pane_tab_cells(pane, strip);
    if cells.is_empty() {
        return;
    }
    let spans = cells
        .iter()
        .map(|cell| {
            let style = if cell.active {
                Style::default()
                    .fg(palette.background)
                    .bg(palette.active)
                    .add_modifier(Modifier::BOLD)
            } else if cell.overflow {
                Style::default()
                    .fg(palette.warning)
                    .bg(palette.background)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(palette.muted).bg(palette.background)
            };
            Span::styled(cell.text.clone(), style)
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(palette.background)),
        strip,
    );
}

fn pane_control_buttons_for(
    layout: Option<&LayoutNode>,
    pane_id: &str,
) -> Vec<(PaneControlAction, &'static str)> {
    let mut buttons = vec![
        (PaneControlAction::SplitRight, "→"),
        (PaneControlAction::SplitDown, "↓"),
    ];
    if crate::model::can_move_pane(layout, pane_id, SplitDirection::Left) {
        buttons.push((PaneControlAction::MoveLeft, "‹"));
    }
    if crate::model::can_move_pane(layout, pane_id, SplitDirection::Right) {
        buttons.push((PaneControlAction::MoveRight, "›"));
    }
    if crate::model::can_move_pane(layout, pane_id, SplitDirection::Up) {
        buttons.push((PaneControlAction::MoveUp, "ˆ"));
    }
    if crate::model::can_move_pane(layout, pane_id, SplitDirection::Down) {
        buttons.push((PaneControlAction::MoveDown, "ˇ"));
    }
    buttons.push((PaneControlAction::Close, "×"));
    buttons
}

fn pane_control_rects(
    area: Rect,
    layout: Option<&LayoutNode>,
    pane_id: &str,
) -> Vec<(PaneControlAction, Rect)> {
    let buttons = pane_control_buttons_for(layout, pane_id);
    if area.width < 10 || area.height == 0 || buttons.is_empty() {
        return Vec::new();
    }
    let width = (buttons.len() as u16).saturating_mul(2).max(2);
    let start = area.x.saturating_add(area.width).saturating_sub(width + 1);
    buttons
        .into_iter()
        .enumerate()
        .map(|(index, (action, _))| {
            (
                action,
                Rect::new(start.saturating_add(index as u16 * 2), area.y, 2, 1),
            )
        })
        .collect()
}

fn pane_control_at(
    snapshot: &Session,
    sidebar_collapsed: bool,
    sidebar_expanded: u16,
    terminal_width: u16,
    terminal_height: u16,
    column: u16,
    row: u16,
) -> Option<PaneControlHit> {
    let active = snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.id == snapshot.active_workspace)?;
    if active.panes.is_empty() {
        return None;
    }
    let area = pane_area(
        sidebar_collapsed,
        sidebar_expanded,
        terminal_width,
        terminal_height,
    );
    if !point_in_rect(area, column, row) {
        return None;
    }
    let layout = active.layout.as_ref();
    if let Some(zoomed) = active
        .zoomed_pane
        .as_ref()
        .filter(|pane| active.panes.iter().any(|item| item == *pane))
    {
        return pane_control_hit_in_area(zoomed, layout, area, column, row);
    }
    pane_control_at_layout(layout, layout, area, column, row)
}

fn pane_control_at_layout(
    node: Option<&LayoutNode>,
    full_layout: Option<&LayoutNode>,
    area: Rect,
    column: u16,
    row: u16,
) -> Option<PaneControlHit> {
    if !point_in_rect(area, column, row) {
        return None;
    }
    let node = node?;
    match node {
        LayoutNode::Pane { pane } => pane_control_hit_in_area(pane, full_layout, area, column, row),
        LayoutNode::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            let ratio = (*ratio).clamp(15, 85) as u32;
            match axis {
                SplitAxis::Horizontal => {
                    let split = area.x + ((area.width as u32 * ratio) / 100) as u16;
                    if column < split {
                        let first_area =
                            Rect::new(area.x, area.y, split.saturating_sub(area.x), area.height);
                        pane_control_at_layout(Some(first), full_layout, first_area, column, row)
                    } else {
                        let second_area = Rect::new(
                            split,
                            area.y,
                            area.x.saturating_add(area.width).saturating_sub(split),
                            area.height,
                        );
                        pane_control_at_layout(Some(second), full_layout, second_area, column, row)
                    }
                }
                SplitAxis::Vertical => {
                    let split = area.y + ((area.height as u32 * ratio) / 100) as u16;
                    if row < split {
                        let first_area =
                            Rect::new(area.x, area.y, area.width, split.saturating_sub(area.y));
                        pane_control_at_layout(Some(first), full_layout, first_area, column, row)
                    } else {
                        let second_area = Rect::new(
                            area.x,
                            split,
                            area.width,
                            area.y.saturating_add(area.height).saturating_sub(split),
                        );
                        pane_control_at_layout(Some(second), full_layout, second_area, column, row)
                    }
                }
            }
        }
    }
}

fn pane_control_hit_in_area(
    pane: &str,
    layout: Option<&LayoutNode>,
    area: Rect,
    column: u16,
    row: u16,
) -> Option<PaneControlHit> {
    pane_control_rects(area, layout, pane)
        .into_iter()
        .find_map(|(action, rect)| {
            point_in_rect(rect, column, row).then(|| PaneControlHit {
                pane: pane.to_string(),
                action,
            })
        })
}

fn draw_pane_controls(
    frame: &mut ratatui::Frame,
    area: Rect,
    pane_id: &str,
    layout: Option<&LayoutNode>,
    hover: Option<&PaneControlHit>,
    theme: UiTheme,
) {
    let palette = theme.palette();
    let buttons = pane_control_buttons_for(layout, pane_id);
    let rects = pane_control_rects(area, layout, pane_id);
    if rects.is_empty() {
        return;
    }
    let spans = rects
        .into_iter()
        .zip(buttons)
        .flat_map(|((action, _), (_, label))| {
            let hovered = hover
                .map(|hit| hit.pane == pane_id && hit.action == action)
                .unwrap_or(false);
            let style = if hovered {
                Style::default()
                    .fg(Color::Black)
                    .bg(palette.hover)
                    .add_modifier(Modifier::BOLD)
            } else if action == PaneControlAction::Close {
                Style::default()
                    .fg(palette.danger)
                    .bg(palette.background)
                    .add_modifier(Modifier::BOLD)
            } else if matches!(
                action,
                PaneControlAction::SplitRight | PaneControlAction::SplitDown
            ) {
                Style::default()
                    .fg(palette.success)
                    .bg(palette.background)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(palette.active)
                    .bg(palette.background)
                    .add_modifier(Modifier::BOLD)
            };
            [Span::styled(label, style), Span::raw(" ")].into_iter()
        })
        .collect::<Vec<_>>();
    let n = pane_control_buttons_for(layout, pane_id).len() as u16;
    let rect = pane_control_rects(area, layout, pane_id)
        .first()
        .map(|(_, rect)| Rect::new(rect.x, rect.y, n.saturating_mul(2).max(2), 1))
        .unwrap_or(area);
    frame.render_widget(Paragraph::new(Line::from(spans)), rect);
}

fn adjust_scroll(offsets: &mut BTreeMap<String, usize>, pane: &str, delta: isize) {
    let current = offsets.get(pane).copied().unwrap_or(0);
    let next = if delta.is_negative() {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        current.saturating_add(delta as usize)
    };
    if next == 0 {
        offsets.remove(pane);
    } else {
        offsets.insert(pane.to_string(), next.min(10_000));
    }
}

fn pane_text_for_view(pane: &crate::model::Pane, height: usize, scroll_offset: usize) -> String {
    if scroll_offset == 0 {
        return tail_lines(&pane.output, height.max(1));
    }
    let cleaned = strip_ansi(&pane.scrollback);
    let lines = cleaned.lines().collect::<Vec<_>>();
    if lines.is_empty() {
        return String::new();
    }
    let end = lines.len().saturating_sub(scroll_offset).max(1);
    let start = end.saturating_sub(height.max(1));
    lines[start..end].join("\n")
}

/// Slice the styled scrollback (`pane.scrollback_formatted`) to the same
/// line window the plain [`pane_text_for_view`] path computes for a scrolled
/// view, keeping the SGR escapes intact so [`ansi_to_lines`] can colorize them.
/// The index math mirrors `pane_text_for_view` exactly so switching between the
/// colored and plain paths does not jump.
fn scrollback_formatted_for_view(
    pane: &crate::model::Pane,
    height: usize,
    scroll_offset: usize,
) -> String {
    let lines = pane.scrollback_formatted.lines().collect::<Vec<_>>();
    if lines.is_empty() {
        return String::new();
    }
    let end = lines.len().saturating_sub(scroll_offset).max(1);
    let start = end.saturating_sub(height.max(1));
    lines[start..end].join("\n")
}

fn pane_lines_for_view(
    pane: &crate::model::Pane,
    height: usize,
    scroll_offset: usize,
    selection: Option<&TextSelection>,
    content_area: Rect,
    palette: ThemePalette,
    cursor_blink_on: bool,
) -> Vec<Line<'static>> {
    let view_height = height.max(1);
    let mut lines = if scroll_offset == 0 && !pane.output_formatted.is_empty() {
        ansi_to_lines(&tail_lines(&pane.output_formatted, view_height))
    } else if scroll_offset > 0 && !pane.scrollback_formatted.is_empty() {
        // Scrolled view with styled history available: slice the same
        // oldest→newest line window the plain path computes, but from the
        // colored scrollback, so scrolled content keeps its colors.
        ansi_to_lines(&scrollback_formatted_for_view(
            pane,
            view_height,
            scroll_offset,
        ))
    } else {
        pane_text_for_view(pane, view_height, scroll_offset)
            .lines()
            .map(|line| Line::from(line.to_string()))
            .collect()
    };
    if let Some(selection) = selection.filter(|_| scroll_offset == 0) {
        lines = highlight_selection_lines(lines, selection, content_area, palette);
    }
    // Active pane only: paint a bright block at the PTY cursor (caller gates blink).
    if scroll_offset == 0 && cursor_blink_on {
        apply_cursor_marker(
            &mut lines,
            pane.cursor_row,
            pane.cursor_col,
            pane.screen_rows,
            view_height,
            palette,
        );
    }
    lines
}

/// Map a PTY screen-row cursor into the pane content view (bottom-aligned live
/// window). Returns `None` when the caret is above the visible slice.
fn cursor_view_row(
    cursor_row: u16,
    screen_rows: Option<u16>,
    view_height: usize,
    content_line_count: usize,
) -> Option<usize> {
    let crow = cursor_row as usize;
    // Prefer the real PTY grid height; fall back to content length or view.
    let screen_rows = screen_rows
        .map(|r| r as usize)
        .filter(|r| *r > 0)
        .unwrap_or_else(|| {
            content_line_count
                .max(view_height)
                .max(crow.saturating_add(1))
        });
    // Live view shows the bottom `view_height` rows of the PTY screen.
    let first_visible = screen_rows.saturating_sub(view_height);
    if crow < first_visible {
        return None;
    }
    Some(crow - first_visible)
}

/// Bright block-mark the cell under the PTY cursor (active pane only).
fn apply_cursor_marker(
    lines: &mut Vec<Line<'static>>,
    cursor_row: Option<u16>,
    cursor_col: Option<u16>,
    screen_rows: Option<u16>,
    view_height: usize,
    palette: ThemePalette,
) {
    let (Some(crow), Some(ccol)) = (cursor_row, cursor_col) else {
        return;
    };
    // Pad so trailing empty PTY rows (stripped from contents) still exist for
    // a cursor sitting on a blank prompt line at the bottom of the screen.
    while lines.len() < view_height {
        lines.push(Line::from(""));
    }
    let Some(view_row) = cursor_view_row(crow, screen_rows, view_height, lines.len()) else {
        return;
    };
    while lines.len() <= view_row {
        lines.push(Line::from(""));
    }
    if view_row >= lines.len() {
        return;
    }
    let line = std::mem::replace(&mut lines[view_row], Line::from(""));
    lines[view_row] = invert_cell_at_column(line, ccol as usize, palette);
}

/// Absolute host-terminal position for the active pane's PTY caret, if visible.
fn host_cursor_position(
    pane: &crate::model::Pane,
    content: Rect,
    scroll_offset: usize,
) -> Option<(u16, u16)> {
    if scroll_offset != 0 || content.width == 0 || content.height == 0 {
        return None;
    }
    let (crow, ccol) = (pane.cursor_row?, pane.cursor_col?);
    let view_height = content.height as usize;
    let view_row = cursor_view_row(crow, pane.screen_rows, view_height, view_height)?;
    if view_row >= view_height {
        return None;
    }
    let max_col = content.width.saturating_sub(1);
    let col = (ccol).min(max_col);
    Some((
        content.x.saturating_add(col),
        content.y.saturating_add(view_row as u16),
    ))
}

/// Reverse-video / block-mark the display cell at column `col` (0-based,
/// unicode-width aware). Pads the line when the cursor sits past its end.
fn invert_cell_at_column(line: Line<'static>, col: usize, palette: ThemePalette) -> Line<'static> {
    let mut cells: Vec<(char, Style)> = Vec::new();
    for span in line.spans {
        let style = span.style;
        for ch in span.content.chars() {
            cells.push((ch, style));
        }
    }
    let mut display_col = 0usize;
    let mut cursor_idx = None;
    for (idx, (ch, _)) in cells.iter().enumerate() {
        let w = UnicodeWidthChar::width(*ch).unwrap_or(0).max(1);
        if display_col == col || (display_col < col && display_col + w > col) {
            cursor_idx = Some(idx);
            break;
        }
        display_col += w;
    }
    if cursor_idx.is_none() {
        while display_col < col {
            cells.push((' ', Style::default()));
            display_col += 1;
        }
        cells.push((' ', Style::default()));
        cursor_idx = Some(cells.len() - 1);
    }
    let idx = cursor_idx.expect("cursor index set");
    let (ch, _style) = cells[idx];
    // Bright theme accent block — never reverse-video black (apps often use
    // black fg which made the old swap-style caret invisible).
    let paint_style = Style::default()
        .fg(palette.on_cursor)
        .bg(palette.cursor)
        .add_modifier(Modifier::BOLD);
    let paint_ch = if ch == ' ' || ch == '\t' { '█' } else { ch };
    cells[idx] = (paint_ch, paint_style);

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut run_style: Option<Style> = None;
    for (ch, style) in cells {
        if run_style != Some(style) {
            if let Some(prev) = run_style {
                if !buf.is_empty() {
                    spans.push(Span::styled(std::mem::take(&mut buf), prev));
                }
            }
            run_style = Some(style);
        }
        buf.push(ch);
    }
    if let Some(prev) = run_style {
        if !buf.is_empty() {
            spans.push(Span::styled(buf, prev));
        }
    }
    if spans.is_empty() {
        spans.push(Span::styled(
            "█".to_string(),
            Style::default()
                .fg(palette.active)
                .bg(palette.surface_alt)
                .add_modifier(Modifier::BOLD),
        ));
    }
    Line::from(spans)
}

fn ansi_to_lines(text: &str) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut plain = String::new();
    let mut style = Style::default();
    let mut col = 0_usize;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            flush_span(&mut spans, &mut plain, style);
            if matches!(chars.peek(), Some('[')) {
                chars.next();
                let mut sequence = String::new();
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        if next == 'm' {
                            style = apply_sgr(&sequence, style);
                        } else if next == 'C' {
                            let count = csi_count(&sequence);
                            plain.push_str(&" ".repeat(count));
                            col = col.saturating_add(count);
                        } else if matches!(next, 'G' | '`') {
                            let target = csi_count(&sequence).saturating_sub(1);
                            if target > col {
                                let count = target.saturating_sub(col);
                                plain.push_str(&" ".repeat(count));
                                col = target;
                            }
                        }
                        break;
                    }
                    sequence.push(next);
                }
            }
            continue;
        }
        match ch {
            '\r' => {}
            '\n' => {
                flush_span(&mut spans, &mut plain, style);
                lines.push(Line::from(std::mem::take(&mut spans)));
                col = 0;
            }
            _ => {
                plain.push(ch);
                col = col.saturating_add(ch.width().unwrap_or(0).max(1));
            }
        }
    }
    flush_span(&mut spans, &mut plain, style);
    lines.push(Line::from(spans));
    lines
}

fn csi_count(sequence: &str) -> usize {
    sequence
        .split([';', ':'])
        .find_map(|part| {
            let digits = part
                .chars()
                .filter(|ch| ch.is_ascii_digit())
                .collect::<String>();
            digits.parse::<usize>().ok().filter(|value| *value > 0)
        })
        .unwrap_or(1)
}

fn flush_span(spans: &mut Vec<Span<'static>>, plain: &mut String, style: Style) {
    if plain.is_empty() {
        return;
    }
    spans.push(Span::styled(std::mem::take(plain), style));
}

fn highlight_selection_lines(
    lines: Vec<Line<'static>>,
    selection: &TextSelection,
    content_area: Rect,
    palette: ThemePalette,
) -> Vec<Line<'static>> {
    let Some((start_col, start_row, end_col, end_row)) =
        selection_range_in_content(selection, content_area)
    else {
        return lines;
    };
    lines
        .into_iter()
        .enumerate()
        .map(|(row, line)| {
            let row = row as u16;
            if row < start_row || row > end_row {
                return line;
            }
            let line_start = if row == start_row { start_col } else { 0 };
            let line_end = if row == end_row { end_col } else { u16::MAX };
            highlight_line_range(line, line_start, line_end, palette)
        })
        .collect()
}

fn highlight_line_range(
    line: Line<'static>,
    start: u16,
    end: u16,
    palette: ThemePalette,
) -> Line<'static> {
    let mut spans = Vec::new();
    let mut col = 0_u16;
    for span in line.spans {
        let style = span.style;
        let mut plain = String::new();
        let mut highlighted = None;
        for ch in span.content.chars() {
            let is_selected = col >= start && col <= end;
            if highlighted != Some(is_selected) {
                if !plain.is_empty() {
                    let final_style = if highlighted == Some(true) {
                        style.bg(palette.selection).fg(palette.on_bright)
                    } else {
                        style
                    };
                    spans.push(Span::styled(std::mem::take(&mut plain), final_style));
                }
                highlighted = Some(is_selected);
            }
            plain.push(ch);
            col = col.saturating_add(1);
        }
        if !plain.is_empty() {
            let final_style = if highlighted == Some(true) {
                style.bg(palette.selection).fg(palette.on_bright)
            } else {
                style
            };
            spans.push(Span::styled(plain, final_style));
        }
    }
    Line::from(spans)
}

fn selection_range_in_content(
    selection: &TextSelection,
    content_area: Rect,
) -> Option<(u16, u16, u16, u16)> {
    if content_area.width == 0 || content_area.height == 0 {
        return None;
    }
    let (start_col, start_row) =
        selection_point_in_content(selection.start_col, selection.start_row, content_area);
    let (end_col, end_row) =
        selection_point_in_content(selection.end_col, selection.end_row, content_area);
    if start_row < end_row || (start_row == end_row && start_col <= end_col) {
        Some((start_col, start_row, end_col, end_row))
    } else {
        Some((end_col, end_row, start_col, start_row))
    }
}

fn selection_point_in_content(column: u16, row: u16, content_area: Rect) -> (u16, u16) {
    let max_col = content_area.width.saturating_sub(1);
    let max_row = content_area.height.saturating_sub(1);
    let col = column.saturating_sub(content_area.x).min(max_col);
    let row = row.saturating_sub(content_area.y).min(max_row);
    (col, row)
}

fn selected_text_from_pane(
    pane: &crate::model::Pane,
    content_area: Rect,
    selection: &TextSelection,
) -> Option<String> {
    let (start_col, start_row, end_col, end_row) =
        selection_range_in_content(selection, content_area)?;
    let lines = pane.output.lines().collect::<Vec<_>>();
    let mut selected = Vec::new();
    for row in start_row..=end_row {
        let Some(line) = lines.get(row as usize) else {
            continue;
        };
        let from = if row == start_row {
            start_col as usize
        } else {
            0
        };
        let to = if row == end_row {
            end_col as usize + 1
        } else {
            line.chars().count()
        };
        let text = line
            .chars()
            .skip(from)
            .take(to.saturating_sub(from))
            .collect::<String>();
        selected.push(text);
    }
    Some(selected.join("\n"))
}

fn apply_sgr(sequence: &str, mut style: Style) -> Style {
    let codes = parse_sgr_codes(sequence);
    let mut index = 0;
    while index < codes.len() {
        match codes[index] {
            0 => style = Style::default(),
            1 => style = style.add_modifier(Modifier::BOLD),
            2 => style = style.add_modifier(Modifier::DIM),
            3 => style = style.add_modifier(Modifier::ITALIC),
            4 => style = style.add_modifier(Modifier::UNDERLINED),
            22 => style = style.remove_modifier(Modifier::BOLD | Modifier::DIM),
            23 => style = style.remove_modifier(Modifier::ITALIC),
            24 => style = style.remove_modifier(Modifier::UNDERLINED),
            30..=37 => style = style.fg(ansi_index_color((codes[index] - 30) as u8)),
            39 => style = style.fg(Color::Reset),
            40..=47 => style = style.bg(ansi_index_color((codes[index] - 40) as u8)),
            49 => style = style.bg(Color::Reset),
            90..=97 => style = style.fg(ansi_bright_color((codes[index] - 90) as u8)),
            100..=107 => style = style.bg(ansi_bright_color((codes[index] - 100) as u8)),
            38 | 48 => {
                let is_fg = codes[index] == 38;
                if codes.get(index + 1) == Some(&5) {
                    if let Some(color) = codes
                        .get(index + 2)
                        .and_then(|value| u8::try_from(*value).ok())
                    {
                        style = if is_fg {
                            style.fg(Color::Indexed(color))
                        } else {
                            style.bg(Color::Indexed(color))
                        };
                    }
                    index += 2;
                } else if codes.get(index + 1) == Some(&2) {
                    if let (Some(r), Some(g), Some(b)) = (
                        codes
                            .get(index + 2)
                            .and_then(|value| u8::try_from(*value).ok()),
                        codes
                            .get(index + 3)
                            .and_then(|value| u8::try_from(*value).ok()),
                        codes
                            .get(index + 4)
                            .and_then(|value| u8::try_from(*value).ok()),
                    ) {
                        style = if is_fg {
                            style.fg(Color::Rgb(r, g, b))
                        } else {
                            style.bg(Color::Rgb(r, g, b))
                        };
                    }
                    index += 4;
                }
            }
            _ => {}
        }
        index += 1;
    }
    style
}

fn parse_sgr_codes(sequence: &str) -> Vec<u16> {
    if sequence.trim().is_empty() {
        return vec![0];
    }
    sequence
        .split([';', ':'])
        .filter(|part| !part.is_empty())
        .map(|part| part.parse::<u16>().unwrap_or(0))
        .collect::<Vec<_>>()
}

fn ansi_index_color(index: u8) -> Color {
    match index {
        0 => Color::Black,
        1 => Color::Red,
        2 => Color::Green,
        3 => Color::Yellow,
        4 => Color::Blue,
        5 => Color::Magenta,
        6 => Color::Cyan,
        _ => Color::Gray,
    }
}

fn ansi_bright_color(index: u8) -> Color {
    match index {
        0 => Color::DarkGray,
        1 => Color::LightRed,
        2 => Color::LightGreen,
        3 => Color::LightYellow,
        4 => Color::LightBlue,
        5 => Color::LightMagenta,
        6 => Color::LightCyan,
        _ => Color::White,
    }
}

fn pane_title_text(pane: &crate::model::Pane, scroll_offset: usize) -> String {
    let mut title = format!(
        " {}{} [{}]",
        surface_prefix(&pane.surface_kind),
        pane.title,
        status_label(&pane.agent_status)
    );
    if let Some(progress) = pane.progress {
        title.push_str(&format!(" {progress}%"));
    }
    if let Some(color) = &pane.notification_color {
        title.push_str(&format!(" {color}"));
    }
    if let Some(message) = &pane.notification_message {
        title.push_str(&format!(": {}", trim_label(message, 28)));
    }
    if scroll_offset > 0 {
        title.push_str(&format!(" scroll:{scroll_offset}"));
    }
    title.push(' ');
    title
}

#[cfg(test)]
fn active_pane_id(snapshot: &Session) -> Option<String> {
    snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.id == snapshot.active_workspace)
        .and_then(|workspace| workspace.active_pane.clone())
}

#[cfg(test)]
fn relative_pane_tab(pane: &crate::model::Pane, delta: isize) -> Option<String> {
    if pane.tabs.len() <= 1 {
        return None;
    }
    let current = pane
        .active_tab
        .as_deref()
        .and_then(|active| pane.tabs.iter().position(|tab| tab.id == active))
        .unwrap_or(0);
    let len = pane.tabs.len() as isize;
    let next = (current as isize + delta).rem_euclid(len) as usize;
    pane.tabs.get(next).map(|tab| tab.id.clone())
}

fn tail_lines(text: &str, height: usize) -> String {
    let lines = text.lines().collect::<Vec<_>>();
    let start = lines.len().saturating_sub(height);
    lines[start..].join("\n")
}

fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            if matches!(chars.peek(), Some('[')) {
                chars.next();
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            continue;
        }
        if ch != '\r' {
            out.push(ch);
        }
    }
    out
}

fn fallback_grid(area: Rect, panes: usize) -> Vec<Rect> {
    let rows = if panes <= 2 { 1 } else { 2 };
    let cols = if panes == 1 { 1 } else { 2 };
    let row_constraints = vec![Constraint::Ratio(1, rows as u32); rows];
    let col_constraints = vec![Constraint::Ratio(1, cols as u32); cols];
    let row_areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints(row_constraints)
        .split(area);
    let mut areas = Vec::with_capacity(panes);
    for row in 0..rows {
        let col_areas = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(col_constraints.clone())
            .split(row_areas[row]);
        for col in 0..cols {
            if areas.len() < panes {
                areas.push(col_areas[col]);
            }
        }
    }
    areas
}

fn pane_at(node: Option<&LayoutNode>, area: Rect, column: u16, row: u16) -> Option<String> {
    if !point_in_rect(area, column, row) {
        return None;
    }
    let node = node?;
    match node {
        LayoutNode::Pane { pane } => Some(pane.clone()),
        LayoutNode::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            let ratio = (*ratio).clamp(15, 85) as u32;
            match axis {
                SplitAxis::Horizontal => {
                    let split = area.x + ((area.width as u32 * ratio) / 100) as u16;
                    if column < split {
                        let first_area =
                            Rect::new(area.x, area.y, split.saturating_sub(area.x), area.height);
                        pane_at(Some(first), first_area, column, row)
                    } else {
                        let second_area = Rect::new(
                            split,
                            area.y,
                            area.x.saturating_add(area.width).saturating_sub(split),
                            area.height,
                        );
                        pane_at(Some(second), second_area, column, row)
                    }
                }
                SplitAxis::Vertical => {
                    let split = area.y + ((area.height as u32 * ratio) / 100) as u16;
                    if row < split {
                        let first_area =
                            Rect::new(area.x, area.y, area.width, split.saturating_sub(area.y));
                        pane_at(Some(first), first_area, column, row)
                    } else {
                        let second_area = Rect::new(
                            area.x,
                            split,
                            area.width,
                            area.y.saturating_add(area.height).saturating_sub(split),
                        );
                        pane_at(Some(second), second_area, column, row)
                    }
                }
            }
        }
    }
}

fn pane_area_at(
    node: Option<&LayoutNode>,
    area: Rect,
    column: u16,
    row: u16,
) -> Option<(String, Rect)> {
    if !point_in_rect(area, column, row) {
        return None;
    }
    let node = node?;
    match node {
        LayoutNode::Pane { pane } => Some((pane.clone(), area)),
        LayoutNode::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            let ratio = (*ratio).clamp(15, 85) as u32;
            match axis {
                SplitAxis::Horizontal => {
                    let split = area.x + ((area.width as u32 * ratio) / 100) as u16;
                    if column < split {
                        let first_area =
                            Rect::new(area.x, area.y, split.saturating_sub(area.x), area.height);
                        pane_area_at(Some(first), first_area, column, row)
                    } else {
                        let second_area = Rect::new(
                            split,
                            area.y,
                            area.x.saturating_add(area.width).saturating_sub(split),
                            area.height,
                        );
                        pane_area_at(Some(second), second_area, column, row)
                    }
                }
                SplitAxis::Vertical => {
                    let split = area.y + ((area.height as u32 * ratio) / 100) as u16;
                    if row < split {
                        let first_area =
                            Rect::new(area.x, area.y, area.width, split.saturating_sub(area.y));
                        pane_area_at(Some(first), first_area, column, row)
                    } else {
                        let second_area = Rect::new(
                            area.x,
                            split,
                            area.width,
                            area.y.saturating_add(area.height).saturating_sub(split),
                        );
                        pane_area_at(Some(second), second_area, column, row)
                    }
                }
            }
        }
    }
}

fn pane_area_by_id(node: Option<&LayoutNode>, area: Rect, pane_id: &str) -> Option<Rect> {
    let node = node?;
    match node {
        LayoutNode::Pane { pane } if pane == pane_id => Some(area),
        LayoutNode::Pane { .. } => None,
        LayoutNode::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            let ratio = (*ratio).clamp(15, 85) as u32;
            match axis {
                SplitAxis::Horizontal => {
                    let split = area.x + ((area.width as u32 * ratio) / 100) as u16;
                    let first_area =
                        Rect::new(area.x, area.y, split.saturating_sub(area.x), area.height);
                    let second_area = Rect::new(
                        split,
                        area.y,
                        area.x.saturating_add(area.width).saturating_sub(split),
                        area.height,
                    );
                    pane_area_by_id(Some(first), first_area, pane_id)
                        .or_else(|| pane_area_by_id(Some(second), second_area, pane_id))
                }
                SplitAxis::Vertical => {
                    let split = area.y + ((area.height as u32 * ratio) / 100) as u16;
                    let first_area =
                        Rect::new(area.x, area.y, area.width, split.saturating_sub(area.y));
                    let second_area = Rect::new(
                        area.x,
                        split,
                        area.width,
                        area.y.saturating_add(area.height).saturating_sub(split),
                    );
                    pane_area_by_id(Some(first), first_area, pane_id)
                        .or_else(|| pane_area_by_id(Some(second), second_area, pane_id))
                }
            }
        }
    }
}

/// Whether the pane shows its own tab strip row (only when it has >1 tab).
fn pane_has_tab_strip(pane: &crate::model::Pane) -> bool {
    pane.tabs.len() > 1
}

/// Rectangle of the dedicated tab strip row (first row inside the block border).
#[cfg(test)]
fn pane_tab_strip_area(area: Rect) -> Rect {
    Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        1,
    )
}

/// Content rectangle inside the pane block. When a tab strip is present the top
/// is pushed down one extra row so the PTY content never overlaps the strip.
fn pane_content_area(area: Rect, has_strip: bool) -> Rect {
    let top = if has_strip { 2 } else { 1 };
    Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(top),
        area.width.saturating_sub(2),
        area.height.saturating_sub(top + 1),
    )
}

fn sgr_mouse_sequence(button: MouseButtonCode, x: u16, y: u16, pressed: bool) -> String {
    let code = match button {
        MouseButtonCode::Left => 0,
        MouseButtonCode::LeftDrag => 32,
        MouseButtonCode::WheelUp => 64,
        MouseButtonCode::WheelDown => 65,
    };
    let suffix = if pressed { 'M' } else { 'm' };
    format!("\x1b[<{code};{x};{y}{suffix}")
}

#[cfg(test)]
fn pane_tab_at(
    snapshot: &Session,
    node: Option<&LayoutNode>,
    area: Rect,
    column: u16,
    row: u16,
) -> Option<(String, String)> {
    if !point_in_rect(area, column, row) {
        return None;
    }
    let node = node?;
    match node {
        LayoutNode::Pane { pane } => snapshot
            .panes
            .get(pane)
            .and_then(|item| pane_tab_hit(item, area, column, row))
            .map(|tab| (pane.clone(), tab)),
        LayoutNode::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            let ratio = (*ratio).clamp(15, 85) as u32;
            match axis {
                SplitAxis::Horizontal => {
                    let split = area.x + ((area.width as u32 * ratio) / 100) as u16;
                    if column < split {
                        let first_area =
                            Rect::new(area.x, area.y, split.saturating_sub(area.x), area.height);
                        pane_tab_at(snapshot, Some(first), first_area, column, row)
                    } else {
                        let second_area = Rect::new(
                            split,
                            area.y,
                            area.x.saturating_add(area.width).saturating_sub(split),
                            area.height,
                        );
                        pane_tab_at(snapshot, Some(second), second_area, column, row)
                    }
                }
                SplitAxis::Vertical => {
                    let split = area.y + ((area.height as u32 * ratio) / 100) as u16;
                    if row < split {
                        let first_area =
                            Rect::new(area.x, area.y, area.width, split.saturating_sub(area.y));
                        pane_tab_at(snapshot, Some(first), first_area, column, row)
                    } else {
                        let second_area = Rect::new(
                            area.x,
                            split,
                            area.width,
                            area.y.saturating_add(area.height).saturating_sub(split),
                        );
                        pane_tab_at(snapshot, Some(second), second_area, column, row)
                    }
                }
            }
        }
    }
}

#[cfg(test)]
fn pane_tab_hit(pane: &crate::model::Pane, area: Rect, column: u16, row: u16) -> Option<String> {
    if !pane_has_tab_strip(pane) || area.width <= 2 || area.height < 3 {
        return None;
    }
    let strip = pane_tab_strip_area(area);
    if row != strip.y {
        return None;
    }
    pane_tab_cells(pane, strip)
        .into_iter()
        .find(|cell| column >= cell.start && column < cell.end)
        .map(|cell| cell.focus_id)
}

fn split_axis_at(
    node: Option<&LayoutNode>,
    area: Rect,
    column: u16,
    row: u16,
) -> Option<SplitAxis> {
    if !point_in_rect(area, column, row) {
        return None;
    }
    let node = node?;
    let LayoutNode::Split {
        axis,
        ratio,
        first,
        second,
    } = node
    else {
        return None;
    };
    let ratio = (*ratio).clamp(15, 85) as u32;
    match axis {
        SplitAxis::Horizontal => {
            let split = area.x + ((area.width as u32 * ratio) / 100) as u16;
            if column == split || column.saturating_add(1) == split {
                return Some(SplitAxis::Horizontal);
            }
            let first_area = Rect::new(area.x, area.y, split.saturating_sub(area.x), area.height);
            let second_area = Rect::new(
                split,
                area.y,
                area.x.saturating_add(area.width).saturating_sub(split),
                area.height,
            );
            split_axis_at(Some(first), first_area, column, row)
                .or_else(|| split_axis_at(Some(second), second_area, column, row))
        }
        SplitAxis::Vertical => {
            let split = area.y + ((area.height as u32 * ratio) / 100) as u16;
            if row == split || row.saturating_add(1) == split {
                return Some(SplitAxis::Vertical);
            }
            let first_area = Rect::new(area.x, area.y, area.width, split.saturating_sub(area.y));
            let second_area = Rect::new(
                area.x,
                split,
                area.width,
                area.y.saturating_add(area.height).saturating_sub(split),
            );
            split_axis_at(Some(first), first_area, column, row)
                .or_else(|| split_axis_at(Some(second), second_area, column, row))
        }
    }
}

fn resize_drag_direction(
    axis: SplitAxis,
    from_column: u16,
    from_row: u16,
    to_column: u16,
    to_row: u16,
) -> Option<SplitDirection> {
    match axis {
        SplitAxis::Horizontal => match to_column.cmp(&from_column) {
            std::cmp::Ordering::Greater => Some(SplitDirection::Right),
            std::cmp::Ordering::Less => Some(SplitDirection::Left),
            std::cmp::Ordering::Equal => None,
        },
        SplitAxis::Vertical => match to_row.cmp(&from_row) {
            std::cmp::Ordering::Greater => Some(SplitDirection::Down),
            std::cmp::Ordering::Less => Some(SplitDirection::Up),
            std::cmp::Ordering::Equal => None,
        },
    }
}

fn point_in_rect(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x
        && column < area.x.saturating_add(area.width)
        && row >= area.y
        && row < area.y.saturating_add(area.height)
}

/// Aggregate agent activity across panes in a workspace for the sidebar.
///
/// Emoji markers (priority: error → needs input → running → done):
/// - ❌ error / failed
/// - 🙋 needs input / attention / approval
/// - 🔄 task running (busy)
/// - ✅ task finished
///
/// A trailing count is included when more than one pane shares that state.
fn workspace_status(snapshot: &Session, pane_ids: &[String], markers: &str) -> String {
    if markers.eq_ignore_ascii_case("off") {
        return String::new();
    }
    let mut busy = 0;
    let mut attention = 0;
    let mut done = 0;
    let mut error = 0;
    for pane_id in pane_ids {
        let Some(pane) = snapshot.panes.get(pane_id) else {
            continue;
        };
        // Prefer explicit agent_status. Only use notification banner as
        // attention when status is not already a settled emoji state.
        match status_label(&pane.agent_status) {
            "busy" => busy += 1,
            "attention" => attention += 1,
            "done" => done += 1,
            "error" => error += 1,
            // Idle/unknown: optional bare notification still means needs input.
            _ if pane.notification_message.is_some() => attention += 1,
            // Do NOT force 🔄 for every running coding agent — Claude/Codex stay
            // Running after a turn ends; pinned Done/Idle must show correctly.
            _ => {}
        }
    }
    let ascii = markers.eq_ignore_ascii_case("ascii");
    if error > 0 {
        status_marker(if ascii { "!" } else { "❌" }, error)
    } else if attention > 0 {
        status_marker(if ascii { "?" } else { "🙋" }, attention)
    } else if busy > 0 {
        status_marker(if ascii { "*" } else { "🔄" }, busy)
    } else if done > 0 {
        status_marker(if ascii { "+" } else { "✅" }, done)
    } else {
        String::new()
    }
}

fn status_marker(marker: &str, count: usize) -> String {
    if count > 1 {
        format!("{marker}{count}")
    } else {
        marker.to_string()
    }
}

/// Heuristic: pane command looks like a coding agent CLI.
fn port_label(ports: &[crate::model::ListeningPort]) -> String {
    if ports.is_empty() {
        return String::new();
    }
    let joined = ports
        .iter()
        .take(3)
        .map(|port| port.port.to_string())
        .collect::<Vec<_>>()
        .join(",");
    if ports.len() > 3 {
        format!(" :{joined}+")
    } else {
        format!(" :{joined}")
    }
}

fn pull_request_label(pr: Option<&crate::model::PullRequestInfo>) -> String {
    let Some(pr) = pr else {
        return String::new();
    };
    let state = if pr.draft {
        "draft"
    } else {
        match pr.state.as_str() {
            "OPEN" => "open",
            "MERGED" => "merged",
            "CLOSED" => "closed",
            other => other,
        }
    };
    format!(" #{}:{state}", pr.number)
}

fn session_footer(snapshot: &Session, mode: UiMode, notification_selected: usize) -> String {
    let mut running = 0;
    let mut busy = 0;
    let mut attention = 0;
    let mut done = 0;
    let mut error = 0;
    for pane in snapshot.panes.values() {
        if matches!(pane.status, crate::model::PaneStatus::Running) {
            running += 1;
        }
        match status_label(&pane.agent_status) {
            "busy" => busy += 1,
            "attention" => attention += 1,
            "done" => done += 1,
            "error" => error += 1,
            _ => {}
        }
    }
    let notes = snapshot
        .notifications
        .iter()
        .rev()
        .filter(|note| !note.clear && !note.message.is_empty())
        .count();
    let mut base = format!(
        " session:{} workspaces:{} panes:{} running:{} busy:{} attention:{} done:{} error:{} notes:{} ",
        snapshot.name,
        snapshot.workspaces.len(),
        snapshot.panes.len(),
        running,
        busy,
        attention,
        done,
        error,
        notes
    );
    if let Some(zoomed) = snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.id == snapshot.active_workspace)
        .and_then(|workspace| workspace.zoomed_pane.as_deref())
    {
        base.push_str(&format!(" zoom:{} ", trim_label(zoomed, 16)));
    }
    match mode {
        UiMode::Panes => base,
        UiMode::Notifications => {
            let visible = snapshot.notifications.len().min(20);
            let selected = if visible == 0 {
                0
            } else {
                notification_selected.min(visible - 1) + 1
            };
            format!("{base} notifications:{selected}/{visible} j/k select Enter jump Esc close ")
        }
        UiMode::Actions => format!("{base} actions j/k select Enter run Esc close "),
        UiMode::Commands => {
            format!("{base} commands type to filter ↑/↓ select Enter run Esc clear/close ")
        }
        UiMode::Settings => {
            format!("{base} settings j/k select h/l or Enter change/install Esc close ")
        }
        UiMode::WorkspacePicker => {
            format!("{base} ☰ workspaces j/k select Enter open Esc close ")
        }
        UiMode::ContextMenu => format!("{base} pane-menu j/k select Enter run Esc close "),
    }
}

fn session_footer_with_modals(
    snapshot: &Session,
    mode: UiMode,
    notification_selected: usize,
    confirm: Option<&PendingConfirm>,
    rename: Option<&RenameDialog>,
) -> String {
    if rename.is_some() {
        return format!(
            "session:{}  rename  type name  [Enter] OK   [Esc] Cancel ",
            snapshot.name
        );
    }
    if confirm.is_some() {
        return format!(
            "session:{}  ⚠ confirm  [Y]/Enter Yes   [N]/Esc Cancel ",
            snapshot.name
        );
    }
    session_footer(snapshot, mode, notification_selected)
}

fn workspace_notification(
    snapshot: &Session,
    workspace_id: &str,
    pane_ids: &[String],
) -> Option<String> {
    snapshot
        .notifications
        .iter()
        .rev()
        .find(|note| note.workspace.as_deref() == Some(workspace_id))
        .and_then(|note| {
            if note.clear || note.message.is_empty() {
                None
            } else {
                Some(note.message.clone())
            }
        })
        .or_else(|| {
            pane_ids.iter().find_map(|pane_id| {
                snapshot
                    .panes
                    .get(pane_id)
                    .and_then(|pane| pane.notification_message.clone())
            })
        })
}

fn notification_panel_lines(snapshot: &Session, selected: usize) -> Vec<String> {
    snapshot
        .notifications
        .iter()
        .rev()
        .take(20)
        .enumerate()
        .map(|(index, note)| {
            let workspace = notification_workspace(snapshot, note);
            let workspace_name = workspace
                .as_ref()
                .map(|workspace| workspace.name.as_str())
                .unwrap_or("-");
            let pane_title = note
                .pane
                .as_ref()
                .and_then(|pane_id| snapshot.panes.get(pane_id))
                .map(|pane| pane.title.as_str())
                .unwrap_or("-");
            let status = note.status.as_deref().unwrap_or("-");
            let clear = if note.clear { " clear" } else { "" };
            let marker = if index == selected { "> " } else { "  " };
            format!(
                "{marker}{} [{}] ws:{} pane:{} status:{}{} {}",
                note.time,
                note.color.as_deref().unwrap_or("-"),
                trim_label(workspace_name, 14),
                trim_label(pane_title, 14),
                status,
                clear,
                trim_label(&note.message, 80)
            )
        })
        .collect()
}

fn action_panel_lines(actions: &[UiAction], selected: usize, error: Option<&str>) -> Vec<String> {
    if let Some(error) = error {
        return vec![format!("error: {error}")];
    }
    actions
        .iter()
        .enumerate()
        .map(|(index, action)| {
            let marker = if index == selected { "> " } else { "  " };
            let title = action
                .title
                .as_deref()
                .map(|title| format!(" title:{}", trim_label(title, 18)))
                .unwrap_or_default();
            let direction = action
                .direction
                .map(|direction| format!(" dir:{direction}"))
                .unwrap_or_default();
            format!(
                "{marker}{}{}{}  {}",
                trim_label(&action.name, 24),
                title,
                direction,
                trim_label(&action.command, 80)
            )
        })
        .collect()
}

#[cfg(test)]
fn agent_panel_entries(snapshot: &Session) -> Vec<AgentPanelEntry> {
    snapshot
        .workspaces
        .iter()
        .flat_map(|workspace| {
            workspace.panes.iter().filter_map(|pane_id| {
                let pane = snapshot.panes.get(pane_id)?;
                Some(AgentPanelEntry {
                    workspace_id: workspace.id.clone(),
                    workspace_name: workspace.name.clone(),
                    pane_id: pane.id.clone(),
                    title: pane.title.clone(),
                    command: pane.command.clone(),
                    surface: pane.surface_kind.clone(),
                    status: pane.status.clone(),
                    agent_status: pane.agent_status.clone(),
                    progress: pane.progress,
                    metadata: pane.metadata.clone(),
                })
            })
        })
        .collect()
}

#[cfg(test)]
fn agent_panel_lines(snapshot: &Session, selected: usize) -> Vec<String> {
    agent_panel_entries(snapshot)
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let marker = if index == selected { "> " } else { "  " };
            let progress = entry
                .progress
                .map(|progress| format!(" {progress}%"))
                .unwrap_or_default();
            let metadata = metadata_summary(&entry.metadata)
                .map(|metadata| format!(" {{{metadata}}}"))
                .unwrap_or_default();
            format!(
                "{marker}{} {}:{} [{}:{}:{}{}]{} {}",
                trim_label(&entry.workspace_name, 14),
                trim_label(&entry.pane_id, 10),
                trim_label(&entry.title, 18),
                surface_label(&entry.surface),
                pane_status_label(&entry.status),
                status_label(&entry.agent_status),
                progress,
                metadata,
                trim_label(&entry.command, 80)
            )
        })
        .collect()
}

/// Single source of truth mapping prefix-suffix keys to command-palette
/// actions. Both the prefix-key handler and the palette shortcut column are
/// driven by this table so the displayed shortcut can never drift from the
/// binding that actually runs. Each tuple is `(key code, display label,
/// action)`. Prefix keys with no palette action (detach `q`, sidebar `B`, open
/// palette `P`, jump-notification `u`, `Tab`, scroll `PageUp`/`PageDown`/
/// `Home`) are intentionally handled outside this table.
fn prefix_action_bindings() -> &'static [(KeyCode, &'static str, CommandPaletteAction)] {
    use CommandPaletteAction::*;
    &[
        (KeyCode::Char('%'), "%", SplitRight),
        (KeyCode::Char('"'), "\"", SplitDown),
        (KeyCode::Char('c'), "c", NewWorkspace),
        (KeyCode::Char('x'), "x", KillPane),
        (KeyCode::Char('w'), "w", CloseWorkspace),
        (KeyCode::Char('n'), "n", NextWorkspace),
        (KeyCode::Char('p'), "p", PreviousWorkspace),
        (KeyCode::Char('N'), "N", ToggleNotifications),
        (KeyCode::Char('A'), "A", ToggleActions),
        (KeyCode::Char('z'), "z", ToggleZoom),
        (KeyCode::Char(']'), "]", NextTab),
        (KeyCode::Char('['), "[", PreviousTab),
        (KeyCode::Char('t'), "t", NewTab),
        (KeyCode::Char('h'), "h", FocusLeft),
        (KeyCode::Char('l'), "l", FocusRight),
        (KeyCode::Char('k'), "k", FocusUp),
        (KeyCode::Char('j'), "j", FocusDown),
        (KeyCode::Left, "←", ResizeLeft),
        (KeyCode::Right, "→", ResizeRight),
        (KeyCode::Up, "↑", ResizeUp),
        (KeyCode::Down, "↓", ResizeDown),
    ]
}

/// Resolves a prefix-suffix key into the command-palette action it triggers.
fn prefix_key_action(code: KeyCode) -> Option<CommandPaletteAction> {
    prefix_action_bindings()
        .iter()
        .find(|(binding, _, _)| *binding == code)
        .map(|(_, _, action)| *action)
}

/// The prefix-suffix key label for a palette action, e.g. `Some("%")`, or
/// `None` when the action has no prefix-key binding.
fn palette_shortcut(action: CommandPaletteAction) -> Option<&'static str> {
    prefix_action_bindings()
        .iter()
        .find(|(_, _, bound)| *bound == action)
        .map(|(_, label, _)| *label)
}

/// Case-insensitive subsequence match. Returns a rank score (higher = better)
/// when every char of `query` appears in `haystack` in order, or `None` when it
/// does not. Adjacent matched chars earn a bonus so contiguous substrings
/// outrank scattered subsequences.
fn subsequence_score(query: &str, haystack: &str) -> Option<i32> {
    let mut chars = haystack.chars();
    let mut score = 0i32;
    let mut last_matched = false;
    for qc in query.chars() {
        loop {
            match chars.next() {
                Some(hc) if hc == qc => {
                    if last_matched {
                        score += 1;
                    }
                    last_matched = true;
                    break;
                }
                Some(_) => last_matched = false,
                None => return None,
            }
        }
    }
    Some(score)
}

/// Fuzzy relevance score for a palette entry against the filter `query`, or
/// `None` when it matches neither the name nor the description. Name matches
/// outrank description matches; within each, contiguous substrings outrank
/// scattered subsequences. An empty query matches everything with a neutral
/// score so the palette keeps its declaration order.
fn command_filter_score(query: &str, entry: &CommandPaletteEntry) -> Option<i32> {
    if query.trim().is_empty() {
        return Some(0);
    }
    let query = query.to_lowercase();
    let name = entry.name.to_lowercase();
    let description = entry.description.to_lowercase();
    let mut best: Option<i32> = None;
    if name.contains(&query) {
        best = Some(best.map_or(400, |current: i32| current.max(400)));
    }
    if let Some(score) = subsequence_score(&query, &name) {
        let score = 200 + score;
        best = Some(best.map_or(score, |current| current.max(score)));
    }
    if description.contains(&query) {
        best = Some(best.map_or(100, |current: i32| current.max(100)));
    }
    if let Some(score) = subsequence_score(&query, &description) {
        best = Some(best.map_or(score, |current| current.max(score)));
    }
    best
}

/// Palette entries matching `filter`, ranked best-first. Ties (including the
/// empty-filter case, where every entry scores equally) keep declaration order.
fn filter_command_entries(filter: &str) -> Vec<CommandPaletteEntry> {
    let mut scored: Vec<(i32, usize, CommandPaletteEntry)> = command_palette_entries()
        .into_iter()
        .enumerate()
        .filter_map(|(index, entry)| {
            command_filter_score(filter, &entry).map(|score| (score, index, entry))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, _, entry)| entry).collect()
}

fn command_palette_entries() -> Vec<CommandPaletteEntry> {
    vec![
        CommandPaletteEntry {
            name: "split-right",
            description: "open a pane to the right",
            action: CommandPaletteAction::SplitRight,
        },
        CommandPaletteEntry {
            name: "split-down",
            description: "open a pane below",
            action: CommandPaletteAction::SplitDown,
        },
        CommandPaletteEntry {
            name: "new-workspace",
            description: "create a workspace",
            action: CommandPaletteAction::NewWorkspace,
        },
        CommandPaletteEntry {
            name: "kill-pane",
            description: "kill the active pane",
            action: CommandPaletteAction::KillPane,
        },
        CommandPaletteEntry {
            name: "duplicate-pane",
            description: "duplicate active pane to the right",
            action: CommandPaletteAction::DuplicatePane,
        },
        CommandPaletteEntry {
            name: "restart-pane",
            description: "restart the active pane",
            action: CommandPaletteAction::RestartPane,
        },
        CommandPaletteEntry {
            name: "clear-pane",
            description: "clear active pane capture",
            action: CommandPaletteAction::ClearPane,
        },
        CommandPaletteEntry {
            name: "copy-pane",
            description: "copy active pane screen",
            action: CommandPaletteAction::CopyPane,
        },
        CommandPaletteEntry {
            name: "paste",
            description: "paste clipboard into active pane",
            action: CommandPaletteAction::PastePane,
        },
        CommandPaletteEntry {
            name: "status-busy",
            description: "mark active agent busy",
            action: CommandPaletteAction::StatusBusy,
        },
        CommandPaletteEntry {
            name: "status-attention",
            description: "mark active agent needs input",
            action: CommandPaletteAction::StatusAttention,
        },
        CommandPaletteEntry {
            name: "status-done",
            description: "mark active agent done",
            action: CommandPaletteAction::StatusDone,
        },
        CommandPaletteEntry {
            name: "status-idle",
            description: "mark active agent idle",
            action: CommandPaletteAction::StatusIdle,
        },
        CommandPaletteEntry {
            name: "close-workspace",
            description: "close the active workspace",
            action: CommandPaletteAction::CloseWorkspace,
        },
        CommandPaletteEntry {
            name: "next-workspace",
            description: "switch to the next workspace",
            action: CommandPaletteAction::NextWorkspace,
        },
        CommandPaletteEntry {
            name: "previous-workspace",
            description: "switch to the previous workspace",
            action: CommandPaletteAction::PreviousWorkspace,
        },
        CommandPaletteEntry {
            name: "notifications",
            description: "open the notification panel",
            action: CommandPaletteAction::ToggleNotifications,
        },
        CommandPaletteEntry {
            name: "actions",
            description: "open project actions",
            action: CommandPaletteAction::ToggleActions,
        },
        CommandPaletteEntry {
            name: "settings",
            description: "open UI theme and behavior settings",
            action: CommandPaletteAction::Settings,
        },
        CommandPaletteEntry {
            name: "zoom-pane",
            description: "toggle active pane zoom",
            action: CommandPaletteAction::ToggleZoom,
        },
        CommandPaletteEntry {
            name: "next-tab",
            description: "activate next workspace tab",
            action: CommandPaletteAction::NextTab,
        },
        CommandPaletteEntry {
            name: "previous-tab",
            description: "activate previous workspace tab",
            action: CommandPaletteAction::PreviousTab,
        },
        CommandPaletteEntry {
            name: "new-tab",
            description: "open a new tab in the active workspace",
            action: CommandPaletteAction::NewTab,
        },
        CommandPaletteEntry {
            name: "focus-left",
            description: "focus pane left",
            action: CommandPaletteAction::FocusLeft,
        },
        CommandPaletteEntry {
            name: "focus-right",
            description: "focus pane right",
            action: CommandPaletteAction::FocusRight,
        },
        CommandPaletteEntry {
            name: "focus-up",
            description: "focus pane above",
            action: CommandPaletteAction::FocusUp,
        },
        CommandPaletteEntry {
            name: "focus-down",
            description: "focus pane below",
            action: CommandPaletteAction::FocusDown,
        },
        CommandPaletteEntry {
            name: "resize-left",
            description: "resize split left",
            action: CommandPaletteAction::ResizeLeft,
        },
        CommandPaletteEntry {
            name: "resize-right",
            description: "resize split right",
            action: CommandPaletteAction::ResizeRight,
        },
        CommandPaletteEntry {
            name: "resize-up",
            description: "resize split up",
            action: CommandPaletteAction::ResizeUp,
        },
        CommandPaletteEntry {
            name: "resize-down",
            description: "resize split down",
            action: CommandPaletteAction::ResizeDown,
        },
    ]
}

fn command_palette_lines(
    entries: &[CommandPaletteEntry],
    selected: usize,
    prefix_label: &str,
    width: u16,
    theme: UiTheme,
) -> Vec<Line<'static>> {
    let palette = theme.palette();
    if entries.is_empty() {
        return vec![Line::from(vec![Span::styled(
            "  (no matches)".to_string(),
            Style::default().fg(palette.muted),
        )])];
    }
    let width = width as usize;
    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let active = index == selected;
            let marker = if active { "> " } else { "  " };
            let left = format!(
                "{marker}{:<18} {}",
                trim_label(entry.name, 18),
                trim_label(entry.description, 80)
            );
            let shortcut = palette_shortcut(entry.action)
                .map(|key| format!("{prefix_label} {key}"))
                .unwrap_or_default();
            // Right-align the shortcut column within the panel width.
            let used =
                UnicodeWidthStr::width(left.as_str()) + UnicodeWidthStr::width(shortcut.as_str());
            let pad = width.saturating_sub(used).max(1);
            let (text_style, shortcut_style) = if active {
                let base = selected_row_style(palette);
                (base, base)
            } else {
                (
                    Style::default().fg(palette.text),
                    Style::default().fg(palette.muted),
                )
            };
            Line::from(vec![
                Span::styled(left, text_style),
                Span::styled(" ".repeat(pad), text_style),
                Span::styled(shortcut, shortcut_style),
            ])
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsEntryId {
    Theme,
    WorkspaceLine,
    Sidebar,
    SidebarResponsive,
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
    MobileRelayLocalhost,
    Section,
    HookShell,
    HookClaude,
    HookCodex,
    HookGrok,
    HookInstallAll,
}

struct SettingsEntry {
    id: SettingsEntryId,
    name: &'static str,
}

fn settings_entries() -> Vec<SettingsEntry> {
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
            id: SettingsEntryId::MobileRelayLocalhost,
            name: "relay localhost",
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

struct SettingsView<'a> {
    theme: UiTheme,
    workspace_second_line: UiWorkspaceSecondLine,
    sidebar_collapsed: bool,
    sidebar_responsive: bool,
    sidebar_width: u16,
    prefix_label: &'a str,
    scroll_step: usize,
    cursor_blink: bool,
    cursor_blink_ms: u64,
    status_markers: &'a str,
    default_shell: &'a str,
    default_cwd: &'a str,
    mouse: bool,
    tab_close_button: bool,
    bell_on_attention: bool,
    mobile_relay_enabled: bool,
    mobile_relay_bind: &'a str,
    mobile_relay_port: u16,
    mobile_relay_allow_localhost: bool,
    mobile_relay_allow_cgnat: bool,
    selected: usize,
}

fn settings_panel_lines(view: SettingsView<'_>) -> Vec<Line<'static>> {
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
                SettingsEntryId::SidebarWidth => {
                    format!("{} cols (drag edge)", view.sidebar_width)
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
                    "Cmux Remote / phone (Tailscale or localhost only)".to_string()
                }
                SettingsEntryId::MobileRelay => {
                    let settings = crate::config::RelaySettings {
                        enabled: view.mobile_relay_enabled,
                        bind: view.mobile_relay_bind.to_string(),
                        port: view.mobile_relay_port,
                        allow_localhost: view.mobile_relay_allow_localhost,
                        allow_tailnet_cgnat: view.mobile_relay_allow_cgnat,
                    };
                    crate::relay::runtime_status_line(&settings)
                }
                SettingsEntryId::MobileRelayBind => match view.mobile_relay_bind {
                    "tailscale" => "tailscale only".to_string(),
                    "local" => "localhost only".to_string(),
                    _ => "auto (Tailscale → localhost)".to_string(),
                },
                SettingsEntryId::MobileRelayLocalhost => {
                    if view.mobile_relay_allow_localhost {
                        "allow register from 127.0.0.1".to_string()
                    } else {
                        "deny localhost register".to_string()
                    }
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

fn context_menu_entries() -> Vec<ContextMenuEntry> {
    vec![
        ContextMenuEntry {
            name: "copy-pane",
            description: "copy pane screen",
            action: ContextMenuAction::CopyPane,
        },
        ContextMenuEntry {
            name: "paste",
            description: "paste clipboard",
            action: ContextMenuAction::PastePane,
        },
        ContextMenuEntry {
            name: "split-right",
            description: "split pane to the right",
            action: ContextMenuAction::SplitRight,
        },
        ContextMenuEntry {
            name: "split-down",
            description: "split pane below",
            action: ContextMenuAction::SplitDown,
        },
        ContextMenuEntry {
            name: "clear-pane",
            description: "clear pane capture",
            action: ContextMenuAction::ClearPane,
        },
    ]
}

fn context_menu_lines(selected: usize, pane: Option<&str>) -> Vec<String> {
    let pane = pane.unwrap_or("active");
    context_menu_entries()
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let marker = if index == selected { "> " } else { "  " };
            format!(
                "{marker}{:<14} {:<12} {}",
                entry.name, pane, entry.description
            )
        })
        .collect()
}

#[cfg(test)]
fn metadata_summary(metadata: &BTreeMap<String, String>) -> Option<String> {
    if metadata.is_empty() {
        return None;
    }
    Some(
        metadata
            .iter()
            .take(3)
            .map(|(key, value)| format!("{}={}", trim_label(key, 12), trim_label(value, 18)))
            .collect::<Vec<_>>()
            .join(" "),
    )
}

fn notification_workspace<'a>(
    snapshot: &'a Session,
    note: &crate::model::Notification,
) -> Option<&'a crate::model::Workspace> {
    if let Some(workspace_id) = &note.workspace {
        return snapshot
            .workspaces
            .iter()
            .find(|workspace| &workspace.id == workspace_id);
    }
    let pane_id = note.pane.as_ref()?;
    snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.panes.iter().any(|item| item == pane_id))
}

fn trim_label(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        value.to_string()
    } else {
        let mut out = value
            .chars()
            .take(max.saturating_sub(1))
            .collect::<String>();
        out.push('.');
        out
    }
}

/// Truncates `value` so its rendered (unicode) width fits within `max` columns,
/// appending an ellipsis when it had to cut. Relies on display width rather than
/// char count so CJK/emoji/wide glyphs never overflow their column budget.
fn truncate_to_width(value: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(value) <= max {
        return value.to_string();
    }
    let budget = max.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0usize;
    for ch in value.chars() {
        let width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + width > budget {
            break;
        }
        out.push(ch);
        used += width;
    }
    out.push('…');
    out
}

fn status_label(status: &crate::model::AgentStatus) -> &'static str {
    match status {
        crate::model::AgentStatus::Unknown => "unknown",
        crate::model::AgentStatus::Idle => "idle",
        crate::model::AgentStatus::Busy => "busy",
        crate::model::AgentStatus::Attention => "attention",
        crate::model::AgentStatus::Done => "done",
        crate::model::AgentStatus::Error => "error",
    }
}

#[cfg(test)]
fn pane_status_label(status: &crate::model::PaneStatus) -> &'static str {
    match status {
        crate::model::PaneStatus::Starting => "starting",
        crate::model::PaneStatus::Running => "running",
        crate::model::PaneStatus::Exited => "exited",
        crate::model::PaneStatus::Restored => "restored",
    }
}

#[cfg(test)]
fn surface_label(kind: &crate::model::SurfaceKind) -> &'static str {
    match kind {
        crate::model::SurfaceKind::Terminal => "term",
        crate::model::SurfaceKind::Browser => "browser",
        crate::model::SurfaceKind::Agent => "agent",
        crate::model::SurfaceKind::Markdown => "markdown",
    }
}

fn surface_prefix(kind: &crate::model::SurfaceKind) -> &'static str {
    match kind {
        crate::model::SurfaceKind::Terminal => "",
        crate::model::SurfaceKind::Browser => "browser:",
        crate::model::SurfaceKind::Agent => "agent:",
        crate::model::SurfaceKind::Markdown => "md:",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    /// Concatenates the text of every span in a rendered line, for asserting on
    /// styled `Vec<Line>` output.
    #[test]
    fn response_error_reports_daemon_failures() {
        // Success carries no error to display.
        let ok = protocol::Response {
            ok: true,
            data: None,
            error: None,
        };
        assert_eq!(response_error(&ok), None);

        // A failure with a message surfaces that message verbatim.
        let failed = protocol::Response {
            ok: false,
            data: None,
            error: Some("unknown pane".to_string()),
        };
        assert_eq!(response_error(&failed), Some("unknown pane".to_string()));

        // A failure with no message still surfaces a generic error.
        let failed_no_message = protocol::Response {
            ok: false,
            data: None,
            error: None,
        };
        assert_eq!(
            response_error(&failed_no_message),
            Some("daemon reported an error".to_string())
        );
    }

    fn line_text(line: &Line) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn render_to_string(session: &Session, sidebar_collapsed: bool) -> String {
        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let mut pane_sizes = BTreeMap::new();
                let scroll_offsets = BTreeMap::new();
                draw(
                    frame,
                    Some(session),
                    &mut pane_sizes,
                    &scroll_offsets,
                    UiMode::Panes,
                    sidebar_collapsed,
                    24,
                    0,
                    0,
                    &[],
                    0,
                    None,
                    0,
                    "",
                    "Ctrl-b",
                    0,
                    0,
                    None,
                    None,
                    None,
                    None,
                    false,
                    None,
                    None,
                    None,
                    UiTheme::Midnight,
                    UiWorkspaceSecondLine::Path,
                    true,
                    "emoji",
                    true,
                    5,
                    true,
                    1000,
                    "",
                    "launch",
                    true,
                    false,
                    false,
                    "auto",
                    4399,
                    false,
                    true,
                    false, // keep sidebar visible in unit tests (not compact-hidden)
                    0,
                )
            })
            .unwrap();
        terminal.backend().to_string()
    }

    /// Like `render_to_string` but renders a specific `mode` with an optional
    /// pending confirmation, for asserting the confirm prompt and empty-pane
    /// placeholder without a live TTY.
    fn render_mode_to_string(
        session: &Session,
        mode: UiMode,
        confirm: Option<&PendingConfirm>,
    ) -> String {
        render_mode_to_string_with_rename(session, mode, confirm, None)
    }

    fn render_mode_to_string_with_rename(
        session: &Session,
        mode: UiMode,
        confirm: Option<&PendingConfirm>,
        rename: Option<&RenameDialog>,
    ) -> String {
        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let mut pane_sizes = BTreeMap::new();
                let scroll_offsets = BTreeMap::new();
                draw(
                    frame,
                    Some(session),
                    &mut pane_sizes,
                    &scroll_offsets,
                    mode,
                    false,
                    24,
                    0,
                    0,
                    &[],
                    0,
                    None,
                    0,
                    "",
                    "Ctrl-b",
                    0,
                    0,
                    None,
                    None,
                    None,
                    None,
                    false,
                    confirm,
                    rename,
                    None,
                    UiTheme::Midnight,
                    UiWorkspaceSecondLine::Path,
                    true,
                    "emoji",
                    true,
                    5,
                    true,
                    1000,
                    "",
                    "launch",
                    true,
                    false,
                    false,
                    "auto",
                    4399,
                    false,
                    true,
                    false, // keep sidebar visible in unit tests (not compact-hidden)
                    0,
                )
            })
            .unwrap();
        terminal.backend().to_string()
    }

    fn make_tab(id: &str, title: &str) -> crate::model::PaneTab {
        crate::model::PaneTab {
            id: id.to_string(),
            title: title.to_string(),
            command: "bash".to_string(),
            surface_kind: crate::model::SurfaceKind::Terminal,
            status: None,
            agent_status: None,
            progress: None,
            notification_color: None,
            notification_message: None,
            exit_code: None,
            output: String::new(),
            output_formatted: String::new(),
            scrollback: String::new(),
            scrollback_formatted: String::new(),
            created_at: 1,
            updated_at: 1,
        }
    }

    fn mouse_test_session() -> Session {
        let mut session = Session::new("test");
        let mut pane_1 = crate::model::Pane::new(
            "pane-1".to_string(),
            "bash".to_string(),
            SplitDirection::Right,
        );
        pane_1.title = "left".to_string();
        let mut pane_2 = crate::model::Pane::new(
            "pane-2".to_string(),
            "cargo test".to_string(),
            SplitDirection::Right,
        );
        pane_2.title = "right".to_string();
        pane_2.tabs.push(crate::model::PaneTab {
            id: "tab-1".to_string(),
            title: "shell".to_string(),
            command: "bash".to_string(),
            surface_kind: crate::model::SurfaceKind::Terminal,
            status: None,
            agent_status: None,
            progress: None,
            notification_color: None,
            notification_message: None,
            exit_code: None,
            output: String::new(),
            output_formatted: String::new(),
            scrollback: String::new(),
            scrollback_formatted: String::new(),
            created_at: 1,
            updated_at: 1,
        });
        pane_2.tabs.push(crate::model::PaneTab {
            id: "tab-2".to_string(),
            title: "tests".to_string(),
            command: "cargo test".to_string(),
            surface_kind: crate::model::SurfaceKind::Terminal,
            status: None,
            agent_status: None,
            progress: None,
            notification_color: None,
            notification_message: None,
            exit_code: None,
            output: String::new(),
            output_formatted: String::new(),
            scrollback: String::new(),
            scrollback_formatted: String::new(),
            created_at: 1,
            updated_at: 1,
        });
        pane_2.active_tab = Some("tab-1".to_string());
        session.workspaces[0].panes = vec!["pane-1".to_string(), "pane-2".to_string()];
        session.workspaces[0].active_pane = Some("pane-1".to_string());
        session.workspaces[0].layout = Some(LayoutNode::Split {
            axis: SplitAxis::Horizontal,
            ratio: 50,
            first: Box::new(LayoutNode::Pane {
                pane: "pane-1".to_string(),
            }),
            second: Box::new(LayoutNode::Pane {
                pane: "pane-2".to_string(),
            }),
        });
        session.workspaces.push(crate::model::Workspace {
            id: "ws-2".to_string(),
            name: "agents".to_string(),
            cwd: crate::model::default_cwd(),
            git_branch: None,
            pull_request: None,
            ports: Vec::new(),
            pinned: false,
            tabs: vec![crate::model::WorkspaceTab::new("tab-1", "main")],
            active_tab: Some("tab-1".to_string()),
            panes: Vec::new(),
            active_pane: None,
            zoomed_pane: None,
            layout: None,
        });
        session.panes.insert("pane-1".to_string(), pane_1);
        session.panes.insert("pane-2".to_string(), pane_2);
        session
    }

    #[test]
    fn maps_common_navigation_keys_to_escape_sequences() {
        assert_eq!(
            key_to_input(KeyCode::Left, KeyModifiers::empty()).as_deref(),
            Some("\x1b[D")
        );
        assert_eq!(
            key_to_input(KeyCode::Right, KeyModifiers::CONTROL).as_deref(),
            Some("\x1b[1;5C")
        );
        assert_eq!(
            key_to_input(KeyCode::Delete, KeyModifiers::SHIFT).as_deref(),
            Some("\x1b[3;2~")
        );
        assert_eq!(
            key_to_input(KeyCode::BackTab, KeyModifiers::empty()).as_deref(),
            Some("\x1b[Z")
        );
        assert_eq!(
            key_to_input(KeyCode::F(5), KeyModifiers::empty()).as_deref(),
            Some("\x1b[15~")
        );
    }

    #[test]
    fn maps_control_and_alt_characters() {
        assert_eq!(
            key_to_input(KeyCode::Char('c'), KeyModifiers::CONTROL).as_deref(),
            Some("\x03")
        );
        assert_eq!(
            key_to_input(KeyCode::Char('x'), KeyModifiers::ALT).as_deref(),
            Some("\x1bx")
        );
        assert_eq!(
            key_to_input(
                KeyCode::Char('m'),
                KeyModifiers::CONTROL | KeyModifiers::ALT
            )
            .as_deref(),
            Some("\x1b\r")
        );
    }

    #[test]
    fn adjusts_scroll_offsets_with_bounds() {
        let mut offsets = BTreeMap::new();
        adjust_scroll(&mut offsets, "pane-1", 3);
        assert_eq!(offsets.get("pane-1"), Some(&3));
        adjust_scroll(&mut offsets, "pane-1", -2);
        assert_eq!(offsets.get("pane-1"), Some(&1));
        adjust_scroll(&mut offsets, "pane-1", -2);
        assert_eq!(offsets.get("pane-1"), None);
    }

    #[test]
    fn sidebar_width_changes_when_collapsed() {
        assert_eq!(sidebar_width(false, 24), 24);
        assert_eq!(sidebar_width(true, 24), 6);
    }

    #[test]
    fn compact_workspace_id_uses_numeric_suffix() {
        assert_eq!(compact_workspace_id("ws-12"), "12");
        assert_eq!(compact_workspace_id("workspace"), "wo");
    }

    #[test]
    fn sidebar_rows_map_to_workspaces() {
        let mut session = Session::new("test");
        session.workspaces.push(crate::model::Workspace {
            id: "ws-2".to_string(),
            name: "agents".to_string(),
            cwd: crate::model::default_cwd(),
            git_branch: None,
            pull_request: None,
            ports: Vec::new(),
            pinned: false,
            tabs: vec![crate::model::WorkspaceTab::new("tab-1", "main")],
            active_tab: Some("tab-1".to_string()),
            panes: Vec::new(),
            active_pane: None,
            zoomed_pane: None,
            layout: None,
        });

        assert_eq!(
            workspace_at_sidebar_row(&session, 0, 29, false, 1)
                .map(|workspace| workspace.id.as_str()),
            Some("ws-1")
        );
        assert_eq!(
            workspace_at_sidebar_row(&session, 0, 29, false, 2)
                .map(|workspace| workspace.id.as_str()),
            Some("ws-1")
        );
        assert_eq!(
            workspace_at_sidebar_row(&session, 0, 29, false, 3)
                .map(|workspace| workspace.id.as_str()),
            Some("ws-2")
        );
        assert_eq!(
            sidebar_position_from_row(&session, 0, 29, false, 1),
            Some(1)
        );
        assert_eq!(
            sidebar_position_from_row(&session, 0, 29, false, 2),
            Some(1)
        );
        assert_eq!(
            sidebar_position_from_row(&session, 0, 29, false, 3),
            Some(2)
        );
        assert_eq!(sidebar_position_from_row(&session, 0, 29, false, 0), None);
        assert!(workspace_at_sidebar_row(&session, 0, 29, false, 20).is_none());
        assert_eq!(sidebar_position_from_row(&session, 0, 29, false, 20), None);
    }

    #[test]
    fn sidebar_entries_window_scrolls_and_marks_overflow() {
        // 10 workspaces, only 5 rows of list height -> must scroll and show
        // overflow indicators at top and/or bottom.
        let top = sidebar_entries(10, 0, 5, true);
        assert_eq!(top.first(), Some(&SidebarEntry::Workspace(0)));
        assert_eq!(top.last(), Some(&SidebarEntry::Below(6)));
        assert_eq!(top.len(), 5);

        let middle = sidebar_entries(10, 3, 5, true);
        assert_eq!(middle.first(), Some(&SidebarEntry::Above(3)));
        assert_eq!(middle.last(), Some(&SidebarEntry::Below(4)));
        // 5 rows = above marker + 3 workspaces + below marker.
        assert_eq!(
            middle,
            vec![
                SidebarEntry::Above(3),
                SidebarEntry::Workspace(3),
                SidebarEntry::Workspace(4),
                SidebarEntry::Workspace(5),
                SidebarEntry::Below(4),
            ]
        );

        let bottom = sidebar_entries(10, max_sidebar_offset(10, 5, true), 5, true);
        assert_eq!(bottom.first(), Some(&SidebarEntry::Above(6)));
        assert_eq!(bottom.last(), Some(&SidebarEntry::Workspace(9)));
    }

    #[test]
    fn scroll_to_reveal_keeps_active_workspace_visible() {
        // Active near the bottom while scrolled to the top -> scroll down.
        let offset = scroll_to_reveal(10, 9, 0, 5, true);
        let (first, last) = sidebar_visible_range(10, offset, 5, true).unwrap();
        assert!((first..=last).contains(&9));

        // Active at the top while scrolled down -> scroll back up.
        let offset = scroll_to_reveal(10, 0, 6, 5, true);
        let (first, last) = sidebar_visible_range(10, offset, 5, true).unwrap();
        assert!((first..=last).contains(&0));
    }

    #[test]
    fn sidebar_hit_test_respects_scroll_offset() {
        let mut session = Session::new("test");
        for index in 2..=10 {
            session.workspaces.push(crate::model::Workspace {
                id: format!("ws-{index}"),
                name: format!("w{index}"),
                cwd: crate::model::default_cwd(),
                git_branch: None,
                pull_request: None,
                ports: Vec::new(),
                pinned: false,
                tabs: vec![crate::model::WorkspaceTab::new("tab-1", "main")],
                active_tab: Some("tab-1".to_string()),
                panes: Vec::new(),
                active_pane: None,
                zoomed_pane: None,
                layout: None,
            });
        }
        // Offset 3, list height 5: rows are Above, ws-4 (two rows), Below.
        // Sidebar list starts at terminal row 1, so row 2 -> the first workspace.
        assert_eq!(
            workspace_at_sidebar_row(&session, 3, 5, false, 2).map(|w| w.id.as_str()),
            Some("ws-4")
        );
        assert_eq!(
            workspace_at_sidebar_row(&session, 3, 5, false, 3).map(|w| w.id.as_str()),
            Some("ws-4")
        );
        // Row 1 is the top overflow marker -> no workspace.
        assert!(workspace_at_sidebar_row(&session, 3, 5, false, 1).is_none());
        // Bottom overflow marker row -> no workspace.
        assert!(workspace_at_sidebar_row(&session, 3, 5, false, 4).is_none());
        assert!(workspace_at_sidebar_row(&session, 3, 5, false, 5).is_none());
        assert_eq!(sidebar_position_from_row(&session, 3, 5, false, 3), Some(4));
    }

    #[test]
    fn primary_mouse_action_maps_sidebar_click_to_workspace_switch() {
        let session = mouse_test_session();

        assert_eq!(
            primary_mouse_action(&session, 0, false, 24, 100, 30, 2, 3, "emoji", true),
            PrimaryMouseAction::SwitchWorkspace("ws-2".to_string())
        );
        assert_eq!(
            primary_mouse_action(&session, 0, false, 24, 100, 30, 2, 20, "emoji", true),
            PrimaryMouseAction::None
        );
    }

    #[test]
    fn primary_mouse_action_detects_split_resize_before_pane_focus() {
        let session = mouse_test_session();

        assert_eq!(
            primary_mouse_action(&session, 0, false, 24, 100, 30, 62, 10, "emoji", true),
            PrimaryMouseAction::StartResize(PaneResizeDrag {
                axis: SplitAxis::Horizontal,
                column: 62,
                row: 10,
            })
        );
    }

    #[test]
    fn primary_mouse_action_maps_pane_and_tab_clicks() {
        let session = mouse_test_session();

        // Pane grid sits below the workspace tab bar (row 0).
        assert_eq!(
            primary_mouse_action(&session, 0, false, 24, 100, 30, 70, 5, "emoji", true),
            PrimaryMouseAction::FocusPane("pane-2".to_string())
        );
        // Workspace tab bar "+" hits new tab (after tab labels).
        assert_eq!(
            primary_mouse_action(
                &session,
                0,
                false,
                24,
                100,
                30,
                sidebar_width(false, 24) + 2,
                0,
                "emoji",
                true
            ),
            PrimaryMouseAction::FocusWorkspaceTab {
                tab: "tab-1".to_string(),
            }
        );
    }

    #[test]
    fn workspace_tab_bar_close_button_hits_when_multiple_tabs() {
        let mut session = mouse_test_session();
        session.workspaces[0]
            .tabs
            .push(crate::model::WorkspaceTab::new("tab-2", "other"));
        let ws = &session.workspaces[0];
        let chips = workspace_tab_chips(&session, ws, "emoji", true);
        assert_eq!(chips.len(), 2);
        assert!(chips[0].close_start.is_some());
        assert!(chips[0].label.contains('×'));

        let area = workspace_tab_bar_area(false, 24, 120);
        // Click the × on the first tab.
        let close_x = area.x + chips[0].close_start.unwrap() as u16 + 0; // first cell of ×
        assert_eq!(
            workspace_tab_bar_hit(&session, ws, area, close_x, area.y, "emoji", true),
            Some(PrimaryMouseAction::CloseWorkspaceTab {
                tab: "tab-1".to_string(),
            })
        );
        // Click the title area focuses, does not close.
        assert_eq!(
            workspace_tab_bar_hit(&session, ws, area, area.x + 1, area.y, "emoji", true),
            Some(PrimaryMouseAction::FocusWorkspaceTab {
                tab: "tab-1".to_string(),
            })
        );
        // Single-tab workspace has no close control.
        let solo = mouse_test_session();
        let solo_chips = workspace_tab_chips(&solo, &solo.workspaces[0], "emoji", true);
        assert!(solo_chips[0].close_start.is_none());
    }

    #[test]
    fn primary_mouse_action_ignores_control_bar_rows() {
        let session = mouse_test_session();

        assert_eq!(
            primary_mouse_action(&session, 0, false, 24, 100, 30, 70, 28, "emoji", true),
            PrimaryMouseAction::None
        );
        assert_eq!(
            pane_area(false, 24, 100, 30),
            Rect::new(sidebar_width(false, 24), TAB_BAR_HEIGHT, 76, 27)
        );
    }

    #[test]
    fn control_bar_maps_clicks_to_actions() {
        // Buttons lay out left-to-right from the main content x (after sidebar).
        let x0 = sidebar_width(false, 24);
        let buttons = control_buttons();
        let mut x = x0;
        for button in &buttons[..3] {
            assert_eq!(
                control_action_at(false, 24, 200, 30, x + 1, 28),
                Some(button.action),
                "click inside {} should hit {:?}",
                button.label,
                button.action
            );
            x = x.saturating_add(button.width());
        }
        assert_eq!(
            control_buttons()[0].label,
            "menu",
            "workspace menu (burger) is first on the control bar"
        );
        assert_eq!(
            control_buttons()[1].label,
            "workspace",
            "new-workspace button uses a clear label"
        );
        // Detach is pinned to the far right of the control bar.
        let detach = detach_control_button();
        let detach_x = 200u16.saturating_sub(detach.width());
        assert_eq!(
            control_action_at(false, 24, 200, 30, detach_x + 1, 28),
            Some(ControlAction::Detach)
        );
        // Every icon should occupy 2 display columns (no mixed 1-cell gaps).
        for button in control_buttons().into_iter().chain(std::iter::once(detach)) {
            let icon_w = UnicodeWidthStr::width(button.icon);
            assert_eq!(
                icon_w, 2,
                "icon {:?} should be double-width, got {icon_w}",
                button.icon
            );
        }
        assert_eq!(control_action_at(false, 24, 100, 30, 1, 28), None);
        assert_eq!(control_action_at(false, 24, 100, 30, 0, 29), None);
    }

    #[test]
    fn sgr_mouse_sequence_uses_one_based_pane_coordinates() {
        assert_eq!(
            sgr_mouse_sequence(MouseButtonCode::Left, 4, 2, true),
            "\x1b[<0;4;2M"
        );
        assert_eq!(
            sgr_mouse_sequence(MouseButtonCode::Left, 4, 2, false),
            "\x1b[<0;4;2m"
        );
        assert_eq!(
            sgr_mouse_sequence(MouseButtonCode::LeftDrag, 4, 2, true),
            "\x1b[<32;4;2M"
        );
        assert_eq!(
            sgr_mouse_sequence(MouseButtonCode::WheelDown, 1, 1, true),
            "\x1b[<65;1;1M"
        );
    }

    #[test]
    fn selected_text_from_pane_uses_content_coordinates() {
        let mut pane = crate::model::Pane::new(
            "pane-1".to_string(),
            "bash".to_string(),
            SplitDirection::Right,
        );
        pane.output = "abcdef\nsecond\nthird".to_string();
        let content = Rect::new(11, 1, 20, 10);
        let selection = TextSelection {
            pane: "pane-1".to_string(),
            start_col: 13,
            start_row: 1,
            end_col: 15,
            end_row: 2,
        };

        assert_eq!(
            selected_text_from_pane(&pane, content, &selection).as_deref(),
            Some("cdef\nsecon")
        );
    }

    #[test]
    fn selection_clamps_to_content_bounds() {
        let mut pane = crate::model::Pane::new(
            "pane-1".to_string(),
            "sh".to_string(),
            SplitDirection::Right,
        );
        pane.output = "abc\nsecond\nthird".to_string();
        let content = Rect::new(10, 5, 4, 2);
        let selection = TextSelection {
            pane: "pane-1".to_string(),
            start_col: 12,
            start_row: 5,
            end_col: 200,
            end_row: 200,
        };

        assert_eq!(
            selection_range_in_content(&selection, content),
            Some((2, 0, 3, 1))
        );
        assert_eq!(
            selected_text_from_pane(&pane, content, &selection).as_deref(),
            Some("c\nseco")
        );
    }

    #[test]
    fn pane_corner_controls_map_to_pane_actions() {
        let session = mouse_test_session();
        let y = TAB_BAR_HEIGHT;
        // Scan the top-right of the right pane for any control hit.
        let mut found_close = false;
        let mut found_split_right = false;
        let mut found_split_down = false;
        for x in 80..99 {
            if let Some(hit) = pane_control_at(&session, false, 24, 100, 30, x, y) {
                if hit.action == PaneControlAction::Close {
                    found_close = true;
                }
                if hit.action == PaneControlAction::SplitRight {
                    found_split_right = true;
                }
                if hit.action == PaneControlAction::SplitDown {
                    found_split_down = true;
                }
            }
        }
        assert!(found_close, "expected a close (×) control on pane chrome");
        assert!(
            found_split_right,
            "expected a split-right control on pane chrome"
        );
        assert!(
            found_split_down,
            "expected a split-down control on pane chrome"
        );
        assert_eq!(pane_control_at(&session, false, 24, 100, 30, 10, y), None);
    }

    #[test]
    fn close_workspace_warning_mentions_panes_and_confirmation() {
        let session = mouse_test_session();
        let text = close_workspace_warning_text(&session);

        assert!(text.contains("Close workspace 'main'?"));
        assert!(text.contains("2 pane(s)"));
        assert!(text.contains("Closing it will stop those panes"));
    }

    #[test]
    fn pending_close_always_kills_the_pane() {
        // Closing a pane always kills that pane (workspace tabs are separate).
        let session = mouse_test_session();
        let pending = pending_close_for_pane(&session, "pane-2").expect("pending confirm");

        assert!(pending.title.contains("close pane"));
        assert!(pending.body.contains("Close pane pane-2"));
        assert!(pending.body.contains("stop the pane's process"));
        assert_eq!(
            pending.action,
            ConfirmAction::KillPane("pane-2".to_string())
        );
        assert_eq!(pending.highlight_pane(), Some("pane-2"));
    }

    #[test]
    fn pending_close_prompts_pane_kill_when_pane_has_no_extra_tabs() {
        let session = mouse_test_session();
        let pending = pending_close_for_pane(&session, "pane-1").expect("pending confirm");

        assert!(pending.title.contains("close pane"));
        assert!(pending.body.contains("Close pane pane-1 (left)?"));
        assert!(pending.body.contains("stop the pane's process"));
        assert_eq!(
            pending.action,
            ConfirmAction::KillPane("pane-1".to_string())
        );

        // Unknown panes produce no prompt rather than panicking.
        assert!(pending_close_for_pane(&session, "pane-404").is_none());
    }

    #[test]
    fn confirm_request_maps_each_action_to_its_daemon_request() {
        let action_name = |action: &ConfirmAction| {
            serde_json::to_value(confirm_request(action)).unwrap()["action"]
                .as_str()
                .unwrap()
                .to_string()
        };
        assert_eq!(
            action_name(&ConfirmAction::CloseWorkspace),
            "close-workspace"
        );
        assert_eq!(
            action_name(&ConfirmAction::KillPane("pane-1".to_string())),
            "kill-pane"
        );
        assert_eq!(
            action_name(&ConfirmAction::CloseWorkspaceTab {
                tab: "tab-1".to_string(),
            }),
            "close-tab"
        );
        let value = serde_json::to_value(confirm_request(&ConfirmAction::CloseWorkspaceTab {
            tab: "tab-1".to_string(),
        }))
        .unwrap();
        assert_eq!(value["tab"], "tab-1");
    }

    #[test]
    fn apply_cursor_marker_paints_block_on_empty_cell() {
        let palette = UiTheme::Midnight.palette();
        let mut lines = vec![Line::from("hello"), Line::from("")];
        apply_cursor_marker(&mut lines, Some(1), Some(0), Some(2), 2, palette);
        let text: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains('█') || text.contains(' '),
            "expected cursor block on empty line, got {text:?}"
        );
        // Cursor style should differ from default (reversed / accent bg).
        assert!(
            lines[1]
                .spans
                .iter()
                .any(|s| { s.style.bg.is_some() || s.style.add_modifier.contains(Modifier::BOLD) }),
            "cursor cell should be styled"
        );
    }

    #[test]
    fn cursor_view_row_maps_screen_coords_into_smaller_pane() {
        // PTY is 40 rows; pane shows bottom 10. Cursor on last screen row → view row 9.
        assert_eq!(cursor_view_row(39, Some(40), 10, 10), Some(9));
        // Cursor near top of screen is above the live window.
        assert_eq!(cursor_view_row(5, Some(40), 10, 10), None);
        // Cursor mid-window.
        assert_eq!(cursor_view_row(35, Some(40), 10, 10), Some(5));
    }

    #[test]
    fn apply_cursor_marker_handles_screen_taller_than_view() {
        let palette = UiTheme::Midnight.palette();
        // View only has 3 lines (already tailed); PTY screen is 10 rows; cursor at row 9.
        let mut lines = vec![Line::from("a"), Line::from("b"), Line::from("c")];
        apply_cursor_marker(&mut lines, Some(9), Some(0), Some(10), 3, palette);
        let text: String = lines[2].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains('█') || lines[2].spans.iter().any(|s| s.style.bg.is_some()),
            "cursor at bottom of tall screen should paint on last view line, got {text:?}"
        );
    }

    #[test]
    fn invert_cell_at_column_marks_character() {
        let palette = UiTheme::Midnight.palette();
        let line = Line::from("abc");
        let painted = invert_cell_at_column(line, 1, palette);
        let text: String = painted.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "abc");
        // Middle cell should carry cursor styling.
        assert!(painted.spans.iter().any(|s| s.content.as_ref() == "b"
            && (s.style.bg.is_some() || s.style.add_modifier.contains(Modifier::BOLD))));
    }

    fn confirm_mode_renders_pending_prompt() {
        let session = mouse_test_session();
        let pending = pending_close_for_pane(&session, "pane-2").expect("pending confirm");
        // Overlay draws on top of the pane grid (Panes mode + pending confirm).
        let rendered = render_mode_to_string(&session, UiMode::Panes, Some(&pending));

        assert!(rendered.contains("close pane"));
        assert!(rendered.contains("Close pane pane-2"));
        assert!(rendered.contains("[Y] Yes") || rendered.contains("Yes"));
        assert!(rendered.contains("[N] Cancel") || rendered.contains("Cancel"));
        // Highlight targets the pane being closed.
        assert_eq!(pending.highlight_pane(), Some("pane-2"));
    }

    #[test]
    fn rename_dialog_renders_input_and_buttons() {
        let session = mouse_test_session();
        let dialog = RenameDialog {
            target: RenameTarget::Pane {
                id: "pane-1".to_string(),
            },
            draft: "new-title".to_string(),
        };
        let rendered =
            render_mode_to_string_with_rename(&session, UiMode::Panes, None, Some(&dialog));
        assert!(rendered.contains("rename pane") || rendered.contains("pane"));
        assert!(rendered.contains("new-title"));
        assert!(rendered.contains("OK") || rendered.contains("Enter"));
        assert!(rendered.contains("Cancel") || rendered.contains("Esc"));
    }

    #[test]
    fn rename_target_at_resolves_workspace_and_tab() {
        let session = mouse_test_session();
        // Sidebar workspace row 1 (after title row 0).
        let hit = rename_target_at(&session, 0, false, 24, 100, 30, 2, 1);
        assert!(
            matches!(hit, Some((RenameTarget::Workspace { .. }, _))),
            "expected workspace rename target, got {hit:?}"
        );
        // Tab bar row 0, first tab label starts just after sidebar.
        let tab_hit = rename_target_at(
            &session,
            0,
            false,
            24,
            100,
            30,
            sidebar_width(false, 24) + 2,
            0,
        );
        assert!(
            matches!(tab_hit, Some((RenameTarget::Tab { .. }, _))),
            "expected tab rename target, got {tab_hit:?}"
        );
    }

    #[test]
    fn empty_workspace_renders_placeholder_with_create_button() {
        // A fresh session has one workspace with no panes.
        let session = Session::new("test");
        let rendered = render_mode_to_string(&session, UiMode::Panes, None);

        assert!(rendered.contains("no panes in this workspace"));
        assert!(rendered.contains("[ + create pane ]"));
        assert!(rendered.contains("split"));
    }

    #[test]
    fn empty_workspace_exposes_create_pane_click_target() {
        // The create-pane button has a real, non-empty click region inside the
        // pane area, and clicks elsewhere in the empty area do not misroute.
        let session = Session::new("test");
        let area = pane_area(false, 24, 120, 40);
        let rect = empty_create_pane_rect(area).expect("create-pane rect");
        assert!(rect.width > 0 && rect.height == 1);
        assert!(point_in_rect(area, rect.x, rect.y));

        let center_col = rect.x + rect.width / 2;
        // The empty pane area never resolves to a pane focus/switch/resize.
        assert_eq!(
            primary_mouse_action(
                &session, 0, false, 24, 120, 40, center_col, rect.y, "emoji", true
            ),
            PrimaryMouseAction::None
        );
        // A cramped pane area yields no button rather than an out-of-bounds rect.
        assert!(empty_create_pane_rect(pane_area(false, 24, 30, 4)).is_none());
    }

    #[test]
    fn render_full_and_collapsed_sidebars() {
        let mut session = Session::new("test");
        session.workspaces[0].cwd = "/tmp/main".to_string();
        session.workspaces.push(crate::model::Workspace {
            id: "ws-2".to_string(),
            name: "agents".to_string(),
            cwd: "/tmp/agents".to_string(),
            git_branch: Some("feature".to_string()),
            pull_request: None,
            ports: Vec::new(),
            pinned: true,
            tabs: vec![crate::model::WorkspaceTab::new("tab-1", "main")],
            active_tab: Some("tab-1".to_string()),
            panes: Vec::new(),
            active_pane: None,
            zoomed_pane: None,
            layout: None,
        });

        let full = render_to_string(&session, false);
        assert!(full.contains("main"));
        assert!(full.contains("agents"));
        // Fixed 2-char prefix: active `"> "`, inactive `"  "`.
        assert!(full.contains("> 1:main") || full.contains(">1:main"));
        assert!(full.contains("📁"));
        // Control bar may clip trailing buttons on narrow fixtures; workspace is first.
        assert!(full.contains("workspace") || full.contains("tab"));

        let collapsed = render_to_string(&session, true);
        assert!(collapsed.contains(">  1"));
        assert!(collapsed.contains("  *2"));
        assert!(!collapsed.contains("agents"));
        assert!(!collapsed.contains("@feature"));
    }

    #[test]
    fn ansi_to_lines_preserves_sgr_colors() {
        let lines = ansi_to_lines("plain \x1b[31mred\x1b[0m normal");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans.len(), 3);
        assert_eq!(lines[0].spans[0].content.as_ref(), "plain ");
        assert_eq!(lines[0].spans[1].content.as_ref(), "red");
        assert_eq!(lines[0].spans[1].style.fg, Some(Color::Red));
        assert_eq!(lines[0].spans[2].content.as_ref(), " normal");
        assert_eq!(lines[0].spans[2].style.fg, None);
    }

    #[test]
    fn ansi_to_lines_preserves_modern_sgr_colors() {
        let lines = ansi_to_lines("\x1b[38;5;45midx\x1b[0m \x1b[38:2::255:100:0mrgb");

        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans[0].content.as_ref(), "idx");
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Indexed(45)));
        assert_eq!(lines[0].spans[2].content.as_ref(), "rgb");
        assert_eq!(lines[0].spans[2].style.fg, Some(Color::Rgb(255, 100, 0)));
    }

    #[test]
    fn ansi_to_lines_preserves_cursor_forward_spacing() {
        let lines = ansi_to_lines("\x1b[38;5;231m●\x1b[CHey!\x1b[CWhat\x1b[Ccan\x1b[CI\x1b[Chelp?");

        let text = lines[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(text, "● Hey! What can I help?");
    }

    #[test]
    fn ansi_to_lines_preserves_absolute_horizontal_spacing() {
        let lines = ansi_to_lines("a\x1b[5Gb");

        let text = lines[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(text, "a   b");
    }

    #[test]
    fn render_zoomed_pane_hides_other_split_panes() {
        let mut session = Session::new("test");
        let mut pane_1 = crate::model::Pane::new(
            "pane-1".to_string(),
            "printf left".to_string(),
            SplitDirection::Right,
        );
        pane_1.output = "left-only-output".to_string();
        let mut pane_2 = crate::model::Pane::new(
            "pane-2".to_string(),
            "printf right".to_string(),
            SplitDirection::Right,
        );
        pane_2.output = "right-only-output".to_string();
        session.workspaces[0].panes = vec!["pane-1".to_string(), "pane-2".to_string()];
        session.workspaces[0].active_pane = Some("pane-2".to_string());
        session.workspaces[0].zoomed_pane = Some("pane-2".to_string());
        session.workspaces[0].layout = Some(LayoutNode::Split {
            axis: SplitAxis::Horizontal,
            ratio: 50,
            first: Box::new(LayoutNode::Pane {
                pane: "pane-1".to_string(),
            }),
            second: Box::new(LayoutNode::Pane {
                pane: "pane-2".to_string(),
            }),
        });
        session.panes.insert("pane-1".to_string(), pane_1);
        session.panes.insert("pane-2".to_string(), pane_2);

        let rendered = render_to_string(&session, false);

        assert!(rendered.contains("right-only-output"));
        assert!(!rendered.contains("left-only-output"));
    }

    #[test]
    fn render_pane_output_clips_instead_of_wrapping_rows() {
        let mut session = Session::new("test");
        let mut pane = crate::model::Pane::new(
            "pane-1".to_string(),
            "sh".to_string(),
            SplitDirection::Right,
        );
        pane.output = "x".repeat(160);
        pane.output_formatted = pane.output.clone();
        session.workspaces[0].panes = vec!["pane-1".to_string()];
        session.workspaces[0].active_pane = Some("pane-1".to_string());
        session.workspaces[0].layout = Some(LayoutNode::Pane {
            pane: "pane-1".to_string(),
        });
        session.panes.insert("pane-1".to_string(), pane);

        let rendered = render_to_string(&session, false);

        assert!(rendered.matches('x').count() < 80);
    }

    #[test]
    fn selects_scrollback_window_and_strips_ansi() {
        let mut pane = crate::model::Pane::new(
            "pane-1".to_string(),
            "sh".to_string(),
            SplitDirection::Right,
        );
        pane.output = "visible\nnow".to_string();
        pane.scrollback = "one\r\ntwo\x1b[31m-red\x1b[m\r\nthree\r\nfour\r\n".to_string();
        assert_eq!(pane_text_for_view(&pane, 2, 0), "visible\nnow");
        assert_eq!(pane_text_for_view(&pane, 2, 1), "two-red\nthree");
    }

    #[test]
    fn scrolled_view_uses_styled_scrollback_when_available() {
        let mut pane = crate::model::Pane::new(
            "pane-1".to_string(),
            "sh".to_string(),
            SplitDirection::Right,
        );
        pane.output_formatted = "visible\nnow".to_string();
        // Colored history: line "two-red" carries an SGR red run.
        pane.scrollback_formatted = "one\ntwo\x1b[31m-red\x1b[m\nthree\nfour".to_string();

        let area = Rect::new(0, 0, 20, 2);
        let lines = pane_lines_for_view(&pane, 2, 1, None, area, UiTheme::Midnight.palette(), true);

        // Same window the plain path picks (end = 4 - 1 = 3, start = 1): the
        // "two-red" and "three" rows, but now colored.
        assert_eq!(lines.len(), 2);
        let colored = lines[0]
            .spans
            .iter()
            .any(|span| span.content.as_ref() == "-red" && span.style.fg == Some(Color::Red));
        assert!(colored, "scrolled history should keep its SGR colors");
    }

    #[test]
    fn scrolled_view_falls_back_to_plain_scrollback_when_unformatted() {
        let mut pane = crate::model::Pane::new(
            "pane-1".to_string(),
            "sh".to_string(),
            SplitDirection::Right,
        );
        // No styled scrollback (exited/restored pane): plain path is used.
        pane.scrollback = "one\r\ntwo\r\nthree\r\nfour\r\n".to_string();

        let area = Rect::new(0, 0, 20, 2);
        let lines = pane_lines_for_view(&pane, 2, 1, None, area, UiTheme::Midnight.palette(), true);

        let text = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert_eq!(text, vec!["two".to_string(), "three".to_string()]);
    }

    #[test]
    fn pane_tab_cells_lay_out_and_mark_active_tab() {
        let mut pane = crate::model::Pane::new(
            "pane-1".to_string(),
            "bash".to_string(),
            SplitDirection::Right,
        );
        pane.tabs.push(crate::model::PaneTab {
            id: "tab-1".to_string(),
            title: "shell".to_string(),
            command: "bash".to_string(),
            surface_kind: crate::model::SurfaceKind::Terminal,
            status: None,
            agent_status: None,
            progress: None,
            notification_color: None,
            notification_message: None,
            exit_code: None,
            output: String::new(),
            output_formatted: String::new(),
            scrollback: String::new(),
            scrollback_formatted: String::new(),
            created_at: 1,
            updated_at: 1,
        });
        pane.tabs.push(crate::model::PaneTab {
            id: "tab-2".to_string(),
            title: "tests".to_string(),
            command: "cargo test".to_string(),
            surface_kind: crate::model::SurfaceKind::Terminal,
            status: None,
            agent_status: None,
            progress: None,
            notification_color: None,
            notification_message: None,
            exit_code: None,
            output: String::new(),
            output_formatted: String::new(),
            scrollback: String::new(),
            scrollback_formatted: String::new(),
            created_at: 1,
            updated_at: 1,
        });
        pane.active_tab = Some("tab-2".to_string());

        let area = Rect::new(10, 0, 80, 20);
        let cells = pane_tab_cells(&pane, pane_tab_strip_area(area));
        assert_eq!(cells.len(), 2);
        assert_eq!(cells[0].text, " 1:shell ");
        assert!(!cells[0].active);
        assert_eq!(cells[0].start, 11);
        assert_eq!(cells[0].end, 20);
        assert_eq!(cells[1].text, " 2:tests ");
        assert!(cells[1].active);
        assert!(!cells[1].overflow);
    }

    #[test]
    fn pane_tab_hit_resolves_clicked_strip_tab() {
        let mut pane = crate::model::Pane::new(
            "pane-1".to_string(),
            "bash".to_string(),
            SplitDirection::Right,
        );
        pane.tabs.push(make_tab("tab-1", "shell"));
        pane.tabs.push(make_tab("tab-2", "tests"));
        pane.active_tab = Some("tab-2".to_string());
        let area = Rect::new(10, 0, 80, 20);
        let strip_row = pane_tab_strip_area(area).y;

        // " 1:shell " spans columns 11..20, " 2:tests " spans 20..29.
        assert_eq!(
            pane_tab_hit(&pane, area, 13, strip_row).as_deref(),
            Some("tab-1")
        );
        assert_eq!(
            pane_tab_hit(&pane, area, 22, strip_row).as_deref(),
            Some("tab-2")
        );
        // The title row (top border) no longer hosts tabs.
        assert_eq!(pane_tab_hit(&pane, area, 13, area.y), None);
    }

    #[test]
    fn pane_tab_hit_handles_unicode_titles() {
        let mut pane = crate::model::Pane::new(
            "pane-1".to_string(),
            "bash".to_string(),
            SplitDirection::Right,
        );
        // Wide (CJK) glyphs: each occupies two columns.
        pane.tabs.push(make_tab("tab-1", "测试"));
        pane.tabs.push(make_tab("tab-2", "日本"));
        pane.active_tab = Some("tab-1".to_string());
        let area = Rect::new(10, 0, 80, 20);
        let strip = pane_tab_strip_area(area);
        let strip_row = strip.y;
        let cells = pane_tab_cells(&pane, strip);

        // " 1:测试 " = 1 + 2 + 2*2 + 1 = 8 columns wide.
        assert_eq!(UnicodeWidthStr::width(cells[0].text.as_str()), 8);
        // Clicking inside the first tab's span resolves to it, not the second.
        assert_eq!(
            pane_tab_hit(&pane, area, cells[0].start + 1, strip_row).as_deref(),
            Some("tab-1")
        );
        assert_eq!(
            pane_tab_hit(&pane, area, cells[1].start + 1, strip_row).as_deref(),
            Some("tab-2")
        );
    }

    #[test]
    fn pane_tab_strip_shows_overflow_and_keeps_active_visible() {
        let mut pane = crate::model::Pane::new(
            "pane-1".to_string(),
            "bash".to_string(),
            SplitDirection::Right,
        );
        for index in 1..=8 {
            pane.tabs
                .push(make_tab(&format!("tab-{index}"), &format!("name{index}")));
        }
        // A late tab is active; the narrow strip must scroll to reveal it.
        pane.active_tab = Some("tab-8".to_string());
        let area = Rect::new(0, 0, 22, 20);
        let strip = pane_tab_strip_area(area);
        let cells = pane_tab_cells(&pane, strip);

        assert!(cells.iter().any(|cell| cell.active));
        assert_eq!(
            cells
                .iter()
                .find(|cell| cell.active)
                .map(|cell| cell.focus_id.as_str()),
            Some("tab-8")
        );
        let overflow = cells
            .iter()
            .find(|cell| cell.overflow)
            .expect("overflow marker");
        assert!(overflow.text.contains('+'));
        // Every cell stays within the strip bounds.
        assert!(cells.iter().all(|cell| cell.end <= strip.x + strip.width));
        // Clicking the overflow marker reveals a currently hidden tab.
        let hidden: Vec<&str> = pane
            .tabs
            .iter()
            .map(|tab| tab.id.as_str())
            .filter(|id| {
                !cells
                    .iter()
                    .any(|cell| !cell.overflow && cell.focus_id == *id)
            })
            .collect();
        assert!(hidden.contains(&overflow.focus_id.as_str()));
    }

    #[test]
    fn pane_tab_at_walks_split_layout() {
        let mut session = Session::new("test");
        let mut pane = crate::model::Pane::new(
            "pane-2".to_string(),
            "bash".to_string(),
            SplitDirection::Right,
        );
        pane.tabs.push(crate::model::PaneTab {
            id: "tab-1".to_string(),
            title: "shell".to_string(),
            command: "bash".to_string(),
            surface_kind: crate::model::SurfaceKind::Terminal,
            status: None,
            agent_status: None,
            progress: None,
            notification_color: None,
            notification_message: None,
            exit_code: None,
            output: String::new(),
            output_formatted: String::new(),
            scrollback: String::new(),
            scrollback_formatted: String::new(),
            created_at: 1,
            updated_at: 1,
        });
        pane.tabs.push(crate::model::PaneTab {
            id: "tab-2".to_string(),
            title: "tests".to_string(),
            command: "cargo test".to_string(),
            surface_kind: crate::model::SurfaceKind::Terminal,
            status: None,
            agent_status: None,
            progress: None,
            notification_color: None,
            notification_message: None,
            exit_code: None,
            output: String::new(),
            output_formatted: String::new(),
            scrollback: String::new(),
            scrollback_formatted: String::new(),
            created_at: 1,
            updated_at: 1,
        });
        pane.active_tab = Some("tab-1".to_string());
        session.workspaces[0].panes = vec!["pane-1".to_string(), "pane-2".to_string()];
        session.workspaces[0].layout = Some(LayoutNode::Split {
            axis: SplitAxis::Horizontal,
            ratio: 50,
            first: Box::new(LayoutNode::Pane {
                pane: "pane-1".to_string(),
            }),
            second: Box::new(LayoutNode::Pane {
                pane: "pane-2".to_string(),
            }),
        });
        session.panes.insert(
            "pane-1".to_string(),
            crate::model::Pane::new(
                "pane-1".to_string(),
                "sh".to_string(),
                SplitDirection::Right,
            ),
        );
        session.panes.insert("pane-2".to_string(), pane);
        let area = Rect::new(10, 0, 90, 30);

        // Split boundary is at column 55; pane-2's tab strip is on inner row 1
        // starting at column 56, so column 58 lands on its first tab.
        assert_eq!(
            pane_tab_at(&session, session.workspaces[0].layout.as_ref(), area, 58, 1,)
                .as_ref()
                .map(|(pane, tab)| (pane.as_str(), tab.as_str())),
            Some(("pane-2", "tab-1"))
        );
    }

    #[test]
    fn relative_pane_tab_wraps_around_tabs() {
        let mut pane = crate::model::Pane::new(
            "pane-1".to_string(),
            "bash".to_string(),
            SplitDirection::Right,
        );
        pane.tabs.push(crate::model::PaneTab {
            id: "tab-1".to_string(),
            title: "shell".to_string(),
            command: "bash".to_string(),
            surface_kind: crate::model::SurfaceKind::Terminal,
            status: None,
            agent_status: None,
            progress: None,
            notification_color: None,
            notification_message: None,
            exit_code: None,
            output: String::new(),
            output_formatted: String::new(),
            scrollback: String::new(),
            scrollback_formatted: String::new(),
            created_at: 1,
            updated_at: 1,
        });
        pane.tabs.push(crate::model::PaneTab {
            id: "tab-2".to_string(),
            title: "tests".to_string(),
            command: "cargo test".to_string(),
            surface_kind: crate::model::SurfaceKind::Terminal,
            status: None,
            agent_status: None,
            progress: None,
            notification_color: None,
            notification_message: None,
            exit_code: None,
            output: String::new(),
            output_formatted: String::new(),
            scrollback: String::new(),
            scrollback_formatted: String::new(),
            created_at: 1,
            updated_at: 1,
        });
        pane.active_tab = Some("tab-2".to_string());

        assert_eq!(relative_pane_tab(&pane, 1).as_deref(), Some("tab-1"));
        assert_eq!(relative_pane_tab(&pane, -1).as_deref(), Some("tab-1"));
        pane.tabs.pop();
        assert_eq!(relative_pane_tab(&pane, 1), None);
    }

    #[test]
    fn command_palette_includes_pane_tab_actions() {
        let entries = command_palette_entries();
        assert!(entries.iter().any(|entry| entry.name == "next-tab"));
        assert!(entries.iter().any(|entry| entry.name == "previous-tab"));
        assert!(entries.iter().any(|entry| entry.name == "new-tab"));
    }

    #[test]
    fn split_axis_at_detects_split_boundaries() {
        let layout = LayoutNode::Split {
            axis: SplitAxis::Horizontal,
            ratio: 50,
            first: Box::new(LayoutNode::Pane {
                pane: "pane-1".to_string(),
            }),
            second: Box::new(LayoutNode::Split {
                axis: SplitAxis::Vertical,
                ratio: 50,
                first: Box::new(LayoutNode::Pane {
                    pane: "pane-2".to_string(),
                }),
                second: Box::new(LayoutNode::Pane {
                    pane: "pane-3".to_string(),
                }),
            }),
        };
        let area = Rect::new(10, 0, 90, 30);

        assert_eq!(
            split_axis_at(Some(&layout), area, 55, 10),
            Some(SplitAxis::Horizontal)
        );
        assert_eq!(
            split_axis_at(Some(&layout), area, 70, 15),
            Some(SplitAxis::Vertical)
        );
        assert_eq!(split_axis_at(Some(&layout), area, 20, 10), None);
        assert_eq!(split_axis_at(Some(&layout), area, 5, 10), None);
    }

    #[test]
    fn resize_drag_direction_follows_split_axis() {
        assert_eq!(
            resize_drag_direction(SplitAxis::Horizontal, 10, 5, 13, 5),
            Some(SplitDirection::Right)
        );
        assert_eq!(
            resize_drag_direction(SplitAxis::Horizontal, 10, 5, 8, 20),
            Some(SplitDirection::Left)
        );
        assert_eq!(
            resize_drag_direction(SplitAxis::Vertical, 10, 5, 20, 8),
            Some(SplitDirection::Down)
        );
        assert_eq!(
            resize_drag_direction(SplitAxis::Vertical, 10, 5, 0, 3),
            Some(SplitDirection::Up)
        );
        assert_eq!(
            resize_drag_direction(SplitAxis::Vertical, 10, 5, 0, 5),
            None
        );
    }

    #[test]
    fn workspace_notification_clear_hides_previous_workspace_message() {
        let mut session = Session::new("test");
        session.notifications.push(crate::model::Notification {
            time: 1,
            pane: None,
            workspace: Some("ws-1".to_string()),
            status: None,
            color: None,
            clear: false,
            message: "agents running".to_string(),
        });
        assert_eq!(
            workspace_notification(&session, "ws-1", &[]).as_deref(),
            Some("agents running")
        );

        session.notifications.push(crate::model::Notification {
            time: 2,
            pane: None,
            workspace: Some("ws-1".to_string()),
            status: None,
            color: None,
            clear: true,
            message: String::new(),
        });

        assert_eq!(workspace_notification(&session, "ws-1", &[]), None);
    }

    #[test]
    fn notification_panel_lines_include_context_newest_first() {
        let mut session = Session::new("test");
        let pane = crate::model::Pane::new(
            "pane-1".to_string(),
            "claude".to_string(),
            SplitDirection::Right,
        );
        session.workspaces[0].panes.push("pane-1".to_string());
        session.panes.insert("pane-1".to_string(), pane);
        session.notifications.push(crate::model::Notification {
            time: 1,
            pane: None,
            workspace: Some("ws-1".to_string()),
            status: Some("busy".to_string()),
            color: Some("yellow".to_string()),
            clear: false,
            message: "workspace note".to_string(),
        });
        session.notifications.push(crate::model::Notification {
            time: 2,
            pane: Some("pane-1".to_string()),
            workspace: None,
            status: Some("done".to_string()),
            color: Some("green".to_string()),
            clear: false,
            message: "pane note".to_string(),
        });

        let lines = notification_panel_lines(&session, 1);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("pane:claude"));
        assert!(lines[0].contains("pane note"));
        assert!(lines[0].starts_with("  "));
        assert!(lines[1].contains("ws:main"));
        assert!(lines[1].contains("workspace note"));
        assert!(lines[1].starts_with("> "));
    }

    #[test]
    fn action_panel_lines_include_title_direction_and_selection() {
        let actions = vec![
            UiAction {
                name: "test".to_string(),
                command: "cargo test".to_string(),
                title: Some("tests".to_string()),
                direction: Some(SplitDirection::Down),
            },
            UiAction {
                name: "agent".to_string(),
                command: "claude".to_string(),
                title: None,
                direction: None,
            },
        ];

        let lines = action_panel_lines(&actions, 1, None);
        assert!(lines[0].starts_with("  test"));
        assert!(lines[0].contains("title:tests"));
        assert!(lines[0].contains("dir:down"));
        assert!(lines[0].contains("cargo test"));
        assert!(lines[1].starts_with("> agent"));
        assert_eq!(
            action_panel_lines(&[], 0, Some("missing config")),
            vec!["error: missing config".to_string()]
        );
    }

    #[test]
    fn settings_panel_lines_include_theme_and_selection() {
        let lines = settings_panel_lines(SettingsView {
            theme: UiTheme::Daylight,
            workspace_second_line: UiWorkspaceSecondLine::Path,
            sidebar_collapsed: false,
            sidebar_responsive: true,
            sidebar_width: 24,
            prefix_label: "Ctrl-b",
            scroll_step: 5,
            cursor_blink: true,
            cursor_blink_ms: 1000,
            status_markers: "emoji",
            default_shell: "",
            default_cwd: "launch",
            mouse: true,
            tab_close_button: true,
            bell_on_attention: false,
            mobile_relay_enabled: false,
            mobile_relay_bind: "auto",
            mobile_relay_port: 4399,
            mobile_relay_allow_localhost: false,
            mobile_relay_allow_cgnat: true,
            selected: 0,
        });

        assert!(lines.len() >= 10);
        assert!(lines[0].spans[0].content.as_ref().contains("theme"));
        assert!(lines[0].spans[0].content.as_ref().contains("Daylight"));
        assert!(lines[1].spans[0]
            .content
            .as_ref()
            .contains("workspace line"));
        assert!(lines[2].spans[0].content.as_ref().contains("sidebar"));
        assert!(lines[3].spans[0]
            .content
            .as_ref()
            .contains("responsive layout"));
        assert!(lines[4].spans[0].content.as_ref().contains("sidebar width"));
        assert!(lines[4].spans[0].content.as_ref().contains("24"));
        assert!(lines.iter().any(|line| {
            line.spans
                .first()
                .map(|s| s.content.as_ref().contains("agent hooks"))
                .unwrap_or(false)
        }));
        assert!(lines.iter().any(|line| {
            line.spans
                .first()
                .map(|s| s.content.as_ref().contains("mobile relay"))
                .unwrap_or(false)
        }));
        assert!(lines.iter().any(|line| {
            line.spans
                .first()
                .map(|s| s.content.as_ref().contains("claude code"))
                .unwrap_or(false)
        }));
        assert!(lines.iter().any(|line| {
            line.spans
                .first()
                .map(|s| s.content.as_ref().contains("install all hooks"))
                .unwrap_or(false)
        }));
        assert_eq!(lines[0].spans[0].style.bg, Some(Color::Yellow));
    }

    #[test]
    fn workspace_second_line_path_uses_home_relative_display() {
        let mut session = mouse_test_session();
        let home = std::env::var("HOME").unwrap_or_else(|_| "/home/tester".to_string());
        session.workspaces[0].cwd = format!("{home}/code/vmux");

        assert_eq!(
            workspace_second_line_text(
                &session,
                &session.workspaces[0],
                UiWorkspaceSecondLine::Path,
            )
            .as_deref(),
            Some("~/code/vmux")
        );
    }

    #[test]
    fn workspace_second_line_cursor_uses_active_pane_position() {
        let mut session = mouse_test_session();
        let pane = session.panes.get_mut("pane-1").unwrap();
        pane.cursor_row = Some(2);
        pane.cursor_col = Some(4);

        assert_eq!(
            workspace_second_line_text(
                &session,
                &session.workspaces[0],
                UiWorkspaceSecondLine::Cursor,
            )
            .as_deref(),
            Some("cursor 3:5")
        );
    }

    #[test]
    fn agent_panel_lines_include_workspace_status_progress_and_command() {
        let mut session = Session::new("test");
        let mut pane = crate::model::Pane::new(
            "pane-1".to_string(),
            "claude --dangerously-skip-permissions".to_string(),
            SplitDirection::Right,
        );
        pane.title = "backend-agent".to_string();
        pane.surface_kind = crate::model::SurfaceKind::Agent;
        pane.status = crate::model::PaneStatus::Running;
        pane.agent_status = crate::model::AgentStatus::Busy;
        pane.progress = Some(42);
        pane.metadata
            .insert("task".to_string(), "auth-api".to_string());
        session.workspaces[0].panes.push("pane-1".to_string());
        session.workspaces[0].active_pane = Some("pane-1".to_string());
        session.panes.insert("pane-1".to_string(), pane);

        let entries = agent_panel_entries(&session);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].workspace_id, "ws-1");
        assert_eq!(entries[0].pane_id, "pane-1");

        let lines = agent_panel_lines(&session, 0);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with("> main pane-1:backend-agent"));
        assert!(lines[0].contains("[agent:running:busy 42%]"));
        assert!(lines[0].contains("{task=auth-api}"));
        assert!(lines[0].contains("claude --dangerously-skip-permissions"));
    }

    #[test]
    fn command_palette_lines_include_selection_and_descriptions() {
        let entries = filter_command_entries("");
        let lines = command_palette_lines(&entries, 0, "Ctrl-b", 80, UiTheme::Midnight);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert!(!texts.is_empty());
        assert!(texts[0].starts_with("> split-right"));
        assert!(texts[0].contains("open a pane to the right"));
        // The shortcut column shows the configured prefix + suffix key.
        assert!(texts[0].contains("Ctrl-b %"));
        assert!(texts.iter().any(|line| line.contains("new-workspace")));
        assert!(texts.iter().any(|line| line.contains("duplicate-pane")));
        assert!(texts.iter().any(|line| line.contains("restart-pane")));
        assert!(texts.iter().any(|line| line.contains("clear-pane")));
        assert!(texts.iter().any(|line| line.contains("copy-pane")));
        assert!(texts.iter().any(|line| line.contains("paste")));
        assert!(texts.iter().any(|line| line.contains("status-busy")));
        assert!(texts.iter().any(|line| line.contains("status-attention")));
        assert!(texts.iter().any(|line| line.contains("status-done")));
        assert!(texts.iter().any(|line| line.contains("status-idle")));
        assert!(texts.iter().any(|line| line.contains("notifications")));
    }

    #[test]
    fn command_palette_lines_report_no_matches_when_empty() {
        let lines = command_palette_lines(&[], 0, "Ctrl-b", 80, UiTheme::Midnight);
        assert_eq!(lines.len(), 1);
        assert!(line_text(&lines[0]).contains("(no matches)"));
    }

    #[test]
    fn palette_shortcut_matches_prefix_handler() {
        // Bound actions expose their prefix-suffix key.
        assert_eq!(
            palette_shortcut(CommandPaletteAction::SplitRight),
            Some("%")
        );
        assert_eq!(
            palette_shortcut(CommandPaletteAction::ToggleActions),
            Some("A")
        );
        assert_eq!(palette_shortcut(CommandPaletteAction::FocusLeft), Some("h"));
        // Actions with no prefix binding report no shortcut.
        assert_eq!(palette_shortcut(CommandPaletteAction::DuplicatePane), None);
        assert_eq!(palette_shortcut(CommandPaletteAction::Settings), None);
    }

    #[test]
    fn prefix_bindings_agree_between_handler_and_shortcut_column() {
        // Every table entry round-trips: the key resolves to its action and the
        // action resolves back to the same label, so the prefix handler and the
        // palette shortcut column can never drift.
        for (code, label, action) in prefix_action_bindings() {
            assert_eq!(prefix_key_action(*code), Some(*action));
            assert_eq!(palette_shortcut(*action), Some(*label));
        }
    }

    #[test]
    fn command_filter_is_case_insensitive_subsequence() {
        // Uppercase query still matches, and a scattered subsequence works.
        let entries = filter_command_entries("SPLT");
        assert!(entries.iter().any(|entry| entry.name == "split-right"));
        // Non-matching query yields nothing.
        assert!(filter_command_entries("zzzzq").is_empty());
    }

    #[test]
    fn command_filter_ranks_name_matches_first() {
        // "status" appears in several names; those should rank ahead of any
        // entry that only matches in its description.
        let entries = filter_command_entries("status");
        assert!(entries[0].name.starts_with("status-"));
    }

    #[test]
    fn empty_filter_returns_all_entries_in_order() {
        let all = command_palette_entries();
        let filtered = filter_command_entries("");
        assert_eq!(filtered.len(), all.len());
        assert_eq!(filtered[0].name, all[0].name);
        assert_eq!(filtered.last().unwrap().name, all.last().unwrap().name);
    }

    #[test]
    fn filtered_selection_runs_matching_entry() {
        // With a filter applied, the selected index addresses the filtered list,
        // so the entry chosen matches the intended command.
        let entries = filter_command_entries("resize");
        assert!(!entries.is_empty());
        assert!(entries.iter().all(|entry| entry.name.contains("resize")));
        // Every resize action carries an arrow shortcut from the shared table.
        assert!(palette_shortcut(entries[0].action).is_some());
    }

    #[test]
    fn context_menu_lines_include_mouse_pane_actions() {
        let lines = context_menu_lines(1, Some("pane-1"));
        assert_eq!(lines.len(), 5);
        assert!(lines[0].starts_with("  copy-pane"));
        assert!(lines[0].contains("pane-1"));
        assert!(lines[1].starts_with("> paste"));
        assert!(lines.iter().any(|line| line.contains("split-right")));
        assert!(lines.iter().any(|line| line.contains("split-down")));
        assert!(lines.iter().any(|line| line.contains("clear-pane")));
    }

    #[test]
    fn session_footer_counts_agent_states() {
        let mut session = Session::new("test");
        let mut pane_1 = crate::model::Pane::new(
            "pane-1".to_string(),
            "claude".to_string(),
            SplitDirection::Right,
        );
        pane_1.status = crate::model::PaneStatus::Running;
        pane_1.agent_status = crate::model::AgentStatus::Busy;
        let mut pane_2 = crate::model::Pane::new(
            "pane-2".to_string(),
            "npm test".to_string(),
            SplitDirection::Right,
        );
        pane_2.status = crate::model::PaneStatus::Exited;
        pane_2.agent_status = crate::model::AgentStatus::Done;
        let mut pane_3 = crate::model::Pane::new(
            "pane-3".to_string(),
            "claude".to_string(),
            SplitDirection::Right,
        );
        pane_3.status = crate::model::PaneStatus::Running;
        pane_3.agent_status = crate::model::AgentStatus::Attention;
        session.panes.insert("pane-1".to_string(), pane_1);
        session.panes.insert("pane-2".to_string(), pane_2);
        session.panes.insert("pane-3".to_string(), pane_3);
        session.notifications.push(crate::model::Notification {
            time: 1,
            pane: Some("pane-1".to_string()),
            workspace: None,
            status: None,
            color: None,
            clear: false,
            message: "working".to_string(),
        });

        assert_eq!(
            session_footer(&session, UiMode::Panes, 0),
            " session:test workspaces:1 panes:3 running:2 busy:1 attention:1 done:1 error:0 notes:1 "
        );
        assert_eq!(
            session_footer(&session, UiMode::Notifications, 0),
            " session:test workspaces:1 panes:3 running:2 busy:1 attention:1 done:1 error:0 notes:1  notifications:1/1 j/k select Enter jump Esc close "
        );
        assert_eq!(
            session_footer(&session, UiMode::Actions, 0),
            " session:test workspaces:1 panes:3 running:2 busy:1 attention:1 done:1 error:0 notes:1  actions j/k select Enter run Esc close "
        );
        assert_eq!(
            session_footer(&session, UiMode::Commands, 0),
            " session:test workspaces:1 panes:3 running:2 busy:1 attention:1 done:1 error:0 notes:1  commands type to filter ↑/↓ select Enter run Esc clear/close "
        );
        session.workspaces[0].zoomed_pane = Some("pane-1".to_string());
        assert_eq!(
            session_footer(&session, UiMode::Panes, 0),
            " session:test workspaces:1 panes:3 running:2 busy:1 attention:1 done:1 error:0 notes:1  zoom:pane-1 "
        );
    }
}

fn compact_path(path: &str) -> String {
    let name = std::path::Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path);
    if name.is_empty() {
        String::new()
    } else {
        format!("({name})")
    }
}

fn short_path(path: &str) -> String {
    let home = std::env::var("HOME").ok().filter(|value| !value.is_empty());
    if let Some(home) = home {
        if path == home {
            return "~".to_string();
        }
        if let Some(rest) = path.strip_prefix(&format!("{home}/")) {
            return format!("~/{rest}");
        }
    }
    path.to_string()
}
