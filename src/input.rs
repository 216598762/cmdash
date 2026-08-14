use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::command::{Command, FocusCommand, TabCommand};

pub fn command_for_key(key: KeyEvent) -> Option<Command> {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::PageDown => Some(Command::Tab(TabCommand::Next)),
            KeyCode::PageUp => Some(Command::Tab(TabCommand::Previous)),
            _ => None,
        };
    }
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => Some(Command::Quit),
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
