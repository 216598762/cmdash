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

use cmdash_layout::{ComputedPane, Rect as LayoutRect};
use cmdash_pty::{PaneLayerId, PanePty, PaneReader, PaneTerminalState, PtyError, ShellSpec};

/// Re-export the pty-operations trait so downstream callers
/// (e.g., future external test harnesses or alternative backend
/// integrations) can refer to it as `cmdash::pane::PanePtyOps`
/// rather than reaching into `cmdash_pty::PanePtyOps` directly.
/// Also brings `PanePtyOps` into local scope, so the `use`
/// block above intentionally does NOT list it (keeps the
/// single-source-of-truth pattern and avoids the redundant-
/// import E0252 trap).
pub use cmdash_pty::PanePtyOps;
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

/// One pane's runtime: a [`PanePtyOps`] trait object + reader
/// thread + `mpsc` receiver. v1 uses [`cmdash_pty::PanePty`] as
/// the production impl behind the trait (boxed at the call site
/// in [`PaneRunner::spawn_with_graphics`]); tests substitute a
///`StubPty` (see `internal_sanity_tests` below) to pin
/// invariants like the resize Err path that real-PTY tests
/// can't reach deterministically.
///
/// ## Manual `Clone` impl (cycle-22 atom-4)
///
/// `PaneRunner` is a runtime resource (PTY child + dashcompositor
/// layer + reader thread); the trait object field
/// `pty: Box<dyn PanePtyOps + Send>` is not `Clone` by default.
/// The additive `TabStack<TabState>` integration in
/// `cmdash::main` needs `TabState: Clone` (the upstream
/// `Tab<T>: Clone` derive in `crate::tabs` requires it), and
/// `TabState` carries a `runners: Vec<PaneRunner>` field. The
/// manual `Clone` impl below returns a "shell" with `pty: None`
/// and `reader_thread: None` so the v1 field's runners (the real
/// ones) stay intact while the `TabState`'s runners are
/// decorative shells — `TabState.runners` is never used at
/// runtime (the v1 field's runners are authoritative for v1
/// code paths, and tab mutations always go through
/// `reconcile_runners` which spawns fresh real runners).
pub struct PaneRunner {
    /// Source pane description (rect, kind, label).
    computed: ComputedPane,
    /// Trait-object PTY backend. Stored as `Option<Box<...>>` so
    /// the manual `Clone` impl can return a shell with `pty: None`
    /// (the source keeps its pty; the clone has no runtime
    /// backend). Production runners always have `pty: Some(_)`
    /// after `spawn_with_graphics`; `None` is the clone-shell case.
    pty: Option<Box<dyn PanePtyOps + Send>>,
    bytes_rx: Receiver<Vec<u8>>,
    reader_thread: Option<thread::JoinHandle<()>>,
    /// Optional close-channel sender. When `Some`, `Drop` sends
    /// `self.pty.layer_id()` into it so the main loop's
    /// `GraphicsState` can revoke the pane's dashcompositor
    /// layers on the next tick (AGENTS.md §"Hard rule").
    close_tx: Option<PaneCloseTx>,
}

// Manual `Clone` impl: the trait object field `pty: Box<dyn ...>`
// is not `Clone` by default. The clone is a "shell" with
// `pty: None` + `reader_thread: None` -- the source keeps both
// pty and reader thread, so the v1 field's `runners` Vec
// (the authoritative real runners for v1 code paths) stays
// intact after `runners.clone()` for the `TabState` mirror.
// The clone's `bytes_rx` is a FRESH dummy channel (the
// original is `std::sync::mpsc::Receiver` which is NOT
// `Clone`); the dummy's sender is dropped on creation, so
// the clone's `tick()` will see a disconnected channel and
// skip the drain loop. The clone is a decorative shell for
// `TabState.runners` mirroring only -- it is NEVER used at
// runtime (any call into a clone's `tick()` / `resize()` /
// `write_input()` would panic on the `pty: None` `.expect`).
// `close_tx.clone()` IS valid (Sender: Clone).
impl Clone for PaneRunner {
    fn clone(&self) -> Self {
        let (_tx, bytes_rx) = channel::<Vec<u8>>();
        Self {
            computed: self.computed.clone(),
            pty: None,
            bytes_rx,
            reader_thread: None,
            close_tx: self.close_tx.clone(),
        }
    }
}

// Manual `Debug` impl: the trait object field
// `pty: Option<Box<dyn PanePtyOps + Send>>` is not `Debug`-able
// (the trait does not require `Debug`). The format_args
// sentinel prints `<dyn PanePtyOps+Send or <empty>>` so the
// presence/absence of the pty is observable without forcing
// the trait to require `Debug` (which would cascade into
// `MasterPty: Debug` and the test stub). `bytes_rx` /
// `reader_thread` / `close_tx` are opaque runtime resources
// -- they print as `<rx>` / `<thread handle or None>` /
// `<tx or None>`. `computed` is a plain [`ComputedPane`] so
// its `Debug` delegate runs normally.
impl std::fmt::Debug for PaneRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PaneRunner")
            .field("computed", &self.computed)
            .field(
                "pty",
                &format_args!(
                    "{}",
                    if self.pty.is_some() {
                        "<dyn PanePtyOps+Send>"
                    } else {
                        "<empty>"
                    },
                ),
            )
            .field("bytes_rx", &format_args!("<rx>"))
            .field(
                "reader_thread",
                &format_args!(
                    "{}",
                    if self.reader_thread.is_some() {
                        "<thread handle>"
                    } else {
                        "<None>"
                    },
                ),
            )
            .field(
                "close_tx",
                &format_args!(
                    "{}",
                    if self.close_tx.is_some() {
                        "<tx>"
                    } else {
                        "<None>"
                    },
                ),
            )
            .finish()
    }
}

// Manual `PartialEq` + `Eq` impls: two `PaneRunner`s are
// "equal" iff they reference the same LOGICAL pane
// (i.e. share a [`cmdash_layout::PaneId`]). The
// `pty` / `bytes_rx` / `reader_thread` / `close_tx` fields
// are transient RUNTIME state (a clone-shell has `pty: None`
// while the source has `pty: Some(_)`, but they're the same
// logical pane -- comparing the source against its clone
// returns `true`). This is the only viable semantics for a
// type with a `Box<dyn PanePtyOps + Send>` field (trait
// objects are not `PartialEq`); the `computed.id` is the
// stable identity from the layout engine's pre-order leaf
// numbering (AGENTS.md §"`PaneId` stability"). The
// `TabState` derive in `cmdash::main` needs
// `Vec<PaneRunner>: PartialEq + Eq` so the tab mirror can
// participate in `assert_eq!` / `Tab<T>: PartialEq` chains.
impl PartialEq for PaneRunner {
    fn eq(&self, other: &Self) -> bool {
        self.computed.id == other.computed.id
    }
}

impl Eq for PaneRunner {}

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
            pty: Some(Box::new(pty)),
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
        let pty = self
            .pty
            .as_mut()
            .expect("PaneRunner::tick: pty is None (clone-shell called at runtime?)");
        while let Ok(bytes) = self.bytes_rx.try_recv() {
            pty.advance(&bytes)?;
        }
        Ok(pty.snapshot())
    }

    /// Non-blocking exit poll.
    pub fn try_wait_exit(&mut self) -> Result<Option<i32>, PtyError> {
        self.pty
            .as_mut()
            .expect("PaneRunner::try_wait_exit: pty is None")
            .try_wait()
    }

    /// Resize the PTY and overwrite the cached [`ComputedPane`]
    /// rect with the layout-engine-supplied cell-grid `<rect>`.
    /// Callers reading `runner.computed().rect` after a resize
    /// see the new pane geometry — dims AND origin — instead of
    /// the spawn-time rect.
    ///
    /// v2 contract: the `(x, y)` from the layout engine is
    /// preserved across resize. A `Split`'s second child sits at
    /// `x = layout_w * ratio` (or a similar non-zero origin);
    /// the blit path in `TickContext::run` reads
    /// `runner.computed().rect.x/.y` straight into a
    /// `ratatui::layout::Rect`, so a resize that zeroed the
    /// origin would silently misplace the pane in a Split
    /// layout. Order is load-bearing: `pty.resize` propagates
    /// any [`PtyError`] via `?` BEFORE the rect write, so a
    /// failed pty resize never refreshes the rect (the cached
    /// value keeps the previous last-good state — pane state
    /// mutates atomically, not in halves).
    pub fn resize(&mut self, rect: LayoutRect) -> Result<(), PtyError> {
        self.pty
            .as_mut()
            .expect("PaneRunner::resize: pty is None")
            .resize(rect.w, rect.h)?;
        self.computed.rect = rect;
        Ok(())
    }

    /// Forward input bytes to the child.
    pub fn write_input(&mut self, bytes: &[u8]) -> Result<usize, PtyError> {
        self.pty
            .as_mut()
            .expect("PaneRunner::write_input: pty is None")
            .write(bytes)
    }

    /// Read-only accessor; transparent pass-through to the spawn-
    /// time `computed` field. The AGENTS.md "Hard rule: one layer
    /// per instance" invariant is enforced at construction
    /// (`spawn_with_graphics` takes `(computed, layer_id)` as
    /// paired args), not here. Kept narrow so any future read-time
    /// check can be added without churning call sites.
    pub fn computed(&self) -> &ComputedPane {
        &self.computed
    }

    /// Refresh the cached [`ComputedPane`] (id, rect, label, kind)
    /// without touching the underlying PTY child. Used by the
    /// runtime mutation paths (`AppNewPane`, `PaneClose`, `PanePreset`
    /// reconciliation) to align a survivor runner with the
    /// post-mutation layout tree resolution. The
    /// [`cmdash_pty::PaneLayerId`] is implicit on the PTY child
    /// and stays stable across the rebind, per AGENTS.md §"Hard
    /// rule: one layer per instance" (`LayerId` is bound to a pane
    /// instance for the instance's whole lifetime; it is NEVER
    /// re-bound to a different pane).
    ///
    /// Pair with [`Self::resize`] if the underlying PTY child
    /// also needs to match the new rect (e.g. after a tree
    /// mutation has shifted proportions). The orchestrator in
    ///`TickContext::reconcile_runners`
    /// pairs the two so a survivor's PTY child AND cached
    /// computed reflect the new layout at the end of one tick.
    ///
    /// The new pane's `id` MUST come from a fresh
    /// [`cmdash_layout::ComputedLayout::compute`] call against
    /// the post-mutation tree; reusing a stale `pane.id` would
    /// re-introduce the broken hero-pane-id-rotates pairing
    /// invariant that the `relayout_drives_per_pane_resize_via_real_pty`
    /// regression catches.
    pub fn rebind_pane(&mut self, pane: ComputedPane) {
        self.computed = pane;
    }

    pub fn layer_id(&self) -> PaneLayerId {
        self.pty
            .as_ref()
            .expect("PaneRunner::layer_id: pty is None")
            .layer_id()
    }

    /// Test-only ctor that injects a [`PanePtyOps`] trait object
    /// WITHOUT spawning a real PTY reader thread. Used by the
    /// `#[cfg(test)] mod internal_sanity_tests` block below to
    /// pin resize-ordering invariants that real-PTY tests can't
    /// reach deterministically. Production paths still go
    /// through [`PaneRunner::spawn_with_graphics`].
    ///
    /// Lives inside [`impl PaneRunner`] (not as a free fn) so the
    /// test call site `PaneRunner::with_pty_for_test(...)`
    /// resolves via the standard associated-fn syntax.
    #[cfg(test)]
    pub(crate) fn with_pty_for_test(
        computed: ComputedPane,
        // `layer_id` is unused here: the StubPty passed in
        // encapsulates its own `PaneLayerId` internally. Kept in
        // the signature (parallel to `spawn_with_graphics`) so
        // test invocations pattern-match the production-ctor shape.
        #[allow(unused)] layer_id: PaneLayerId,
        pty: Box<dyn PanePtyOps + Send>,
        close_tx: Option<PaneCloseTx>,
    ) -> Self {
        // No real reader thread: tests that exercise this ctor
        // only care about `resize` / `computed().rect` invariants
        // and don't need byte feeding. Integration paths still go
        // through `spawn_with_graphics` (with a real PanePty +
        // thread).
        let (_tx, rx) = channel::<Vec<u8>>();
        Self {
            computed,
            pty: Some(pty),
            bytes_rx: rx,
            reader_thread: None,
            close_tx,
        }
    }
}

impl Drop for PaneRunner {
    fn drop(&mut self) {
        // Best-effort: kill the child before joining the reader so
        // the reader sees EOF promptly instead of an indefinite
        // hang. The child is reachable via `&mut self.pty` since
        // `self.pty` is still in scope. `Option::take()` so a
        // clone-shell (`pty: None`) is a no-op (the source's
        // pty is unaffected; the clone has no pty to kill).
        if let Some(pty) = self.pty.as_mut() {
            if let Err(e) = pty.kill() {
                debug!(error = ?e, layer_id = ?pty.layer_id(), "kill on drop");
            }
        }
        if let Some(handle) = self.reader_thread.take() {
            let _ = handle.join();
        }
        // AGENTS.md §"Hard rule: one layer per instance" -- the
        // PaneLayerId binding ends at pane close. Notify the main
        // loop so its next tick can call
        // `GraphicsState::close_pane`. We swallow a closed-channel
        // error (binary already exited) which is benign.
        // Skipped for clone-shells (pty: None) since the source
        // runner's `Drop` is the authoritative emitter of the
        // close-channel message.
        if let (Some(tx), Some(pty)) = (self.close_tx.as_ref(), self.pty.as_ref()) {
            if let Err(e) = tx.send(pty.layer_id()) {
                debug!(error = ?e, layer_id = ?pty.layer_id(),
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

// ---------------------------------------------------------------------------
// Internal sanity tests. The `StubPty` here implements
// [`PanePtyOps`] so we can pin the resize-ordering invariant
// (`pty.resize()?` BEFORE `self.computed.rect = ...`) without
// depending on a real portable_pty child, which would be
// non-deterministic for the failure path. Mirrors the existing
// pattern in `cmdash::graphics::internal_sanity_tests`.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod internal_sanity_tests {
    use super::*;
    use cmdash_pty::TextGrid;

    /// Stub whose `resize` returns the queued error on the next
    /// call (consuming the slot) and `Ok(())` thereafter. All
    /// other methods hand back minimal valid-shape defaults; the
    /// resize-path tests don't invoke them.
    struct StubPty {
        layer_id: PaneLayerId,
        next_resize_result: Option<PtyError>,
    }

    impl StubPty {
        fn new(layer_id: PaneLayerId) -> Self {
            Self {
                layer_id,
                next_resize_result: None,
            }
        }
        fn fail_next_resize(&mut self, err: PtyError) {
            self.next_resize_result = Some(err);
        }
    }

    impl PanePtyOps for StubPty {
        fn layer_id(&self) -> PaneLayerId {
            self.layer_id
        }
        fn resize(&mut self, _cols: u16, _rows: u16) -> Result<(), PtyError> {
            if let Some(err) = self.next_resize_result.take() {
                Err(err)
            } else {
                Ok(())
            }
        }
        fn write(&mut self, _bytes: &[u8]) -> Result<usize, PtyError> {
            Ok(0)
        }
        fn advance(&mut self, _bytes: &[u8]) -> Result<(), PtyError> {
            Ok(())
        }
        fn snapshot(&mut self) -> PaneTerminalState {
            PaneTerminalState {
                grid: TextGrid::new(0, 0),
                cols: 0,
                rows: 0,
                pending_events: Vec::new(),
            }
        }
        fn try_wait(&mut self) -> Result<Option<i32>, PtyError> {
            Ok(None)
        }
        fn kill(&mut self) -> Result<(), PtyError> {
            Ok(())
        }
    }

    /// Build a [`ComputedPane`] fixture for the unit tests by
    /// routing through the same `cmdash_config` + `ComputedLayout`
    /// path that the integration tests use, so the test exercises
    /// the real leaf-pane shape (id, kind, label) -- not a
    /// hand-crafted [`ComputedPane`] with private-field access.
    fn make_test_pane() -> ComputedPane {
        use cmdash_config::parse as parse_config;
        use cmdash_layout::ComputedLayout;
        let cfg_text = r#"layout { pane kind=shell label="resize-stub-test" }"#;
        let cfg = parse_config(cfg_text).expect("parse config");
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

    /// Regression test pinning reviewer nit (G): a failed
    /// `pty.resize` MUST NOT mutate `self.computed.rect`. The
    /// ordering `self.pty.resize(...)?` BEFORE the rect rewrite
    /// is on-sight and un-unit-tested until the trait extraction
    /// in commit `0102ae4` unlocked a stub backend. AGENTS.md
    /// §"every invariant needs a regression test" demands this.
    #[test]
    fn resize_failure_leaves_rect_unchanged_and_propagates_err() {
        let computed = make_test_pane();
        let pre_rect = computed.rect;
        let mut stub = StubPty::new(PaneLayerId(1));
        stub.fail_next_resize(PtyError::InvalidSize(132, 0));
        let mut runner =
            PaneRunner::with_pty_for_test(computed, PaneLayerId(1), Box::new(stub), None);
        let result = runner.resize(LayoutRect {
            x: 0,
            y: 0,
            w: 132,
            h: 50,
        });
        assert!(
            matches!(result, Err(PtyError::InvalidSize(132, 0))),
            "resize must propagate PtyError::InvalidSize unchanged; got {:?}",
            result
        );
        assert_eq!(
            runner.computed().rect,
            pre_rect,
            "resize failure must leave self.computed.rect unchanged"
        );
    }

    /// Symmetric success-path pin: when `pty.resize` returns
    /// `Ok(())`, `self.computed.rect` MUST be overwritten with
    /// the FULL caller-supplied `<rect>` — dims AND origin.
    /// Pins the v2 split-pane contract: the layout-engine's
    /// `(x, y)` carry forward into the cached rect, so a
    /// `Split`'s second child stays at its layout-derived `x`
    /// offset after resize. A v1 regression that zeroed the
    /// origin would silently misplace the pane in phase 3a.
    ///
    /// The pre-state is locked to `(48, 0, 32, 24)` via a
    /// `SplitAxis::Horizontal ratio=0.6` layout fixture over
    /// `(80, 24)` parent area — that's the second child's
    /// computed origin per [`cmdash_layout::split_rect`]. The
    /// target rect is `(48, 0, 132, 50)`: pivots from the
    /// Split-derived origin `x:48` to a size-grew input. A
    /// `x:0` regression would fail the assert below.
    #[test]
    fn resize_success_overwrites_full_rect_preserving_origin() {
        use cmdash_config::parse as parse_config;
        use cmdash_layout::ComputedLayout;
        let cfg_text = r#"layout {
            split axis=horizontal ratio=0.6 {
                pane kind=shell label="split-a"
                pane kind=shell label="split-b"
            }
        }"#;
        let cfg = parse_config(cfg_text).expect("parse split config");
        let root = cfg.layout.expect("layout block");
        let parent = LayoutRect {
            x: 0,
            y: 0,
            w: 80,
            h: 24,
        };
        let layout = ComputedLayout::compute(&root, parent).expect("compute split layout");
        // Second child carries the non-zero origin.
        let computed_b = layout.panes[1].clone();
        let pre_rect = computed_b.rect;
        assert_eq!(
            pre_rect,
            LayoutRect {
                x: 48,
                y: 0,
                w: 32,
                h: 24
            },
            "fixture invariant: Split's second child sits at (x:48, y:0, w:32, h:24)"
        );
        let stub = StubPty::new(PaneLayerId(2));
        let mut runner =
            PaneRunner::with_pty_for_test(computed_b, PaneLayerId(2), Box::new(stub), None);
        // Carry the layout-derived origin forward; also grow dims.
        let target = LayoutRect {
            x: 48,
            y: 0,
            w: 132,
            h: 50,
        };
        let result = runner.resize(target);
        assert!(matches!(result, Ok(())));
        assert_eq!(
            runner.computed().rect,
            target,
            "resize success must overwrite self.computed.rect with the caller-supplied full rect"
        );
    }
}
