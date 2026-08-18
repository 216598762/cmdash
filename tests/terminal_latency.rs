use std::{
    sync::{
        Arc, Mutex,
        mpsc::{Receiver, TryRecvError},
    },
    time::{Duration, Instant},
};

use cmdash::{SessionId, SessionWakeup, TerminalSession, TerminalSize, UiEvent, ui_event_channel};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;

const TERMINAL_AREA: Rect = Rect::new(0, 0, 80, 24);
const SAMPLE_COUNT: usize = 100;
const WARMUP_COUNT: usize = 10;
const RECEIVE_TIMEOUT: Duration = Duration::from_secs(2);
const READY_MARKER: &str = "__CMDASH_LATENCY_READY__";

#[test]
#[ignore = "run explicitly with --ignored --nocapture to measure local PTY latency"]
fn terminal_key_to_echo_latency_benchmark() {
    let (sender, receiver, wakeup) = ui_event_channel();
    drop(sender);
    let mut session = TerminalSession::spawn_with_session_id_and_wakeup(
        SessionId::new(90_001),
        Some("sh"),
        &[
            "-c",
            "stty -icanon min 1 time 0; printf '__CMDASH_LATENCY_READY__'; cat",
        ],
        TerminalSize::new(80, 24),
        Some(wakeup.clone()),
        "xterm-256color",
        Arc::new(Mutex::new(None)),
    )
    .expect("could not spawn the latency benchmark PTY");

    wait_for_marker(&mut session, &receiver, &wakeup, READY_MARKER)
        .expect("latency benchmark PTY did not become ready");
    drain_events(&receiver, &wakeup);
    session
        .poll_output()
        .expect("could not drain initial PTY output");

    for index in 0..WARMUP_COUNT {
        round_trip(&mut session, &receiver, &wakeup, sample_key(index))
            .expect("latency benchmark warmup failed");
        drain_events(&receiver, &wakeup);
    }

    let mut samples = Vec::with_capacity(SAMPLE_COUNT);
    for index in 0..SAMPLE_COUNT {
        samples.push(
            round_trip(
                &mut session,
                &receiver,
                &wakeup,
                sample_key(index + WARMUP_COUNT),
            )
            .expect("latency benchmark sample failed"),
        );
    }
    session
        .shutdown()
        .expect("could not shut down benchmark PTY");

    samples.sort_unstable();
    println!(
        "terminal key-to-echo latency ({SAMPLE_COUNT} samples): min={} µs, median={} µs, p95={} µs, max={} µs",
        samples[0].as_micros(),
        percentile(&samples, 50).as_micros(),
        percentile(&samples, 95).as_micros(),
        samples[SAMPLE_COUNT - 1].as_micros(),
    );
}

fn wait_for_marker(
    session: &mut TerminalSession,
    receiver: &Receiver<UiEvent>,
    wakeup: &SessionWakeup,
    marker: &str,
) -> Result<(), String> {
    let deadline = Instant::now() + RECEIVE_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(format!("timed out waiting for {marker:?}"));
        }
        match receiver.recv_timeout(remaining) {
            Ok(UiEvent::PtyOutput) => {
                wakeup.clear_pending();
                session.poll_output().map_err(|error| error.to_string())?;
                let rendered = rendered_text(session);
                if rendered.contains(marker) {
                    return Ok(());
                }
            }
            Ok(
                UiEvent::Tick
                | UiEvent::AnimationFrame
                | UiEvent::ApiWakeup
                | UiEvent::CursorBlink(_)
                | UiEvent::ClipboardStore(_)
                | UiEvent::ClipboardRead(_)
                | UiEvent::Bell(_)
                | UiEvent::Notification(_, _)
                | UiEvent::SessionTitle(_, _),
            ) => {}
            Ok(UiEvent::Input(_)) => return Err("unexpected input event".to_owned()),
            Ok(UiEvent::OuterInput(_)) => {}
            Ok(UiEvent::OuterClipboard(_)) => {}
            Ok(UiEvent::InputError(error)) => return Err(error),
            Err(error) => return Err(format!("timed out waiting for PTY output: {error}")),
        }
    }
}

fn round_trip(
    session: &mut TerminalSession,
    receiver: &Receiver<UiEvent>,
    wakeup: &SessionWakeup,
    key: char,
) -> Result<Duration, String> {
    session.poll_output().map_err(|error| error.to_string())?;
    let before = rendered_text(session);
    let started = Instant::now();
    session
        .write_key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE))
        .map_err(|error| error.to_string())?;

    let deadline = started + RECEIVE_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("timed out waiting for key echo".to_owned());
        }
        match receiver.recv_timeout(remaining) {
            Ok(UiEvent::PtyOutput) => {
                wakeup.clear_pending();
                if session.poll_output().map_err(|error| error.to_string())?
                    && rendered_text(session) != before
                {
                    return Ok(started.elapsed());
                }
            }
            Ok(
                UiEvent::Tick
                | UiEvent::AnimationFrame
                | UiEvent::ApiWakeup
                | UiEvent::CursorBlink(_)
                | UiEvent::ClipboardStore(_)
                | UiEvent::ClipboardRead(_)
                | UiEvent::Bell(_)
                | UiEvent::Notification(_, _)
                | UiEvent::SessionTitle(_, _),
            ) => {}
            Ok(UiEvent::Input(_)) => return Err("unexpected input event".to_owned()),
            Ok(UiEvent::OuterInput(_)) => {}
            Ok(UiEvent::OuterClipboard(_)) => {}
            Ok(UiEvent::InputError(error)) => return Err(error),
            Err(error) => return Err(format!("timed out waiting for key echo: {error}")),
        }
    }
}

fn drain_events(receiver: &Receiver<UiEvent>, wakeup: &SessionWakeup) {
    loop {
        match receiver.try_recv() {
            Ok(UiEvent::PtyOutput) => wakeup.clear_pending(),
            Ok(_) => {}
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
        }
    }
    wakeup.clear_pending();
}

fn rendered_text(session: &TerminalSession) -> String {
    let scene = session.render(TERMINAL_AREA, false);
    let mut text =
        String::with_capacity(usize::from(TERMINAL_AREA.width) * usize::from(TERMINAL_AREA.height));
    for y in TERMINAL_AREA.y..TERMINAL_AREA.y.saturating_add(TERMINAL_AREA.height) {
        for x in TERMINAL_AREA.x..TERMINAL_AREA.x.saturating_add(TERMINAL_AREA.width) {
            if let Some(cell) = scene.cell_at(x, y) {
                text.push(cell.symbol);
            }
        }
    }
    text
}

fn sample_key(index: usize) -> char {
    const KEYS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    KEYS[index % KEYS.len()] as char
}

fn percentile(samples: &[Duration], percentile: usize) -> Duration {
    let index = (samples.len() - 1) * percentile / 100;
    samples[index]
}
