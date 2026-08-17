use std::{
    process::Command,
    thread,
    time::{Duration, Instant},
};

use cmdash::{
    GraphicsProtocolAdapter, GraphicsProtocolEvent, SessionId, TerminalSession, TerminalSize,
};
use ratatui::layout::Rect;

const TERMINAL_AREA: Rect = Rect::new(0, 0, 80, 12);
const RECEIVE_TIMEOUT: Duration = Duration::from_secs(2);
const NEGOTIATION_MARKER: &str = "icat-negotiation-ok";
const UPLOAD_MARKER: &str = "icat-upload-ok";
const TINY_GIF: &[u8] = &[
    0x47, 0x49, 0x46, 0x38, 0x39, 0x61, 0x01, 0x00, 0x01, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
    0xff, 0xff, 0xff, 0x21, 0xf9, 0x04, 0x01, 0x00, 0x00, 0x00, 0x00, 0x2c, 0x00, 0x00, 0x00, 0x00,
    0x01, 0x00, 0x01, 0x00, 0x00, 0x02, 0x01, 0x44, 0x00, 0x3b,
];
const ANIMATED_GIF: &[u8] = &[
    0x47, 0x49, 0x46, 0x38, 0x39, 0x61, 0x01, 0x00, 0x01, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
    0xff, 0xff, 0xff, 0x21, 0xf9, 0x04, 0x01, 0x0a, 0x00, 0x00, 0x00, 0x2c, 0x00, 0x00, 0x00, 0x00,
    0x01, 0x00, 0x01, 0x00, 0x00, 0x02, 0x01, 0x44, 0x00, 0x21, 0xf9, 0x04, 0x01, 0x0a, 0x00, 0x00,
    0x00, 0x2c, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x02, 0x01, 0x44, 0x00, 0x3b,
];

fn write_image_fixture(name: &str, bytes: &[u8]) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("cmdash-kitty-{name}-{}.gif", std::process::id()));
    std::fs::write(&path, bytes).expect("could not write the Kitty image fixture");
    path
}

/// A shell-based stand-in for kitty's `kitten icat` detection handshake.
///
/// It emits the direct, file, and shared-memory Kitty query commands followed
/// by DA1, reads the responses from the child PTY, and then emits a quiet
/// transmit-and-display command. The file and shared-memory probes point at
/// real 1x1 RGB payloads created by the fixture, because a query now loads
/// and validates its payload before replying OK. No Kitty terminal is needed:
/// the test exercises cmdash's terminal-emulator response and retained-placement
/// paths directly.
struct IcatNegotiationFixture {
    script: String,
}

impl IcatNegotiationFixture {
    fn new() -> Self {
        let pid = std::process::id();
        let file_path = std::env::temp_dir().join(format!("cmdash-icat-file-{pid}"));
        std::fs::write(&file_path, [1, 2, 3]).expect("could not write the icat file fixture");
        let file_name = encode_base64_for_test(file_path.to_str().expect("fixture path is UTF-8"));
        let shm_name = create_query_shm(pid);
        let script = format!(
            r#"
                stty raw -echo
                printf '\033_Ga=q,i=1,t=d,s=1,v=1,f=24;MTIz\033\\\033_Ga=q,i=2,t=f,s=1,v=1,f=24;{file_name}\033\\\033_Ga=q,i=3,t=s,s=1,v=1,f=24;{shm_name}\033\\\033[c'
                direct=$(dd bs=1 count=11 2>/dev/null)
                file=$(dd bs=1 count=11 2>/dev/null)
                memory=$(dd bs=1 count=11 2>/dev/null)
                da=$(dd bs=1 count=7 2>/dev/null)
                if [ "$direct" = "$(printf '\033_Gi=1;OK\033\\')" ] && \
                   [ "$file" = "$(printf '\033_Gi=2;OK\033\\')" ] && \
                   [ "$memory" = "$(printf '\033_Gi=3;OK\033\\')" ] && \
                   [ "$da" = "$(printf '\033[?1;2c')" ]; then
                    printf 'icat-negotiation-ok'
                    printf '\033_Ga=T,f=24,i=42,s=2,v=1,c=2,r=2,q=2,m=1;AQID\033\\'
                    printf '\033_Gm=0;BAUG\033\\'
                    printf 'icat-upload-ok'
                else
                    printf 'icat-negotiation-failed'
                fi
                sleep 5
            "#
        );
        Self { script }
    }

    fn spawn(&self) -> TerminalSession {
        TerminalSession::spawn_with_session_id(
            SessionId::new(200_001),
            Some("sh"),
            &["-c", self.script.as_str()],
            TerminalSize::new(80, 12),
        )
        .expect("could not spawn the icat negotiation fixture")
    }
}

/// Creates a POSIX shared-memory object holding a 1x1 RGB payload (3 bytes)
/// and returns its base64-encoded name for the `t=s` query probe.
#[cfg(unix)]
fn create_query_shm(pid: u32) -> String {
    use std::ffi::CString;
    let name = format!("/cmdash-icat-shm-{pid}");
    let cname = CString::new(name.as_str()).unwrap();
    unsafe {
        let fd = libc::shm_open(
            cname.as_ptr(),
            libc::O_CREAT | libc::O_RDWR | libc::O_EXCL,
            0o600,
        );
        assert!(fd >= 0, "shm_open failed for the icat fixture");
        assert_eq!(libc::ftruncate(fd, 3), 0);
        let pixels = [1, 2, 3];
        let written = libc::write(fd, pixels.as_ptr() as *const libc::c_void, pixels.len());
        assert_eq!(written, pixels.len() as libc::ssize_t);
        libc::close(fd);
    }
    encode_base64_for_test(name)
}

#[cfg(not(unix))]
fn create_query_shm(_pid: u32) -> String {
    String::new()
}

fn encode_base64_for_test(bytes: impl AsRef<[u8]>) -> String {
    const TABLE: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = bytes.as_ref();
    let mut output = String::new();
    for chunk in bytes.chunks(3) {
        let value = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        output.push(TABLE[((value >> 18) & 63) as usize] as char);
        output.push(TABLE[((value >> 12) & 63) as usize] as char);
        output.push(if chunk.len() > 1 {
            TABLE[((value >> 6) & 63) as usize] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            TABLE[(value & 63) as usize] as char
        } else {
            '='
        });
    }
    output
}

fn decode_base64_for_test(encoded: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    let mut accumulator = 0u32;
    let mut bits = 0u8;
    for byte in encoded.iter().copied().filter(|byte| *byte != b'=') {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => continue,
        };
        accumulator = (accumulator << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push((accumulator >> bits) as u8);
            accumulator &= (1 << bits) - 1;
        }
    }
    output
}

#[test]
fn icat_negotiation_receives_graphics_and_da1_responses_without_kitty() {
    let fixture = IcatNegotiationFixture::new();
    let mut session = fixture.spawn();
    let deadline = Instant::now() + RECEIVE_TIMEOUT;

    let found_marker = loop {
        if Instant::now() >= deadline {
            break false;
        }
        session
            .poll_output()
            .expect("icat negotiation fixture PTY failed");
        let scene = session.render(TERMINAL_AREA, false);
        let mut rendered = String::new();
        for y in TERMINAL_AREA.y..TERMINAL_AREA.y.saturating_add(TERMINAL_AREA.height) {
            for x in TERMINAL_AREA.x..TERMINAL_AREA.x.saturating_add(TERMINAL_AREA.width) {
                if let Some(cell) = scene.cell_at(x, y) {
                    rendered.push(cell.symbol);
                }
            }
        }
        if rendered.contains(UPLOAD_MARKER) {
            break true;
        }
        thread::sleep(Duration::from_millis(10));
    };

    assert!(
        found_marker,
        "icat negotiation fixture did not receive the expected responses and upload marker"
    );
    let submissions = session.graphics(TERMINAL_AREA);
    assert_eq!(submissions.len(), 1);
    assert_eq!(submissions[0].resource().image(), 42);
    assert_eq!(submissions[0].format(), 24);
    assert_eq!(submissions[0].encoded_payload(), b"AQIDBAUG");
    assert_eq!(
        submissions[0].placement().area(),
        Rect::new(NEGOTIATION_MARKER.len() as u16, 0, 2, 2)
    );
    let scene = session.render(TERMINAL_AREA, false);
    let mut rendered = String::new();
    for y in TERMINAL_AREA.y..TERMINAL_AREA.y.saturating_add(TERMINAL_AREA.height) {
        for x in TERMINAL_AREA.x..TERMINAL_AREA.x.saturating_add(TERMINAL_AREA.width) {
            if let Some(cell) = scene.cell_at(x, y) {
                rendered.push(cell.symbol);
            }
        }
    }
    assert!(rendered.contains(NEGOTIATION_MARKER));
    session
        .shutdown()
        .expect("could not shut down the icat negotiation fixture");
}

#[test]
#[ignore = "requires the installed kitten executable, but not a Kitty terminal"]
fn installed_kitten_detect_support_completes_inside_the_pty_fixture() {
    let mut session = TerminalSession::spawn_with_session_id(
        SessionId::new(200_002),
        Some("kitten"),
        &["icat", "--detect-support"],
        TerminalSize::with_pixels(80, 12, 800, 240),
    )
    .expect("could not spawn the installed kitten executable");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut rendered = String::new();
    while Instant::now() < deadline {
        session.poll_output().expect("installed kitten PTY failed");
        let scene = session.render(TERMINAL_AREA, false);
        rendered.clear();
        for y in TERMINAL_AREA.y..TERMINAL_AREA.y.saturating_add(TERMINAL_AREA.height) {
            for x in TERMINAL_AREA.x..TERMINAL_AREA.x.saturating_add(TERMINAL_AREA.width) {
                if let Some(cell) = scene.cell_at(x, y) {
                    rendered.push(cell.symbol);
                }
            }
        }
        if rendered.contains("stream") || session.is_closed() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        rendered.contains("stream") || rendered.contains("files") || rendered.contains("memory"),
        "installed kitten did not complete graphics detection: closed={}, failure={:?}, output={rendered:?}",
        session.is_closed(),
        session.failure(),
    );
    session
        .shutdown()
        .expect("could not shut down the installed kitten fixture");
}

#[test]
#[ignore = "requires the installed kitten executable, but not a Kitty terminal"]
fn installed_kitten_image_upload_reaches_the_retained_graphics_store() {
    let path = write_image_fixture("upload", TINY_GIF);

    let mut session = TerminalSession::spawn_with_session_id(
        SessionId::new(200_003),
        Some("kitten"),
        &[
            "icat",
            "--use-window-size",
            "80,12,800,240",
            "--stdin=no",
            path.to_str().expect("fixture path is not valid UTF-8"),
        ],
        TerminalSize::with_pixels(80, 12, 800, 240),
    )
    .expect("could not spawn kitten image fixture");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut submissions = Vec::new();
    let mut rendered = String::new();
    while Instant::now() < deadline {
        session
            .poll_output()
            .expect("kitten image fixture PTY failed");
        submissions = session.graphics(TERMINAL_AREA);
        let scene = session.render(TERMINAL_AREA, false);
        rendered.clear();
        for y in TERMINAL_AREA.y..TERMINAL_AREA.y.saturating_add(TERMINAL_AREA.height) {
            for x in TERMINAL_AREA.x..TERMINAL_AREA.x.saturating_add(TERMINAL_AREA.width) {
                if let Some(cell) = scene.cell_at(x, y) {
                    rendered.push(cell.symbol);
                }
            }
        }
        if !submissions.is_empty() || session.is_closed() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let _ = std::fs::remove_file(&path);

    assert!(
        !submissions.is_empty(),
        "kitten uploaded no retained graphics; session closed={}, failure={:?}, output={rendered:?}",
        session.is_closed(),
        session.failure()
    );
    session
        .shutdown()
        .expect("could not shut down kitten image fixture");
}

#[test]
#[ignore = "requires the installed kitten executable, but not a Kitty terminal"]
fn installed_kitten_file_transfer_stream_reaches_the_retained_graphics_store() {
    // Drive kitten's real file-transfer fast path (`--transfer-mode file`):
    // kitten writes the decoded pixels to a `kitty-tty-graphics-protocol-*`
    // temp file and transmits `t=t` with that path. The store must read it,
    // retain the image, and delete the temp file (marker convention).
    let path = write_image_fixture("file-transfer", TINY_GIF);
    if !Command::new("kitten")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        eprintln!("skipping file-transfer fixture: kitten is not installed");
        let _ = std::fs::remove_file(&path);
        return;
    }
    // The store deletes a `t=t` temp file after reading it (marker
    // convention). Snapshot the shm dir before/after so a parallel test's
    // leftover files don't make this flaky.
    let shm_before = shm_temp_files();

    let mut session = TerminalSession::spawn_with_session_id(
        SessionId::new(200_009),
        Some("kitten"),
        &[
            "icat",
            "--use-window-size",
            "80,12,800,240",
            "--transfer-mode",
            "file",
            "--stdin=no",
            path.to_str().expect("fixture path is not valid UTF-8"),
        ],
        TerminalSize::with_pixels(80, 12, 800, 240),
    )
    .expect("could not spawn kitten file-transfer fixture");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut submissions = Vec::new();
    while Instant::now() < deadline {
        session
            .poll_output()
            .expect("kitten file-transfer PTY failed");
        submissions = session.graphics(TERMINAL_AREA);
        if !submissions.is_empty() || session.is_closed() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let _ = std::fs::remove_file(&path);

    assert!(
        !submissions.is_empty(),
        "kitten file transfer uploaded no retained graphics; closed={}, failure={:?}, diagnostics={:?}",
        session.is_closed(),
        session.failure(),
        session.graphics_diagnostics()
    );
    // kitten transmits `t=t` (temp-file) for --transfer-mode file, so the
    // capture must carry the temp-file marker path (base64-encoded in the
    // payload).
    let capture = session.graphics_protocol_capture();
    let uses_file_transfer = capture.windows(3).any(|window| window == b"t=t")
        && capture
            .split(|byte| *byte == b';')
            .nth(1)
            .is_some_and(|payload| {
                String::from_utf8_lossy(&decode_base64_for_test(payload))
                    .contains("tty-graphics-protocol")
            });
    assert!(
        uses_file_transfer,
        "kitten did not use the file-transfer fast path: capture={:?}",
        String::from_utf8_lossy(capture)
    );
    // The 1x1 GIF decodes to a single RGBA pixel (4 bytes) in the retained
    // store; the temp file is deleted after reading.
    let submission = &submissions[0];
    assert_eq!(submission.pixel_width(), 1);
    assert_eq!(submission.pixel_height(), 1);
    // The file-transfer path delivers the *decoded* pixel, not the raw GIF
    // bytes: TINY_GIF is a 1x1 GIF89a with transparency enabled and
    // transparent index 0, so kitten ships a single fully-transparent RGBA
    // pixel (4 bytes, alpha 0) through the temp file.
    let decoded = decode_base64_for_test(submission.encoded_payload());
    assert_eq!(decoded, [0, 0, 0, 0]);
    session
        .shutdown()
        .expect("could not shut down kitten file-transfer fixture");
    // The store removed the temp file it read (no *new* marker files remain).
    let after = shm_temp_files();
    assert!(
        after.len() <= shm_before.len(),
        "t=t temp file was not deleted after reading: new files={:?}",
        after.iter().filter(|name| !shm_before.contains(name)).collect::<Vec<_>>()
    );
}

/// Lists the `kitty-tty-graphics-protocol-*` temp files currently in the
/// shared-memory directory (where kitten writes them for `--transfer-mode
/// file`).
fn shm_temp_files() -> Vec<String> {
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/dev/shm") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("kitty-tty-graphics-protocol-") {
                files.push(name);
            }
        }
    }
    files
}

#[test]
#[ignore = "requires the installed kitten executable, but not a Kitty terminal"]
fn installed_kitten_place_option_reaches_the_expected_retained_geometry() {
    let path = write_image_fixture("place", TINY_GIF);
    let mut session = TerminalSession::spawn_with_session_id(
        SessionId::new(200_004),
        Some("kitten"),
        &[
            "icat",
            "--use-window-size",
            "80,12,800,240",
            "--place",
            "2x1@3x2",
            "--scale-up",
            "--transfer-mode",
            "stream",
            "--stdin=no",
            path.to_str().expect("fixture path is not valid UTF-8"),
        ],
        TerminalSize::with_pixels(80, 12, 800, 240),
    )
    .expect("could not spawn kitten placement fixture");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut submissions = Vec::new();
    while Instant::now() < deadline {
        session.poll_output().expect("kitten placement PTY failed");
        submissions = session.graphics(TERMINAL_AREA);
        if !submissions.is_empty() || session.is_closed() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let _ = std::fs::remove_file(&path);

    assert_eq!(submissions.len(), 1, "kitten emitted no retained placement");
    assert_eq!(submissions[0].placement().area(), Rect::new(3, 2, 2, 1));
    session
        .shutdown()
        .expect("could not shut down kitten placement fixture");
}

#[test]
#[ignore = "requires the installed kitten executable, but not a Kitty terminal"]
fn installed_kitten_unicode_placeholder_option_reaches_the_pty_session() {
    let path = write_image_fixture("placeholder", TINY_GIF);
    let mut session = TerminalSession::spawn_with_session_id(
        SessionId::new(200_005),
        Some("kitten"),
        &[
            "icat",
            "--use-window-size",
            "80,12,800,240",
            "--place",
            "1x1@3x2",
            "--unicode-placeholder",
            "--transfer-mode",
            "stream",
            "--stdin=no",
            path.to_str().expect("fixture path is not valid UTF-8"),
        ],
        TerminalSize::with_pixels(80, 12, 800, 240),
    )
    .expect("could not spawn kitten placeholder fixture");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut submissions = Vec::new();
    let mut cell_is_placeholder = false;
    while Instant::now() < deadline {
        session
            .poll_output()
            .expect("kitten placeholder PTY failed");
        submissions = session.graphics(TERMINAL_AREA);
        cell_is_placeholder = session
            .render(TERMINAL_AREA, false)
            .cell_at(3, 2)
            .is_some_and(|cell| cell.symbol != ' ');
        if cell_is_placeholder {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let _ = std::fs::remove_file(&path);

    assert!(
        cell_is_placeholder,
        "Kitty placeholder did not reach the PTY scene"
    );
    // A U=1 virtual placement reserves the cell for the outer terminal's
    // placeholder layer without rendering as a visible graphics submission.
    assert_eq!(
        submissions.len(),
        0,
        "a virtual (U=1) placeholder must not produce a visible graphics submission"
    );
    session
        .shutdown()
        .expect("could not shut down kitten placeholder fixture");
}

#[test]
#[ignore = "requires installed kitten and tmux executables, but not a Kitty terminal"]
fn installed_kitten_tmux_passthrough_reaches_the_session_adapter() {
    let path = write_image_fixture("passthrough", TINY_GIF);
    if !Command::new("tmux")
        .arg("-V")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        eprintln!("skipping passthrough fixture: tmux is not installed");
        let _ = std::fs::remove_file(&path);
        return;
    }
    // icat writes graphics to the PTY's controlling TTY, not redirected
    // stdout. The fixture starts a private tmux server so icat can query the
    // passthrough policy, while the session still receives the wrapped bytes.
    let socket = std::env::temp_dir().join(format!("cmdash-tmux-{}", std::process::id()));
    let session_name = format!("cmdash-{}", std::process::id());
    let script = format!(
        "tmux -S {} new-session -d -s {} && pid=$(tmux -S {} display-message -p '#{{pid}}') && TMUX={},$pid,0 TMUX_PANE=%0 TERM=screen-256color kitten icat --use-window-size 80,12,800,240 --place 1x1@2x1 --passthrough tmux --transfer-mode stream --stdin=no {} ; result=$?; tmux -S {} kill-server >/dev/null 2>&1; exit $result",
        socket.display(),
        session_name,
        socket.display(),
        socket.display(),
        path.display(),
        socket.display()
    );
    let mut session = TerminalSession::spawn_with_session_id(
        SessionId::new(200_006),
        Some("sh"),
        &["-c", &script],
        TerminalSize::with_pixels(80, 12, 800, 240),
    )
    .expect("could not spawn kitten passthrough fixture");
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !session.is_closed() {
        session
            .poll_output()
            .expect("kitten passthrough PTY failed");
        thread::sleep(Duration::from_millis(10));
    }
    let bytes = session.graphics_protocol_capture();
    assert!(
        bytes.windows(7).any(|window| window == b"\x1bPtmux;"),
        "Kitty emitted no tmux wrapper: bytes={bytes:?}"
    );
    let _ = std::fs::remove_file(&path);

    let mut adapter = GraphicsProtocolAdapter::default();
    let mut events = adapter
        .feed(bytes)
        .expect("passthrough capture was rejected");
    events.extend(
        adapter
            .finish()
            .expect("passthrough capture was incomplete"),
    );
    let commands = events
        .into_iter()
        .filter_map(|event| match event {
            GraphicsProtocolEvent::Command(command) => Some(command),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        !commands.is_empty(),
        "Kitty emitted no passthrough graphics command: bytes={bytes:?}"
    );
    assert!(
        commands.iter().any(|command| {
            command.parameters().windows(3).any(|field| field == b"a=T")
                && command.parameters().windows(3).any(|field| field == b"U=1")
        }),
        "unexpected installed passthrough parameters: {:?}",
        commands
            .iter()
            .map(|command| String::from_utf8_lossy(command.parameters()).into_owned())
            .collect::<Vec<_>>()
    );
    // The passthrough carried a U=1 virtual placement, which is invisible in
    // the visible-submissions view by design (it only reserves the cell for
    // the outer terminal's placeholder layer).
    assert_eq!(
        session.graphics(TERMINAL_AREA).len(),
        0,
        "a virtual (U=1) passthrough placement must not produce a visible graphics submission: diagnostics={:?}",
        session.graphics_diagnostics()
    );
    session
        .shutdown()
        .expect("could not shut down kitten passthrough fixture");
}

#[test]
#[ignore = "requires the installed kitten executable, but not a Kitty terminal"]
fn installed_kitten_animation_reaches_the_retained_frame_store() {
    let path = write_image_fixture("animation", ANIMATED_GIF);
    let mut session = TerminalSession::spawn_with_session_id(
        SessionId::new(200_007),
        Some("kitten"),
        &[
            "icat",
            "--use-window-size",
            "80,12,800,240",
            "--place",
            "1x1@0x0",
            "--loop",
            "1",
            "--transfer-mode",
            "stream",
            "--stdin=no",
            path.to_str().expect("fixture path is not valid UTF-8"),
        ],
        TerminalSize::with_pixels(80, 12, 800, 240),
    )
    .expect("could not spawn kitten animation fixture");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut image_id = None;
    while Instant::now() < deadline {
        session.poll_output().expect("kitten animation PTY failed");
        image_id = session
            .graphics(TERMINAL_AREA)
            .first()
            .map(|item| item.resource().image());
        if image_id
            .is_some_and(|image| session.graphics_animation_frame_count(image).unwrap_or(0) > 0)
        {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let _ = std::fs::remove_file(&path);
    let image_id = image_id.expect("kitten emitted no retained animation image");
    assert!(
        session
            .graphics_animation_frame_count(image_id)
            .unwrap_or(0)
            > 0,
        "kitten emitted no retained animation frames: closed={}, failure={:?}, diagnostics={:?}",
        session.is_closed(),
        session.failure(),
        session.graphics_diagnostics()
    );
    session
        .shutdown()
        .expect("could not shut down kitten animation fixture");
}

#[test]
#[ignore = "requires the installed kitten executable, but not a Kitty terminal"]
fn installed_kitten_failure_path_does_not_create_a_graphics_frame() {
    let missing = std::env::temp_dir().join(format!(
        "cmdash-kitty-missing-{}-{}.gif",
        std::process::id(),
        200_008
    ));
    let mut session = TerminalSession::spawn_with_session_id(
        SessionId::new(200_008),
        Some("kitten"),
        &[
            "icat",
            "--use-window-size",
            "80,12,800,240",
            "--transfer-mode",
            "stream",
            "--stdin=no",
            missing.to_str().expect("fixture path is not valid UTF-8"),
        ],
        TerminalSize::with_pixels(80, 12, 800, 240),
    )
    .expect("could not spawn kitten failure fixture");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut rendered = String::new();
    let mut submissions = Vec::new();
    while Instant::now() < deadline {
        session.poll_output().expect("kitten failure PTY failed");
        submissions = session.graphics(TERMINAL_AREA);
        let scene = session.render(TERMINAL_AREA, false);
        rendered.clear();
        for y in TERMINAL_AREA.y..TERMINAL_AREA.y.saturating_add(TERMINAL_AREA.height) {
            for x in TERMINAL_AREA.x..TERMINAL_AREA.x.saturating_add(TERMINAL_AREA.width) {
                if let Some(cell) = scene.cell_at(x, y) {
                    rendered.push(cell.symbol);
                }
            }
        }
        if session.is_closed() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    assert!(submissions.is_empty());
    assert!(
        rendered.to_ascii_lowercase().contains("error")
            || rendered.to_ascii_lowercase().contains("not found")
            || rendered.to_ascii_lowercase().contains("no such")
    );
    session
        .shutdown()
        .expect("could not shut down kitten failure fixture");
}
