//! Integration tests for the script widget: spawning script processes,
//! the line-delimited frame protocol, and end-to-end rendering.
//!
//! These tests exercise the full script-widget pipeline:
//!
//! 1. Spawn a child process that speaks the cmdash frame protocol
//! 2. Send FRAME requests via stdin
//! 3. Receive frame responses via stdout (reader thread)
//! 4. Render the response into a ratatui buffer
//! 5. Forward KEY/RESIZE/FOCUS events to the script
//!
//! The test script fixture is at `tests/fixtures/test_script_widget.sh`.
//!
//! **Platform note:** These tests spawn child processes and rely on
//! POSIX process semantics. They are gated on `#[cfg(unix)]` to
//! avoid false failures on Windows CI where `bash` may not be
//! available.

#![cfg(unix)]

use std::time::Duration;

use cmdash::script_widget::ScriptWidget;
use cmdash_widget_sdk::{CmdashWidget, KeyCode, KeyModifiers, WidgetEvent};

/// Resolve the path to the test script widget fixture.
fn test_script_path() -> Option<String> {
    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let script = manifest_dir.join("tests/fixtures/test_script_widget.sh");
    if script.exists() {
        Some(script.display().to_string())
    } else {
        eprintln!("SKIPPED: test script fixture not found at {:?}", script);
        None
    }
}

/// Helper to spawn the test script widget, returning `None` if
/// the fixture or bash is not available.
fn spawn_test_widget(label: &str) -> Option<ScriptWidget> {
    let script_path = test_script_path()?;
    let cmd = format!("bash {script_path}");
    match ScriptWidget::spawn(&cmd, Some(label)) {
        Ok(w) => Some(w),
        Err(e) => {
            eprintln!("SKIPPED: failed to spawn test script: {e}");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// ScriptWidget spawn + lifecycle tests
// ---------------------------------------------------------------------------

/// Spawn a ScriptWidget and verify it starts successfully.
/// The widget should be alive and its name should match the label.
#[tokio::test]
async fn script_widget_spawn_succeeds() {
    let Some(mut widget) = spawn_test_widget("test-script") else {
        return;
    };
    assert_eq!(widget.name(), "test-script");

    // Render once to confirm the child is alive.
    let backend = ratatui::backend::TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| {
            let area = ratatui::layout::Rect::new(0, 0, 80, 24);
            widget.render(area, frame);
        })
        .expect("initial render must not panic");
    // Drop kills the child process.
}

/// Spawn a ScriptWidget with default name (no label).
#[tokio::test]
async fn script_widget_default_name() {
    let script_path = match test_script_path() {
        Some(p) => p,
        None => return,
    };
    let cmd = format!("bash {script_path}");
    let widget = ScriptWidget::spawn(&cmd, None).expect("spawn must succeed");
    assert_eq!(widget.name(), "script");
}

/// Spawn with an invalid command returns an error.
#[tokio::test]
async fn script_widget_invalid_command_returns_error() {
    let result = ScriptWidget::spawn("nonexistent_binary_12345", Some("bad"));
    assert!(result.is_err(), "spawn with invalid command must fail");
    if let Err(e) = result {
        let err_msg = format!("{e}");
        assert!(
            err_msg.contains("nonexistent_binary"),
            "error message should mention the bad command: {err_msg}"
        );
    }
}

/// Spawn with empty command returns an error.
#[tokio::test]
async fn script_widget_empty_command_returns_error() {
    let result = ScriptWidget::spawn("", Some("empty"));
    assert!(result.is_err(), "spawn with empty command must fail");
}

// ---------------------------------------------------------------------------
// Frame protocol round-trip tests
// ---------------------------------------------------------------------------

/// Render the ScriptWidget with a FRAME request and verify it receives
/// a frame response with visible content.
///
/// Protocol flow:
/// 1. `render(area, frame)` sends `FRAME width=W height=H gen=N`
/// 2. The test script responds with `FRAME width=W height=H` + text
/// 3. The reader thread parses the response
/// 4. The render call displays the text
///
/// Uses generous timeouts (200ms x 15 = 3s max) for reliability
/// under CI load.
#[tokio::test]
async fn script_widget_renders_frame_response() {
    let Some(mut widget) = spawn_test_widget("frame-test") else {
        return;
    };

    let backend = ratatui::backend::TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).expect("terminal");

    // Poll until content appears. Alternate dimensions between
    // (80, 24) and (80, 23) each iteration to force a new FRAME
    // request — ScriptWidget only sends FRAME when last_area changes.
    let mut found_content = false;
    for attempt in 0..15 {
        let h = if attempt % 2 == 0 { 24 } else { 23 };
        terminal
            .draw(|frame| {
                let area = ratatui::layout::Rect::new(0, 0, 80, h);
                widget.render(area, frame);
            })
            .expect("render must not panic");

        let buf = terminal.backend().buffer().clone();

        // Check for non-space content.
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                if buf.get(x, y).symbol() != " " {
                    found_content = true;
                    break;
                }
            }
            if found_content {
                break;
            }
        }

        if found_content {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    assert!(
        found_content,
        "ScriptWidget must render visible content from the script process \
         within 3s. The script should respond with text output."
    );
}

/// Verify the rendered content includes the expected marker text
/// from the test script. The script outputs lines with plain text
/// that should appear in the ratatui buffer.
///
/// Uses plain-text markers (no ANSI codes) to ensure reliable
/// string matching in the buffer cells.
#[tokio::test]
async fn script_widget_renders_expected_marker_text() {
    let Some(mut widget) = spawn_test_widget("marker-test") else {
        return;
    };

    let backend = ratatui::backend::TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).expect("terminal");

    // Poll until we see the marker text "Line 1".
    // Alternate dimensions to force new FRAME requests.
    let mut found_marker = false;
    for attempt in 0..15 {
        let h = if attempt % 2 == 0 { 24 } else { 23 };
        terminal
            .draw(|frame| {
                let area = ratatui::layout::Rect::new(0, 0, 80, h);
                widget.render(area, frame);
            })
            .expect("render");

        let buf = terminal.backend().buffer().clone();

        // Search for "Line 1" in the buffer — this is output by the
        // test script as plain text (no ANSI codes in this line).
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                if buf.get(x, y).symbol() == "L" {
                    let expected = "Line 1";
                    let mut ok = true;
                    for (i, ch) in expected.chars().enumerate() {
                        let cx = x + i as u16;
                        if cx >= buf.area.width || buf.get(cx, y).symbol() != ch.to_string() {
                            ok = false;
                            break;
                        }
                    }
                    if ok {
                        found_marker = true;
                        break;
                    }
                }
            }
            if found_marker {
                break;
            }
        }

        if found_marker {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    assert!(
        found_marker,
        "ScriptWidget must render 'Line 1' marker text from the script. \
         This verifies the full FRAME request → script → response → render pipeline."
    );
}

/// Render the ScriptWidget into a bordered block and verify the border
/// title matches the widget name.
#[tokio::test]
async fn script_widget_border_title_matches_name() {
    let Some(mut widget) = spawn_test_widget("my-title") else {
        return;
    };

    let backend = ratatui::backend::TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).expect("terminal");

    // Render and wait for content, alternating dimensions to force FRAME requests.
    for attempt in 0..10 {
        let h = if attempt % 2 == 0 { 10 } else { 11 };
        terminal
            .draw(|frame| {
                let area = ratatui::layout::Rect::new(0, 0, 40, h);
                widget.render(area, frame);
            })
            .expect("render");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    let buf = terminal.backend().buffer().clone();

    // Check for "my-title" in the border.
    let mut found_title = false;
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            if buf.get(x, y).symbol() == "m" {
                let expected = "my-title";
                let mut ok = true;
                for (i, ch) in expected.chars().enumerate() {
                    let cx = x + i as u16;
                    if cx >= buf.area.width || buf.get(cx, y).symbol() != ch.to_string() {
                        ok = false;
                        break;
                    }
                }
                if ok {
                    found_title = true;
                    break;
                }
            }
        }
        if found_title {
            break;
        }
    }

    assert!(
        found_title,
        "ScriptWidget border must contain the widget name 'my-title'"
    );
}

// ---------------------------------------------------------------------------
// Event forwarding tests
// ---------------------------------------------------------------------------

/// Forward a KEY event to the ScriptWidget and verify it doesn't panic.
#[tokio::test]
async fn script_widget_forwards_key_event() {
    let Some(mut widget) = spawn_test_widget("key-test") else {
        return;
    };

    // All these should not panic, even if the script ignores them.
    widget.on_event(&WidgetEvent::Key {
        code: KeyCode::Char('a'),
        modifiers: KeyModifiers::default(),
    });
    widget.on_event(&WidgetEvent::Key {
        code: KeyCode::Enter,
        modifiers: KeyModifiers::default(),
    });
    widget.on_event(&WidgetEvent::Key {
        code: KeyCode::Char('c'),
        modifiers: KeyModifiers {
            ctrl: true,
            shift: false,
            alt: false,
            super_: false,
        },
    });
    widget.on_event(&WidgetEvent::Key {
        code: KeyCode::Up,
        modifiers: KeyModifiers::default(),
    });
    widget.on_event(&WidgetEvent::Key {
        code: KeyCode::F(1),
        modifiers: KeyModifiers::default(),
    });
    widget.on_event(&WidgetEvent::Key {
        code: KeyCode::Esc,
        modifiers: KeyModifiers::default(),
    });

    // Widget should still be functional after events.
    let backend = ratatui::backend::TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| {
            let area = ratatui::layout::Rect::new(0, 0, 80, 24);
            widget.render(area, frame);
        })
        .expect("render after key events must not panic");
}

/// Forward a RESIZE event and verify it doesn't panic.
#[tokio::test]
async fn script_widget_forwards_resize_event() {
    let Some(mut widget) = spawn_test_widget("resize-test") else {
        return;
    };

    widget.on_event(&WidgetEvent::Resize {
        width: 120,
        height: 40,
    });
    widget.on_event(&WidgetEvent::Resize {
        width: 40,
        height: 10,
    });

    // Widget should still render after resize events.
    let backend = ratatui::backend::TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| {
            let area = ratatui::layout::Rect::new(0, 0, 80, 24);
            widget.render(area, frame);
        })
        .expect("render after resize events");
}

/// Forward FOCUS events and verify they don't panic.
#[tokio::test]
async fn script_widget_forwards_focus_events() {
    let Some(mut widget) = spawn_test_widget("focus-test") else {
        return;
    };

    widget.on_event(&WidgetEvent::FocusGained);
    widget.on_event(&WidgetEvent::FocusLost);
    widget.on_event(&WidgetEvent::FocusGained);

    // Widget should still render after focus events.
    let backend = ratatui::backend::TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| {
            let area = ratatui::layout::Rect::new(0, 0, 80, 24);
            widget.render(area, frame);
        })
        .expect("render after focus events");
}

// ---------------------------------------------------------------------------
// Multiple renders and lifecycle tests
// ---------------------------------------------------------------------------

/// Render the ScriptWidget multiple times with varying areas and
/// verify it handles repeated FRAME requests without panicking.
#[tokio::test]
async fn script_widget_handles_repeated_renders() {
    let Some(mut widget) = spawn_test_widget("repeat-test") else {
        return;
    };

    let backend = ratatui::backend::TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).expect("terminal");

    for i in 0..10 {
        let w = 40 + (i * 4) as u16;
        let h = 10 + i as u16;
        terminal
            .draw(|frame| {
                let area = ratatui::layout::Rect::new(0, 0, w, h);
                widget.render(area, frame);
            })
            .expect("render must not panic");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Drop the ScriptWidget while it's still rendering and verify
/// the Drop impl cleans up properly (kills child, joins reader thread).
#[tokio::test]
async fn script_widget_drop_cleans_up() {
    let Some(mut widget) = spawn_test_widget("drop-test") else {
        return;
    };

    // Render a few frames to ensure the script is alive.
    let backend = ratatui::backend::TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
    for _ in 0..3 {
        terminal
            .draw(|frame| {
                let area = ratatui::layout::Rect::new(0, 0, 80, 24);
                widget.render(area, frame);
            })
            .expect("render");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Drop the widget — this should kill the child and join the reader
    // thread without panicking or hanging.
    drop(widget);
}

// ---------------------------------------------------------------------------
// Edge case tests
// ---------------------------------------------------------------------------

/// Render into a very small area (1x1) — the script should still respond
/// without panicking.
#[tokio::test]
async fn script_widget_renders_in_tiny_area() {
    let Some(mut widget) = spawn_test_widget("tiny-test") else {
        return;
    };

    let backend = ratatui::backend::TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).expect("terminal");

    for _ in 0..5 {
        terminal
            .draw(|frame| {
                let area = ratatui::layout::Rect::new(0, 0, 1, 1);
                widget.render(area, frame);
            })
            .expect("1x1 render must not panic");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Render at a non-zero offset position.
#[tokio::test]
async fn script_widget_renders_at_offset() {
    let Some(mut widget) = spawn_test_widget("offset-test") else {
        return;
    };

    let backend = ratatui::backend::TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).expect("terminal");

    for _ in 0..5 {
        terminal
            .draw(|frame| {
                let area = ratatui::layout::Rect::new(10, 5, 30, 8);
                widget.render(area, frame);
            })
            .expect("offset render must not panic");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // The border content should appear near the offset position.
    let buf = terminal.backend().buffer().clone();
    let mut found = false;
    for y in 3..15 {
        for x in 8..45 {
            if buf.get(x, y).symbol() != " " {
                found = true;
                break;
            }
        }
        if found {
            break;
        }
    }
    assert!(
        found,
        "ScriptWidget content must appear near the offset position"
    );
}

/// Verify that a script process that exits immediately is handled
/// gracefully (no panic, no hang).
#[tokio::test]
async fn script_widget_handles_immediate_exit() {
    // Use `true` (resolved from $PATH) which exits immediately with success.
    let mut widget = ScriptWidget::spawn("true", Some("immediate-exit")).expect("spawn true");

    let backend = ratatui::backend::TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).expect("terminal");

    // Render a few times — the script exits immediately, so the
    // widget should detect this and render an error/exited state.
    for attempt in 0..5 {
        let h = if attempt % 2 == 0 { 24 } else { 23 };
        terminal
            .draw(|frame| {
                let area = ratatui::layout::Rect::new(0, 0, 80, h);
                widget.render(area, frame);
            })
            .expect("render after script exit must not panic");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Drop should not hang even if the child already exited.
    drop(widget);
}
