#[path = "support/headless_kitty.rs"]
mod headless_kitty;

use cmdash::{
    Backend, BackendCapabilities, CrosstermBackend, GraphicsCapabilityConfidence,
    GraphicsCapabilitySource, GraphicsSubmission, GraphicsSubmissionStatus, SessionGraphicsStore,
    SessionId, TerminalSession, TerminalSize,
};
use headless_kitty::HeadlessKittyTerminal;
use ratatui::layout::Rect;
use std::{
    io::{self, Write},
    thread,
    time::{Duration, Instant},
};

struct FailingWriter;

impl Write for FailingWriter {
    fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
        Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "synthetic outer-terminal write failure",
        ))
    }

    fn flush(&mut self) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "synthetic outer-terminal flush failure",
        ))
    }
}

fn capabilities(
    kitty_graphics: bool,
    kitty_unicode_placeholders: bool,
    kitty_passthrough: bool,
    kitty_text_fallback: bool,
) -> BackendCapabilities {
    BackendCapabilities {
        truecolor: true,
        mouse: true,
        bracketed_paste: true,
        kitty_graphics,
        kitty_unicode_placeholders,
        graphics_source: GraphicsCapabilitySource::ExplicitOverride,
        graphics_confidence: if kitty_graphics {
            GraphicsCapabilityConfidence::Confirmed
        } else {
            GraphicsCapabilityConfidence::Rejected
        },
        kitty_passthrough,
        kitty_text_fallback,
        sixel: false,
    }
}

fn captured_submission(image: u32, width: u16, height: u16) -> GraphicsSubmission {
    let mut store = SessionGraphicsStore::new(SessionId::new(0));
    let parameters = format!("a=T,f=24,i={image},c={width},r={height},q=2");
    store
        .apply_kitty_command_with_context(parameters.as_bytes(), b"AQID", (0, 0), (0, 0))
        .expect("capture fixture image should be accepted");
    store
        .visible_submissions(Rect::new(0, 0, 16, 8))
        .into_iter()
        .next()
        .expect("capture fixture should create one placement")
}

fn captured_submission_with_placement(
    image: u32,
    width: u16,
    height: u16,
    placement_id: u32,
) -> GraphicsSubmission {
    let mut store = SessionGraphicsStore::new(SessionId::new(0));
    let parameters = format!("a=T,f=24,i={image},c={width},r={height},p={placement_id},q=2");
    store
        .apply_kitty_command_with_context(parameters.as_bytes(), b"AQID", (0, 0), (0, 0))
        .expect("placement-id fixture image should be accepted");
    store
        .visible_submissions(Rect::new(0, 0, 16, 8))
        .into_iter()
        .next()
        .expect("placement-id fixture should create one placement")
}

fn encode_test_base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = u32::from(chunk[0]);
        let second = u32::from(chunk.get(1).copied().unwrap_or(0));
        let third = u32::from(chunk.get(2).copied().unwrap_or(0));
        let combined = (first << 16) | (second << 8) | third;
        output.push(TABLE[((combined >> 18) & 63) as usize] as char);
        output.push(TABLE[((combined >> 12) & 63) as usize] as char);
        output.push(if chunk.len() > 1 {
            TABLE[((combined >> 6) & 63) as usize] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            TABLE[(combined & 63) as usize] as char
        } else {
            '='
        });
    }
    output
}

fn assert_rendered(status: GraphicsSubmissionStatus, resources: usize, placements: usize) {
    assert_eq!(
        status,
        GraphicsSubmissionStatus::Rendered {
            resources,
            placements,
        }
    );
}

fn replay_in_chunks(
    bytes: &[u8],
    seed: u64,
    viewport: Option<(u16, u16)>,
) -> Result<HeadlessKittyTerminal, String> {
    let mut terminal = HeadlessKittyTerminal::with_viewport(viewport);
    let mut state = seed;
    let mut offset = 0;
    while offset < bytes.len() {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let chunk_length = 1 + (state as usize % 23);
        let end = offset.saturating_add(chunk_length).min(bytes.len());
        terminal.feed(&bytes[offset..end])?;
        offset = end;
    }
    terminal.finish()?;
    Ok(terminal)
}

fn assert_random_chunk_boundaries_preserve_semantics(
    bytes: &[u8],
    viewport: Option<(u16, u16)>,
    image_ids: &[u32],
) {
    let expected = HeadlessKittyTerminal::replay_with_viewport(bytes, viewport).unwrap();
    for seed in [1, 7, 0xC0FFEE, u64::MAX - 1, 0x5EED_1234] {
        let actual = replay_in_chunks(bytes, seed, viewport)
            .unwrap_or_else(|error| panic!("chunked replay failed for seed {seed}: {error}"));
        assert_eq!(actual.actions(), expected.actions(), "seed {seed}");
        assert_eq!(actual.text(), expected.text(), "seed {seed}");
        assert_eq!(actual.placements(), expected.placements(), "seed {seed}");
        assert_eq!(
            actual.placeholder_cells(),
            expected.placeholder_cells(),
            "seed {seed}"
        );
        assert_eq!(
            actual.resource_count(),
            expected.resource_count(),
            "seed {seed}"
        );
        for image_id in image_ids {
            assert_eq!(
                actual.resource_payload(*image_id),
                expected.resource_payload(*image_id),
                "seed {seed}, image {image_id}"
            );
        }
    }
}

#[test]
fn headless_model_reassembles_chunked_kitty_uploads() {
    let stream = b"\x1b[1;1H\x1b_Ga=T,f=24,i=21,c=1,r=1,m=1;AQ\x1b\\\x1b_Gm=0;ID\x1b\\";
    let model = HeadlessKittyTerminal::replay(stream).unwrap();

    assert_eq!(model.actions(), &["transmit"]);
    assert_eq!(model.resource_count(), 1);
    assert_eq!(model.placement_count(), 1);
    assert_eq!(model.resource_payload(21), Some(&b"AQID"[..]));
}

#[test]
fn randomized_chunk_boundaries_preserve_headless_kitty_semantics() {
    let chunked = b"\x1b[1;1H\x1b_Ga=T,f=24,i=21,c=1,r=1,m=1;AQ\x1b\\\x1b_Gm=0;ID\x1b\\text";
    assert_random_chunk_boundaries_preserve_semantics(chunked, None, &[21]);

    let command = b"\x1b_Ga=T,f=24,i=22,c=1,r=1,q=2,m=0;BAUG\x1b\\";
    let mut passthrough = b"\x1b[2;2H\x1bPtmux;".to_vec();
    for byte in command {
        if *byte == 0x1b {
            passthrough.push(0x1b);
        }
        passthrough.push(*byte);
    }
    passthrough.extend_from_slice(b"\x1b\\");
    assert_random_chunk_boundaries_preserve_semantics(&passthrough, None, &[22]);

    let placeholder = "\u{10eeee}\u{305}\u{305}\u{305}";
    let mut placeholder_stream =
        b"\x1b_Ga=T,f=24,i=41,c=1,r=1,U=1,z=-2;AQID\x1b\\\x1b[38;2;0;0;41m\x1b[1;1H".to_vec();
    placeholder_stream.extend_from_slice(placeholder.as_bytes());
    assert_random_chunk_boundaries_preserve_semantics(&placeholder_stream, Some((1, 1)), &[41]);
}

#[test]
fn headless_model_validates_z_order_and_placement_id_replacement() {
    let stream = b"\x1b[1;1H\x1b_Ga=T,f=24,i=31,c=2,r=1,q=2;AQID\x1b\\\x1b[2;3H\x1b_Ga=p,i=31,p=7,c=1,r=1,z=5,q=2;\x1b\\\x1b[3;4H\x1b_Ga=p,i=31,p=8,c=1,r=1,z=10,q=2;\x1b\\\x1b[4;5H\x1b_Ga=p,i=31,p=7,c=3,r=2,z=-2,q=2;\x1b\\";
    let model = HeadlessKittyTerminal::replay(stream).unwrap();

    assert_eq!(model.placements().len(), 3);
    let replacement = model
        .placements()
        .iter()
        .find(|placement| placement.placement_id == Some(7))
        .expect("placement id 7 should be present");
    assert_eq!(replacement.x, 4);
    assert_eq!(replacement.y, 3);
    assert_eq!(replacement.width, 3);
    assert_eq!(replacement.height, 2);
    assert_eq!(replacement.z, -2);

    let z_order = model.placements_in_z_order();
    assert_eq!(
        z_order
            .iter()
            .map(|placement| placement.z)
            .collect::<Vec<_>>(),
        vec![-2, 0, 10]
    );
    assert_eq!(model.actions(), &["transmit", "place", "place", "place"]);
}

#[test]
fn headless_model_validates_placeholder_clipping_and_z_index_occlusion() {
    let placeholder = "\u{10eeee}\u{305}\u{305}\u{305}";
    let mut stream = b"\x1b_Ga=T,f=24,i=41,c=2,r=1,U=1,z=-3;AQID\x1b\\".to_vec();
    stream.extend_from_slice(b"\x1b_Ga=T,f=24,i=42,c=2,r=1,U=1,z=7;BAUG\x1b\\");
    stream.extend_from_slice(b"\x1b[38;2;0;0;41m\x1b[1;1H");
    stream.extend_from_slice(placeholder.as_bytes());
    stream.extend_from_slice(b"\x1b[38;2;0;0;42m\x1b[1;1H");
    stream.extend_from_slice(placeholder.as_bytes());
    stream.extend_from_slice(b"\x1b[38;2;0;0;41m\x1b[1;3H");
    stream.extend_from_slice(placeholder.as_bytes());

    let model = HeadlessKittyTerminal::replay_with_viewport(&stream, Some((2, 1))).unwrap();

    assert_eq!(model.placeholder_count(), 3);
    assert_eq!(
        model
            .placeholder_cells()
            .iter()
            .map(|cell| (cell.image_id, cell.x, cell.y))
            .collect::<Vec<_>>(),
        vec![(41, 0, 0), (42, 0, 0), (41, 2, 0)]
    );
    let visible = model.visible_placeholder_cells();
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].image_id, 42);
    assert_eq!(visible[0].z, 7);
    assert_eq!((visible[0].x, visible[0].y), (0, 0));
}

#[test]
fn headless_framebuffer_renders_rgb_pixels_and_applies_z_order() {
    let stream = b"\x1b[1;1H\x1b_Ga=T,f=24,i=51,s=2,v=2,c=2,r=2,z=-2;/wAAAP8AAAD/////\x1b\\";
    let terminal = HeadlessKittyTerminal::replay_with_framebuffer(stream, 4, 3).unwrap();

    assert_eq!(terminal.framebuffer_size(), Some((4, 3)));
    assert_eq!(
        terminal.pixel(0, 0),
        Some(headless_kitty::HeadlessPixel::rgb(255, 0, 0))
    );
    assert_eq!(
        terminal.pixel(1, 0),
        Some(headless_kitty::HeadlessPixel::rgb(0, 255, 0))
    );
    assert_eq!(
        terminal.pixel(0, 1),
        Some(headless_kitty::HeadlessPixel::rgb(0, 0, 255))
    );
    assert_eq!(
        terminal.pixel(1, 1),
        Some(headless_kitty::HeadlessPixel::rgb(255, 255, 255))
    );
    assert_eq!(terminal.visible_pixel_count(), 4);

    let layered = b"\x1b[1;2H\x1b_Ga=T,f=24,i=61,s=1,v=1,c=1,r=1,z=-3;/wAA\x1b\\\x1b[1;2H\x1b_Ga=T,f=24,i=62,s=1,v=1,c=1,r=1,z=4;AP8A\x1b\\";
    let terminal = HeadlessKittyTerminal::replay_with_framebuffer(layered, 4, 2).unwrap();
    assert_eq!(
        terminal.pixel(1, 0),
        Some(headless_kitty::HeadlessPixel::rgb(0, 255, 0))
    );
}

#[test]
fn headless_framebuffer_applies_source_crops_and_delete_updates_pixels() {
    let upload = b"\x1b_Ga=t,f=24,i=63,s=2,v=1; /wAA AAD/\x1b\\"
        .iter()
        .copied()
        .filter(|byte| *byte != b' ')
        .collect::<Vec<_>>();
    let mut stream = upload;
    stream.extend_from_slice(b"\x1b[1;1H\x1b_Ga=p,i=63,c=2,r=1,x=1,y=0,w=1,h=1;\x1b\\");
    let terminal = HeadlessKittyTerminal::replay_with_framebuffer(&stream, 3, 1).unwrap();
    assert_eq!(
        terminal.pixel(0, 0),
        Some(headless_kitty::HeadlessPixel::rgb(0, 0, 255))
    );
    assert_eq!(
        terminal.pixel(1, 0),
        Some(headless_kitty::HeadlessPixel::rgb(0, 0, 255))
    );

    let mut deleted = stream;
    deleted.extend_from_slice(b"\x1b_Ga=d,d=i,i=63;\x1b\\");
    let terminal = HeadlessKittyTerminal::replay_with_framebuffer(&deleted, 3, 1).unwrap();
    assert_eq!(terminal.visible_pixel_count(), 0);
    assert_eq!(terminal.resource_count(), 0);
}

#[test]
fn headless_framebuffer_renders_unicode_placeholder_cells() {
    let mut stream =
        b"\x1b_Ga=T,f=24,i=71,s=2,v=1,c=2,r=1,U=1; /wAAAAD/\x1b\\\x1b[38;2;0;0;71m\x1b[1;1H"
            .iter()
            .copied()
            .filter(|byte| *byte != b' ')
            .collect::<Vec<_>>();
    stream.extend_from_slice(
        "\u{10eeee}\u{305}\u{305}\u{305}\u{10eeee}\u{305}\u{30d}\u{305}".as_bytes(),
    );
    let terminal = HeadlessKittyTerminal::replay_with_framebuffer(&stream, 3, 1).unwrap();

    assert_eq!(
        terminal.pixel(0, 0),
        Some(headless_kitty::HeadlessPixel::rgb(255, 0, 0))
    );
    assert_eq!(
        terminal.pixel(1, 0),
        Some(headless_kitty::HeadlessPixel::rgb(0, 0, 255))
    );
    assert_eq!(terminal.visible_pixel_count(), 2);
}

#[test]
fn headless_model_rejects_malformed_and_unbounded_streams() {
    let unterminated_apc = b"\x1b_Ga=T,f=24,i=1;AQID";
    assert!(
        HeadlessKittyTerminal::replay(unterminated_apc)
            .unwrap_err()
            .contains("unterminated Kitty APC")
    );

    let unterminated_tmux = b"\x1bPtmux;\x1b\x1b_Ga=T,f=24,i=1;AQID\x1b\x1b\\";
    assert!(
        HeadlessKittyTerminal::replay(unterminated_tmux)
            .unwrap_err()
            .contains("unterminated tmux passthrough")
    );

    let invalid_action = b"\x1b_Ga=x,i=1;AQID\x1b\\";
    assert!(
        HeadlessKittyTerminal::replay(invalid_action)
            .unwrap_err()
            .contains("unsupported Kitty action")
    );

    let invalid_parameter = b"\x1b_Ga=T,f=24,i=not-a-number;AQID\x1b\\";
    assert!(
        HeadlessKittyTerminal::replay(invalid_parameter)
            .unwrap_err()
            .contains("invalid Kitty APC i")
    );

    let unknown_placement = b"\x1b_Ga=p,i=7;\x1b\\";
    assert!(
        HeadlessKittyTerminal::replay(unknown_placement)
            .unwrap_err()
            .contains("unknown image 7")
    );

    let mut unknown_placeholder =
        b"\x1b_Ga=T,f=24,i=5,c=1,r=1,U=1;AQID\x1b\\\x1b[38;2;0;0;5m\x1b[1;1H".to_vec();
    unknown_placeholder.extend_from_slice("\u{10eeee}\u{9999}\u{305}\u{305}".as_bytes());
    assert!(
        HeadlessKittyTerminal::replay(&unknown_placeholder)
            .unwrap_err()
            .contains("unknown Kitty placeholder combining mark")
    );

    let oversized_stream = vec![b'x'; 1024 * 1024 + 1];
    assert!(
        HeadlessKittyTerminal::replay(&oversized_stream)
            .unwrap_err()
            .contains("bounded input limit")
    );

    let mut oversized_payload = b"\x1b_Ga=T,f=24,i=6;".to_vec();
    oversized_payload.extend(std::iter::repeat_n(b'A', 512 * 1024 + 1));
    oversized_payload.extend_from_slice(b"\x1b\\");
    assert!(
        HeadlessKittyTerminal::replay(&oversized_payload)
            .unwrap_err()
            .contains("bounded input limit")
    );
}

#[test]
fn pty_session_upload_reaches_a_rendered_headless_framebuffer() {
    let script = r"printf '\033_Ga=T,f=24,i=91,s=2,v=2,c=2,r=2,q=2;/wAAAP8AAAD/////\033\\'";
    let mut session = TerminalSession::spawn_with_session_id(
        SessionId::new(91),
        Some("sh"),
        &["-c", script],
        TerminalSize::new(20, 5),
    )
    .expect("could not spawn the framebuffer PTY fixture");
    let area = Rect::new(0, 0, 20, 5);
    let deadline = Instant::now() + Duration::from_secs(1);
    let submissions = loop {
        session.poll_output().expect("framebuffer PTY failed");
        let submissions = session.graphics(area);
        if !submissions.is_empty() || Instant::now() >= deadline {
            break submissions;
        }
        thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(submissions.len(), 1);

    let mut backend = CrosstermBackend::new(Vec::new())
        .with_capabilities(capabilities(true, false, false, false));
    backend
        .submit_graphics(&submissions, &submissions, &[])
        .expect("outer adapter should serialize the PTY image");
    let terminal = HeadlessKittyTerminal::replay_with_framebuffer(backend.writer(), 4, 3)
        .expect("headless outer terminal should accept the PTY stream");
    assert_eq!(
        terminal.pixel(0, 0),
        Some(headless_kitty::HeadlessPixel::rgb(255, 0, 0))
    );
    assert_eq!(
        terminal.pixel(1, 0),
        Some(headless_kitty::HeadlessPixel::rgb(0, 255, 0))
    );
    assert_eq!(
        terminal.pixel(0, 1),
        Some(headless_kitty::HeadlessPixel::rgb(0, 0, 255))
    );
    assert_eq!(
        terminal.pixel(1, 1),
        Some(headless_kitty::HeadlessPixel::rgb(255, 255, 255))
    );
    session
        .shutdown()
        .expect("could not shut down framebuffer PTY");
}

#[test]
fn direct_adapter_matches_captured_upload_stream() {
    let submission = captured_submission(7, 2, 1);
    let mut backend = CrosstermBackend::new(Vec::new())
        .with_capabilities(capabilities(true, false, false, false));

    let status = backend
        .submit_graphics(
            std::slice::from_ref(&submission),
            std::slice::from_ref(&submission),
            &[],
        )
        .expect("direct capture should write");
    assert_rendered(status, 1, 1);

    assert_eq!(
        backend.writer(),
        b"\x1b[1;1H\x1b_Ga=T,f=24,i=7,c=2,r=1,C=1,q=2,m=0;AQID\x1b\\"
    );
    let model = HeadlessKittyTerminal::replay(backend.writer()).unwrap();
    assert_eq!(model.actions(), &["transmit"]);
    assert_eq!(model.resource_count(), 1);
    assert_eq!(model.placement_count(), 1);
    assert_eq!(model.resource_payload(7), Some(&b"AQID"[..]));
}

#[test]
fn direct_adapter_reuses_resources_and_captures_placement_only_replay() {
    let submission = captured_submission(8, 1, 1);
    let mut backend = CrosstermBackend::new(Vec::new())
        .with_capabilities(capabilities(true, false, false, false));
    backend
        .submit_graphics(
            std::slice::from_ref(&submission),
            std::slice::from_ref(&submission),
            &[],
        )
        .unwrap();
    let first_len = backend.writer().len();

    let status = backend
        .submit_graphics(
            std::slice::from_ref(&submission),
            std::slice::from_ref(&submission),
            &[],
        )
        .expect("resource reuse capture should write");
    assert_rendered(status, 0, 1);

    assert_eq!(
        &backend.writer()[first_len..],
        b"\x1b[1;1H\x1b_Ga=p,i=8,c=1,r=1,C=1,q=2;\x1b\\"
    );
    assert_eq!(backend.metrics().graphics_uploads, 1);
    assert_eq!(backend.metrics().graphics_reuses, 1);
    let model = HeadlessKittyTerminal::replay(backend.writer()).unwrap();
    assert_eq!(model.actions(), &["transmit", "place"]);
    assert_eq!(model.resource_count(), 1);
    assert_eq!(model.placement_count(), 2);
}

#[test]
fn direct_adapter_matches_captured_delete_stream() {
    let submission = captured_submission(9, 1, 1);
    let mut backend = CrosstermBackend::new(Vec::new())
        .with_capabilities(capabilities(true, false, false, false));
    backend
        .submit_graphics(
            std::slice::from_ref(&submission),
            std::slice::from_ref(&submission),
            &[],
        )
        .unwrap();
    let upload_ack = backend.feed_outer_input(b"\x1b_Gi=9;OK\x1b\\");
    assert_eq!(upload_ack.graphics_acknowledgements.len(), 1);
    let first_len = backend.writer().len();

    let status = backend
        .submit_graphics(&[], &[], std::slice::from_ref(&submission))
        .expect("delete capture should write");
    assert_rendered(status, 0, 0);

    assert_eq!(&backend.writer()[first_len..], b"\x1b_Ga=d,d=i,i=9;\x1b\\");
    let model = HeadlessKittyTerminal::replay(backend.writer()).unwrap();
    assert_eq!(model.actions(), &["transmit", "delete"]);
    assert_eq!(model.resource_count(), 0);
    assert_eq!(model.placement_count(), 0);
}

#[test]
fn placeholder_adapter_matches_captured_upload_and_cell_stream() {
    let submission = captured_submission(7, 2, 1);
    let mut backend =
        CrosstermBackend::new(Vec::new()).with_capabilities(capabilities(true, true, false, false));

    let status = backend
        .submit_graphics(
            std::slice::from_ref(&submission),
            std::slice::from_ref(&submission),
            &[],
        )
        .expect("placeholder capture should write");
    assert_rendered(status, 1, 1);

    let expected = format!(
        "{}\x1b[38;2;0;0;7m\x1b[1;1H{}{}{}{}{}{}{}{}\x1b[39m",
        "\x1b[1;1H\x1b_Ga=T,f=24,i=7,c=2,r=1,U=1,C=1,q=2,m=0;AQID\x1b\\",
        '\u{10eeee}',
        '\u{305}',
        '\u{305}',
        '\u{305}',
        '\u{10eeee}',
        '\u{305}',
        '\u{30d}',
        '\u{305}',
    );
    assert_eq!(backend.writer(), expected.as_bytes());
    let model = HeadlessKittyTerminal::replay(backend.writer()).unwrap();
    assert_eq!(model.actions(), &["transmit"]);
    assert_eq!(model.resource_count(), 1);
    assert_eq!(model.placement_count(), 0);
    assert_eq!(model.placeholder_count(), 2);
}

#[test]
fn passthrough_adapter_matches_captured_escaped_stream() {
    let submission = captured_submission(10, 1, 1);
    let mut backend =
        CrosstermBackend::new(Vec::new()).with_capabilities(capabilities(true, false, true, false));

    backend
        .submit_graphics(
            std::slice::from_ref(&submission),
            std::slice::from_ref(&submission),
            &[],
        )
        .expect("passthrough capture should write");

    let command = b"\x1b_Ga=T,f=24,i=10,c=1,r=1,C=1,q=2,m=0;AQID\x1b\\";
    let mut expected = b"\x1b[1;1H\x1bPtmux;".to_vec();
    for byte in command {
        if *byte == 0x1b {
            expected.push(0x1b);
        }
        expected.push(*byte);
    }
    expected.extend_from_slice(b"\x1b\\");

    assert_eq!(backend.writer(), expected.as_slice());
    let model = HeadlessKittyTerminal::replay(backend.writer()).unwrap();
    assert_eq!(model.actions(), &["transmit"]);
    assert_eq!(model.resource_count(), 1);
    assert_eq!(model.placement_count(), 1);
}

#[test]
fn headless_terminal_accepts_delete_and_returns_a_kitty_acknowledgement() {
    let submission = captured_submission(14, 1, 1);
    let mut backend = CrosstermBackend::new(Vec::new())
        .with_capabilities(capabilities(true, false, false, false));
    backend
        .submit_graphics(
            std::slice::from_ref(&submission),
            std::slice::from_ref(&submission),
            &[],
        )
        .unwrap();

    let mut terminal = HeadlessKittyTerminal::with_viewport(None);
    terminal.feed(backend.writer()).unwrap();
    terminal.finish().unwrap();
    assert_eq!(terminal.resource_count(), 1);
    assert!(terminal.acknowledgements().is_empty());

    let upload_ack = backend.feed_outer_input(b"\x1b_Gi=14;OK\x1b\\");
    assert_eq!(upload_ack.graphics_acknowledgements.len(), 1);
    let delete_start = backend.writer().len();
    backend
        .submit_graphics(&[], &[], std::slice::from_ref(&submission))
        .expect("acknowledged resource removal should emit delete");
    let delete_stream = backend.writer()[delete_start..].to_vec();
    assert_eq!(delete_stream, b"\x1b_Ga=d,d=i,i=14;\x1b\\");

    terminal.feed(&delete_stream).unwrap();
    terminal.finish().unwrap();
    assert_eq!(terminal.resource_count(), 0);
    assert_eq!(terminal.placement_count(), 0);
    assert_eq!(terminal.actions(), &["transmit", "delete"]);
    assert_eq!(
        terminal.acknowledgements(),
        &[b"\x1b_Gi=14;OK\x1b\\".to_vec()]
    );

    let delete_ack = backend.feed_outer_input(&terminal.acknowledgements()[0]);
    assert_eq!(delete_ack.graphics_acknowledgements.len(), 1);
    assert!(delete_ack.graphics_acknowledgements[0].success);
    assert_eq!(backend.metrics().graphics_gc, 1);
}

#[test]
fn resource_gc_waits_for_upload_ack_then_delete_ack() {
    let submission = captured_submission(12, 1, 1);
    let mut backend = CrosstermBackend::new(Vec::new())
        .with_capabilities(capabilities(true, false, false, false));
    backend
        .submit_graphics(
            std::slice::from_ref(&submission),
            std::slice::from_ref(&submission),
            &[],
        )
        .unwrap();
    let upload_len = backend.writer().len();

    backend
        .submit_graphics(&[], &[], std::slice::from_ref(&submission))
        .expect("unacknowledged resource removal should be deferred");
    assert_eq!(backend.writer().len(), upload_len);

    let upload_ack = backend.feed_outer_input(b"\x1b_Gi=12;OK\x1b\\");
    assert_eq!(upload_ack.terminal_bytes, b"");
    assert_eq!(upload_ack.graphics_acknowledgements.len(), 1);
    assert!(upload_ack.graphics_acknowledgements[0].success);
    assert_eq!(
        &backend.writer()[upload_len..],
        b"\x1b_Ga=d,d=i,i=12;\x1b\\"
    );
    assert_eq!(backend.metrics().graphics_gc, 0);

    let delete_ack = backend.feed_outer_input(b"\x1b_Gi=12;OK\x1b\\");
    assert_eq!(delete_ack.graphics_acknowledgements.len(), 1);
    assert!(delete_ack.graphics_acknowledgements[0].success);
    assert_eq!(backend.metrics().graphics_acknowledgements, 2);
    assert_eq!(backend.metrics().graphics_gc, 1);
    let model = HeadlessKittyTerminal::replay(backend.writer()).unwrap();
    assert_eq!(model.actions(), &["transmit", "delete"]);
    assert_eq!(model.resource_count(), 0);
}

#[test]
fn missing_outer_acknowledgements_are_retried_with_a_bounded_budget() {
    let submission = captured_submission(15, 1, 1);
    let mut backend = CrosstermBackend::new(Vec::new())
        .with_capabilities(capabilities(true, false, false, false));
    backend
        .submit_graphics(
            std::slice::from_ref(&submission),
            std::slice::from_ref(&submission),
            &[],
        )
        .unwrap();
    let original_len = backend.writer().len();
    let now = Instant::now();
    assert_eq!(
        backend
            .poll_graphics_retries(now + Duration::from_secs(1))
            .unwrap(),
        1
    );
    assert_eq!(
        backend
            .poll_graphics_retries(now + Duration::from_secs(2))
            .unwrap(),
        1
    );
    assert_eq!(
        backend
            .poll_graphics_retries(now + Duration::from_secs(3))
            .unwrap(),
        0
    );
    assert_eq!(backend.metrics().graphics_ack_failures, 1);
    assert!(backend.writer().len() > original_len);
}

#[test]
fn cancelling_outer_graphics_drops_unacknowledged_work_but_cleans_accepted_work() {
    let submission = captured_submission(16, 1, 1);
    let mut backend = CrosstermBackend::new(Vec::new())
        .with_capabilities(capabilities(true, false, false, false));
    backend
        .submit_graphics(
            std::slice::from_ref(&submission),
            std::slice::from_ref(&submission),
            &[],
        )
        .unwrap();
    let before_cancel = backend.writer().len();
    backend.cancel_graphics_transfers().unwrap();
    assert_eq!(backend.writer().len(), before_cancel);

    backend
        .submit_graphics(
            std::slice::from_ref(&submission),
            std::slice::from_ref(&submission),
            &[],
        )
        .unwrap();
    backend.feed_outer_input(b"\x1b_Gi=16;OK\x1b\\");
    let before_cleanup = backend.writer().len();
    backend.cancel_graphics_transfers().unwrap();
    assert!(backend.writer().len() > before_cleanup);
    assert!(backend.writer()[before_cleanup..].ends_with(b"\x1b_Ga=d,d=i,i=16;\x1b\\"));
}

#[test]
fn outer_graphics_failures_are_reported_without_collecting_resources() {
    let submission = captured_submission(13, 1, 1);
    let mut backend = CrosstermBackend::new(Vec::new())
        .with_capabilities(capabilities(true, false, false, false));
    backend
        .submit_graphics(
            std::slice::from_ref(&submission),
            std::slice::from_ref(&submission),
            &[],
        )
        .unwrap();

    let batch = backend.feed_outer_input(b"\x1b_Gi=13;ENOENT:missing\x1b\\");
    assert_eq!(batch.graphics_acknowledgements.len(), 1);
    assert!(!batch.graphics_acknowledgements[0].success);
    assert_eq!(batch.graphics_acknowledgements[0].message, "ENOENT:missing");
    assert_eq!(backend.metrics().graphics_ack_failures, 1);
    assert_eq!(backend.metrics().graphics_gc, 0);
}

#[test]
fn graphics_failure_paths_are_explicit_and_diagnostic() {
    let submission = captured_submission(101, 1, 1);
    let mut suppressed = CrosstermBackend::new(Vec::new())
        .with_capabilities(capabilities(false, false, false, false));
    let status = suppressed
        .submit_graphics(
            std::slice::from_ref(&submission),
            std::slice::from_ref(&submission),
            &[],
        )
        .expect("unsupported graphics should be reported, not fail the frame");
    assert!(matches!(
        status,
        GraphicsSubmissionStatus::Suppressed {
            placements: 1,
            ref reason
        } if reason.contains("unavailable")
    ));
    assert!(suppressed.writer().is_empty());

    let limits = cmdash::GraphicsLimits {
        max_decoded_bytes: 2,
        max_resources: 1,
        max_placements: 1,
    };
    let mut quota_store = SessionGraphicsStore::with_limits(SessionId::new(102), limits);
    quota_store
        .apply_kitty_command(b"a=T,f=24,i=102", b"AQID")
        .expect("quota rejection is a handled diagnostic outcome");
    assert_eq!(quota_store.resource_count(), 0);
    assert!(
        quota_store
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.message().contains("byte limit"))
    );

    let mut probe = cmdash::GraphicsCapabilityProbe::new(Duration::from_millis(1), 128);
    let started = Instant::now();
    probe.begin(started).expect("probe should start");
    let report = probe
        .poll_timeout(started + Duration::from_millis(2))
        .expect("probe timeout should produce a report");
    assert!(!report.kitty_graphics);
    assert!(
        report
            .diagnostic
            .as_deref()
            .is_some_and(|message| message.contains("timeout"))
    );

    let mut write_failure = CrosstermBackend::new(FailingWriter)
        .with_capabilities(capabilities(true, false, false, false));
    let error = write_failure
        .submit_graphics(
            std::slice::from_ref(&submission),
            std::slice::from_ref(&submission),
            &[],
        )
        .expect_err("outer write failure must not become a successful frame");
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
}

#[test]
fn malformed_pty_graphics_become_session_diagnostics() {
    let script = r"printf '\033_Ga=T,f=24,i=103;!!!!\033\\'; sleep 2";
    let mut session = TerminalSession::spawn_with_session_id(
        SessionId::new(103),
        Some("sh"),
        &["-c", script],
        TerminalSize::new(20, 4),
    )
    .expect("could not spawn malformed graphics fixture");
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline && session.graphics_diagnostics().is_empty() {
        session.poll_output().expect("malformed fixture PTY failed");
        thread::sleep(Duration::from_millis(5));
    }
    assert!(
        session
            .graphics_diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.message().contains("base64"))
    );
    assert!(session.graphics(Rect::new(0, 0, 20, 4)).is_empty());
    session
        .shutdown()
        .expect("could not shut down malformed graphics fixture");
}

#[test]
fn graphics_lifecycle_preserves_anchors_across_resize_and_clears_on_shutdown() {
    let script = r"printf '\033[2;2H\033_Ga=T,f=24,i=104,c=2,r=1,q=2;AQID\033\\'; sleep 2";
    let mut session = TerminalSession::spawn_with_session_id(
        SessionId::new(104),
        Some("sh"),
        &["-c", script],
        TerminalSize::new(20, 6),
    )
    .expect("could not spawn graphics lifecycle fixture");
    let deadline = Instant::now() + Duration::from_secs(1);
    let submissions = loop {
        session.poll_output().expect("lifecycle fixture PTY failed");
        let submissions = session.graphics(Rect::new(0, 0, 20, 6));
        if !submissions.is_empty() || Instant::now() >= deadline {
            break submissions;
        }
        thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(submissions.len(), 1);
    assert_eq!(submissions[0].placement().area(), Rect::new(1, 1, 2, 1));

    session
        .resize(TerminalSize::new(30, 8))
        .expect("graphics lifecycle resize should succeed");
    let resized = session.graphics(Rect::new(5, 3, 20, 5));
    assert_eq!(resized.len(), 1);
    assert_eq!(resized[0].placement().area(), Rect::new(6, 4, 2, 1));

    session
        .shutdown()
        .expect("could not shut down graphics lifecycle fixture");
    assert!(session.is_closed());
    assert!(session.graphics(Rect::new(0, 0, 30, 8)).is_empty());
}

#[test]
fn compositor_clips_graphics_for_overlays_and_hidden_panes() {
    let mut store = SessionGraphicsStore::new(SessionId::new(105));
    store
        .apply_kitty_command_with_context(b"a=T,f=24,i=105,c=6,r=2,q=2", b"AQID", (0, 0), (0, 0))
        .unwrap();
    let submission = store
        .visible_submissions(Rect::new(0, 0, 8, 4))
        .into_iter()
        .next()
        .expect("fixture image should have a placement");

    let mut source = cmdash::Scene::new(Rect::new(0, 0, 8, 4));
    source.add_image_layer(submission);
    let mut composed = cmdash::Scene::new(Rect::new(0, 0, 8, 4));
    composed.blit(&source, Rect::new(0, 0, 8, 4));

    let overlay = cmdash::Scene::new(Rect::new(2, 0, 2, 2));
    composed.blit(&overlay, overlay.area());
    assert!(composed.image_layers().iter().all(|layer| {
        let area = layer.placement().area();
        area.x + area.width <= 2 || area.x >= 4 || area.y >= 2
    }));

    let mut hidden_surface = cmdash::Scene::new(Rect::new(0, 0, 8, 4));
    hidden_surface.blit(&source, Rect::new(0, 0, 0, 0));
    assert!(hidden_surface.image_layers().is_empty());
}

#[test]
fn rapid_pane_switching_reuses_resources_without_retaining_duplicate_placements() {
    let left = captured_submission_with_placement(301, 1, 1, 1);
    let right = captured_submission_with_placement(302, 1, 1, 1);
    let mut backend = CrosstermBackend::new(Vec::new())
        .with_capabilities(capabilities(true, false, false, false));

    for cycle in 0..128 {
        let submission = if cycle % 2 == 0 { &left } else { &right };
        backend
            .submit_graphics(
                std::slice::from_ref(submission),
                std::slice::from_ref(submission),
                &[],
            )
            .expect("pane switch replay should remain bounded");
    }

    let metrics = backend.metrics();
    assert_eq!(metrics.graphics_uploads, 2);
    assert_eq!(metrics.graphics_reuses, 126);
    let model = HeadlessKittyTerminal::replay(backend.writer()).unwrap();
    assert_eq!(model.resource_count(), 2);
    assert_eq!(model.placement_count(), 2);
    assert_eq!(model.actions().len(), 128);
    assert_eq!(
        model
            .actions()
            .iter()
            .filter(|action| **action == "transmit")
            .count(),
        2
    );
    assert_eq!(
        model
            .actions()
            .iter()
            .filter(|action| **action == "place")
            .count(),
        126
    );
}

#[test]
fn placeholder_redraws_do_not_reupload_an_unchanged_resource() {
    let submission = captured_submission(303, 1, 1);
    let mut backend =
        CrosstermBackend::new(Vec::new()).with_capabilities(capabilities(true, true, false, false));

    for frame in 0..96 {
        let changed = if frame == 0 {
            std::slice::from_ref(&submission)
        } else {
            &[]
        };
        backend
            .submit_graphics_frame(changed, std::slice::from_ref(&submission), &[], &[], &[])
            .expect("placeholder redraw should remain renderable");
    }

    let metrics = backend.metrics();
    assert_eq!(metrics.graphics_uploads, 1);
    assert_eq!(
        metrics.graphics_bytes,
        submission.encoded_payload().len() as u64
    );
    let model = HeadlessKittyTerminal::replay_with_framebuffer(backend.writer(), 1, 1).unwrap();
    assert_eq!(model.resource_count(), 1);
    assert_eq!(
        model
            .actions()
            .iter()
            .filter(|action| **action == "transmit")
            .count(),
        1
    );
    assert_eq!(model.visible_pixel_count(), 1);
    assert_eq!(
        model.pixel(0, 0),
        Some(headless_kitty::HeadlessPixel::rgb(1, 2, 3))
    );
    assert!(model.placeholder_count() >= 96);
}

#[test]
fn repeated_acknowledged_cleanup_does_not_leak_outer_resources() {
    let mut backend = CrosstermBackend::new(Vec::new())
        .with_capabilities(capabilities(true, false, false, false));

    for image in 320..352 {
        let submission = captured_submission(image, 1, 1);
        backend
            .submit_graphics(
                std::slice::from_ref(&submission),
                std::slice::from_ref(&submission),
                &[],
            )
            .expect("resource pressure upload should write");
        let upload_ack = format!("\x1b_Gi={image};OK\x1b\\");
        let batch = backend.feed_outer_input(upload_ack.as_bytes());
        assert_eq!(batch.graphics_acknowledgements.len(), 1);

        backend
            .submit_graphics(&[], &[], std::slice::from_ref(&submission))
            .expect("resource pressure delete should write");
        let delete_ack = format!("\x1b_Gi={image};OK\x1b\\");
        let batch = backend.feed_outer_input(delete_ack.as_bytes());
        assert_eq!(batch.graphics_acknowledgements.len(), 1);
    }

    assert_eq!(backend.metrics().graphics_gc, 32);
    assert_eq!(backend.metrics().graphics_ack_failures, 0);
    let model = HeadlessKittyTerminal::replay(backend.writer()).unwrap();
    assert_eq!(model.resource_count(), 0);
    assert_eq!(model.placement_count(), 0);
}

#[test]
fn large_chunked_upload_renders_pixels_without_exceeding_headless_bounds() {
    let width = 96_u16;
    let height = 64_u16;
    let mut pixels = Vec::with_capacity(usize::from(width) * usize::from(height) * 3);
    for y in 0..height {
        for x in 0..width {
            pixels.extend_from_slice(&[x as u8, y as u8, (x as u8).wrapping_add(y as u8)]);
        }
    }
    let encoded = encode_test_base64(&pixels);
    let mut stream =
        format!("\x1b_Ga=T,f=24,i=201,s={width},v={height},c={width},r={height},m=1;").into_bytes();
    let mut chunks = encoded.as_bytes().chunks(1024).peekable();
    stream.extend_from_slice(chunks.next().expect("large fixture has a payload"));
    stream.extend_from_slice(b"\x1b\\");
    while let Some(chunk) = chunks.next() {
        stream.extend_from_slice(b"\x1b_Gm=");
        stream.extend_from_slice(if chunks.peek().is_some() {
            b"1;"
        } else {
            b"0;"
        });
        stream.extend_from_slice(chunk);
        stream.extend_from_slice(b"\x1b\\");
    }

    let terminal = HeadlessKittyTerminal::replay_with_framebuffer(&stream, width, height)
        .expect("large chunked stream should render in the bounded headless terminal");
    assert_eq!(terminal.framebuffer_size(), Some((width, height)));
    assert_eq!(terminal.resource_count(), 1);
    assert_eq!(terminal.placement_count(), 1);
    assert_eq!(
        terminal.visible_pixel_count(),
        usize::from(width) * usize::from(height)
    );
    assert_eq!(
        terminal.pixel(0, 0),
        Some(headless_kitty::HeadlessPixel::rgb(0, 0, 0))
    );
    assert_eq!(
        terminal.pixel(width - 1, height - 1),
        Some(headless_kitty::HeadlessPixel::rgb(95, 63, 158))
    );
}

#[test]
fn retained_graphics_bytes_and_resources_stay_within_session_limits() {
    let limits = cmdash::GraphicsLimits {
        max_decoded_bytes: 1024,
        max_resources: 4,
        max_placements: 8,
    };
    let mut store = SessionGraphicsStore::with_limits(SessionId::new(202), limits);
    let decoded = vec![7_u8; 192];
    let encoded = encode_test_base64(&decoded);

    for _ in 0..32 {
        store
            .apply_kitty_command(b"a=T,f=24,i=202,c=1,r=1,q=2", encoded.as_bytes())
            .expect("retransmitting one image should replace its retained bytes");
    }
    assert_eq!(store.resource_count(), 1);
    assert_eq!(store.decoded_bytes_total(), decoded.len());
    assert!(store.decoded_bytes_total() <= limits.max_decoded_bytes);

    for image in 203..=205 {
        let parameters = format!("a=T,f=24,i={image},c=1,r=1,q=2");
        store
            .apply_kitty_command(parameters.as_bytes(), encoded.as_bytes())
            .expect("resources below the configured quota should be accepted");
    }
    assert_eq!(store.resource_count(), limits.max_resources);
    assert!(store.decoded_bytes_total() <= limits.max_decoded_bytes);

    store
        .apply_kitty_command(b"a=T,f=24,i=206,c=1,r=1,q=2", encoded.as_bytes())
        .expect("resource quota rejection should be a diagnostic outcome");
    assert_eq!(store.resource_count(), limits.max_resources);
    assert!(
        store
            .diagnostics()
            .iter()
            .any(|diagnostic| { diagnostic.message().contains("resource limit") })
    );

    store.clear();
    assert_eq!(store.resource_count(), 0);
    assert_eq!(store.placement_count(), 0);
    assert_eq!(store.decoded_bytes_total(), 0);
}

#[cfg(feature = "sixel")]
#[test]
fn sixel_adapter_is_accepted_as_a_bounded_outer_stream() {
    use cmdash::{SixelImage, SixelSubmission};

    let image = SixelSubmission::new(
        1,
        2,
        SixelImage {
            width: 1,
            height: 1,
            rgb: &[255, 255, 255],
        },
    )
    .unwrap();
    let capabilities = BackendCapabilities {
        truecolor: true,
        mouse: true,
        bracketed_paste: true,
        kitty_graphics: false,
        kitty_unicode_placeholders: false,
        graphics_source: GraphicsCapabilitySource::Unavailable,
        graphics_confidence: GraphicsCapabilityConfidence::Rejected,
        kitty_passthrough: false,
        kitty_text_fallback: false,
        sixel: true,
    };
    let mut backend = CrosstermBackend::new(Vec::new()).with_capabilities(capabilities);
    backend.submit_sixel(&[image]).unwrap();
    assert!(backend.writer().starts_with(b"\x1b[3;2H\x1bPq"));
    assert!(backend.writer().ends_with(b"\x1b\\"));
}

#[test]
fn text_fallback_matches_captured_degraded_stream() {
    let submission = captured_submission(11, 1, 1);
    let mut backend = CrosstermBackend::new(Vec::new())
        .with_capabilities(capabilities(false, false, false, true));

    let status = backend
        .submit_graphics(
            std::slice::from_ref(&submission),
            std::slice::from_ref(&submission),
            &[],
        )
        .expect("fallback capture should write");
    assert!(matches!(status, GraphicsSubmissionStatus::Degraded { .. }));
    assert_eq!(backend.writer(), b"\x1b[1;1H[image:11]");
    let model = HeadlessKittyTerminal::replay(backend.writer()).unwrap();
    assert_eq!(model.text(), "[image:11]");
    assert_eq!(model.resource_count(), 0);
}
