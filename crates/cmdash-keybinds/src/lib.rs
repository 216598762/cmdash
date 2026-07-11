//! cmdash-keybinds: modifier-aware key router with named modes
//! (`Normal`, `PaneResize`, `TabSwitch`, `PresetPick`) and a
//! dispatch table built from [`cmdash_config::Keybind`] entries.
//!
//! See `AGENTS.md` §"Keybinding system".
//!
//! ## Design
//!
//! - **v1 supports only [`Mode::Normal`].** Other modes exist as
//!   enum variants but routing through them is a future PR.
//! - **Press-only.** Crossterm's `KeyEvent` carries Press/Repeat/
//!   Release; only Press events are routed through the keybind
//!   table. PTYs already auto-repeat typed characters internally,
//!   so global Repeat handling would double-fire.
//! - **Chord → action.** A keybind's `Modifiers` + `KeyToken`
//!   forms the chord; lookup is a linear scan over the binds.
//!   v1 has tens of keybinds; v2 will switch to a `HashMap` for
//!   scale.

use std::collections::HashMap;

use cmdash_config::{KeyAction, KeyName, KeyToken, Keybind, Modifiers};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

/// One of the four key-router modes. v1 only routes [`Mode::Normal`];
/// the other variants exist so v2 can extend without churning
/// call sites or kernel Aurora-0 struct fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Mode {
    #[default]
    Normal,
    PaneResize,
    TabSwitch,
    PresetPick,
}

/// Hold the parsed keybind table plus the current mode. Built
/// once at startup from [`cmdash_config::Config::keybinds`] and
/// shared by reference across the main loop.
///
/// # Mode routing
///
/// In [`Mode::Normal`], the full `keybinds` table is searched.
/// When a non-Normal mode is active (`PaneResize`, `TabSwitch`,
/// `PresetPick`), mode-specific keybinds are searched first;
/// if none match, `Escape` is intercepted to return to Normal,
/// and all other keys fall through as `None` (forwarded to the
/// focused pane's PTY).
#[derive(Debug, Clone)]
pub struct Router {
    /// Global keybinds active in all modes (including Normal).
    keybinds: Vec<Keybind>,
    /// Per-mode keybind overrides. A mode entry here means those
    /// keybinds shadow the global table while that mode is active.
    mode_keybinds: HashMap<Mode, Vec<Keybind>>,
    /// Current routing mode.
    mode: Mode,
}

impl Router {
    pub fn new(keybinds: Vec<Keybind>) -> Self {
        let mut mode_keybinds = HashMap::new();
        // --- PaneResize mode defaults ---
        // Arrow keys resize the focused pane's parent split.
        mode_keybinds.insert(
            Mode::PaneResize,
            vec![
                Keybind {
                    mods: Modifiers::default(),
                    key: KeyToken::Named(KeyName::Up),
                    action: KeyAction::PaneResizeUp,
                },
                Keybind {
                    mods: Modifiers::default(),
                    key: KeyToken::Named(KeyName::Down),
                    action: KeyAction::PaneResizeDown,
                },
                Keybind {
                    mods: Modifiers::default(),
                    key: KeyToken::Named(KeyName::Left),
                    action: KeyAction::PaneResizeLeft,
                },
                Keybind {
                    mods: Modifiers::default(),
                    key: KeyToken::Named(KeyName::Right),
                    action: KeyAction::PaneResizeRight,
                },
            ],
        );
        // --- TabSwitch mode defaults ---
        // Number keys 1-9 switch to the corresponding tab.
        let mut tab_switch_binds = Vec::new();
        for n in 1..=9u8 {
            tab_switch_binds.push(Keybind {
                mods: Modifiers::default(),
                key: KeyToken::Char((b'0' + n) as char),
                action: KeyAction::TabSwitch(n as usize),
            });
        }
        mode_keybinds.insert(Mode::TabSwitch, tab_switch_binds);
        // --- PresetPick mode defaults ---
        // Number keys 1-9 select the Nth preset (by insertion order).
        // The main loop wires these at startup based on actual preset names.
        // For now, register empty; main.rs calls set_mode_keybinds after
        // loading presets.
        Self {
            keybinds,
            mode_keybinds,
            mode: Mode::Normal,
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }

    /// Register mode-specific keybinds. These shadow the global
    /// table while the given mode is active.
    pub fn set_mode_keybinds(&mut self, mode: Mode, keybinds: Vec<Keybind>) {
        self.mode_keybinds.insert(mode, keybinds);
    }

    /// Pure-data lookup: maps `(mods, key)` to a [`KeyAction`],
    /// ignoring crossterm's `Press`/`Release` distinction. Test
    /// scaffolding and v2's non-cross plugin callers use this.
    pub fn lookup(&self, mods: Modifiers, key: KeyToken) -> Option<&KeyAction> {
        self.keybinds.iter().find_map(|kb| {
            if kb.mods == mods && kb.key == key {
                Some(&kb.action)
            } else {
                None
            }
        })
    }

    /// Mode-aware lookup: searches mode-specific keybinds first,
    /// then falls back to the global table.
    fn lookup_mode_aware(&self, mods: Modifiers, key: KeyToken) -> Option<&KeyAction> {
        if self.mode != Mode::Normal {
            if let Some(action) = self
                .mode_keybinds
                .get(&self.mode)?
                .iter()
                .find(|kb| kb.mods == mods && kb.key == key)
                .map(|kb| &kb.action)
            {
                return Some(action);
            }
        }
        self.lookup(mods, key)
    }

    /// Crossterm-aware dispatch: only fires on Press; ignores
    /// FocusGained/Lost/Resize/Mouse by returning `None`.
    ///
    /// In non-Normal modes, `Escape` always returns
    /// [`KeyAction::ModeExit`]. Other unmatched keys fall through
    /// as `None` (forwarded to the focused pane's PTY).
    pub fn dispatch_crossterm(&self, ev: &Event) -> Option<KeyAction> {
        let Event::Key(KeyEvent {
            code,
            modifiers,
            kind,
            ..
        }) = ev
        else {
            return None;
        };
        if !matches!(kind, KeyEventKind::Press) {
            return None;
        }
        let token = from_key_code(*code)?;
        let m = from_key_modifiers(*modifiers);
        // In non-Normal modes, Escape always exits the mode.
        if self.mode != Mode::Normal
            && token == KeyToken::Named(KeyName::Escape)
            && m == Modifiers::default()
        {
            return Some(KeyAction::ModeExit);
        }
        self.lookup_mode_aware(m, token).cloned()
    }
}

/// Translate a crossterm [`KeyCode`] into a [`KeyToken`].
///
/// Returns `None` for variants cmdash does not key-bind in v1
/// (Insert, Null, modifier-only events, Media keys, F-key
/// numbers above 24).
pub fn from_key_code(code: KeyCode) -> Option<KeyToken> {
    Some(match code {
        KeyCode::Char(c) => KeyToken::Char(c),
        KeyCode::Enter => KeyToken::Named(KeyName::Enter),
        KeyCode::Esc => KeyToken::Named(KeyName::Escape),
        KeyCode::Tab => KeyToken::Named(KeyName::Tab),
        KeyCode::Backspace => KeyToken::Named(KeyName::Backspace),
        KeyCode::Up => KeyToken::Named(KeyName::Up),
        KeyCode::Down => KeyToken::Named(KeyName::Down),
        KeyCode::Left => KeyToken::Named(KeyName::Left),
        KeyCode::Right => KeyToken::Named(KeyName::Right),
        KeyCode::Home => KeyToken::Named(KeyName::Home),
        KeyCode::End => KeyToken::Named(KeyName::End),
        KeyCode::PageUp => KeyToken::Named(KeyName::PageUp),
        KeyCode::PageDown => KeyToken::Named(KeyName::PageDown),
        KeyCode::F(n) if (1..=24).contains(&n) => KeyToken::F(n),
        _ => return None,
    })
}

/// Translate crossterm [`KeyModifiers`] (bitflags) to cmdash
/// [`Modifiers`] (plain bool struct).
pub fn from_key_modifiers(km: KeyModifiers) -> Modifiers {
    let mut m = Modifiers::default();
    if km.contains(KeyModifiers::CONTROL) {
        m.ctrl = true;
    }
    if km.contains(KeyModifiers::SHIFT) {
        m.shift = true;
    }
    if km.contains(KeyModifiers::ALT) {
        m.alt = true;
    }
    if km.contains(KeyModifiers::SUPER) {
        m.super_ = true;
    }
    m
}

// Marker for the formatter that `KeyEventState::NONE` is part of
// the public surface; private to this crate so we don't break
// when crossterm tweaks the struct layout.
#[allow(dead_code)]
pub(crate) fn _state_marker(_: KeyEventState) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn kb(mods: Modifiers, key: KeyToken, action: KeyAction) -> Keybind {
        Keybind { mods, key, action }
    }

    #[test]
    fn lookup_matches_chord() {
        let r = Router::new(vec![kb(
            Modifiers {
                ctrl: true,
                ..Modifiers::default()
            },
            KeyToken::Char('a'),
            KeyAction::AppClose,
        )]);
        let action = r.lookup(
            Modifiers {
                ctrl: true,
                ..Modifiers::default()
            },
            KeyToken::Char('a'),
        );
        assert_eq!(action, Some(&KeyAction::AppClose));
    }

    #[test]
    fn lookup_misses_when_mods_differ() {
        let r = Router::new(vec![kb(
            Modifiers::default(),
            KeyToken::Char('a'),
            KeyAction::AppClose,
        )]);
        let action = r.lookup(
            Modifiers {
                ctrl: true,
                ..Modifiers::default()
            },
            KeyToken::Char('a'),
        );
        assert!(action.is_none());
    }

    #[test]
    fn dispatch_crossterm_press_only() {
        let r = Router::new(vec![kb(
            Modifiers::default(),
            KeyToken::Char('q'),
            KeyAction::AppClose,
        )]);
        let press = Event::Key(KeyEvent {
            code: KeyCode::Char('q'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        });
        assert_eq!(r.dispatch_crossterm(&press), Some(KeyAction::AppClose));

        let release = Event::Key(KeyEvent {
            code: KeyCode::Char('q'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: KeyEventState::NONE,
        });
        assert_eq!(r.dispatch_crossterm(&release), None);
    }

    #[test]
    fn from_key_code_round_trip_arrow() {
        assert_eq!(
            from_key_code(KeyCode::Up),
            Some(KeyToken::Named(KeyName::Up))
        );
        assert_eq!(from_key_code(KeyCode::F(12)), Some(KeyToken::F(12)));
        assert_eq!(from_key_code(KeyCode::Insert), None);
    }

    #[test]
    fn from_key_modifiers_picks_correct_bits() {
        assert!(from_key_modifiers(KeyModifiers::CONTROL).ctrl);
        assert!(from_key_modifiers(KeyModifiers::ALT).alt);
        assert!(from_key_modifiers(KeyModifiers::SHIFT).shift);
        assert!(from_key_modifiers(KeyModifiers::SUPER).super_);
        assert!(!from_key_modifiers(KeyModifiers::NONE).ctrl);
    }
}
