//! Per-pane runner: owns a [`cmdash_pty::PanePty`] and a dedicated
//! thread that drains the master-side reader and forwards bytes
//! over a `std::sync::mpsc::Sender<Vec<u8>>` to the binary's main
//! tick loop.
//!
//! AGENTS.md §"Rendering pipeline" step 2 prescribes the cell body
//! path as: PTY bytes → vte → `TextGrid` → ratatui `Frame`. This
//! module is the "PTY bytes → vte" half.
//!
//! ## Why a dedicated reader thread
//!
//! `portable_pty`'s master `try_clone_reader()` blocks on `Read`.
//! If we did that on the main UI thread, a single pane with no
//! pending data would freeze the renderer. One thread per pane
//! keeps reads off the UI thread; the main loop drives `try_recv`.
//!
//! ## Drop + dashcompositor teardown
//!
//! When a [`PaneRunner`] is dropped, it sends its [`PaneLayerId`]
//! into an optional close-channel if the binary registered one
//! via [`PaneRunner::spawn_with_graphics`]. The main loop drains
//! that channel at the start of each tick and calls
//! [`crate::graphics::GraphicsState::close_pane`] for each id.
//! This keeps the dashcompositor layer binding 1:1 with the live
//! pane even on process exit or panic unwinding (AGENTS.md
//! §"Hard rule").
//!
//! ## Why a channel, not Arc<Mutex<>>
//!
//! `dashcompositor::LayerStack` is not `Sync`-derivable through
//! its public API, so wrapping `GraphicsState` in
//! `Arc<Mutex<..>>` triggers `clippy::arc-with-non-send-sync`.
//! A `mpsc::Sender<PaneLayerId>` is `Send`-only (no `Sync`
//! required), trivial for a u64 newtype, and avoids the lock
//! entirely.
//!
//! This module is **sync** (no async runtime). v2 will swap in
//! `tokio` for true non-blocking IO per AGENTS.md §"Key
//! dependencies".

use std::io::Read;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;

use cmdash_layout::ComputedPane;
use cmdash_pty::{PaneLayerId, PanePty, PaneReader, PaneTerminalState, PtyError, ShellSpec};
use thiserror::Error;
use tracing::{debug, warn};

/// Back-channel used by the binary to wire `PaneRunner::Drop`
/// into `GraphicsState::close_pane`. v1 only needs the sender;
/// the receiver is owned by the main loop's `tick_loop`.
pub type PaneCloseTx = Sender<PaneLayerId>;

/// Reader-side error.
#[derive(Debug, Error)]
pub enum RunnerError {
    #[error("pty spawn failed: {0}")]
    Spawn(#[source] PtyError),
}

/// One pane's runtime: [`PanePty`] + reader thread + mpsc receiver.
pub struct PaneRunner {
    /// Source pane description (rect, kind, label).
    pub computed: ComputedPane,
    pty: PanePty,
    bytes_rx: Receiver<Vec<u8>>,
    reader_thread: Option<thread::JoinHandle<()>>,
    /// Optional close-channel sender. When `Some`, `Drop` sends
    /// `self.pty.layer_id()` into it so the main loop's
    /// `GraphicsState` can revoke the pane's dashcompositor
    /// layers on the next tick (AGENTS.md §"Hard rule").
    close_tx: Option<PaneCloseTx>,
}

impl PaneRunner {
    /// Spawn a child PTY and a reader thread that forwards
    /// master bytes into the mpsc receiver. **No** close-channel
    /// hooked up — `Drop` will skip the teardown path. Use
    /// [`PaneRunner::spawn_with_graphics`] from the production
    /// binary so `Drop` notifies the main loop's
    /// `GraphicsState`.
    pub fn spawn(
        computed: ComputedPane,
        layer_id: PaneLayerId,
        shell: ShellSpec,
    ) -> Result<Self, RunnerError> {
        Self::spawn_with_graphics(computed, layer_id, shell, None)
    }

    /// Same as [`PaneRunner::spawn`] but registers an mpsc close
    /// sender. When this runner is dropped, `Drop` enqueues
    /// `self.pty.layer_id()` so a `GraphicsState`-aware main
    /// loop can revoke the pane's dashcompositor layers on the
    /// next tick.
    pub fn spawn_with_graphics(
        computed: ComputedPane,
        layer_id: PaneLayerId,
        shell: ShellSpec,
        close_tx: Option<PaneCloseTx>,
    ) -> Result<Self, RunnerError> {
        let (pty, reader) = PanePty::spawn(shell, computed.rect.w, computed.rect.h, layer_id)
            .map_err(RunnerError::Spawn)?;
        let (tx, rx) = channel::<Vec<u8>>();
        let reader_thread = thread::Builder::new()
            .name(format!("cmdash-pane-{}", layer_id.0))
            .spawn(move || run_reader(reader, tx))
            .expect("spawn reader thread");
        Ok(Self {
            computed,
            pty,
            bytes_rx: rx,
            reader_thread: Some(reader_thread),
            close_tx,
        })
    }

    /// Drain any pending bytes from the reader thread into the
    /// `pty` and return a `PaneTerminalState` snapshot (clone of
    /// the cell grid + events emitted during this tick).
    pub fn tick(&mut self) -> Result<PaneTerminalState, PtyError> {
        // `try_recv`: no blocking. Empty queue is the common case
        // when a child is idle.
        while let Ok(bytes) = self.bytes_rx.try_recv() {
            self.pty.advance(&bytes)?;
        }
        Ok(self.pty.snapshot())
    }

    /// Non-blocking exit poll.
    pub fn try_wait_exit(&mut self) -> Result<Option<i32>, PtyError> {
        self.pty.try_wait()
    }

    /// Resize the PTY.
    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<(), PtyError> {
        self.pty.resize(cols, rows)
    }

    /// Forward input bytes to the child.
    pub fn write_input(&mut self, bytes: &[u8]) -> Result<usize, PtyError> {
        self.pty.write(bytes)
    }

    pub fn layer_id(&self) -> PaneLayerId {
        self.pty.layer_id()
    }
}

impl Drop for PaneRunner {
    fn drop(&mut self) {
        // Best-effort: kill the child before joining the reader so
        // the reader sees EOF promptly instead of an indefinite
        // hang. The child is reachable via `&mut self.pty` since
        // `self.pty` is still in scope.
        if let Err(e) = self.pty.kill() {
            debug!(error = ?e, layer_id = ?self.pty.layer_id(), "kill on drop");
        }
        if let Some(handle) = self.reader_thread.take() {
            let _ = handle.join();
        }
        // AGENTS.md §"Hard rule: one layer per instance" -- the
        // PaneLayerId binding ends at pane close. Notify the main
        // loop so its next tick can call
        // `GraphicsState::close_pane`. We swallow a closed-channel
        // error (binary already exited) which is benign.
        if let Some(tx) = self.close_tx.as_ref() {
            if let Err(e) = tx.send(self.pty.layer_id()) {
                debug!(error = ?e, layer_id = ?self.pty.layer_id(),
                       "close_tx send on drop failed (receiver gone?)");
            }
        }
    }
}

fn run_reader(mut reader: PaneReader, tx: std::sync::mpsc::Sender<Vec<u8>>) {
    let mut buf = [0u8; 4096];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break, // EOF; child closed the master.
            Ok(n) => {
                if tx.send(buf[..n].to_vec()).is_err() {
                    break; // Receiver dropped; binary is exiting.
                }
            }
            Err(e) => {
                warn!(error = %e, "pane reader error; stopping");
                break;
            }
        }
    }
}
