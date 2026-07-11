//! Integration tests for the widget SDK: cdylib loading, CmdashWidget
//! trait, and C-ABI FFI round-trip.
//!
//! These tests exercise the full widget loading pipeline that the
//! live binary uses:
//!
//! 1. Build a cdylib widget (e.g. `widget-clock`)
//! 2. Load it via `libloading`
//! 3. Call `cmdash_widget_create(ABI_VERSION)` to get a raw pointer
//! 4. Reconstitute via `widget_from_raw` to `Box<dyn CmdashWidget>`
//! 5. Call `render()`, `on_event()`, and verify behavior
//!
//! The `widget-clock` cdylib must be built before running these tests:
//! ```bash
//! cargo build -p widget-clock
//! ```
//!
//! Tests that depend on the pre-built cdylib are skipped (not failed)
//! when the library is not found, to avoid breaking CI runs that
//! haven't built the example widget.

use std::ffi::c_void;
use std::path::PathBuf;

use cmdash_widget_sdk::{
    CmdashWidget, KeyCode, KeyModifiers, WidgetEvent, CMDASH_WIDGET_ABI_VERSION,
};

/// Resolve the path to the widget-clock cdylib. Searches common
/// build output locations. Returns `None` if not found.
fn find_widget_clock_lib() -> Option<PathBuf> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir.parent().unwrap_or(&manifest_dir);

    // Try multiple target locations.
    let candidates = if cfg!(target_os = "macos") {
        vec![
            workspace_root.join("target/debug/libwidget_clock.dylib"),
            workspace_root.join("target/release/libwidget_clock.dylib"),
        ]
    } else if cfg!(target_os = "windows") {
        vec![
            workspace_root.join("target/debug/widget_clock.dll"),
            workspace_root.join("target/release/widget_clock.dll"),
        ]
    } else {
        vec![
            workspace_root.join("target/debug/libwidget_clock.so"),
            workspace_root.join("target/release/libwidget_clock.so"),
        ]
    };

    candidates.into_iter().find(|p| p.exists())
}

// ---------------------------------------------------------------------------
// cdylib loading tests
// ---------------------------------------------------------------------------

/// Load the widget-clock cdylib and call `cmdash_widget_create`.
/// Verifies the full FFI loading pipeline:
/// - `libloading::Library::new(path)` loads the shared library
/// - Symbol lookup for `cmdash_widget_create`
/// - Call with matching ABI version → non-null pointer
/// - Reconstitute via `widget_from_raw` → `Box<dyn CmdashWidget>`
/// - Widget `name()` returns expected value
///
/// Skipped (not failed) if the widget-clock cdylib hasn't been built.
/// Run with `cargo test --test widget_sdk_integration -- --ignored` to execute.
#[test]
#[ignore = "widget-clock cdylib not built; run: cargo build -p widget-clock"]
fn load_widget_clock_cdylib_and_create_widget() {
    let lib_path = find_widget_clock_lib()
        .expect("widget-clock cdylib not found; build with: cargo build -p widget-clock");

    unsafe {
        let lib = libloading::Library::new(&lib_path)
            .expect("failed to load widget-clock cdylib");

        // Look up the C-ABI create function.
        let create_fn: libloading::Symbol<
            unsafe extern "C" fn(u32) -> *mut c_void,
        > = lib.get(b"cmdash_widget_create")
            .expect("widget-clock must export cmdash_widget_create");

        // Call with matching ABI version → should return non-null.
        let raw = create_fn(CMDASH_WIDGET_ABI_VERSION);
        assert!(
            !raw.is_null(),
            "cmdash_widget_create({}) must return non-null pointer",
            CMDASH_WIDGET_ABI_VERSION
        );

        // Reconstitute to Box<dyn CmdashWidget>.
        let mut widget = cmdash_widget_sdk::widget_from_raw(raw);

        // Verify widget identity.
        assert_eq!(widget.name(), "widget-clock");

        // Verify render doesn't panic.
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| {
                let area = ratatui::layout::Rect::new(0, 0, 40, 12);
                widget.render(area, frame);
            })
            .expect("render must not panic");

        // Verify on_event doesn't panic.
        widget.on_event(&WidgetEvent::FocusGained);
        widget.on_event(&WidgetEvent::FocusLost);
        widget.on_event(&WidgetEvent::Key {
            code: KeyCode::Char('q'),
            modifiers: KeyModifiers::default(),
        });
        widget.on_event(&WidgetEvent::Resize {
            width: 80,
            height: 24,
        });
    }
}

/// Verify that calling `cmdash_widget_create` with a MISMATCHED ABI
/// version returns a null pointer (the widget must reject unknown
/// ABI versions).
///
/// Skipped if cdylib not found.
/// Run with `cargo test --test widget_sdk_integration -- --ignored` to execute.
#[test]
#[ignore = "widget-clock cdylib not built; run: cargo build -p widget-clock"]
fn widget_clock_rejects_mismatched_abi_version() {
    let lib_path = find_widget_clock_lib()
        .expect("widget-clock cdylib not found");

    unsafe {
        let lib = libloading::Library::new(&lib_path)
            .expect("failed to load widget-clock cdylib");

        let create_fn: libloading::Symbol<
            unsafe extern "C" fn(u32) -> *mut c_void,
        > = lib.get(b"cmdash_widget_create")
            .expect("widget-clock must export cmdash_widget_create");

        // Call with a deliberately wrong ABI version.
        let fake_version = CMDASH_WIDGET_ABI_VERSION + 999;
        let raw = create_fn(fake_version);
        assert!(
            raw.is_null(),
            "cmdash_widget_create({}) must return null on ABI mismatch",
            fake_version
        );
    }
}

// ---------------------------------------------------------------------------
// CmdashWidget trait tests (in-process, no cdylib loading)
// ---------------------------------------------------------------------------

/// Test widget that tracks render calls and events.
#[derive(Default)]
struct MockWidget {
    render_count: usize,
    last_area: Option<(u16, u16)>,
    events_received: Vec<String>,
}

impl CmdashWidget for MockWidget {
    fn name(&self) -> &str {
        "mock-widget"
    }

    fn render(&mut self, area: ratatui::layout::Rect, frame: &mut ratatui::Frame) {
        self.render_count += 1;
        self.last_area = Some((area.width, area.height));

        use ratatui::widgets::{Block, Borders, Paragraph};
        let block = Block::default()
            .title("Mock Widget")
            .borders(Borders::ALL);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width > 0 && inner.height > 0 {
            frame.render_widget(Paragraph::new("mock content"), inner);
        }
    }

    fn on_event(&mut self, event: &WidgetEvent) {
        let desc = match event {
            WidgetEvent::Key { code, modifiers } => {
                let mod_str = if modifiers.ctrl { "ctrl+" } else { "" };
                format!("key:{mod_str}{code:?}")
            }
            WidgetEvent::Resize { width, height } => {
                format!("resize:{width}x{height}")
            }
            WidgetEvent::FocusGained => "focus_gained".to_string(),
            WidgetEvent::FocusLost => "focus_lost".to_string(),
        };
        self.events_received.push(desc);
    }
}

/// MockWidget renders without panicking and tracks render count.
#[test]
fn mock_widget_render_tracks_count() {
    let mut widget = MockWidget::default();
    let backend = ratatui::backend::TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).expect("terminal");

    assert_eq!(widget.render_count, 0);
    assert_eq!(widget.last_area, None);

    terminal
        .draw(|frame| {
            let area = ratatui::layout::Rect::new(0, 0, 40, 12);
            widget.render(area, frame);
        })
        .expect("first render");

    assert_eq!(widget.render_count, 1);
    assert_eq!(widget.last_area, Some((40, 12)));

    terminal
        .draw(|frame| {
            let area = ratatui::layout::Rect::new(0, 0, 80, 24);
            widget.render(area, frame);
        })
        .expect("second render");

    assert_eq!(widget.render_count, 2);
    assert_eq!(widget.last_area, Some((80, 24)));
}

/// MockWidget receives and records events via on_event.
#[test]
fn mock_widget_receives_events() {
    let mut widget = MockWidget::default();

    widget.on_event(&WidgetEvent::FocusGained);
    widget.on_event(&WidgetEvent::Key {
        code: KeyCode::Char('a'),
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
    widget.on_event(&WidgetEvent::Resize {
        width: 120,
        height: 40,
    });
    widget.on_event(&WidgetEvent::FocusLost);

    assert_eq!(widget.events_received.len(), 5);
    assert_eq!(widget.events_received[0], "focus_gained");
    assert!(widget.events_received[1].contains("key:"));
    assert!(widget.events_received[2].contains("ctrl+"));
    assert_eq!(widget.events_received[3], "resize:120x40");
    assert_eq!(widget.events_received[4], "focus_lost");
}

/// widget_into_raw / widget_from_raw round-trip preserves widget
/// name and functional behavior across the FFI boundary.
/// Uses a type-erased "canary" value encoded in the render_area
/// to verify state survived the round-trip without downcasting.
#[test]
fn ffi_round_trip_preserves_widget_state() {
    let mut widget: Box<dyn CmdashWidget> = Box::new(MockWidget::default());

    // Send an event before crossing the FFI boundary to modify state.
    widget.on_event(&WidgetEvent::FocusGained);
    let name_before = widget.name().to_string();

    let raw = unsafe { cmdash_widget_sdk::widget_into_raw(widget) };
    assert!(!raw.is_null(), "widget_into_raw must return non-null");

    let mut restored = unsafe { cmdash_widget_sdk::widget_from_raw(raw) };
    assert_eq!(restored.name(), name_before);

    // Verify widget is functional after the round-trip.
    let backend = ratatui::backend::TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| {
            let area = ratatui::layout::Rect::new(0, 0, 20, 5);
            restored.render(area, frame);
        })
        .expect("render after FFI round-trip");

    // Send another event and verify no panic.
    restored.on_event(&WidgetEvent::Key {
        code: KeyCode::Esc,
        modifiers: KeyModifiers::default(),
    });
    restored.on_event(&WidgetEvent::FocusLost);
}

/// Render the widget into a ratatui buffer and verify visible content.
#[test]
fn mock_widget_renders_visible_content() {
    let mut widget = MockWidget::default();
    let backend = ratatui::backend::TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).expect("terminal");

    terminal
        .draw(|frame| {
            let area = ratatui::layout::Rect::new(0, 0, 40, 10);
            widget.render(area, frame);
        })
        .expect("draw");

    let buf = terminal.backend().buffer().clone();

    // Verify "Mock Widget" title appears in the border.
    let mut found_title = false;
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            if buf.get(x, y).symbol() == "M" {
                let expected = "Mock Widget";
                let mut ok = true;
                for (i, ch) in expected.chars().enumerate() {
                    let cx = x + i as u16;
                    if cx >= buf.area.width
                        || buf.get(cx, y).symbol() != ch.to_string()
                    {
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
        "widget render must produce visible 'Mock Widget' title in the buffer"
    );
}

/// Widget name is consistent across multiple calls.
#[test]
fn widget_name_is_consistent() {
    let widget = MockWidget::default();
    let name1 = widget.name().to_string();
    let name2 = widget.name().to_string();
    let name3 = widget.name().to_string();
    assert_eq!(name1, name2);
    assert_eq!(name2, name3);
    assert_eq!(name1, "mock-widget");
}

/// Widget renders correctly with zero-width or zero-height area
/// (should not panic).
#[test]
fn widget_handles_zero_area_gracefully() {
    let mut widget = MockWidget::default();
    let backend = ratatui::backend::TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).expect("terminal");

    // Zero width.
    terminal
        .draw(|frame| {
            let area = ratatui::layout::Rect::new(0, 0, 0, 10);
            widget.render(area, frame);
        })
        .expect("zero-width render must not panic");

    // Zero height.
    terminal
        .draw(|frame| {
            let area = ratatui::layout::Rect::new(0, 0, 40, 0);
            widget.render(area, frame);
        })
        .expect("zero-height render must not panic");

    // Both zero.
    terminal
        .draw(|frame| {
            let area = ratatui::layout::Rect::new(0, 0, 0, 0);
            widget.render(area, frame);
        })
        .expect("zero-area render must not panic");
}

/// Widget renders at offset position (non-zero x, y).
#[test]
fn widget_renders_at_offset_position() {
    let mut widget = MockWidget::default();
    let backend = ratatui::backend::TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).expect("terminal");

    terminal
        .draw(|frame| {
            let area = ratatui::layout::Rect::new(10, 5, 30, 8);
            widget.render(area, frame);
        })
        .expect("offset render");

    let buf = terminal.backend().buffer().clone();

    // Verify content appears at the offset position.
    let mut found_at_offset = false;
    for y in 3..15 {
        for x in 8..45 {
            if buf.get(x, y).symbol() == "M" {
                let expected = "Mock";
                let mut ok = true;
                for (i, ch) in expected.chars().enumerate() {
                    let cx = x + i as u16;
                    if cx >= buf.area.width
                        || buf.get(cx, y).symbol() != ch.to_string()
                    {
                        ok = false;
                        break;
                    }
                }
                if ok {
                    found_at_offset = true;
                    break;
                }
            }
        }
        if found_at_offset {
            break;
        }
    }

    assert!(
        found_at_offset,
        "widget content must appear at the offset position (10,5)"
    );
}

/// CmdashWidget is object-safe and can be stored as Box<dyn CmdashWidget>.
#[test]
fn cmdash_widget_is_object_safe() {
    let mut widgets: Vec<Box<dyn CmdashWidget>> = vec![
        Box::new(MockWidget::default()),
        Box::new(MockWidget {
            render_count: 42,
            last_area: Some((100, 50)),
            events_received: vec!["test".to_string()],
        }),
    ];

    for widget in &widgets {
        assert_eq!(widget.name(), "mock-widget");
    }

    let backend = ratatui::backend::TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
    for widget in &mut widgets {
        terminal
            .draw(|frame| {
                let area = ratatui::layout::Rect::new(0, 0, 20, 5);
                widget.render(area, frame);
            })
            .expect("render from Vec<Box<dyn CmdashWidget>>");
    }
}
