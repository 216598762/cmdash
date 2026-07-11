//! Theme/color configuration for cmdash.
//!
//! Parsed from a top-level `theme { ... }` block in the KDL config.
//! When present, theme colors override the hardcoded defaults for the
//! tab bar, status bar, and widget borders.
//!
//! # Example
//!
//! ```kdl
//! theme {
//!     // Terminal defaults
//!     default-fg       "white"
//!     default-bg       "black"
//!     cursor-style     "block"       // "block" | "underline" | "bar"
//!
//!     tab-bar-bg       "dark-gray"
//!     tab-active-bg    "blue"
//!     tab-active-fg    "white"
//!     tab-inactive-bg  "dark-gray"
//!     tab-inactive-fg  "gray"
//!
//!     status-bar-bg    "dark-gray"
//!     status-mode-fg   "white"
//!     status-clock-fg  "gray"
//!
//!     border-color     "dark-gray"
//!     error-color      "red"
//! }
//! ```

use ratatui::style::Color;

/// Cursor style for the terminal.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CursorStyle {
    /// Solid block cursor (default).
    #[default]
    Block,
    /// Underline cursor.
    Underline,
    /// Vertical bar cursor.
    Bar,
}

impl CursorStyle {
    /// Parse a cursor style string from KDL.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "block" => Some(CursorStyle::Block),
            "underline" | "under" | "u" => Some(CursorStyle::Underline),
            "bar" | "pipe" | "|" => Some(CursorStyle::Bar),
            _ => None,
        }
    }
}

/// Theme configuration for cmdash.
///
/// All fields are `Option<Color>`. When `None`, the hardcoded default
/// color is used. This allows partial themes — users only need to
/// specify the colors they want to override.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Theme {
    // Terminal defaults.
    /// Default foreground color for the terminal body.
    pub default_fg: Option<Color>,
    /// Default background color for the terminal body.
    pub default_bg: Option<Color>,
    /// Cursor style (block, underline, bar).
    pub cursor_style: Option<CursorStyle>,

    // Tab bar colors.
    /// Background color of the tab bar itself.
    pub tab_bar_bg: Option<Color>,
    /// Background color of the active tab.
    pub tab_active_bg: Option<Color>,
    /// Foreground color of the active tab.
    pub tab_active_fg: Option<Color>,
    /// Background color of inactive tabs.
    pub tab_inactive_bg: Option<Color>,
    /// Foreground color of inactive tabs.
    pub tab_inactive_fg: Option<Color>,

    // Status bar colors.
    /// Background color of the status bar.
    pub status_bar_bg: Option<Color>,
    /// Foreground color of the mode indicator (e.g. "Normal").
    pub status_mode_fg: Option<Color>,
    /// Background color of the mode indicator.
    pub status_mode_bg: Option<Color>,
    /// Foreground color of the clock.
    pub status_clock_fg: Option<Color>,
    /// Foreground color of the pane title.
    pub status_pane_title_fg: Option<Color>,

    // Widget/border colors.
    /// Default border color for widgets and bordered blocks.
    pub border_color: Option<Color>,
    /// Color for error messages.
    pub error_color: Option<Color>,
}

impl Theme {
    // -- Terminal defaults --

    /// Default foreground: White.
    pub fn default_fg(&self) -> Color {
        self.default_fg.unwrap_or(Color::White)
    }

    /// Default background: Black.
    pub fn default_bg(&self) -> Color {
        self.default_bg.unwrap_or(Color::Black)
    }

    /// Default cursor style: Block.
    pub fn cursor_style(&self) -> CursorStyle {
        self.cursor_style.unwrap_or_default()
    }

    // -- Tab bar defaults --

    /// Default tab bar background: DarkGray.
    pub fn tab_bar_bg(&self) -> Color {
        self.tab_bar_bg.unwrap_or(Color::DarkGray)
    }

    /// Default active tab background: Blue.
    pub fn tab_active_bg(&self) -> Color {
        self.tab_active_bg.unwrap_or(Color::Blue)
    }

    /// Default active tab foreground: White.
    pub fn tab_active_fg(&self) -> Color {
        self.tab_active_fg.unwrap_or(Color::White)
    }

    /// Default inactive tab background: DarkGray.
    pub fn tab_inactive_bg(&self) -> Color {
        self.tab_inactive_bg.unwrap_or(Color::DarkGray)
    }

    /// Default inactive tab foreground: Gray.
    pub fn tab_inactive_fg(&self) -> Color {
        self.tab_inactive_fg.unwrap_or(Color::Gray)
    }

    // -- Status bar defaults --

    /// Default status bar background: DarkGray.
    pub fn status_bar_bg(&self) -> Color {
        self.status_bar_bg.unwrap_or(Color::DarkGray)
    }

    /// Default status mode foreground: White.
    pub fn status_mode_fg(&self) -> Color {
        self.status_mode_fg.unwrap_or(Color::White)
    }

    /// Default status mode background: DarkGray.
    pub fn status_mode_bg(&self) -> Color {
        self.status_mode_bg.unwrap_or(Color::DarkGray)
    }

    /// Default status clock foreground: Gray.
    pub fn status_clock_fg(&self) -> Color {
        self.status_clock_fg.unwrap_or(Color::Gray)
    }

    /// Default status pane title foreground: Gray.
    pub fn status_pane_title_fg(&self) -> Color {
        self.status_pane_title_fg.unwrap_or(Color::Gray)
    }

    // -- Widget/border defaults --

    /// Default border color: DarkGray.
    pub fn border_color(&self) -> Color {
        self.border_color.unwrap_or(Color::DarkGray)
    }

    /// Default error color: Red.
    pub fn error_color(&self) -> Color {
        self.error_color.unwrap_or(Color::Red)
    }

    // -- RGBA helpers for pixel overlay (graphics.rs) --

    /// Convert a `ratatui::style::Color` to `[r, g, b, a]` RGBA bytes.
    /// Named colors use the standard ANSI-256-to-RGB mapping.
    /// `Color::Reset` maps to the terminal default bg (0, 0, 0).
    pub fn color_to_rgba(c: Color, alpha: u8) -> [u8; 4] {
        match c {
            Color::Reset => [0, 0, 0, alpha],
            Color::Black => [0, 0, 0, alpha],
            Color::DarkGray => [85, 85, 85, alpha],
            Color::Gray => [170, 170, 170, alpha],
            Color::White => [255, 255, 255, alpha],
            Color::Red => [255, 0, 0, alpha],
            Color::Green => [0, 255, 0, alpha],
            Color::Blue => [0, 0, 255, alpha],
            Color::Yellow => [255, 255, 0, alpha],
            Color::Cyan => [0, 255, 255, alpha],
            Color::Magenta => [255, 0, 255, alpha],
            Color::Rgb(r, g, b) => [r, g, b, alpha],
            Color::Indexed(n) => Self::ansi256_to_rgb(n, alpha),
            _ => [128, 128, 128, alpha],
        }
    }

    /// Map ANSI-256 indexed color to RGB. Uses the standard xterm-256 palette.
    fn ansi256_to_rgb(n: u8, alpha: u8) -> [u8; 4] {
        match n {
            0 => [0, 0, 0, alpha],
            1 => [128, 0, 0, alpha],
            2 => [0, 128, 0, alpha],
            3 => [128, 128, 0, alpha],
            4 => [0, 0, 128, alpha],
            5 => [128, 0, 128, alpha],
            6 => [0, 128, 128, alpha],
            7 => [192, 192, 192, alpha],
            8 => [128, 128, 128, alpha],
            9 => [255, 0, 0, alpha],
            10 => [0, 255, 0, alpha],
            11 => [255, 255, 0, alpha],
            12 => [0, 0, 255, alpha],
            13 => [255, 0, 255, alpha],
            14 => [0, 255, 255, alpha],
            15 => [255, 255, 255, alpha],
            // 16-231: 6x6x6 color cube
            16..=231 => {
                let idx = n - 16;
                let b = (idx % 6) * 51;
                let g = ((idx / 6) % 6) * 51;
                let r = (idx / 36) * 51;
                [r, g, b, alpha]
            }
            // 232-255: grayscale ramp
            _ => {
                let v = 8 + (n - 232) * 10;
                [v, v, v, alpha]
            }
        }
    }

    /// RGBA for tab bar background (pixel overlay).
    pub fn tab_bar_bg_rgba(&self) -> [u8; 4] {
        Self::color_to_rgba(self.tab_bar_bg(), 255)
    }

    /// RGBA for active tab background (pixel overlay).
    pub fn tab_active_bg_rgba(&self) -> [u8; 4] {
        Self::color_to_rgba(self.tab_active_bg(), 255)
    }

    /// RGBA for active tab foreground (pixel overlay).
    pub fn tab_active_fg_rgba(&self) -> [u8; 4] {
        Self::color_to_rgba(self.tab_active_fg(), 255)
    }

    /// RGBA for inactive tab background (pixel overlay).
    pub fn tab_inactive_bg_rgba(&self) -> [u8; 4] {
        Self::color_to_rgba(self.tab_inactive_bg(), 255)
    }

    /// RGBA for inactive tab foreground (pixel overlay).
    pub fn tab_inactive_fg_rgba(&self) -> [u8; 4] {
        Self::color_to_rgba(self.tab_inactive_fg(), 255)
    }

    /// Returns `true` if the theme has no overrides (all fields are
    /// `None`). Useful for short-circuiting theme-dependent code paths.
    pub fn is_default(&self) -> bool {
        self == &Theme::default()
    }
}

/// Parse a color string from KDL into a `ratatui::style::Color`.
///
/// Supported formats:
/// - Named colors: `black`, `dark-gray`, `gray`, `white`, `red`,
///   `green`, `blue`, `yellow`, `cyan`, `magenta`, `reset`
/// - Indexed: `indexed(5)` or `i5`
/// - RGB: `rgb(255, 128, 64)` or `#FF8040` or `#ff8040`
///
/// Returns `None` for unrecognized formats.
pub fn parse_color(s: &str) -> Option<Color> {
    let s = s.trim();

    // Try hex RGB: #RRGGBB or #RGB
    if let Some(hex) = s.strip_prefix('#') {
        if hex.len() == 6 {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            return Some(Color::Rgb(r, g, b));
        }
        if hex.len() == 3 {
            let r = u8::from_str_radix(&hex[0..1], 16).ok()? * 17;
            let g = u8::from_str_radix(&hex[1..2], 16).ok()? * 17;
            let b = u8::from_str_radix(&hex[2..3], 16).ok()? * 17;
            return Some(Color::Rgb(r, g, b));
        }
        return None;
    }

    // Try rgb(R, G, B)
    if let Some(rest) = s.strip_prefix("rgb(").and_then(|r| r.strip_suffix(')')) {
        let parts: Vec<&str> = rest.split(',').collect();
        if parts.len() == 3 {
            let r = parts[0].trim().parse::<u8>().ok()?;
            let g = parts[1].trim().parse::<u8>().ok()?;
            let b = parts[2].trim().parse::<u8>().ok()?;
            return Some(Color::Rgb(r, g, b));
        }
        return None;
    }

    // Try indexed(N) or iN
    if let Some(rest) = s.strip_prefix("indexed(").and_then(|r| r.strip_suffix(')')) {
        let n = rest.trim().parse::<u8>().ok()?;
        return Some(Color::Indexed(n));
    }
    if let Some(rest) = s.strip_prefix('i') {
        if let Ok(n) = rest.parse::<u8>() {
            return Some(Color::Indexed(n));
        }
    }

    // Named colors (case-insensitive)
    match s.to_lowercase().as_str() {
        "black" => Some(Color::Black),
        "dark-gray" | "darkgray" | "dark_gray" => Some(Color::DarkGray),
        "gray" | "grey" => Some(Color::Gray),
        "white" => Some(Color::White),
        "red" => Some(Color::Red),
        "green" => Some(Color::Green),
        "blue" => Some(Color::Blue),
        "yellow" => Some(Color::Yellow),
        "cyan" => Some(Color::Cyan),
        "magenta" => Some(Color::Magenta),
        "reset" => Some(Color::Reset),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_named_colors() {
        assert_eq!(parse_color("black"), Some(Color::Black));
        assert_eq!(parse_color("dark-gray"), Some(Color::DarkGray));
        assert_eq!(parse_color("DarkGray"), Some(Color::DarkGray));
        assert_eq!(parse_color("gray"), Some(Color::Gray));
        assert_eq!(parse_color("grey"), Some(Color::Gray));
        assert_eq!(parse_color("white"), Some(Color::White));
        assert_eq!(parse_color("red"), Some(Color::Red));
        assert_eq!(parse_color("green"), Some(Color::Green));
        assert_eq!(parse_color("blue"), Some(Color::Blue));
        assert_eq!(parse_color("yellow"), Some(Color::Yellow));
        assert_eq!(parse_color("cyan"), Some(Color::Cyan));
        assert_eq!(parse_color("magenta"), Some(Color::Magenta));
        assert_eq!(parse_color("reset"), Some(Color::Reset));
    }

    #[test]
    fn parse_hex_rgb() {
        assert_eq!(parse_color("#FF8040"), Some(Color::Rgb(255, 128, 64)));
        assert_eq!(parse_color("#ff8040"), Some(Color::Rgb(255, 128, 64)));
        assert_eq!(parse_color("#F0F"), Some(Color::Rgb(255, 0, 255)));
        assert_eq!(parse_color("#fff"), Some(Color::Rgb(255, 255, 255)));
    }

    #[test]
    fn parse_rgb_function() {
        assert_eq!(
            parse_color("rgb(255, 128, 64)"),
            Some(Color::Rgb(255, 128, 64))
        );
        assert_eq!(parse_color("rgb( 0, 0, 0 )"), Some(Color::Rgb(0, 0, 0)));
    }

    #[test]
    fn parse_indexed() {
        assert_eq!(parse_color("indexed(5)"), Some(Color::Indexed(5)));
        assert_eq!(parse_color("i5"), Some(Color::Indexed(5)));
        assert_eq!(parse_color("i0"), Some(Color::Indexed(0)));
    }

    #[test]
    fn parse_invalid_returns_none() {
        assert_eq!(parse_color(""), None);
        assert_eq!(parse_color("notacolor"), None);
        assert_eq!(parse_color("#GG0000"), None);
        assert_eq!(parse_color("rgb(256, 0, 0)"), None);
        assert_eq!(parse_color("i256"), None);
    }

    #[test]
    fn cursor_style_parse() {
        assert_eq!(CursorStyle::parse("block"), Some(CursorStyle::Block));
        assert_eq!(CursorStyle::parse("Block"), Some(CursorStyle::Block));
        assert_eq!(
            CursorStyle::parse("underline"),
            Some(CursorStyle::Underline)
        );
        assert_eq!(CursorStyle::parse("bar"), Some(CursorStyle::Bar));
        assert_eq!(CursorStyle::parse("pipe"), Some(CursorStyle::Bar));
        assert_eq!(CursorStyle::parse("|"), Some(CursorStyle::Bar));
        assert_eq!(CursorStyle::parse("invalid"), None);
    }

    #[test]
    fn theme_defaults() {
        let theme = Theme::default();
        assert!(theme.is_default());
        assert_eq!(theme.default_fg(), Color::White);
        assert_eq!(theme.default_bg(), Color::Black);
        assert_eq!(theme.cursor_style(), CursorStyle::Block);
        assert_eq!(theme.tab_bar_bg(), Color::DarkGray);
        assert_eq!(theme.tab_active_bg(), Color::Blue);
        assert_eq!(theme.tab_active_fg(), Color::White);
        assert_eq!(theme.tab_inactive_bg(), Color::DarkGray);
        assert_eq!(theme.tab_inactive_fg(), Color::Gray);
        assert_eq!(theme.status_bar_bg(), Color::DarkGray);
        assert_eq!(theme.status_mode_fg(), Color::White);
        assert_eq!(theme.status_mode_bg(), Color::DarkGray);
        assert_eq!(theme.status_clock_fg(), Color::Gray);
        assert_eq!(theme.status_pane_title_fg(), Color::Gray);
        assert_eq!(theme.border_color(), Color::DarkGray);
        assert_eq!(theme.error_color(), Color::Red);
    }

    #[test]
    fn theme_partial_override() {
        let theme = Theme {
            tab_active_bg: Some(Color::Magenta),
            border_color: Some(Color::Cyan),
            default_fg: Some(Color::Green),
            ..Default::default()
        };
        assert!(!theme.is_default());
        assert_eq!(theme.tab_active_bg(), Color::Magenta);
        assert_eq!(theme.border_color(), Color::Cyan);
        assert_eq!(theme.default_fg(), Color::Green);
        assert_eq!(theme.tab_active_fg(), Color::White);
        assert_eq!(theme.error_color(), Color::Red);
        assert_eq!(theme.default_bg(), Color::Black);
    }

    #[test]
    fn color_to_rgba_named() {
        assert_eq!(Theme::color_to_rgba(Color::Red, 255), [255, 0, 0, 255]);
        assert_eq!(Theme::color_to_rgba(Color::Blue, 200), [0, 0, 255, 200]);
        assert_eq!(Theme::color_to_rgba(Color::Reset, 255), [0, 0, 0, 255]);
    }

    #[test]
    fn color_to_rgba_rgb() {
        assert_eq!(
            Theme::color_to_rgba(Color::Rgb(10, 20, 30), 128),
            [10, 20, 30, 128]
        );
    }

    #[test]
    fn color_to_rgba_indexed_6x6x6() {
        assert_eq!(
            Theme::color_to_rgba(Color::Indexed(16), 255),
            [0, 0, 0, 255]
        );
        assert_eq!(
            Theme::color_to_rgba(Color::Indexed(17), 255),
            [0, 0, 51, 255]
        );
        assert_eq!(
            Theme::color_to_rgba(Color::Indexed(231), 255),
            [255, 255, 255, 255]
        );
    }

    #[test]
    fn color_to_rgba_grayscale() {
        assert_eq!(
            Theme::color_to_rgba(Color::Indexed(232), 255),
            [8, 8, 8, 255]
        );
        assert_eq!(
            Theme::color_to_rgba(Color::Indexed(255), 255),
            [238, 238, 238, 255]
        );
    }

    #[test]
    fn rgba_helpers() {
        let theme = Theme {
            tab_active_bg: Some(Color::Rgb(100, 200, 50)),
            ..Default::default()
        };
        assert_eq!(theme.tab_active_bg_rgba(), [100, 200, 50, 255]);
        assert_eq!(theme.tab_bar_bg_rgba(), [85, 85, 85, 255]);
    }
}
