use std::{collections::BTreeMap, fmt};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::command::{Command, FocusCommand, FocusDirection, PaneCommand, TabCommand};
use crate::config::SplitDirection;

/// A backend-neutral keyboard chord: one key plus the supported modifiers.
///
/// Chords are the public unit of keybinding configuration. They are parsed
/// from strings such as `"ctrl+shift+h"` and matched against crossterm
/// `KeyEvent`s without exposing crossterm types in the configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct KeyChord {
    key: KeyKind,
    modifiers: Modifiers,
}

/// Backend-neutral key identifiers.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
enum KeyKind {
    Char(char),
    Enter,
    Tab,
    BackTab,
    Backspace,
    Esc,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Delete,
    Insert,
    F(u8),
}

/// Supported modifiers. Super/hyper/meta are intentionally not representable.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Ord, PartialOrd)]
struct Modifiers {
    shift: bool,
    alt: bool,
    ctrl: bool,
}

impl KeyChord {
    /// Parses a chord such as `"ctrl+shift+h"`, `"tab"`, or `"esc"`.
    pub fn parse(source: &str) -> Result<Self, KeymapError> {
        let mut shift = false;
        let mut alt = false;
        let mut ctrl = false;
        let mut key = None;
        for token in source.split('+') {
            let token = token.trim().to_ascii_lowercase();
            match token.as_str() {
                "ctrl" | "control" if !ctrl => ctrl = true,
                "alt" | "option" if !alt => alt = true,
                "shift" if !shift => shift = true,
                "ctrl" | "control" | "alt" | "option" | "shift" => {
                    return Err(KeymapError::InvalidChord(source.to_owned()));
                }
                other => {
                    if key.is_some() {
                        return Err(KeymapError::InvalidChord(source.to_owned()));
                    }
                    key = Some(
                        parse_key(other)
                            .ok_or_else(|| KeymapError::InvalidChord(source.to_owned()))?,
                    );
                }
            }
        }
        let key = key.ok_or_else(|| KeymapError::InvalidChord(source.to_owned()))?;
        let (key, shift) = if key == KeyKind::Tab && shift {
            (KeyKind::BackTab, false)
        } else {
            (key, shift)
        };
        Ok(Self {
            key,
            modifiers: Modifiers { shift, alt, ctrl },
        })
    }

    /// Converts a crossterm key event into a comparable chord, returning
    /// `None` for key codes that cannot be bound (media, modifier, etc.).
    pub fn from_key_event(event: KeyEvent) -> Option<Self> {
        let mut key = match event.code {
            KeyCode::Char(character) => KeyKind::Char(character),
            KeyCode::Enter => KeyKind::Enter,
            KeyCode::Tab => KeyKind::Tab,
            KeyCode::BackTab => KeyKind::BackTab,
            KeyCode::Backspace => KeyKind::Backspace,
            KeyCode::Esc => KeyKind::Esc,
            KeyCode::Up => KeyKind::Up,
            KeyCode::Down => KeyKind::Down,
            KeyCode::Left => KeyKind::Left,
            KeyCode::Right => KeyKind::Right,
            KeyCode::Home => KeyKind::Home,
            KeyCode::End => KeyKind::End,
            KeyCode::PageUp => KeyKind::PageUp,
            KeyCode::PageDown => KeyKind::PageDown,
            KeyCode::Delete => KeyKind::Delete,
            KeyCode::Insert => KeyKind::Insert,
            KeyCode::F(number) => KeyKind::F(number),
            _ => return None,
        };
        let mut modifiers = Modifiers {
            shift: event.modifiers.contains(KeyModifiers::SHIFT),
            alt: event.modifiers.contains(KeyModifiers::ALT),
            ctrl: event.modifiers.contains(KeyModifiers::CONTROL),
        };
        // `BackTab` already encodes shift, so it must not carry a redundant
        // shift modifier that would prevent it matching the `backtab` binding.
        if key == KeyKind::Tab && modifiers.shift {
            modifiers.shift = false;
            key = KeyKind::BackTab;
        } else if key == KeyKind::BackTab {
            modifiers.shift = false;
        }
        Some(Self { key, modifiers })
    }
}

impl fmt::Display for KeyChord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.modifiers.ctrl {
            formatter.write_str("ctrl+")?;
        }
        if self.modifiers.alt {
            formatter.write_str("alt+")?;
        }
        if self.modifiers.shift {
            formatter.write_str("shift+")?;
        }
        match self.key {
            KeyKind::Char(' ') => formatter.write_str("space"),
            KeyKind::Char(character) => write!(formatter, "{character}"),
            KeyKind::Enter => formatter.write_str("enter"),
            KeyKind::Tab => formatter.write_str("tab"),
            KeyKind::BackTab => formatter.write_str("backtab"),
            KeyKind::Backspace => formatter.write_str("backspace"),
            KeyKind::Esc => formatter.write_str("esc"),
            KeyKind::Up => formatter.write_str("up"),
            KeyKind::Down => formatter.write_str("down"),
            KeyKind::Left => formatter.write_str("left"),
            KeyKind::Right => formatter.write_str("right"),
            KeyKind::Home => formatter.write_str("home"),
            KeyKind::End => formatter.write_str("end"),
            KeyKind::PageUp => formatter.write_str("pageup"),
            KeyKind::PageDown => formatter.write_str("pagedown"),
            KeyKind::Delete => formatter.write_str("delete"),
            KeyKind::Insert => formatter.write_str("insert"),
            KeyKind::F(number) => write!(formatter, "f{number}"),
        }
    }
}

fn parse_key(token: &str) -> Option<KeyKind> {
    Some(match token {
        "esc" | "escape" => KeyKind::Esc,
        "enter" | "return" => KeyKind::Enter,
        "tab" => KeyKind::Tab,
        "backtab" => KeyKind::BackTab,
        "backspace" => KeyKind::Backspace,
        "up" => KeyKind::Up,
        "down" => KeyKind::Down,
        "left" => KeyKind::Left,
        "right" => KeyKind::Right,
        "home" => KeyKind::Home,
        "end" => KeyKind::End,
        "pageup" | "pgup" => KeyKind::PageUp,
        "pagedown" | "pgdn" => KeyKind::PageDown,
        "delete" | "del" => KeyKind::Delete,
        "insert" | "ins" => KeyKind::Insert,
        "space" => KeyKind::Char(' '),
        token if token.len() == 1 => KeyKind::Char(token.chars().next()?),
        token if token.starts_with('f') => {
            let number: u8 = token[1..].parse().ok()?;
            if !(1..=12).contains(&number) {
                return None;
            }
            KeyKind::F(number)
        }
        _ => return None,
    })
}

/// A named, configurable application action.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyAction {
    Quit,
    QuitAlt,
    Help,
    Palette,
    Reload,
    CopySelection,
    FocusNext,
    FocusPrevious,
    FocusLeft,
    FocusRight,
    FocusUp,
    FocusDown,
    TabNext,
    TabPrevious,
    PaneSplitHorizontal,
    PaneSplitVertical,
    PaneGrow,
    PaneShrink,
    PaneClose,
    PaneMerge,
}

impl KeyAction {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Quit => "quit",
            Self::QuitAlt => "quit_alt",
            Self::Help => "help",
            Self::Palette => "palette",
            Self::Reload => "reload",
            Self::CopySelection => "copy_selection",
            Self::FocusNext => "focus_next",
            Self::FocusPrevious => "focus_previous",
            Self::FocusLeft => "focus_left",
            Self::FocusRight => "focus_right",
            Self::FocusUp => "focus_up",
            Self::FocusDown => "focus_down",
            Self::TabNext => "tab_next",
            Self::TabPrevious => "tab_previous",
            Self::PaneSplitHorizontal => "pane_split_horizontal",
            Self::PaneSplitVertical => "pane_split_vertical",
            Self::PaneGrow => "pane_grow",
            Self::PaneShrink => "pane_shrink",
            Self::PaneClose => "pane_close",
            Self::PaneMerge => "pane_merge",
        }
    }

    pub fn from_name(name: &str) -> Result<Self, KeymapError> {
        match name {
            "quit" => Ok(Self::Quit),
            "quit_alt" => Ok(Self::QuitAlt),
            "help" => Ok(Self::Help),
            "palette" => Ok(Self::Palette),
            "reload" => Ok(Self::Reload),
            "copy_selection" => Ok(Self::CopySelection),
            "focus_next" => Ok(Self::FocusNext),
            "focus_previous" => Ok(Self::FocusPrevious),
            "focus_left" => Ok(Self::FocusLeft),
            "focus_right" => Ok(Self::FocusRight),
            "focus_up" => Ok(Self::FocusUp),
            "focus_down" => Ok(Self::FocusDown),
            "tab_next" => Ok(Self::TabNext),
            "tab_previous" => Ok(Self::TabPrevious),
            "pane_split_horizontal" => Ok(Self::PaneSplitHorizontal),
            "pane_split_vertical" => Ok(Self::PaneSplitVertical),
            "pane_grow" => Ok(Self::PaneGrow),
            "pane_shrink" => Ok(Self::PaneShrink),
            "pane_close" => Ok(Self::PaneClose),
            "pane_merge" => Ok(Self::PaneMerge),
            _ => Err(KeymapError::UnknownAction(name.to_owned())),
        }
    }

    pub const fn command(self) -> Command {
        match self {
            Self::Quit | Self::QuitAlt => Command::Quit,
            Self::Help => Command::ToggleHelp,
            Self::Palette => Command::TogglePalette,
            Self::Reload => Command::ReloadConfig,
            Self::CopySelection => Command::CopySelection,
            Self::FocusNext => Command::Focus(FocusCommand::Next),
            Self::FocusPrevious => Command::Focus(FocusCommand::Previous),
            Self::FocusLeft => Command::Focus(FocusCommand::Direction(FocusDirection::Left)),
            Self::FocusRight => Command::Focus(FocusCommand::Direction(FocusDirection::Right)),
            Self::FocusUp => Command::Focus(FocusCommand::Direction(FocusDirection::Up)),
            Self::FocusDown => Command::Focus(FocusCommand::Direction(FocusDirection::Down)),
            Self::TabNext => Command::Tab(TabCommand::Next),
            Self::TabPrevious => Command::Tab(TabCommand::Previous),
            Self::PaneSplitHorizontal => {
                Command::Pane(PaneCommand::Split(SplitDirection::Horizontal))
            }
            Self::PaneSplitVertical => Command::Pane(PaneCommand::Split(SplitDirection::Vertical)),
            Self::PaneGrow => Command::Pane(PaneCommand::Grow),
            Self::PaneShrink => Command::Pane(PaneCommand::Shrink),
            Self::PaneClose => Command::Pane(PaneCommand::Close),
            Self::PaneMerge => Command::Pane(PaneCommand::Merge),
        }
    }

    /// Whether this action remains active while a terminal captures input.
    /// Only focus navigation escapes terminal capture by default.
    const fn is_terminal_escape(self) -> bool {
        matches!(self, Self::FocusNext | Self::FocusPrevious)
    }
}

/// A resolved chord-to-action map with a stable default.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Keymap {
    bindings: BTreeMap<KeyChord, KeyAction>,
}

impl Default for Keymap {
    fn default() -> Self {
        let mut keymap = Self {
            bindings: BTreeMap::new(),
        };
        for (action, chord) in default_bindings() {
            let chord = KeyChord::parse(chord).expect("default keybindings are valid chords");
            keymap
                .rebind(action, chord)
                .expect("default keybindings are unique");
        }
        keymap
    }
}

impl Keymap {
    /// The default keymap plus any `[keybindings]` overrides. Unknown action
    /// names, unparsable chords, and conflicting chords are rejected.
    pub fn from_overrides(overrides: &BTreeMap<String, String>) -> Result<Self, KeymapError> {
        let mut keymap = Self::default();
        for (name, chord) in overrides {
            let action = KeyAction::from_name(name)?;
            let chord = KeyChord::parse(chord)?;
            keymap.rebind(action, chord)?;
        }
        Ok(keymap)
    }

    pub fn command_for(&self, chord: KeyChord) -> Option<Command> {
        self.bindings.get(&chord).map(|action| action.command())
    }

    pub fn terminal_capture(&self, chord: KeyChord) -> Option<Command> {
        let action = self.bindings.get(&chord)?;
        action.is_terminal_escape().then(|| action.command())
    }

    pub fn command_for_key(&self, key: KeyEvent) -> Option<Command> {
        KeyChord::from_key_event(key).and_then(|chord| self.command_for(chord))
    }

    pub fn terminal_capture_for_key(&self, key: KeyEvent) -> Option<Command> {
        KeyChord::from_key_event(key).and_then(|chord| self.terminal_capture(chord))
    }

    pub fn bindings(&self) -> impl Iterator<Item = (KeyAction, KeyChord)> + '_ {
        self.bindings
            .iter()
            .map(|(&chord, &action)| (action, chord))
    }

    /// The chord currently bound to `action`, if any.
    pub fn chord_for(&self, action: KeyAction) -> Option<KeyChord> {
        self.bindings()
            .find_map(|(bound, chord)| (bound == action).then_some(chord))
    }

    /// A display string for `action`'s binding, or `"unbound"`.
    pub fn display_binding(&self, action: KeyAction) -> String {
        self.chord_for(action)
            .map(|chord| chord.to_string())
            .unwrap_or_else(|| "unbound".to_owned())
    }

    fn rebind(&mut self, action: KeyAction, chord: KeyChord) -> Result<(), KeymapError> {
        self.bindings.retain(|_, bound| *bound != action);
        if let Some(existing) = self.bindings.get(&chord)
            && *existing != action
        {
            return Err(KeymapError::Conflict {
                chord,
                action,
                existing: *existing,
            });
        }
        self.bindings.insert(chord, action);
        Ok(())
    }
}

fn default_bindings() -> Vec<(KeyAction, &'static str)> {
    vec![
        (KeyAction::Quit, "q"),
        (KeyAction::QuitAlt, "esc"),
        (KeyAction::Help, "?"),
        (KeyAction::Palette, "ctrl+p"),
        (KeyAction::Reload, "ctrl+r"),
        (KeyAction::CopySelection, "ctrl+shift+c"),
        (KeyAction::FocusNext, "tab"),
        (KeyAction::FocusPrevious, "backtab"),
        (KeyAction::FocusLeft, "alt+left"),
        (KeyAction::FocusRight, "alt+right"),
        (KeyAction::FocusUp, "alt+up"),
        (KeyAction::FocusDown, "alt+down"),
        (KeyAction::TabNext, "ctrl+pagedown"),
        (KeyAction::TabPrevious, "ctrl+pageup"),
        (KeyAction::PaneSplitHorizontal, "ctrl+shift+h"),
        (KeyAction::PaneSplitVertical, "ctrl+shift+v"),
        (KeyAction::PaneGrow, "ctrl+shift+right"),
        (KeyAction::PaneShrink, "ctrl+shift+left"),
        (KeyAction::PaneClose, "ctrl+shift+w"),
        (KeyAction::PaneMerge, "ctrl+shift+m"),
    ]
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum KeymapError {
    #[error("unknown keybinding action {0:?}")]
    UnknownAction(String),
    #[error("invalid keybinding chord {0:?}")]
    InvalidChord(String),
    #[error("keybinding {chord} for {} conflicts with {}", .action.name(), .existing.name())]
    Conflict {
        chord: KeyChord,
        action: KeyAction,
        existing: KeyAction,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};

    fn event(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn default_keymap_preserves_the_legacy_commands() {
        let keymap = Keymap::default();
        assert_eq!(
            keymap.command_for_key(event(KeyCode::Char('q'), KeyModifiers::NONE)),
            Some(Command::Quit)
        );
        assert_eq!(
            keymap.command_for_key(event(KeyCode::Esc, KeyModifiers::NONE)),
            Some(Command::Quit)
        );
        assert_eq!(
            keymap.command_for_key(event(KeyCode::Char('?'), KeyModifiers::NONE)),
            Some(Command::ToggleHelp)
        );
        assert_eq!(
            keymap.command_for_key(event(KeyCode::Tab, KeyModifiers::NONE)),
            Some(Command::Focus(FocusCommand::Next))
        );
        assert_eq!(
            keymap.command_for_key(event(KeyCode::BackTab, KeyModifiers::SHIFT)),
            Some(Command::Focus(FocusCommand::Previous))
        );
        assert_eq!(
            keymap.command_for_key(event(KeyCode::Char('p'), KeyModifiers::CONTROL)),
            Some(Command::TogglePalette)
        );
        assert_eq!(
            keymap.command_for_key(event(KeyCode::PageDown, KeyModifiers::CONTROL)),
            Some(Command::Tab(TabCommand::Next))
        );
        assert_eq!(
            keymap.command_for_key(event(
                KeyCode::Char('h'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            )),
            Some(Command::Pane(PaneCommand::Split(
                SplitDirection::Horizontal
            )))
        );
        assert_eq!(
            keymap.command_for_key(event(KeyCode::Down, KeyModifiers::ALT)),
            Some(Command::Focus(FocusCommand::Direction(
                FocusDirection::Down
            )))
        );
    }

    #[test]
    fn terminal_capture_only_exposes_focus_navigation() {
        let keymap = Keymap::default();
        assert_eq!(
            keymap.terminal_capture_for_key(event(KeyCode::Tab, KeyModifiers::NONE)),
            Some(Command::Focus(FocusCommand::Next))
        );
        assert_eq!(
            keymap.terminal_capture_for_key(event(KeyCode::BackTab, KeyModifiers::SHIFT)),
            Some(Command::Focus(FocusCommand::Previous))
        );
        assert_eq!(
            keymap.terminal_capture_for_key(event(KeyCode::Char('q'), KeyModifiers::NONE)),
            None
        );
        assert_eq!(
            keymap.terminal_capture_for_key(event(KeyCode::Left, KeyModifiers::ALT)),
            None
        );
    }

    #[test]
    fn overrides_rebind_an_action_and_remove_its_default_chord() {
        let overrides = BTreeMap::from([("quit".to_owned(), "ctrl+q".to_owned())]);
        let keymap = Keymap::from_overrides(&overrides).unwrap();
        assert_eq!(
            keymap.command_for_key(event(KeyCode::Char('q'), KeyModifiers::NONE)),
            None
        );
        assert_eq!(
            keymap.command_for_key(event(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            Some(Command::Quit)
        );
        // quit_alt is independent and unchanged.
        assert_eq!(
            keymap.command_for_key(event(KeyCode::Esc, KeyModifiers::NONE)),
            Some(Command::Quit)
        );
    }

    #[test]
    fn conflicting_and_unknown_overrides_are_rejected() {
        let conflict = BTreeMap::from([("quit".to_owned(), "tab".to_owned())]);
        assert!(matches!(
            Keymap::from_overrides(&conflict),
            Err(KeymapError::Conflict { .. })
        ));

        let unknown = BTreeMap::from([("explode".to_owned(), "x".to_owned())]);
        assert!(matches!(
            Keymap::from_overrides(&unknown),
            Err(KeymapError::UnknownAction(_))
        ));

        let invalid = BTreeMap::from([("quit".to_owned(), "not+a+key".to_owned())]);
        assert!(matches!(
            Keymap::from_overrides(&invalid),
            Err(KeymapError::InvalidChord(_))
        ));
    }

    #[test]
    fn chords_round_trip_through_parse_and_display() {
        for source in [
            "q",
            "esc",
            "ctrl+shift+h",
            "alt+left",
            "backtab",
            "f5",
            "space",
        ] {
            let chord = KeyChord::parse(source).unwrap();
            assert_eq!(chord.to_string(), source, "chord {source:?}");
        }
        assert_eq!(KeyChord::parse("shift+tab").unwrap().to_string(), "backtab");
    }

    #[test]
    fn malformed_and_out_of_range_chords_are_rejected() {
        for malformed in [
            "",
            "ctrl+",
            "+q",
            "ctrl+ctrl+q",
            "q+x",
            "ctrl+shift",
            "notakey",
            "f0",
            "f13",
            "f14",
            "ctrl+alt+hyper+q",
            "+++",
        ] {
            assert!(
                KeyChord::parse(malformed).is_err(),
                "chord {malformed:?} should not parse"
            );
        }
    }
}
