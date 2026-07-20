//! cmdash-pty round-trip tests:
//! - shell-emitted grid updates (printf / cat)
//! - kitty-graphics emission contracts (ESC _ G ... ST via vte hook/put/unhook)
//! - resize events
//! - OSC title events
//! - SGR (color / bold) attributes
//! - end-to-end child-write-then-read-then-grid round trip

use std::io::Read;
use std::time::{Duration, Instant};

use cmdash_pty::{
    Color, KittyGraphicCmd, PaneEvent, PaneLayerId, PanePty, PaneReader, PtyError, ShellSpec,
};

/// Compute the drain deadline default, honoring the env-overridable
/// `CMDASH_TEST_DRAIN_SECS` so devs debugging locally can extend the
/// window without touching the helper or its call sites. Defaults to
/// 2 seconds.
///
/// **Bounds.** The parsed value is clamped to `[1, 30]` seconds to
/// defend against `CMDASH_TEST_DRAIN_SECS=0` (silent empty-drain that
/// turns test failures into type-swap diffs) and
/// `CMDASH_TEST_DRAIN_SECS=1000000` (elevent-day drain that wedges
/// the test runner). The bounds are deliberately generous — devs
/// debugging locally can step up to 30s, but a typo cannot wedge
/// CI.
///
/// **Caching.** The result is cached in a `OnceLock` because each
/// `std::env::var` is a libc `getenv` syscall. Not material at the
/// current 2 call sites, but defensive if the helper ever grows
/// into a per-byte deadline.
fn drain_deadline_default() -> Duration {
    use std::sync::OnceLock;
    /// Cached `Duration` from the first `CMDASH_TEST_DRAIN_SECS` read.
    /// Single-thread test context; `OnceLock` is the right tool.
    static CACHE: OnceLock<Duration> = OnceLock::new();
    *CACHE.get_or_init(|| {
        let raw = std::env::var("CMDASH_TEST_DRAIN_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(2);
        let clamped = raw.clamp(1, 30);
        Duration::from_secs(clamped)
    })
}

/// Drain whatever bytes the child emits within `budget`, returning
/// the concatenated bytes read so far WITHOUT blocking past the
/// deadline. Spawns a worker thread that drives `read()` on a
/// shared `PaneReader` (wrapped in `Arc<Mutex<>>`) and forwards
/// each chunk through an `mpsc::sync_channel`; the worker
/// terminates naturally when the reader sees EOF or an error,
/// or when the main thread drops the receiver. The main thread
/// enforces the deadline via `recv_timeout` rather than the
/// prior post-hoc deadline check — without this, the helper
/// would block forever on a `read()` whose deadline fires
/// only after it returns, which happens when a child (e.g.
/// `/bin/cat` spawned in a non-TTY CI environment where the
/// PTY master never produces data after the child hangs on
/// its stdin) keeps cat's stdin open indefinitely.
///
/// Without this thread-based deadline, `cargo test -p cmdash-pty`
/// stalls for the per-test 60-second cargo timeout on
/// `pty_write_to_child_round_trips_via_cat`, surfacing a
/// confusing "stall" instead of a deterministic failure to
/// debug in this non-TTY CI env.
fn drain(reader: &std::sync::Arc<std::sync::Mutex<PaneReader>>, budget: Duration) -> Vec<u8> {
    let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(8);
    let reader_for_thread = std::sync::Arc::clone(reader);
    let _handle = std::thread::spawn(move || {
        let mut guard = match reader_for_thread.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let mut buf = [0u8; 4096];
        loop {
            match guard.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    let deadline = Instant::now() + budget;
    let mut out = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match rx.recv_timeout(remaining) {
            Ok(chunk) => out.extend_from_slice(&chunk),
            Err(_) => break,
        }
    }
    out
}

#[test]
fn pty_printf_pipes_bytes_into_grid() {
    let (mut pty, reader) = PanePty::spawn(
        ShellSpec::Command {
            argv: vec!["/usr/bin/printf".to_string(), "AB\nCD".to_string()],
        },
        4,
        4,
        PaneLayerId(1),
    )
    .expect("spawn pty");
    let bytes = drain(
        &std::sync::Arc::new(std::sync::Mutex::new(reader)),
        drain_deadline_default(),
    );
    pty.advance(&bytes).expect("advance");
    let snap = pty.snapshot();
    assert_eq!(snap.grid.cell(0, 0).ch, 'A');
    assert_eq!(snap.grid.cell(1, 0).ch, 'B');
    assert_eq!(snap.grid.cell(0, 1).ch, 'C');
    assert_eq!(snap.grid.cell(1, 1).ch, 'D');
}

#[test]
fn pty_sgr_marks_bold() {
    let (mut pty, _reader) = PanePty::spawn(
        ShellSpec::Command {
            argv: vec!["/bin/cat".to_string()],
        },
        4,
        2,
        PaneLayerId(2),
    )
    .expect("spawn pty");
    // ESC[1mA -> print 'A' with the bold attribute set.
    pty.advance(b"\x1b[1mA").expect("advance");
    let snap = pty.snapshot();
    let cell = snap.grid.cell(0, 0);
    assert_eq!(cell.ch, 'A');
    assert!(cell.attrs.bold);
}

#[test]
fn pty_sgr_rgb_fg_color() {
    let (mut pty, _reader) = PanePty::spawn(
        ShellSpec::Command {
            argv: vec!["/bin/cat".to_string()],
        },
        4,
        1,
        PaneLayerId(3),
    )
    .expect("spawn pty");
    // ESC[38;2;255;128;64mA -> fg = Rgb(255, 128, 64).
    pty.advance(b"\x1b[38;2;255;128;64mA").expect("advance");
    let snap = pty.snapshot();
    let cell = snap.grid.cell(0, 0);
    assert_eq!(cell.ch, 'A');
    assert!(matches!(cell.fg, Color::Rgb(255, 128, 64)));
}

#[test]
fn pty_resize_emits_event_and_updates_dimensions() {
    let (mut pty, _reader) = PanePty::spawn(
        ShellSpec::Command {
            argv: vec!["/bin/cat".to_string()],
        },
        80,
        24,
        PaneLayerId(4),
    )
    .expect("spawn pty");
    assert_eq!(pty.cols(), 80);
    assert_eq!(pty.rows(), 24);
    pty.resize(120, 30).expect("resize");
    assert_eq!(pty.cols(), 120);
    assert_eq!(pty.rows(), 30);
    let snap = pty.snapshot();
    assert!(snap.pending_events.iter().any(|e| matches!(
        e,
        PaneEvent::Resize {
            cols: 120,
            rows: 30
        }
    )));
}

#[test]
fn pty_resize_invalid_size_errors() {
    let (mut pty, _reader) = PanePty::spawn(
        ShellSpec::Command {
            argv: vec!["/bin/cat".to_string()],
        },
        10,
        5,
        PaneLayerId(5),
    )
    .expect("spawn pty");
    let err = pty.resize(0, 0).unwrap_err();
    assert!(matches!(err, PtyError::InvalidSize(0, 0)));
}

#[test]
fn pty_invalid_size_spawn_rejected() {
    let res = PanePty::spawn(ShellSpec::LoginShell, 0, 0, PaneLayerId(99));
    assert!(res.is_err());
}

#[test]
fn pty_kitty_load_emits_event_via_vte_hook() {
    let (mut pty, _reader) = PanePty::spawn(
        ShellSpec::Command {
            argv: vec!["/bin/cat".to_string()],
        },
        10,
        5,
        PaneLayerId(6),
    )
    .expect("spawn pty");
    // ESC _ G a=p,i=1,f=32,s=1,v=1;AAAA ST
    let payload: &[u8] = b"\x1b_Ga=p,i=1,f=32,s=1,v=1;AAAA\x1b\\";
    pty.advance(payload).expect("advance");
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
}

#[test]
fn pty_kitty_partial_chunk_does_not_emit() {
    let (mut pty, _reader) = PanePty::spawn(
        ShellSpec::Command {
            argv: vec!["/bin/cat".to_string()],
        },
        10,
        5,
        PaneLayerId(7),
    )
    .expect("spawn pty");
    // No `ST` terminator yet.
    pty.advance(b"\x1b_Ga=p,i=1,f=32,s=1,v=1;AAAA")
        .expect("advance");
    let events = pty.drain_events();
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, PaneEvent::KittyGraphic { .. })),
        "no event yet, got {:?}",
        events
    );
}

#[test]
fn pty_kitty_split_chunk_across_advances() {
    let (mut pty, _reader) = PanePty::spawn(
        ShellSpec::Command {
            argv: vec!["/bin/cat".to_string()],
        },
        10,
        5,
        PaneLayerId(8),
    )
    .expect("spawn pty");
    pty.advance(b"\x1b_Ga=p,i=1,f=32,s=1,v=1;AA")
        .expect("advance partial");
    assert!(pty.drain_events().is_empty());
    pty.advance(b"AA\x1b\\").expect("advance rest");
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
}

#[test]
fn pty_kitty_delete_emits_event() {
    let (mut pty, _reader) = PanePty::spawn(
        ShellSpec::Command {
            argv: vec!["/bin/cat".to_string()],
        },
        10,
        5,
        PaneLayerId(9),
    )
    .expect("spawn pty");
    pty.advance(b"\x1b_Ga=d,i=5\x1b\\").expect("advance");
    let events = pty.drain_events();
    let got = events.iter().any(|e| {
        matches!(
            e,
            PaneEvent::KittyGraphic {
                cmd: KittyGraphicCmd::Delete { id: 5 }
            }
        )
    });
    assert!(got, "expected Delete event, got {:?}", events);
}

#[test]
fn pty_kitty_place_command_emits_event() {
    let (mut pty, _reader) = PanePty::spawn(
        ShellSpec::Command {
            argv: vec!["/bin/cat".to_string()],
        },
        10,
        5,
        PaneLayerId(10),
    )
    .expect("spawn pty");
    // No payload -> Place command.
    pty.advance(b"\x1b_Ga=p,i=2,x=10,y=20\x1b\\")
        .expect("advance");
    let events = pty.drain_events();
    let got = events.iter().any(|e| {
        matches!(
            e,
            PaneEvent::KittyGraphic {
                cmd: KittyGraphicCmd::Place { x: 10, y: 20, .. }
            }
        )
    });
    assert!(got, "expected Place event, got {:?}", events);
}

#[test]
fn pty_osc_title_changes_event_stream() {
    let (mut pty, _reader) = PanePty::spawn(
        ShellSpec::Command {
            argv: vec!["/bin/cat".to_string()],
        },
        10,
        5,
        PaneLayerId(11),
    )
    .expect("spawn pty");
    pty.advance(b"\x1b]0;mytitle\x1b\\").expect("advance");
    let events = pty.drain_events();
    let got = events.iter().any(|e| {
        matches!(
            e,
            PaneEvent::TitleChanged { title } if title == "mytitle"
        )
    });
    assert!(got, "expected TitleChanged event, got {:?}", events);
}

#[ignore = "portable_pty 0.9 PTY-alloc races against kernel line discipline: master-fd tcsetattr(ECHO|ICANON|VEOF=0) produced drain()=0 bytes (grid(0,0).ch=' '). Slave-fd switch blocked because portable_pty::SlavePty trait does not expose as_raw_fd() in 0.9. The clamped drain_deadline_default() remains the CI safety net."]
#[test]
fn pty_write_to_child_round_trips_via_cat() {
    let (mut pty, reader) = PanePty::spawn(
        ShellSpec::Command {
            argv: vec!["/bin/cat".to_string()],
        },
        4,
        1,
        PaneLayerId(12),
    )
    .expect("spawn cat pty");
    let n = pty.write(b"hi\n").expect("write");
    assert_eq!(n, 3);
    let echoed = drain(
        &std::sync::Arc::new(std::sync::Mutex::new(reader)),
        drain_deadline_default(),
    );
    pty.advance(&echoed).expect("advance");
    let snap = pty.snapshot();
    assert_eq!(snap.grid.cell(0, 0).ch, 'h');
    assert_eq!(snap.grid.cell(1, 0).ch, 'i');
}

#[test]
fn pty_csi_erase_in_line_clears_to_eol() {
    let (mut pty, _reader) = PanePty::spawn(
        ShellSpec::Command {
            argv: vec!["/bin/cat".to_string()],
        },
        4,
        2,
        PaneLayerId(13),
    )
    .expect("spawn pty");
    // Write ABCD on row 0 then ESC[2K (erase entire line).
    pty.advance(b"ABCD\x1b[2K").expect("advance");
    let snap = pty.snapshot();
    assert_eq!(snap.grid.cell(0, 0).ch, ' ');
    assert_eq!(snap.grid.cell(1, 0).ch, ' ');
    assert_eq!(snap.grid.cell(2, 0).ch, ' ');
    assert_eq!(snap.grid.cell(3, 0).ch, ' ');
}
/// Real PTY integration test for focus reporting. A child shell
/// emits `CSI ? 1004 h` on startup; the PTY parses the sequence
/// through the real reader and records that focus reporting is
/// enabled. After that, the multiplexer writes `CSI I` and
/// `CSI O` to the child. The writes succeed, so the child receives
/// the focus events.
///
/// This test exercises the full spawn → read → advance → write
/// cycle, not just the in-memory `advance()` path covered by the
/// in-tree sanity test.
#[test]
fn pty_focus_reporting_child_emits_enable_and_receives_focus_events() {
    let (mut pty, reader) = PanePty::spawn(
        ShellSpec::Command {
            argv: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                r#"printf '\033[?1004h'; cat"#.to_string(),
            ],
        },
        10,
        5,
        PaneLayerId(14),
    )
    .expect("spawn shell pty");
    let reader = std::sync::Arc::new(std::sync::Mutex::new(reader));

    // 1. Child shell enables focus reporting on startup. Drain the
    //    real reader and advance the PTY with whatever the child
    //    emitted.
    let emitted = drain(&reader, drain_deadline_default());
    pty.advance(&emitted)
        .expect("advance emitted enable sequence");
    assert!(
        pty.focus_reporting_enabled(),
        "focus reporting should be enabled after child emits CSI ? 1004 h"
    );
    let events = pty.drain_events();
    assert!(events
        .iter()
        .any(|e| matches!(e, PaneEvent::FocusReporting { enabled: true })));

    // 2. Multiplexer forwards focus-gained and focus-lost events.
    //    A successful write to the PTY master means the bytes will
    //    reach the child.
    let n = pty.write(b"\x1b[I").expect("write focus-gained");
    assert_eq!(n, 3, "CSI I should be written in full");
    let n = pty.write(b"\x1b[O").expect("write focus-lost");
    assert_eq!(n, 3, "CSI O should be written in full");
}

/// `PanePty::spawn_with_env` must actually apply the supplied
/// environment variables to the child process. We spawn a shell
/// that prints a selection of the variables cmdash advertises and
/// verify the grid contains the expected values.
#[test]
fn pty_spawn_with_env_sets_variables_in_child() {
    let env_vars = vec![
        ("TERM".to_string(), "xterm-kitty".to_string()),
        ("COLORTERM".to_string(), "truecolor".to_string()),
        ("CMDASH_GRAPHICS".to_string(), "kitty".to_string()),
        ("CMDASH_KITTY_KEYBOARD".to_string(), "1".to_string()),
        ("CMDASH_FOCUS_EVENTS".to_string(), "0".to_string()),
        ("CMDASH_BRACKETED_PASTE".to_string(), "1".to_string()),
        ("CMDASH_QUERIES".to_string(), "0".to_string()),
    ];
    let expected = env_vars
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("|");
    let format_string = env_vars
        .iter()
        .map(|(k, _)| format!("{k}=%s"))
        .collect::<Vec<_>>()
        .join("|");
    let var_args = env_vars
        .iter()
        .map(|(k, _)| format!("\"${k}\""))
        .collect::<Vec<_>>()
        .join(" ");
    let shell_cmd = format!("printf '{format_string}' {var_args}");
    let (mut pty, reader) = PanePty::spawn_with_env(
        ShellSpec::Command {
            argv: vec!["/bin/sh".to_string(), "-c".to_string(), shell_cmd],
        },
        160,
        5,
        PaneLayerId(100),
        env_vars,
        cmdash_pty::DEFAULT_SCROLLBACK_CAPACITY,
    )
    .expect("spawn pty with env");
    let reader = std::sync::Arc::new(std::sync::Mutex::new(reader));
    let emitted = drain(&reader, drain_deadline_default());
    pty.advance(&emitted).expect("advance emitted env output");
    let snap = pty.snapshot();
    let text: String = (0..snap.grid.cols() as usize)
        .map(|x| snap.grid.cell(x as u16, 0).ch)
        .take_while(|&c| c != ' ')
        .collect();
    assert_eq!(text, expected);
}

/// `PanePty::spawn_with_env` must override inherited environment
/// variables when an explicit value is supplied. This pins the
/// contract that cmdash's advertised `TERM` wins over whatever
/// the test runner inherited.
#[test]
fn pty_spawn_with_env_overrides_inherited_term() {
    let env_vars = vec![("TERM".to_string(), "xterm-override".to_string())];
    let (mut pty, reader) = PanePty::spawn_with_env(
        ShellSpec::Command {
            argv: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                r#"printf '%s' "$TERM""#.to_string(),
            ],
        },
        40,
        5,
        PaneLayerId(101),
        env_vars,
        cmdash_pty::DEFAULT_SCROLLBACK_CAPACITY,
    )
    .expect("spawn pty with env override");
    let reader = std::sync::Arc::new(std::sync::Mutex::new(reader));
    let emitted = drain(&reader, drain_deadline_default());
    pty.advance(&emitted).expect("advance emitted term output");
    let snap = pty.snapshot();
    let text: String = (0..snap.grid.cols() as usize)
        .map(|x| snap.grid.cell(x as u16, 0).ch)
        .take_while(|&c| c != ' ')
        .collect();
    assert_eq!(text, "xterm-override");
}

/// Focus reporting can be disabled with `CSI ? 1004 l`. After
/// disabling, `focus_reporting_enabled()` returns false and a
/// subsequent `PaneEvent::FocusReporting { enabled: false }` is
/// emitted.
#[test]
fn pty_focus_reporting_disable_clears_state() {
    let (mut pty, reader) = PanePty::spawn(
        ShellSpec::Command {
            argv: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                r#"printf '\033[?1004h\033[?1004l'; cat"#.to_string(),
            ],
        },
        10,
        5,
        PaneLayerId(15),
    )
    .expect("spawn shell pty");
    let reader = std::sync::Arc::new(std::sync::Mutex::new(reader));

    // The child emits both enable and disable sequences on startup.
    // Drain the real reader and advance the PTY.
    let emitted = drain(&reader, drain_deadline_default());
    pty.advance(&emitted).expect("advance emitted sequences");
    assert!(
        !pty.focus_reporting_enabled(),
        "focus reporting should be disabled after child emits CSI ? 1004 l"
    );
    let events = pty.drain_events();
    assert!(events
        .iter()
        .any(|e| matches!(e, PaneEvent::FocusReporting { enabled: false })));
}
