//! Wiring smoke: KDL config → layout → PanePty → vte → TextGrid
//! → ratatui `TestBackend`. Asserts that text the child emits ends
//! up in both the vte-consumed grid and the rendered ratatui
//! buffer.

use std::time::Duration;

use cmdash::pane::PaneRunner;
use cmdash_layout::{ComputedLayout, Rect as LayoutRect};
use cmdash_pty::{PaneLayerId, ShellSpec};

#[test]
fn wiring_round_trip_renders_echoed_text() {
    let source = r#"layout { pane kind=shell label="wiring" }"#;
    let cfg = cmdash_config::parse(source).expect("parse config");
    let root = cfg.layout.expect("layout block");
    let area = LayoutRect {
        x: 0,
        y: 0,
        w: 80,
        h: 24,
    };
    let layout = ComputedLayout::compute(&root, area).expect("compute layout");
    assert_eq!(layout.panes.len(), 1, "expected 1 leaf pane");
    let pane = layout.panes[0].clone();

    let layer_id = cmdash::derive_layer_id(&pane.id);
    assert_eq!(layer_id, PaneLayerId(pane.id.pre_order() as u64));

    let shell = ShellSpec::Command {
        argv: vec![
            "sh".to_string(),
            "-c".to_string(),
            "printf 'hello world\\n'; sleep 0.05; exit 0".to_string(),
        ],
    };
    let mut runner = PaneRunner::spawn(pane.clone(), layer_id, shell).expect("spawn runner");

    // Allow the child to start. Sleep once up-front lets the
    // reader thread accumulate some bytes; subsequent ticks are
    // bounded by `try_wait` + short sleeps.
    std::thread::sleep(Duration::from_millis(250));

    let mut last_snap = None;
    let mut found_in_grid = false;
    // Tick FIRST so `last_snap` is populated even when the child
    // exits faster than the first iteration's `try_wait_exit` would
    // have returned `Some(_)`.
    for _ in 0..80 {
        let snap = runner.tick().expect("tick");
        if !found_in_grid {
            for y in 0..snap.rows {
                for x in 0..snap.cols {
                    if snap.grid.cell(x, y).ch == 'h' {
                        found_in_grid = true;
                        break;
                    }
                }
                if found_in_grid {
                    break;
                }
            }
        }
        last_snap = Some(snap);
        if found_in_grid {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let snap = last_snap.expect("at least one snapshot");
    assert!(
        found_in_grid,
        "wiring did not surface 'hello' into the grid; rows={} cols={}",
        snap.rows, snap.cols
    );

    // Render to ratatui TestBackend (matches the live `Frame`
    // path: same `blit_grid` helper).
    let backend = ratatui::backend::TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| {
            let area = ratatui::layout::Rect::new(0, 0, pane.rect.w, pane.rect.h);
            cmdash::render::blit_grid(&snap.grid, frame.buffer_mut(), area);
        })
        .expect("draw");
    let buf = terminal.backend().buffer().clone();
    let mut saw_h = false;
    for y in 0..24 {
        for x in 0..80 {
            if buf.get(x, y).symbol() == "h" {
                saw_h = true;
                break;
            }
        }
    }
    assert!(saw_h, "rendered buffer did not contain 'h'");
}

// ---------------------------------------------------------------------------
// Kitty graphics end-to-end smoke: push a synthetic image onto
// GraphicsState, render and emit through dashcompositor's
// passthrough encoder, and assert the byte stream contains the
// kitty APC-G escape per AGENTS.md §"Rendering pipeline" step 6.
//
// This covers the dashcompositor wiring path without depending on
// a real PTY child or an embedded PNG byte fixture.
// ---------------------------------------------------------------------------

#[test]
fn kitty_graphics_route_emits_escape_sequence() {
    use cmdash::graphics::{GraphicsState, Metrics};
    use cmdash_pty::PaneLayerId;

    let mut graphics = GraphicsState::new(Metrics::default(), (80, 24));
    graphics.push_image(PaneLayerId(1), 7, image::RgbaImage::new(1, 1));

    let mut out = Vec::new();
    graphics
        .render_and_write(&mut out)
        .expect("render_and_write ok");

    assert!(
        out.windows(3).any(|w| w == b"\x1b_G"),
        "encoded stream missing kitty APC-G escape; bytes (first 64): {:?}",
        &out[..out.len().min(64)]
    );
}
