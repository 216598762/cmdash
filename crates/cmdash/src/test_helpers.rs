//! Shared test helpers for full-loop FrontendTask + ServerTask integration tests.
//!
//! This module provides reusable infrastructure for driving a complete
//! `FrontendTask` + `ServerTask` async event loop in tests, including:
//!
//! - Layout helpers (`single_layout`, `split_h_layout`, `split_v_layout`)
//! - Router helpers (`quit_router`, `resize_router`, `tab_router`)
//! - Server/frontend builders (`build_server`, `make_server_with_tracking_pty`,
//!   `make_frontend_with_router`)
//! - Async wait helpers (`wait_for_resize_calls`, `wait_for_input_writes`,
//!   `wait_for_no_input_writes`)
//! - Server-message helpers (`drain_frames`, `last_frame_focus`, `last_frame_mode`)
//! - The `run_full_loop_test` orchestrator
//!
//! Tests that exercise a specific feature (resize, key input, mouse, tabs,
//! paste) live in `crates/cmdash/src/frontend_task.rs` `#[cfg(test)] mod tests`
//! and call into these helpers.
//!
//! ## Helper → test mapping
//!
//! All frontend tests live in `frontend_task::tests::frontend_*` and are run
//! via:
//!
//! ```sh
//! cargo test -p cmdash --lib -- frontend_task::tests::frontend
//! ```
//!
//! Server-task unit tests live in `server_task::tests` and are run via:
//!
//! ```sh
//! cargo test -p cmdash --lib -- server_task::tests
//! ```
//!
//! ### Event constructors
//!
//! | Helper | Used by |
//! |--------|---------|
//! | `key()` | All keyboard tests (`frontend_forwards_*`, `frontend_keyboard_*`,
//! |        | `frontend_resize_*`, `frontend_does_not_forward_matched_action_as_input`,
//! |        | `frontend_tab_*`, `frontend_paste_*`) |
//! | `mouse_event()` | `frontend_mouse_click_to_focus_*`, `frontend_alt_drag_resize_*` |
//!
//! ### Layout helpers
//!
//! | Helper | Used by |
//! |--------|---------|
//! | `single_layout()` | `frontend_forwards_*`, `frontend_does_not_forward_*`,
//! |                    | `frontend_paste_*` |
//! | `split_h_layout()` | `frontend_resize_drives_per_pane_pty_resize_in_split`,
//! |                     | `frontend_mouse_click_to_focus_*`,
//! |                     | `frontend_alt_drag_resize_*`,
//! |                     | `frontend_keyboard_resize_root_horizontal_split`,
//! |                     | `server_task::tests` (split-based tests) |
//! | `split_v_layout()` | `frontend_keyboard_resize_root_vertical_split`,
//! |                     | `server_task::tests` (vertical split tests) |
//!
//! ### Router helpers
//!
//! | Helper | Used by |
//! |--------|---------|
//! | `quit_router()` | Most tests (default quit binding `'q'` → `AppClose`). |
//! |                 | Also reused as a base inside `resize_router()` and `tab_router()` |
//! | `resize_router()` | `frontend_keyboard_resize_root_*_split` |
//! | `tab_router()`    | `frontend_tab_close_on_last_tab_stops_server` |
//!
//! ### Server/frontend builders
//!
//! | Helper | Used by |
//! |--------|---------|
//! | `build_server()` | Generic server builder; used by `make_server_with_tracking_pty`
//! |                  | and directly by `server_task::tests` for custom PTY factories |
//! | `make_server_with_tracking_pty()` | All `frontend_*` full-loop tests |
//! | `make_frontend_with_router()` | All `frontend_*` full-loop tests |
//!
//! ### Async wait helpers
//!
//! | Helper | Used by |
//! |--------|---------|
//! | `wait_for_resize_calls()` | All resize tests (`frontend_resize_*`),
//! |                           | all keyboard resize tests (`frontend_keyboard_*`),
//! |                           | tab and paste tests (to wait for initial layout) |
//! | `wait_for_input_writes()` | `frontend_forwards_unmatched_key_to_pty`,
//! |                           | `frontend_forwards_special_keys_to_pty`,
//! |                           | `frontend_forwards_key_with_modifiers_to_pty`,
//! |                           | `frontend_mouse_click_to_focus_*`,
//! |                           | `frontend_paste_forwards_text_to_pty` |
//! | `wait_for_no_input_writes()` | `frontend_does_not_forward_matched_action_as_input` |
//!
//! ### Server-message helpers
//!
//! | Helper | Used by |
//! |--------|---------|
//! | `drain_frames()` | `server_task::tests` (all frame-based tests) |
//! | `last_frame_focus()` | `server_task::tests` (focus assertion tests) |
//! | `last_frame_mode()` | `server_task::tests` (mode transition tests) |
//!
//! ### Full-loop orchestrator
//!
//! `run_full_loop_test()` is used by every full-loop test — it wires up the
//! `ServerTask` + `FrontendTask` pair, spawns the server, and drives the
//! input closure. Tests only need to supply a layout, a router, and a
//! closure that sends events and returns a `JoinHandle`.
//!
//! ### Notes
//!
//! - `main.rs` has its own `test_helpers::StubPty` (unrelated to this module).
//! - `server_task::tests` uses `StubPty`-based unit tests for sync state checks
//!   but imports layout helpers and message helpers from this module
//!   (see tables above for the full mapping).
//! - `TabNew`/`TabSwitch` full-loop tests are omitted because `create_new_tab`
//!   spawns real PTY shells that exit immediately in CI; unit-level coverage in
//!   `server_task::tests::tab_operations` fills this gap.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cmdash_config::{
    KeyAction, KeyToken, Keybind, LayoutNode, Modifiers as CfgModifiers, Pane as CfgPane, PaneKind,
};
use cmdash_keybinds::Router;
use cmdash_layout::{ComputedLayout, Rect as LayoutRect};
use cmdash_pty::{PaneLayerId, ShellSpec};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::graphics::{GraphicsState, Metrics};
use crate::pane::PaneRunner;
use crate::protocol::{ClientMessage, ServerConfig, ServerMessage};
use crate::server_task::ServerTask;
use crate::test_pty::{TrackingPty, TrackingState};

// ---------------------------------------------------------------------------
// Event constructors
// ---------------------------------------------------------------------------

pub fn key(code: KeyCode, mods: KeyModifiers) -> Event {
    Event::Key(KeyEvent {
        code,
        modifiers: mods,
        kind: KeyEventKind::Press,
        state: crossterm::event::KeyEventState::NONE,
    })
}

pub fn mouse_event(
    kind: crossterm::event::MouseEventKind,
    column: u16,
    row: u16,
    modifiers: KeyModifiers,
) -> Event {
    Event::Mouse(crossterm::event::MouseEvent {
        kind,
        column,
        row,
        modifiers,
    })
}

// ---------------------------------------------------------------------------
// Layout helpers
// ---------------------------------------------------------------------------

pub fn single_layout() -> LayoutNode {
    LayoutNode::Pane(CfgPane {
        kind: PaneKind::Shell,
        label: None,
        command: None,
        scrollback_capacity: None,
    })
}

pub fn split_h_layout() -> LayoutNode {
    LayoutNode::Split {
        axis: cmdash_config::SplitAxis::Horizontal,
        ratio: cmdash_config::Ratio(50),
        children: vec![
            LayoutNode::Pane(CfgPane {
                kind: PaneKind::Shell,
                label: None,
                command: None,
                scrollback_capacity: None,
            }),
            LayoutNode::Pane(CfgPane {
                kind: PaneKind::Shell,
                label: None,
                command: None,
                scrollback_capacity: None,
            }),
        ],
    }
}

pub fn split_v_layout() -> LayoutNode {
    LayoutNode::Split {
        axis: cmdash_config::SplitAxis::Vertical,
        ratio: cmdash_config::Ratio(50),
        children: vec![
            LayoutNode::Pane(CfgPane {
                kind: PaneKind::Shell,
                label: None,
                command: None,
                scrollback_capacity: None,
            }),
            LayoutNode::Pane(CfgPane {
                kind: PaneKind::Shell,
                label: None,
                command: None,
                scrollback_capacity: None,
            }),
        ],
    }
}

// ---------------------------------------------------------------------------
// Router helpers
// ---------------------------------------------------------------------------

/// Router that binds 'q' -> AppClose so tests can shut down the server.
pub fn quit_router() -> Router {
    Router::new(vec![Keybind {
        mods: CfgModifiers::default(),
        key: KeyToken::Char('q'),
        action: KeyAction::AppClose,
    }])
}

/// Router that binds 'r' -> EnterPaneResize and 'q' -> AppClose,
/// so tests can exercise keyboard-driven pane resizing. Arrow keys
/// in PaneResize mode are provided by Router's default mode binds.
pub fn resize_router() -> Router {
    Router::new(vec![
        Keybind {
            mods: CfgModifiers::default(),
            key: KeyToken::Char('r'),
            action: KeyAction::EnterPaneResize,
        },
        Keybind {
            mods: CfgModifiers::default(),
            key: KeyToken::Char('q'),
            action: KeyAction::AppClose,
        },
    ])
}

/// Router that binds tab operations and quit for tab integration tests.
#[allow(dead_code)]
pub fn tab_router() -> Router {
    Router::new(vec![
        Keybind {
            mods: CfgModifiers::default(),
            key: KeyToken::Char('n'),
            action: KeyAction::TabNew,
        },
        Keybind {
            mods: CfgModifiers::default(),
            key: KeyToken::Char('x'),
            action: KeyAction::TabClose,
        },
        Keybind {
            mods: CfgModifiers::default(),
            key: KeyToken::Char('1'),
            action: KeyAction::TabSwitch(1),
        },
        Keybind {
            mods: CfgModifiers::default(),
            key: KeyToken::Char('2'),
            action: KeyAction::TabSwitch(2),
        },
        Keybind {
            mods: CfgModifiers::default(),
            key: KeyToken::Char('q'),
            action: KeyAction::AppClose,
        },
    ])
}

// ---------------------------------------------------------------------------
// Server / frontend builders
// ---------------------------------------------------------------------------

/// Generic server builder shared across all test modules.
///
/// Accepts a PTY factory closure that maps `(PaneLayerId) -> Box<dyn PanePtyOps>`.
/// Returns `(ServerTask, client_tx, server_rx, close_tx_handle)`.
pub fn build_server<F>(
    layout_root: LayoutNode,
    focus: usize,
    presets: BTreeMap<String, LayoutNode>,
    make_pty: F,
) -> (
    ServerTask,
    tokio::sync::mpsc::UnboundedSender<ClientMessage>,
    tokio::sync::mpsc::UnboundedReceiver<ServerMessage>,
    tokio::sync::mpsc::UnboundedSender<PaneLayerId>,
)
where
    F: Fn(PaneLayerId) -> Box<dyn cmdash_pty::PanePtyOps + Send>,
{
    let area = LayoutRect {
        x: 0,
        y: 0,
        w: 80,
        h: 24,
    };
    let layout = ComputedLayout::compute(&layout_root, area).unwrap();
    let runners: Vec<PaneRunner> = layout
        .panes
        .iter()
        .enumerate()
        .map(|(i, pane)| {
            let lid = PaneLayerId(i as u64 + 1);
            let pty = make_pty(lid);
            PaneRunner::with_pty_for_test(pane.clone(), lid, pty, None)
        })
        .collect();
    let config = ServerConfig {
        layout_root,
        presets,
        shell: ShellSpec::LoginShell,
        status_bar: None,
        theme: cmdash_config::Theme::default(),
        widget_factories: HashMap::new(),
    };
    let (client_tx, client_rx) = tokio::sync::mpsc::unbounded_channel();
    let (server_tx, server_rx) = tokio::sync::mpsc::unbounded_channel();
    let (close_tx, close_rx) = tokio::sync::mpsc::unbounded_channel();
    let close_tx_handle = close_tx.clone();
    let server = ServerTask::new(
        config,
        runners,
        focus,
        area,
        crate::server_task::ServerChannels {
            close_tx,
            close_rx,
            config_reload_rx: None,
            client_rx,
            server_tx,
        },
    );
    (server, client_tx, server_rx, close_tx_handle)
}

/// Build a `ServerTask` backed by `TrackingPty` runners and
/// expose the channels/state needed to drive the full loop.
pub fn make_server_with_tracking_pty(
    layout_root: LayoutNode,
) -> (
    ServerTask,
    tokio::sync::mpsc::UnboundedSender<ClientMessage>,
    tokio::sync::mpsc::UnboundedReceiver<ServerMessage>,
    Arc<Mutex<TrackingState>>,
) {
    let state = Arc::new(Mutex::new(TrackingState::default()));
    let state_clone = Arc::clone(&state);
    let (server, client_tx, server_rx, _close_tx) =
        build_server(layout_root, 0, BTreeMap::new(), move |lid| {
            Box::new(TrackingPty {
                layer_id: lid,
                state: Arc::clone(&state_clone),
            })
        });
    (server, client_tx, server_rx, state)
}

/// Build a frontend wired to the supplied client/server channels
/// using the provided keybind router.
pub fn make_frontend_with_router<'a>(
    terminal: &'a mut ratatui::Terminal<ratatui::backend::TestBackend>,
    client_tx: tokio::sync::mpsc::UnboundedSender<ClientMessage>,
    server_rx: tokio::sync::mpsc::UnboundedReceiver<ServerMessage>,
    bindings: Router,
) -> crate::frontend_task::FrontendTask<'a, ratatui::backend::TestBackend> {
    let gs = GraphicsState::new(Metrics::default(), (80, 24));
    crate::frontend_task::FrontendTask::new(terminal, gs, bindings, client_tx, server_rx)
}

// ---------------------------------------------------------------------------
// Async wait helpers
// ---------------------------------------------------------------------------

/// Poll the shared `TrackingState` until `resize_calls` reaches
/// `expected` or the timeout elapses. Returns the collected calls.
pub async fn wait_for_resize_calls(
    state: Arc<Mutex<TrackingState>>,
    expected: usize,
) -> Vec<(u16, u16)> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let calls = state.lock().unwrap().resize_calls.clone();
        if calls.len() >= expected {
            return calls;
        }
        if tokio::time::Instant::now() >= deadline {
            return calls;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Poll the shared `TrackingState` until `write_input_bufs` reaches
/// `expected` or the timeout elapses. Returns the collected buffers.
pub async fn wait_for_input_writes(
    state: Arc<Mutex<TrackingState>>,
    expected: usize,
) -> Vec<Vec<u8>> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let bufs = state.lock().unwrap().write_input_bufs.clone();
        if bufs.len() >= expected {
            return bufs;
        }
        if tokio::time::Instant::now() >= deadline {
            return bufs;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Poll the shared `TrackingState` for `duration`, asserting that no
/// input buffers are written during that window. Returns the buffers
/// collected so far (expected to be empty).
pub async fn wait_for_no_input_writes(
    state: Arc<Mutex<TrackingState>>,
    duration: Duration,
) -> Vec<Vec<u8>> {
    let deadline = tokio::time::Instant::now() + duration;
    while tokio::time::Instant::now() < deadline {
        let bufs = state.lock().unwrap().write_input_bufs.clone();
        assert!(
            bufs.is_empty(),
            "no PTY input should be written for a matched action"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    state.lock().unwrap().write_input_bufs.clone()
}

// ---------------------------------------------------------------------------
// Server-message helpers (used by server_task integration tests)
// ---------------------------------------------------------------------------

/// Drain all pending `FrameIncremental` messages from
/// `server_rx` using timeout-based recv for reliability.
pub async fn drain_frames(
    server_rx: &mut tokio::sync::mpsc::UnboundedReceiver<ServerMessage>,
    max: usize,
) -> Vec<ServerMessage> {
    let mut frames = Vec::new();
    while frames.len() < max {
        match tokio::time::timeout(Duration::from_millis(200), server_rx.recv()).await {
            Ok(Some(msg)) => frames.push(msg),
            _ => break,
        }
    }
    frames
}

/// Extract focus from the last FrameIncremental in a slice.
pub fn last_frame_focus(frames: &[ServerMessage]) -> Option<usize> {
    frames.iter().rev().find_map(|msg| match msg {
        ServerMessage::FrameIncremental { focus, .. } => Some(*focus),
        _ => None,
    })
}

/// Extract mode from the last FrameIncremental in a slice.
pub fn last_frame_mode(frames: &[ServerMessage]) -> Option<cmdash_keybinds::Mode> {
    frames.iter().rev().find_map(|msg| match msg {
        ServerMessage::FrameIncremental { mode, .. } => Some(*mode),
        _ => None,
    })
}

// ---------------------------------------------------------------------------
// Full-loop orchestrator
// ---------------------------------------------------------------------------

/// Drive a full FrontendTask + ServerTask loop with the supplied
/// keybind router. `send_inputs` is called with the input sender and
/// shared state; it should send the events and then a quit once the
/// condition has been observed. The frontend is run to completion in
/// the test task, so its terminal borrow remains valid.
pub async fn run_full_loop_test<T, F>(
    layout_root: LayoutNode,
    bindings: Router,
    send_inputs: F,
) -> T
where
    F: FnOnce(
        tokio::sync::mpsc::UnboundedSender<Event>,
        Arc<Mutex<TrackingState>>,
    ) -> tokio::task::JoinHandle<T>,
{
    let (mut server, client_tx, server_rx, state) = make_server_with_tracking_pty(layout_root);
    let (input_tx, input_rx) = tokio::sync::mpsc::unbounded_channel::<Event>();

    let server_handle = tokio::spawn(async move { server.run().await });

    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
    let mut frontend = make_frontend_with_router(&mut terminal, client_tx, server_rx, bindings);

    // Spawn the input driver before running the frontend, so events
    // are injected while the frontend loop is active.
    let input_driver = send_inputs(input_tx.clone(), Arc::clone(&state));

    let frontend_result =
        tokio::time::timeout(Duration::from_secs(3), frontend.run_with_input(input_rx)).await;
    let server_result = tokio::time::timeout(Duration::from_secs(3), server_handle).await;

    assert!(
        frontend_result.is_ok(),
        "frontend should exit within timeout"
    );
    assert!(server_result.is_ok(), "server should exit within timeout");
    assert!(
        frontend_result.unwrap().is_ok(),
        "frontend run should succeed"
    );
    assert!(server_result.unwrap().is_ok(), "server run should succeed");

    input_driver.await.unwrap()
}
