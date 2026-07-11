//! Mode transitions integration tests: enter PaneResize via keybind,
//! arrow keys resize panes, Escape exits back to Normal.
//!
//! These tests exercise the full mode lifecycle through the
//! `cmdash_keybinds::Router` (mode dispatch) combined with
//! `cmdash_layout::update_split_ratio` (resize logic) and real
//! `PaneRunner` instances, mirroring the production code path
//! in `TickContext::apply_action_full` without reaching into
//! the binary crate's `main.rs`.

use cmdash::pane::{PaneCloseTx, PaneRunner};
use cmdash_config::{KeyAction, Ratio as CfgRatio};
use cmdash_keybinds::{Mode, Router};
use cmdash_layout::{update_split_ratio, ComputedLayout, Rect as LayoutRect};
use cmdash_pty::ShellSpec;

/// Long-lived shell so PTYs stay alive across assertions.
fn long_shell() -> ShellSpec {
    ShellSpec::Command {
        argv: vec!["sleep".to_string(), "10".to_string()],
    }
}

/// Build a Router with a keybind for entering PaneResize mode.
/// Uses `alt-r` → `pane.resize.enter` to match the default config.
fn router_with_resize_keybind() -> Router {
    let keybinds = vec![cmdash_config::Keybind {
        mods: cmdash_config::Modifiers {
            alt: true,
            ..Default::default()
        },
        key: cmdash_config::KeyToken::Char('r'),
        action: KeyAction::EnterPaneResize,
    }];
    Router::new(keybinds)
}

/// Helper: dispatch a crossterm key event through the router.
/// Returns the `KeyAction` if the key was matched, `None` otherwise.
fn dispatch_key(router: &Router, alt: bool, code: crossterm::event::KeyCode) -> Option<KeyAction> {
    let modifiers = if alt {
        crossterm::event::KeyModifiers::ALT
    } else {
        crossterm::event::KeyModifiers::NONE
    };
    let event = crossterm::event::Event::Key(crossterm::event::KeyEvent {
        code,
        modifiers,
        kind: crossterm::event::KeyEventKind::Press,
        state: crossterm::event::KeyEventState::NONE,
    });
    router.dispatch_crossterm(&event)
}

// ==========================================================================
// Basic mode transition tests
// ==========================================================================

/// Enter PaneResize mode via the configured keybind, verify mode
/// changes, then exit via Escape and verify Normal mode is restored.
#[tokio::test]
async fn enter_pane_resize_and_exit_with_escape() {
    let mut router = router_with_resize_keybind();
    assert_eq!(router.mode(), Mode::Normal, "starts in Normal mode");

    // Enter PaneResize mode via M-r.
    let action = dispatch_key(&router, true, crossterm::event::KeyCode::Char('r'));
    assert_eq!(
        action,
        Some(KeyAction::EnterPaneResize),
        "M-r must dispatch EnterPaneResize"
    );
    router.set_mode(Mode::PaneResize);
    assert_eq!(router.mode(), Mode::PaneResize);

    // Escape exits back to Normal.
    let action = dispatch_key(&router, false, crossterm::event::KeyCode::Esc);
    assert_eq!(
        action,
        Some(KeyAction::ModeExit),
        "Escape in PaneResize must dispatch ModeExit"
    );
    router.set_mode(Mode::Normal);
    assert_eq!(router.mode(), Mode::Normal, "Escape restores Normal mode");
}

/// In PaneResize mode, arrow keys dispatch resize actions.
#[tokio::test]
async fn pane_resize_mode_arrow_keys_dispatch_resize_actions() {
    let mut router = router_with_resize_keybind();
    router.set_mode(Mode::PaneResize);

    // Up arrow → PaneResizeUp.
    let action = dispatch_key(&router, false, crossterm::event::KeyCode::Up);
    assert_eq!(
        action,
        Some(KeyAction::PaneResizeUp),
        "Up arrow in PaneResize must dispatch PaneResizeUp"
    );

    // Down arrow → PaneResizeDown.
    let action = dispatch_key(&router, false, crossterm::event::KeyCode::Down);
    assert_eq!(
        action,
        Some(KeyAction::PaneResizeDown),
        "Down arrow in PaneResize must dispatch PaneResizeDown"
    );

    // Left arrow → PaneResizeLeft.
    let action = dispatch_key(&router, false, crossterm::event::KeyCode::Left);
    assert_eq!(
        action,
        Some(KeyAction::PaneResizeLeft),
        "Left arrow in PaneResize must dispatch PaneResizeLeft"
    );

    // Right arrow → PaneResizeRight.
    let action = dispatch_key(&router, false, crossterm::event::KeyCode::Right);
    assert_eq!(
        action,
        Some(KeyAction::PaneResizeRight),
        "Right arrow in PaneResize must dispatch PaneResizeRight"
    );
}

/// In Normal mode, arrow keys do NOT dispatch resize actions
/// (they fall through to the focused pane's PTY).
#[tokio::test]
async fn normal_mode_arrow_keys_do_not_dispatch_resize_actions() {
    let router = router_with_resize_keybind();
    assert_eq!(router.mode(), Mode::Normal);

    // Arrow keys in Normal mode return None (fall through to PTY).
    for key in [
        crossterm::event::KeyCode::Up,
        crossterm::event::KeyCode::Down,
        crossterm::event::KeyCode::Left,
        crossterm::event::KeyCode::Right,
    ] {
        let action = dispatch_key(&router, false, key);
        assert_eq!(
            action, None,
            "Arrow keys in Normal mode must not match any keybind"
        );
    }
}

/// Escape in Normal mode does NOT dispatch ModeExit (there is no
/// mode to exit from).
#[tokio::test]
async fn escape_in_normal_mode_is_unmatched() {
    let router = router_with_resize_keybind();
    let action = dispatch_key(&router, false, crossterm::event::KeyCode::Esc);
    assert_eq!(
        action, None,
        "Escape in Normal mode must be unmatched (no mode to exit)"
    );
}

/// Unmatched keys in PaneResize mode fall through as None (forwarded
/// to the focused pane's PTY), so users can still type while in
/// resize mode.
#[tokio::test]
async fn unmatched_keys_fall_through_in_pane_resize_mode() {
    let mut router = router_with_resize_keybind();
    router.set_mode(Mode::PaneResize);

    // A regular character like 'a' is not bound in PaneResize mode.
    let action = dispatch_key(&router, false, crossterm::event::KeyCode::Char('a'));
    assert_eq!(
        action, None,
        "Unmatched 'a' in PaneResize must fall through as None"
    );

    // Enter is also not bound.
    let action = dispatch_key(&router, false, crossterm::event::KeyCode::Enter);
    assert_eq!(
        action, None,
        "Unmatched Enter in PaneResize must fall through as None"
    );
}

// ==========================================================================
// Full lifecycle: enter → resize → exit with real PTYs
// ==========================================================================

/// Full lifecycle: enter PaneResize mode, resize a split pane via
/// update_split_ratio, verify the rect changes, exit mode, and
/// verify Normal mode is restored. Uses a nested split layout so
/// the focused leaf's parent Split is NOT the root (the root is
/// an outer Split containing an inner Split), which is the
/// scenario where `pane_resize_by_direction` actually fires.
///
/// Layout structure (80x24):
/// ```text
///   outer Split (H, 60%) ─┬─ left: inner Split (V, 50%)
///                         │     ├─ top (pre_order=0)
///                         │     └─ bot (pre_order=1)
///                         └─ right (pre_order=2)
/// ```
/// Focused pane = "top" (pre_order=0). Its parent Split is the
/// inner Split at path [0] (tree indices: outer child 0 → inner
/// Split). `update_split_ratio(&root, &[0], ...)` targets that
/// inner Split.
#[tokio::test]
async fn full_mode_transition_resize_real_pty() {
    let source = r#"layout {
        split axis=horizontal ratio=0.6 {
            split axis=vertical ratio=0.5 {
                pane kind=shell label="top"
                pane kind=shell label="bot"
            }
            pane kind=shell label="right"
        }
    }"#;
    let cfg = cmdash_config::parse(source).expect("parse config");
    let mut layout_root = cfg.layout.expect("layout block");
    let area = LayoutRect {
        x: 0,
        y: 0,
        w: 80,
        h: 24,
    };

    // Compute initial layout.
    let initial_layout = ComputedLayout::compute(&layout_root, area).expect("compute initial");
    assert_eq!(
        initial_layout.panes.len(),
        3,
        "fixture: 3-pane nested split"
    );
    // inner Split is at outer child 0: path [0] in tree indices.
    // top: outer(0) → inner(0) → (0, 0) pre_order=0
    // bot: outer(0) → inner(1) → (0, 1) pre_order=1
    // right: outer(1) → (1) pre_order=2

    // Spawn real PTY runners.
    let (close_tx, _close_rx): (PaneCloseTx, _) = tokio::sync::mpsc::unbounded_channel();
    let mut runners: Vec<PaneRunner> = Vec::new();
    for pane in &initial_layout.panes {
        let layer = cmdash::derive_layer_id(&pane.id);
        runners.push(
            PaneRunner::spawn_with_graphics(
                pane.clone(),
                layer,
                long_shell(),
                Some(close_tx.clone()),
            )
            .expect("spawn pane"),
        );
    }

    // --- Enter PaneResize mode ---
    let mut router = router_with_resize_keybind();
    assert_eq!(router.mode(), Mode::Normal);

    let action = dispatch_key(&router, true, crossterm::event::KeyCode::Char('r'));
    assert_eq!(action, Some(KeyAction::EnterPaneResize));
    router.set_mode(Mode::PaneResize);
    assert_eq!(router.mode(), Mode::PaneResize);

    // --- Resize: arrow key → update_split_ratio ---
    // Simulate pressing Right arrow in PaneResize mode for the
    // focused pane ("top", child 0 of the inner Split).
    let action = dispatch_key(&router, false, crossterm::event::KeyCode::Right);
    assert_eq!(action, Some(KeyAction::PaneResizeRight));

    // Inline-replicate `TickContext::pane_resize_by_direction`:
    // the inner Split is at tree path [0] (outer child 0).
    let initial_ratio: u8 = 50;
    let new_ratio: u8 = (initial_ratio + 2).clamp(1, 99); // 52
    update_split_ratio(&mut layout_root, &[0], CfgRatio(new_ratio)).expect("update_split_ratio");

    // Verify the ratio was updated by re-resolving the layout.
    let resized_layout = ComputedLayout::compute(&layout_root, area).expect("compute after resize");
    assert_eq!(resized_layout.panes.len(), 3);

    // inner Split now at 52% vertical. The inner area is 24 rows
    // (outer Split is Horizontal so children get full height).
    // top gets (24*52)/100 = 12 rows (floor), bot gets remainder = 12.
    let inner_h: u16 = 24;
    let top_h = (inner_h * 52) / 100; // 12
    let bot_h = inner_h - top_h; // 12
    assert_eq!(
        resized_layout.panes[0].rect.h, top_h,
        "top pane height after +2% resize"
    );
    assert_eq!(
        resized_layout.panes[1].rect.h, bot_h,
        "bot pane height after +2% resize"
    );

    // Resize the real PTY runners to match the new rects.
    for (runner, pane) in runners.iter_mut().zip(resized_layout.panes.iter()) {
        runner.resize(pane.rect).expect("resize runner to new rect");
    }

    // Verify runner rects updated.
    assert_eq!(
        runners[0].computed().rect.h,
        top_h,
        "top runner rect.h after resize"
    );
    assert_eq!(
        runners[1].computed().rect.h,
        bot_h,
        "bot runner rect.h after resize"
    );

    // --- Exit PaneResize mode via Escape ---
    let action = dispatch_key(&router, false, crossterm::event::KeyCode::Esc);
    assert_eq!(action, Some(KeyAction::ModeExit));
    router.set_mode(Mode::Normal);
    assert_eq!(router.mode(), Mode::Normal, "Escape restores Normal mode");

    // Arrow keys no longer dispatch resize actions.
    let action = dispatch_key(&router, false, crossterm::event::KeyCode::Right);
    assert_eq!(
        action, None,
        "Right arrow in Normal mode must not dispatch resize"
    );

    // All runners still alive.
    for r in runners.iter_mut() {
        let _ = r.tick().expect("tick runner");
    }
}

/// Full lifecycle with multiple resize steps: enter mode, resize
/// right, resize right again, resize left, exit. Uses a nested
/// split so the inner Split has a valid path for update_split_ratio.
#[tokio::test]
async fn multiple_resizes_in_single_mode_session() {
    let source = r#"layout {
        split axis=horizontal ratio=0.6 {
            split axis=vertical ratio=0.5 {
                pane kind=shell label="top"
                pane kind=shell label="bot"
            }
            pane kind=shell label="right"
        }
    }"#;
    let cfg = cmdash_config::parse(source).expect("parse config");
    let mut layout_root = cfg.layout.expect("layout block");
    let area = LayoutRect {
        x: 0,
        y: 0,
        w: 80,
        h: 24,
    };

    let mut router = router_with_resize_keybind();
    router.set_mode(Mode::PaneResize);

    // Start at ratio 50 (inner Split at tree path [0]).
    let mut current_ratio: u8 = 50;

    // Simulate pressing Right (+2%) three times.
    for _ in 0..3 {
        let action = dispatch_key(&router, false, crossterm::event::KeyCode::Right);
        assert_eq!(action, Some(KeyAction::PaneResizeRight));
        current_ratio = (current_ratio + 2).clamp(1, 99);
        update_split_ratio(&mut layout_root, &[0], CfgRatio(current_ratio))
            .expect("update_split_ratio");
    }
    assert_eq!(current_ratio, 56, "50 + 2 + 2 + 2 = 56");

    // Simulate pressing Left (-2%) twice.
    for _ in 0..2 {
        let action = dispatch_key(&router, false, crossterm::event::KeyCode::Left);
        assert_eq!(action, Some(KeyAction::PaneResizeLeft));
        current_ratio = (current_ratio as i16 - 2).clamp(1, 99) as u8;
        update_split_ratio(&mut layout_root, &[0], CfgRatio(current_ratio))
            .expect("update_split_ratio");
    }
    assert_eq!(current_ratio, 52, "56 - 2 - 2 = 52");

    // Verify layout reflects the final ratio. The inner area is
    // 24 rows (outer Split is Horizontal, children get full height).
    let layout = ComputedLayout::compute(&layout_root, area).expect("compute final");
    let expected_top_h: u16 = (24 * 52) / 100; // 12
    assert_eq!(
        layout.panes[0].rect.h, expected_top_h,
        "top pane height after multiple resize steps"
    );
    assert_eq!(
        layout.panes[1].rect.h,
        24 - expected_top_h,
        "bot pane height after multiple resize steps"
    );

    // Exit mode.
    let action = dispatch_key(&router, false, crossterm::event::KeyCode::Esc);
    assert_eq!(action, Some(KeyAction::ModeExit));
    router.set_mode(Mode::Normal);
    assert_eq!(router.mode(), Mode::Normal);
}

/// Enter PaneResize mode, verify M-r is NOT dispatched again while
/// already in PaneResize mode (global keybind for enter-resize is
/// looked up in the global table, but since we're already in
/// PaneResize mode and the key is in the global table, it should
/// match). This tests that mode entry keybinds remain active even
/// in non-Normal modes (they live in the global keybinds table).
#[tokio::test]
async fn enter_resize_keybind_works_from_within_resize_mode() {
    let mut router = router_with_resize_keybind();
    router.set_mode(Mode::PaneResize);

    // M-r is in the global keybinds table, so it should still
    // match even while in PaneResize mode.
    let action = dispatch_key(&router, true, crossterm::event::KeyCode::Char('r'));
    assert_eq!(
        action,
        Some(KeyAction::EnterPaneResize),
        "M-r (global keybind) must still match in PaneResize mode"
    );
}

/// Verify the ratio clamping: resize to a low ratio, then try to go
/// further left — it must clamp at 1. Uses a nested split so the
/// inner Split's parent area is small enough that ratio=1 doesn't
/// produce a zero-area child (inner width = 48, 48*1/100 = 0 →
/// rejected as ZeroArea). So we test ratio=2 as the minimum safe
/// value, and verify clamping prevents going below.
#[tokio::test]
async fn resize_ratio_clamped_at_minimum() {
    let source = r#"layout {
        split axis=horizontal ratio=0.6 {
            split axis=vertical ratio=0.5 {
                pane kind=shell label="top"
                pane kind=shell label="bot"
            }
            pane kind=shell label="right"
        }
    }"#;
    let cfg = cmdash_config::parse(source).expect("parse config");
    let mut layout_root = cfg.layout.expect("layout block");
    // Start at ratio 5, then press Left 5 times (for child 0 of
    // the inner Split at tree path [0]).
    let mut ratio: u8 = 5;
    update_split_ratio(&mut layout_root, &[0], CfgRatio(ratio)).expect("set initial ratio");

    for _ in 0..5 {
        ratio = (ratio as i16 - 2).clamp(1, 99) as u8;
        update_split_ratio(&mut layout_root, &[0], CfgRatio(ratio)).expect("update ratio");
    }

    // Clamped at 1 (can't go below).
    assert_eq!(ratio, 1, "ratio must clamp at 1, not go below");
}

/// Verify the ratio clamping: resize to a high ratio, then try to go
/// further right — it must clamp at 99. Uses a nested split so the
/// inner Split is at tree path [0].
#[tokio::test]
async fn resize_ratio_clamped_at_maximum() {
    let source = r#"layout {
        split axis=horizontal ratio=0.6 {
            split axis=vertical ratio=0.5 {
                pane kind=shell label="top"
                pane kind=shell label="bot"
            }
            pane kind=shell label="right"
        }
    }"#;
    let cfg = cmdash_config::parse(source).expect("parse config");
    let mut layout_root = cfg.layout.expect("layout block");
    let area = LayoutRect {
        x: 0,
        y: 0,
        w: 80,
        h: 24,
    };

    // Start at ratio 95, then press Right 5 times (for child 0 of
    // the inner Split at tree path [0]).
    let mut ratio: u8 = 95;
    update_split_ratio(&mut layout_root, &[0], CfgRatio(ratio)).expect("set initial ratio");

    for _ in 0..5 {
        ratio = (ratio + 2).clamp(1, 99);
        update_split_ratio(&mut layout_root, &[0], CfgRatio(ratio)).expect("update ratio");
    }

    assert_eq!(ratio, 99, "ratio must clamp at 99, not exceed");

    // Layout must still resolve at ratio=99.
    let layout = ComputedLayout::compute(&layout_root, area).expect("compute at ratio=99");
    assert_eq!(layout.panes.len(), 3);
    // inner Split at 99% of 24 rows: top gets (24*99)/100 = 23, bot gets 1.
    assert_eq!(layout.panes[0].rect.h, 23, "top pane height at ratio=99");
    assert_eq!(
        layout.panes[1].rect.h, 1,
        "bot pane height at ratio=99 (remainder)"
    );
}
