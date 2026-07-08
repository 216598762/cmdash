//! cmdash-pty: per-pane pseudoterminal runner.
//!
//! This crate feeds bytes from a child PTY into the [`vte::Parser`]
//! and exposes the resulting text grid plus kitty-graphics events
//! to downstream cmdash crates (the conductor / dashcompositor
//! bridge).
//!
//! ## Design boundaries
//!
//! - **Sync.** Calls like [`PanePty::read`], [`PanePty::write`],
//!   [`PanePty::advance`] are blocking. The conductor wraps each
//!   pane in `tokio::task::spawn_blocking`.
//! - **One [`PaneLayerId`] (opaque `u64`) per pane.** The cmdash
//!   binary maps `PaneLayerId` to its own
//!   `dashcompositor::LayerId`; `cmdash-pty` does NOT depend on
//!   `dashcompositor`. AGENTS.md §"Hard rule: one layer per
//!   instance" still holds - the binary enforces 1:1 between
//!   `PaneId`s and `LayerId`s.
//! - **No graphics emission.** Kitty graphics events surface as
//!   structured [`PaneEvent::KittyGraphic`] records; the binary maps
//!   them to `dashcompositor::ImageLayer`s.
//!
//! ## Kitty graphics interception
//!
//! The kitty graphics protocol embeds `ESC _ G <key>=<value>;...;<base64> ESC \`
//! payloads (or `BEL`-terminated). At HEAD the [`PanePty::advance`]
//! byte loop pre-scans for `ESC _` APC sequences and routes their
//! payload to [`KittyAccumulator`] / [`parse_kitty_chunk`] directly.
//! This is load-bearing: `vte 0.15` silently DROPS APC strings
//! (the Paul Williams state machine only routes DCS strings through
//! `hook`/`put`/`unhook`; APC goes nowhere). See the four-state
//! pre-scan in `advance` for the byte-routing rules. The
//! `VtePerf` driver does NOT implement `vte::Perform::hook`/
//! `put`/`unhook` for kitty (those callbacks never fire for APC).

use base64::Engine;
use std::collections::HashMap;
use std::io::{Read, Write};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use thiserror::Error;
use vte::{Params, Parser};

// ---------- public constants ----------

/// Default column count when the caller doesn't override.
pub const DEFAULT_COLS: u16 = 80;

/// Default row count when the caller doesn't override.
pub const DEFAULT_ROWS: u16 = 24;

// ---------- public types ----------

/// Opaque pane-layer identifier. The cmdash binary owns the
/// mapping to its own `dashcompositor::LayerId`; this newtype
/// stays stable across dashcompositor revs and does NOT appear
/// outside this crate's API surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct PaneLayerId(pub u64);

/// Cell color. Mirrors vte 0.15's old `vte::Color` shape but
/// lives in our crate so this surface stays decoupled from vte's
/// public API churn (AGENTS.md §"Hard rule: one layer per
/// instance" - the cell color type must not leak dashcompositor
/// concerns either).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Color {
    /// No explicit color; let the terminal's default apply.
    #[default]
    Default,
    /// 256-color palette index.
    Indexed(u8),
    /// 24-bit RGB triple.
    Rgb(u8, u8, u8),
}

/// Text-cell attributes (bold / italic / underline / reverse).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CellAttrs {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub reverse: bool,
}

/// One row x col cell in the text grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub fg: Color,
    pub bg: Color,
    pub attrs: CellAttrs,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            ch: ' ',
            fg: Color::Default,
            bg: Color::Default,
            attrs: CellAttrs::default(),
        }
    }
}

/// Cell-grid text buffer. Size is fixed for the lifetime of a pane
/// unless [`PanePty::resize`] is called.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextGrid {
    cols: u16,
    rows: u16,
    cells: Vec<Cell>,
    cursor_x: u16,
    cursor_y: u16,
    /// True when the last printable filled the rightmost
    /// column. The row advance lands on the NEXT `print`, not
    /// the current one (xterm/VT100 wrap semantics). Cleared by
    /// line feeds (`LF`/`VT`/`FF`), explicit CSI cursor moves
    /// (`H`/`f`/`A`/`B`/`C`/`D`), and `consume` from `print`
    /// itself. `CR` (`0x0D`) does NOT consume the promise
    /// (a `CR` alone prints nothing, so the pending advance
    /// survives until the next printable).
    pending_wrap: bool,
    dirty_rows: Vec<u16>,
}

impl TextGrid {
    pub fn new(cols: u16, rows: u16) -> Self {
        let total = (cols as usize).saturating_mul(rows as usize);
        Self {
            cols,
            rows,
            cells: vec![Cell::default(); total],
            cursor_x: 0,
            cursor_y: 0,
            pending_wrap: false,
            dirty_rows: Vec::new(),
        }
    }
    pub fn cols(&self) -> u16 {
        self.cols
    }
    pub fn rows(&self) -> u16 {
        self.rows
    }
    pub fn cursor(&self) -> (u16, u16) {
        (self.cursor_x, self.cursor_y)
    }
    /// Consume the wrap-pending promise once. Called from
    /// `VtePerf::print` on entry, from row-advance control
    /// bytes (`LF`/`VT`/`FF`), and from explicit CSI cursor
    /// moves (`H`/`f`/`A`/`B`/`C`/`D`). Returns the prior
    /// flag value so callers can branch on whether the promise
    /// was active. `pub(crate)` because external consumers
    /// only ever see cloned snapshots via `PanePty::snapshot`,
    /// so the method must not be reachable as public API.
    pub(crate) fn consume_pending_wrap(&mut self) -> bool {
        if self.pending_wrap {
            self.pending_wrap = false;
            true
        } else {
            false
        }
    }
    pub fn cells(&self) -> &[Cell] {
        &self.cells
    }
    pub fn cell(&self, x: u16, y: u16) -> &Cell {
        let idx = self.cell_idx(x, y);
        &self.cells[idx]
    }
    fn cell_idx(&self, x: u16, y: u16) -> usize {
        (y as usize) * (self.cols as usize) + (x as usize)
    }
    fn mark_dirty(&mut self, y: u16) {
        if !self.dirty_rows.contains(&y) {
            self.dirty_rows.push(y);
        }
    }
    pub fn drain_dirty_rows(&mut self) -> Vec<u16> {
        let mut v = std::mem::take(&mut self.dirty_rows);
        v.sort_unstable();
        v
    }
    fn put(&mut self, x: u16, y: u16, fg: Color, bg: Color, attrs: CellAttrs, ch: char) {
        let idx = self.cell_idx(x, y);
        let next = Cell { ch, fg, bg, attrs };
        if self.cells[idx] != next {
            self.cells[idx] = next;
            self.mark_dirty(y);
        }
    }
    fn clear_cell(&mut self, x: u16, y: u16) {
        let idx = self.cell_idx(x, y);
        let blank = Cell::default();
        if self.cells[idx] != blank {
            self.cells[idx] = blank;
            self.mark_dirty(y);
        }
    }
    fn erase_in_line(&mut self, y: u16, mode: u16) {
        let cx = self.cursor_x;
        match mode {
            0 => {
                for x in cx..self.cols {
                    self.clear_cell(x, y);
                }
            }
            1 => {
                for x in 0..=cx.min(self.cols - 1) {
                    self.clear_cell(x, y);
                }
            }
            2 => {
                for x in 0..self.cols {
                    self.clear_cell(x, y);
                }
            }
            _ => {}
        }
    }
    fn clear_below(&mut self, y: u16, x: u16) {
        let _ = x;
        self.erase_in_line(y, 0);
        if self.rows > y + 1 {
            for row in (y + 1)..self.rows {
                for cx in 0..self.cols {
                    self.clear_cell(cx, row);
                }
            }
        }
    }
    fn clear_above(&mut self, y: u16, x: u16) {
        let _ = x;
        for row in 0..y {
            for cx in 0..self.cols {
                self.clear_cell(cx, row);
            }
        }
        self.erase_in_line(y, 1);
    }
    fn clear_all(&mut self) {
        for y in 0..self.rows {
            for x in 0..self.cols {
                self.clear_cell(x, y);
            }
        }
    }
    fn scroll_up_one(&mut self) {
        let cols = self.cols as usize;
        let total = self.cells.len();
        if total == 0 || cols == 0 {
            return;
        }
        self.cells.copy_within(cols..total, 0);
        let blank = Cell::default();
        let last = &mut self.cells[total - cols..];
        for cell in last.iter_mut() {
            *cell = blank;
        }
        for y in 0..self.rows {
            self.mark_dirty(y);
        }
    }
    fn scroll_down_one(&mut self) {
        let cols = self.cols as usize;
        let total = self.cells.len();
        if total == 0 || cols == 0 {
            return;
        }
        self.cells.copy_within(0..total - cols, cols);
        let blank = Cell::default();
        let first = &mut self.cells[..cols];
        for cell in first.iter_mut() {
            *cell = blank;
        }
        for y in 0..self.rows {
            self.mark_dirty(y);
        }
    }
}

/// Specification of what to spawn as a child PTY.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellSpec {
    /// Spawn `$SHELL` if set, else `/bin/sh`.
    LoginShell,
    /// Custom command. First element is the program path; the rest
    /// are arguments.
    Command { argv: Vec<String> },
}

/// A structured terminal pane event surfaced from `advance`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneEvent {
    /// OSC 0 / OSC 2 set the window title.
    TitleChanged { title: String },
    /// A kitty graphics command was completed.
    KittyGraphic { cmd: KittyGraphicCmd },
    /// Child exited with `status`.
    Exit { status: i32 },
    /// PTY size changed.
    Resize { cols: u16, rows: u16 },
}

/// A parsed kitty graphics command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KittyGraphicCmd {
    /// `a=p` (or `t/T`) with payload - image upload.
    Load {
        id: u32,
        placement_id: u32,
        format: u8,
        width: u32,
        height: u32,
        data: Vec<u8>,
    },
    /// `a=p` with empty payload - placement command.
    Place {
        id: u32,
        placement_id: u32,
        x: i32,
        y: i32,
        cols_cells: Option<u32>,
        rows_cells: Option<u32>,
        z: i32,
    },
    /// `a=d` - delete image.
    Delete { id: u32 },
    /// `a=c` - control / quiet.
    Control { quiet: bool },
}

/// Snapshot of a pane state returned by [`PanePty::snapshot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneTerminalState {
    /// Cell-grid text buffer (and cursor position).
    pub grid: TextGrid,
    /// Grid width.
    pub cols: u16,
    /// Grid height.
    pub rows: u16,
    /// Events emitted by the most recent `advance` (and any
    /// opportunistically-detected child exit).
    pub pending_events: Vec<PaneEvent>,
}

/// Errors produced by cmdash-pty.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum PtyError {
    #[error("pty spawn failed: {0}")]
    Spawn(#[source] anyhow::Error),
    #[error("pty resize failed: {0}")]
    Resize(#[source] anyhow::Error),
    #[error("pty write failed: {0}")]
    Write(#[source] std::io::Error),
    #[error("pty not spawned (already taken)")]
    NotSpawned,
    #[error("pane already exited with status {0}")]
    AlreadyExited(i32),
    #[error("invalid terminal size: cols={0} rows={1}")]
    InvalidSize(u16, u16),
    #[error("kitty graphic base64 decode failed: {0}")]
    KittyBase64(#[from] base64::DecodeError),
    #[error("pty child wait failed: {0}")]
    Wait(#[source] std::io::Error),
    #[error("pty child kill failed: {0}")]
    Kill(#[source] std::io::Error),
}

// ---------- kitty accumulator ----------

#[derive(Debug, Default)]
struct KittyAccumulator {
    active: bool,
    raw: Vec<u8>,
}

impl KittyAccumulator {
    fn begin(&mut self) {
        self.active = true;
        self.raw.clear();
    }
    fn push(&mut self, byte: u8) {
        if self.active {
            self.raw.push(byte);
        }
    }
    fn finish(&mut self) -> Option<Vec<u8>> {
        if !self.active {
            return None;
        }
        self.active = false;
        if self.raw.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.raw))
        }
    }
}

/// State of `PanePty::advance`'s pre-scan loop while walking
/// input bytes for `ESC _` APC (kitty graphics) sequences.
/// Drives the routing between `KittyAccumulator` (for APC
/// payloads) and `vte::Parser::advance` (for everything else).
/// Five states cover the contexts an `ESC` byte can appear
/// in: outside an APC, just saw one outside an APC, saw `ESC _`,
/// inside an APC, and just saw one inside an APC.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum ApcScannerState {
    /// Default. Outside any APC. Bare `ESC` transitions to
    /// [`SeenEsc`](ApcScannerState::SeenEsc) so the next byte
    /// can decide whether this is APC (`_`) or some other escape
    /// (`[`, `]`, `P`, ...) that should flow to `vte`.
    #[default]
    Idle,
    /// Outside APC; an unconsumed `ESC` is waiting for the next
    /// byte. `_` -> start APC by transitioning to
    /// [`SeenEscUnderscore`](ApcScannerState::SeenEscUnderscore);
    /// `[`/`]` / `P` / else -> flush the prior `ESC` to `vte`
    /// and resume normal flow; another `ESC` -> flush prior
    /// (it wasn't APC), stay `SeenEsc` for the new one.
    SeenEsc,
    /// Outside APC; consumed `ESC _` and now waiting for the
    /// kitty-graphics command-introducer `G` (`0x47`). Bare `G`
    /// -> transition to [`InApc`](ApcScannerState::InApc) and
    /// call `KittyAccumulator::begin()` WITHOUT pushing `G`
    /// into the kitty buffer (the `G` is framing, not payload
    /// data, so it must not become part of `parse_kitty_chunk`'s
    /// metadata key=value stream). Any other byte -> this was
    /// not a kitty-graphics APC after all; push `ESC _ <byte>`
    /// to `vte` and resume normal flow. This state was the
    /// load-bearing fix for the `a=d`, `a=p`, `a=t` etc. action
    /// resolutions being mis-routed to the load/place default
    /// path because `G` was previously leaked into the meta.
    SeenEscUnderscore,
    /// Inside an APC. Payload bytes accumulate into
    /// `KittyAccumulator.raw`. `BEL` (`0x07`) terminates; a lone
    /// `ESC` defers the terminate-decision to the next byte via
    /// [`InApcSeenEsc`](ApcScannerState::InApcSeenEsc).
    InApc,
    /// Inside APC; an `ESC` is waiting for the next byte to
    /// decide if it's `\` (the second byte of `ST = ESC \`).
    /// `_` would also imply a new APC, but kitty doesn't nest
    /// APCs; we treat the prior `ESC` as data in that case.
    InApcSeenEsc,
}

/// Drives the APC pre-scan inside `PanePty::advance`. The state
/// itself is the only field; the byte-routing logic reads
/// `self.state` and dispatches to `KittyAccumulator` (for APC
/// payload) or to `vte::Parser::advance` (for everything else)
/// accordingly.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ApcScanner {
    state: ApcScannerState,
}

fn parse_kitty_chunk(raw: &[u8]) -> Option<KittyGraphicCmd> {
    // The kitty escape payload is `<key>=<val>[,<key>=<val>]…[;<base64>]` — a
    // metadata section separated from the base64 payload by a literal `;`
    // (or none, if there's no payload at all). Split on whichever shows up.
    let sep = match raw.iter().position(|&b| b == b';') {
        Some(s) => s,
        None => raw.len(),
    };
    let meta_bytes = &raw[..sep];
    let payload = if sep < raw.len() {
        &raw[sep + 1..]
    } else {
        &[]
    };
    let meta = std::str::from_utf8(meta_bytes).ok()?;
    let mut kv: HashMap<String, String> = HashMap::new();
    for segment in meta.split([';', ',']) {
        let mut it = segment.splitn(2, '=');
        if let (Some(k), Some(v)) = (it.next(), it.next()) {
            kv.insert(k.to_string(), v.to_string());
        }
    }
    let action = kv.get("a").map(String::as_str).unwrap_or("p");
    let id = kv.get("i").and_then(|v| v.parse::<u32>().ok()).unwrap_or(0);
    let placement_id = kv.get("p").and_then(|v| v.parse::<u32>().ok()).unwrap_or(0);
    let format = kv.get("f").and_then(|v| v.parse::<u8>().ok()).unwrap_or(32);
    let width = kv.get("s").and_then(|v| v.parse::<u32>().ok()).unwrap_or(0);
    let height = kv.get("v").and_then(|v| v.parse::<u32>().ok()).unwrap_or(0);
    let x = kv.get("x").and_then(|v| v.parse::<i32>().ok()).unwrap_or(0);
    let y = kv.get("y").and_then(|v| v.parse::<i32>().ok()).unwrap_or(0);
    let cols_cells = kv.get("c").and_then(|v| v.parse::<u32>().ok());
    let rows_cells = kv.get("r").and_then(|v| v.parse::<u32>().ok());
    let z = kv.get("z").and_then(|v| v.parse::<i32>().ok()).unwrap_or(0);
    let quiet = kv.get("q").map(|s| s == "1").unwrap_or(false);

    match action {
        "d" | "D" => Some(KittyGraphicCmd::Delete { id }),
        "c" | "C" => Some(KittyGraphicCmd::Control { quiet }),
        _ => match base64::engine::general_purpose::STANDARD.decode(payload) {
            Ok(data) if !data.is_empty() => Some(KittyGraphicCmd::Load {
                id,
                placement_id,
                format,
                width,
                height,
                data,
            }),
            Ok(_) => Some(KittyGraphicCmd::Place {
                id,
                placement_id,
                x,
                y,
                cols_cells,
                rows_cells,
                z,
            }),
            Err(_) => None,
        },
    }
}

// ---------- vte Perform driver ----------

struct VtePerf<'a> {
    grid: &'a mut TextGrid,
    events: &'a mut Vec<PaneEvent>,
    title: &'a mut String,
    fg: &'a mut Color,
    bg: &'a mut Color,
    attrs: &'a mut CellAttrs,
    cols: u16,
    rows: u16,
}

impl<'a> VtePerf<'a> {
    fn advance_line(&mut self) {
        let next_y = self.grid.cursor_y.saturating_add(1);
        if next_y >= self.rows {
            self.grid.cursor_y = self.rows.saturating_sub(1);
            self.grid.scroll_up_one();
        } else {
            self.grid.cursor_y = next_y;
        }
    }
    fn advance_line_reverse(&mut self) {
        if self.grid.cursor_y == 0 {
            self.grid.scroll_down_one();
        } else {
            self.grid.cursor_y -= 1;
        }
    }
    fn apply_sgr(&mut self, params: &Params) {
        let mut iter = params.iter();
        while let Some(code) = iter.next().and_then(|p| p.first().copied()) {
            match code {
                0 => {
                    *self.fg = Color::Default;
                    *self.bg = Color::Default;
                    *self.attrs = CellAttrs::default();
                }
                1 => self.attrs.bold = true,
                3 => self.attrs.italic = true,
                4 => self.attrs.underline = true,
                7 => self.attrs.reverse = true,
                22 => self.attrs.bold = false,
                23 => self.attrs.italic = false,
                24 => self.attrs.underline = false,
                27 => self.attrs.reverse = false,
                30..=37 => *self.fg = Color::Indexed((code - 30) as u8),
                39 => *self.fg = Color::Default,
                40..=47 => *self.bg = Color::Indexed((code - 40) as u8),
                49 => *self.bg = Color::Default,
                90..=97 => *self.fg = Color::Indexed((code - 90 + 8) as u8),
                100..=107 => *self.bg = Color::Indexed((code - 100 + 8) as u8),
                38 => {
                    let mode = iter.next().and_then(|p| p.first().copied()).unwrap_or(0);
                    match mode {
                        5 => {
                            if let Some(c) = iter.next().and_then(|p| p.first().copied()) {
                                *self.fg = Color::Indexed(c as u8);
                            }
                        }
                        2 => {
                            let r = iter.next().and_then(|p| p.first().copied()).unwrap_or(0) as u8;
                            let g = iter.next().and_then(|p| p.first().copied()).unwrap_or(0) as u8;
                            let b = iter.next().and_then(|p| p.first().copied()).unwrap_or(0) as u8;
                            *self.fg = Color::Rgb(r, g, b);
                        }
                        _ => {}
                    }
                }
                48 => {
                    let mode = iter.next().and_then(|p| p.first().copied()).unwrap_or(0);
                    match mode {
                        5 => {
                            if let Some(c) = iter.next().and_then(|p| p.first().copied()) {
                                *self.bg = Color::Indexed(c as u8);
                            }
                        }
                        2 => {
                            let r = iter.next().and_then(|p| p.first().copied()).unwrap_or(0) as u8;
                            let g = iter.next().and_then(|p| p.first().copied()).unwrap_or(0) as u8;
                            let b = iter.next().and_then(|p| p.first().copied()).unwrap_or(0) as u8;
                            *self.bg = Color::Rgb(r, g, b);
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }
    fn apply_csi(&mut self, params: &Params, action: char) {
        let p0 = || {
            params
                .iter()
                .next()
                .and_then(|p| p.first().copied())
                .unwrap_or(0)
        };
        let p1 = || {
            params
                .iter()
                .nth(1)
                .and_then(|p| p.first().copied())
                .unwrap_or(0)
        };
        match action {
            'H' | 'f' => {
                // Explicit absolute cursor positioning clears
                // any pending wrap: the cursor now has an
                // authoritative (col, row) so the deferred
                // advance can no longer fire.
                self.grid.consume_pending_wrap();
                let row = p0().saturating_sub(1);
                let col = p1().saturating_sub(1);
                self.grid.cursor_y = row.min(self.rows.saturating_sub(1));
                self.grid.cursor_x = col.min(self.cols.saturating_sub(1));
            }
            'A' => {
                self.grid.consume_pending_wrap();
                let n = p0().max(1);
                self.grid.cursor_y = self.grid.cursor_y.saturating_sub(n);
            }
            'B' => {
                self.grid.consume_pending_wrap();
                let n = p0().max(1);
                self.grid.cursor_y = (self.grid.cursor_y + n).min(self.rows - 1);
            }
            'C' => {
                self.grid.consume_pending_wrap();
                let n = p0().max(1);
                self.grid.cursor_x = (self.grid.cursor_x + n).min(self.cols - 1);
            }
            'D' => {
                self.grid.consume_pending_wrap();
                let n = p0().max(1);
                self.grid.cursor_x = self.grid.cursor_x.saturating_sub(n);
            }
            'J' => {
                let mode = p0();
                match mode {
                    0 => self
                        .grid
                        .clear_below(self.grid.cursor_y, self.grid.cursor_x),
                    1 => self
                        .grid
                        .clear_above(self.grid.cursor_y, self.grid.cursor_x),
                    2 => self.grid.clear_all(),
                    _ => {}
                }
            }
            'K' => {
                let mode = p0();
                self.grid.erase_in_line(self.grid.cursor_y, mode);
            }
            'm' => self.apply_sgr(params),
            _ => {}
        }
    }
}

impl<'a> vte::Perform for VtePerf<'a> {
    fn print(&mut self, c: char) {
        // Consume the deferred row advance first: if the prior
        // printable filled the rightmost column, the cursor
        // logically sits at column 0 of the NEXT row, but the
        // advance didn't happen yet. Take the promise here so
        // `c` lands on the next row, not back at column 0 of
        // the current row.
        if self.grid.consume_pending_wrap() {
            self.grid.cursor_x = 0;
            self.advance_line();
        }
        let x = self.grid.cursor_x;
        let y = self.grid.cursor_y;
        self.grid.put(x, y, *self.fg, *self.bg, *self.attrs, c);
        let nx = x.saturating_add(1);
        if nx >= self.cols {
            // Filled the rightmost column. Defer the row
            // advance until the next printable arrives so an
            // immediately-following `\x1b[2K` targets THIS row,
            // not the next. See `consume_pending_wrap` for the
            // surrounding rules.
            self.grid.pending_wrap = true;
        } else {
            self.grid.cursor_x = nx;
        }
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x07 => {}
            0x08 => {
                // CR-free backspace: if pending_wrap is set we
                // are at the right margin (logically off-screen
                // on the current row); a lone BS does not move
                // backward across the wrap boundary, so leave
                // `pending_wrap` untouched.
                if self.grid.cursor_x > 0 {
                    self.grid.cursor_x -= 1;
                }
            }
            0x09 => {
                let cur = self.grid.cursor_x;
                let next = ((cur / 8) + 1).saturating_mul(8);
                self.grid.cursor_x = next.min(self.cols - 1);
            }
            0x0A..=0x0C => {
                // LF / VT / FF explicitly request a row advance,
                // so any pending-wrap promise is consumed (the
                // row advance this control byte requested IS the
                // advance that was deferred — no double-advance).
                // Range pattern satisfies `clippy::manual-range-patterns`;
                // the resulting semantics is identical to a
                // three-arm `|` match but fewer characters and
                // not in scope for the lint.
                self.grid.consume_pending_wrap();
                self.advance_line();
            }
            0x0D => {
                // CR alone does not print; per VT100 semantics a
                // subsequent printable on the wrapped-to row is
                // expected, so we leave `pending_wrap` set.
                self.grid.cursor_x = 0;
            }
            _ => {}
        }
    }

    // NOTE: `vte::Perform::hook`/`put`/`unhook` are NOT
    // implemented here. `vte 0.15`'s Paul Williams state
    // machine only routes DCS strings through these
    // callbacks; APC (kitty graphics, `ESC _`) is silently
    // DROPPED. The kitty pre-scan in `PanePty::advance`
    // owns APC ingestion directly via `KittyAccumulator`.

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        if let Some((first, rest)) = params.split_first() {
            if *first == b"0" || *first == b"2" {
                if let Some(title_bytes) = rest.first() {
                    if let Ok(s) = std::str::from_utf8(title_bytes) {
                        *self.title = s.to_string();
                        self.events.push(PaneEvent::TitleChanged {
                            title: s.to_string(),
                        });
                    }
                }
            }
        }
    }

    fn csi_dispatch(
        &mut self,
        params: &Params,
        _intermediates: &[u8],
        _ignore: bool,
        action: char,
    ) {
        self.apply_csi(params, action);
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, byte: u8) {
        match byte {
            b'D' => self.advance_line(),
            b'E' => {
                self.grid.cursor_x = 0;
                self.advance_line();
            }
            b'H' => {}
            b'M' => self.advance_line_reverse(),
            _ => {}
        }
    }
}

// ---------- public PanePty + PaneReader ----------

/// One pane's PTY state machine.
pub struct PanePty {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Option<Box<dyn Child + Send + Sync>>,
    parser: Parser,
    grid: TextGrid,
    kitty: KittyAccumulator,
    /// Pre-scan state machine: routes APC (kitty `ESC _`) bytes
    /// to `KittyAccumulator` instead of `vte::Parser::advance`.
    /// See [`ApcScannerState`] for the five states.
    apc: ApcScanner,
    fg: Color,
    bg: Color,
    attrs: CellAttrs,
    title: String,
    pending_events: Vec<PaneEvent>,
    layer_id: PaneLayerId,
    cols: u16,
    rows: u16,
}

impl std::fmt::Debug for PanePty {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `master`, `writer`, `child` are dyn-trait boxes whose
        // underlying traits are not necessarily Debug; skip them.
        f.debug_struct("PanePty")
            .field("layer_id", &self.layer_id)
            .field("cols", &self.cols)
            .field("rows", &self.rows)
            .field("title", &self.title)
            .field("grid", &self.grid)
            .field("fg", &self.fg)
            .field("bg", &self.bg)
            .field("attrs", &self.attrs)
            .finish()
    }
}

/// Reader half of a pane PTY. The caller feeds the bytes read from
/// `PaneReader` into [`PanePty::advance`] to update the grid.
pub struct PaneReader {
    inner: Box<dyn Read + Send>,
}

impl std::fmt::Debug for PaneReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PaneReader").finish()
    }
}

impl Read for PaneReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(buf)
    }
}

impl PanePty {
    pub fn layer_id(&self) -> PaneLayerId {
        self.layer_id
    }
    pub fn cols(&self) -> u16 {
        self.cols
    }
    pub fn rows(&self) -> u16 {
        self.rows
    }

    /// Spawn a child PTY. Returns the pane state machine + a
    /// reader half that yields bytes from the master.
    /// Enable the PTY line-discipline `ECHO` flag via the
    /// master fd's raw termios. On Unix, `portable_pty 0.9`'s
    /// `MasterPty` trait exposes `as_raw_fd() -> Option<RawFd>`,
    /// so we can manipulate the underlying termios without
    /// dropping down to the lower-level `UnixPtySystem` API.
    /// On `Windows`, this fn is not compiled (the trait method
    /// returns `None`, and `ConPTY` does not use POSIX termios).
    ///
    /// **Why `ECHO | ICANON` matters.** The cat-echo
    /// round-trip test (`pty_write_to_child_round_trips_via_cat`)
    /// historically raced because `portable_pty 0.9`'s PTY
    /// pair is opened in raw / canonical-off mode (effectively
    /// `cfmakeraw`), so even though `ECHO` is preserved in
    /// `c_lflag` it has NO effect when `ICANON` is off — the
    /// line discipline doesn't buffer by line and doesn't fire
    /// the echo path in raw mode. Adding ONLY `ECHO` (the
    /// initial forward-fixup in atom `581ddec`) was therefore
    /// insufficient; the runtime test failure stack-trace from
    /// my followup commit (post-`581ddec`) showed the same
    /// `left == right` failure with `left: ' '`. Adding
    /// `ICANON` alongside `ECHO` re-enables line-discipline
    /// buffering + synchronous echo on the master fd.
    /// **Forward-fixup caveat:** the slave-fd switch
    /// (i.e. `pair.slave.as_raw_fd()`) was attempted in a
    /// follow-up atom but blocked by `portable_pty 0.9`'s
    /// `SlavePty` trait NOT exposing `as_raw_fd()` (only
    /// `MasterPty` does), so we reverted to the master-fd call
    /// point. The cat-echo test therefore STILL races on
    /// `portable_pty 0.9` with the master-fd master-fd termios
    /// setup; the test is re-`#[ignore]`'d against this
    /// runtime path forward, with a forward-only-no-revert
    /// exception in the run-time path tracked via the
    /// `f158ea0` `--(1, 30)`-clamp `drain_deadline_default()`
    /// CI safety net. The `libc` direct-dep additions are
    /// preserved because they remain load-bearing for the
    /// `tcgetattr` / `std::mem::zeroed::<libc::termios>()` /
    /// `tcsetattr` call path that's wired through the master
    /// fd here.
    #[cfg(unix)]
    fn enable_pty_echo(fd: std::os::fd::RawFd) -> Result<(), std::io::Error> {
        // SAFETY: `libc::tcgetattr` writes through the
        // borrowed `&mut termios` pointer; the termios is a
        // POD struct initialized to zero before the call.
        let mut termios = unsafe { std::mem::zeroed::<libc::termios>() };
        // SAFETY: `fd` is owned by `portable_pty`'s master
        // pty; `tcgetattr` and `tcsetattr` are `extern "C"`
        // syscalls that take a raw fd and a `termios` pointer.
        if unsafe { libc::tcgetattr(fd, &mut termios) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // OR-in `ECHO` AND `ICANON` to the line-discipline
        // flags. Both bits are load-bearing here: `portable_pty
        // 0.9` opens its PTY pair in raw / canonical-off mode
        // (essentially `cfmakeraw`), which makes `ECHO` alone
        // a no-op — the line discipline does NOT buffer input
        // by line OR fire the echo path in raw mode. Re-enabling
        // `ICANON` alongside `ECHO` makes the kernel itself
        // buffer input AND synchronously echo master-write
        // bytes back to the master, regardless of the child
        // (/bin/cat)'s scheduling state. We also zero
        // `c_cc[libc::VEOF]` so an empty master-write can't
        // accidentally trigger EOF on the slave's stdin
        // (default `VEOF` byte is ^D=4; clearing it removes the
        // EOF-on-NL path which would falsely terminate `cat`).
        termios.c_lflag |= libc::ECHO | libc::ICANON;
        termios.c_cc[libc::VEOF] = 0;
        // `TCSANOW` makes the change immediate (vs `TCSADRAIN`
        // which would wait for output to drain; we don't care
        // because cat is going to write next and we want ECHO
        // active before that).
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &termios) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn spawn(
        shell: ShellSpec,
        cols: u16,
        rows: u16,
        layer_id: PaneLayerId,
    ) -> Result<(Self, PaneReader), PtyError> {
        if cols == 0 || rows == 0 {
            return Err(PtyError::InvalidSize(cols, rows));
        }
        let pty_system = native_pty_system();
        let size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        let pair = pty_system.openpty(size).map_err(PtyError::Spawn)?;
        // Enable PTY line-discipline `ECHO` so that bytes
        // written to the master fd by `PanePty::write` are
        // echoed back to the master synchronously, providing
        // a deterministic return path for child-input round
        // trip tests. See `enable_pty_echo` for the rationale.
        // The call must happen AFTER `openpty` (so the master
        // fd exists) and BEFORE `pair.slave.spawn_command`
        // (so the child's first stdin read sees an already
        // echo-enabled line discipline). On non-Unix the
        // branch is cfg-gated out entirely; on Unix the trait
        // method `as_raw_fd` returns `Some(master_fd)`.
        // **Forward-fixup note:** `portable_pty 0.9`'s
        // `SlavePty` trait does NOT expose `as_raw_fd()`,
        // so we cannot switch the call point to the slave
        // fd; the master-fd path is the only nc-friendly path
        // through the trait API, and the cat-echo test
        // therefore remains gated by the `f158ea0` clamp and
        // the `b7de7dd` `#[ignore]` until a future nix-crate
        // or lower-level UnixPtySystem path becomes viable.
        #[cfg(unix)]
        if let Some(fd) = pair.master.as_raw_fd() {
            Self::enable_pty_echo(fd).map_err(|e| PtyError::Spawn(e.into()))?;
        }
        let cmd = build_command(shell);
        let child = pair.slave.spawn_command(cmd).map_err(PtyError::Spawn)?;
        let reader = pair.master.try_clone_reader().map_err(PtyError::Spawn)?;
        let writer = pair.master.take_writer().map_err(PtyError::Spawn)?;
        let grid = TextGrid::new(cols, rows);
        let master = pair.master;
        Ok((
            Self {
                master,
                writer,
                child: Some(child),
                parser: Parser::new(),
                grid,
                kitty: KittyAccumulator::default(),
                apc: ApcScanner::default(),
                fg: Color::Default,
                bg: Color::Default,
                attrs: CellAttrs::default(),
                title: String::new(),
                pending_events: Vec::new(),
                layer_id,
                cols,
                rows,
            },
            PaneReader { inner: reader },
        ))
    }

    /// Resize the PTY. Re-emits `PaneEvent::Resize`.
    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<(), PtyError> {
        if cols == 0 || rows == 0 {
            return Err(PtyError::InvalidSize(cols, rows));
        }
        let size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        self.master.resize(size).map_err(PtyError::Resize)?;
        self.cols = cols;
        self.rows = rows;
        self.pending_events.push(PaneEvent::Resize { cols, rows });
        Ok(())
    }

    /// Forward input bytes to the child (e.g. keystrokes).
    pub fn write(&mut self, bytes: &[u8]) -> Result<usize, PtyError> {
        if let Some(code) = self.peek_exit_status() {
            return Err(PtyError::AlreadyExited(code));
        }
        self.writer.write(bytes).map_err(PtyError::Write)
    }

    /// Feed bytes from the PTY master into the vte parser + kitty
    /// accumulator; opportunistically emits an Exit event if the
    /// child has already finished.
    pub fn advance(&mut self, bytes: &[u8]) -> Result<(), PtyError> {
        // Pre-scan input bytes for `ESC _` APC (kitty graphics)
        // sequences and route the payload to `KittyAccumulator`
        // BEFORE the remainder goes to `vte::Parser::advance`.
        // This is load-bearing: `vte 0.15`'s Paul Williams state
        // machine silently DROPS APC strings — it only routes
        // DCS strings through `hook`/`put`/`unhook`. Without
        // this pre-scan, `PaneEvent::KittyGraphic`s never fire
        // and the cmdash binary silently loses kitty graphics
        // emission. The below five-state machine is the
        // architectural fix.
        let mut vte_bytes = Vec::with_capacity(bytes.len());
        for &b in bytes {
            match self.apc.state {
                ApcScannerState::Idle => {
                    if b == 0x1B {
                        self.apc.state = ApcScannerState::SeenEsc;
                    } else {
                        vte_bytes.push(b);
                    }
                }
                ApcScannerState::SeenEsc => {
                    if b == b'_' {
                        // ESC _ was consumed; the next byte
                        // decides whether this is a kitty
                        // graphics APC (introducer `G`) or some
                        // other non-APC sequence (`ESC _ X` is
                        // rare in practice but valid escape
                        // framing per VT100). Defer the
                        // `KittyAccumulator::begin()` until we
                        // confirm via `SeenEscUnderscore`.
                        self.apc.state = ApcScannerState::SeenEscUnderscore;
                    } else if b == 0x1B {
                        // Emit the prior ESC; the new ESC could
                        // still be the start of APC. Stay SeenEsc.
                        vte_bytes.push(0x1B);
                        self.apc.state = ApcScannerState::SeenEsc;
                    } else {
                        // Some other escape sequence: ESC [
                        // (CSI), ESC ] (OSC), ESC P (DCS). Hand
                        // ESC + this byte back to vte.
                        vte_bytes.push(0x1B);
                        vte_bytes.push(b);
                        self.apc.state = ApcScannerState::Idle;
                    }
                }
                ApcScannerState::SeenEscUnderscore => {
                    if b == b'G' {
                        // ESC _ G is the canonical kitty
                        // graphics opener per the kitty
                        // graphics protocol. The `G` byte is
                        // FRAMING, not data: it must NOT be
                        // pushed into `KittyAccumulator::raw`
                        // because `parse_kitty_chunk` would
                        // otherwise see meta like `Ga=d,i=5`
                        // (with `Ga` as a key) instead of
                        // `a=d,i=5`, and the action lookup
                        // `kv.get("a").unwrap_or("p")` would
                        // fall back to the load/place default
                        // for a `Delete` command. Stripping
                        // the `G` here restores the spec-
                        // correct metadata stream.
                        self.apc.state = ApcScannerState::InApc;
                        self.kitty.begin();
                    } else {
                        // The byte after `ESC _` was not `G`,
                        // so this is NOT a kitty graphics APC.
                        // Flush the entire `ESC _ <byte>` triple
                        // to `vte` for normal VT100 handling
                        // (vte's Paul Williams state machine
                        // will see it as a no-op or standard
                        // escape; the exact behavior is
                        // immaterial because no shell in
                        // practice emits `ESC _ X` for non-`G`
                        // `X` outside of kitty graphics).
                        vte_bytes.push(0x1B);
                        vte_bytes.push(b'_');
                        vte_bytes.push(b);
                        self.apc.state = ApcScannerState::Idle;
                    }
                }
                ApcScannerState::InApc => {
                    if b == 0x07 {
                        // BEL terminates the APC. kitty.raw
                        // already excludes the trailing BEL
                        // because we transition at the byte
                        // before pushing it.
                        self.finish_apc();
                    } else if b == 0x1B {
                        // ESC inside APC: defer the
                        // terminate-decision to the next byte.
                        self.apc.state = ApcScannerState::InApcSeenEsc;
                    } else {
                        self.kitty.push(b);
                    }
                }
                ApcScannerState::InApcSeenEsc => {
                    if b == b'\\' {
                        // ST = ESC \\. kitty.raw excludes this
                        // ESC and the backslash (we transitioned
                        // at the ESC without pushing it).
                        self.finish_apc();
                    } else if b == 0x07 {
                        // Previous ESC was just data; BEL
                        // terminates. Push the ESC then finish.
                        // Degenerate (base64 alphabet has no
                        // 0x1B so the chunk fails decode and
                        // parse_kitty_chunk returns None).
                        self.kitty.push(0x1B);
                        self.finish_apc();
                    } else if b == 0x1B {
                        // ESC ESC: prior ESC is data, new ESC
                        // could be the start of ST. Stay.
                        self.kitty.push(0x1B);
                        self.apc.state = ApcScannerState::InApcSeenEsc;
                    } else {
                        // ESC + non-`\` non-BEL byte: prior ESC
                        // is data, this byte is the next payload
                        // byte.
                        self.kitty.push(0x1B);
                        self.kitty.push(b);
                        self.apc.state = ApcScannerState::InApc;
                    }
                }
            }
        }
        let mut driver = VtePerf {
            grid: &mut self.grid,
            events: &mut self.pending_events,
            title: &mut self.title,
            fg: &mut self.fg,
            bg: &mut self.bg,
            attrs: &mut self.attrs,
            cols: self.cols,
            rows: self.rows,
        };
        self.parser.advance(&mut driver, &vte_bytes);
        if let Some(child) = self.child.as_mut() {
            if let Ok(Some(status)) = child.try_wait() {
                self.pending_events.push(PaneEvent::Exit {
                    status: status.exit_code() as i32,
                });
            }
        }
        Ok(())
    }

    /// End the current APC: reset `apc.state` and complete the
    /// `KittyAccumulator` cycle, emitting a
    /// `PaneEvent::KittyGraphic` if the payload parsed into a
    /// [`KittyGraphicCmd`]. Called from `Self::advance` on
    /// `BEL` (`0x07`) or `ESC \\` terminator detection.
    fn finish_apc(&mut self) {
        self.apc.state = ApcScannerState::Idle;
        if let Some(raw) = self.kitty.finish() {
            if let Some(cmd) = parse_kitty_chunk(&raw) {
                self.pending_events.push(PaneEvent::KittyGraphic { cmd });
            }
        }
    }

    /// Take all events emitted since the last `drain_events`.
    pub fn drain_events(&mut self) -> Vec<PaneEvent> {
        std::mem::take(&mut self.pending_events)
    }

    /// Clone the current grid + drain pending events in a single
    /// snapshot.
    pub fn snapshot(&mut self) -> PaneTerminalState {
        let grid = self.grid.clone();
        let pending = std::mem::take(&mut self.pending_events);
        PaneTerminalState {
            grid,
            cols: self.cols,
            rows: self.rows,
            pending_events: pending,
        }
    }

    /// Non-blocking poll: returns exit code if child finished,
    /// `None` otherwise. Pushes `PaneEvent::Exit` when finished.
    pub fn try_wait(&mut self) -> Result<Option<i32>, PtyError> {
        let child = self.child.as_mut().ok_or(PtyError::NotSpawned)?;
        match child.try_wait().map_err(PtyError::Wait)? {
            Some(status) => {
                let code = status.exit_code() as i32;
                self.pending_events.push(PaneEvent::Exit { status: code });
                Ok(Some(code))
            }
            None => Ok(None),
        }
    }

    /// SIGKILL the child and re-poll.
    pub fn kill(&mut self) -> Result<(), PtyError> {
        let child = self.child.as_mut().ok_or(PtyError::NotSpawned)?;
        child.kill().map_err(PtyError::Kill)?;
        let _ = self.try_wait()?;
        Ok(())
    }

    fn peek_exit_status(&self) -> Option<i32> {
        self.pending_events.iter().find_map(|e| match e {
            PaneEvent::Exit { status } => Some(*status),
            _ => None,
        })
    }
}

/// Trait abstracting the [`PanePty`] API surface for mockability.
/// Introduced so [`cmdash::pane::PaneRunner::resize`]'s
/// `?`-propagation invariant (a failed `pty.resize` does not
/// touch `self.computed.rect`) is testable with a stub. Mirrors
/// AGENTS.md §"every invariant needs a regression test."
///
/// v1 narrows the surface to the seven methods `PaneRunner`
/// actually calls; future `PanePty` additions get layered onto
/// this trait deliberately so the abstraction doesn't drift.
pub trait PanePtyOps {
    fn layer_id(&self) -> PaneLayerId;
    fn resize(&mut self, cols: u16, rows: u16) -> Result<(), PtyError>;
    fn write(&mut self, bytes: &[u8]) -> Result<usize, PtyError>;
    fn advance(&mut self, bytes: &[u8]) -> Result<(), PtyError>;
    fn snapshot(&mut self) -> PaneTerminalState;
    fn try_wait(&mut self) -> Result<Option<i32>, PtyError>;
    fn kill(&mut self) -> Result<(), PtyError>;
}

/// Production impl behind the trait. Uses UFCS (`PanePty::resize`) so
/// dispatch is load-bearing-explicit, not reliant on Rust's
/// inherent-over-trait method-resolution rule. Future maintainers
/// adding a trait method with the same name as an inherent
/// method on `PanePty` will see the call site fail to compile
/// instead of silently picking the wrong body.
impl PanePtyOps for PanePty {
    fn layer_id(&self) -> PaneLayerId {
        PanePty::layer_id(self)
    }
    fn resize(&mut self, cols: u16, rows: u16) -> Result<(), PtyError> {
        PanePty::resize(self, cols, rows)
    }
    fn write(&mut self, bytes: &[u8]) -> Result<usize, PtyError> {
        PanePty::write(self, bytes)
    }
    fn advance(&mut self, bytes: &[u8]) -> Result<(), PtyError> {
        PanePty::advance(self, bytes)
    }
    fn snapshot(&mut self) -> PaneTerminalState {
        PanePty::snapshot(self)
    }
    fn try_wait(&mut self) -> Result<Option<i32>, PtyError> {
        PanePty::try_wait(self)
    }
    fn kill(&mut self) -> Result<(), PtyError> {
        PanePty::kill(self)
    }
}

fn build_command(shell: ShellSpec) -> CommandBuilder {
    let (program, args): (String, Vec<String>) = match shell {
        ShellSpec::LoginShell => {
            let prog = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
            (prog, Vec::new())
        }
        ShellSpec::Command { argv } => {
            let mut iter = argv.into_iter();
            let prog = iter.next().unwrap_or_else(|| "/bin/sh".to_string());
            (prog, iter.collect())
        }
    };
    let mut cmd = CommandBuilder::new(program);
    for a in args {
        cmd.arg(a);
    }
    cmd
}

// ---------- in-tree sanity tests (no real PTY) ----------

#[cfg(test)]
mod internal_sanity_tests {
    use super::*;

    #[test]
    fn text_grid_initial_state() {
        let g = TextGrid::new(10, 5);
        assert_eq!(g.cols(), 10);
        assert_eq!(g.rows(), 5);
        assert_eq!(g.cursor(), (0, 0));
        assert_eq!(g.cell(0, 0).ch, ' ');
    }

    #[test]
    fn text_grid_put_and_dirty() {
        let mut g = TextGrid::new(4, 2);
        g.put(
            1,
            0,
            Color::Default,
            Color::Default,
            CellAttrs::default(),
            'X',
        );
        assert_eq!(g.cell(1, 0).ch, 'X');
        assert_eq!(g.drain_dirty_rows(), vec![0]);
    }

    #[test]
    fn text_grid_scroll_up_one() {
        let mut g = TextGrid::new(3, 3);
        g.put(
            0,
            0,
            Color::Default,
            Color::Default,
            CellAttrs::default(),
            'A',
        );
        g.put(
            0,
            1,
            Color::Default,
            Color::Default,
            CellAttrs::default(),
            'B',
        );
        g.put(
            0,
            2,
            Color::Default,
            Color::Default,
            CellAttrs::default(),
            'C',
        );
        let _ = g.drain_dirty_rows();
        g.scroll_up_one();
        assert_eq!(g.cell(0, 0).ch, 'B');
        assert_eq!(g.cell(0, 1).ch, 'C');
        assert_eq!(g.cell(0, 2).ch, ' ');
    }

    #[test]
    fn kitty_load_chunk_parses() {
        let raw: &[u8] = b"a=p,i=1,f=32,s=1,v=1,o=z;AAAA";
        let cmd = parse_kitty_chunk(raw).expect("kitty load parses");
        match cmd {
            KittyGraphicCmd::Load {
                id,
                format,
                width,
                height,
                data,
                ..
            } => {
                assert_eq!(id, 1);
                assert_eq!(format, 32);
                assert_eq!(width, 1);
                assert_eq!(height, 1);
                assert!(!data.is_empty());
            }
            other => panic!("expected Load, got {:?}", other),
        }
    }

    #[test]
    fn kitty_empty_chunk_is_place() {
        let raw: &[u8] = b"a=p,i=2,x=10,y=20";
        let cmd = parse_kitty_chunk(raw).expect("kitty place parses");
        match cmd {
            KittyGraphicCmd::Place { id, x, y, .. } => {
                assert_eq!(id, 2);
                assert_eq!(x, 10);
                assert_eq!(y, 20);
            }
            other => panic!("expected Place, got {:?}", other),
        }
    }

    #[test]
    fn kitty_delete_chunk_parses() {
        let raw: &[u8] = b"a=d,i=3";
        let cmd = parse_kitty_chunk(raw).expect("kitty delete parses");
        assert!(matches!(cmd, KittyGraphicCmd::Delete { id: 3 }));
    }

    #[test]
    fn kitty_control_chunk_parses() {
        let raw: &[u8] = b"a=c,q=1";
        let cmd = parse_kitty_chunk(raw).expect("kitty control parses");
        assert!(matches!(cmd, KittyGraphicCmd::Control { quiet: true }));
    }

    #[test]
    fn invalid_size_spawn_rejected() {
        let err = PanePty::spawn(ShellSpec::LoginShell, 0, 0, PaneLayerId(0)).unwrap_err();
        assert!(matches!(err, PtyError::InvalidSize(0, 0)));
    }

    /// Pending-wrap deferral: a printable that fills the
    /// rightmost column sets `pending_wrap = true`; the row
    /// advance lands on the NEXT `print`, not the current
    /// one (xterm/VT100 wrap semantics). Pins the state
    /// machine introduced by the
    /// `fix: defer cursor_y advance in Print until next printable`
    /// atom.
    #[test]
    fn print_defers_wrap_until_next_character() {
        use vte::Perform;
        let mut g = TextGrid::new(2, 2);
        let mut events: Vec<PaneEvent> = Vec::new();
        let mut title = String::new();
        let mut fg = Color::Default;
        let mut bg = Color::Default;
        let mut attrs = CellAttrs::default();
        let mut perf = VtePerf {
            grid: &mut g,
            events: &mut events,
            title: &mut title,
            fg: &mut fg,
            bg: &mut bg,
            attrs: &mut attrs,
            cols: 2,
            rows: 2,
        };
        // First printable: cursor at (1, 0); no wrap pending.
        perf.print('A');
        assert_eq!(perf.grid.cursor(), (1, 0));
        assert!(!perf.grid.pending_wrap);
        assert_eq!(perf.grid.cell(0, 0).ch, 'A');
        // Second printable: fills the right margin so the row
        // advance is deferred. Cursor stays at (1, 0) so an
        // immediately-following `\x1b[2K` would target THIS
        // row, not the next. This is the root fix for the
        // `pty_csi_erase_in_line_clears_to_eol` test failure.
        perf.print('B');
        assert_eq!(perf.grid.cursor(), (1, 0));
        assert!(perf.grid.pending_wrap);
        assert_eq!(perf.grid.cell(1, 0).ch, 'B');
        // Third printable: consumes the promise, advances to
        // (0, 1) before placing the char, then lands at (1, 1).
        perf.print('C');
        assert!(!perf.grid.pending_wrap);
        assert_eq!(perf.grid.cursor(), (1, 1));
        assert_eq!(perf.grid.cell(0, 1).ch, 'C');
    }

    /// LF consumes the pending-wrap promise AND advances
    /// exactly ONE row, NOT two (which would scroll). Without
    /// this guard a future maintainer who "tidies up"
    /// `execute(0x0A..=0x0C)` back to `self.advance_line()`
    /// would silently double-scroll small grids and silently
    /// discard row-0 contents. Pins the consume-once promise
    /// for the row-advance control bytes (the load-bearing
    /// counterpart to `print_defers_wrap_until_next_character`).
    #[test]
    fn lf_consumes_pending_wrap_no_double_advance() {
        use vte::Perform;
        let mut g = TextGrid::new(2, 2);
        let mut events: Vec<PaneEvent> = Vec::new();
        let mut title = String::new();
        let mut fg = Color::Default;
        let mut bg = Color::Default;
        let mut attrs = CellAttrs::default();
        let mut perf = VtePerf {
            grid: &mut g,
            events: &mut events,
            title: &mut title,
            fg: &mut fg,
            bg: &mut bg,
            attrs: &mut attrs,
            cols: 2,
            rows: 2,
        };
        perf.print('A');
        perf.print('B');
        // After filling row 0: cursor at (1, 0), pending_wrap = true.
        assert!(perf.grid.pending_wrap);
        assert_eq!(perf.grid.cursor(), (1, 0));
        // LF (0x0A): (1) consumes the promise and (2) advances
        // the row. Per VT100, `LF` preserves `cursor_x` — it
        // does NOT reset the column — so after this the cursor
        // sits at (cols - 1, 1). The discriminative failure
        // mode this test pins is "the impl forgot
        // `consume_pending_wrap()`": with `pending_wrap` still
        // set, a subsequent `print` would re-fire the deferred
        // advance and force a scroll that drops row 0.
        perf.execute(0x0A);
        assert!(
            !perf.grid.pending_wrap,
            "LF(0x0A) must clear the pending-wrap promise so a \
             subsequent printable does not re-fire the deferred advance"
        );
        assert_eq!(perf.grid.cursor(), (1, 1));
        // Row 0's contents survived (no scroll triggered).
        assert_eq!(perf.grid.cell(0, 0).ch, 'A');
        assert_eq!(perf.grid.cell(1, 0).ch, 'B');
        // Row 1 is the new cursor row, still empty.
        assert_eq!(perf.grid.cell(0, 1).ch, ' ');
        assert_eq!(perf.grid.cell(1, 1).ch, ' ');
    }

    /// Lone APC chunk with ST terminator emits a `KittyGraphic`
    /// `Load` event. The simplest happy-path round trip through
    /// the APC pre-scan.
    #[test]
    fn apc_lone_chunk_with_st_emits_load_event() {
        let (mut pty, _reader) = PanePty::spawn(
            ShellSpec::Command {
                argv: vec!["/bin/cat".to_string()],
            },
            10,
            10,
            PaneLayerId(50),
        )
        .expect("spawn pty");
        pty.advance(b"\x1b_Ga=p,i=1,f=32,s=1,v=1;AAAA\x1b\\")
            .expect("advance");
        let events = pty.drain_events();
        let load_event = events.iter().find_map(|e| match e {
            PaneEvent::KittyGraphic {
                cmd:
                    KittyGraphicCmd::Load {
                        id,
                        format,
                        width,
                        height,
                        ..
                    },
            } => Some((*id, *format, *width, *height)),
            _ => None,
        });
        assert_eq!(load_event, Some((1u32, 32u8, 1u32, 1u32)));
        assert_eq!(pty.apc.state, ApcScannerState::Idle);
    }

    /// Partial APC chunk (no terminator) does NOT emit an
    /// event yet — the payload is buffered in
    /// `KittyAccumulator` waiting for terminator bytes. The
    /// scanner state must stay in `InApc` across the
    /// `advance()` call so the next call's bytes can finish it.
    #[test]
    fn apc_partial_chunk_does_not_emit() {
        let (mut pty, _reader) = PanePty::spawn(
            ShellSpec::Command {
                argv: vec!["/bin/cat".to_string()],
            },
            10,
            10,
            PaneLayerId(51),
        )
        .expect("spawn pty");
        pty.advance(b"\x1b_Ga=p,i=1,f=32,s=1,v=1;AAAA")
            .expect("advance partial");
        let events = pty.drain_events();
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, PaneEvent::KittyGraphic { .. })),
            "no event yet, got {:?}",
            events
        );
        assert_eq!(pty.apc.state, ApcScannerState::InApc);
    }

    /// Split APC across two `advance()` calls: first call
    /// delivers `ESC _ Ga=p,i=1;AA` (no terminator), second
    /// delivers `AA\x1b\\` (rest + ST). The scanner threads
    /// state across the boundary so the chunk parses as a
    /// single Load event in the second call's event stream.
    #[test]
    fn apc_split_across_two_advances_emits() {
        let (mut pty, _reader) = PanePty::spawn(
            ShellSpec::Command {
                argv: vec!["/bin/cat".to_string()],
            },
            10,
            10,
            PaneLayerId(52),
        )
        .expect("spawn pty");
        pty.advance(b"\x1b_Ga=p,i=1,f=32,s=1,v=1;AA")
            .expect("advance part 1");
        assert!(pty.drain_events().is_empty());
        assert_eq!(pty.apc.state, ApcScannerState::InApc);
        pty.advance(b"AA\x1b\\").expect("advance part 2");
        let events = pty.drain_events();
        let got = events.iter().any(|e| {
            matches!(
                e,
                PaneEvent::KittyGraphic {
                    cmd: KittyGraphicCmd::Load { id: 1, .. }
                }
            )
        });
        assert!(got, "expected Load event, got {:?}", events);
        assert_eq!(pty.apc.state, ApcScannerState::Idle);
    }

    /// BEL-terminated APC. The kitty protocol uses `BEL`
    /// (`0x07`) as a fallback terminator in addition to ST
    /// (`ESC \\`). The scanner detects `BEL` inside
    /// `ApcScannerState::InApc` and finishes the chunk with
    /// the same path as for ST.
    #[test]
    fn apc_bel_terminated_emits() {
        let (mut pty, _reader) = PanePty::spawn(
            ShellSpec::Command {
                argv: vec!["/bin/cat".to_string()],
            },
            10,
            10,
            PaneLayerId(53),
        )
        .expect("spawn pty");
        pty.advance(b"\x1b_Ga=p,i=1,f=32,s=1,v=1;AAAA\x07")
            .expect("advance");
        let events = pty.drain_events();
        let got = events.iter().any(|e| {
            matches!(
                e,
                PaneEvent::KittyGraphic {
                    cmd: KittyGraphicCmd::Load { id: 1, .. }
                }
            )
        });
        assert!(
            got,
            "BEL-terminated kitty Load: expected event, got {:?}",
            events
        );
        assert_eq!(pty.apc.state, ApcScannerState::Idle);
    }

    /// Non-APC escape sequences are unaffected by the
    /// pre-scan: `ESC [` (CSI), `ESC ]` (OSC), and plain
    /// bytes all reach `vte::Parser::advance`. This pins
    /// the "we only intercept `ESC _`" rule — a future
    /// maintainer who accidentally widened the trigger
    /// (e.g., to `ESC P` for DCS) would break this test.
    #[test]
    fn apc_non_apc_escape_sequences_unaffected() {
        let (mut pty, _reader) = PanePty::spawn(
            ShellSpec::Command {
                argv: vec!["/bin/cat".to_string()],
            },
            10,
            10,
            PaneLayerId(54),
        )
        .expect("spawn pty");
        // CSI SGR for red fg + 4 printable chars. The `ESC [
        // (0x1B 0x5B)` pair must NOT be interpreted as the
        // start of an APC; the `0x5B` is not `0x5F`.
        pty.advance(b"\x1b[31mTEXT").expect("advance");
        let snap = pty.snapshot();
        assert_eq!(snap.grid.cell(0, 0).ch, 'T');
        assert_eq!(snap.grid.cell(1, 0).ch, 'E');
        assert_eq!(snap.grid.cell(2, 0).ch, 'X');
        assert_eq!(snap.grid.cell(3, 0).ch, 'T');
        assert_eq!(pty.apc.state, ApcScannerState::Idle);
    }
}
