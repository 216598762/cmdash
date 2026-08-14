use crossterm::event::{KeyCode, KeyEvent};

use crate::command::{Command, FocusCommand};

pub fn command_for_key(key: KeyEvent) -> Option<Command> {
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
