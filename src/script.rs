//! Script-driven dashboard widgets (Phase 17).
//!
//! A `widget` dashboard item is a shell script that is spawned directly (via
//! `/bin/sh -c "<command>"`) and whose stdout renders into the surface. This
//! module owns the bounded process lifecycle: spawn, stdout ring, stderr
//! diagnostics, restart backoff, and SIGTERM/SIGKILL shutdown.

use std::{
    collections::VecDeque,
    io::{Read, Write},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{
        mpsc::{self, Receiver},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use ratatui::layout::Rect;

use crate::{
    appearance::Theme,
    config::{LabelPolicy, WidgetInstanceConfig},
    scene::{CellStyle, Scene},
    session::{SessionWakeup, TerminalSize},
    widget::{
        bordered_chrome, parse_log_line, StatusLevel, Widget, WidgetAppearance, WidgetError,
        WidgetHealth, WidgetUpdate,
    },
};

/// How a script's stdout is consumed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ScriptMode {
    /// Run once and keep reading stdout as it arrives.
    Stream,
    /// Run to EOF and re-run every `interval`.
    Interval,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionEventMode {
    Off,
    Text,
    Json,
}

#[derive(Clone, Debug)]
struct ScriptSettings {
    mode: ScriptMode,
    interval: Duration,
    parse_tags: bool,
    restart: bool,
    handles_input: bool,
    session_env: bool,
    // Parsed and validated now; the fd-3 event pipe delivery is the remaining
    // Phase 17 work item.
    #[allow(dead_code)]
    session_events: SessionEventMode,
    max_lines: usize,
    max_bytes: usize,
}

impl ScriptSettings {
    fn from_settings(
        settings: &std::collections::BTreeMap<String, String>,
    ) -> Result<Self, WidgetError> {
        let mode = match settings.get("mode").map(String::as_str) {
            None | Some("stream") => ScriptMode::Stream,
            Some("interval") => ScriptMode::Interval,
            Some(other) => {
                return Err(WidgetError::InvalidConfiguration(format!(
                    "widget mode must be \"stream\" or \"interval\", got {other:?}"
                )));
            }
        };
        let interval_ms = parse_bounded_u64(settings.get("interval_ms"), 1000, 100, 60_000, "interval_ms")?;
        match settings.get("render").map(String::as_str) {
            None | Some("text") => {}
            Some(other) => {
                return Err(WidgetError::InvalidConfiguration(format!(
                    "widget render must be \"text\", got {other:?}"
                )));
            }
        }
        let parse_tags = parse_bool(settings.get("parse_tags"), false, "parse_tags")?;
        let restart = parse_bool(settings.get("restart"), true, "restart")?;
        let handles_input = parse_bool(settings.get("handles_input"), false, "handles_input")?;
        let session_env = parse_bool(settings.get("session_env"), true, "session_env")?;
        let session_events = match settings.get("session_events").map(String::as_str) {
            None | Some("off") => SessionEventMode::Off,
            Some("text") => SessionEventMode::Text,
            Some("json") => SessionEventMode::Json,
            Some(other) => {
                return Err(WidgetError::InvalidConfiguration(format!(
                    "widget session_events must be \"off\", \"text\", or \"json\", got {other:?}"
                )));
            }
        };
        let max_lines =
            parse_bounded_u64(settings.get("max_lines"), 1024, 1, 1_000_000, "max_lines")? as usize;
        let max_bytes = parse_bounded_u64(
            settings.get("max_bytes"),
            65_536,
            1024,
            16 * 1024 * 1024,
            "max_bytes",
        )? as usize;
        Ok(Self {
            mode,
            interval: Duration::from_millis(interval_ms),
            parse_tags,
            restart,
            handles_input,
            session_env,
            session_events,
            max_lines,
            max_bytes,
        })
    }
}

fn parse_bounded_u64(
    value: Option<&String>,
    default: u64,
    min: u64,
    max: u64,
    name: &str,
) -> Result<u64, WidgetError> {
    let Some(value) = value else {
        return Ok(default);
    };
    let parsed = value.parse::<u64>().map_err(|_| {
        WidgetError::InvalidConfiguration(format!(
            "widget {name} must be an integer between {min} and {max}, got {value:?}"
        ))
    })?;
    if !(min..=max).contains(&parsed) {
        return Err(WidgetError::InvalidConfiguration(format!(
            "widget {name} must be an integer between {min} and {max}, got {parsed}"
        )));
    }
    Ok(parsed)
}

fn parse_bool(value: Option<&String>, default: bool, name: &str) -> Result<bool, WidgetError> {
    let Some(value) = value else {
        return Ok(default);
    };
    match value.as_str() {
        "true" | "yes" | "1" | "on" => Ok(true),
        "false" | "no" | "0" | "off" => Ok(false),
        other => Err(WidgetError::InvalidConfiguration(format!(
            "widget {name} must be a boolean, got {other:?}"
        ))),
    }
}

const MAX_RESTART_ATTEMPTS: u32 = 6;
const CRASH_THRESHOLD: Duration = Duration::from_secs(1);
const STREAM_RESTART_DELAY: Duration = Duration::from_millis(250);
const MAX_BACKOFF: Duration = Duration::from_secs(8);
const STDERR_TAIL_BYTES: usize = 4 * 1024;

/// The bounded, retained lifecycle of a single spawned script.
struct ScriptProcess {
    command: String,
    mode: ScriptMode,
    interval: Duration,
    restart: bool,
    stdin_enabled: bool,
    wakeup: Option<SessionWakeup>,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    stdout_rx: Option<Receiver<Vec<u8>>>,
    stderr_tail: Arc<Mutex<String>>,
    started_at: Option<Instant>,
    consecutive_exits: u32,
    respawn_deadline: Option<Instant>,
    finished: bool,
    last_exit_code: Option<i32>,
}

impl ScriptProcess {
    fn new(
        command: String,
        mode: ScriptMode,
        interval: Duration,
        restart: bool,
        stdin_enabled: bool,
        wakeup: Option<SessionWakeup>,
    ) -> Self {
        Self {
            command,
            mode,
            interval,
            restart,
            stdin_enabled,
            wakeup,
            child: None,
            stdin: None,
            stdout_rx: None,
            stderr_tail: Arc::new(Mutex::new(String::new())),
            started_at: None,
            consecutive_exits: 0,
            respawn_deadline: None,
            finished: false,
            last_exit_code: None,
        }
    }

    /// The delay before a re-spawn after the previous process exits.
    fn restart_delay(&self) -> Duration {
        match self.mode {
            ScriptMode::Interval => self.interval,
            ScriptMode::Stream => {
                let shift = self.consecutive_exits.min(5);
                STREAM_RESTART_DELAY.saturating_mul(1_u32 << shift).min(MAX_BACKOFF)
            }
        }
    }

    fn spawn(&mut self, env: &[(String, String)]) -> Result<(), String> {
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(&self.command)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if self.stdin_enabled {
            command.stdin(Stdio::piped());
        }
        for (key, value) in env {
            command.env(key, value);
        }
        let mut child = command.spawn().map_err(|error| error.to_string())?;
        let stdout: ChildStdout = child.stdout.take().ok_or("script stdout is unavailable")?;
        let stderr = child.stderr.take().ok_or("script stderr is unavailable")?;
        let stdin = child.stdin.take();

        let (sender, receiver) = mpsc::channel();
        let wakeup = self.wakeup.clone();
        thread::spawn(move || {
            let mut reader = stdout;
            let mut buffer = [0_u8; 4096];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(length) => {
                        if sender.send(buffer[..length].to_vec()).is_err() {
                            break;
                        }
                        if let Some(wakeup) = &wakeup {
                            wakeup.notify();
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        let stderr_tail = Arc::clone(&self.stderr_tail);
        thread::spawn(move || {
            let mut reader = stderr;
            let mut buffer = [0_u8; 4096];
            let mut tail = Vec::new();
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(length) => {
                        tail.extend_from_slice(&buffer[..length]);
                        if tail.len() > STDERR_TAIL_BYTES {
                            let excess = tail.len() - STDERR_TAIL_BYTES;
                            tail.drain(..excess);
                        }
                        if let Ok(mut guard) = stderr_tail.lock() {
                            *guard = String::from_utf8_lossy(&tail).into_owned();
                        }
                    }
                }
            }
        });

        self.child = Some(child);
        self.stdin = stdin;
        self.stdout_rx = Some(receiver);
        self.started_at = Some(Instant::now());
        self.respawn_deadline = None;
        Ok(())
    }

    /// Drains any newly arrived stdout bytes into the caller-provided fold.
    fn drain_stdout(&mut self, mut on_bytes: impl FnMut(&[u8])) {
        let Some(receiver) = &self.stdout_rx else {
            return;
        };
        while let Ok(chunk) = receiver.try_recv() {
            on_bytes(&chunk);
        }
    }

    /// Reaps a finished child and schedules the next action, if any.
    fn reap(&mut self, now: Instant) {
        let Some(child) = &mut self.child else {
            return;
        };
        match child.try_wait() {
            Ok(Some(status)) => {
                self.last_exit_code = status.code();
                let run_duration = self.started_at.map_or(Duration::ZERO, |start| now - start);
                if run_duration < CRASH_THRESHOLD {
                    self.consecutive_exits = self.consecutive_exits.saturating_add(1);
                } else {
                    self.consecutive_exits = 0;
                }
                self.child = None;
                self.stdin = None;
                self.stdout_rx = None;
                if self.restart && self.consecutive_exits < MAX_RESTART_ATTEMPTS {
                    self.respawn_deadline = Some(now + self.restart_delay());
                } else {
                    self.finished = true;
                }
            }
            Ok(None) => {}
            Err(_) => {
                self.child = None;
                self.finished = true;
            }
        }
    }

    /// Spawns the next process when a scheduled respawn deadline has passed.
    fn advance(&mut self, now: Instant, env: &[(String, String)]) -> Result<bool, String> {
        if let Some(deadline) = self.respawn_deadline
            && now >= deadline
        {
            self.spawn(env)?;
            return Ok(true);
        }
        Ok(false)
    }

    fn is_failed(&self) -> bool {
        self.finished && self.last_exit_code != Some(0)
    }

    fn stderr_tail(&self) -> String {
        self.stderr_tail
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    fn exit_code(&self) -> Option<i32> {
        self.last_exit_code
    }

    fn write_stdin(&mut self, bytes: &[u8]) -> Result<(), String> {
        let Some(stdin) = &mut self.stdin else {
            return Ok(());
        };
        stdin
            .write_all(bytes)
            .and_then(|_| stdin.flush())
            .map_err(|error| error.to_string())
    }

    fn shutdown(&mut self) -> Result<(), String> {
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        let pid = child.id() as i32;
        #[cfg(unix)]
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
        let grace = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return Ok(()),
                Ok(None) => {
                    if grace.elapsed() >= Duration::from_millis(500) {
                        #[cfg(unix)]
                        unsafe {
                            libc::kill(pid, libc::SIGKILL);
                        }
                        #[cfg(not(unix))]
                        let _ = child.kill();
                    } else {
                        thread::sleep(Duration::from_millis(10));
                    }
                }
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Ok(());
                }
            }
        }
    }
}

impl Drop for ScriptProcess {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

struct ScriptWidget {
    id: u64,
    title: String,
    label: bool,
    settings: ScriptSettings,
    appearance: WidgetAppearance,
    theme: Theme,
    process: ScriptProcess,
    lines: VecDeque<String>,
    ring_bytes: usize,
    overflowed: bool,
    pending: Vec<u8>,
    surface_size: TerminalSize,
    visible: bool,
}

impl ScriptWidget {
    fn env_at_spawn(&self) -> Vec<(String, String)> {
        if !self.settings.session_env {
            return Vec::new();
        }
        vec![
            ("CMDASH_WIDGET_ID".to_owned(), self.id.to_string()),
            ("CMDASH_WIDGET_TITLE".to_owned(), self.title.clone()),
            (
                "CMDASH_SURFACE_COLUMNS".to_owned(),
                self.surface_size.columns.to_string(),
            ),
            (
                "CMDASH_SURFACE_ROWS".to_owned(),
                self.surface_size.rows.to_string(),
            ),
        ]
    }

    fn ingest(&mut self, chunk: &[u8]) -> bool {
        self.pending.extend_from_slice(chunk);
        let mut changed = false;
        while let Some(newline) = self.pending.iter().position(|byte| *byte == b'\n') {
            let rest = self.pending.split_off(newline + 1);
            let mut line = std::mem::replace(&mut self.pending, rest);
            line.pop(); // drop the trailing newline
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = String::from_utf8_lossy(&line).into_owned();
            self.push_line(line);
            changed = true;
        }
        changed
    }

    fn push_line(&mut self, line: String) {
        self.ring_bytes = self.ring_bytes.saturating_add(line.len());
        self.lines.push_back(line);
        while self.lines.len() > self.settings.max_lines
            || self.ring_bytes > self.settings.max_bytes
        {
            if let Some(oldest) = self.lines.pop_front() {
                self.ring_bytes = self.ring_bytes.saturating_sub(oldest.len());
                self.overflowed = true;
            } else {
                break;
            }
        }
    }

    fn compute_health(&self) -> WidgetHealth {
        if self.process.is_failed() {
            let tail = self.process.stderr_tail();
            let detail = if tail.is_empty() {
                format!("exited with code {}", self.process.exit_code().unwrap_or(-1))
            } else {
                tail
            };
            WidgetHealth::Failed(detail)
        } else if self.overflowed {
            WidgetHealth::Degraded(
                "widget output exceeded its ring; oldest lines dropped".to_owned(),
            )
        } else {
            WidgetHealth::Healthy
        }
    }
}

impl Widget for ScriptWidget {
    fn kind(&self) -> &str {
        "widget"
    }

    fn content_area(&self, area: Rect) -> Rect {
        self.appearance.content_area(area)
    }

    fn initialize(&mut self) -> Result<(), String> {
        if self.process.child.is_none() && !self.process.finished {
            let env = self.env_at_spawn();
            self.process.spawn(&env)?;
        }
        Ok(())
    }

    fn update(&mut self, _now: SystemTime) -> Result<WidgetUpdate, String> {
        if !self.visible && self.settings.mode == ScriptMode::Interval {
            // Hidden widgets pause interval re-runs; stream processes keep a
            // bounded ring but do not redraw.
            self.process.reap(Instant::now());
            return Ok(WidgetUpdate::Unchanged);
        }
        let mut changed = false;
        let mut buffer = Vec::new();
        self.process.drain_stdout(|chunk| buffer.extend_from_slice(chunk));
        if !buffer.is_empty() {
            changed |= self.ingest(&buffer);
        }
        let now = Instant::now();
        self.process.reap(now);
        let env = self.env_at_spawn();
        changed |= self
            .process
            .advance(now, &env)
            .map_err(|error| error.to_string())?;
        if changed {
            Ok(WidgetUpdate::Redraw)
        } else {
            Ok(WidgetUpdate::Unchanged)
        }
    }

    fn health(&self) -> WidgetHealth {
        self.compute_health()
    }

    fn render(&self, area: Rect, focused: bool) -> Scene {
        let (mut scene, content_area) = bordered_chrome(
            area,
            if self.label { &self.title } else { "" },
            focused,
            self.theme,
            self.appearance,
        );
        if content_area.width == 0 || content_area.height == 0 {
            return scene;
        }
        let background = self.theme.surface();
        let mut content = Scene::new(content_area);
        content.fill(content_area, CellStyle::new(self.theme.foreground(), background));

        let height = usize::from(content_area.height);
        let start = self.lines.len().saturating_sub(height);
        for (row_offset, line) in self.lines.iter().skip(start).enumerate() {
            let row = content_area.y.saturating_add(row_offset as u16);
            if row >= content_area.y.saturating_add(content_area.height) {
                break;
            }
            let (level, text) = if self.settings.parse_tags {
                parse_log_line(line)
            } else {
                (StatusLevel::Neutral, line.clone())
            };
            let style = CellStyle::new(level.color(self.theme), background);
            content.text(content_area.x, row, &text, style);
        }
        scene.blit(&content, area);
        scene
    }

    fn handles_input(&self) -> bool {
        self.settings.handles_input
    }

    fn handle_key(&mut self, key: KeyEvent) -> Result<WidgetUpdate, String> {
        if !self.settings.handles_input {
            return Ok(WidgetUpdate::Unchanged);
        }
        let bytes = key_event_bytes(key);
        if bytes.is_empty() {
            return Ok(WidgetUpdate::Unchanged);
        }
        self.process.write_stdin(&bytes)?;
        Ok(WidgetUpdate::Unchanged)
    }

    fn handle_paste(&mut self, text: &str) -> Result<WidgetUpdate, String> {
        if !self.settings.handles_input {
            return Ok(WidgetUpdate::Unchanged);
        }
        self.process.write_stdin(text.as_bytes())?;
        Ok(WidgetUpdate::Unchanged)
    }

    fn resize(&mut self, size: TerminalSize) -> Result<WidgetUpdate, String> {
        self.surface_size = size;
        Ok(WidgetUpdate::Unchanged)
    }

    fn handle_mouse(
        &mut self,
        _mouse: MouseEvent,
        _origin: (u16, u16),
    ) -> Result<WidgetUpdate, String> {
        Ok(WidgetUpdate::Unchanged)
    }

    fn shutdown(&mut self) -> Result<(), String> {
        self.process.shutdown()
    }

    fn set_visible(&mut self, visible: bool) {
        self.visible = visible;
    }
}

fn key_event_bytes(key: KeyEvent) -> Vec<u8> {
    match key.code {
        KeyCode::Char(character) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                if character.is_ascii_lowercase() {
                    vec![(character as u8) - b'a' + 1]
                } else {
                    Vec::new()
                }
            } else {
                let mut buffer = [0_u8; 4];
                character.encode_utf8(&mut buffer).as_bytes().to_vec()
            }
        }
        KeyCode::Enter => b"\r".to_vec(),
        KeyCode::Tab => b"\t".to_vec(),
        KeyCode::Backspace => b"\x7f".to_vec(),
        KeyCode::Esc => b"\x1b".to_vec(),
        _ => Vec::new(),
    }
}

pub(crate) fn script_widget_factory(
    config: &WidgetInstanceConfig,
    context: &crate::widget::WidgetRuntimeContext,
) -> Result<Box<dyn Widget>, WidgetError> {
    let command = config.command.clone().ok_or_else(|| {
        WidgetError::InvalidConfiguration(
            "widget type requires a command (the shell script to run)".to_owned(),
        )
    })?;
    if command.trim().is_empty() {
        return Err(WidgetError::InvalidConfiguration(
            "widget command cannot be empty".to_owned(),
        ));
    }
    let settings = ScriptSettings::from_settings(&config.settings)?;
    let appearance = WidgetAppearance::from_settings(&config.settings)?;
    let theme = context
        .theme()
        .with_settings(&config.settings)
        .map_err(|error| WidgetError::InvalidConfiguration(error.to_string()))?;
    let surface_size = context
        .initial_terminal_size()
        .unwrap_or_else(|| TerminalSize::new(80, 24));
    let process = ScriptProcess::new(
        command,
        settings.mode,
        settings.interval,
        settings.restart,
        settings.handles_input,
        context.session_wakeup().cloned(),
    );
    Ok(Box::new(ScriptWidget {
        id: config.id,
        title: config.title.clone().unwrap_or_else(|| " widget ".to_owned()),
        label: config.label != LabelPolicy::Never,
        settings,
        appearance,
        theme,
        process,
        lines: VecDeque::new(),
        ring_bytes: 0,
        overflowed: false,
        pending: Vec::new(),
        surface_size,
        visible: true,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::widget::Widget;

    fn make_widget(command: &str, mode: ScriptMode) -> ScriptWidget {
        make_widget_with(command, mode, &[])
    }

    fn make_widget_with(
        command: &str,
        mode: ScriptMode,
        settings: &[(&str, &str)],
    ) -> ScriptWidget {
        let mut map = std::collections::BTreeMap::new();
        for (key, value) in settings {
            map.insert((*key).to_owned(), (*value).to_owned());
        }
        let parsed = ScriptSettings::from_settings(&map).unwrap();
        let (restart, handles_input) = (parsed.restart, parsed.handles_input);
        ScriptWidget {
            id: 1,
            title: " test ".to_owned(),
            label: true,
            settings: parsed,
            appearance: WidgetAppearance::default(),
            theme: Theme::default(),
            process: ScriptProcess::new(
                command.to_owned(),
                mode,
                Duration::from_millis(100),
                restart,
                handles_input,
                None,
            ),
            lines: VecDeque::new(),
            ring_bytes: 0,
            overflowed: false,
            pending: Vec::new(),
            surface_size: TerminalSize::new(80, 24),
            visible: true,
        }
    }

    fn pump(widget: &mut ScriptWidget, until: impl Fn(&ScriptWidget) -> bool, max_ticks: usize) {
        widget.initialize().unwrap();
        for _ in 0..max_ticks {
            let _ = widget.update(SystemTime::now());
            if until(widget) {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn stream_script_renders_its_stdout_lines() {
        let mut widget = make_widget("printf 'hello\\nworld\\n'", ScriptMode::Stream);
        pump(&mut widget, |w| w.lines.len() >= 2, 100);
        assert_eq!(
            widget.lines.iter().cloned().collect::<Vec<_>>(),
            vec!["hello".to_owned(), "world".to_owned()]
        );
        widget.shutdown().unwrap();
    }

    #[test]
    fn interval_script_reruns_on_the_configured_cadence() {
        let mut widget = make_widget_with("printf 'tick\\n'", ScriptMode::Interval, &[("interval_ms", "100")]);
        pump(&mut widget, |w| w.lines.len() >= 2, 80);
        assert!(
            widget.lines.len() >= 2,
            "interval script should re-run at least once, got {:?}",
            widget.lines
        );
        widget.shutdown().unwrap();
    }

    #[test]
    fn ring_drops_oldest_lines_and_records_overflow() {
        let mut widget = make_widget_with(
            "printf 'a\\nb\\nc\\nd\\ne\\n'",
            ScriptMode::Stream,
            &[("max_lines", "2")],
        );
        pump(&mut widget, |w| w.lines.len() >= 2, 100);
        assert_eq!(
            widget.lines.iter().cloned().collect::<Vec<_>>(),
            vec!["d".to_owned(), "e".to_owned()]
        );
        assert!(widget.overflowed);
        assert!(matches!(widget.compute_health(), WidgetHealth::Degraded(_)));
        widget.shutdown().unwrap();
    }

    #[test]
    fn parse_tags_styles_lines_by_severity_prefix() {
        let mut widget = make_widget_with(
            "printf '[error] boom\\n[info] fine\\nplain\\n'",
            ScriptMode::Stream,
            &[("parse_tags", "true")],
        );
        pump(&mut widget, |w| w.lines.len() >= 3, 100);
        assert_eq!(widget.lines.len(), 3);
        assert_eq!(
            parse_log_line("[error] boom"),
            (StatusLevel::Error, "boom".to_owned())
        );
        assert_eq!(
            parse_log_line("[info] fine"),
            (StatusLevel::Neutral, "fine".to_owned())
        );
        assert_eq!(
            parse_log_line("plain"),
            (StatusLevel::Neutral, "plain".to_owned())
        );
        widget.shutdown().unwrap();
    }

    #[test]
    fn restart_disabled_marks_an_exiting_script_failed() {
        let mut widget = make_widget_with(
            "exit 3",
            ScriptMode::Stream,
            &[("restart", "false")],
        );
        pump(&mut widget, |w| w.process.finished, 100);
        assert!(widget.process.finished);
        assert!(matches!(widget.compute_health(), WidgetHealth::Failed(_)));
        widget.shutdown().unwrap();
    }

    #[test]
    fn settings_reject_unknown_modes_and_out_of_range_intervals() {
        let mut map = std::collections::BTreeMap::new();
        map.insert("mode".to_owned(), "batch".to_owned());
        assert!(ScriptSettings::from_settings(&map).is_err());

        let mut map = std::collections::BTreeMap::new();
        map.insert("interval_ms".to_owned(), "50".to_owned());
        assert!(ScriptSettings::from_settings(&map).is_err());

        let mut map = std::collections::BTreeMap::new();
        map.insert("render".to_owned(), "chart".to_owned());
        assert!(ScriptSettings::from_settings(&map).is_err());
    }

    #[test]
    fn factory_requires_a_command() {
        let context = crate::widget::WidgetRuntimeContext::new();
        let config = WidgetInstanceConfig {
            id: 7,
            kind: "widget".to_owned(),
            title: None,
            label: LabelPolicy::Auto,
            text: None,
            format: None,
            command: None,
            settings: Default::default(),
        };
        assert!(script_widget_factory(&config, &context).is_err());
    }
}
