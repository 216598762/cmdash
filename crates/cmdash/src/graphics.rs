//! cmdash-side dashcompositor adapter for kitty graphics coming
//! from nested PTY children.
//!
//! ## Design
//!
//! - One [`GraphicsState`] owns the command's [`dashcompositor::LayerStack`].
//! - Each pane keeps `(pane_layer_id, kitty_image_id) -> LayerId`
//!   in a flat `HashMap` keyed by a stable
//!   [`cmdash_pty::PaneLayerId`] (1:1 with the pane — AGENTS.md
//!   §"Hard rule: one layer per instance").
//! - `GraphicsState::on_kitty` dispatches each
//!   [`KittyGraphicCmd`] variant:
//!   - `Load`: decode the RGBA payload via [`image::load_from_memory`]
//!     and call `Self::push_image` to register the freshly-pushed
//!     [`dashcompositor::ImageLayer`].
//!   - `Place`: re-create the layer at the new pixel position
//!     while preserving the cached RGBA. (dashcompositor's
//!     [`dashcompositor::Layer`] trait has no `set_position`, so
//!     a remove-then-push is the documented v1 path; the pane-side
//!     [`PaneLayerId`] stays stable across this operation.)
//!   - `Delete`: remove the cached entry and the layer.
//!   - `Control`: no-op (matches vte-via-cmdash-pty semantics).
//! - [`GraphicsState::render_and_write`] composites the stack
//!   into a [`dashcompositor::FrameBuffer`] sized from
//!   [`Metrics`] (default `8x16` per cell) and emits through
//!   `dashcompositor::encode_passthrough_to_writer` (Kitty)
//!   or `dashcompositor::encoder::encode_to_writer` (Sixel
//!   fallback), depending on [`GraphicsProtocol`].
//! - [`GraphicsState::close_pane`] tears down every layer that
//!   came from a given pane (AGENTS.md §"MUST NOT" — bindings
//!   outliving their pane).

use std::collections::HashMap;
use std::io::Write;

use cmdash_pty::{KittyGraphicCmd, PaneLayerId};
use dashcompositor::encoder::encode_to_writer as encode_sixel_to_writer;
use dashcompositor::{
    encode_passthrough_to_writer, Compositor, CpuCompositor, FrameBuffer, ImageLayer, LayerId,
    LayerStack, RectLayer, TextLayer,
};
use thiserror::Error;
use tracing::{info, warn};

/// Cell-pixel metrics used when converting a pane's text rect to
/// the underlying pixel framebuffer size. v1 sticks to the
/// common 8x16 default; per-terminal overrides are a v2 topic.
///
/// Fields are private; construct via [`Self::new`] or
/// [`Default::default`]. The ctor enforces `cell_w > 0 &&
/// cell_h > 0` so [`GraphicsState::render_and_write`] cannot
/// produce a zero-area framebuffer component.
#[derive(Debug, Clone, Copy)]
pub struct Metrics {
    cell_w: u32,
    cell_h: u32,
}

impl Metrics {
    /// Construct [`Metrics`] with non-zero cell dimensions.
    /// `cell_w > 0 && cell_h > 0` is enforced by `assert!`
    /// (matching the ctor invariant on
    /// [`crate::graphics::GraphicsState`]). The exact panic
    /// phrase `"cell_w and cell_h must be non-zero"` is
    /// consumed by the `metrics_new_panics_on_zero_*`
    /// regression tests in
    ///`internal_sanity_tests` .
    ///
    /// Not `const fn` -- no const-eval consumer exists today
    /// (`Default::default()` is `fn`, not `const fn`;
    /// [`crate::graphics::GraphicsState::new`] takes `Metrics`
    /// by value in a non-const context), and dropping `const`
    /// lets the panic phrase stay stable for debug-time
    /// correlation.
    pub fn new(cell_w: u32, cell_h: u32) -> Self {
        assert!(
            cell_w > 0 && cell_h > 0,
            "Metrics::new: cell_w and cell_h must be non-zero, got {}x{}",
            cell_w,
            cell_h,
        );
        Self { cell_w, cell_h }
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new(8, 16)
    }
}

#[derive(Debug, Error)]
pub enum GraphicsError {
    #[error("image decode failed: {0}")]
    ImageDecode(#[from] image::ImageError),
    #[error("graphics dispatch failed: {0}")]
    Dispatch(String),
}

/// Per-(pane, kitty-image-id) bookkeeping: one dashcompositor
/// layer slot plus the cached RGBA so that `Place` commands can
/// rebuild an [`ImageLayer`] without re-decoding the payload.
#[derive(Debug, Clone)]
struct ImageEntry {
    layer_id: LayerId,
    rgba: image::RgbaImage,
}

/// Z-order base for tab bar layers. Background sits at
/// `TAB_BAR_Z_BASE`, per-tab highlights at `+1`, text at
/// `+2`. High enough to sit above pane image layers (which
/// use z-order 0 by default).
const TAB_BAR_Z_BASE: u32 = 1000;

/// Detected graphics protocol for the host terminal.
/// Determined at startup from `TERM` env var and device
/// attributes. Drives the encoder selection in
/// [`GraphicsState::render_and_write`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphicsProtocol {
    /// Kitty graphics protocol (preferred). Detected when
    /// `TERM=kitty` or `TERM=xterm-kitty`.
    Kitty,
    /// Sixel graphics fallback. Detected when `TERM` contains
    /// `sixel` or the terminal is known Sixel-capable (`mlterm`,
    /// `xterm` with Sixel support, `foot`, `wezterm`).
    Sixel,
    /// No graphics protocol detected. Text-only mode —
    /// `render_and_write` is a no-op.
    TextOnly,
}

impl GraphicsProtocol {
    /// Detect the graphics protocol from the `TERM` env var.
    /// Returns the best available protocol based on terminal
    /// identification.
    ///
    /// Detection priority:
    /// 1. `TERM=kitty` or `TERM=xterm-kitty` → Kitty
    /// 2. `TERM` contains `sixel` → Sixel
    /// 3. Known Sixel-capable terminals (`mlterm`, `foot`,
    ///    `wezterm`) → Sixel
    /// 4. `CMDASH_GRAPHICS` env override (`kitty`, `sixel`,
    ///    `none`) — explicit user choice wins
    /// 5. Otherwise → `TextOnly`
    pub fn detect() -> Self {
        // Explicit override takes priority.
        if let Ok(val) = std::env::var("CMDASH_GRAPHICS") {
            match val.to_lowercase().as_str() {
                "kitty" => return Self::Kitty,
                "sixel" => return Self::Sixel,
                "none" | "text" | "off" => return Self::TextOnly,
                _ => {}
            }
        }
        let term = std::env::var("TERM").unwrap_or_default();
        let term_program = std::env::var("TERM_PROGRAM").unwrap_or_default();
        // Kitty-native terminals.
        if term == "kitty" || term == "xterm-kitty" || term_program == "kitty" {
            return Self::Kitty;
        }
        // Known Sixel-capable terminals.
        if term.contains("sixel")
            || term_program == "mlterm"
            || term_program == "foot"
            || term_program == "WezTerm"
            || term == "mlterm"
            || term == "foot"
        {
            return Self::Sixel;
        }
        Self::TextOnly
    }

    /// Human-readable protocol name for logging.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Kitty => "kitty",
            Self::Sixel => "sixel",
            Self::TextOnly => "text-only",
        }
    }

    /// Send a DEC VT220 Primary Device Attributes (DA1) query
    /// (`ESC[c`) to the terminal and parse the response to detect
    /// graphics capabilities. Returns `Some(Sixel)` if the
    /// response contains attribute 4 (Sixel support), `None`
    /// on timeout or malformed response.
    ///
    /// This is called at startup (after raw mode is enabled) only
    /// when env-var detection yielded `TextOnly`, as a runtime
    /// fallback. The `CMDASH_GRAPHICS` override and `TERM`/`TERM_PROGRAM`
    /// checks always take priority.
    ///
    /// **Timeout:** If the terminal does not respond within `timeout`
    /// (typically 100ms), returns `None` — this handles piped output,
    /// non-interactive sessions, and terminals that don't support DA1.
    pub fn query_device_attributes(timeout: std::time::Duration) -> Option<Self> {
        use std::fs::File;
        use std::io::{IsTerminal, Read};
        use std::mem::ManuallyDrop;
        use std::os::raw::{c_int, c_short, c_ulong};
        use std::os::unix::io::FromRawFd;
        use std::time::Instant;

        // Skip in CI, piped environments, or non-TTY stdin.
        // Avoids the 100ms timeout penalty and prevents the
        // DA1 query from interfering with test keystrokes.
        if !std::io::stdin().is_terminal() {
            return None;
        }

        // libc poll(2) binding — avoids adding `libc` crate.
        #[repr(C)]
        struct PollFd {
            fd: c_int,
            events: c_short,
            revents: c_short,
        }
        const POLLIN: c_short = 1;
        extern "C" {
            fn poll(fds: *mut PollFd, nfds: c_ulong, timeout: c_int) -> c_int;
        }

        // Send DA1 query: ESC[c
        let mut stdout = std::io::stdout();
        if stdout.write_all(b"\x1b[c").is_err() || stdout.flush().is_err() {
            return None;
        }

        // Poll stdin fd 0 for readability, then read only when
        // data is available. This avoids a background thread
        // entirely — no stray bytes consumed on timeout, no
        // stdin lock contention with crossterm.
        //
        // Uses `ManuallyDrop<File::from_raw_fd(0)>` to bypass
        // the global `Stdin` mutex. `ManuallyDrop` prevents
        // `Drop` from closing fd 0.
        let mut pfd = PollFd {
            fd: 0,
            events: POLLIN,
            revents: 0,
        };
        let mut stdin_fd = ManuallyDrop::new(unsafe { File::from_raw_fd(0) });
        let end_time = Instant::now() + timeout;
        let mut acc = Vec::with_capacity(64);

        loop {
            let remaining_ms = end_time
                .saturating_duration_since(Instant::now())
                .as_millis();
            if remaining_ms == 0 {
                break;
            }
            pfd.revents = 0;
            let res = unsafe { poll(&mut pfd, 1, remaining_ms as c_int) };
            if res < 0 {
                // EINTR (signal interruption) — retry; any other
                // error — give up.
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                break;
            }
            if res == 0 {
                break; // timeout
            }
            // poll confirmed data is available — read will not block.
            let mut buf = [0u8; 1];
            match stdin_fd.read(&mut buf) {
                Ok(1) => {
                    acc.push(buf[0]);
                    if buf[0] == b'c' {
                        break; // DA1 response terminator
                    }
                }
                _ => break, // EOF or error
            }
        }

        parse_da1_response(&acc)
    }

    /// Parse a protocol override string (as from `CMDASH_GRAPHICS`).
    /// Used by tests to exercise detection logic without
    /// manipulating environment variables. Returns `TextOnly`
    /// for unrecognized values.
    #[cfg(test)]
    pub(crate) fn detect_from_override(val: &str) -> Self {
        match val.to_lowercase().as_str() {
            "kitty" => Self::Kitty,
            "sixel" => Self::Sixel,
            "none" | "text" | "off" => Self::TextOnly,
            _ => Self::TextOnly,
        }
    }
}

impl Default for GraphicsProtocol {
    fn default() -> Self {
        Self::detect()
    }
}
/// Parse a DEC VT220 Primary Device Attributes (DA1) response.
/// The response has the form `ESC [ ? {params} c` where params
/// are semicolon-separated integers. Returns `Some(Sixel)` if
/// param 4 is present (Sixel graphics support). Kitty is NOT
/// detected via DA1 — it uses `TERM`/`TERM_PROGRAM` env vars
/// in [`GraphicsProtocol::detect()`] instead.
///
/// This is a pure function (no I/O) so it can be exhaustively
/// unit-tested in CI without a real terminal.
pub(crate) fn parse_da1_response(bytes: &[u8]) -> Option<GraphicsProtocol> {
    // DA1 response: ESC [ ? <params> c
    // Find the ESC[? prefix.
    let mut i = 0;
    // Skip to ESC (0x1b).
    while i < bytes.len() && bytes[i] != 0x1b {
        i += 1;
    }
    if i + 2 >= bytes.len() {
        return None;
    }
    if bytes[i + 1] != b'[' || bytes[i + 2] != b'?' {
        return None;
    }
    i += 3; // past ESC[?
            // Read params until 'c' (0x63).
    let mut has_sixel = false;
    let mut current_num: u32 = 0;
    let mut reading_num = false;
    while i < bytes.len() {
        match bytes[i] {
            b'0'..=b'9' => {
                reading_num = true;
                current_num = current_num.wrapping_mul(10) + (bytes[i] - b'0') as u32;
            }
            b';' => {
                if reading_num {
                    if current_num == 4 {
                        has_sixel = true;
                    }
                    current_num = 0;
                    reading_num = false;
                }
            }
            b'c' => {
                // Final parameter (no trailing semicolon).
                if reading_num && current_num == 4 {
                    has_sixel = true;
                }
                break;
            }
            _ => {} // ignore unexpected bytes
        }
        i += 1;
    }
    if has_sixel {
        Some(GraphicsProtocol::Sixel)
    } else {
        // Valid DA1 response but no graphics capability reported.
        None
    }
}

/// Tab bar colors as dashcompositor `[u8; 4]` RGBA quads.
/// Match the ratatui text-mode colors from [`render_tab_bar`]
/// so the pixel overlay is visually consistent with the
/// degraded text fallback.
const TAB_BAR_BG: [u8; 4] = [60, 60, 60, 255]; // DarkGray
const TAB_BAR_ACTIVE_BG: [u8; 4] = [0, 0, 200, 255]; // Blue
const TAB_BAR_ACTIVE_FG: [u8; 4] = [255, 255, 255, 255]; // White
const TAB_BAR_INACTIVE_BG: [u8; 4] = [60, 60, 60, 255]; // DarkGray
const TAB_BAR_INACTIVE_FG: [u8; 4] = [160, 160, 160, 255]; // Gray

/// Snapshot of tab bar state passed to
/// [`GraphicsState::update_tab_bar`] each frame. Rebuilt from
/// [`cmdash::TabStack`] at the call site so `GraphicsState`
/// doesn't borrow the full `TabStack`.
pub struct TabBarData<'a> {
    /// Per-tab labels (`None` for tabs without a label).
    pub labels: Vec<Option<&'a str>>,
    /// Index of the currently-active tab.
    pub active_idx: usize,
    /// Total tab bar width in cells (terminal columns).
    pub bar_width_cells: u16,
}

/// Per-pane graphics state. Holds a shared
/// [`dashcompositor::LayerStack`], per-pane image maps, and the
/// cell-pixel metrics used for framebuffer sizing.
pub struct GraphicsState {
    /// dashcompositor layer stack -- private; mutating is exposed
    /// through `push_image` / `close_pane` / `render_and_write`.
    stack: LayerStack,
    /// Cell-pixel metrics for framebuffer sizing -- private;
    /// passed in via `Self::new` and read inside `render_and_write`.
    metrics: Metrics,
    /// Total terminal size in cells; private. Set once in
    /// `Self::new`, which enforces `cells.0 > 0 && cells.1 > 0`
    /// via `assert!` so a downstream `render_and_write` cannot
    /// produce a zero-size framebuffer. v1 has a single tab with
    /// one root layout rect, so resizing isn't a `set_cells`
    /// path -- constructing a fresh `GraphicsState` is the v1
    /// contract; v2 may add `pub fn set_cells` with the same
    /// assert guard.
    cells: (u16, u16),
    /// Bookkeeping for per-(pane, `kitty_image_id`) layers.
    /// Invariant: for every `pane`, every `kitty_id` recorded in
    /// `pane_images[pane]` is also the second component of a key
    /// in `images`. Maintained by `push_image`, the `on_kitty`
    /// `Delete` path, and `close_pane`. The cross-key invariant
    /// is locked by `pane_images_subset_of_images_keys_after_*`
    /// below -- a future regression that mutated one map without
    /// the other would not survive that check.
    images: HashMap<(PaneLayerId, u32), ImageEntry>,
    pane_images: HashMap<PaneLayerId, Vec<u32>>,
    /// dashcompositor `LayerId`s for the current tab bar
    /// overlay. One background `RectLayer` + one `TextLayer`
    /// per tab. Rebuilt every frame by [`Self::update_tab_bar`]
    /// (old layers are removed first). Empty when no tab bar
    /// has been rendered yet.
    tab_bar_layers: Vec<LayerId>,
    /// Implicit kitty-protocol detection flag. Set to `true`
    /// when the first pane image is loaded via [`Self::push_image`],
    /// which proves the host terminal supports the kitty
    /// graphics protocol (nested PTY children forwarded their
    /// kitty commands through it). Gates
    /// [`Self::update_tab_bar`] so non-kitty terminals never
    /// have tab bar layers pushed and never emit a full-frame
    /// APC-G block that would produce garbled output.
    kitty_capable: bool,
    /// Detected graphics protocol for the host terminal.
    /// Drives encoder selection in [`Self::render_and_write`].
    /// Defaults to [`GraphicsProtocol::detect()`] at
    /// construction; can be overridden via the
    /// `CMDASH_GRAPHICS` env var.
    protocol: GraphicsProtocol,
}

impl GraphicsState {
    /// Construct a [`GraphicsState`] with cell-pixel metrics and
    /// a non-zero cell dimension. `cells.0 > 0 && cells.1 > 0`
    /// is enforced by `assert!` so a downstream
    /// [`Self::render_and_write`] cannot produce a zero-size
    /// framebuffer. The exactly-string `"cells must be non-zero"`
    /// in the panic message is consumed by the
    /// `graphics_state_new_panics_on_zero_*` regression tests.
    pub fn new(metrics: Metrics, cells: (u16, u16)) -> Self {
        Self::new_with_protocol(metrics, cells, GraphicsProtocol::detect())
    }

    /// Construct a [`GraphicsState`] with an explicit protocol
    /// override. Tests use this to force a specific protocol
    /// without relying on the `TERM` env var.
    pub fn new_with_protocol(
        metrics: Metrics,
        cells: (u16, u16),
        protocol: GraphicsProtocol,
    ) -> Self {
        assert!(
            cells.0 > 0 && cells.1 > 0,
            "GraphicsState::new: cells must be non-zero (cols > 0 and rows > 0), got {}x{}",
            cells.0,
            cells.1,
        );
        info!(protocol = protocol.name(), "graphics protocol detected");
        Self {
            stack: LayerStack::default(),
            metrics,
            cells,
            images: HashMap::new(),
            pane_images: HashMap::new(),
            tab_bar_layers: Vec::new(),
            kitty_capable: false,
            protocol,
        }
    }

    /// Replace the cell-grid size [`Self::render_and_write`]
    /// composes against. v1 had a single tab with one root
    /// layout rect, so resizing wasn't a path; v2 wires host
    /// SIGWINCH (crossterm `Event::Resize`) into the binary's
    /// tick loop, which must call [`Self::set_cells`] so the
    /// dashcompositor framebuffer pixel dimensions stay
    /// in-sync with the layout engine's cell-grid rect.
    /// Asserts the same `non-zero` invariant as [`Self::new`]
    /// -- window-snap / hide-and-restore can briefly emit
    /// `Event::Resize(0, 0)` and we must reject before a
    /// zero-pixel composition would crash dashcompositor.
    pub fn set_cells(&mut self, cells: (u16, u16)) {
        assert!(
            cells.0 > 0 && cells.1 > 0,
            "GraphicsState::set_cells: cells must be non-zero (cols > 0 and rows > 0), got {}x{}",
            cells.0,
            cells.1,
        );
        self.cells = cells;
    }

    /// Read-only accessor for the cell-grid size
    /// [`Self::render_and_write`] composes against. Mirrors
    /// [`Self::set_cells`]; non-zero-by-construction guarantee
    /// is inherited from [`Self::new`] or any prior
    /// [`Self::set_cells`] call. Used by tests to assert a
    /// host resize made it through the binary's tick loop.
    pub fn cells(&self) -> (u16, u16) {
        self.cells
    }

    /// Push a fresh [`ImageLayer`] onto the stack from a
    /// pre-decoded `RGBA`, register it under `(pane, kitty_id)`,
    /// and add the `kitty_id` to the `pane`'s image list. Used by
    /// both `Self::on_kitty` (production) and unit/integration
    /// tests (no real PNG decode required).
    pub fn push_image(
        &mut self,
        pane: PaneLayerId,
        kitty_id: u32,
        rgba: image::RgbaImage,
    ) -> LayerId {
        let layer = ImageLayer::from_dynamic(image::DynamicImage::ImageRgba8(rgba.clone()), 0, 0);
        let lid = self.stack.push(layer);
        self.images.insert(
            (pane, kitty_id),
            ImageEntry {
                layer_id: lid,
                rgba,
            },
        );
        self.pane_images.entry(pane).or_default().push(kitty_id);
        // First image load proves the host terminal supports
        // the kitty graphics protocol (nested PTY children
        // forwarded their kitty commands through it). Enables
        // tab bar dashcompositor layers via [`Self::update_tab_bar`].
        self.kitty_capable = true;
        lid
    }

    /// Apply one kitty event from the supplied pane's PTY. Errors
    /// are surfaced, never swallowed silently: callers decide
    /// whether to log+continue (`apply_kitty_event` is a thin
    /// wrapper that logs via `tracing::warn!` and returns `()`).
    /// Private -- only `Self::apply_kitty_event` (the public
    /// surface) and the internal sanity tests in this module
    /// call this; the pub surface is exactly `apply_kitty_event`.
    fn on_kitty(&mut self, pane: PaneLayerId, cmd: &KittyGraphicCmd) -> Result<(), GraphicsError> {
        match cmd {
            KittyGraphicCmd::Load {
                id,
                placement_id: _,
                format: _,
                width: _,
                height: _,
                data,
            } => {
                let dyn_img = image::load_from_memory(data)?;
                self.push_image(pane, *id, dyn_img.to_rgba8());
            }
            KittyGraphicCmd::Place {
                id,
                placement_id: _,
                x,
                y,
                cols_cells: _,
                rows_cells: _,
                z,
            } => {
                if let Some(mut entry) = self.images.remove(&(pane, *id)) {
                    self.stack.remove(entry.layer_id);
                    let dyn_img = image::DynamicImage::ImageRgba8(entry.rgba.clone());
                    let layer =
                        ImageLayer::from_dynamic(dyn_img, *x as u32, *y as u32).with_z(*z as u32);
                    let new_lid = self.stack.push(layer);
                    entry.layer_id = new_lid;
                    self.images.insert((pane, *id), entry);
                }
            }
            KittyGraphicCmd::Delete { id } => {
                if let Some(entry) = self.images.remove(&(pane, *id)) {
                    self.stack.remove(entry.layer_id);
                    if let Some(v) = self.pane_images.get_mut(&pane) {
                        v.retain(|x| x != id);
                    }
                }
            }
            KittyGraphicCmd::Control { .. } => {}
        }
        Ok(())
    }

    /// Best-effort wrapper around `Self::on_kitty` that logs
    /// failures via `tracing::warn!` instead of propagating. v1
    /// treats kitty errors as non-fatal because the child's own
    /// shell session must keep running; a failed image must not
    /// crash the multiplexer.
    pub fn apply_kitty_event(&mut self, pane: PaneLayerId, cmd: &KittyGraphicCmd) {
        if let Err(e) = self.on_kitty(pane, cmd) {
            warn!(error = %e, ?pane, "kitty graphics decode/route failed");
        }
    }

    /// Compose the layer stack into a framebuffer sized from
    /// `cells.0 * cell_w` by `cells.1 * cell_h` pixels, then enqueue
    /// it through dashcompositor's kitty passthrough encoder.
    /// Uses `CpuCompositor.compose` rather than
    /// `LayerStack::render_to_current_terminal` so frame size is
    /// driven by the binary's grid (not dashcompositor's
    /// `TerminalSize::current()` heuristic, which can drift on
    /// non-TTY CI).
    pub fn render_and_write<W: Write>(&self, writer: &mut W) -> Result<(), GraphicsError> {
        // Early-out when no images and no tab bar layers exist:
        // composing an empty LayerStack still produces a
        // full-frame APC-G block (~1 MiB at 80×24 cells) that
        // overwrites the text body rendered by ratatui in
        // phase 3a. Skipping the compose+encode avoids both
        // the stdout corruption and the per-frame CPU cost.
        // When tab bar layers are present, we always compose
        // so the kitty-native tab bar overlay is emitted.
        if self.images.is_empty() && self.tab_bar_layers.is_empty() {
            return Ok(());
        }
        // Text-only mode: no encoder to call. The early-out
        // above already handles the empty-stack case; if we
        // reach here with layers but TextOnly protocol, skip
        // encoding to avoid garbled output.
        if self.protocol == GraphicsProtocol::TextOnly {
            return Ok(());
        }
        let w_px = self.cells.0 as u32 * self.metrics.cell_w;
        let h_px = self.cells.1 as u32 * self.metrics.cell_h;
        let mut fb = FrameBuffer::new(w_px, h_px);
        CpuCompositor.compose(&self.stack, &mut fb);
        match self.protocol {
            GraphicsProtocol::Kitty => {
                encode_passthrough_to_writer(&fb, writer)
                    .map_err(|e| GraphicsError::Dispatch(e.to_string()))?;
            }
            GraphicsProtocol::Sixel => {
                encode_sixel_to_writer(&fb, writer)
                    .map_err(|e| GraphicsError::Dispatch(e.to_string()))?;
            }
            GraphicsProtocol::TextOnly => {
                // Unreachable: guarded above.
            }
        }
        Ok(())
    }

    /// The detected graphics protocol. Exposed for logging
    /// and diagnostics.
    pub fn protocol(&self) -> GraphicsProtocol {
        self.protocol
    }

    /// Returns `true` if a record exists for `(pane, kitty_id)`,
    /// i.e. an image layer was loaded into the pane and has not
    /// since been deleted. Useful for tests; cheap because the
    /// inner map has at most one entry per `(pane, kitty_id)`.
    pub fn has_image(&self, pane: PaneLayerId, kitty_id: u32) -> bool {
        self.images.contains_key(&(pane, kitty_id))
    }

    /// Tear down every layer that originated from `pane`. Called
    /// from the binary when a pane's child exits — the per-pane
    /// [`PaneLayerId`] is dropped from the maps and the
    /// dashcompositor `LayerStack` is asked to forget each
    /// associated `LayerId`.
    pub fn close_pane(&mut self, pane: PaneLayerId) {
        if let Some(ids) = self.pane_images.remove(&pane) {
            for id in ids {
                if let Some(entry) = self.images.remove(&(pane, id)) {
                    self.stack.remove(entry.layer_id);
                }
            }
        }
    }

    /// Rebuild the tab bar as dashcompositor layers. Removes
    /// any previously-pushed tab bar layers, then pushes a
    /// background `RectLayer` (dark gray, full-width, one cell
    /// row) and one `TextLayer` per tab (active tab highlighted
    /// with blue bg + white bold; inactive tabs dim gray). Uses
    /// dashcompositor's bundled fontdue rasterizer via the
    /// `font-rasterizer` feature.
    ///
    /// Called once per frame from [`TickContext::run`] before
    /// `render_and_write`. The ratatui text tab bar in phase 3a
    /// is preserved as a degraded-mode fallback for non-kitty
    /// terminals; the pixel overlay overwrites it on kitty-
    /// capable hosts.
    pub fn update_tab_bar(&mut self, data: &TabBarData) {
        // TODO(v2): add a dirty flag or compare `data` against
        // the previous frame's state to skip the full rebuild
        // when nothing changed. The tab bar rarely changes (only
        // on tab switch / new / close); v1's per-frame rebuild
        // of ~5-7 layers is acceptable.
        // Remove previous tab bar layers.
        for lid in self.tab_bar_layers.drain(..) {
            self.stack.remove(lid);
        }
        // Gate on kitty-capable detection: non-kitty terminals
        // must never have tab bar layers pushed (the full-frame
        // APC-G block would produce garbled output). On kitty
        // terminals, the first pane image load sets this flag.
        // Gate on graphics-capable detection: non-kitty/non-sixel
        // terminals must never have tab bar layers pushed (the
        // full-frame APC-G block would produce garbled output).
        // On kitty terminals, the first pane image load sets
        // kitty_capable. On sixel terminals, the protocol field
        // is set at construction.
        let has_graphics = self.kitty_capable || self.protocol != GraphicsProtocol::TextOnly;
        if !has_graphics || data.bar_width_cells == 0 {
            return;
        }

        let cw = self.metrics.cell_w;
        let ch = self.metrics.cell_h;
        let bar_w_px = data.bar_width_cells as u32 * cw;

        // Background: dark gray full-width bar.
        let bg = RectLayer::new(0, 0, bar_w_px, ch, TAB_BAR_BG)
            .with_name("tab_bar_bg")
            .with_z(TAB_BAR_Z_BASE);
        self.tab_bar_layers.push(self.stack.push(bg));

        // Per-tab highlight + text.
        let mut col: u32 = 0;
        for (idx, label) in data.labels.iter().enumerate() {
            if col >= data.bar_width_cells as u32 {
                break;
            }
            let is_active = idx == data.active_idx;
            let tab_text = if let Some(l) = label.filter(|s| !s.is_empty()) {
                format!(" {}:{} ", idx + 1, l)
            } else {
                format!(" {} ", idx + 1)
            }
            .chars()
            .take(data.bar_width_cells as usize - col as usize)
            .collect::<String>();
            let tab_chars = tab_text.chars().count() as u32;
            if tab_chars == 0 {
                break;
            }

            // Highlight background rectangle for this tab.
            let hl_color = if is_active {
                TAB_BAR_ACTIVE_BG
            } else {
                TAB_BAR_INACTIVE_BG
            };
            let hl = RectLayer::new(col * cw, 0, tab_chars * cw, ch, hl_color)
                .with_name(format!("tab_bar_tab_{idx}_bg"))
                .with_z(TAB_BAR_Z_BASE + 1);
            self.tab_bar_layers.push(self.stack.push(hl));

            // Text layer. Starts at the same pixel x as the
            // highlight rect (the leading space character in the
            // tab text string provides the visual indent). y=1
            // shifts the baseline 1px from the top of the cell
            // row, centering a ~14px glyph in a 16px row.
            let text_color = if is_active {
                TAB_BAR_ACTIVE_FG
            } else {
                TAB_BAR_INACTIVE_FG
            };
            let text_x = col * cw;
            // Guard font size underflow: ch=1 (hypothetical 1px
            // cells) would produce -1.0, panicking fontdue.
            let font_px = (ch as f32 - 2.0).max(4.0);
            let tl = TextLayer::new(text_x, 1, tab_text, text_color)
                .with_font_size(font_px)
                .with_name(format!("tab_bar_tab_{idx}_text"))
                .with_z(TAB_BAR_Z_BASE + 2);
            self.tab_bar_layers.push(self.stack.push(tl));

            col += tab_chars;
            // Separator gap between tabs (1 cell).
            if col < data.bar_width_cells as u32 && idx + 1 < data.labels.len() {
                col += 1;
            }
        }
    }

    /// Remove all tab bar layers from the stack. Called when
    /// the tab bar should no longer be rendered (e.g. when
    /// switching to a single-tab mode).
    pub fn clear_tab_bar(&mut self) {
        for lid in self.tab_bar_layers.drain(..) {
            self.stack.remove(lid);
        }
    }

    /// Manually set the kitty-capable flag. Used by tests that
    /// need to exercise [`Self::update_tab_bar`] without loading
    /// a real pane image. Production callers should NOT use this;
    /// the flag is set automatically by [`Self::push_image`].
    #[cfg(test)]
    pub(crate) fn set_kitty_capable(&mut self, capable: bool) {
        self.kitty_capable = capable;
    }
}

// ---------------------------------------------------------------------------
// Internal sanity tests. Image layers are pushed via [`GraphicsState::push_image`]
// so we do not depend on a (notoriously fiddly) embedded PNG byte sequence.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod internal_sanity_tests {
    use super::*;

    fn rgba1x1() -> image::RgbaImage {
        image::RgbaImage::new(1, 1)
    }

    fn place_cmd(id: u32, x: i32, y: i32, z: i32) -> KittyGraphicCmd {
        KittyGraphicCmd::Place {
            id,
            placement_id: 0,
            x,
            y,
            cols_cells: None,
            rows_cells: None,
            z,
        }
    }

    #[test]
    fn load_inserts_layer_and_records_mapping() {
        let mut g = GraphicsState::new(Metrics::default(), (80, 24));
        g.push_image(PaneLayerId(1), 7, rgba1x1());
        assert!(g.images.contains_key(&(PaneLayerId(1), 7)));
        let n = g
            .pane_images
            .get(&PaneLayerId(1))
            .map_or(0, std::vec::Vec::len);
        assert_eq!(n, 1);
    }

    #[test]
    fn place_updates_position_and_keeps_rgba() {
        let mut g = GraphicsState::new(Metrics::default(), (80, 24));
        let pane = PaneLayerId(2);
        g.push_image(pane, 7, rgba1x1());
        g.on_kitty(pane, &place_cmd(7, 10, 20, 0)).expect("place");
        assert!(g.images.contains_key(&(pane, 7)));
    }

    #[test]
    fn delete_removes_layer_and_clears_per_pane_listing() {
        let mut g = GraphicsState::new(Metrics::default(), (80, 24));
        let pane = PaneLayerId(3);
        g.push_image(pane, 7, rgba1x1());
        g.on_kitty(pane, &KittyGraphicCmd::Delete { id: 7 })
            .expect("delete");
        assert!(!g.images.contains_key(&(pane, 7)));
        let v = g.pane_images.get(&pane).expect("pane_images entry");
        assert!(
            v.is_empty(),
            "deleted image should leave an empty per-pane vec"
        );
    }

    #[test]
    fn unknown_place_is_silent_no_op() {
        let mut g = GraphicsState::new(Metrics::default(), (80, 24));
        g.on_kitty(PaneLayerId(4), &place_cmd(99, 1, 2, 0))
            .expect("unknown place is a no-op");
        assert!(g.images.is_empty());
    }

    #[test]
    fn render_and_write_emits_escapes() {
        // Explicit Kitty protocol: `new()` detects TextOnly in CI
        // (no real terminal), which early-outs before encoding.
        let mut g =
            GraphicsState::new_with_protocol(Metrics::default(), (80, 24), GraphicsProtocol::Kitty);
        g.push_image(PaneLayerId(5), 7, rgba1x1());
        let mut out = Vec::new();
        g.render_and_write(&mut out).expect("render");
        assert!(
            out.windows(3).any(|w| w == b"\x1b_G"),
            "encoded stream should contain the kitty APC-G escape"
        );
    }

    #[test]
    fn close_pane_drops_all_layers() {
        let mut g = GraphicsState::new(Metrics::default(), (80, 24));
        let pane = PaneLayerId(6);
        g.push_image(pane, 7, rgba1x1());
        g.push_image(pane, 8, rgba1x1());
        g.close_pane(pane);
        assert!(!g.pane_images.contains_key(&pane));
        assert!(g.images.is_empty());
    }

    /// Cross-key invariant pin: for every `pane`, every `kitty_id`
    /// recorded in `pane_images[pane]` MUST also appear as the
    /// second component of a key in `images`. Exercised against
    /// the three mutating paths (`push_image`, `on_kitty::Place`,
    /// `on_kitty::Delete`) so a future regression that mutates one
    /// map without the other is caught at unit-test time.
    #[test]
    fn pane_images_subset_of_images_keys_after_push_place_delete() {
        let mut g = GraphicsState::new(Metrics::default(), (80, 24));
        let pane = PaneLayerId(42);
        // Three pushes.
        g.push_image(pane, 1, rgba1x1());
        g.push_image(pane, 2, rgba1x1());
        g.push_image(pane, 3, rgba1x1());
        // Place-and-replace on kitty_id=2 (keeps both class membership
        // and the entry in pane_images).
        g.on_kitty(pane, &place_cmd(2, 5, 6, 0)).expect("place");
        // Delete on kitty_id=1 (removes from BOTH maps).
        g.on_kitty(pane, &KittyGraphicCmd::Delete { id: 1 })
            .expect("delete");
        // After all three ops the surviving pane_images[pane] is [2, 3]
        // (insert order; delete removed 1, place on 2 didn't change
        // its membership). Every entry must back a real `images` key.
        let recorded = g
            .pane_images
            .get(&pane)
            .expect("pane_images should still hold an entry for this pane")
            .clone();
        assert_eq!(recorded, vec![2, 3]);
        for kitty_id in &recorded {
            assert!(
                g.images.contains_key(&(pane, *kitty_id)),
                "pane_images[pane] = {:?} contains kitty_id {} but \
                 images lacks key ({:?}, {}) -- cross-key invariant violated",
                recorded,
                kitty_id,
                pane,
                kitty_id,
            );
        }
    }

    /// Ctor invariant pin: zero cols must panic with the exact phrase
    /// `"cells must be non-zero"` so external debuggers and test
    /// matchers can correlate the failure to the `Self::new` assert
    /// rather than chasing an opaque zero-framebuffer downstream.
    #[test]
    #[should_panic(expected = "cells must be non-zero")]
    fn graphics_state_new_panics_on_zero_cols() {
        let _ = GraphicsState::new(Metrics::default(), (0, 24));
    }

    /// Ctor invariant pin: zero rows must panic with the same
    /// phrase, symmetric to the cols case above.
    #[test]
    #[should_panic(expected = "cells must be non-zero")]
    fn graphics_state_new_panics_on_zero_rows() {
        let _ = GraphicsState::new(Metrics::default(), (80, 0));
    }

    /// Ctor invariant pin: zero `cell_w` must panic with the exact
    /// phrase `"cell_w and cell_h must be non-zero"` so debug-time
    /// failures (and tests) can correlate directly to the
    /// [`Metrics::new`] assert rather than chasing an opaque
    /// panic. Mirrors `graphics_state_new_panics_on_zero_cols`
    /// in shape and structure.
    #[test]
    #[should_panic(expected = "cell_w and cell_h must be non-zero")]
    fn metrics_new_panics_on_zero_cell_w() {
        let _ = Metrics::new(0, 16);
    }

    /// Ctor invariant pin: zero `cell_h` must panic with the same
    /// exact phrase, symmetric to the `cell_w` case above and to
    /// `graphics_state_new_panics_on_zero_rows`.
    #[test]
    #[should_panic(expected = "cell_w and cell_h must be non-zero")]
    fn metrics_new_panics_on_zero_cell_h() {
        let _ = Metrics::new(8, 0);
    }

    /// Regression test for `PaneRunner::Drop` -> `GraphicsState::close_pane`
    /// coupling through the close-channel. Spawns a real `PaneRunner` with
    /// the channel sender, drops the runner, drains the receiver
    /// (simulating `tick_loop`'s phase 1), and finally calls
    /// `close_pane` with the received id to assert the bookkeeping
    /// revokes the pane's image.
    ///
    /// This is the integration check that proves: (1) `Drop` enqueues
    /// the pane's `PaneLayerId` onto the close channel, and (2) the
    /// message contains the same id the binary will resolve through
    /// its tick loop's drain.
    #[tokio::test]
    async fn drop_pane_runner_sends_close_to_channel() {
        use crate::pane::{PaneCloseTx, PaneRunner};
        use cmdash_config::parse as parse_config;
        use cmdash_layout::{ComputedLayout, Rect as LayoutRect};
        use cmdash_pty::ShellSpec;
        let mut graphics = GraphicsState::new(Metrics::default(), (80, 24));
        let (close_tx, mut close_rx): (PaneCloseTx, _) = tokio::sync::mpsc::unbounded_channel();
        let pane_id = PaneLayerId(99);

        // Pre-populate one image layer for the pane.
        graphics.push_image(pane_id, 1, image::RgbaImage::new(1, 1));
        assert!(graphics.has_image(pane_id, 1), "image registered pre-drop");

        let cfg_text = "layout { pane kind=shell label=\"drop_test\" }";
        let cfg = parse_config(cfg_text).expect("parse KDL");
        let cfg_root = cfg.layout.expect("layout block");
        let layout = ComputedLayout::compute(
            &cfg_root,
            LayoutRect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            },
        )
        .expect("compute layout");
        let computed = layout.panes[0].clone();
        let runner = PaneRunner::spawn_with_graphics(
            computed,
            pane_id,
            ShellSpec::Command {
                argv: vec!["true".to_string()],
            },
            Some(close_tx),
        )
        .expect("spawn_with_graphics");

        // Drop enqueues the pane's layer id onto the close channel.
        drop(runner);

        // Simulate `tick_loop` phase 1: drain the close message and
        // call `close_pane` to revoke the dashcompositor layers.
        let received = close_rx
            .try_recv()
            .expect("PaneRunner::Drop must send a close message to the channel");
        assert_eq!(received, pane_id);
        graphics.close_pane(received);
        assert!(
            !graphics.has_image(pane_id, 1),
            "image layer should be revoked once the close-channel message is applied"
        );
    }

    /// `set_cells` ctor invariant pin: zero cols must panic
    /// with the same `"cells must be non-zero"` phrase the
    /// [`Self::new`] ctor uses, so callers -- debuggers and
    /// test matchers alike -- can correlate the panic to the
    /// `set_cells` assert rather than chasing an opaque
    /// zero-framebuffer downstream.
    #[test]
    #[should_panic(expected = "cells must be non-zero")]
    fn set_cells_panics_on_zero_cols() {
        let mut g = GraphicsState::new(Metrics::default(), (80, 24));
        g.set_cells((0, 24));
    }

    /// Symmetric to `set_cells_panics_on_zero_cols`: zero
    /// rows must trip the same assert with the same panic
    /// phrase.
    #[test]
    #[should_panic(expected = "cells must be non-zero")]
    fn set_cells_panics_on_zero_rows() {
        let mut g = GraphicsState::new(Metrics::default(), (80, 24));
        g.set_cells((80, 0));
    }

    /// Happy-path regression: a non-zero resize must round-trip
    /// through the read-only `cells()` accessor. Exercises the
    /// binding from the binary's host-resize-driven
    /// `GraphicsState::set_cells(...)` call to the
    /// `render_and_write` pixel composition surface.
    #[test]
    fn set_cells_updates_internal_state() {
        let mut g = GraphicsState::new(Metrics::default(), (80, 24));
        g.set_cells((132, 50));
        assert_eq!(g.cells(), (132, 50));
    }

    /// Render-and-write with an empty `LayerStack` (no images
    /// pushed) must succeed and produce ZERO output. Without
    /// this early-out, `render_and_write` would compose a
    /// full-frame APC-G block (~1 MiB at 80×24 cells) into
    /// stdout on EVERY tick, overwriting the text body from
    /// ratatui's phase 3a `terminal.draw()`. This is the
    /// root-cause fix for the blank-screen bug: the encoder
    /// was emitting a full-screen empty kitty frame that
    /// occluded all text content.
    #[test]
    fn render_and_write_empty_stack_succeeds() {
        let g = GraphicsState::new(Metrics::default(), (80, 24));
        let mut out = Vec::new();
        g.render_and_write(&mut out)
            .expect("render_and_write with empty stack must not error");
        assert!(
            out.is_empty(),
            "empty-stack render must produce ZERO output (early-out); got {} bytes",
            out.len()
        );
    }

    /// Non-empty-stack output must be bounded: the encoder
    /// should not dump excessive framebuffer data. A 640x384
    /// pixel framebuffer (80×24 cells at 8×16 px/cell) with
    /// one 1×1 image should produce a compressed passthrough
    /// frame well under 4 MiB.
    ///
    /// Explicit Kitty protocol: `new()` detects `TextOnly` in CI
    /// (no real terminal), which early-outs via the `TextOnly`
    /// guard before encoding, causing this test to pass with
    /// 0 bytes (wrong reason).
    #[test]
    fn render_and_write_nonempty_stack_output_is_bounded() {
        let mut g =
            GraphicsState::new_with_protocol(Metrics::default(), (80, 24), GraphicsProtocol::Kitty);
        g.push_image(PaneLayerId(1), 1, rgba1x1());
        let mut out = Vec::new();
        g.render_and_write(&mut out).expect("render");
        assert!(
            out.len() < 4 * 1024 * 1024,
            "non-empty-stack render output must be under 4 MiB; got {} bytes",
            out.len()
        );
    }

    // ------------------------------------------------------------------
    // Tab-bar dashcompositor layer tests.
    // ------------------------------------------------------------------

    use super::TabBarData;

    #[test]
    fn update_tab_bar_pushes_layers() {
        let mut g = GraphicsState::new(Metrics::default(), (80, 24));
        g.set_kitty_capable(true);
        let data = TabBarData {
            labels: vec![Some("active"), Some("inactive")],
            active_idx: 0,
            bar_width_cells: 80,
        };
        g.update_tab_bar(&data);
        // 1 bg RectLayer + 2 highlight RectLayers + 2 TextLayers = 5.
        assert_eq!(
            g.tab_bar_layers.len(),
            5,
            "update_tab_bar must push 1 bg + 2 highlights + 2 text = 5 layers"
        );
    }

    #[test]
    fn update_tab_bar_removes_old_layers_before_pushing_new() {
        let mut g = GraphicsState::new(Metrics::default(), (80, 24));
        g.set_kitty_capable(true);
        let data = TabBarData {
            labels: vec![Some("a")],
            active_idx: 0,
            bar_width_cells: 80,
        };
        g.update_tab_bar(&data);
        let first_count = g.tab_bar_layers.len();
        // Second call with 3 tabs should remove old layers and
        // push new ones — total count changes.
        let data2 = TabBarData {
            labels: vec![Some("x"), Some("y"), Some("z")],
            active_idx: 1,
            bar_width_cells: 80,
        };
        g.update_tab_bar(&data2);
        assert_eq!(
            g.tab_bar_layers.len(),
            7,
            "second update must push 1 bg + 3 highlights + 3 text = 7 layers"
        );
        assert_ne!(
            first_count,
            g.tab_bar_layers.len(),
            "layer count must differ between 1-tab and 3-tab configs"
        );
    }

    #[test]
    fn clear_tab_bar_removes_all_tab_bar_layers() {
        let mut g = GraphicsState::new(Metrics::default(), (80, 24));
        g.set_kitty_capable(true);
        let data = TabBarData {
            labels: vec![Some("a"), Some("b")],
            active_idx: 0,
            bar_width_cells: 80,
        };
        g.update_tab_bar(&data);
        assert!(!g.tab_bar_layers.is_empty());
        g.clear_tab_bar();
        assert!(
            g.tab_bar_layers.is_empty(),
            "clear_tab_bar must drain all tab bar layer ids"
        );
    }

    #[test]
    fn update_tab_bar_zero_width_is_noop() {
        let mut g = GraphicsState::new(Metrics::default(), (80, 24));
        g.set_kitty_capable(true);
        // Zero-width bar must be a no-op even when kitty_capable
        // is true, isolating the zero-width gate from the
        // kitty_capable gate.
        let data = TabBarData {
            labels: vec![Some("a")],
            active_idx: 0,
            bar_width_cells: 0,
        };
        g.update_tab_bar(&data);
        assert!(
            g.tab_bar_layers.is_empty(),
            "zero-width tab bar must push no layers even when kitty_capable"
        );
    }

    #[test]
    fn render_and_write_emits_output_for_tab_bar_only() {
        // Explicit Kitty protocol: `new()` detects TextOnly in CI
        // (no real terminal), which early-outs before encoding.
        let mut g =
            GraphicsState::new_with_protocol(Metrics::default(), (80, 24), GraphicsProtocol::Kitty);
        g.set_kitty_capable(true);
        // No images loaded — render_and_write early-outs.
        let mut out = Vec::new();
        g.render_and_write(&mut out).expect("empty");
        assert!(out.is_empty(), "empty state must produce zero output");

        // After adding tab bar layers, render_and_write must
        // emit APC-G output even without any pane images.
        let data = TabBarData {
            labels: vec![Some("tab1")],
            active_idx: 0,
            bar_width_cells: 80,
        };
        g.update_tab_bar(&data);
        let mut out2 = Vec::new();
        g.render_and_write(&mut out2).expect("tab bar render");
        assert!(
            !out2.is_empty(),
            "tab-bar-only render must produce non-zero output (kitty APC-G)"
        );
        assert!(
            out2.windows(3).any(|w| w == b"\x1b_G"),
            "output must contain the kitty APC-G escape"
        );
    }

    #[test]
    fn update_tab_bar_single_tab_produces_three_layers() {
        let mut g = GraphicsState::new(Metrics::default(), (40, 20));
        g.set_kitty_capable(true);
        let data = TabBarData {
            labels: vec![None],
            active_idx: 0,
            bar_width_cells: 40,
        };
        g.update_tab_bar(&data);
        // 1 bg + 1 highlight + 1 text = 3.
        assert_eq!(g.tab_bar_layers.len(), 3);
    }

    #[test]
    fn update_tab_bar_preserves_pane_image_layers() {
        let mut g = GraphicsState::new(Metrics::default(), (80, 24));
        g.push_image(PaneLayerId(1), 1, rgba1x1());
        // push_image sets kitty_capable automatically.
        assert!(g.kitty_capable, "push_image must set kitty_capable");
        assert!(g.has_image(PaneLayerId(1), 1));
        let data = TabBarData {
            labels: vec![Some("a")],
            active_idx: 0,
            bar_width_cells: 80,
        };
        g.update_tab_bar(&data);
        // Pane image must survive the tab bar update.
        assert!(
            g.has_image(PaneLayerId(1), 1),
            "pane image must not be removed by update_tab_bar"
        );
        // Tab bar layers must be present too.
        assert!(!g.tab_bar_layers.is_empty());
    }

    /// `update_tab_bar` is a no-op when `kitty_capable` is
    /// false AND protocol is `TextOnly`. The guard
    /// (`kitty_capable || protocol != TextOnly`) gates tab bar
    /// dashcompositor layers so non-graphics terminals never
    /// emit garbled APC-G/Sixel output.
    #[test]
    fn update_tab_bar_noop_when_not_kitty_capable() {
        let mut g = GraphicsState::new(Metrics::default(), (80, 24));
        // kitty_capable defaults to false (no push_image yet).
        let data = TabBarData {
            labels: vec![Some("a"), Some("b")],
            active_idx: 0,
            bar_width_cells: 80,
        };
        g.update_tab_bar(&data);
        assert!(
            g.tab_bar_layers.is_empty(),
            "update_tab_bar must be a no-op when kitty_capable is false"
        );
        // After setting kitty_capable, layers are pushed.
        g.set_kitty_capable(true);
        g.update_tab_bar(&data);
        assert!(
            !g.tab_bar_layers.is_empty(),
            "update_tab_bar must push layers when kitty_capable is true"
        );
    }

    // ------------------------------------------------------------------
    // GraphicsProtocol detection + Sixel fallback tests.
    // ------------------------------------------------------------------

    /// `GraphicsProtocol::new_with_protocol` with Sixel protocol
    /// must produce a `GraphicsState` whose `protocol()` returns
    /// `Sixel`. Pins the explicit-protocol ctor path so tests
    /// can exercise Sixel encoding without relying on `TERM`.
    #[test]
    fn new_with_protocol_sixel_returns_sixel() {
        let g =
            GraphicsState::new_with_protocol(Metrics::default(), (80, 24), GraphicsProtocol::Sixel);
        assert_eq!(g.protocol(), GraphicsProtocol::Sixel);
    }

    /// `GraphicsProtocol::new_with_protocol` with Kitty protocol
    /// must return `Kitty`. Symmetric to the Sixel test above.
    #[test]
    fn new_with_protocol_kitty_returns_kitty() {
        let g =
            GraphicsState::new_with_protocol(Metrics::default(), (80, 24), GraphicsProtocol::Kitty);
        assert_eq!(g.protocol(), GraphicsProtocol::Kitty);
    }

    /// `GraphicsProtocol::new_with_protocol` with `TextOnly`
    /// must return `TextOnly`.
    #[test]
    fn new_with_protocol_text_only_returns_text_only() {
        let g = GraphicsState::new_with_protocol(
            Metrics::default(),
            (80, 24),
            GraphicsProtocol::TextOnly,
        );
        assert_eq!(g.protocol(), GraphicsProtocol::TextOnly);
    }

    /// `render_and_write` with `TextOnly` protocol must skip
    /// encoding entirely — the writer must remain empty. Pins
    /// the early-out path that prevents garbled output when
    /// neither Kitty nor Sixel is available.
    #[test]
    fn text_only_protocol_skips_encoding() {
        let mut g = GraphicsState::new_with_protocol(
            Metrics::default(),
            (80, 24),
            GraphicsProtocol::TextOnly,
        );
        let data = TabBarData {
            labels: vec![Some("a"), Some("b")],
            active_idx: 0,
            bar_width_cells: 80,
        };
        g.update_tab_bar(&data);
        // TextOnly: render_and_write must NOT emit any bytes.
        let mut out = Vec::new();
        g.render_and_write(&mut out).expect("render_and_write");
        assert!(
            out.is_empty(),
            "TextOnly protocol must produce empty output, got {} bytes",
            out.len()
        );
    }

    /// `render_and_write` with `Kitty` protocol and layers must
    /// not contain Sixel DCS sequences. Pins the kitty-encoder
    /// dispatch path.
    #[test]
    fn render_and_write_kitty_emits_apc_g() {
        let mut g =
            GraphicsState::new_with_protocol(Metrics::default(), (80, 24), GraphicsProtocol::Kitty);
        let data = TabBarData {
            labels: vec![Some("a"), Some("b")],
            active_idx: 0,
            bar_width_cells: 80,
        };
        g.update_tab_bar(&data);
        let mut out = Vec::new();
        g.render_and_write(&mut out).expect("render_and_write");
        // Kitty output must NOT contain Sixel DCS sequences.
        assert!(
            !out.windows(2).any(|w| w == b"\x1bP"),
            "Kitty protocol output must not contain Sixel DCS"
        );
    }

    /// `render_and_write` with `Sixel` protocol and layers must
    /// produce DCS sequences (`\x1bP`) and must NOT contain kitty
    /// APC-G escapes (`\x1b_G`). Pins the sixel-encoder dispatch
    /// path and verifies the roadmap item "verify the fallback
    /// path produces valid Sixel escapes".
    #[test]
    fn render_and_write_sixel_emits_dcs() {
        let mut g =
            GraphicsState::new_with_protocol(Metrics::default(), (80, 24), GraphicsProtocol::Sixel);
        let data = TabBarData {
            labels: vec![Some("a"), Some("b")],
            active_idx: 0,
            bar_width_cells: 80,
        };
        g.update_tab_bar(&data);
        let mut out = Vec::new();
        g.render_and_write(&mut out).expect("render_and_write");
        // Sixel output must contain DCS sequences.
        assert!(
            out.windows(2).any(|w| w == b"\x1bP"),
            "Sixel protocol output must contain DCS sequences (ESC P)"
        );
        // Sixel output must NOT contain kitty APC-G escapes.
        assert!(
            !out.windows(3).any(|w| w == b"\x1b_G"),
            "Sixel protocol output must not contain kitty APC-G escapes"
        );
    }

    /// `GraphicsProtocol::name()` must return the expected
    /// human-readable string for each variant. Pins the
    /// startup-log format.
    #[test]
    fn protocol_name_returns_expected_strings() {
        assert_eq!(GraphicsProtocol::Kitty.name(), "kitty");
        assert_eq!(GraphicsProtocol::Sixel.name(), "sixel");
        assert_eq!(GraphicsProtocol::TextOnly.name(), "text-only");
    }

    /// `GraphicsProtocol::detect()` must respect the
    /// `CMDASH_GRAPHICS` env var when set to `kitty`.
    #[test]
    fn detect_from_cmdash_graphics_env_kitty() {
        let g = GraphicsState::new_with_protocol(
            Metrics::default(),
            (80, 24),
            GraphicsProtocol::detect_from_override("kitty"),
        );
        assert_eq!(g.protocol(), GraphicsProtocol::Kitty);
    }

    /// `GraphicsProtocol::detect()` must respect the
    /// `CMDASH_GRAPHICS` env var when set to `sixel`.
    #[test]
    fn detect_from_cmdash_graphics_env_sixel() {
        let g = GraphicsState::new_with_protocol(
            Metrics::default(),
            (80, 24),
            GraphicsProtocol::detect_from_override("sixel"),
        );
        assert_eq!(g.protocol(), GraphicsProtocol::Sixel);
    }

    /// `update_tab_bar` with Sixel protocol must push tab bar
    /// layers (not gated by `kitty_capable`). Pins the fix
    /// that broadened the guard from `kitty_capable`-only to
    /// `kitty_capable || protocol != TextOnly`.
    #[test]
    fn update_tab_bar_works_with_sixel_protocol() {
        let mut g =
            GraphicsState::new_with_protocol(Metrics::default(), (80, 24), GraphicsProtocol::Sixel);
        let data = TabBarData {
            labels: vec![Some("a")],
            active_idx: 0,
            bar_width_cells: 80,
        };
        g.update_tab_bar(&data);
        assert!(
            !g.tab_bar_layers.is_empty(),
            "Sixel protocol must allow tab bar layers"
        );
    }

    /// `update_tab_bar` with `TextOnly` protocol must NOT push
    /// tab bar layers (no graphics path available). Symmetric
    /// to `update_tab_bar_noop_when_not_kitty_capable`.
    #[test]
    fn update_tab_bar_noop_when_text_only() {
        let mut g = GraphicsState::new_with_protocol(
            Metrics::default(),
            (80, 24),
            GraphicsProtocol::TextOnly,
        );
        let data = TabBarData {
            labels: vec![Some("a"), Some("b")],
            active_idx: 0,
            bar_width_cells: 80,
        };
        g.update_tab_bar(&data);
        assert!(
            g.tab_bar_layers.is_empty(),
            "TextOnly protocol must not push tab bar layers"
        );
    }

    // ------------------------------------------------------------------
    // DA1 (Device Attributes) response parsing tests.
    // ------------------------------------------------------------------

    /// `parse_da1_response` with a standard xterm DA1 response
    /// (`ESC[?62;22c`) must return `None` — no Sixel attribute.
    #[test]
    fn parse_da1_xterm_no_sixel() {
        let resp = b"[?62;22c";
        assert!(
            parse_da1_response(resp).is_none(),
            "xterm DA1 without attribute 4 must return None"
        );
    }

    /// `parse_da1_response` with Sixel attribute 4
    /// (`ESC[?62;4c`) must return `Some(Sixel)`.
    #[test]
    fn parse_da1_sixel_detected() {
        let resp = b"[?62;4c";
        assert_eq!(
            parse_da1_response(resp),
            Some(GraphicsProtocol::Sixel),
            "DA1 response with attribute 4 must detect Sixel"
        );
    }
    /// `parse_da1_response` with attribute 31 only
    /// (`ESC[?62;31c`) must return `None`. Kitty is detected
    /// via `TERM`/`TERM_PROGRAM`, not DA1.
    #[test]
    fn parse_da1_attr_31_returns_none() {
        let resp = b"\x1b[?62;31c";
        assert!(
            parse_da1_response(resp).is_none(),
            "DA1 attribute 31 is not Sixel; must return None"
        );
    }

    /// `parse_da1_response` with both Sixel (4) and attr 31
    /// must still detect Sixel (31 is irrelevant).
    #[test]
    fn parse_da1_sixel_with_attr_31_still_detects_sixel() {
        let resp = b"\x1b[?62;4;31c";
        assert_eq!(
            parse_da1_response(resp),
            Some(GraphicsProtocol::Sixel),
            "DA1 with both 4 and 31 must detect Sixel"
        );
    }
    /// return `None`.
    #[test]
    fn parse_da1_empty_returns_none() {
        assert!(parse_da1_response(b"").is_none());
        assert!(parse_da1_response(b"garbage").is_none());
        assert!(parse_da1_response(b"[c").is_none());
    }

    /// `parse_da1_response` with a partial response (no
    /// terminator `c`) must return `None`.
    #[test]
    fn parse_da1_partial_returns_none() {
        let resp = b"[?62;4";
        assert!(
            parse_da1_response(resp).is_none(),
            "partial DA1 response without terminator must return None"
        );
    }

    /// `parse_da1_response` with a response that has trailing
    /// bytes after the `c` terminator must still parse
    /// correctly (the parser stops at the first `c`).
    #[test]
    fn parse_da1_trailing_bytes_ok() {
        let resp = b"[?62;4c[?1;2;3c";
        assert_eq!(
            parse_da1_response(resp),
            Some(GraphicsProtocol::Sixel),
            "trailing bytes after DA1 terminator must not break parsing"
        );
    }

    /// `parse_da1_response` with Sixel as the sole parameter
    /// (`ESC[?4c`) must return `Some(Sixel)`.
    #[test]
    fn parse_da1_sole_param_sixel() {
        let resp = b"[?4c";
        assert_eq!(
            parse_da1_response(resp),
            Some(GraphicsProtocol::Sixel),
            "DA1 with sole param 4 must detect Sixel"
        );
    }

    /// `parse_da1_response` with many parameters including 4
    /// must detect Sixel. Mimics real terminals that report
    /// multiple capabilities.
    #[test]
    fn parse_da1_many_params_with_sixel() {
        let resp = b"[?62;1;2;4;9;15;22c";
        assert_eq!(
            parse_da1_response(resp),
            Some(GraphicsProtocol::Sixel),
            "DA1 with many params including 4 must detect Sixel"
        );
    }

    /// `parse_da1_response` with a response missing the `?`
    /// prefix after `ESC[` must return `None`.
    #[test]
    fn parse_da1_missing_question_mark() {
        let resp = b"[62;4c";
        assert!(
            parse_da1_response(resp).is_none(),
            "DA1 response missing ? after ESC[ must return None"
        );
    }
}
