//! Layout skins + color palettes for the attach UI.
//!
//! - **Layout** (`ui.layout`) changes structure and chrome treatment: sidebar
//!   density, control-bar button style, pane frames, tab chips.
//! - **Colors** (`ui.colors`, legacy `ui.theme`) only change the palette.

use ratatui::style::Color;
use ratatui::widgets::Borders;

/// Screen **structure** (not colors). Selected via `ui.layout`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UiLayout {
    /// Full chrome — box panes, labeled toolbar, classic sidebar fill.
    Classic,
    /// Dense IDE: icon toolbar, left-accent sidebar selection, tight spacing.
    Compact,
    /// Focus mode: only the active pane is framed; ghost chrome elsewhere.
    Minimal,
    /// Product UI: surface rail, pill buttons, left-edge active pane accent.
    Flat,
    /// Content-first: almost no chrome — text-only sidebar, bare icons, no frames.
    Zen,
}

/// How the workspace rail is painted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SidebarStyle {
    /// Header " vmux ", right rule, full-row accent fill on active.
    Classic,
    /// Surface-tinted rail, left `▎` accent on active (no full-row paint).
    Compact,
    /// Soft surface rail, no rule; active = bold + surface_alt row.
    Pill,
    /// No rule, no fills — active is bold text only (zen).
    Ghost,
}

/// How control-bar actions are presented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ControlBarStyle {
    /// Icon + full labels on a surface strip; session footer on row 2.
    Labeled,
    /// Equal-width icon tiles on a surface strip (dense / touch).
    Icons,
    /// Spaced surface_alt "pills" with short labels on the main background.
    Pills,
    /// Bare icons on the main background — no strip fill (zen).
    Ghost,
}

/// How workspace tabs look.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TabBarStyle {
    /// Solid fill chips (classic product tabs).
    Chips,
    /// Underline active tab; muted inactive text.
    Underline,
    /// Plain text; bold active, no fills.
    Ghost,
}

/// How pane outer frames are drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PaneFrameStyle {
    /// Full box borders on every pane.
    Box,
    /// Full box only on the focused (or danger) pane.
    ActiveBox,
    /// Single left edge on the focused pane only (flat product).
    LeftAccent,
    /// No pane borders at all.
    None,
}

/// Concrete metrics + style choices derived from [`UiLayout`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct LayoutChrome {
    /// Rows reserved at the bottom for control chrome (1 or 2).
    pub control_bar_height: u16,
    /// Draw the session status footer under the control buttons (classic only).
    pub show_session_footer: bool,
    /// Whether to draw the workspace tab strip when there is only one tab.
    pub hide_tab_bar_when_single: bool,
    /// Draw pane titles (can be restricted to the active pane).
    pub show_pane_titles: bool,
    /// If true, only the active pane gets a title.
    pub titles_active_only: bool,
    /// Corner split/close controls on panes.
    pub show_pane_controls: bool,
    /// 1-cell gap between split children.
    pub split_gap: u16,
    /// Sidebar list treatment.
    pub sidebar_style: SidebarStyle,
    /// Control-bar button treatment.
    pub control_bar_style: ControlBarStyle,
    /// Workspace tab strip treatment.
    pub tab_bar_style: TabBarStyle,
    /// Pane frame treatment.
    pub pane_frame: PaneFrameStyle,
    /// Dim horizontal rule above the control bar.
    pub control_bar_separator: bool,
    /// Sidebar header text when expanded (empty = no header branding).
    pub sidebar_header: &'static str,
}

impl LayoutChrome {
    /// Borders for a pane given focus / danger.
    pub(crate) fn pane_borders(self, active: bool, danger: bool) -> Borders {
        if danger {
            return Borders::ALL;
        }
        match self.pane_frame {
            PaneFrameStyle::Box => Borders::ALL,
            PaneFrameStyle::ActiveBox => {
                if active {
                    Borders::ALL
                } else {
                    Borders::NONE
                }
            }
            PaneFrameStyle::LeftAccent => {
                if active {
                    Borders::LEFT
                } else {
                    Borders::NONE
                }
            }
            PaneFrameStyle::None => Borders::NONE,
        }
    }

    /// Whether the sidebar paints a right edge rule.
    pub(crate) fn sidebar_border(self) -> bool {
        matches!(
            self.sidebar_style,
            SidebarStyle::Classic | SidebarStyle::Compact
        )
    }
}

impl UiLayout {
    pub(crate) fn from_name(name: &str) -> Self {
        match name.trim().to_ascii_lowercase().as_str() {
            "compact" | "dense" => Self::Compact,
            "minimal" | "focus" => Self::Minimal,
            "flat" | "product" => Self::Flat,
            "zen" | "immersive" => Self::Zen,
            _ => Self::Classic,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Classic => "classic",
            Self::Compact => "compact",
            Self::Minimal => "minimal",
            Self::Flat => "flat",
            Self::Zen => "zen",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Classic => "Classic",
            Self::Compact => "Compact",
            Self::Minimal => "Minimal",
            Self::Flat => "Flat",
            Self::Zen => "Zen",
        }
    }

    /// Short blurb for settings / docs.
    pub(crate) fn blurb(self) -> &'static str {
        match self {
            Self::Classic => "full boxes · labeled bar · filled sidebar",
            Self::Compact => "dense icons · left accent rail · tight",
            Self::Minimal => "frame active only · ghost chrome",
            Self::Flat => "product rail · pill buttons · left accent",
            Self::Zen => "content first · bare icons · no frames",
        }
    }

    pub(crate) fn all() -> &'static [Self] {
        &[
            Self::Classic,
            Self::Compact,
            Self::Minimal,
            Self::Flat,
            Self::Zen,
        ]
    }

    pub(crate) fn relative(self, delta: isize) -> Self {
        let items = Self::all();
        let current = items.iter().position(|item| *item == self).unwrap_or(0);
        let next = (current as isize + delta).rem_euclid(items.len() as isize) as usize;
        items[next]
    }

    pub(crate) fn chrome(self) -> LayoutChrome {
        match self {
            Self::Classic => LayoutChrome {
                control_bar_height: 2,
                show_session_footer: true,
                hide_tab_bar_when_single: false,
                show_pane_titles: true,
                titles_active_only: false,
                show_pane_controls: true,
                split_gap: 0,
                sidebar_style: SidebarStyle::Classic,
                control_bar_style: ControlBarStyle::Labeled,
                tab_bar_style: TabBarStyle::Chips,
                pane_frame: PaneFrameStyle::Box,
                control_bar_separator: false,
                sidebar_header: " vmux ",
            },
            Self::Compact => LayoutChrome {
                control_bar_height: 1,
                show_session_footer: false,
                hide_tab_bar_when_single: false,
                show_pane_titles: true,
                titles_active_only: false,
                show_pane_controls: true,
                split_gap: 0,
                sidebar_style: SidebarStyle::Compact,
                control_bar_style: ControlBarStyle::Icons,
                tab_bar_style: TabBarStyle::Chips,
                pane_frame: PaneFrameStyle::Box,
                control_bar_separator: true,
                sidebar_header: " · ",
            },
            Self::Minimal => LayoutChrome {
                control_bar_height: 1,
                show_session_footer: false,
                hide_tab_bar_when_single: true,
                show_pane_titles: true,
                titles_active_only: true,
                show_pane_controls: false,
                split_gap: 0,
                sidebar_style: SidebarStyle::Ghost,
                control_bar_style: ControlBarStyle::Ghost,
                tab_bar_style: TabBarStyle::Underline,
                pane_frame: PaneFrameStyle::ActiveBox,
                control_bar_separator: false,
                sidebar_header: "",
            },
            Self::Flat => LayoutChrome {
                control_bar_height: 1,
                show_session_footer: false,
                hide_tab_bar_when_single: true,
                show_pane_titles: true,
                titles_active_only: true,
                show_pane_controls: false,
                split_gap: 1,
                sidebar_style: SidebarStyle::Pill,
                control_bar_style: ControlBarStyle::Pills,
                tab_bar_style: TabBarStyle::Underline,
                pane_frame: PaneFrameStyle::LeftAccent,
                control_bar_separator: true,
                sidebar_header: "  ·  ",
            },
            Self::Zen => LayoutChrome {
                control_bar_height: 1,
                show_session_footer: false,
                hide_tab_bar_when_single: true,
                show_pane_titles: false,
                titles_active_only: false,
                show_pane_controls: false,
                split_gap: 0,
                sidebar_style: SidebarStyle::Ghost,
                control_bar_style: ControlBarStyle::Ghost,
                tab_bar_style: TabBarStyle::Ghost,
                pane_frame: PaneFrameStyle::None,
                control_bar_separator: false,
                sidebar_header: "",
            },
        }
    }

    /// Tab strip height for the current workspace (0 when hidden).
    pub(crate) fn tab_bar_height(self, tab_count: usize) -> u16 {
        let chrome = self.chrome();
        if tab_count == 0 {
            return 0;
        }
        if chrome.hide_tab_bar_when_single && tab_count <= 1 {
            0
        } else {
            1
        }
    }
}

/// Color palette only (legacy name: "theme"). Selected via `ui.colors`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UiTheme {
    /// Default dark chrome — also available as `classic`.
    Midnight,
    /// Flat product dark (Linear / Vercel-style slate + indigo).
    Modern,
    /// Warm low-contrast stone + soft accents for long sessions.
    Soft,
    /// Deep black with electric pink / cyan accents.
    Neon,
    /// Light warm paper / ink editorial.
    Paper,
    /// Near-monochrome zinc with a single cool accent.
    Minimal,
    Daylight,
    Contrast,
    Nord,
    Dracula,
    Gruvbox,
    Catppuccin,
    SolarizedDark,
    SolarizedLight,
    TokyoNight,
    Forest,
    RosePine,
    Ocean,
    Ember,
    Monokai,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UiWorkspaceSecondLine {
    Path,
    Details,
    Branch,
    Id,
    Status,
    Cursor,
    None,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ThemePalette {
    pub(crate) background: Color,
    pub(crate) surface: Color,
    pub(crate) surface_alt: Color,
    pub(crate) text: Color,
    pub(crate) muted: Color,
    pub(crate) border: Color,
    pub(crate) active: Color,
    pub(crate) hover: Color,
    pub(crate) danger: Color,
    pub(crate) success: Color,
    pub(crate) warning: Color,
    pub(crate) command: Color,
    /// Readable foreground for text drawn on an `active`-accent fill (selected
    /// sidebar row, active tab, active control button).
    pub(crate) on_accent: Color,
    /// Readable foreground for text drawn on a bright fill (hover, success,
    /// danger, warning) — black reads well on every theme's bright colors.
    pub(crate) on_bright: Color,
    /// Background used to highlight a text selection in a pane.
    pub(crate) selection: Color,
    /// Optional painted-cursor fill (unused while the host terminal caret is
    /// the active-pane marker — kept so themes stay complete if we re-offer a
    /// drawn cursor mode later).
    #[allow(dead_code)]
    pub(crate) cursor: Color,
    /// Glyph colour for a painted cursor on top of [`Self::cursor`].
    #[allow(dead_code)]
    pub(crate) on_cursor: Color,
}

impl UiTheme {
    pub(crate) fn from_name(name: &str) -> Self {
        match name.trim().to_ascii_lowercase().as_str() {
            "classic" | "midnight" => Self::Midnight,
            "modern" | "flat" => Self::Modern,
            "soft" | "warm-soft" => Self::Soft,
            "neon" | "cyber" => Self::Neon,
            "paper" | "editorial" => Self::Paper,
            "minimal" | "mono" | "zinc" => Self::Minimal,
            "daylight" => Self::Daylight,
            "contrast" => Self::Contrast,
            "nord" => Self::Nord,
            "dracula" => Self::Dracula,
            "gruvbox" => Self::Gruvbox,
            "catppuccin" | "mocha" => Self::Catppuccin,
            "solarized-dark" | "solarized_dark" | "solarizeddark" => Self::SolarizedDark,
            "solarized-light" | "solarized_light" | "solarizedlight" => Self::SolarizedLight,
            "tokyo-night" | "tokyo_night" | "tokyonight" => Self::TokyoNight,
            "forest" | "everforest" => Self::Forest,
            "rose-pine" | "rose_pine" | "rosepine" => Self::RosePine,
            "ocean" | "deep-ocean" => Self::Ocean,
            "ember" | "warm" => Self::Ember,
            "monokai" => Self::Monokai,
            // Unknown names fall back to the current default palette.
            _ => Self::TokyoNight,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Midnight => "midnight",
            Self::Modern => "modern",
            Self::Soft => "soft",
            Self::Neon => "neon",
            Self::Paper => "paper",
            Self::Minimal => "minimal",
            Self::Daylight => "daylight",
            Self::Contrast => "contrast",
            Self::Nord => "nord",
            Self::Dracula => "dracula",
            Self::Gruvbox => "gruvbox",
            Self::Catppuccin => "catppuccin",
            Self::SolarizedDark => "solarized-dark",
            Self::SolarizedLight => "solarized-light",
            Self::TokyoNight => "tokyo-night",
            Self::Forest => "forest",
            Self::RosePine => "rose-pine",
            Self::Ocean => "ocean",
            Self::Ember => "ember",
            Self::Monokai => "monokai",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            // Settings picker label; config key remains `midnight` (alias `classic`).
            Self::Midnight => "Classic",
            Self::Modern => "Modern",
            Self::Soft => "Soft",
            Self::Neon => "Neon",
            Self::Paper => "Paper",
            Self::Minimal => "Minimal",
            Self::Daylight => "Daylight",
            Self::Contrast => "Contrast",
            Self::Nord => "Nord",
            Self::Dracula => "Dracula",
            Self::Gruvbox => "Gruvbox",
            Self::Catppuccin => "Catppuccin",
            Self::SolarizedDark => "Solarized Dark",
            Self::SolarizedLight => "Solarized Light",
            Self::TokyoNight => "Tokyo Night",
            Self::Forest => "Forest",
            Self::RosePine => "Rose Pine",
            Self::Ocean => "Ocean",
            Self::Ember => "Ember",
            Self::Monokai => "Monokai",
        }
    }

    pub(crate) fn all() -> &'static [Self] {
        // Default first so Settings ←/→ cycles start from tokyo-night.
        &[
            Self::TokyoNight,
            Self::Midnight,
            Self::Modern,
            Self::Soft,
            Self::Neon,
            Self::Paper,
            Self::Minimal,
            Self::Daylight,
            Self::Contrast,
            Self::Nord,
            Self::Dracula,
            Self::Gruvbox,
            Self::Catppuccin,
            Self::SolarizedDark,
            Self::SolarizedLight,
            Self::Forest,
            Self::RosePine,
            Self::Ocean,
            Self::Ember,
            Self::Monokai,
        ]
    }

    pub(crate) fn relative(self, delta: isize) -> Self {
        let items = Self::all();
        let current = items.iter().position(|item| *item == self).unwrap_or(0);
        let next = (current as isize + delta).rem_euclid(items.len() as isize) as usize;
        items[next]
    }

    pub(crate) fn palette(self) -> ThemePalette {
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
            Self::Modern => ThemePalette {
                background: Color::Rgb(9, 9, 11),
                surface: Color::Rgb(24, 24, 27),
                surface_alt: Color::Rgb(39, 39, 42),
                text: Color::Rgb(250, 250, 250),
                muted: Color::Rgb(161, 161, 170),
                border: Color::Rgb(63, 63, 70),
                active: Color::Rgb(99, 102, 241),
                hover: Color::Rgb(129, 140, 248),
                danger: Color::Rgb(248, 113, 113),
                success: Color::Rgb(52, 211, 153),
                warning: Color::Rgb(251, 191, 36),
                command: Color::Rgb(167, 139, 250),
                on_accent: Color::Rgb(250, 250, 250),
                on_bright: Color::Black,
                selection: Color::Rgb(39, 39, 42),
                cursor: Color::Rgb(99, 102, 241),
                on_cursor: Color::Rgb(250, 250, 250),
            },
            Self::Soft => ThemePalette {
                background: Color::Rgb(28, 25, 23),
                surface: Color::Rgb(41, 37, 36),
                surface_alt: Color::Rgb(68, 64, 60),
                text: Color::Rgb(231, 229, 228),
                muted: Color::Rgb(168, 162, 158),
                border: Color::Rgb(87, 83, 78),
                active: Color::Rgb(251, 146, 60),
                hover: Color::Rgb(253, 186, 116),
                danger: Color::Rgb(248, 113, 113),
                success: Color::Rgb(134, 239, 172),
                warning: Color::Rgb(253, 224, 71),
                command: Color::Rgb(196, 181, 253),
                on_accent: Color::Rgb(28, 25, 23),
                on_bright: Color::Black,
                selection: Color::Rgb(68, 64, 60),
                cursor: Color::Rgb(251, 146, 60),
                on_cursor: Color::Rgb(28, 25, 23),
            },
            Self::Neon => ThemePalette {
                background: Color::Rgb(5, 5, 8),
                surface: Color::Rgb(18, 18, 28),
                surface_alt: Color::Rgb(32, 32, 48),
                text: Color::Rgb(240, 240, 255),
                muted: Color::Rgb(140, 140, 180),
                border: Color::Rgb(60, 60, 100),
                active: Color::Rgb(255, 0, 170),
                hover: Color::Rgb(0, 255, 255),
                danger: Color::Rgb(255, 60, 100),
                success: Color::Rgb(0, 255, 160),
                warning: Color::Rgb(255, 220, 0),
                command: Color::Rgb(160, 100, 255),
                on_accent: Color::Rgb(5, 5, 8),
                on_bright: Color::Black,
                selection: Color::Rgb(40, 20, 50),
                cursor: Color::Rgb(0, 255, 255),
                on_cursor: Color::Rgb(5, 5, 8),
            },
            Self::Paper => ThemePalette {
                background: Color::Rgb(250, 248, 243),
                surface: Color::Rgb(240, 236, 228),
                surface_alt: Color::Rgb(230, 224, 212),
                text: Color::Rgb(40, 36, 32),
                muted: Color::Rgb(120, 112, 100),
                border: Color::Rgb(200, 190, 175),
                active: Color::Rgb(180, 80, 50),
                hover: Color::Rgb(200, 110, 70),
                danger: Color::Rgb(180, 50, 50),
                success: Color::Rgb(60, 120, 80),
                warning: Color::Rgb(180, 130, 40),
                command: Color::Rgb(90, 80, 140),
                on_accent: Color::Rgb(250, 248, 243),
                on_bright: Color::White,
                selection: Color::Rgb(230, 220, 200),
                cursor: Color::Rgb(180, 80, 50),
                on_cursor: Color::Rgb(250, 248, 243),
            },
            Self::Minimal => ThemePalette {
                background: Color::Rgb(9, 9, 11),
                surface: Color::Rgb(18, 18, 20),
                surface_alt: Color::Rgb(28, 28, 32),
                text: Color::Rgb(228, 228, 231),
                muted: Color::Rgb(113, 113, 122),
                border: Color::Rgb(39, 39, 42),
                active: Color::Rgb(161, 161, 170),
                hover: Color::Rgb(212, 212, 216),
                danger: Color::Rgb(161, 161, 170),
                success: Color::Rgb(161, 161, 170),
                warning: Color::Rgb(161, 161, 170),
                command: Color::Rgb(161, 161, 170),
                on_accent: Color::Rgb(9, 9, 11),
                on_bright: Color::Black,
                selection: Color::Rgb(39, 39, 42),
                cursor: Color::Rgb(228, 228, 231),
                on_cursor: Color::Rgb(9, 9, 11),
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
                selection: Color::LightYellow,
                cursor: Color::LightCyan,
                on_cursor: Color::Black,
            },
            Self::Nord => ThemePalette {
                background: Color::Rgb(46, 52, 64),
                surface: Color::Rgb(59, 66, 82),
                surface_alt: Color::Rgb(67, 76, 94),
                text: Color::Rgb(216, 222, 233),
                muted: Color::Rgb(129, 161, 193),
                border: Color::Rgb(76, 86, 106),
                active: Color::Rgb(136, 192, 208),
                hover: Color::Rgb(235, 203, 139),
                danger: Color::Rgb(191, 97, 106),
                success: Color::Rgb(163, 190, 140),
                warning: Color::Rgb(235, 203, 139),
                command: Color::Rgb(129, 161, 193),
                on_accent: Color::Rgb(46, 52, 64),
                on_bright: Color::Rgb(46, 52, 64),
                selection: Color::Rgb(94, 129, 172),
                cursor: Color::Rgb(136, 192, 208),
                on_cursor: Color::Rgb(46, 52, 64),
            },
            Self::Dracula => ThemePalette {
                background: Color::Rgb(40, 42, 54),
                surface: Color::Rgb(68, 71, 90),
                surface_alt: Color::Rgb(50, 52, 66),
                text: Color::Rgb(248, 248, 242),
                muted: Color::Rgb(98, 114, 164),
                border: Color::Rgb(98, 114, 164),
                active: Color::Rgb(189, 147, 249),
                hover: Color::Rgb(241, 250, 140),
                danger: Color::Rgb(255, 85, 85),
                success: Color::Rgb(80, 250, 123),
                warning: Color::Rgb(255, 184, 108),
                command: Color::Rgb(139, 233, 253),
                on_accent: Color::Rgb(40, 42, 54),
                on_bright: Color::Rgb(40, 42, 54),
                selection: Color::Rgb(68, 71, 90),
                cursor: Color::Rgb(248, 248, 242),
                on_cursor: Color::Rgb(40, 42, 54),
            },
            Self::Gruvbox => ThemePalette {
                background: Color::Rgb(40, 40, 40),
                surface: Color::Rgb(60, 56, 54),
                surface_alt: Color::Rgb(80, 73, 69),
                text: Color::Rgb(235, 219, 178),
                muted: Color::Rgb(168, 153, 132),
                border: Color::Rgb(124, 111, 100),
                active: Color::Rgb(250, 189, 47),
                hover: Color::Rgb(254, 128, 25),
                danger: Color::Rgb(251, 73, 52),
                success: Color::Rgb(184, 187, 38),
                warning: Color::Rgb(250, 189, 47),
                command: Color::Rgb(131, 165, 152),
                on_accent: Color::Rgb(40, 40, 40),
                on_bright: Color::Rgb(40, 40, 40),
                selection: Color::Rgb(80, 73, 69),
                cursor: Color::Rgb(250, 189, 47),
                on_cursor: Color::Rgb(40, 40, 40),
            },
            Self::Catppuccin => ThemePalette {
                background: Color::Rgb(30, 30, 46),
                surface: Color::Rgb(49, 50, 68),
                surface_alt: Color::Rgb(69, 71, 90),
                text: Color::Rgb(205, 214, 244),
                muted: Color::Rgb(166, 173, 200),
                border: Color::Rgb(88, 91, 112),
                active: Color::Rgb(203, 166, 247),
                hover: Color::Rgb(249, 226, 175),
                danger: Color::Rgb(243, 139, 168),
                success: Color::Rgb(166, 227, 161),
                warning: Color::Rgb(249, 226, 175),
                command: Color::Rgb(137, 180, 250),
                on_accent: Color::Rgb(30, 30, 46),
                on_bright: Color::Rgb(30, 30, 46),
                selection: Color::Rgb(69, 71, 90),
                cursor: Color::Rgb(245, 224, 220),
                on_cursor: Color::Rgb(30, 30, 46),
            },
            Self::SolarizedDark => ThemePalette {
                background: Color::Rgb(0, 43, 54),
                surface: Color::Rgb(7, 54, 66),
                surface_alt: Color::Rgb(0, 61, 76),
                text: Color::Rgb(131, 148, 150),
                muted: Color::Rgb(88, 110, 117),
                border: Color::Rgb(88, 110, 117),
                active: Color::Rgb(38, 139, 210),
                hover: Color::Rgb(181, 137, 0),
                danger: Color::Rgb(220, 50, 47),
                success: Color::Rgb(133, 153, 0),
                warning: Color::Rgb(181, 137, 0),
                command: Color::Rgb(42, 161, 152),
                on_accent: Color::Rgb(253, 246, 227),
                on_bright: Color::Rgb(0, 43, 54),
                selection: Color::Rgb(7, 54, 66),
                cursor: Color::Rgb(131, 148, 150),
                on_cursor: Color::Rgb(0, 43, 54),
            },
            Self::SolarizedLight => ThemePalette {
                background: Color::Rgb(253, 246, 227),
                surface: Color::Rgb(238, 232, 213),
                surface_alt: Color::Rgb(221, 214, 193),
                text: Color::Rgb(101, 123, 131),
                muted: Color::Rgb(147, 161, 161),
                border: Color::Rgb(147, 161, 161),
                active: Color::Rgb(38, 139, 210),
                hover: Color::Rgb(181, 137, 0),
                danger: Color::Rgb(220, 50, 47),
                success: Color::Rgb(133, 153, 0),
                warning: Color::Rgb(203, 75, 22),
                command: Color::Rgb(42, 161, 152),
                on_accent: Color::Rgb(253, 246, 227),
                on_bright: Color::Rgb(0, 43, 54),
                selection: Color::Rgb(238, 232, 213),
                cursor: Color::Rgb(38, 139, 210),
                on_cursor: Color::White,
            },
            Self::TokyoNight => ThemePalette {
                background: Color::Rgb(26, 27, 38),
                surface: Color::Rgb(36, 40, 59),
                surface_alt: Color::Rgb(41, 46, 66),
                text: Color::Rgb(192, 202, 245),
                muted: Color::Rgb(86, 95, 137),
                border: Color::Rgb(65, 72, 104),
                active: Color::Rgb(122, 162, 247),
                hover: Color::Rgb(224, 175, 104),
                danger: Color::Rgb(247, 118, 142),
                success: Color::Rgb(158, 206, 106),
                warning: Color::Rgb(224, 175, 104),
                command: Color::Rgb(125, 207, 255),
                on_accent: Color::Rgb(26, 27, 38),
                on_bright: Color::Rgb(26, 27, 38),
                selection: Color::Rgb(41, 46, 66),
                cursor: Color::Rgb(192, 202, 245),
                on_cursor: Color::Rgb(26, 27, 38),
            },
            Self::Forest => ThemePalette {
                background: Color::Rgb(35, 42, 35),
                surface: Color::Rgb(45, 55, 45),
                surface_alt: Color::Rgb(55, 68, 55),
                text: Color::Rgb(211, 198, 170),
                muted: Color::Rgb(133, 150, 122),
                border: Color::Rgb(86, 110, 86),
                active: Color::Rgb(167, 192, 128),
                hover: Color::Rgb(230, 194, 104),
                danger: Color::Rgb(230, 126, 128),
                success: Color::Rgb(167, 192, 128),
                warning: Color::Rgb(230, 194, 104),
                command: Color::Rgb(127, 187, 179),
                on_accent: Color::Rgb(35, 42, 35),
                on_bright: Color::Rgb(35, 42, 35),
                selection: Color::Rgb(55, 68, 55),
                cursor: Color::Rgb(167, 192, 128),
                on_cursor: Color::Rgb(35, 42, 35),
            },
            Self::RosePine => ThemePalette {
                background: Color::Rgb(25, 23, 36),
                surface: Color::Rgb(31, 29, 46),
                surface_alt: Color::Rgb(38, 35, 58),
                text: Color::Rgb(224, 222, 244),
                muted: Color::Rgb(110, 106, 134),
                border: Color::Rgb(64, 61, 82),
                active: Color::Rgb(196, 167, 231),
                hover: Color::Rgb(246, 193, 119),
                danger: Color::Rgb(235, 111, 146),
                success: Color::Rgb(156, 207, 216),
                warning: Color::Rgb(246, 193, 119),
                command: Color::Rgb(156, 207, 216),
                on_accent: Color::Rgb(25, 23, 36),
                on_bright: Color::Rgb(25, 23, 36),
                selection: Color::Rgb(38, 35, 58),
                cursor: Color::Rgb(224, 222, 244),
                on_cursor: Color::Rgb(25, 23, 36),
            },
            Self::Ocean => ThemePalette {
                background: Color::Rgb(10, 22, 34),
                surface: Color::Rgb(16, 34, 52),
                surface_alt: Color::Rgb(22, 46, 68),
                text: Color::Rgb(184, 212, 230),
                muted: Color::Rgb(90, 130, 150),
                border: Color::Rgb(40, 80, 110),
                active: Color::Rgb(64, 196, 196),
                hover: Color::Rgb(240, 200, 120),
                danger: Color::Rgb(240, 100, 110),
                success: Color::Rgb(100, 200, 140),
                warning: Color::Rgb(240, 200, 120),
                command: Color::Rgb(100, 180, 220),
                on_accent: Color::Rgb(10, 22, 34),
                on_bright: Color::Rgb(10, 22, 34),
                selection: Color::Rgb(22, 46, 68),
                cursor: Color::Rgb(64, 196, 196),
                on_cursor: Color::Rgb(10, 22, 34),
            },
            Self::Ember => ThemePalette {
                background: Color::Rgb(28, 18, 14),
                surface: Color::Rgb(42, 28, 22),
                surface_alt: Color::Rgb(56, 36, 28),
                text: Color::Rgb(240, 220, 190),
                muted: Color::Rgb(170, 130, 100),
                border: Color::Rgb(110, 70, 50),
                active: Color::Rgb(240, 140, 70),
                hover: Color::Rgb(250, 200, 100),
                danger: Color::Rgb(230, 80, 70),
                success: Color::Rgb(160, 190, 90),
                warning: Color::Rgb(250, 200, 100),
                command: Color::Rgb(220, 160, 100),
                on_accent: Color::Rgb(28, 18, 14),
                on_bright: Color::Rgb(28, 18, 14),
                selection: Color::Rgb(56, 36, 28),
                cursor: Color::Rgb(255, 180, 100),
                on_cursor: Color::Rgb(28, 18, 14),
            },
            Self::Monokai => ThemePalette {
                background: Color::Rgb(39, 40, 34),
                surface: Color::Rgb(50, 51, 45),
                surface_alt: Color::Rgb(62, 63, 56),
                text: Color::Rgb(248, 248, 242),
                muted: Color::Rgb(117, 113, 94),
                border: Color::Rgb(117, 113, 94),
                active: Color::Rgb(166, 226, 46),
                hover: Color::Rgb(230, 219, 116),
                danger: Color::Rgb(249, 38, 114),
                success: Color::Rgb(166, 226, 46),
                warning: Color::Rgb(253, 151, 31),
                command: Color::Rgb(102, 217, 239),
                on_accent: Color::Rgb(39, 40, 34),
                on_bright: Color::Rgb(39, 40, 34),
                selection: Color::Rgb(62, 63, 56),
                cursor: Color::Rgb(248, 248, 242),
                on_cursor: Color::Rgb(39, 40, 34),
            },
        }
    }
}

impl UiWorkspaceSecondLine {
    pub(crate) fn from_name(name: &str) -> Self {
        match name.trim().to_ascii_lowercase().as_str() {
            "details" => Self::Details,
            "branch" => Self::Branch,
            "id" => Self::Id,
            "status" => Self::Status,
            "cursor" => Self::Cursor,
            "none" | "off" | "hidden" => Self::None,
            _ => Self::Path,
        }
    }

    pub(crate) fn name(self) -> &'static str {
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

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Path => "Path",
            Self::Details => "Details",
            Self::Branch => "Branch",
            Self::Id => "Id",
            Self::Status => "Status",
            Self::Cursor => "Cursor",
            Self::None => "None",
        }
    }

    pub(crate) fn all() -> &'static [Self] {
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

    pub(crate) fn relative(self, delta: isize) -> Self {
        let items = Self::all();
        let current = items.iter().position(|item| *item == self).unwrap_or(0);
        let next = (current as isize + delta).rem_euclid(items.len() as isize) as usize;
        items[next]
    }
}
