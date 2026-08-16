use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::command::{Command, FocusCommand, FocusDirection, PaneCommand, TabCommand};

/// Commands that remain active while a terminal shell captures keyboard input.
///
/// Inside a focused terminal widget every key is forwarded to the child PTY
/// except these focus-escape bindings, which still move focus to another widget
/// so the user can reach the dashboard command surface.
pub fn terminal_capture_command(key: KeyEvent) -> Option<Command> {
    if key.modifiers.contains(KeyModifiers::ALT) || key.modifiers.contains(KeyModifiers::CONTROL) {
        return None;
    }
    match key.code {
        KeyCode::Tab => Some(Command::Focus(FocusCommand::Next)),
        KeyCode::BackTab => Some(Command::Focus(FocusCommand::Previous)),
        _ => None,
    }
}

pub fn command_for_key(key: KeyEvent) -> Option<Command> {
    if key.modifiers.contains(KeyModifiers::ALT) {
        return match key.code {
            KeyCode::Left => Some(Command::Focus(FocusCommand::Direction(
                FocusDirection::Left,
            ))),
            KeyCode::Right => Some(Command::Focus(FocusCommand::Direction(
                FocusDirection::Right,
            ))),
            KeyCode::Up => Some(Command::Focus(FocusCommand::Direction(FocusDirection::Up))),
            KeyCode::Down => Some(Command::Focus(FocusCommand::Direction(
                FocusDirection::Down,
            ))),
            _ => None,
        };
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.modifiers.contains(KeyModifiers::SHIFT)
    {
        match key.code {
            KeyCode::Left => return Some(Command::Pane(PaneCommand::Shrink)),
            KeyCode::Right => return Some(Command::Pane(PaneCommand::Grow)),
            KeyCode::Char('h') => {
                return Some(Command::Pane(PaneCommand::Split(
                    crate::config::SplitDirection::Horizontal,
                )));
            }
            KeyCode::Char('v') => {
                return Some(Command::Pane(PaneCommand::Split(
                    crate::config::SplitDirection::Vertical,
                )));
            }
            KeyCode::Char('m') => return Some(Command::Pane(PaneCommand::Merge)),
            KeyCode::Char('w') => return Some(Command::Pane(PaneCommand::Close)),
            _ => {}
        }
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::PageDown => Some(Command::Tab(TabCommand::Next)),
            KeyCode::PageUp => Some(Command::Tab(TabCommand::Previous)),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::SHIFT) => {
                Some(Command::CopySelection)
            }
            KeyCode::Char('p') => Some(Command::TogglePalette),
            KeyCode::Char('r') => Some(Command::ReloadConfig),
            _ => None,
        };
    }
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => Some(Command::Quit),
        KeyCode::Char('?') => Some(Command::ToggleHelp),
        KeyCode::Tab => Some(Command::Focus(FocusCommand::Next)),
        KeyCode::BackTab => Some(Command::Focus(FocusCommand::Previous)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    #[test]
    fn terminal_capture_only_exposes_focus_escape_bindings() {
        assert_eq!(
            terminal_capture_command(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            Some(Command::Focus(FocusCommand::Next))
        );
        assert_eq!(
            terminal_capture_command(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)),
            Some(Command::Focus(FocusCommand::Previous))
        );
        assert_eq!(
            terminal_capture_command(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
            None
        );
        assert_eq!(
            terminal_capture_command(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            None
        );
        assert_eq!(
            terminal_capture_command(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL)),
            None
        );
        assert_eq!(
            terminal_capture_command(KeyEvent::new(KeyCode::PageDown, KeyModifiers::CONTROL)),
            None
        );
        assert_eq!(
            terminal_capture_command(KeyEvent::new(KeyCode::Tab, KeyModifiers::CONTROL)),
            None
        );
    }

    #[test]
    fn tab_keys_map_to_focus_navigation_commands() {
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            Some(Command::Focus(FocusCommand::Next))
        );
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)),
            Some(Command::Focus(FocusCommand::Previous))
        );
    }

    #[test]
    fn control_page_keys_switch_tabs() {
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::CONTROL)),
            Some(Command::Tab(TabCommand::Next))
        );
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::CONTROL)),
            Some(Command::Tab(TabCommand::Previous))
        );
    }

    #[test]
    fn help_palette_and_reload_keys_are_discoverable_commands() {
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE)),
            Some(Command::ToggleHelp)
        );
        assert_eq!(
            command_for_key(KeyEvent::new(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            )),
            Some(Command::CopySelection)
        );
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL)),
            Some(Command::TogglePalette)
        );
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)),
            Some(Command::ReloadConfig)
        );
    }

    #[test]
    fn pane_and_directional_focus_keys_are_available() {
        assert_eq!(
            command_for_key(KeyEvent::new(
                KeyCode::Right,
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            )),
            Some(Command::Pane(PaneCommand::Grow))
        );
        assert_eq!(
            command_for_key(KeyEvent::new(
                KeyCode::Left,
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            )),
            Some(Command::Pane(PaneCommand::Shrink))
        );
        assert_eq!(
            command_for_key(KeyEvent::new(
                KeyCode::Char('w'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            )),
            Some(Command::Pane(PaneCommand::Close))
        );
        assert_eq!(
            command_for_key(KeyEvent::new(
                KeyCode::Char('h'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            )),
            Some(Command::Pane(PaneCommand::Split(
                crate::config::SplitDirection::Horizontal
            )))
        );
        assert_eq!(
            command_for_key(KeyEvent::new(
                KeyCode::Char('v'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            )),
            Some(Command::Pane(PaneCommand::Split(
                crate::config::SplitDirection::Vertical
            )))
        );
        assert_eq!(
            command_for_key(KeyEvent::new(
                KeyCode::Char('m'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            )),
            Some(Command::Pane(PaneCommand::Merge))
        );
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Down, KeyModifiers::ALT)),
            Some(Command::Focus(FocusCommand::Direction(
                FocusDirection::Down
            )))
        );
    }

    #[test]
    fn quit_keys_remain_available() {
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
            Some(Command::Quit)
        );
        assert_eq!(
            command_for_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Some(Command::Quit)
        );
    }
}
