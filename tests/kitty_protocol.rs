use std::{
    thread,
    time::{Duration, Instant},
};

use cmdash::{SessionId, TerminalSession, TerminalSize};
use ratatui::layout::Rect;

const TERMINAL_AREA: Rect = Rect::new(0, 0, 80, 12);
const RECEIVE_TIMEOUT: Duration = Duration::from_secs(2);
const NEGOTIATION_MARKER: &str = "icat-negotiation-ok";
const UPLOAD_MARKER: &str = "icat-upload-ok";

/// A shell-based stand-in for kitty's `kitten icat` detection handshake.
///
/// It emits the direct, file, and shared-memory Kitty query commands followed
/// by DA1, reads the responses from the child PTY, and then emits a quiet
/// transmit-and-display command. No Kitty terminal is needed: the test exercises
/// cmdash's terminal-emulator response and retained-placement paths directly.
struct IcatNegotiationFixture {
    script: &'static str,
}

impl IcatNegotiationFixture {
    const fn new() -> Self {
        Self {
            script: r#"
                stty raw -echo
                printf '\033_Ga=q,i=1,t=d,s=1,v=1,f=24;MTIz\033\\\033_Ga=q,i=2,t=f,s=1,v=1,f=24;L3RtcA==\033\\\033_Ga=q,i=3,t=s,s=1,v=1,f=24;aWQ=\033\\\033[c'
                direct=$(dd bs=1 count=11 2>/dev/null)
                file=$(dd bs=1 count=65 2>/dev/null)
                memory=$(dd bs=1 count=65 2>/dev/null)
                da=$(dd bs=1 count=7 2>/dev/null)
                if [ "$direct" = "$(printf '\033_Gi=1;OK\033\\')" ] && \
                   [ "$file" = "$(printf '\033_Gi=2;ENOTSUP:direct transfer is the only supported Kitty mode\033\\')" ] && \
                   [ "$memory" = "$(printf '\033_Gi=3;ENOTSUP:direct transfer is the only supported Kitty mode\033\\')" ] && \
                   [ "$da" = "$(printf '\033[?1;2c')" ]; then
                    printf 'icat-negotiation-ok'
                    printf '\033_Ga=T,f=24,i=42,s=2,v=1,c=2,r=2,q=2,m=1;AQID\033\\'
                    printf '\033_Gm=0;BAUG\033\\'
                    printf 'icat-upload-ok'
                else
                    printf 'icat-negotiation-failed'
                fi
                sleep 5
            "#,
        }
    }

    fn spawn(&self) -> TerminalSession {
        TerminalSession::spawn_with_session_id(
            SessionId::new(200_001),
            Some("sh"),
            &["-c", self.script],
            TerminalSize::new(80, 12),
        )
        .expect("could not spawn the icat negotiation fixture")
    }
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
    const TINY_GIF: &[u8] = &[
        0x47, 0x49, 0x46, 0x38, 0x39, 0x61, 0x01, 0x00, 0x01, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00,
        0x00, 0xff, 0xff, 0xff, 0x21, 0xf9, 0x04, 0x01, 0x00, 0x00, 0x00, 0x00, 0x2c, 0x00, 0x00,
        0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x02, 0x01, 0x44, 0x00, 0x3b,
    ];
    let path =
        std::env::temp_dir().join(format!("cmdash-kitty-fixture-{}.gif", std::process::id()));
    std::fs::write(&path, TINY_GIF).expect("could not write the tiny GIF fixture");

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
