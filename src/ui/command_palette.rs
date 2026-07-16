//! Command palette: actions, entries, filtering, and draw.

use crossterm::event::KeyCode;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use super::theme::{ThemePalette, UiTheme};
use super::{pad_to_width, panel_block, selected_row_style, truncate_to_width};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommandPaletteAction {
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
    TogglePorts,
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

/// Visual group for the command palette. Order here is display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum CommandPaletteSection {
    Panes,
    Focus,
    Tabs,
    Workspaces,
    Agent,
    Panels,
}

impl CommandPaletteSection {
    pub(crate) fn all() -> &'static [Self] {
        &[
            Self::Panes,
            Self::Focus,
            Self::Tabs,
            Self::Workspaces,
            Self::Agent,
            Self::Panels,
        ]
    }

    pub(crate) fn title(self) -> &'static str {
        match self {
            Self::Panes => "PANES",
            Self::Focus => "FOCUS & RESIZE",
            Self::Tabs => "TABS",
            Self::Workspaces => "WORKSPACES",
            Self::Agent => "AGENT STATUS",
            Self::Panels => "PANELS & UI",
        }
    }

    pub(crate) fn icon(self) -> &'static str {
        match self {
            Self::Panes => "⧉",
            Self::Focus => "⇔",
            Self::Tabs => "☰",
            Self::Workspaces => "▣",
            Self::Agent => "●",
            Self::Panels => "⚙",
        }
    }

    /// Accent color for this section's header and command names.
    pub(crate) fn color(self, palette: ThemePalette) -> Color {
        match self {
            Self::Panes => palette.command,
            Self::Focus => palette.success,
            Self::Tabs => palette.active,
            Self::Workspaces => palette.warning,
            Self::Agent => palette.hover,
            Self::Panels => palette.active,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct CommandPaletteEntry {
    pub(crate) name: &'static str,
    pub(crate) description: &'static str,
    pub(crate) action: CommandPaletteAction,
    pub(crate) section: CommandPaletteSection,
    /// Optional leading glyph shown next to the command name.
    pub(crate) icon: &'static str,
}

/// Single source of truth mapping prefix-suffix keys to command-palette
/// actions. Both the prefix-key handler and the palette shortcut column are
/// driven by this table so the displayed shortcut can never drift from the
/// binding that actually runs. Each tuple is `(key code, display label,
/// action)`. Prefix keys with no palette action (detach `q`, sidebar `B`, open
/// palette `P`, jump-notification `u`, `Tab`, scroll `PageUp`/`PageDown`/
/// `Home`) are intentionally handled outside this table.
pub(crate) fn prefix_action_bindings() -> &'static [(KeyCode, &'static str, CommandPaletteAction)] {
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
        (KeyCode::Char('o'), "o", TogglePorts),
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
pub(crate) fn prefix_key_action(code: KeyCode) -> Option<CommandPaletteAction> {
    prefix_action_bindings()
        .iter()
        .find(|(binding, _, _)| *binding == code)
        .map(|(_, _, action)| *action)
}

/// The prefix-suffix key label for a palette action, e.g. `Some("%")`, or
/// `None` when the action has no prefix-key binding.
pub(crate) fn palette_shortcut(action: CommandPaletteAction) -> Option<&'static str> {
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
pub(crate) fn command_filter_score(query: &str, entry: &CommandPaletteEntry) -> Option<i32> {
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

pub(crate) fn command_palette_entries() -> Vec<CommandPaletteEntry> {
    use CommandPaletteSection::*;
    vec![
        // ── Panes ──────────────────────────────────────────────
        CommandPaletteEntry {
            name: "split-right",
            description: "open a pane to the right",
            action: CommandPaletteAction::SplitRight,
            section: Panes,
            icon: "▸",
        },
        CommandPaletteEntry {
            name: "split-down",
            description: "open a pane below",
            action: CommandPaletteAction::SplitDown,
            section: Panes,
            icon: "▾",
        },
        CommandPaletteEntry {
            name: "kill-pane",
            description: "kill the active pane",
            action: CommandPaletteAction::KillPane,
            section: Panes,
            icon: "✕",
        },
        CommandPaletteEntry {
            name: "duplicate-pane",
            description: "duplicate active pane to the right",
            action: CommandPaletteAction::DuplicatePane,
            section: Panes,
            icon: "⧉",
        },
        CommandPaletteEntry {
            name: "restart-pane",
            description: "restart the active pane",
            action: CommandPaletteAction::RestartPane,
            section: Panes,
            icon: "↻",
        },
        CommandPaletteEntry {
            name: "clear-pane",
            description: "clear active pane capture",
            action: CommandPaletteAction::ClearPane,
            section: Panes,
            icon: "⌫",
        },
        CommandPaletteEntry {
            name: "copy-pane",
            description: "copy active pane screen",
            action: CommandPaletteAction::CopyPane,
            section: Panes,
            icon: "⎘",
        },
        CommandPaletteEntry {
            name: "paste",
            description: "paste clipboard into active pane",
            action: CommandPaletteAction::PastePane,
            section: Panes,
            icon: "📋",
        },
        CommandPaletteEntry {
            name: "zoom-pane",
            description: "toggle active pane zoom",
            action: CommandPaletteAction::ToggleZoom,
            section: Panes,
            icon: "⛶",
        },
        // ── Focus & resize ─────────────────────────────────────
        CommandPaletteEntry {
            name: "focus-left",
            description: "focus pane left",
            action: CommandPaletteAction::FocusLeft,
            section: Focus,
            icon: "←",
        },
        CommandPaletteEntry {
            name: "focus-right",
            description: "focus pane right",
            action: CommandPaletteAction::FocusRight,
            section: Focus,
            icon: "→",
        },
        CommandPaletteEntry {
            name: "focus-up",
            description: "focus pane above",
            action: CommandPaletteAction::FocusUp,
            section: Focus,
            icon: "↑",
        },
        CommandPaletteEntry {
            name: "focus-down",
            description: "focus pane below",
            action: CommandPaletteAction::FocusDown,
            section: Focus,
            icon: "↓",
        },
        CommandPaletteEntry {
            name: "resize-left",
            description: "resize split left",
            action: CommandPaletteAction::ResizeLeft,
            section: Focus,
            icon: "⇐",
        },
        CommandPaletteEntry {
            name: "resize-right",
            description: "resize split right",
            action: CommandPaletteAction::ResizeRight,
            section: Focus,
            icon: "⇒",
        },
        CommandPaletteEntry {
            name: "resize-up",
            description: "resize split up",
            action: CommandPaletteAction::ResizeUp,
            section: Focus,
            icon: "⇑",
        },
        CommandPaletteEntry {
            name: "resize-down",
            description: "resize split down",
            action: CommandPaletteAction::ResizeDown,
            section: Focus,
            icon: "⇓",
        },
        // ── Tabs ───────────────────────────────────────────────
        CommandPaletteEntry {
            name: "new-tab",
            description: "open a new tab in the active workspace",
            action: CommandPaletteAction::NewTab,
            section: Tabs,
            icon: "+",
        },
        CommandPaletteEntry {
            name: "next-tab",
            description: "activate next workspace tab",
            action: CommandPaletteAction::NextTab,
            section: Tabs,
            icon: "»",
        },
        CommandPaletteEntry {
            name: "previous-tab",
            description: "activate previous workspace tab",
            action: CommandPaletteAction::PreviousTab,
            section: Tabs,
            icon: "«",
        },
        // ── Workspaces ─────────────────────────────────────────
        CommandPaletteEntry {
            name: "new-workspace",
            description: "create a workspace",
            action: CommandPaletteAction::NewWorkspace,
            section: Workspaces,
            icon: "+",
        },
        CommandPaletteEntry {
            name: "close-workspace",
            description: "close the active workspace",
            action: CommandPaletteAction::CloseWorkspace,
            section: Workspaces,
            icon: "✕",
        },
        CommandPaletteEntry {
            name: "next-workspace",
            description: "switch to the next workspace",
            action: CommandPaletteAction::NextWorkspace,
            section: Workspaces,
            icon: "»",
        },
        CommandPaletteEntry {
            name: "previous-workspace",
            description: "switch to the previous workspace",
            action: CommandPaletteAction::PreviousWorkspace,
            section: Workspaces,
            icon: "«",
        },
        // ── Agent status ───────────────────────────────────────
        CommandPaletteEntry {
            name: "status-busy",
            description: "mark active agent busy",
            action: CommandPaletteAction::StatusBusy,
            section: Agent,
            icon: "🔄",
        },
        CommandPaletteEntry {
            name: "status-attention",
            description: "mark active agent needs input",
            action: CommandPaletteAction::StatusAttention,
            section: Agent,
            icon: "🙋",
        },
        CommandPaletteEntry {
            name: "status-done",
            description: "mark active agent done",
            action: CommandPaletteAction::StatusDone,
            section: Agent,
            icon: "✅",
        },
        CommandPaletteEntry {
            name: "status-idle",
            description: "mark active agent idle",
            action: CommandPaletteAction::StatusIdle,
            section: Agent,
            icon: "○",
        },
        // ── Panels & UI ────────────────────────────────────────
        CommandPaletteEntry {
            name: "notifications",
            description: "open the notification panel",
            action: CommandPaletteAction::ToggleNotifications,
            section: Panels,
            icon: "🔔",
        },
        CommandPaletteEntry {
            name: "ports",
            description: "list listening ports · copy ssh -L · Tailscale forward",
            action: CommandPaletteAction::TogglePorts,
            section: Panels,
            icon: "🔌",
        },
        CommandPaletteEntry {
            name: "actions",
            description: "open project actions",
            action: CommandPaletteAction::ToggleActions,
            section: Panels,
            icon: "⚡",
        },
        CommandPaletteEntry {
            name: "settings",
            description: "open UI theme and behavior settings",
            action: CommandPaletteAction::Settings,
            section: Panels,
            icon: "⚙",
        },
    ]
}

/// Palette entries matching `filter`, in display order: section groups first,
/// then score (when filtering) / declaration order within each section.
pub(crate) fn filter_command_entries(filter: &str) -> Vec<CommandPaletteEntry> {
    let mut scored: Vec<(i32, usize, CommandPaletteEntry)> = command_palette_entries()
        .into_iter()
        .enumerate()
        .filter_map(|(index, entry)| {
            command_filter_score(filter, &entry).map(|score| (score, index, entry))
        })
        .collect();
    // Keep section-major order so colorful headers stay stable while filtering.
    let mut ordered = Vec::with_capacity(scored.len());
    for section in CommandPaletteSection::all() {
        let mut group: Vec<(i32, usize, CommandPaletteEntry)> = scored
            .extract_if(.., |(_, _, entry)| entry.section == *section)
            .collect();
        if filter.trim().is_empty() {
            group.sort_by_key(|(_, index, _)| *index);
        } else {
            group.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        }
        ordered.extend(group.into_iter().map(|(_, _, entry)| entry));
    }
    ordered
}

pub(crate) fn command_palette_section_header(
    section: CommandPaletteSection,
    width: usize,
    palette: ThemePalette,
) -> Line<'static> {
    let accent = section.color(palette);
    let label = format!(" {} {} ", section.icon(), section.title());
    let label_w = UnicodeWidthStr::width(label.as_str());
    let rule_cols = width.saturating_sub(1 + label_w + 1);
    let rule = if rule_cols == 0 {
        String::new()
    } else {
        format!(" {}", "─".repeat(rule_cols.saturating_sub(1).max(1)))
    };
    // Keep header on one row: clamp total display width.
    let mut line = Line::from(vec![
        Span::styled(" ", Style::default().fg(accent)),
        Span::styled(
            label,
            Style::default()
                .fg(accent)
                .bg(palette.surface_alt)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(rule, Style::default().fg(accent)),
    ]);
    clamp_line_to_width(&mut line, width, Style::default().fg(accent));
    line
}

/// One command row, forced to a single line of exactly `width` columns.
pub(crate) fn command_palette_entry_line(
    entry: &CommandPaletteEntry,
    active: bool,
    prefix_label: &str,
    width: usize,
    palette: ThemePalette,
) -> Line<'static> {
    let section_color = entry.section.color(palette);
    let shortcut = palette_shortcut(entry.action)
        .map(|key| format!("{prefix_label} {key}"))
        .unwrap_or_else(|| "·".to_string());
    let shortcut_w = UnicodeWidthStr::width(shortcut.as_str());

    // Layout: [2 prefix][icon+1][name 16][1][desc flex][pad][shortcut]
    // Prefix is always width 2 so active/inactive columns align.
    let prefix = if active { "› " } else { "  " };
    let icon = entry.icon;
    let icon_w = UnicodeWidthStr::width(icon).max(1);
    const NAME_COLS: usize = 16;
    let name = pad_to_width(&truncate_to_width(entry.name, NAME_COLS), NAME_COLS);

    // Budget for description: everything left after fixed columns + shortcut.
    // prefix(2) + icon + space(1) + name + space(1) + shortcut
    let fixed = 2 + icon_w + 1 + NAME_COLS + 1 + shortcut_w;
    let desc_budget = width.saturating_sub(fixed);
    let desc = if desc_budget == 0 {
        String::new()
    } else {
        truncate_to_width(entry.description, desc_budget)
    };
    let desc_w = UnicodeWidthStr::width(desc.as_str());
    let used = 2 + icon_w + 1 + NAME_COLS + 1 + desc_w + shortcut_w;
    let pad = width.saturating_sub(used);

    if active {
        let base = selected_row_style(palette);
        let mut line = Line::from(vec![
            Span::styled(
                prefix.to_string(),
                Style::default()
                    .fg(section_color)
                    .bg(palette.hover)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{icon} "),
                Style::default()
                    .fg(section_color)
                    .bg(palette.hover)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(name, base.add_modifier(Modifier::BOLD)),
            Span::styled(format!(" {desc}"), base),
            Span::styled(" ".repeat(pad), base),
            Span::styled(
                shortcut,
                Style::default()
                    .fg(palette.on_bright)
                    .bg(palette.hover)
                    .add_modifier(Modifier::BOLD),
            ),
        ]);
        clamp_line_to_width(&mut line, width, base);
        line
    } else {
        let mut line = Line::from(vec![
            Span::styled(prefix.to_string(), Style::default().fg(palette.muted)),
            Span::styled(format!("{icon} "), Style::default().fg(section_color)),
            Span::styled(
                name,
                Style::default()
                    .fg(section_color)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" {desc}"), Style::default().fg(palette.text)),
            Span::styled(" ".repeat(pad), Style::default()),
            Span::styled(shortcut, Style::default().fg(palette.muted)),
        ]);
        clamp_line_to_width(&mut line, width, Style::default());
        line
    }
}

/// Ensure a multi-span line does not exceed `width` display columns.
fn clamp_line_to_width(line: &mut Line<'static>, width: usize, pad_style: Style) {
    let mut used = 0usize;
    let mut keep = 0usize;
    for (i, span) in line.spans.iter().enumerate() {
        let w = UnicodeWidthStr::width(span.content.as_ref());
        if used + w > width {
            // Truncate this span to the remaining budget.
            let remain = width.saturating_sub(used);
            let content = truncate_to_width(span.content.as_ref(), remain);
            let style = span.style;
            line.spans.truncate(i);
            if remain > 0 {
                line.spans.push(Span::styled(content, style));
            }
            keep = line.spans.len();
            used = width;
            break;
        }
        used += w;
        keep = i + 1;
    }
    if keep < line.spans.len() {
        line.spans.truncate(keep);
    }
    if used < width {
        line.spans
            .push(Span::styled(" ".repeat(width - used), pad_style));
    }
}

pub(crate) fn command_palette_lines(
    entries: &[CommandPaletteEntry],
    selected: usize,
    prefix_label: &str,
    width: u16,
    theme: UiTheme,
    max_lines: usize,
) -> Vec<Line<'static>> {
    let palette = theme.palette();
    if entries.is_empty() {
        return vec![
            Line::from(Span::styled(
                "  (no matches)".to_string(),
                Style::default().fg(palette.muted),
            )),
            Line::from(Span::styled(
                "  try a shorter filter · Esc clears".to_string(),
                Style::default().fg(palette.muted),
            )),
        ];
    }
    let width = (width as usize).max(20);
    let max_lines = max_lines.max(1);
    let selected = selected.min(entries.len() - 1);

    // Build the full list (each command is exactly one row), then take a
    // viewport window that keeps the selection visible.
    let mut all: Vec<Line<'static>> = Vec::new();
    let mut selected_line = 0usize;
    let mut last_section: Option<CommandPaletteSection> = None;
    for (command_index, entry) in entries.iter().enumerate() {
        if last_section != Some(entry.section) {
            if last_section.is_some() {
                all.push(Line::from(Span::raw("")));
            }
            all.push(command_palette_section_header(
                entry.section,
                width,
                palette,
            ));
            last_section = Some(entry.section);
        }
        if command_index == selected {
            selected_line = all.len();
        }
        all.push(command_palette_entry_line(
            entry,
            command_index == selected,
            prefix_label,
            width,
            palette,
        ));
    }

    if all.len() <= max_lines {
        return all;
    }

    // Reserve rows for scroll hints when content is clipped.
    // First assume both hints, then shrink if one side is flush.
    let mut start = selected_line.saturating_sub((max_lines.saturating_sub(2)) / 3);
    let mut end = start + max_lines.saturating_sub(2);
    if end > all.len() {
        end = all.len();
        start = end.saturating_sub(max_lines.saturating_sub(2));
    }
    // Pull window so selected is inside [start, end).
    if selected_line < start {
        start = selected_line;
        end = (start + max_lines.saturating_sub(2)).min(all.len());
    } else if selected_line >= end {
        end = selected_line + 1;
        start = end.saturating_sub(max_lines.saturating_sub(2));
    }

    let show_above = start > 0;
    let show_below = end < all.len();
    // Grow the body if a hint slot is unused.
    let hints = usize::from(show_above) + usize::from(show_below);
    let body = max_lines.saturating_sub(hints).max(1);
    if end - start < body {
        let extra = body - (end - start);
        if !show_below {
            start = start.saturating_sub(extra);
        } else if !show_above {
            end = (end + extra).min(all.len());
        } else {
            // Prefer extending downward, then upward.
            let down = extra.min(all.len() - end);
            end += down;
            start = start.saturating_sub(extra - down);
        }
    }
    // Re-check selected after growth.
    if selected_line < start {
        start = selected_line;
    }
    if selected_line >= end {
        end = selected_line + 1;
        start = end.saturating_sub(body);
    }
    end = end.min(all.len());
    start = start.min(end.saturating_sub(1));

    let mut out = Vec::with_capacity(max_lines);
    if start > 0 {
        out.push(Line::from(Span::styled(
            "  ↑ more above".to_string(),
            Style::default().fg(palette.muted),
        )));
    }
    out.extend(all[start..end].iter().cloned());
    if end < all.len() {
        out.push(Line::from(Span::styled(
            "  ↓ more below".to_string(),
            Style::default().fg(palette.muted),
        )));
    }
    if out.len() > max_lines {
        out.truncate(max_lines);
    }
    out
}

pub(crate) fn draw_commands(
    frame: &mut ratatui::Frame,
    area: Rect,
    selected: usize,
    filter: &str,
    prefix_label: &str,
    theme: UiTheme,
) {
    let palette = theme.palette();
    let inner_width = area.width.saturating_sub(2);
    let inner_height = area.height.saturating_sub(2) as usize;
    let entries = filter_command_entries(filter);
    let match_count = entries.len();
    let total_count = command_palette_entries().len();

    // Chrome: title + search + blank + blank before footer + footer = 5 lines.
    const CHROME: usize = 5;
    let list_budget = inner_height.saturating_sub(CHROME).max(3);

    let mut lines = Vec::new();

    // Title strip: counts + filter hint.
    let count_label = if filter.is_empty() {
        format!("{total_count} commands")
    } else {
        format!("{match_count} of {total_count}")
    };
    lines.push(Line::from(vec![
        Span::styled(
            " ⌘  Command palette ",
            Style::default()
                .fg(palette.active)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("· {count_label}"),
            Style::default().fg(palette.muted),
        ),
    ]));

    // Search field with filled background so it reads as an input box.
    let placeholder = if filter.is_empty() {
        "type to filter…".to_string()
    } else {
        filter.to_string()
    };
    let search_fg = if filter.is_empty() {
        palette.muted
    } else {
        palette.text
    };
    let search_pad = (inner_width as usize)
        .saturating_sub(4)
        .saturating_sub(UnicodeWidthStr::width(placeholder.as_str()))
        .saturating_sub(1);
    lines.push(Line::from(vec![
        Span::styled(
            " 🔍 ",
            Style::default().fg(palette.active).bg(palette.surface_alt),
        ),
        Span::styled(
            format!("{placeholder}▏{}", " ".repeat(search_pad)),
            Style::default().fg(search_fg).bg(palette.surface_alt),
        ),
    ]));
    lines.push(Line::from(Span::raw("")));

    lines.extend(command_palette_lines(
        &entries,
        selected,
        prefix_label,
        inner_width,
        theme,
        list_budget,
    ));

    // Footer with key hints.
    lines.push(Line::from(Span::raw("")));
    lines.push(Line::from(vec![
        Span::styled(
            " ↑↓",
            Style::default()
                .fg(palette.active)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" move  ", Style::default().fg(palette.muted)),
        Span::styled(
            "Enter",
            Style::default()
                .fg(palette.success)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" run  ", Style::default().fg(palette.muted)),
        Span::styled(
            "Esc",
            Style::default()
                .fg(palette.warning)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" close  ", Style::default().fg(palette.muted)),
        Span::styled(
            format!("{prefix_label} <key>"),
            Style::default()
                .fg(palette.command)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" shortcut", Style::default().fg(palette.muted)),
    ]));

    // No wrap: each command is one row (width-clamped). Wrap would split
    // name/desc/shortcut across lines and hide later entries.
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel_block(" commands ", palette))
            .style(Style::default().bg(palette.surface)),
        area,
    );
}
