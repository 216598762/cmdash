//! PaneRunner-level focus-reporting integration test.
//!
//! This test exercises the next layer above `cmdash-pty`: a real
//! `PaneRunner` backed by a spawned shell PTY. The child shell
//! emits `CSI ? 1004 h` on startup; the runner's reader thread
//! feeds those bytes back, and `tick()` advances the underlying
//! `PanePty`. We verify that `PaneRunner::focus_reporting_enabled()`
//! reflects the real child's request.

use std::time::Duration;

use cmdash::pane::PaneRunner;
use cmdash_layout::{ComputedLayout, Rect as LayoutRect};
use cmdash_pty::{PaneLayerId, ShellSpec};

/// Build a single-pane `ComputedPane` fixture suitable for spawning
/// a real PTY runner.
fn make_test_pane() -> cmdash_layout::ComputedPane {
    let cfg = cmdash_config::parse(r#"layout { pane kind=shell label="focus-pty-test" }"#)
        .expect("parse config");
    let root = cfg.layout.expect("layout block");
    let layout = ComputedLayout::compute(
        &root,
        LayoutRect {
            x: 0,
            y: 0,
            w: 80,
            h: 24,
        },
    )
    .expect("compute layout");
    layout.panes[0].clone()
}

/// A real shell PTY runner should track focus-reporting state when
/// the child emits `CSI ? 1004 h`. The sequence travels through the
/// reader thread, `tick()` advances the PTY, and the runner reports
/// that focus reporting is enabled.
#[tokio::test]
async fn pane_runner_tracks_focus_reporting_from_real_pty() {
    let pane = make_test_pane();
    let layer_id = PaneLayerId(77);
    let mut runner = PaneRunner::spawn(
        pane,
        layer_id,
        ShellSpec::Command {
            argv: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                r#"printf '\033[?1004h'; cat"#.to_string(),
            ],
        },
        cmdash_pty::DEFAULT_SCROLLBACK_CAPACITY,
    )
    .expect("spawn pane runner with real pty");

    // Initially focus reporting is not enabled.
    assert!(!runner.focus_reporting_enabled());

    // The shell emitted CSI ? 1004 h on startup. Wait for the reader
    // thread to deliver those bytes, ticking the runner to advance the
    // PTY state machine.
    let mut enabled = false;
    for _ in 0..50 {
        std::thread::sleep(Duration::from_millis(10));
        let _ = runner.tick().expect("tick runner");
        if runner.focus_reporting_enabled() {
            enabled = true;
            break;
        }
    }

    assert!(
        enabled,
        "PaneRunner should track focus-reporting enabled after child emits CSI ? 1004 h"
    );
}
