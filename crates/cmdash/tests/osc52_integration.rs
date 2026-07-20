//! OSC 52 clipboard integration tests through real PTY children.
//!
//! These tests exercise the full byte-flow path:
//!   shell printf -> PTY master -> reader thread -> PaneRunner::tick ->
//!   PanePty::advance -> vte OSC 52 dispatch -> PaneEvent::ClipboardOsc52 ->
//!   collect_osc52_events
//!
//! They complement the unit tests in `cmdash::main::osc52_tests` by
//! verifying that real child processes emit sequences that survive
//! the PTY and event-collection layers. The actual clipboard
//! routing (write/read) is exercised by the unit tests in
//! `cmdash::main::osc52_tests` using a mock clipboard; these
//! integration tests focus on the PTY parsing and event-extraction
//! path.

use std::time::Duration;

use cmdash::pane::{collect_osc52_events, PaneRunner};
use cmdash_layout::{ComputedLayout, Rect as LayoutRect};
use cmdash_pty::{Osc52Action, ShellSpec};

/// Spawn a single-pane runner whose child emits an single OSC 52
/// `Set` sequence for the clipboard selection (`c`). Wait for the
/// event to surface in a snapshot, then assert
/// `collect_osc52_events` extracts it with the decoded text.
#[tokio::test]
async fn osc52_set_from_real_pty_child_is_collected() {
    let source = r#"layout { pane kind=shell label="osc52-set" }"#;
    let cfg = cmdash_config::parse(source).expect("parse config");
    let root = cfg.layout.expect("layout block");
    let area = LayoutRect {
        x: 0,
        y: 0,
        w: 80,
        h: 24,
    };
    let layout = ComputedLayout::compute(&root, area).expect("compute layout");
    let pane = layout.panes[0].clone();
    let layer_id = cmdash::derive_layer_id(&pane.id);

    // "hello" base64-encoded is "aGVsbG8=".
    // printf interprets \033 as ESC and \\ as a single backslash,
    // producing the ST terminator ESC \.
    let shell = ShellSpec::Command {
        argv: vec![
            "sh".to_string(),
            "-c".to_string(),
            r"printf '\033]52;c;aGVsbG8=\033\\'".to_string(),
        ],
    };
    let mut runner = PaneRunner::spawn(
        pane.clone(),
        layer_id,
        shell,
        cmdash_pty::DEFAULT_SCROLLBACK_CAPACITY,
    )
    .expect("spawn runner");

    let mut events = Vec::new();
    for _ in 0..80 {
        tokio::time::sleep(Duration::from_millis(25)).await;
        let snap = runner.tick().expect("tick");
        let snapshots = vec![Some(snap.clone())];
        events = collect_osc52_events(std::slice::from_ref(&runner), &snapshots);
        if !events.is_empty() {
            break;
        }
    }

    assert_eq!(
        events.len(),
        1,
        "expected exactly one OSC 52 Set event from the real PTY child"
    );
    assert_eq!(events[0].0, layer_id);
    assert_eq!(events[0].1, 'c');
    assert_eq!(events[0].2, Osc52Action::Set("hello".to_string()));
}

/// Spawn a single-pane runner whose child emits an OSC 52 `Query`
/// sequence for the clipboard selection (`c`). Wait for the event
/// to surface and assert `collect_osc52_events` extracts a `Query`
/// action.
#[tokio::test]
async fn osc52_query_from_real_pty_child_is_collected() {
    let source = r#"layout { pane kind=shell label="osc52-query" }"#;
    let cfg = cmdash_config::parse(source).expect("parse config");
    let root = cfg.layout.expect("layout block");
    let area = LayoutRect {
        x: 0,
        y: 0,
        w: 80,
        h: 24,
    };
    let layout = ComputedLayout::compute(&root, area).expect("compute layout");
    let pane = layout.panes[0].clone();
    let layer_id = cmdash::derive_layer_id(&pane.id);

    let shell = ShellSpec::Command {
        argv: vec![
            "sh".to_string(),
            "-c".to_string(),
            r"printf '\033]52;c;?\033\\'".to_string(),
        ],
    };
    let mut runner = PaneRunner::spawn(
        pane.clone(),
        layer_id,
        shell,
        cmdash_pty::DEFAULT_SCROLLBACK_CAPACITY,
    )
    .expect("spawn runner");

    let mut events = Vec::new();
    for _ in 0..80 {
        tokio::time::sleep(Duration::from_millis(25)).await;
        let snap = runner.tick().expect("tick");
        let snapshots = vec![Some(snap.clone())];
        events = collect_osc52_events(std::slice::from_ref(&runner), &snapshots);
        if !events.is_empty() {
            break;
        }
    }

    assert_eq!(
        events.len(),
        1,
        "expected exactly one OSC 52 Query event from the real PTY child"
    );
    assert_eq!(events[0].0, layer_id);
    assert_eq!(events[0].1, 'c');
    assert_eq!(events[0].2, Osc52Action::Query);
}

/// Spawn a single-pane runner whose child emits both a `Set` and a
/// `Query` sequence. Assert both events are collected in the
/// correct order.
#[tokio::test]
async fn osc52_set_and_query_from_real_pty_child_are_collected_in_order() {
    let source = r#"layout { pane kind=shell label="osc52-both" }"#;
    let cfg = cmdash_config::parse(source).expect("parse config");
    let root = cfg.layout.expect("layout block");
    let area = LayoutRect {
        x: 0,
        y: 0,
        w: 80,
        h: 24,
    };
    let layout = ComputedLayout::compute(&root, area).expect("compute layout");
    let pane = layout.panes[0].clone();
    let layer_id = cmdash::derive_layer_id(&pane.id);

    // Emit Set for "hi" (base64: aGk=) then Query.
    let shell = ShellSpec::Command {
        argv: vec![
            "sh".to_string(),
            "-c".to_string(),
            r"printf '\033]52;c;aGk=\033\\\033]52;c;?\033\\\\'".to_string(),
        ],
    };
    let mut runner = PaneRunner::spawn(
        pane.clone(),
        layer_id,
        shell,
        cmdash_pty::DEFAULT_SCROLLBACK_CAPACITY,
    )
    .expect("spawn runner");

    let mut events = Vec::new();
    for _ in 0..80 {
        tokio::time::sleep(Duration::from_millis(25)).await;
        let snap = runner.tick().expect("tick");
        let snapshots = vec![Some(snap.clone())];
        events = collect_osc52_events(std::slice::from_ref(&runner), &snapshots);
        if events.len() >= 2 {
            break;
        }
    }

    assert_eq!(
        events.len(),
        2,
        "expected both OSC 52 Set and Query events from the real PTY child"
    );
    assert_eq!(events[0].2, Osc52Action::Set("hi".to_string()));
    assert_eq!(events[1].2, Osc52Action::Query);
}

/// Empty OSC 52 data produces a Set empty string event.
/// The VTE parser delivers the empty data field to the OSC 52 handler,
/// which base64-decodes it to an empty string rather than treating it
/// as a Query (only `?` is treated as Query).
#[tokio::test]
async fn osc52_empty_data_produces_empty_set() {
    let source = r#"layout { pane kind=shell label="osc52-empty" }"#;
    let cfg = cmdash_config::parse(source).expect("parse config");
    let root = cfg.layout.expect("layout block");
    let area = LayoutRect {
        x: 0,
        y: 0,
        w: 80,
        h: 24,
    };
    let layout = ComputedLayout::compute(&root, area).expect("compute layout");
    let pane = layout.panes[0].clone();
    let layer_id = cmdash::derive_layer_id(&pane.id);

    // OSC 52 with empty data field: ESC]52;c;ESC\
    let shell = ShellSpec::Command {
        argv: vec![
            "sh".to_string(),
            "-c".to_string(),
            r"printf '\033]52;c;\033\\'".to_string(),
        ],
    };
    let mut runner = PaneRunner::spawn(
        pane.clone(),
        layer_id,
        shell,
        cmdash_pty::DEFAULT_SCROLLBACK_CAPACITY,
    )
    .expect("spawn runner");

    let mut events = Vec::new();
    for _ in 0..80 {
        tokio::time::sleep(Duration::from_millis(25)).await;
        let snap = runner.tick().expect("tick");
        let snapshots = vec![Some(snap.clone())];
        events = collect_osc52_events(std::slice::from_ref(&runner), &snapshots);
        if !events.is_empty() {
            break;
        }
    }

    assert_eq!(events.len(), 1, "expected one OSC 52 event for empty data");
    assert_eq!(events[0].1, 'c');
    assert_eq!(events[0].2, Osc52Action::Set(String::new()));
}

/// OSC 52 targeting the secondary selection ('s') should preserve
/// the clipboard identifier in the collected event.
#[tokio::test]
async fn osc52_secondary_selection_preserves_buffer_char() {
    let source = r#"layout { pane kind=shell label="osc52-secondary" }"#;
    let cfg = cmdash_config::parse(source).expect("parse config");
    let root = cfg.layout.expect("layout block");
    let area = LayoutRect {
        x: 0,
        y: 0,
        w: 80,
        h: 24,
    };
    let layout = ComputedLayout::compute(&root, area).expect("compute layout");
    let pane = layout.panes[0].clone();
    let layer_id = cmdash::derive_layer_id(&pane.id);

    // "test" base64-encoded is "dGVzdA==".
    // Use 's' (secondary selection) instead of 'c'.
    let shell = ShellSpec::Command {
        argv: vec![
            "sh".to_string(),
            "-c".to_string(),
            r"printf '\033]52;s;dGVzdA==\033\\'".to_string(),
        ],
    };
    let mut runner = PaneRunner::spawn(
        pane.clone(),
        layer_id,
        shell,
        cmdash_pty::DEFAULT_SCROLLBACK_CAPACITY,
    )
    .expect("spawn runner");

    let mut events = Vec::new();
    for _ in 0..80 {
        tokio::time::sleep(Duration::from_millis(25)).await;
        let snap = runner.tick().expect("tick");
        let snapshots = vec![Some(snap.clone())];
        events = collect_osc52_events(std::slice::from_ref(&runner), &snapshots);
        if !events.is_empty() {
            break;
        }
    }

    assert_eq!(
        events.len(),
        1,
        "expected one OSC 52 event for secondary selection"
    );
    assert_eq!(
        events[0].1, 's',
        "clipboard buffer char should be 's' for secondary"
    );
    assert_eq!(events[0].2, Osc52Action::Set("test".to_string()));
}

/// OSC 52 with invalid base64 data should be silently dropped
/// (no event emitted).
#[tokio::test]
async fn osc52_invalid_base64_is_silently_dropped() {
    let source = r#"layout { pane kind=shell label="osc52-invalid" }"#;
    let cfg = cmdash_config::parse(source).expect("parse config");
    let root = cfg.layout.expect("layout block");
    let area = LayoutRect {
        x: 0,
        y: 0,
        w: 80,
        h: 24,
    };
    let layout = ComputedLayout::compute(&root, area).expect("compute layout");
    let pane = layout.panes[0].clone();
    let layer_id = cmdash::derive_layer_id(&pane.id);

    // Invalid base64: "!!!" is not valid base64.
    let shell = ShellSpec::Command {
        argv: vec![
            "sh".to_string(),
            "-c".to_string(),
            r"printf '\033]52;c;!!!\033\\'".to_string(),
        ],
    };
    let mut runner = PaneRunner::spawn(
        pane.clone(),
        layer_id,
        shell,
        cmdash_pty::DEFAULT_SCROLLBACK_CAPACITY,
    )
    .expect("spawn runner");

    let mut any_events = false;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(25)).await;
        let snap = runner.tick().expect("tick");
        let snapshots = vec![Some(snap.clone())];
        let events = collect_osc52_events(std::slice::from_ref(&runner), &snapshots);
        if !events.is_empty() {
            any_events = true;
            break;
        }
    }

    assert!(
        !any_events,
        "invalid base64 should produce no OSC 52 events"
    );
}
