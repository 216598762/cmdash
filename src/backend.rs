use std::io::{self, Write};

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
    graphics::GraphicsSubmission,
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
pub struct BackendCapabilities {
    pub truecolor: bool,
    pub mouse: bool,
    pub bracketed_paste: bool,
    pub kitty_graphics: bool,
}

impl BackendCapabilities {
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

        Self {
            truecolor: color_hint.contains("truecolor") || color_hint.contains("24bit"),
            mouse: true,
            bracketed_paste: true,
            kitty_graphics: terminal_hint.contains("kitty") || program_hint.contains("kitty"),
        }
    }
}

pub trait Backend {
    type Error;

    fn capabilities(&self) -> BackendCapabilities;
    fn metrics(&self) -> OutputMetrics;
    fn size(&self) -> Result<Rect, Self::Error>;
    fn enter(&mut self) -> Result<(), Self::Error>;
    fn leave(&mut self) -> Result<(), Self::Error>;
    fn submit(&mut self, scene: &Scene) -> Result<(), Self::Error>;
    fn submit_diff(&mut self, diff: &FrameDiff) -> Result<(), Self::Error>;

    fn submit_graphics(
        &mut self,
        _graphics: &[GraphicsSubmission],
        _removed: &[u32],
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

pub struct CrosstermBackend<W: Write> {
    writer: ByteCountingWriter<W>,
    capabilities: BackendCapabilities,
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
        let (width, height) = terminal::size()?;
        Ok(Rect::new(0, 0, width, height))
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
        graphics: &[GraphicsSubmission],
        removed: &[u32],
    ) -> Result<(), Self::Error> {
        if !self.capabilities.kitty_graphics {
            return Ok(());
        }
        for image_id in removed {
            write!(self.writer, "\x1b_Ga=d,d=i,i={image_id};\x1b\\")?;
        }
        for submission in graphics {
            let physical_id = submission.terminal_image_id();
            write!(
                self.writer,
                "\x1b_Ga=T,f={},i={},m=0;",
                submission.format(),
                physical_id
            )?;
            self.writer.write_all(submission.encoded_payload())?;
            self.writer.write_all(b"\x1b\\")?;
            let placement = submission.placement();
            write!(
                self.writer,
                "\x1b_Ga=p,i={},x={},y={},c={},r={};\x1b\\",
                physical_id,
                placement.x(),
                placement.y(),
                placement.width(),
                placement.height()
            )?;
        }
        self.writer.flush()
    }

    fn submit_diff(&mut self, diff: &FrameDiff) -> Result<(), Self::Error> {
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

impl<W: Write> Drop for CrosstermBackend<W> {
    fn drop(&mut self) {
        let _ = self.leave();
    }
}

fn to_crossterm_color(color: Color) -> crossterm::style::Color {
    crossterm::style::Color::Rgb {
        r: color.red,
        g: color.green,
        b: color.blue,
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
    fn backend_capabilities_are_stable_for_a_constructed_backend() {
        let backend = CrosstermBackend::new(Vec::<u8>::new());
        assert!(backend.capabilities().mouse);
        assert!(backend.capabilities().bracketed_paste);
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
        };
        let mut backend = CrosstermBackend::new(Vec::<u8>::new()).with_capabilities(capabilities);
        backend.submit_graphics(&graphics, &[]).unwrap();

        let output = backend.writer();
        assert!(output.windows(4).any(|window| window == b"a=T,"));
        assert!(output.windows(4).any(|window| window == b"a=p,"));
        assert!(output.windows(3).any(|window| window == b"AQI"));
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
