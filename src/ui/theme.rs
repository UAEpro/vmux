//! Themes and workspace second-line display modes for the attach UI.

use ratatui::style::Color;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UiTheme {
    Midnight,
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
    /// Block-cursor fill (bright accent — never pure black on dark panes).
    pub(crate) cursor: Color,
    /// Glyph color drawn on top of [`Self::cursor`].
    pub(crate) on_cursor: Color,
}

impl UiTheme {
    pub(crate) fn from_name(name: &str) -> Self {
        match name.trim().to_ascii_lowercase().as_str() {
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
            _ => Self::Midnight,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Midnight => "midnight",
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
            Self::Midnight => "Midnight",
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
        &[
            Self::Midnight,
            Self::Daylight,
            Self::Contrast,
            Self::Nord,
            Self::Dracula,
            Self::Gruvbox,
            Self::Catppuccin,
            Self::SolarizedDark,
            Self::SolarizedLight,
            Self::TokyoNight,
            Self::Forest,
            Self::RosePine,
            Self::Ocean,
            Self::Ember,
            Self::Monokai,
        ]
    }

    pub(crate) fn relative(self, delta: isize) -> Self {
        let themes = Self::all();
        let current = themes.iter().position(|item| *item == self).unwrap_or(0);
        let next = (current as isize + delta).rem_euclid(themes.len() as isize) as usize;
        themes[next]
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
            // Nord-inspired cool blues/frost.
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
            // Dracula purple night.
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
            // Gruvbox dark warm earth.
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
            // Catppuccin Mocha.
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
            // Soft green evergreen.
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
            // Deep ocean teal.
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
            // Warm ember / sepia night.
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
            // Classic Monokai.
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
            "none" => Self::None,
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
            Self::Id => "ID",
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
