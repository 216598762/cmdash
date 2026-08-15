use std::{collections::BTreeMap, fmt};

use crate::config::AppearanceConfig;
use crate::scene::Color;

/// Semantic colors shared by dashboard widgets and overlays.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Theme {
    background: Color,
    surface: Color,
    foreground: Color,
    muted: Color,
    border: Color,
    focus: Color,
    accent: Color,
    success: Color,
    warning: Color,
    error: Color,
    selection_foreground: Color,
    selection_background: Color,
    overlay_foreground: Color,
    overlay_background: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self::inherited()
    }
}

impl Theme {
    /// Uses terminal reset and ANSI references so the parent terminal resolves
    /// the palette instead of cmdash imposing a hard-coded RGB theme.
    pub const fn inherited() -> Self {
        Self {
            background: Color::reset(),
            surface: Color::reset(),
            foreground: Color::reset(),
            muted: Color::ansi(8),
            border: Color::ansi(7),
            focus: Color::ansi(11),
            accent: Color::ansi(14),
            success: Color::ansi(10),
            warning: Color::ansi(11),
            error: Color::ansi(9),
            selection_foreground: Color::reset(),
            selection_background: Color::ansi(7),
            overlay_foreground: Color::reset(),
            overlay_background: Color::reset(),
        }
    }

    /// Provides a deterministic RGB theme for terminals or tests that require
    /// explicit colors rather than terminal-native references.
    pub const fn fallback() -> Self {
        Self {
            background: Color::rgb(18, 22, 30),
            surface: Color::rgb(27, 33, 44),
            foreground: Color::rgb(226, 232, 240),
            muted: Color::rgb(148, 163, 184),
            border: Color::rgb(125, 211, 252),
            focus: Color::rgb(250, 204, 21),
            accent: Color::rgb(125, 211, 252),
            success: Color::rgb(134, 239, 172),
            warning: Color::rgb(245, 158, 11),
            error: Color::rgb(248, 113, 113),
            selection_foreground: Color::rgb(18, 22, 30),
            selection_background: Color::rgb(125, 211, 252),
            overlay_foreground: Color::rgb(245, 232, 255),
            overlay_background: Color::rgb(38, 28, 58),
        }
    }

    pub fn from_config(config: &AppearanceConfig) -> Result<Self, AppearanceError> {
        let mut theme = match config.theme.to_ascii_lowercase().as_str() {
            "inherit" | "terminal" | "default" => Self::inherited(),
            "fallback" | "dark" => Self::fallback(),
            name => return Err(AppearanceError::UnknownTheme(name.to_owned())),
        };
        theme.apply_overrides(&config.colors)?;
        Ok(theme)
    }

    pub fn with_settings(
        self,
        settings: &BTreeMap<String, String>,
    ) -> Result<Self, AppearanceError> {
        let overrides = settings
            .iter()
            .filter_map(|(key, value)| is_color_role(key).then_some((key.clone(), value.clone())))
            .collect::<BTreeMap<_, _>>();
        let mut theme = self;
        theme.apply_overrides(&overrides)?;
        Ok(theme)
    }

    fn apply_overrides(
        &mut self,
        overrides: &BTreeMap<String, String>,
    ) -> Result<(), AppearanceError> {
        for (role, value) in overrides {
            let color = parse_color(value)?;
            match role.as_str() {
                "background" => self.background = color,
                "surface" => self.surface = color,
                "foreground" | "text" => self.foreground = color,
                "muted" => self.muted = color,
                "border" | "border_color" => self.border = color,
                "focus" | "focused" => self.focus = color,
                "accent" => self.accent = color,
                "success" => self.success = color,
                "warning" => self.warning = color,
                "error" => self.error = color,
                "selection_foreground" => self.selection_foreground = color,
                "selection_background" => self.selection_background = color,
                "overlay_foreground" => self.overlay_foreground = color,
                "overlay_background" => self.overlay_background = color,
                _ => return Err(AppearanceError::UnknownColorRole(role.clone())),
            }
        }
        Ok(())
    }

    pub const fn background(self) -> Color {
        self.background
    }

    pub const fn surface(self) -> Color {
        self.surface
    }

    pub const fn foreground(self) -> Color {
        self.foreground
    }

    pub const fn muted(self) -> Color {
        self.muted
    }

    pub const fn border(self) -> Color {
        self.border
    }

    pub const fn focus(self) -> Color {
        self.focus
    }

    pub const fn accent(self) -> Color {
        self.accent
    }

    pub const fn success(self) -> Color {
        self.success
    }

    pub const fn warning(self) -> Color {
        self.warning
    }

    pub const fn error(self) -> Color {
        self.error
    }

    pub const fn selection_foreground(self) -> Color {
        self.selection_foreground
    }

    pub const fn selection_background(self) -> Color {
        self.selection_background
    }

    pub const fn overlay_foreground(self) -> Color {
        self.overlay_foreground
    }

    pub const fn overlay_background(self) -> Color {
        self.overlay_background
    }
}

fn is_color_role(key: &str) -> bool {
    matches!(
        key,
        "background"
            | "surface"
            | "foreground"
            | "text"
            | "muted"
            | "border_color"
            | "focus"
            | "focused"
            | "accent"
            | "success"
            | "warning"
            | "error"
            | "selection_foreground"
            | "selection_background"
            | "overlay_foreground"
            | "overlay_background"
    )
}

fn parse_color(value: &str) -> Result<Color, AppearanceError> {
    let value = value.trim();
    if matches!(
        value.to_ascii_lowercase().as_str(),
        "inherit" | "terminal" | "default"
    ) {
        return Ok(Color::reset());
    }
    if let Some(index) = value.strip_prefix("ansi:") {
        let index = index
            .parse::<u8>()
            .map_err(|_| AppearanceError::InvalidColor(value.to_owned()))?;
        return Ok(Color::ansi(index));
    }
    if let Some(hex) = value.strip_prefix('#') {
        if hex.len() != 6 {
            return Err(AppearanceError::InvalidColor(value.to_owned()));
        }
        let red = u8::from_str_radix(&hex[0..2], 16)
            .map_err(|_| AppearanceError::InvalidColor(value.to_owned()))?;
        let green = u8::from_str_radix(&hex[2..4], 16)
            .map_err(|_| AppearanceError::InvalidColor(value.to_owned()))?;
        let blue = u8::from_str_radix(&hex[4..6], 16)
            .map_err(|_| AppearanceError::InvalidColor(value.to_owned()))?;
        return Ok(Color::rgb(red, green, blue));
    }
    Err(AppearanceError::InvalidColor(value.to_owned()))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AppearanceError {
    UnknownTheme(String),
    UnknownColorRole(String),
    InvalidColor(String),
}

impl fmt::Display for AppearanceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownTheme(theme) => {
                write!(
                    formatter,
                    "unknown appearance theme {theme:?}; expected inherit or fallback"
                )
            }
            Self::UnknownColorRole(role) => {
                write!(formatter, "unknown appearance color role {role:?}")
            }
            Self::InvalidColor(color) => write!(
                formatter,
                "invalid appearance color {color:?}; expected inherit, ansi:N, or #RRGGBB"
            ),
        }
    }
}

impl std::error::Error for AppearanceError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inherited_theme_uses_terminal_native_colors() {
        let theme = Theme::inherited();
        assert_eq!(theme.background(), Color::reset());
        assert_eq!(theme.accent(), Color::ansi(14));
    }

    #[test]
    fn config_overrides_support_rgb_and_ansi_colors() {
        let config = AppearanceConfig {
            theme: "inherit".to_owned(),
            colors: BTreeMap::from([
                ("background".to_owned(), "#010203".to_owned()),
                ("focus".to_owned(), "ansi:12".to_owned()),
            ]),
        };
        let theme = Theme::from_config(&config).unwrap();
        assert_eq!(theme.background(), Color::rgb(1, 2, 3));
        assert_eq!(theme.focus(), Color::ansi(12));
    }

    #[test]
    fn invalid_theme_values_are_rejected() {
        let config = AppearanceConfig {
            theme: "inherit".to_owned(),
            colors: BTreeMap::from([("border".to_owned(), "not-a-color".to_owned())]),
        };
        assert!(matches!(
            Theme::from_config(&config),
            Err(AppearanceError::InvalidColor(_))
        ));
    }
}
