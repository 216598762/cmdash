//! Test-only PTY stub that records operations for unit/integration tests.
//!
//! `TrackingPty` implements [`cmdash_pty::PanePtyOps`] and writes every
//! interesting operation into a shared [`TrackingState`] protected by a
//! mutex. This lets tests verify that `ServerTask`, `FrontendTask`, and
//! `PaneRunner` forward resize/input/scrollback events correctly without
//! spawning real shells.

use std::sync::{Arc, Mutex};

use cmdash_pty::{PaneLayerId, PanePtyOps, PaneTerminalState, PtyError, TextGrid};

/// Shared recorder for stub PTY operations.
#[derive(Default, Debug, Clone)]
pub(crate) struct TrackingState {
    pub resize_calls: Vec<(u16, u16)>,
    pub write_input_bufs: Vec<Vec<u8>>,
    /// Per-pane write events, recorded as `(layer_id, bytes)`.
    pub write_input_events: Vec<(PaneLayerId, Vec<u8>)>,
    pub scrollback_up_calls: Vec<usize>,
    pub scrollback_down_calls: Vec<usize>,
    pub scrollback_reset_count: usize,
    pub in_scrollback: bool,
    pub in_alternate_screen: bool,
    pub focus_reporting: bool,
}

/// Test-only PTY that records operations to [`TrackingState`].
pub(crate) struct TrackingPty {
    pub layer_id: PaneLayerId,
    pub state: Arc<Mutex<TrackingState>>,
}

impl PanePtyOps for TrackingPty {
    fn layer_id(&self) -> PaneLayerId {
        self.layer_id
    }

    fn resize(&mut self, cols: u16, rows: u16) -> Result<(), PtyError> {
        self.state.lock().unwrap().resize_calls.push((cols, rows));
        Ok(())
    }

    fn write(&mut self, bytes: &[u8]) -> Result<usize, PtyError> {
        let mut state = self.state.lock().unwrap();
        state.write_input_bufs.push(bytes.to_vec());
        state
            .write_input_events
            .push((self.layer_id, bytes.to_vec()));
        Ok(bytes.len())
    }

    fn advance(&mut self, _bytes: &[u8]) -> Result<(), PtyError> {
        Ok(())
    }

    fn snapshot(&mut self) -> PaneTerminalState {
        PaneTerminalState {
            grid: TextGrid::new(80, 24),
            cols: 80,
            rows: 24,
            pending_events: Vec::new(),
        }
    }

    fn try_wait(&mut self) -> Result<Option<i32>, PtyError> {
        Ok(None)
    }

    fn kill(&mut self) -> Result<(), PtyError> {
        Ok(())
    }

    fn keyboard_flags(&self) -> u8 {
        0
    }

    fn focus_reporting_enabled(&self) -> bool {
        self.state.lock().unwrap().focus_reporting
    }

    fn scrollback_up(&mut self, n: usize) {
        self.state.lock().unwrap().scrollback_up_calls.push(n);
    }

    fn scrollback_down(&mut self, n: usize) {
        self.state.lock().unwrap().scrollback_down_calls.push(n);
    }

    fn scrollback_reset(&mut self) {
        self.state.lock().unwrap().scrollback_reset_count += 1;
    }

    fn in_scrollback(&self) -> bool {
        self.state.lock().unwrap().in_scrollback
    }

    fn in_alternate_screen(&self) -> bool {
        self.state.lock().unwrap().in_alternate_screen
    }
}
