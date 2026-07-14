//! Copy-mode integration tests.
//!
//! These tests exercise the copy-mode keybind dispatch through
//! `cmdash_keybinds::Router` and the clipboard/text-extraction
//! helpers exposed from `cmdash::render`.

use cmdash_config::{KeyAction, KeyToken, Keybind, Modifiers};
use cmdash_keybinds::{Mode, Router};

/// Build a Router with a keybind for entering Copy mode.
/// Uses `alt-c` → `copy.enter` to match the default config.
fn router_with_copy_keybind() -> Router {
    let keybinds = vec![Keybind {
        mods: Modifiers {
            alt: true,
            ..Default::default()
        },
        key: KeyToken::Char('c'),
        action: KeyAction::EnterCopyMode,
    }];
    Router::new(keybinds)
}

/// Helper: dispatch a crossterm key event through the router.
/// Returns the `KeyAction` if the key was matched, `None` otherwise.
fn dispatch_key(
    router: &Router,
    modifiers: crossterm::event::KeyModifiers,
    code: crossterm::event::KeyCode,
) -> Option<KeyAction> {
    let event = crossterm::event::Event::Key(crossterm::event::KeyEvent {
        code,
        modifiers,
        kind: crossterm::event::KeyEventKind::Press,
        state: crossterm::event::KeyEventState::NONE,
    });
    router.dispatch_crossterm(&event)
}

// ==========================================================================
// Copy-mode keybind dispatch tests
// ==========================================================================

/// Enter Copy mode via the configured keybind, verify mode changes,
/// then exit via Escape and verify Normal mode is restored.
#[tokio::test]
async fn enter_copy_mode_and_exit_with_escape() {
    let mut router = router_with_copy_keybind();
    assert_eq!(router.mode(), Mode::Normal, "starts in Normal mode");

    // Enter Copy mode via M-c.
    let action = dispatch_key(
        &router,
        crossterm::event::KeyModifiers::ALT,
        crossterm::event::KeyCode::Char('c'),
    );
    assert_eq!(
        action,
        Some(KeyAction::EnterCopyMode),
        "M-c must dispatch EnterCopyMode"
    );
    router.set_mode(Mode::Copy);
    assert_eq!(router.mode(), Mode::Copy);

    // Escape exits back to Normal.
    let action = dispatch_key(
        &router,
        crossterm::event::KeyModifiers::NONE,
        crossterm::event::KeyCode::Esc,
    );
    assert_eq!(
        action,
        Some(KeyAction::ModeExit),
        "Escape in Copy must dispatch ModeExit"
    );
    router.set_mode(Mode::Normal);
    assert_eq!(router.mode(), Mode::Normal, "Escape restores Normal mode");
}

/// In Copy mode, arrow keys dispatch copy-mode movement actions.
#[tokio::test]
async fn copy_mode_arrow_keys_dispatch_movement_actions() {
    let mut router = router_with_copy_keybind();
    router.set_mode(Mode::Copy);

    let action = dispatch_key(
        &router,
        crossterm::event::KeyModifiers::NONE,
        crossterm::event::KeyCode::Up,
    );
    assert_eq!(
        action,
        Some(KeyAction::CopyModeMoveUp),
        "Up arrow in Copy must dispatch CopyModeMoveUp"
    );

    let action = dispatch_key(
        &router,
        crossterm::event::KeyModifiers::NONE,
        crossterm::event::KeyCode::Down,
    );
    assert_eq!(
        action,
        Some(KeyAction::CopyModeMoveDown),
        "Down arrow in Copy must dispatch CopyModeMoveDown"
    );

    let action = dispatch_key(
        &router,
        crossterm::event::KeyModifiers::NONE,
        crossterm::event::KeyCode::Left,
    );
    assert_eq!(
        action,
        Some(KeyAction::CopyModeMoveLeft),
        "Left arrow in Copy must dispatch CopyModeMoveLeft"
    );

    let action = dispatch_key(
        &router,
        crossterm::event::KeyModifiers::NONE,
        crossterm::event::KeyCode::Right,
    );
    assert_eq!(
        action,
        Some(KeyAction::CopyModeMoveRight),
        "Right arrow in Copy must dispatch CopyModeMoveRight"
    );
}

/// In Copy mode, 'v' starts/extends the selection and 'y' copies.
#[tokio::test]
async fn copy_mode_v_selects_and_y_copies() {
    let mut router = router_with_copy_keybind();
    router.set_mode(Mode::Copy);

    let action = dispatch_key(
        &router,
        crossterm::event::KeyModifiers::NONE,
        crossterm::event::KeyCode::Char('v'),
    );
    assert_eq!(
        action,
        Some(KeyAction::CopyModeStartSelection),
        "v in Copy must dispatch CopyModeStartSelection"
    );

    let action = dispatch_key(
        &router,
        crossterm::event::KeyModifiers::NONE,
        crossterm::event::KeyCode::Char('y'),
    );
    assert_eq!(
        action,
        Some(KeyAction::CopyModeCopy),
        "y in Copy must dispatch CopyModeCopy"
    );

    // Enter also copies.
    let action = dispatch_key(
        &router,
        crossterm::event::KeyModifiers::NONE,
        crossterm::event::KeyCode::Enter,
    );
    assert_eq!(
        action,
        Some(KeyAction::CopyModeCopy),
        "Enter in Copy must dispatch CopyModeCopy"
    );
}

/// In Normal mode, copy-mode keys fall through (forwarded to PTY).
#[tokio::test]
async fn normal_mode_copy_keys_fall_through() {
    let router = router_with_copy_keybind();
    assert_eq!(router.mode(), Mode::Normal);

    for key in [
        crossterm::event::KeyCode::Up,
        crossterm::event::KeyCode::Down,
        crossterm::event::KeyCode::Left,
        crossterm::event::KeyCode::Right,
        crossterm::event::KeyCode::Char('v'),
        crossterm::event::KeyCode::Char('y'),
    ] {
        let action = dispatch_key(&router, crossterm::event::KeyModifiers::NONE, key);
        assert_eq!(
            action, None,
            "Copy-mode keys in Normal mode must fall through to PTY"
        );
    }
}

// ==========================================================================
// Text extraction tests
// ==========================================================================

/// `extract_selected_text` returns the character under the cursor
/// when no selection anchor is set.
#[test]
fn extract_selected_text_returns_cursor_cell_without_selection() {
    let mut grid = cmdash_pty::TextGrid::new(10, 5);
    grid.put_char(0, 0, 'X');
    let text = cmdash::render::extract_selected_text(&grid, 0, 0, None);
    assert_eq!(text, "X");
}

/// `extract_selected_text` returns the rectangular selection
/// between the anchor and the cursor.
#[test]
fn extract_selected_text_returns_rectangular_selection() {
    let mut grid = cmdash_pty::TextGrid::new(10, 5);
    // Row 1: " hello" (leading space).
    grid.put_char(1, 1, ' ');
    for (i, c) in "hello".chars().enumerate() {
        grid.put_char(2 + i as u16, 1, c);
    }
    let text = cmdash::render::extract_selected_text(&grid, 6, 1, Some((1, 1)));
    assert_eq!(text, " hello");
}

/// `extract_selected_text` clamps coordinates outside the grid.
#[test]
fn extract_selected_text_clamps_out_of_bounds_coordinates() {
    let grid = cmdash_pty::TextGrid::new(10, 5);
    let text = cmdash::render::extract_selected_text(&grid, 100, 100, None);
    assert_eq!(text, " ");
}

// ==========================================================================
// Clipboard copy tests
// ==========================================================================

/// `copy_text_to_clipboard` writes text to the system clipboard and
/// `arboard::Clipboard::get_text` reads it back.
///
/// On headless CI hosts without a clipboard provider (no X11/Wayland),
/// `arboard::Clipboard::new()` fails. The test skips the round-trip in
/// that case rather than failing the suite.
#[test]
fn copy_text_to_clipboard_round_trips_text() {
    let text = "cmdash copy-mode clipboard test";

    // Attempt the copy first; if the environment has no clipboard
    // provider, `copy_text_to_clipboard` will return an error and we
    // skip the round-trip assertion.
    if let Err(e) = cmdash::render::copy_text_to_clipboard(text) {
        eprintln!("clipboard unavailable in test environment, skipping round-trip: {e}");
        return;
    }

    let mut clipboard = arboard::Clipboard::new().expect("create clipboard");
    let got = clipboard.get_text().expect("read clipboard text");
    assert_eq!(got, text);
}
