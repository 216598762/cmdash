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
//! payloads (or `BEL`-terminated). vte 0.15 routes these via
//! [`vte::Perform::hook`] / [`put`] / [`unhook`] with action char
//! `'G'`. We accumulate the raw payload and parse it on `unhook`.

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
    fn cell_mut(&mut self, x: u16, y: u16) -> &mut Cell {
        let idx = self.cell_idx(x, y);
        &mut self.cells[idx]
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

fn parse_kitty_chunk(raw: &[u8]) -> Option<KittyGraphicCmd> {
    // The kitty escape payload is `<key>=<val>[,<key>=<val>]…[;<base64>]` — a
    // metadata section separated from the base64 payload by a literal `;`
    // (or none, if there's no payload at all). Split on whichever shows up.
    let sep = match raw.iter().position(|&b| b == b';') {
        Some(s) => s,
        None => raw.len(),
    };
    let meta_bytes = &raw[..sep];
    let payload = if sep + 1 <= raw.len() {
        &raw[sep + 1..]
    } else {
        &[]
    };
    let meta = std::str::from_utf8(meta_bytes).ok()?;
    let mut kv: HashMap<String, String> = HashMap::new();
    for segment in meta.split(|c: char| c == ';' || c == ',') {
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
    kitty: &'a mut KittyAccumulator,
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
        loop {
            let code = match iter.next().and_then(|p| p.first().copied()) {
                Some(c) => c,
                None => break,
            };
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
                let row = p0().saturating_sub(1) as u16;
                let col = p1().saturating_sub(1) as u16;
                self.grid.cursor_y = row.min(self.rows.saturating_sub(1));
                self.grid.cursor_x = col.min(self.cols.saturating_sub(1));
            }
            'A' => {
                let n = p0().max(1) as u16;
                self.grid.cursor_y = self.grid.cursor_y.saturating_sub(n);
            }
            'B' => {
                let n = p0().max(1) as u16;
                self.grid.cursor_y = (self.grid.cursor_y + n).min(self.rows - 1);
            }
            'C' => {
                let n = p0().max(1) as u16;
                self.grid.cursor_x = (self.grid.cursor_x + n).min(self.cols - 1);
            }
            'D' => {
                let n = p0().max(1) as u16;
                self.grid.cursor_x = self.grid.cursor_x.saturating_sub(n);
            }
            'J' => {
                let mode = p0() as u16;
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
                let mode = p0() as u16;
                self.grid.erase_in_line(self.grid.cursor_y, mode);
            }
            'm' => self.apply_sgr(params),
            _ => {}
        }
    }
}

impl<'a> vte::Perform for VtePerf<'a> {
    fn print(&mut self, c: char) {
        let x = self.grid.cursor_x;
        let y = self.grid.cursor_y;
        self.grid.put(x, y, *self.fg, *self.bg, *self.attrs, c);
        let nx = x.saturating_add(1);
        if nx >= self.cols {
            self.grid.cursor_x = 0;
            self.advance_line();
        } else {
            self.grid.cursor_x = nx;
        }
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x07 => {}
            0x08 => {
                if self.grid.cursor_x > 0 {
                    self.grid.cursor_x -= 1;
                }
            }
            0x09 => {
                let cur = self.grid.cursor_x;
                let next = ((cur / 8) + 1).saturating_mul(8);
                self.grid.cursor_x = next.min(self.cols - 1);
            }
            0x0A => self.advance_line(),
            0x0B => self.advance_line(),
            0x0C => self.advance_line(),
            0x0D => self.grid.cursor_x = 0,
            _ => {}
        }
    }

    fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, action: char) {
        if action == 'G' {
            self.kitty.begin();
        }
    }

    fn put(&mut self, byte: u8) {
        self.kitty.push(byte);
    }

    fn unhook(&mut self) {
        if let Some(raw) = self.kitty.finish() {
            if let Some(cmd) = parse_kitty_chunk(&raw) {
                self.events.push(PaneEvent::KittyGraphic { cmd });
            }
        }
    }

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
        let mut driver = VtePerf {
            grid: &mut self.grid,
            kitty: &mut self.kitty,
            events: &mut self.pending_events,
            title: &mut self.title,
            fg: &mut self.fg,
            bg: &mut self.bg,
            attrs: &mut self.attrs,
            cols: self.cols,
            rows: self.rows,
        };
        self.parser.advance(&mut driver, bytes);
        if let Some(child) = self.child.as_mut() {
            if let Ok(Some(status)) = child.try_wait() {
                self.pending_events.push(PaneEvent::Exit {
                    status: status.exit_code() as i32,
                });
            }
        }
        Ok(())
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
}
