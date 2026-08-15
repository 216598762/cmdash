use std::{
    io::{self, Write},
    time::{Duration, Instant},
};

use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{DisableMouseCapture, EnableMouseCapture},
    execute, queue,
    style::{Attribute, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor},
    terminal::{
        self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
        enable_raw_mode,
    },
};
use ratatui::layout::Rect;

use crate::{
    compositor::{CellSpan, FrameDiff},
    graphics::{GraphicsProtocolBroker, GraphicsSubmission},
    scene::{Cell, CellStyle, CellWidth, Color, Scene},
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OutputMetrics {
    pub frames_submitted: u64,
    pub frames_skipped: u64,
    pub bytes_written: u64,
    pub optimized_diff_bytes: u64,
    pub naive_diff_bytes: u64,
    pub bytes_saved: u64,
}

/// The outer terminal's cell and pixel dimensions.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TerminalWindowSize {
    pub columns: u16,
    pub rows: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
}

impl TerminalWindowSize {
    pub const fn area(self) -> Rect {
        Rect::new(0, 0, self.columns, self.rows)
    }
}

impl From<crossterm::terminal::WindowSize> for TerminalWindowSize {
    fn from(size: crossterm::terminal::WindowSize) -> Self {
        Self {
            columns: size.columns,
            rows: size.rows,
            pixel_width: size.width,
            pixel_height: size.height,
        }
    }
}

impl From<TerminalWindowSize> for crate::session::TerminalSize {
    fn from(size: TerminalWindowSize) -> Self {
        Self::with_pixels(size.columns, size.rows, size.pixel_width, size.pixel_height)
    }
}

impl TerminalWindowSize {
    pub const fn terminal_size(self) -> crate::session::TerminalSize {
        crate::session::TerminalSize::with_pixels(
            self.columns,
            self.rows,
            self.pixel_width,
            self.pixel_height,
        )
    }
}

struct ByteCountingWriter<W> {
    inner: W,
    bytes_written: u64,
}

impl<W> ByteCountingWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            bytes_written: 0,
        }
    }
}

impl<W: Write> Write for ByteCountingWriter<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buffer)?;
        self.bytes_written += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KittyGraphicsMode {
    Disabled,
    Direct,
    UnicodePlaceholder,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GraphicsSubmissionStatus {
    /// The outer adapter accepted and emitted every requested resource and placement.
    Rendered { resources: usize, placements: usize },
    /// The adapter emitted a usable result but had to degrade the requested mode.
    Degraded { placements: usize, reason: String },
    /// The child protocol was handled, but the outer terminal intentionally received no image data.
    Suppressed { placements: usize, reason: String },
    /// The outer adapter could not safely emit the requested graphics frame.
    Failed { placements: usize, reason: String },
}

impl GraphicsSubmissionStatus {
    pub const fn placements(&self) -> usize {
        match self {
            Self::Rendered { placements, .. }
            | Self::Degraded { placements, .. }
            | Self::Suppressed { placements, .. }
            | Self::Failed { placements, .. } => *placements,
        }
    }

    pub const fn is_successful(&self) -> bool {
        matches!(self, Self::Rendered { .. } | Self::Degraded { .. })
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Rendered { .. } => None,
            Self::Degraded { reason, .. }
            | Self::Suppressed { reason, .. }
            | Self::Failed { reason, .. } => Some(reason),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GraphicsCapabilitySource {
    EnvironmentHint,
    ExplicitOverride,
    ActiveProbe,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GraphicsCapabilityConfidence {
    Inferred,
    Confirmed,
    Rejected,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GraphicsCapabilityReport {
    pub kitty_graphics: bool,
    pub kitty_unicode_placeholders: bool,
    pub da1_seen: bool,
    pub pixel_size: Option<(u16, u16)>,
    pub source: GraphicsCapabilitySource,
    pub confidence: GraphicsCapabilityConfidence,
    pub diagnostic: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GraphicsProbeState {
    Idle,
    AwaitingResponse,
    Confirmed,
    Rejected,
    TimedOut,
}

/// Active outer-terminal Kitty probe and response correlator.
///
/// The probe deliberately accepts only a bounded response buffer and recognizes
/// only responses that contain the probe's Kitty APC terminator. Callers that
/// read the outer terminal should pass those bytes to `feed`; child PTY bytes
/// must not be passed here.
#[derive(Clone, Debug)]
pub struct GraphicsCapabilityProbe {
    state: GraphicsProbeState,
    buffer: Vec<u8>,
    deadline: Option<Instant>,
    timeout: Duration,
    max_response_bytes: usize,
    da1_seen: bool,
    pixel_size: Option<(u16, u16)>,
}

impl Default for GraphicsCapabilityProbe {
    fn default() -> Self {
        Self::new(Duration::from_millis(250), 16 * 1024)
    }
}

impl GraphicsCapabilityProbe {
    pub fn new(timeout: Duration, max_response_bytes: usize) -> Self {
        Self {
            state: GraphicsProbeState::Idle,
            buffer: Vec::new(),
            deadline: None,
            timeout,
            max_response_bytes: max_response_bytes.max(64),
            da1_seen: false,
            pixel_size: None,
        }
    }

    pub const fn state(&self) -> GraphicsProbeState {
        self.state
    }

    pub fn begin(&mut self, now: Instant) -> Option<Vec<u8>> {
        if matches!(self.state, GraphicsProbeState::AwaitingResponse) {
            return None;
        }
        self.buffer.clear();
        self.da1_seen = false;
        self.pixel_size = None;
        self.deadline = Some(now + self.timeout);
        self.state = GraphicsProbeState::AwaitingResponse;
        Some(b"\x1b_Ga=q,i=0,t=d,s=1,v=1,f=24;\x1b\\\x1b[c\x1b[14t".to_vec())
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Option<GraphicsCapabilityReport> {
        if !matches!(self.state, GraphicsProbeState::AwaitingResponse) {
            return None;
        }
        let remaining = self.max_response_bytes.saturating_sub(self.buffer.len());
        self.buffer
            .extend_from_slice(&bytes[..bytes.len().min(remaining)]);
        if self.buffer.len() >= self.max_response_bytes {
            self.state = GraphicsProbeState::Rejected;
            self.deadline = None;
            return Some(self.report(
                false,
                GraphicsCapabilityConfidence::Rejected,
                "outer terminal probe response exceeded the bounded buffer",
            ));
        }
        self.da1_seen |= self.buffer.windows(3).any(|window| window == b"\x1b[?");
        if let Some(start) = self
            .buffer
            .windows(3)
            .position(|window| window == b"\x1b[4")
            && let Some(end) = self.buffer[start..].iter().position(|byte| *byte == b't')
        {
            let response = &self.buffer[start..start + end];
            let mut values = response[4..].split(|byte| *byte == b';');
            self.pixel_size = values
                .next()
                .and_then(|height| std::str::from_utf8(height).ok())
                .and_then(|height| height.parse::<u16>().ok())
                .zip(
                    values
                        .next()
                        .and_then(|width| std::str::from_utf8(width).ok())
                        .and_then(|width| width.parse::<u16>().ok()),
                );
        }
        let start = self
            .buffer
            .windows(3)
            .position(|window| window == b"\x1b_G")?;
        let end = self.buffer[start + 3..]
            .windows(2)
            .position(|window| window == b"\x1b\\")?
            + start
            + 3;
        let response = &self.buffer[start..end + 2];
        let supported = response.windows(3).any(|window| window == b";OK");
        self.state = if supported {
            GraphicsProbeState::Confirmed
        } else {
            GraphicsProbeState::Rejected
        };
        self.deadline = None;
        Some(self.report(
            supported,
            if supported {
                GraphicsCapabilityConfidence::Confirmed
            } else {
                GraphicsCapabilityConfidence::Rejected
            },
            if supported {
                "outer terminal acknowledged the Kitty graphics probe"
            } else {
                "outer terminal rejected the Kitty graphics probe"
            },
        ))
    }

    pub fn poll_timeout(&mut self, now: Instant) -> Option<GraphicsCapabilityReport> {
        if matches!(self.state, GraphicsProbeState::AwaitingResponse)
            && self.deadline.is_some_and(|deadline| now >= deadline)
        {
            self.state = GraphicsProbeState::TimedOut;
            self.deadline = None;
            return Some(self.report(
                false,
                GraphicsCapabilityConfidence::Rejected,
                "outer terminal did not answer the Kitty graphics probe before timeout",
            ));
        }
        None
    }

    fn report(
        &self,
        kitty_graphics: bool,
        confidence: GraphicsCapabilityConfidence,
        diagnostic: &str,
    ) -> GraphicsCapabilityReport {
        GraphicsCapabilityReport {
            kitty_graphics,
            kitty_unicode_placeholders: kitty_graphics,
            da1_seen: self.da1_seen,
            pixel_size: self.pixel_size,
            source: GraphicsCapabilitySource::ActiveProbe,
            confidence,
            diagnostic: Some(diagnostic.to_owned()),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BackendCapabilities {
    pub truecolor: bool,
    pub mouse: bool,
    pub bracketed_paste: bool,
    pub kitty_graphics: bool,
    /// Use Kitty's Unicode-placeholder placement model for pane-safe replay.
    pub kitty_unicode_placeholders: bool,
    pub graphics_source: GraphicsCapabilitySource,
    pub graphics_confidence: GraphicsCapabilityConfidence,
    pub sixel: bool,
}

impl BackendCapabilities {
    pub const fn kitty_graphics_mode(self) -> KittyGraphicsMode {
        if !self.kitty_graphics {
            KittyGraphicsMode::Disabled
        } else if self.kitty_unicode_placeholders {
            KittyGraphicsMode::UnicodePlaceholder
        } else {
            KittyGraphicsMode::Direct
        }
    }

    pub fn detect() -> Self {
        let color_hint = std::env::var("COLORTERM")
            .unwrap_or_default()
            .to_ascii_lowercase();
        let terminal_hint = std::env::var("TERM")
            .unwrap_or_default()
            .to_ascii_lowercase();
        let program_hint = std::env::var("TERM_PROGRAM")
            .unwrap_or_default()
            .to_ascii_lowercase();

        let graphics_mode = std::env::var("CMDASH_KITTY_GRAPHICS_MODE")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        let explicit_graphics = std::env::var("CMDASH_KITTY_GRAPHICS").ok();
        let detected_kitty_graphics = match explicit_graphics.as_deref() {
            Some(value)
                if matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "0" | "false" | "no"
                ) =>
            {
                false
            }
            Some(value)
                if matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes"
                ) =>
            {
                true
            }
            _ => kitty_graphics_from_hints(
                &terminal_hint,
                &program_hint,
                std::env::var_os("KITTY_WINDOW_ID").is_some(),
                std::env::var_os("WEZTERM_PANE").is_some(),
                std::env::var_os("GHOSTTY_RESOURCES_DIR").is_some(),
            ),
        };
        let kitty_graphics =
            detected_kitty_graphics && !matches!(graphics_mode.as_str(), "off" | "disabled");
        let placeholder_capable = kitty_placeholder_from_hints(
            &terminal_hint,
            &program_hint,
            std::env::var_os("KITTY_WINDOW_ID").is_some(),
            std::env::var_os("GHOSTTY_RESOURCES_DIR").is_some(),
        );
        let kitty_unicode_placeholders = kitty_graphics
            && match graphics_mode.as_str() {
                "placeholder" | "unicode" => true,
                "direct" | "off" | "disabled" => false,
                _ => placeholder_capable,
            };

        let graphics_source = if explicit_graphics.is_some()
            || matches!(graphics_mode.as_str(), "off" | "disabled")
        {
            GraphicsCapabilitySource::ExplicitOverride
        } else if kitty_graphics {
            GraphicsCapabilitySource::EnvironmentHint
        } else {
            GraphicsCapabilitySource::Unavailable
        };
        let graphics_confidence = if kitty_graphics {
            GraphicsCapabilityConfidence::Inferred
        } else {
            GraphicsCapabilityConfidence::Rejected
        };

        Self {
            truecolor: color_hint.contains("truecolor") || color_hint.contains("24bit"),
            mouse: true,
            bracketed_paste: true,
            kitty_graphics,
            kitty_unicode_placeholders,
            graphics_source,
            graphics_confidence,
            sixel: cfg!(feature = "sixel")
                && (terminal_hint.contains("sixel")
                    || std::env::var("CMDASH_SIXEL").is_ok_and(|value| value == "1")),
        }
    }
}

fn kitty_graphics_from_hints(
    terminal_hint: &str,
    program_hint: &str,
    kitty_window: bool,
    wezterm_pane: bool,
    ghostty_resources: bool,
) -> bool {
    kitty_window
        || kitty_graphics_terminal_name(terminal_hint)
        || kitty_graphics_terminal_name(program_hint)
        || wezterm_pane
        || ghostty_resources
}

fn kitty_graphics_terminal_name(hint: &str) -> bool {
    ["kitty", "wezterm", "ghostty", "konsole", "iterm"]
        .iter()
        .any(|name| hint.contains(name))
}

fn kitty_placeholder_from_hints(
    terminal_hint: &str,
    program_hint: &str,
    kitty_window: bool,
    ghostty_resources: bool,
) -> bool {
    kitty_window
        || ghostty_resources
        || terminal_hint.contains("kitty")
        || terminal_hint.contains("ghostty")
        || program_hint.contains("kitty")
        || program_hint.contains("ghostty")
}

// Kitty reserves a stable set of combining marks for row/column/image-id
// encoding. This is the generated table used by Kitty's icat placeholder
// implementation; keeping it here avoids making placeholder rendering depend
// on a terminal-specific crate.
const KITTY_DIACRITICS: &[u32] = &[
    0x305, 0x30d, 0x30e, 0x310, 0x312, 0x33d, 0x33e, 0x33f, 0x346, 0x34a, 0x34b, 0x34c, 0x350,
    0x351, 0x352, 0x357, 0x35b, 0x363, 0x364, 0x365, 0x366, 0x367, 0x368, 0x369, 0x36a, 0x36b,
    0x36c, 0x36d, 0x36e, 0x36f, 0x483, 0x484, 0x485, 0x486, 0x487, 0x592, 0x593, 0x594, 0x595,
    0x597, 0x598, 0x599, 0x59c, 0x59d, 0x59e, 0x59f, 0x5a0, 0x5a1, 0x5a8, 0x5a9, 0x5ab, 0x5ac,
    0x5af, 0x5c4, 0x610, 0x611, 0x612, 0x613, 0x614, 0x615, 0x616, 0x617, 0x657, 0x658, 0x659,
    0x65a, 0x65b, 0x65d, 0x65e, 0x6d6, 0x6d7, 0x6d8, 0x6d9, 0x6da, 0x6db, 0x6dc, 0x6df, 0x6e0,
    0x6e1, 0x6e2, 0x6e4, 0x6e7, 0x6e8, 0x6eb, 0x6ec, 0x730, 0x732, 0x733, 0x735, 0x736, 0x73a,
    0x73d, 0x73f, 0x740, 0x741, 0x743, 0x745, 0x747, 0x749, 0x74a, 0x7eb, 0x7ec, 0x7ed, 0x7ee,
    0x7ef, 0x7f0, 0x7f1, 0x7f3, 0x816, 0x817, 0x818, 0x819, 0x81b, 0x81c, 0x81d, 0x81e, 0x81f,
    0x820, 0x821, 0x822, 0x823, 0x825, 0x826, 0x827, 0x829, 0x82a, 0x82b, 0x82c, 0x82d, 0x951,
    0x953, 0x954, 0xf82, 0xf83, 0xf86, 0xf87, 0x135d, 0x135e, 0x135f, 0x17dd, 0x193a, 0x1a17,
    0x1a75, 0x1a76, 0x1a77, 0x1a78, 0x1a79, 0x1a7a, 0x1a7b, 0x1a7c, 0x1b6b, 0x1b6d, 0x1b6e, 0x1b6f,
    0x1b70, 0x1b71, 0x1b72, 0x1b73, 0x1cd0, 0x1cd1, 0x1cd2, 0x1cda, 0x1cdb, 0x1ce0, 0x1dc0, 0x1dc1,
    0x1dc3, 0x1dc4, 0x1dc5, 0x1dc6, 0x1dc7, 0x1dc8, 0x1dc9, 0x1dcb, 0x1dcc, 0x1dd1, 0x1dd2, 0x1dd3,
    0x1dd4, 0x1dd5, 0x1dd6, 0x1dd7, 0x1dd8, 0x1dd9, 0x1dda, 0x1ddb, 0x1ddc, 0x1ddd, 0x1dde, 0x1ddf,
    0x1de0, 0x1de1, 0x1de2, 0x1de3, 0x1de4, 0x1de5, 0x1de6, 0x1dfe, 0x20d0, 0x20d1, 0x20d4, 0x20d5,
    0x20d6, 0x20d7, 0x20db, 0x20dc, 0x20e1, 0x20e7, 0x20e9, 0x20f0, 0x2cef, 0x2cf0, 0x2cf1, 0x2de0,
    0x2de1, 0x2de2, 0x2de3, 0x2de4, 0x2de5, 0x2de6, 0x2de7, 0x2de8, 0x2de9, 0x2dea, 0x2deb, 0x2dec,
    0x2ded, 0x2dee, 0x2def, 0x2df0, 0x2df1, 0x2df2, 0x2df3, 0x2df4, 0x2df5, 0x2df6, 0x2df7, 0x2df8,
    0x2df9, 0x2dfa, 0x2dfb, 0x2dfc, 0x2dfd, 0x2dfe, 0x2dff, 0xa66f, 0xa67c, 0xa67d, 0xa6f0, 0xa6f1,
    0xa8e0, 0xa8e1, 0xa8e2, 0xa8e3, 0xa8e4, 0xa8e5, 0xa8e6, 0xa8e7, 0xa8e8, 0xa8e9, 0xa8ea, 0xa8eb,
    0xa8ec, 0xa8ed, 0xa8ee, 0xa8ef, 0xa8f0, 0xa8f1, 0xaab0, 0xaab2, 0xaab3, 0xaab7, 0xaab8, 0xaabe,
    0xaabf, 0xaac1, 0xfe20, 0xfe21, 0xfe22, 0xfe23, 0xfe24, 0xfe25, 0xfe26, 0x10a0f, 0x10a38,
    0x1d185, 0x1d186, 0x1d187, 0x1d188, 0x1d189, 0x1d1aa, 0x1d1ab, 0x1d1ac, 0x1d1ad, 0x1d242,
    0x1d243, 0x1d244,
];

fn kitty_diacritic(value: u16) -> Option<char> {
    KITTY_DIACRITICS
        .get(usize::from(value))
        .and_then(|codepoint| char::from_u32(*codepoint))
}

pub trait Backend {
    type Error;

    fn capabilities(&self) -> BackendCapabilities;
    fn metrics(&self) -> OutputMetrics;
    fn size(&self) -> Result<Rect, Self::Error>;

    /// Returns cell and pixel dimensions for the outer terminal.
    ///
    /// Backends that cannot report pixels retain the cell-only fallback.
    fn window_size(&self) -> Result<TerminalWindowSize, Self::Error> {
        let area = self.size()?;
        Ok(TerminalWindowSize {
            columns: area.width,
            rows: area.height,
            pixel_width: 0,
            pixel_height: 0,
        })
    }

    fn enter(&mut self) -> Result<(), Self::Error>;
    fn leave(&mut self) -> Result<(), Self::Error>;
    fn submit(&mut self, scene: &Scene) -> Result<(), Self::Error>;
    fn submit_diff(&mut self, diff: &FrameDiff) -> Result<(), Self::Error>;

    fn submit_graphics(
        &mut self,
        _changed: &[GraphicsSubmission],
        visible: &[GraphicsSubmission],
        _removed: &[GraphicsSubmission],
    ) -> Result<GraphicsSubmissionStatus, Self::Error> {
        if visible.is_empty() {
            Ok(GraphicsSubmissionStatus::Rendered {
                resources: 0,
                placements: 0,
            })
        } else {
            Ok(GraphicsSubmissionStatus::Suppressed {
                placements: visible.len(),
                reason: "backend does not provide a graphics submission adapter".to_owned(),
            })
        }
    }

    #[cfg(feature = "sixel")]
    fn submit_sixel(
        &mut self,
        _sixel: &[crate::sixel::SixelSubmission],
    ) -> Result<(), Self::Error> {
        Ok(())
    }

    fn submit_clipboard(&mut self, _text: &str) -> Result<(), Self::Error> {
        Ok(())
    }
}

pub struct CrosstermBackend<W: Write> {
    writer: ByteCountingWriter<W>,
    capabilities: BackendCapabilities,
    graphics_probe: GraphicsCapabilityProbe,
    graphics_broker: GraphicsProtocolBroker,
    entered: bool,
    frames_submitted: u64,
    frames_skipped: u64,
    optimized_diff_bytes: u64,
    naive_diff_bytes: u64,
    bytes_saved: u64,
}

impl<W: Write> CrosstermBackend<W> {
    pub fn new(writer: W) -> Self {
        Self {
            writer: ByteCountingWriter::new(writer),
            capabilities: BackendCapabilities::detect(),
            graphics_probe: GraphicsCapabilityProbe::default(),
            graphics_broker: GraphicsProtocolBroker::default(),
            entered: false,
            frames_submitted: 0,
            frames_skipped: 0,
            optimized_diff_bytes: 0,
            naive_diff_bytes: 0,
            bytes_saved: 0,
        }
    }

    pub fn writer(&self) -> &W {
        &self.writer.inner
    }

    pub fn with_capabilities(mut self, capabilities: BackendCapabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    pub const fn metrics(&self) -> OutputMetrics {
        OutputMetrics {
            frames_submitted: self.frames_submitted,
            frames_skipped: self.frames_skipped,
            bytes_written: self.writer.bytes_written,
            optimized_diff_bytes: self.optimized_diff_bytes,
            naive_diff_bytes: self.naive_diff_bytes,
            bytes_saved: self.bytes_saved,
        }
    }

    pub const fn graphics_probe_state(&self) -> GraphicsProbeState {
        self.graphics_probe.state()
    }

    /// Starts an active outer-terminal probe and emits it through the outer
    /// response queue. The caller must route raw outer-terminal input back to
    /// `feed_graphics_probe`; child PTY output must remain separate.
    pub fn begin_graphics_probe(&mut self) -> io::Result<bool> {
        let Some(request) = self.graphics_probe.begin(Instant::now()) else {
            return Ok(false);
        };
        if !self.graphics_broker.queue_outer(request) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "outer graphics probe response queue is full",
            ));
        }
        self.flush_outer_graphics_responses()?;
        Ok(true)
    }

    pub fn feed_graphics_probe(&mut self, bytes: &[u8]) -> Option<GraphicsCapabilityReport> {
        let report = self.graphics_probe.feed(bytes)?;
        self.capabilities.kitty_graphics = report.kitty_graphics;
        self.capabilities.kitty_unicode_placeholders = report.kitty_unicode_placeholders;
        self.capabilities.graphics_source = report.source;
        self.capabilities.graphics_confidence = report.confidence;
        Some(report)
    }

    pub fn poll_graphics_probe_timeout(&mut self) -> Option<GraphicsCapabilityReport> {
        let report = self.graphics_probe.poll_timeout(Instant::now())?;
        self.capabilities.kitty_graphics = false;
        self.capabilities.kitty_unicode_placeholders = false;
        self.capabilities.graphics_source = report.source;
        self.capabilities.graphics_confidence = report.confidence;
        Some(report)
    }

    fn flush_outer_graphics_responses(&mut self) -> io::Result<()> {
        for response in self.graphics_broker.drain_outer() {
            self.writer.write_all(response.bytes())?;
        }
        self.writer.flush()
    }
}

impl<W: Write> Backend for CrosstermBackend<W> {
    type Error = io::Error;

    fn capabilities(&self) -> BackendCapabilities {
        self.capabilities
    }

    fn metrics(&self) -> OutputMetrics {
        CrosstermBackend::metrics(self)
    }

    fn size(&self) -> Result<Rect, Self::Error> {
        Ok(self.window_size()?.area())
    }

    fn window_size(&self) -> Result<TerminalWindowSize, Self::Error> {
        Ok(terminal::window_size()?.into())
    }

    fn enter(&mut self) -> Result<(), Self::Error> {
        if self.entered {
            return Ok(());
        }

        enable_raw_mode()?;
        if let Err(error) = execute!(
            self.writer,
            EnterAlternateScreen,
            EnableMouseCapture,
            Hide,
            Clear(ClearType::All)
        ) {
            let _ = disable_raw_mode();
            return Err(error);
        }
        self.entered = true;
        Ok(())
    }

    fn leave(&mut self) -> Result<(), Self::Error> {
        if !self.entered {
            return Ok(());
        }

        let terminal_result = execute!(
            self.writer,
            Show,
            DisableMouseCapture,
            LeaveAlternateScreen,
            ResetColor,
            SetAttribute(Attribute::Reset)
        );
        let raw_mode_result = disable_raw_mode();
        self.entered = false;
        terminal_result.and(raw_mode_result)
    }

    fn submit(&mut self, scene: &Scene) -> Result<(), Self::Error> {
        queue!(
            self.writer,
            MoveTo(scene.area().x, scene.area().y),
            Clear(ClearType::All),
            Hide
        )?;

        let area = scene.area();
        for (index, cell) in scene.cells().iter().enumerate() {
            let column = index % area.width as usize;
            let row = index / area.width as usize;
            let x = area.x.saturating_add(column as u16);
            let y = area.y.saturating_add(row as u16);
            write_cell(&mut self.writer, x, y, *cell)?;
        }

        queue!(
            self.writer,
            ResetColor,
            SetAttribute(Attribute::Reset),
            MoveTo(area.x, area.y),
            Show
        )?;
        self.writer.flush()?;
        self.frames_submitted += 1;
        Ok(())
    }

    fn submit_graphics(
        &mut self,
        changed: &[GraphicsSubmission],
        visible: &[GraphicsSubmission],
        removed: &[GraphicsSubmission],
    ) -> Result<GraphicsSubmissionStatus, Self::Error> {
        if !self.capabilities.kitty_graphics {
            return if visible.is_empty() {
                Ok(GraphicsSubmissionStatus::Rendered {
                    resources: 0,
                    placements: 0,
                })
            } else {
                Ok(GraphicsSubmissionStatus::Suppressed {
                    placements: visible.len(),
                    reason: "outer terminal graphics capability is unavailable".to_owned(),
                })
            };
        }
        if self.capabilities.kitty_unicode_placeholders {
            if let Some(reason) = placeholder_geometry_error(visible) {
                return Ok(GraphicsSubmissionStatus::Failed {
                    placements: visible.len(),
                    reason,
                });
            }
            for submission in changed {
                write_placeholder_upload(&mut self.writer, submission)?;
            }
            for submission in visible {
                write_placeholder_cells(&mut self.writer, submission)?;
            }
            self.writer.flush()?;
            return Ok(GraphicsSubmissionStatus::Rendered {
                resources: changed.len(),
                placements: visible.len(),
            });
        }
        for submission in removed {
            write!(
                self.writer,
                "\x1b_Ga=d,d=i,i={};\x1b\\",
                submission.terminal_image_id()
            )?;
        }
        for submission in changed {
            let physical_id = submission.terminal_image_id();
            let placement = submission.placement();
            queue!(self.writer, MoveTo(placement.x(), placement.y()))?;
            write!(
                self.writer,
                "\x1b_Ga=T,f={},i={},c={},r={},C=1,q=2,m=0",
                submission.format(),
                physical_id,
                placement.width(),
                placement.height()
            )?;
            if placement.z_index() != 0 {
                write!(self.writer, ",z={}", placement.z_index())?;
            }
            self.writer.write_all(b";")?;
            self.writer.write_all(submission.encoded_payload())?;
            self.writer.write_all(b"\x1b\\")?;
        }
        self.writer.flush()?;
        Ok(GraphicsSubmissionStatus::Rendered {
            resources: changed.len(),
            placements: visible.len(),
        })
    }

    #[cfg(feature = "sixel")]
    fn submit_sixel(&mut self, sixel: &[crate::sixel::SixelSubmission]) -> Result<(), Self::Error> {
        if !self.capabilities.sixel {
            return Ok(());
        }
        for image in sixel {
            queue!(self.writer, MoveTo(image.x(), image.y()))?;
            self.writer.write_all(image.encoded())?;
        }
        self.writer.flush()
    }

    fn submit_clipboard(&mut self, text: &str) -> Result<(), Self::Error> {
        let encoded = encode_base64(text.as_bytes());
        write!(self.writer, "\x1b]52;c;")?;
        self.writer.write_all(&encoded)?;
        self.writer.write_all(b"\x07")?;
        self.writer.flush()
    }

    fn submit_diff(&mut self, diff: &FrameDiff) -> Result<(), Self::Error> {
        if self.capabilities.kitty_unicode_placeholders {
            for submission in diff.removed_graphics() {
                clear_placeholder_layer(&mut self.writer, submission)?;
            }
        }
        if diff.is_empty() {
            self.frames_skipped += 1;
            return Ok(());
        }

        let naive_bytes = measure_diff(diff)?;
        let bytes_before = self.writer.bytes_written;
        write_diff(&mut self.writer, diff, true)?;
        let optimized_bytes = self.writer.bytes_written - bytes_before;
        self.frames_submitted += 1;
        self.optimized_diff_bytes += optimized_bytes;
        self.naive_diff_bytes += naive_bytes;
        self.bytes_saved += naive_bytes.saturating_sub(optimized_bytes);
        Ok(())
    }
}

fn encode_base64(bytes: &[u8]) -> Vec<u8> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = Vec::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0] as u32;
        let second = chunk.get(1).copied().unwrap_or(0) as u32;
        let third = chunk.get(2).copied().unwrap_or(0) as u32;
        let combined = (first << 16) | (second << 8) | third;
        output.push(TABLE[((combined >> 18) & 63) as usize]);
        output.push(TABLE[((combined >> 12) & 63) as usize]);
        output.push(if chunk.len() > 1 {
            TABLE[((combined >> 6) & 63) as usize]
        } else {
            b'='
        });
        output.push(if chunk.len() > 2 {
            TABLE[(combined & 63) as usize]
        } else {
            b'='
        });
    }
    output
}

fn write_cell<W: Write>(writer: &mut W, x: u16, y: u16, cell: Cell) -> io::Result<()> {
    queue!(writer, MoveTo(x, y))?;
    write_cell_contents(writer, cell)
}

fn write_span<W: Write>(
    writer: &mut W,
    span: &CellSpan,
    active_style: &mut Option<CellStyle>,
) -> io::Result<()> {
    let Some(first_index) = span
        .cells()
        .iter()
        .position(|cell| cell.width != CellWidth::Continuation)
    else {
        return Ok(());
    };
    let first = &span.cells()[first_index];

    queue!(
        writer,
        MoveTo(span.x().saturating_add(first_index as u16), span.y())
    )?;
    write_style_if_changed(writer, first.style, active_style)?;
    for cell in span.cells().iter().skip(first_index) {
        if cell.width != CellWidth::Continuation {
            queue!(writer, Print(cell.symbol))?;
        }
    }
    Ok(())
}

fn write_diff<W: Write>(writer: &mut W, diff: &FrameDiff, grouped: bool) -> io::Result<()> {
    queue!(writer, Hide)?;
    if diff.full_redraw() {
        queue!(writer, Clear(ClearType::All))?;
    }
    let mut active_style = None;
    if grouped {
        for span in diff.spans() {
            write_span(writer, span, &mut active_style)?;
        }
    } else {
        for change in diff.changes() {
            write_cell(writer, change.x, change.y, change.cell)?;
        }
    }
    queue!(
        writer,
        ResetColor,
        SetAttribute(Attribute::Reset),
        MoveTo(diff.viewport().x, diff.viewport().y),
        Show
    )?;
    writer.flush()
}

fn measure_diff(diff: &FrameDiff) -> io::Result<u64> {
    let mut output = Vec::new();
    write_diff(&mut output, diff, false)?;
    Ok(output.len() as u64)
}

fn write_cell_contents<W: Write>(writer: &mut W, cell: Cell) -> io::Result<()> {
    if cell.width == CellWidth::Continuation {
        return Ok(());
    }
    write_style(writer, cell.style)?;
    queue!(writer, Print(cell.symbol))
}

fn write_style_if_changed<W: Write>(
    writer: &mut W,
    style: CellStyle,
    active_style: &mut Option<CellStyle>,
) -> io::Result<()> {
    if *active_style == Some(style) {
        return Ok(());
    }
    write_style(writer, style)?;
    *active_style = Some(style);
    Ok(())
}

fn write_style<W: Write>(writer: &mut W, style: CellStyle) -> io::Result<()> {
    queue!(
        writer,
        SetAttribute(Attribute::Reset),
        SetForegroundColor(to_crossterm_color(style.foreground)),
        SetBackgroundColor(to_crossterm_color(style.background))
    )?;
    if style.bold {
        queue!(writer, SetAttribute(Attribute::Bold))?;
    }
    if style.dim {
        queue!(writer, SetAttribute(Attribute::Dim))?;
    }
    Ok(())
}

fn write_placeholder_upload<W: Write>(
    writer: &mut W,
    submission: &GraphicsSubmission,
) -> io::Result<()> {
    let physical_id = submission.terminal_image_id();
    let placement = submission.placement();
    queue!(writer, MoveTo(placement.x(), placement.y()))?;
    write!(
        writer,
        "\x1b_Ga=T,f={},i={},c={},r={},U=1,C=1,q=2,m=0",
        submission.format(),
        physical_id,
        placement.width(),
        placement.height()
    )?;
    if placement.z_index() != 0 {
        write!(writer, ",z={}", placement.z_index())?;
    }
    writer.write_all(b";")?;
    writer.write_all(submission.encoded_payload())?;
    writer.write_all(b"\x1b\\")
}

fn placeholder_geometry_error(submissions: &[GraphicsSubmission]) -> Option<String> {
    for submission in submissions {
        if ((submission.terminal_image_id() >> 24) & 0xff) as usize >= KITTY_DIACRITICS.len() {
            return Some(format!(
                "image {} cannot be encoded as a Kitty Unicode placeholder",
                submission.resource().image()
            ));
        }
        if usize::from(submission.placement().width()) >= KITTY_DIACRITICS.len()
            || usize::from(submission.placement().height()) >= KITTY_DIACRITICS.len()
        {
            return Some(format!(
                "image {} placement {}x{} exceeds Kitty Unicode placeholder geometry",
                submission.resource().image(),
                submission.placement().width(),
                submission.placement().height()
            ));
        }
    }
    None
}

fn write_placeholder_cells<W: Write>(
    writer: &mut W,
    submission: &GraphicsSubmission,
) -> io::Result<()> {
    let placement = submission.placement();
    let physical_id = submission.terminal_image_id();
    let red = (physical_id >> 16) as u8;
    let green = (physical_id >> 8) as u8;
    let blue = physical_id as u8;
    let high = ((physical_id >> 24) & 0xff) as u16;
    let Some(high) = kitty_diacritic(high) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Kitty image id cannot be encoded as a Unicode placeholder",
        ));
    };

    write!(writer, "\x1b[38;2;{red};{green};{blue}m")?;
    for row in 0..placement.height() {
        let row_mark = kitty_diacritic(row).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Kitty image height cannot be encoded as a Unicode placeholder",
            )
        })?;
        queue!(
            writer,
            MoveTo(placement.x(), placement.y().saturating_add(row))
        )?;
        for column in 0..placement.width() {
            let column_mark = kitty_diacritic(column).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Kitty image width cannot be encoded as a Unicode placeholder",
                )
            })?;
            write!(
                writer,
                "{}{}{}{}",
                '\u{10eeee}', row_mark, column_mark, high
            )?;
        }
    }
    writer.write_all(b"\x1b[39m")
}

fn clear_placeholder_layer<W: Write>(
    writer: &mut W,
    submission: &GraphicsSubmission,
) -> io::Result<()> {
    write!(
        writer,
        "\x1b_Ga=d,d=i,i={};\x1b\\",
        submission.terminal_image_id()
    )?;
    let placement = submission.placement();
    for row in 0..placement.height() {
        queue!(
            writer,
            MoveTo(placement.x(), placement.y().saturating_add(row))
        )?;
        for _ in 0..placement.width() {
            writer.write_all(b" ")?;
        }
    }
    Ok(())
}

impl<W: Write> Drop for CrosstermBackend<W> {
    fn drop(&mut self) {
        let _ = self.leave();
    }
}

fn to_crossterm_color(color: Color) -> crossterm::style::Color {
    match color {
        Color::Rgb { red, green, blue } => crossterm::style::Color::Rgb {
            r: red,
            g: green,
            b: blue,
        },
        Color::Ansi(index) => crossterm::style::Color::AnsiValue(index),
        Color::Reset => crossterm::style::Color::Reset,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Compositor, SessionGraphicsStore, SessionId,
        scene::{CellStyle, Color},
    };

    #[test]
    fn terminal_window_size_preserves_cell_and_pixel_metrics() {
        let size = TerminalWindowSize {
            columns: 80,
            rows: 24,
            pixel_width: 800,
            pixel_height: 480,
        };
        assert_eq!(size.area(), Rect::new(0, 0, 80, 24));
        assert_eq!(size.terminal_size().cell_width(), 10);
        assert_eq!(size.terminal_size().cell_height(), 20);
    }

    #[test]
    fn backend_capabilities_are_stable_for_a_constructed_backend() {
        let backend = CrosstermBackend::new(Vec::<u8>::new());
        assert!(backend.capabilities().mouse);
        assert!(backend.capabilities().bracketed_paste);
    }

    #[test]
    fn compatible_outer_terminals_are_recognized_for_kitty_graphics() {
        assert!(kitty_graphics_from_hints(
            "xterm-256color",
            "wezterm",
            false,
            false,
            false
        ));
        assert!(kitty_graphics_from_hints(
            "xterm-256color",
            "",
            false,
            false,
            true
        ));
        assert!(kitty_graphics_from_hints(
            "xterm-ghostty",
            "",
            false,
            false,
            false
        ));
        assert!(!kitty_graphics_from_hints(
            "xterm-256color",
            "",
            false,
            false,
            false
        ));
        assert!(kitty_placeholder_from_hints(
            "xterm-kitty",
            "",
            false,
            false
        ));
        assert!(!kitty_placeholder_from_hints(
            "xterm-256color",
            "wezterm",
            false,
            false
        ));
    }

    #[test]
    fn active_graphics_probe_correlates_only_outer_kitty_acknowledgements() {
        let mut probe = GraphicsCapabilityProbe::new(Duration::from_secs(1), 256);
        let request = probe.begin(Instant::now()).unwrap();
        assert!(request.windows(4).any(|window| window == b"a=q,"));
        assert_eq!(probe.feed(b"\x1b[?1;2c\x1b[4;160;400t"), None);

        let report = probe.feed(b"\x1b_Gi=0;OK\x1b\\").unwrap();
        assert!(report.da1_seen);
        assert_eq!(report.pixel_size, Some((160, 400)));
        assert_eq!(report.source, GraphicsCapabilitySource::ActiveProbe);
        assert_eq!(report.confidence, GraphicsCapabilityConfidence::Confirmed);
        assert!(report.kitty_graphics);
        assert_eq!(probe.state(), GraphicsProbeState::Confirmed);
    }

    #[test]
    fn backend_probe_uses_the_outer_queue_and_updates_capabilities() {
        let mut backend = CrosstermBackend::new(Vec::<u8>::new());
        assert!(backend.begin_graphics_probe().unwrap());
        assert!(backend.writer().windows(4).any(|window| window == b"a=q,"));
        let report = backend.feed_graphics_probe(b"\x1b_Gi=0;OK\x1b\\").unwrap();
        assert_eq!(report.source, GraphicsCapabilitySource::ActiveProbe);
        assert!(backend.capabilities().kitty_graphics);
        assert_eq!(
            backend.capabilities().graphics_confidence,
            GraphicsCapabilityConfidence::Confirmed
        );
    }

    #[test]
    fn graphics_probe_times_out_without_claiming_outer_support() {
        let now = Instant::now();
        let mut probe = GraphicsCapabilityProbe::new(Duration::from_millis(1), 256);
        probe.begin(now).unwrap();
        let report = probe.poll_timeout(now + Duration::from_millis(2)).unwrap();
        assert!(!report.kitty_graphics);
        assert_eq!(report.confidence, GraphicsCapabilityConfidence::Rejected);
        assert_eq!(probe.state(), GraphicsProbeState::TimedOut);
    }

    #[test]
    fn unsupported_graphics_are_reported_as_suppressed_instead_of_succeeding_silently() {
        let mut store = SessionGraphicsStore::new(SessionId::new(13));
        store
            .apply_kitty_command_with_context(b"a=T,f=24,i=1,c=2,r=1", b"AQID", (0, 0), (10, 20))
            .unwrap();
        let graphics = store.visible_submissions(Rect::new(0, 0, 4, 2));
        let capabilities = BackendCapabilities {
            truecolor: true,
            mouse: true,
            bracketed_paste: true,
            kitty_graphics: false,
            kitty_unicode_placeholders: false,
            graphics_source: GraphicsCapabilitySource::Unavailable,
            graphics_confidence: GraphicsCapabilityConfidence::Rejected,
            sixel: false,
        };
        let mut backend = CrosstermBackend::new(Vec::<u8>::new()).with_capabilities(capabilities);
        let status = backend.submit_graphics(&graphics, &graphics, &[]).unwrap();
        assert!(matches!(
            status,
            GraphicsSubmissionStatus::Suppressed { placements: 1, .. }
        ));
    }

    #[test]
    fn capability_modes_are_typed_for_backend_selection() {
        let disabled = BackendCapabilities {
            truecolor: true,
            mouse: true,
            bracketed_paste: true,
            kitty_graphics: false,
            kitty_unicode_placeholders: false,
            graphics_source: GraphicsCapabilitySource::Unavailable,
            graphics_confidence: GraphicsCapabilityConfidence::Rejected,
            sixel: false,
        };
        assert_eq!(disabled.kitty_graphics_mode(), KittyGraphicsMode::Disabled);

        let direct = BackendCapabilities {
            kitty_unicode_placeholders: false,
            kitty_graphics: true,
            ..disabled
        };
        assert_eq!(direct.kitty_graphics_mode(), KittyGraphicsMode::Direct);

        let placeholder = BackendCapabilities {
            kitty_unicode_placeholders: true,
            ..direct
        };
        assert_eq!(
            placeholder.kitty_graphics_mode(),
            KittyGraphicsMode::UnicodePlaceholder
        );
    }

    #[test]
    fn native_palette_colors_are_preserved_for_the_parent_terminal() {
        assert_eq!(
            to_crossterm_color(Color::ansi(14)),
            crossterm::style::Color::AnsiValue(14)
        );
        assert_eq!(
            to_crossterm_color(Color::reset()),
            crossterm::style::Color::Reset
        );
    }

    #[test]
    fn submitting_a_scene_writes_terminal_commands() {
        let mut backend = CrosstermBackend::new(Vec::<u8>::new());
        let mut scene = Scene::new(Rect::new(0, 0, 2, 1));
        scene.set(
            0,
            0,
            'x',
            CellStyle::new(Color::rgb(255, 255, 255), Color::rgb(0, 0, 0)),
        );

        backend.submit(&scene).unwrap();
        assert!(!backend.writer().is_empty());
    }

    #[test]
    fn empty_frame_diffs_do_not_write_terminal_commands() {
        let mut backend = CrosstermBackend::new(Vec::<u8>::new());
        let scene = Scene::new(Rect::new(0, 0, 2, 1));
        let mut compositor = Compositor::new();
        let first = compositor.diff(&scene);
        backend.submit_diff(&first).unwrap();
        let bytes_after_first = backend.writer().len();

        let unchanged = compositor.diff(&scene);
        backend.submit_diff(&unchanged).unwrap();

        assert_eq!(backend.writer().len(), bytes_after_first);
        assert_eq!(backend.metrics().frames_skipped, 1);
    }

    #[test]
    fn metrics_report_bytes_saved_by_grouped_spans() {
        let mut backend = CrosstermBackend::new(Vec::<u8>::new());
        let scene = Scene::new(Rect::new(0, 0, 8, 1));
        let mut compositor = Compositor::new();
        let diff = compositor.diff(&scene);

        backend.submit_diff(&diff).unwrap();
        let metrics = backend.metrics();

        assert!(metrics.bytes_saved > 0);
        assert!(metrics.naive_diff_bytes > metrics.optimized_diff_bytes);
        assert_eq!(metrics.frames_submitted, 1);
    }

    #[test]
    fn wide_glyphs_are_emitted_once_with_continuation_cells_skipped() {
        let mut backend = CrosstermBackend::new(Vec::<u8>::new());
        let mut compositor = Compositor::new();
        let mut scene = Scene::new(Rect::new(0, 0, 4, 1));
        scene.text(
            0,
            0,
            "界a",
            CellStyle::new(Color::rgb(255, 255, 255), Color::rgb(0, 0, 0)),
        );
        let diff = compositor.diff(&scene);

        backend.submit_diff(&diff).unwrap();
        let glyph = "界".as_bytes();
        let occurrences = backend
            .writer()
            .windows(glyph.len())
            .filter(|window| *window == glyph)
            .count();
        assert_eq!(occurrences, 1);
    }

    #[test]
    fn clipboard_submission_uses_osc52_base64() {
        let mut backend = CrosstermBackend::new(Vec::<u8>::new());
        backend.submit_clipboard("copy").unwrap();
        assert!(backend.writer().windows(4).any(|window| window == b"52;c"));
        assert!(
            backend
                .writer()
                .windows(8)
                .any(|window| window == b"Y29weQ==")
        );
    }

    #[test]
    fn kitty_graphics_are_replayed_only_when_the_backend_supports_them() {
        let mut store = SessionGraphicsStore::new(SessionId::new(7));
        store.apply_kitty_command(b"a=T,f=24,i=1", b"AQID").unwrap();
        store
            .apply_kitty_command(b"a=p,i=1,x=0,y=0,c=2,r=1", b"")
            .unwrap();
        let graphics = store.visible_submissions(Rect::new(0, 0, 4, 2));
        let capabilities = BackendCapabilities {
            truecolor: true,
            mouse: true,
            bracketed_paste: true,
            kitty_graphics: true,
            kitty_unicode_placeholders: false,
            graphics_source: GraphicsCapabilitySource::Unavailable,
            graphics_confidence: GraphicsCapabilityConfidence::Rejected,
            sixel: false,
        };
        let mut backend = CrosstermBackend::new(Vec::<u8>::new()).with_capabilities(capabilities);
        backend.submit_graphics(&graphics, &graphics, &[]).unwrap();

        let output = backend.writer();
        assert!(output.windows(4).any(|window| window == b"a=T,"));
        assert!(output.windows(7).any(|window| window == b"c=2,r=1"));
        assert!(output.windows(3).any(|window| window == b"C=1"));
        assert!(!output.windows(4).any(|window| window == b"a=p,"));
        assert!(output.windows(3).any(|window| window == b"AQI"));
    }

    #[test]
    fn kitty_graphics_can_be_replayed_with_unicode_placeholders() {
        let mut store = SessionGraphicsStore::new(SessionId::new(11));
        store
            .apply_kitty_command(b"a=T,f=24,i=1,c=2,r=1", b"AQID")
            .unwrap();
        let graphics = store.visible_submissions(Rect::new(0, 0, 4, 2));
        let capabilities = BackendCapabilities {
            truecolor: true,
            mouse: true,
            bracketed_paste: true,
            kitty_graphics: true,
            kitty_unicode_placeholders: true,
            graphics_source: GraphicsCapabilitySource::EnvironmentHint,
            graphics_confidence: GraphicsCapabilityConfidence::Inferred,
            sixel: false,
        };
        let mut backend = CrosstermBackend::new(Vec::<u8>::new()).with_capabilities(capabilities);
        backend.submit_graphics(&graphics, &graphics, &[]).unwrap();

        let output = backend.writer();
        assert!(output.windows(5).any(|window| window == b"U=1,C"));
        assert!(output.windows(7).any(|window| window == b"c=2,r=1"));
        let placeholder = '\u{10eeee}'.to_string();
        assert!(
            output
                .windows(placeholder.len())
                .any(|window| window == placeholder.as_bytes())
        );
        assert!(output.windows(4).any(|window| window == b"AQID"));
    }

    #[test]
    fn placeholder_geometry_failures_are_reported_without_partial_output() {
        let mut store = SessionGraphicsStore::new(SessionId::new(14));
        store
            .apply_kitty_command(b"a=T,f=24,i=1,c=400,r=1", b"AQID")
            .unwrap();
        let graphics = store.visible_submissions(Rect::new(0, 0, 500, 2));
        let capabilities = BackendCapabilities {
            truecolor: true,
            mouse: true,
            bracketed_paste: true,
            kitty_graphics: true,
            kitty_unicode_placeholders: true,
            graphics_source: GraphicsCapabilitySource::EnvironmentHint,
            graphics_confidence: GraphicsCapabilityConfidence::Inferred,
            sixel: false,
        };
        let mut backend = CrosstermBackend::new(Vec::<u8>::new()).with_capabilities(capabilities);
        let status = backend.submit_graphics(&graphics, &graphics, &[]).unwrap();

        assert!(matches!(
            status,
            GraphicsSubmissionStatus::Failed { placements: 1, .. }
        ));
        assert!(backend.writer().is_empty());
    }

    #[test]
    fn placeholder_replay_clears_removed_layers_before_writing_the_next_frame() {
        let mut store = SessionGraphicsStore::new(SessionId::new(12));
        store
            .apply_kitty_command(b"a=T,f=24,i=1,c=2,r=1", b"AQID")
            .unwrap();
        let graphics = store.visible_submissions(Rect::new(0, 0, 4, 2));
        let mut compositor = Compositor::new();
        let mut first_scene = Scene::new(Rect::new(0, 0, 4, 2));
        first_scene.add_image_layer(graphics[0].clone());
        let first = compositor.diff(&first_scene);
        let second = compositor.diff(&Scene::new(Rect::new(0, 0, 4, 2)));
        let capabilities = BackendCapabilities {
            truecolor: true,
            mouse: true,
            bracketed_paste: true,
            kitty_graphics: true,
            kitty_unicode_placeholders: true,
            graphics_source: GraphicsCapabilitySource::EnvironmentHint,
            graphics_confidence: GraphicsCapabilityConfidence::Inferred,
            sixel: false,
        };
        let mut backend = CrosstermBackend::new(Vec::<u8>::new()).with_capabilities(capabilities);
        backend.submit_diff(&first).unwrap();
        backend
            .submit_graphics(
                first.graphics(),
                first.visible_graphics(),
                first.removed_graphics(),
            )
            .unwrap();
        let before = backend.writer().len();
        backend.submit_diff(&second).unwrap();
        let cleanup = &backend.writer()[before..];
        assert!(cleanup.windows(7).any(|window| window == b"a=d,d=i"));
        assert!(cleanup.windows(2).any(|window| window == b"  "));
    }

    #[test]
    fn metrics_include_style_cache_savings_across_separated_runs() {
        let mut backend = CrosstermBackend::new(Vec::<u8>::new());
        let viewport = Rect::new(0, 0, 6, 1);
        let mut compositor = Compositor::new();
        compositor.diff(&Scene::new(viewport));
        let mut scene = Scene::new(viewport);
        let style = CellStyle::new(Color::rgb(255, 255, 255), Color::rgb(0, 0, 0));
        scene.set(0, 0, 'a', style);
        scene.set(2, 0, 'b', style);
        let diff = compositor.diff(&scene);

        assert_eq!(diff.spans().len(), 2);
        backend.submit_diff(&diff).unwrap();

        let metrics = backend.metrics();
        assert!(metrics.bytes_saved > 0);
        assert!(metrics.naive_diff_bytes > metrics.optimized_diff_bytes);
    }
}
