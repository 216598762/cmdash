#[path = "support/headless_kitty.rs"]
mod headless_kitty;

use cmdash::{
    Backend, BackendCapabilities, Compositor, CrosstermBackend, GraphicsAnimationState,
    GraphicsCapabilityConfidence, GraphicsCapabilitySource, GraphicsScreen, GraphicsScrollRegion,
    GraphicsSubmission, GraphicsSubmissionStatus, ImagePlacement, Scene, SessionGraphicsStore,
    SessionId, TerminalSession, TerminalSize,
};
use headless_kitty::{HeadlessKittyTerminal, HeadlessPixel};
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
        cell_size: None,
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

/// A real two-frame 1x1 animated GIF: frame 1 is opaque red, frame 2 is
/// opaque blue, each with a 100 ms delay, looping forever.
fn animated_gif_for_conformance() -> Vec<u8> {
    let palette = [255, 0, 0, 0, 0, 255]; // index 0: red, index 1: blue
    let mut output = Vec::new();
    {
        let mut encoder = gif::Encoder::new(&mut output, 1, 1, &palette).unwrap();
        encoder.set_repeat(gif::Repeat::Infinite).unwrap();
        let first = gif::Frame {
            delay: 10, // 100 ms
            width: 1,
            height: 1,
            buffer: std::borrow::Cow::Owned(vec![0]),
            ..gif::Frame::default()
        };
        encoder.write_frame(&first).unwrap();
        let second = gif::Frame {
            delay: 10, // 100 ms
            width: 1,
            height: 1,
            buffer: std::borrow::Cow::Owned(vec![1]),
            ..gif::Frame::default()
        };
        encoder.write_frame(&second).unwrap();
    }
    output
}

/// Encodes an RGBA image as a PNG for the non-raw composition conformance
/// fixture.
fn png_fixture(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut output, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(rgba).unwrap();
        writer.finish().unwrap();
    }
    output
}

/// Decodes base64 produced by [`encode_test_base64`] back into bytes.
fn decode_test_base64(encoded: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    let mut accumulator = 0_u32;
    let mut bits = 0_u8;
    for byte in encoded
        .iter()
        .copied()
        .filter(|byte| !byte.is_ascii_whitespace())
    {
        if byte == b'=' {
            break;
        }
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => continue,
        } as u32;
        accumulator = (accumulator << 6) | value;
        bits = bits.saturating_add(6);
        if bits >= 8 {
            bits -= 8;
            output.push((accumulator >> bits) as u8);
            accumulator &= (1_u32 << bits).saturating_sub(1);
        }
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
fn headless_model_and_pty_session_agree_on_equal_z_image_id_tie_break() {
    // Two overlapping placements at the same z-index: the higher image id is
    // transmitted first, but Kitty draws equal-z overlaps in ascending image
    // id order, so the lower id is first and the higher id occludes it.
    let stream = b"\x1b[1;1H\x1b_Ga=T,f=24,i=52,s=1,v=1,c=1,r=1,C=1,z=0,q=2;AP8A\x1b\\\
        \x1b[1;1H\x1b_Ga=T,f=24,i=51,s=1,v=1,c=1,r=1,C=1,z=0,q=2;/wAA\x1b\\";
    let model = HeadlessKittyTerminal::replay(stream).unwrap();
    assert_eq!(model.placement_count(), 2);
    let order = model
        .placements_in_z_order()
        .into_iter()
        .map(|placement| placement.image_id)
        .collect::<Vec<_>>();
    assert_eq!(order, vec![51, 52]);

    // The higher id must occlude the lower id on the deterministic framebuffer.
    let terminal = HeadlessKittyTerminal::replay_with_framebuffer(stream, 2, 1).unwrap();
    assert_eq!(
        terminal.pixel(0, 0),
        Some(headless_kitty::HeadlessPixel::rgb(0, 255, 0))
    );

    // The PTY session's visible submissions must be ordered the same way.
    let script = r"printf '\033[1;1H\033_Ga=T,f=24,i=52,s=1,v=1,c=1,r=1,C=1,z=0,q=2;AP8A\033\\\033[1;1H\033_Ga=T,f=24,i=51,s=1,v=1,c=1,r=1,C=1,z=0,q=2;/wAA\033\\'";
    let mut session = TerminalSession::spawn_with_session_id(
        SessionId::new(52),
        Some("sh"),
        &["-c", script],
        TerminalSize::new(4, 2),
    )
    .expect("could not spawn equal-z tie-break fixture");
    let area = Rect::new(0, 0, 4, 2);
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && session.graphics(area).len() < 2 {
        session
            .poll_output()
            .expect("equal-z tie-break fixture PTY failed");
        thread::sleep(Duration::from_millis(5));
    }
    let ids = session
        .graphics(area)
        .iter()
        .map(|submission| submission.resource().image())
        .collect::<Vec<_>>();
    assert_eq!(ids, vec![51, 52]);
    session
        .shutdown()
        .expect("could not shut down equal-z tie-break fixture");
}

#[test]
fn composited_scene_tie_breaks_equal_z_across_sessions_by_resource_id() {
    // Two sessions upload an image with the *same* client id (5) at the same
    // z-index. A single session breaks the tie by ascending image id; across
    // sessions the tie-break must stay total, so the full resource id
    // (session, image) orders the composited scene. The lower session id wins
    // even though the image id collides.
    let area = Rect::new(0, 0, 8, 2);
    let mut first = SessionGraphicsStore::new(SessionId::new(100));
    first
        .apply_kitty_command_with_context(b"a=T,f=24,i=5,z=0,c=1,r=1,q=2", b"AQID", (0, 0), (0, 0))
        .unwrap();
    let mut second = SessionGraphicsStore::new(SessionId::new(200));
    second
        .apply_kitty_command_with_context(b"a=T,f=24,i=5,z=0,c=1,r=1,q=2", b"BAUG", (0, 0), (0, 0))
        .unwrap();

    // Add the higher-session layer first, so an insertion-order-preserving
    // sort would leave session 200 on top; the scene must re-sort by
    // (z, session, image) instead.
    let mut composed = Scene::new(area);
    composed.add_image_layer(second.visible_submissions(area).into_iter().next().unwrap());
    composed.add_image_layer(first.visible_submissions(area).into_iter().next().unwrap());

    let ids = composed
        .image_layers()
        .iter()
        .map(|layer| (layer.resource().session().get(), layer.resource().image()))
        .collect::<Vec<_>>();
    assert_eq!(ids, vec![(100, 5), (200, 5)]);
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
    // Lowercase `d=i` releases the placement but retains the image data
    // (verified against a real Kitty), so the resource survives for a
    // re-display without retransmission.
    assert_eq!(terminal.resource_count(), 1);
}

#[test]
fn clipped_placements_render_the_visible_sub_image_in_the_outer_terminal() {
    // A 2x2 pixel image (red, green / blue, white) displayed over 2x2 cells.
    // Clipping to the right column must render only the green/white pixels,
    // not a squashed copy of the whole image.
    let mut store = SessionGraphicsStore::new(SessionId::new(0));
    store
        .apply_kitty_command_with_context(
            b"a=T,f=24,i=511,s=2,v=2,c=2,r=2,q=2",
            b"/wAAAP8AAAD/////",
            (0, 0),
            (0, 0),
        )
        .unwrap();
    let submission = store
        .visible_submissions(Rect::new(0, 0, 8, 4))
        .into_iter()
        .next()
        .unwrap();
    let clipped = submission.clipped_to(Rect::new(1, 0, 1, 2)).unwrap();

    let mut backend = CrosstermBackend::new(Vec::new())
        .with_capabilities(capabilities(true, false, false, false));
    backend
        .submit_graphics(
            std::slice::from_ref(&clipped),
            std::slice::from_ref(&clipped),
            &[],
        )
        .expect("clipped placement should serialize");

    let terminal = HeadlessKittyTerminal::replay_with_framebuffer(backend.writer(), 2, 2)
        .expect("clipped placement should replay");
    assert_eq!(
        terminal.pixel(1, 0),
        Some(headless_kitty::HeadlessPixel::rgb(0, 255, 0))
    );
    assert_eq!(
        terminal.pixel(1, 1),
        Some(headless_kitty::HeadlessPixel::rgb(255, 255, 255))
    );
    assert_eq!(terminal.visible_pixel_count(), 2);
}

#[test]
fn sub_cell_offset_clips_replay_with_a_pixel_shifted_crop() {
    // A 30x10 image drawn at its natural size with an X=6 offset in 10x10
    // cells occupies cells 0..4. Clipping past the first cell must re-emit a
    // crop starting at source pixel 4 (six pixels into the anchor cell), not
    // the whole-cell fraction 30 / 4 == 7.
    let mut store = SessionGraphicsStore::new(SessionId::new(512));
    store
        .apply_kitty_command_with_context(
            b"a=T,f=24,i=512,s=30,v=10,X=6,q=2",
            b"AQID",
            (0, 0),
            (10, 10),
        )
        .unwrap();
    let submission = store
        .visible_submissions(Rect::new(0, 0, 8, 4))
        .into_iter()
        .next()
        .unwrap();
    let clipped = submission.clipped_to(Rect::new(1, 0, 3, 1)).unwrap();

    let mut backend = CrosstermBackend::new(Vec::new())
        .with_capabilities(capabilities(true, false, false, false));
    backend
        .submit_graphics(
            std::slice::from_ref(&clipped),
            std::slice::from_ref(&clipped),
            &[],
        )
        .expect("offset-clipped placement should serialize");

    let terminal = HeadlessKittyTerminal::replay(backend.writer())
        .expect("offset-clipped placement should replay");
    let placement = terminal
        .placements()
        .first()
        .expect("the replayed stream should contain one placement");
    assert_eq!((placement.x, placement.y), (1, 0));
    assert_eq!((placement.width, placement.height), (3, 1));
    assert_eq!(placement.source(), Some((4, 0, 26, 10)));
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
        b"\x1b[1;1H\x1b_Ga=T,f=24,i=7,c=2,r=1,C=1,q=2,m=0,p=1;AQID\x1b\\\x1b[?25l"
    );
    let model = HeadlessKittyTerminal::replay(backend.writer()).unwrap();
    assert_eq!(model.actions(), &["transmit"]);
    assert_eq!(model.resource_count(), 1);
    assert_eq!(model.placement_count(), 1);
    assert_eq!(model.resource_payload(7), Some(&b"AQID"[..]));
}

#[test]
fn direct_adapter_preserves_sub_cell_offsets_in_the_outer_stream() {
    let mut store = SessionGraphicsStore::new(SessionId::new(0));
    store
        .apply_kitty_command_with_context(
            b"a=T,f=24,i=450,c=2,r=1,X=4,Y=6,q=2",
            b"AQID",
            (0, 0),
            (10, 20),
        )
        .expect("sub-cell offset fixture should be accepted");
    let submission = store
        .visible_submissions(Rect::new(0, 0, 16, 8))
        .into_iter()
        .next()
        .expect("sub-cell offset fixture should create a placement");

    let mut backend = CrosstermBackend::new(Vec::new())
        .with_capabilities(capabilities(true, false, false, false));
    backend
        .submit_graphics(
            std::slice::from_ref(&submission),
            std::slice::from_ref(&submission),
            &[],
        )
        .expect("sub-cell offset capture should write");

    let output = backend.writer();
    assert!(
        output.windows(9).any(|window| window == b",X=4,Y=6;"),
        "sub-cell offsets were not serialized: {:?}",
        String::from_utf8_lossy(output)
    );
    let model = HeadlessKittyTerminal::replay(output).unwrap();
    assert_eq!(model.actions(), &["transmit"]);
    assert_eq!(model.resource_count(), 1);
    assert_eq!(model.placement_count(), 1);
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
        b"\x1b[1;1H\x1b_Ga=p,i=8,c=1,r=1,C=1,q=2,p=1;\x1b\\\x1b[?25l"
    );
    assert_eq!(backend.metrics().graphics_uploads, 1);
    assert_eq!(backend.metrics().graphics_reuses, 1);
    let model = HeadlessKittyTerminal::replay(backend.writer()).unwrap();
    assert_eq!(model.actions(), &["transmit", "place"]);
    assert_eq!(model.resource_count(), 1);
    // The re-place carries the same stable `p=1` id, so the outer terminal
    // reuses the existing placement instead of stacking a duplicate.
    assert_eq!(model.placement_count(), 1);
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
    // Lowercase `d=i` keeps the image data at the outer terminal (verified
    // against a real Kitty), so the resource survives for re-display.
    assert_eq!(model.resource_count(), 1);
    assert_eq!(model.placement_count(), 0);
}

#[test]
fn scrolled_placement_replaces_in_place_without_a_stale_ghost() {
    // A placement re-anchored by scrollback view movement must not leave a
    // ghost painted at its old cells: the backend re-places it with the same
    // stable `p=` id, so the outer terminal moves the placement (Kitty's
    // `grman_put` reuses the ref by `(i, p)`) instead of stacking a second
    // placement. This is the end-to-end regression for the scroll-tearing
    // report: the diff sees the placement as removed (old cell) and changed
    // (new cell) simultaneously.
    let mut store = SessionGraphicsStore::new(SessionId::new(1));
    store
        .apply_kitty_command_with_scroll_region(
            b"a=T,f=24,i=7,c=1,r=1,q=2",
            b"AQID",
            (0, 1),
            (10, 20),
            0,
            GraphicsScreen::Primary,
            GraphicsScrollRegion::new(0, 6, 6),
            0,
        )
        .unwrap();
    let area = Rect::new(0, 0, 4, 3);
    let mut compositor = Compositor::new();
    let mut backend = CrosstermBackend::new(Vec::new())
        .with_capabilities(capabilities(true, false, false, false));

    // Frame 1: the placement is visible at row 1.
    let frame1 = store.visible_submissions_with_state(area, 0, GraphicsScreen::Primary);
    assert_eq!(frame1.len(), 1);
    assert_eq!(frame1[0].placement().y(), 1);
    let mut scene1 = Scene::new(area);
    scene1.add_image_layer(frame1[0].clone());
    let diff1 = compositor.diff(&scene1);
    backend.submit_diff(&diff1).unwrap();
    backend
        .submit_graphics_frame(
            diff1.graphics(),
            diff1.visible_graphics(),
            diff1.removed_graphics(),
            diff1.visible_placeholders(),
            diff1.removed_placeholders(),
        )
        .unwrap();
    let first_len = backend.writer().len();

    // Frame 2: one line of history scrolls in; the same placement resolves to
    // row 0 and keeps its stable key and outer placement id.
    let second = store.visible_submissions_with_state(area, 1, GraphicsScreen::Primary);
    assert_eq!(second[0].placement().y(), 0);
    assert_eq!(second[0].placement().key(), frame1[0].placement().key());
    assert_eq!(
        second[0].placement().outer_placement_id(),
        frame1[0].placement().outer_placement_id()
    );
    let mut scene2 = Scene::new(area);
    scene2.add_image_layer(second[0].clone());
    let diff2 = compositor.diff(&scene2);
    assert_eq!(diff2.graphics().len(), 1);
    assert_eq!(diff2.removed_graphics().len(), 1);
    backend.submit_diff(&diff2).unwrap();
    backend
        .submit_graphics_frame(
            diff2.graphics(),
            diff2.visible_graphics(),
            diff2.removed_graphics(),
            diff2.visible_placeholders(),
            diff2.removed_placeholders(),
        )
        .unwrap();

    let scroll_stream = &backend.writer()[first_len..];
    assert!(
        !scroll_stream.windows(6).any(|window| window == b"a=d,d="),
        "a moved placement must not be deleted: {:?}",
        String::from_utf8_lossy(scroll_stream)
    );
    let re_place = format!("q=2,p={};", second[0].placement().outer_placement_id());
    assert!(
        scroll_stream
            .windows(re_place.len())
            .any(|window| window == re_place.as_bytes()),
        "the re-place must keep the stable placement id: {:?}",
        String::from_utf8_lossy(scroll_stream)
    );
    let terminal = HeadlessKittyTerminal::replay_with_framebuffer(backend.writer(), 4, 3).unwrap();
    assert_eq!(terminal.resource_count(), 1);
    assert_eq!(
        terminal.placement_count(),
        1,
        "a moved placement must not leave a ghost at the outer terminal"
    );
    assert_eq!(
        (terminal.placements()[0].x, terminal.placements()[0].y),
        (0, 0)
    );

    // Frame 3: the placement is removed entirely; its last placement frees
    // the whole image with an image-level delete.
    let scroll_len = backend.writer().len();
    let diff3 = compositor.diff(&Scene::new(area));
    backend.submit_diff(&diff3).unwrap();
    backend
        .submit_graphics_frame(
            diff3.graphics(),
            diff3.visible_graphics(),
            diff3.removed_graphics(),
            diff3.visible_placeholders(),
            diff3.removed_placeholders(),
        )
        .unwrap();
    let cleanup = &backend.writer()[scroll_len..];
    assert!(
        cleanup.windows(6).any(|window| window == b"a=d,d="),
        "removing the last placement must release it at the outer terminal: {:?}",
        String::from_utf8_lossy(cleanup)
    );
    let terminal = HeadlessKittyTerminal::replay_with_framebuffer(backend.writer(), 4, 3).unwrap();
    // The lowercase delete keeps the image data (verified against a real
    // Kitty), so a scrolled-away image re-displays without retransmission.
    assert_eq!(terminal.resource_count(), 1);
    assert_eq!(terminal.placement_count(), 0);
}

#[test]
fn pty_scroll_moves_and_removes_outer_placements_in_step_with_text() {
    // End-to-end regression for scroll tearing: a real PTY places an image,
    // one line of output scrolls it into history, and navigating the view
    // must re-place the same placement (same `p=` id, no ghost) and, when it
    // leaves the view, delete the image cleanly.
    let script = r"printf '\033[1;1H\033_Ga=T,f=24,i=31,s=1,v=1,c=1,r=1,C=1,q=2;AQID\033\\'; for i in 0 1 2 3 4 5 6 7 8 9; do echo row$i; done";
    let mut session = TerminalSession::spawn_with_session_id(
        SessionId::new(31),
        Some("sh"),
        &["-c", script],
        TerminalSize::new(6, 4),
    )
    .expect("could not spawn scroll-tearing fixture");
    let area = Rect::new(0, 0, 6, 4);
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && session.scrollback_lines() < 1 {
        session
            .poll_output()
            .expect("scroll-tearing fixture PTY failed");
        thread::sleep(Duration::from_millis(5));
    }
    assert!(
        session.scrollback_lines() >= 1,
        "image should have scrolled into history"
    );

    let mut compositor = Compositor::new();
    let mut backend = CrosstermBackend::new(Vec::new())
        .with_capabilities(capabilities(true, false, false, false));

    // Scroll to the top: the history line carrying the image is at row 0.
    assert!(session.scroll_display(alacritty_terminal::grid::Scroll::Top));
    let mut scene1 = session.render(area, false);
    for graphics in session.graphics(area) {
        scene1.add_image_layer(graphics);
    }
    assert_eq!(scene1.image_layers().len(), 1);
    assert_eq!(scene1.image_layers()[0].placement().y(), 0);
    let diff1 = compositor.diff(&scene1);
    backend.submit_diff(&diff1).unwrap();
    backend
        .submit_graphics_frame(
            diff1.graphics(),
            diff1.visible_graphics(),
            diff1.removed_graphics(),
            diff1.visible_placeholders(),
            diff1.removed_placeholders(),
        )
        .unwrap();
    let first_len = backend.writer().len();

    // Scroll back down one line: the image leaves the view. The backend must
    // emit a delete and the outer terminal must end up with nothing painted.
    assert!(session.scroll_display(alacritty_terminal::grid::Scroll::Delta(-1)));
    let mut scene2 = session.render(area, false);
    for graphics in session.graphics(area) {
        scene2.add_image_layer(graphics);
    }
    assert!(scene2.image_layers().is_empty());
    let diff2 = compositor.diff(&scene2);
    assert_eq!(diff2.removed_graphics().len(), 1);
    backend.submit_diff(&diff2).unwrap();
    backend
        .submit_graphics_frame(
            diff2.graphics(),
            diff2.visible_graphics(),
            diff2.removed_graphics(),
            diff2.visible_placeholders(),
            diff2.removed_placeholders(),
        )
        .unwrap();
    let cleanup = &backend.writer()[first_len..];
    assert!(
        cleanup.windows(6).any(|window| window == b"a=d,d="),
        "leaving the view must delete the placement: {:?}",
        String::from_utf8_lossy(cleanup)
    );
    let terminal = HeadlessKittyTerminal::replay_with_framebuffer(backend.writer(), 6, 4).unwrap();
    // Lowercase `d=i` releases the placement but retains the image data.
    assert_eq!(terminal.resource_count(), 1);
    assert_eq!(terminal.placement_count(), 0);
    session
        .shutdown()
        .expect("could not shut down scroll-tearing fixture");
}

#[test]
fn direct_mode_emits_graphics_before_the_text_frame() {
    // Images must lead, not trail, the scrolled text: in direct placement
    // mode the graphics commands (upload/place/delete) are independent of the
    // text frame, so they are flushed first. Mirror the ordering main.rs
    // applies and assert the byte order the outer terminal receives.
    let script = r"printf '\033[1;1H\033_Ga=T,f=24,i=31,s=1,v=1,c=1,r=1,C=1,q=2;AQID\033\\'; for i in 0 1 2 3 4 5 6 7 8 9; do echo row$i; done";
    let mut session = TerminalSession::spawn_with_session_id(
        SessionId::new(31),
        Some("sh"),
        &["-c", script],
        TerminalSize::new(6, 4),
    )
    .expect("could not spawn graphics-first fixture");
    let area = Rect::new(0, 0, 6, 4);
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && session.scrollback_lines() < 1 {
        session
            .poll_output()
            .expect("graphics-first fixture PTY failed");
        thread::sleep(Duration::from_millis(5));
    }
    assert!(
        session.scrollback_lines() >= 1,
        "image should have scrolled into history"
    );

    let mut compositor = Compositor::new();
    let mut backend = CrosstermBackend::new(Vec::new())
        .with_capabilities(capabilities(true, false, false, false));

    // Frame 1: scroll to the top so the image is at row 0. The `a=T` upload
    // must land before the text frame (the frame starts with the cursor
    // Hide `ESC[?25l`; the graphics flush's own MoveTo is not a text frame).
    assert!(session.scroll_display(alacritty_terminal::grid::Scroll::Top));
    let mut scene1 = session.render(area, false);
    for graphics in session.graphics(area) {
        scene1.add_image_layer(graphics);
    }
    let diff1 = compositor.diff(&scene1);
    let deltas1 = session.drain_graphics_deltas(area);
    backend
        .submit_graphics_frame(
            &deltas1.changed,
            diff1.visible_graphics(),
            &deltas1.removed,
            diff1.visible_placeholders(),
            diff1.removed_placeholders(),
        )
        .unwrap();
    backend.submit_diff(&diff1).unwrap();
    let frame1_end = backend.writer().len();
    let frame1 = &backend.writer()[..frame1_end];
    let upload_at = frame1
        .windows(3)
        .position(|window| window == b"a=T")
        .expect("frame 1 must upload the image");
    let text_at = frame1
        .windows(6)
        .position(|window| window == b"\x1b[?25l")
        .expect("frame 1 must emit a text frame");
    assert!(
        upload_at < text_at,
        "direct-mode upload must precede the text frame"
    );

    // Frame 2: scroll down one line so the image leaves the view. The
    // placement delete must arrive before the scrolled text frame.
    assert!(session.scroll_display(alacritty_terminal::grid::Scroll::Delta(-1)));
    let mut scene2 = session.render(area, false);
    for graphics in session.graphics(area) {
        scene2.add_image_layer(graphics);
    }
    assert!(scene2.image_layers().is_empty());
    let diff2 = compositor.diff(&scene2);
    let deltas2 = session.drain_graphics_deltas(area);
    backend
        .submit_graphics_frame(
            &deltas2.changed,
            diff2.visible_graphics(),
            &deltas2.removed,
            diff2.visible_placeholders(),
            diff2.removed_placeholders(),
        )
        .unwrap();
    backend.submit_diff(&diff2).unwrap();
    let frame2 = &backend.writer()[frame1_end..];
    let delete_at = frame2
        .windows(3)
        .position(|window| window == b"a=d")
        .expect("frame 2 must delete the scrolled-out placement");
    let text_at = frame2
        .windows(6)
        .position(|window| window == b"\x1b[?25l")
        .expect("frame 2 must emit a text frame");
    assert!(
        delete_at < text_at,
        "direct-mode delete must precede the scrolled text frame"
    );

    session
        .shutdown()
        .expect("could not shut down graphics-first fixture");
}

#[test]
fn removing_one_placement_keeps_the_image_for_its_other_placements() {
    // Two placements of the same image; removing one must delete exactly that
    // placement (`d=i` scoped with `p=`), keeping the image data alive for
    // the other placement instead of erasing both.
    let mut store = SessionGraphicsStore::new(SessionId::new(1));
    store
        .apply_kitty_command(b"a=T,f=24,i=5,c=1,r=1,q=2", b"AQID")
        .unwrap();
    store
        .apply_kitty_command(b"a=p,i=5,c=1,r=1,q=2", b"")
        .unwrap();
    let visible = store.visible_submissions(Rect::new(0, 0, 4, 2));
    assert_eq!(visible.len(), 2);
    assert_ne!(
        visible[0].placement().outer_placement_id(),
        visible[1].placement().outer_placement_id()
    );
    let mut backend = CrosstermBackend::new(Vec::new())
        .with_capabilities(capabilities(true, false, false, false));
    backend.submit_graphics(&visible, &visible, &[]).unwrap();
    let first_len = backend.writer().len();

    backend
        .submit_graphics(&[], &visible[1..], std::slice::from_ref(&visible[0]))
        .unwrap();
    let scoped = &backend.writer()[first_len..];
    let expected = format!(
        "\x1b_Ga=d,d=i,i={},p={};\x1b\\",
        visible[0].terminal_image_id(),
        visible[0].placement().outer_placement_id()
    );
    assert!(
        scoped
            .windows(expected.len())
            .any(|window| window == expected.as_bytes()),
        "expected a placement-scoped delete: {:?}",
        String::from_utf8_lossy(scoped)
    );
    assert!(
        scoped.windows(6).any(|window| window == b"a=d,d="),
        "expected a delete command: {:?}",
        String::from_utf8_lossy(scoped)
    );
    let model = HeadlessKittyTerminal::replay(backend.writer()).unwrap();
    assert_eq!(
        model.resource_count(),
        1,
        "image data must survive for the other placement"
    );
    assert_eq!(model.placement_count(), 1);
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
        "{}\x1b[38;2;0;0;7m\x1b[1;1H{}{}{}{}{}{}{}{}\x1b[39m\x1b[?25l",
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

    let command = b"\x1b_Ga=T,f=24,i=10,c=1,r=1,C=1,q=2,m=0,p=1;AQID\x1b\\";
    let mut expected = b"\x1b[1;1H\x1bPtmux;".to_vec();
    for byte in command {
        if *byte == 0x1b {
            expected.push(0x1b);
        }
        expected.push(*byte);
    }
    expected.extend_from_slice(b"\x1b\\\x1b[?25l");

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
    assert_eq!(terminal.resource_count(), 1);
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
fn resource_removal_emits_delete_immediately_without_upload_ack() {
    // Uploads are always sent quiet (`q=2`), so the outer terminal never
    // acknowledges; a delete gated on the upload ack could never fire and the
    // removed placement would linger as a ghost. Deletes are idempotent and
    // are emitted unconditionally instead.
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
        .expect("removal should write the delete immediately");
    assert_eq!(
        &backend.writer()[upload_len..],
        b"\x1b_Ga=d,d=i,i=12;\x1b\\"
    );
    // The lowercase delete retains the image data at the outer terminal and
    // the cached resource survives, so a reappearance re-places with a bare
    // `a=p` instead of re-uploading (verified against a real Kitty).
    assert_eq!(backend.metrics().graphics_gc, 0);
    let reuse_start = backend.writer().len();
    backend
        .submit_graphics(
            std::slice::from_ref(&submission),
            std::slice::from_ref(&submission),
            &[],
        )
        .expect("reappearance should reuse the retained resource");
    let re_place = format!("q=2,p={};", submission.placement().outer_placement_id());
    assert!(
        backend.writer()[reuse_start..]
            .windows(re_place.len())
            .any(|window| window == re_place.as_bytes()),
        "reappearance must re-place without re-uploading"
    );
    assert_eq!(backend.metrics().graphics_uploads, 1);
    assert_eq!(backend.metrics().graphics_reuses, 1);

    // An acknowledgement for the still-pending delete reaps the tracked
    // resource without touching the outer terminal's retained data.
    let upload_ack = backend.feed_outer_input(b"\x1b_Gi=12;OK\x1b\\");
    assert_eq!(upload_ack.graphics_acknowledgements.len(), 1);
    assert!(upload_ack.graphics_acknowledgements[0].success);
    assert_eq!(backend.metrics().graphics_gc, 1);
    let model = HeadlessKittyTerminal::replay(backend.writer()).unwrap();
    assert_eq!(model.actions(), &["transmit", "delete", "place"]);
    assert_eq!(model.resource_count(), 1);
    assert_eq!(model.placement_count(), 1);
}

#[test]
fn full_redraw_frames_do_not_clear_and_erase_outer_placements() {
    // Regression for images tearing away from text after a resize or a UI
    // animation. A full redraw rewrites every cell, so emitting `ED 2` on such
    // frames is redundant — and destructive: Kitty's `ED 2` erases visible
    // image placements at the outer terminal, and the graphics path only
    // re-places images whose projection changed, so the images would stay
    // missing until the next scroll. The first frame here is a full redraw and
    // must place the image without clearing it away.
    let script = r"printf '\033[2;1H\033_Ga=T,f=24,i=21,s=1,v=1,c=1,r=1,C=1,q=2;AQID\033\\'; printf 'A\nB\nC\nD\nE\n'";
    let mut session = TerminalSession::spawn_with_session_id(
        SessionId::new(21),
        Some("sh"),
        &["-c", script],
        TerminalSize::new(8, 6),
    )
    .expect("could not spawn full-redraw fixture");
    let area = Rect::new(0, 0, 8, 6);
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && session.scrollback_lines() < 1 {
        session
            .poll_output()
            .expect("full-redraw fixture PTY failed");
        thread::sleep(Duration::from_millis(5));
    }
    session.poll_output().expect("final poll");

    let mut compositor = Compositor::new();
    let mut backend = CrosstermBackend::new(Vec::new())
        .with_capabilities(capabilities(true, false, false, false));
    let mut scene = session.render(area, false);
    for graphics in session.graphics(area) {
        scene.add_image_layer(graphics);
    }
    let diff = compositor.diff(&scene);
    assert!(diff.full_redraw(), "the first frame is a full redraw");
    backend.submit_diff(&diff).unwrap();
    backend
        .submit_graphics_frame(
            diff.graphics(),
            diff.visible_graphics(),
            diff.removed_graphics(),
            diff.visible_placeholders(),
            diff.removed_placeholders(),
        )
        .unwrap();
    assert!(
        !backend
            .writer()
            .windows(4)
            .any(|window| window == b"\x1b[2J"),
        "a full redraw must not emit ED 2: {:?}",
        String::from_utf8_lossy(backend.writer())
    );
    let terminal = HeadlessKittyTerminal::replay(backend.writer()).unwrap();
    assert_eq!(terminal.placement_count(), 1);
    assert_eq!(terminal.actions(), &["transmit"]);
    session
        .shutdown()
        .expect("could not shut down full-redraw fixture");
}

#[test]
fn delete_ack_keeps_a_tombstone_so_reentry_replaces_without_reuploading() {
    // Regression for the scroll-edge flicker: a scrolled-away image is deleted
    // (lowercase `d=i`, data retained at the outer terminal) and the delete is
    // acknowledged. Re-entry must re-place with a bare `a=p` (same image id
    // and generation) instead of re-uploading the payload, which blinked the
    // image every time it crossed the scroll edge.
    let submission = captured_submission(17, 2, 2);
    let mut backend = CrosstermBackend::new(Vec::new())
        .with_capabilities(capabilities(true, false, false, false));
    backend
        .submit_graphics(
            std::slice::from_ref(&submission),
            std::slice::from_ref(&submission),
            &[],
        )
        .expect("initial upload should write");
    assert_eq!(backend.metrics().graphics_uploads, 1);
    let upload_ack = backend.feed_outer_input(b"\x1b_Gi=17;OK\x1b\\");
    assert_eq!(upload_ack.graphics_acknowledgements.len(), 1);

    // The placement leaves the view; the delete is written immediately.
    backend
        .submit_graphics(&[], &[], std::slice::from_ref(&submission))
        .expect("removal should write the delete immediately");
    assert_eq!(backend.metrics().graphics_gc, 0);

    // The outer terminal confirms the delete, retaining the image data.
    let delete_ack = backend.feed_outer_input(b"\x1b_Gi=17;OK\x1b\\");
    assert_eq!(delete_ack.graphics_acknowledgements.len(), 1);
    assert!(delete_ack.graphics_acknowledgements[0].success);
    assert_eq!(backend.metrics().graphics_gc, 1);

    // Re-entry must re-place with the stable `p=` id, not re-upload.
    let reentry_start = backend.writer().len();
    backend
        .submit_graphics(
            std::slice::from_ref(&submission),
            std::slice::from_ref(&submission),
            &[],
        )
        .expect("re-entry should reuse the retained resource");
    let reentry = &backend.writer()[reentry_start..];
    let re_place = format!("q=2,p={};", submission.placement().outer_placement_id());
    assert!(
        reentry
            .windows(re_place.len())
            .any(|window| window == re_place.as_bytes()),
        "re-entry must re-place without re-uploading: {:?}",
        String::from_utf8_lossy(reentry)
    );
    assert!(
        !reentry.windows(4).any(|window| window == b"a=T,"),
        "re-entry must not re-upload the payload: {:?}",
        String::from_utf8_lossy(reentry)
    );
    assert_eq!(backend.metrics().graphics_uploads, 1);
    assert_eq!(backend.metrics().graphics_reuses, 1);
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
fn kitty_placements_advance_the_emulator_cursor_like_a_graphics_terminal() {
    // A default (C=0) placement advances the cursor right by `c` cells and
    // down by `r` cells, so the trailing `X` lands below the 2x1 image instead
    // of overwriting its top-left cell.
    let script = r"printf '\033_Ga=T,f=24,i=401,s=2,v=1,c=2,r=1,q=2;AQID\033\\X'";
    let mut session = TerminalSession::spawn_with_session_id(
        SessionId::new(401),
        Some("sh"),
        &["-c", script],
        TerminalSize::new(20, 4),
    )
    .expect("could not spawn cursor-movement fixture");
    let area = Rect::new(0, 0, 20, 4);
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && session.cursor_position() != (3, 1) {
        session
            .poll_output()
            .expect("cursor-movement fixture PTY failed");
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(session.cursor_position(), (3, 1));

    let submissions = session.graphics(area);
    assert_eq!(submissions.len(), 1);
    assert_eq!(submissions[0].placement().area(), Rect::new(0, 0, 2, 1));

    let scene = session.render(area, false);
    assert_eq!(scene.cell_at(2, 1).map(|cell| cell.symbol), Some('X'));
    session
        .shutdown()
        .expect("could not shut down cursor-movement fixture");
}

#[test]
fn headless_model_advances_the_cursor_after_default_placements() {
    // Two default (C=0) placements stack after each other's cursor movement:
    // the 2x1 image moves the cursor from (0,0) to (2,1), so the second image
    // lands below it.
    let stream =
        b"\x1b_Ga=T,f=24,i=501,c=2,r=1,q=2;AQID\x1b\\\x1b_Ga=T,f=24,i=502,c=1,r=1,q=2;BAUG\x1b\\";
    let model = HeadlessKittyTerminal::replay(stream).unwrap();

    assert_eq!(model.placement_count(), 2);
    assert_eq!((model.placements()[0].x, model.placements()[0].y), (0, 0));
    assert_eq!((model.placements()[1].x, model.placements()[1].y), (2, 1));
    assert_eq!(model.cursor(), (3, 2));
}

#[test]
fn headless_model_respects_static_and_transmit_only_cursor_policy() {
    // C=1 keeps the cursor fixed, so both images share the top-left cell.
    let stream = b"\x1b_Ga=T,f=24,i=503,c=2,r=1,C=1,q=2;AQID\x1b\\\x1b_Ga=T,f=24,i=504,c=1,r=1,C=1,q=2;BAUG\x1b\\";
    let model = HeadlessKittyTerminal::replay(stream).unwrap();
    assert_eq!(model.placement_count(), 2);
    assert_eq!((model.placements()[0].x, model.placements()[0].y), (0, 0));
    assert_eq!((model.placements()[1].x, model.placements()[1].y), (0, 0));
    assert_eq!(model.cursor(), (0, 0));

    // A lowercase transmit only stores the image; it neither displays nor
    // moves the cursor, so the later a=T placement starts at (0,0).
    let stream =
        b"\x1b_Ga=t,f=24,i=505,s=2,v=1,q=2;AQID\x1b\\\x1b_Ga=T,f=24,i=506,c=1,r=1,q=2;BAUG\x1b\\";
    let model = HeadlessKittyTerminal::replay(stream).unwrap();
    assert_eq!(model.placement_count(), 1);
    assert_eq!((model.placements()[0].x, model.placements()[0].y), (0, 0));
    assert_eq!(model.cursor(), (1, 1));
}

#[test]
fn headless_model_and_pty_session_agree_on_cursor_advancement() {
    // The same raw child stream must place its image and advance the cursor to
    // identical positions in both the headless reference terminal and the
    // cmdash session emulator.
    let stream = b"\x1b_Ga=T,f=24,i=510,s=2,v=1,c=2,r=1,q=2;AQID\x1b\\X";
    let model = HeadlessKittyTerminal::replay(stream).unwrap();
    assert_eq!(model.placement_count(), 1);
    assert_eq!((model.placements()[0].x, model.placements()[0].y), (0, 0));
    assert_eq!(
        (model.placements()[0].width, model.placements()[0].height),
        (2, 1)
    );
    assert_eq!(model.cursor(), (3, 1));

    let script = r"printf '\033_Ga=T,f=24,i=510,s=2,v=1,c=2,r=1,q=2;AQID\033\\X'";
    let mut session = TerminalSession::spawn_with_session_id(
        SessionId::new(510),
        Some("sh"),
        &["-c", script],
        TerminalSize::new(20, 4),
    )
    .expect("could not spawn cursor agreement fixture");
    let area = Rect::new(0, 0, 20, 4);
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && session.cursor_position() != (3, 1) {
        session
            .poll_output()
            .expect("cursor agreement fixture PTY failed");
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(session.cursor_position(), (3, 1));
    let submissions = session.graphics(area);
    assert_eq!(submissions.len(), 1);
    assert_eq!(submissions[0].placement().area(), Rect::new(0, 0, 2, 1));
    assert_eq!(
        session
            .render(area, false)
            .cell_at(2, 1)
            .map(|cell| cell.symbol),
        Some('X')
    );
    session
        .shutdown()
        .expect("could not shut down cursor agreement fixture");
}

#[test]
fn headless_model_and_pty_session_agree_on_relative_placements() {
    // A parent at (0,0) with a 1x1 extent moves the cursor to (1,1). The child
    // is relative to it with H=3,V=2, so it lands at (3,2) and must not move
    // the cursor further.
    let stream = b"\x1b_Ga=T,f=24,i=602,s=2,v=1,c=1,r=1,p=1,q=2;AQID\x1b\\\x1b_Ga=p,i=602,p=2,P=602,Q=1,H=3,V=2,c=1,r=1,q=2\x1b\\";
    let model = HeadlessKittyTerminal::replay(stream).unwrap();
    assert_eq!(model.placement_count(), 2);
    assert_eq!((model.placements()[0].x, model.placements()[0].y), (0, 0));
    assert_eq!((model.placements()[1].x, model.placements()[1].y), (3, 2));
    assert_eq!(model.cursor(), (1, 1));

    let script = r"printf '\033_Ga=T,f=24,i=602,s=2,v=1,c=1,r=1,p=1,q=2;AQID\033\\\033_Ga=p,i=602,p=2,P=602,Q=1,H=3,V=2,c=1,r=1,q=2\033\\'";
    let mut session = TerminalSession::spawn_with_session_id(
        SessionId::new(602),
        Some("sh"),
        &["-c", script],
        TerminalSize::new(20, 4),
    )
    .expect("could not spawn relative placement fixture");
    let area = Rect::new(0, 0, 20, 4);
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && session.graphics(area).len() < 2 {
        session
            .poll_output()
            .expect("relative placement fixture PTY failed");
        thread::sleep(Duration::from_millis(5));
    }
    let submissions = session.graphics(area);
    assert_eq!(submissions.len(), 2);
    let areas = submissions
        .iter()
        .map(|submission| submission.placement().area())
        .collect::<Vec<_>>();
    assert!(areas.contains(&Rect::new(0, 0, 1, 1)));
    assert!(areas.contains(&Rect::new(3, 2, 1, 1)));
    session
        .shutdown()
        .expect("could not shut down relative placement fixture");
}

#[test]
fn headless_model_and_pty_session_agree_on_virtual_parent_origin() {
    // A virtual parent (U=1) has no physical cell of its own: its origin is
    // the min x / min y of the Unicode placeholder cells written after it.
    // Placeholders land at (5,2) and (3,6), so a child with H=1,V=1 resolves
    // to (3,2)+(1,1) = (4,3) instead of the creating cursor.
    let stream = "\u{1b}_Ga=T,f=24,i=800,s=2,v=1,c=1,r=1,U=1,p=1,q=2;AQID\u{1b}\u{5c}\u{1b}[3;6H\u{1b}[38;2;0;3;32m\u{10eeee}\u{305}\u{305}\u{305}\u{1b}[7;4H\u{1b}[38;2;0;3;32m\u{10eeee}\u{305}\u{305}\u{305}\u{1b}_Ga=p,i=800,p=2,P=800,Q=1,H=1,V=1,c=1,r=1,q=2\u{1b}\u{5c}";
    let model = HeadlessKittyTerminal::replay(stream.as_bytes()).unwrap();
    assert_eq!(model.virtual_placement_count(), 1);
    assert_eq!(model.placeholder_count(), 2);
    assert_eq!(model.placement_count(), 1);
    assert_eq!((model.placements()[0].x, model.placements()[0].y), (4, 3));

    // The placeholder grapheme is emitted as octal `printf` escapes
    // (`\364\216\273\256` = U+10EEEE, `\314\205` = U+0305) rather than
    // `\U`/`\u`, which are bash-only. Ubuntu CI runs dash as /bin/sh, whose
    // printf does not decode `\U` and would drop the placeholder cells.
    let script = r#"printf '\033_Ga=T,f=24,i=800,s=2,v=1,c=1,r=1,U=1,p=1,q=2;AQID\033\\\033[3;6H\033[38;2;0;3;32m\364\216\273\256\314\205\314\205\314\205\033[7;4H\033[38;2;0;3;32m\364\216\273\256\314\205\314\205\314\205\033_Ga=p,i=800,p=2,P=800,Q=1,H=1,V=1,c=1,r=1,q=2\033\\'"#;
    let mut session = TerminalSession::spawn_with_session_id(
        SessionId::new(800),
        Some("sh"),
        &["-c", script],
        TerminalSize::new(20, 6),
    )
    .expect("could not spawn virtual-parent origin fixture");
    let area = Rect::new(0, 0, 20, 6);
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && session.graphics(area).is_empty() {
        session
            .poll_output()
            .expect("virtual-parent origin fixture PTY failed");
        thread::sleep(Duration::from_millis(5));
    }
    let submissions = session.graphics(area);
    assert_eq!(submissions.len(), 1);
    assert_eq!(submissions[0].resource().image(), 800);
    assert_eq!(submissions[0].placement().area(), Rect::new(4, 3, 1, 1));
    session
        .shutdown()
        .expect("could not shut down virtual-parent origin fixture");
}

#[test]
fn headless_model_and_pty_session_agree_on_virtual_placement_selector_immunity() {
    // A virtual placement (U=1) and a real placement. The virtual one is
    // invisible and immune to the position/visible delete selectors; only the
    // id selector removes it.
    let stream = b"\x1b_Ga=T,f=24,i=701,c=1,r=1,U=1,q=2;AQID\x1b\\\x1b[1;5H\x1b_Ga=T,f=24,i=702,c=1,r=1,q=2;BAUG\x1b\\";
    let mut model = HeadlessKittyTerminal::replay_with_viewport(stream, Some((8, 4))).unwrap();
    assert_eq!(model.virtual_placement_count(), 1);
    assert_eq!(model.placement_count(), 1);

    // d=p targeting the virtual placement's cell (1,1) leaves it alone.
    model.feed(b"\x1b_Ga=d,d=p,x=1,y=1\x1b\\").unwrap();
    model.finish().unwrap();
    assert_eq!(model.virtual_placement_count(), 1);
    assert_eq!(model.placement_count(), 1);

    // d=a (delete visible) removes the real placement but not the virtual one.
    model.feed(b"\x1b_Ga=d,d=a\x1b\\").unwrap();
    model.finish().unwrap();
    assert_eq!(model.virtual_placement_count(), 1);
    assert_eq!(model.placement_count(), 0);

    // Only d=i removes the virtual placement.
    model.feed(b"\x1b_Ga=d,d=i,i=701\x1b\\").unwrap();
    model.finish().unwrap();
    assert_eq!(model.virtual_placement_count(), 0);

    let script = r"printf '\033_Ga=T,f=24,i=701,c=1,r=1,U=1,q=2;AQID\033\\\033[1;5H\033_Ga=T,f=24,i=702,c=1,r=1,q=2;BAUG\033\\'";
    let mut session = TerminalSession::spawn_with_session_id(
        SessionId::new(701),
        Some("sh"),
        &["-c", script],
        TerminalSize::new(20, 4),
    )
    .expect("could not spawn virtual placement fixture");
    let area = Rect::new(0, 0, 20, 4);
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && session.graphics(area).is_empty() {
        session
            .poll_output()
            .expect("virtual placement fixture PTY failed");
        thread::sleep(Duration::from_millis(5));
    }
    // The virtual placement never renders: only the real placement is visible.
    let submissions = session.graphics(area);
    assert_eq!(submissions.len(), 1);
    assert_eq!(submissions[0].resource().image(), 702);
    session
        .shutdown()
        .expect("could not shut down virtual placement fixture");
}

#[test]
fn animated_gif_payload_auto_extracts_frames_in_store_and_session() {
    // A client that uploads an animated GIF as a single `f=100` payload (as a
    // graphical terminal would treat it, rather than pre-splitting frames into
    // `a=f` commands like `kitten icat` does) must have its frames extracted
    // and animated by the store itself.
    let gif = animated_gif_for_conformance();
    let encoded = encode_test_base64(&gif);

    // Store path.
    let mut store = SessionGraphicsStore::new(SessionId::new(720));
    let parameters = "a=T,f=100,i=720,q=2";
    store
        .apply_kitty_command_with_context(parameters.as_bytes(), encoded.as_bytes(), (0, 0), (0, 0))
        .expect("animated GIF transmit should be accepted");
    assert_eq!(store.animation_frame_count(720), Some(1));
    assert_eq!(
        store.animation_state(720),
        Some(GraphicsAnimationState::Running)
    );
    // GIF `Repeat::Infinite` maps to Kitty `v=1` (loop forever).
    assert_eq!(store.animation_loops(720), Some(1));
    // The root frame is the first GIF frame coalesced to full-canvas RGBA.
    assert_eq!(store.decoded_bytes(720), Some(&[255, 0, 0, 255][..]));
    // The extra frame is the second GIF frame's coalesced RGBA.
    assert_eq!(
        store.animation_frame_bytes(720, 2),
        Some(&[0, 0, 255, 255][..])
    );
    let submissions = store.visible_submissions(Rect::new(0, 0, 4, 2));
    assert_eq!(submissions.len(), 1);
    assert_eq!(submissions[0].format(), 32);
    assert_eq!(submissions[0].pixel_width(), 1);
    assert_eq!(submissions[0].pixel_height(), 1);

    // PTY session path: the same command through a real child shell.
    let script = format!("printf '\\033_Ga=T,f=100,i=720,q=2;{encoded}\\033\\\\'");
    let mut session = TerminalSession::spawn_with_session_id(
        SessionId::new(720),
        Some("sh"),
        &["-c", &script],
        TerminalSize::new(20, 4),
    )
    .expect("could not spawn animated GIF fixture");
    let area = Rect::new(0, 0, 20, 4);
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && session.graphics(area).is_empty() {
        session
            .poll_output()
            .expect("animated GIF fixture PTY failed");
        thread::sleep(Duration::from_millis(5));
    }
    let submissions = session.graphics(area);
    assert_eq!(submissions.len(), 1);
    assert_eq!(submissions[0].format(), 32);
    let image = submissions[0].resource().image();
    assert_eq!(session.graphics_animation_frame_count(image), Some(1));
    assert_eq!(
        session.graphics_animation_state(image),
        Some(GraphicsAnimationState::Running)
    );
    session
        .shutdown()
        .expect("could not shut down animated GIF fixture");
}

#[test]
fn headless_model_and_pty_session_agree_on_non_raw_frame_composition() {
    // A non-raw (PNG) root composed with a raw frame must decode to RGBA and
    // convert to format 32, identically in the headless reference terminal and
    // the cmdash session emulator.
    let red = [
        255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255,
    ];
    let green = [
        0, 255, 0, 255, 0, 255, 0, 255, 0, 255, 0, 255, 0, 255, 0, 255,
    ];
    let red_pixels = rgba_pixels(&red);
    let green_pixels = rgba_pixels(&green);
    let png = png_fixture(2, 2, &red);
    let png_b64 = encode_test_base64(&png);
    let green_b64 = encode_test_base64(&green);

    // Composing the green frame onto the PNG root decodes the root to RGBA and
    // converts its wire format to 32.
    let onto_root = format!(
        "\x1b_Ga=T,f=100,i=300,s=2,v=2,q=2;{png_b64}\x1b\\\
         \x1b_Ga=f,i=300,r=2,s=2,v=2,q=2;{green_b64}\x1b\\\
         \x1b_Ga=c,i=300,r=2,c=1,X=0,Y=0,x=0,y=0,w=2,h=2,C=1,q=2\x1b\\"
    );
    let model = HeadlessKittyTerminal::replay(onto_root.as_bytes()).unwrap();
    assert_eq!(model.resource_format(300), Some(32));
    assert_eq!(model.resource_pixels(300), Some(green_pixels.as_slice()));

    // Composing the PNG root (non-raw source) onto the frame decodes it and
    // overwrites frame 2 with the root's red pixels.
    let onto_frame = format!(
        "\x1b_Ga=T,f=100,i=301,s=2,v=2,q=2;{png_b64}\x1b\\\
         \x1b_Ga=f,i=301,r=2,s=2,v=2,q=2;{green_b64}\x1b\\\
         \x1b_Ga=c,i=301,r=1,c=2,X=0,Y=0,x=0,y=0,w=2,h=2,C=1,q=2\x1b\\"
    );
    let frame_model = HeadlessKittyTerminal::replay(onto_frame.as_bytes()).unwrap();
    assert_eq!(frame_model.animation_frame_count(301), Some(1));
    assert_eq!(
        frame_model.animation_frame_pixels(301, 2),
        Some(red_pixels.as_slice())
    );

    // The PTY session must agree with the headless model on the observable
    // root composition: a single format-32 submission carrying the green
    // pixels.
    let script = format!(
        "printf '\\033_Ga=T,f=100,i=300,s=2,v=2,q=2;{png_b64}\\033\\\\\
         \\033_Ga=f,i=300,r=2,s=2,v=2,q=2;{green_b64}\\033\\\\\
         \\033_Ga=c,i=300,r=2,c=1,X=0,Y=0,x=0,y=0,w=2,h=2,C=1,q=2\\033\\\\'"
    );
    let mut session = TerminalSession::spawn_with_session_id(
        SessionId::new(300),
        Some("sh"),
        &["-c", &script],
        TerminalSize::new(20, 4),
    )
    .expect("could not spawn non-raw composition fixture");
    let area = Rect::new(0, 0, 20, 4);
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline
        && session
            .graphics(area)
            .first()
            .map(|submission| submission.format())
            != Some(32)
    {
        session
            .poll_output()
            .expect("non-raw composition fixture PTY failed");
        thread::sleep(Duration::from_millis(5));
    }
    let submissions = session.graphics(area);
    assert_eq!(submissions.len(), 1);
    assert_eq!(submissions[0].format(), 32);
    assert_eq!(
        decode_test_base64(submissions[0].encoded_payload()),
        green.to_vec()
    );
    session
        .shutdown()
        .expect("could not shut down non-raw composition fixture");
}

/// Converts raw RGBA bytes into the headless model's pixel type.
fn rgba_pixels(bytes: &[u8]) -> Vec<HeadlessPixel> {
    bytes
        .chunks_exact(4)
        .map(|pixel| HeadlessPixel {
            red: pixel[0],
            green: pixel[1],
            blue: pixel[2],
            alpha: pixel[3],
        })
        .collect()
}

#[test]
fn quiet_key_suppresses_success_responses_like_kitty() {
    // Kitty's `q` rule: `q=0` emits an OK acknowledgement, `q=1` and `q=2`
    // suppress it. The store's return value is exactly what the session writes
    // back to the child terminal.
    let mut store = SessionGraphicsStore::new(SessionId::new(703));
    let ok = store
        .apply_kitty_command_with_context(b"a=T,f=24,i=1,q=0", b"AQID", (0, 0), (0, 0))
        .unwrap()
        .expect("q=0 must emit an OK response");
    assert!(String::from_utf8_lossy(&ok).contains("OK"));

    assert!(
        store
            .apply_kitty_command_with_context(b"a=T,f=24,i=2,q=1", b"BAUG", (0, 0), (0, 0))
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .apply_kitty_command_with_context(b"a=T,f=24,i=3,q=2", b"CAUI", (0, 0), (0, 0))
            .unwrap()
            .is_none()
    );

    // Query responses follow the same rule.
    let query = store
        .apply_kitty_command_with_context(b"a=q,i=1,t=d,s=1,v=1,f=24,q=0", b"MTIz", (0, 0), (0, 0))
        .unwrap()
        .expect("q=0 query must emit a response");
    assert!(String::from_utf8_lossy(&query).contains("OK"));
    assert!(
        store
            .apply_kitty_command_with_context(
                b"a=q,i=1,t=d,s=1,v=1,f=24,q=1",
                b"MTIz",
                (0, 0),
                (0, 0),
            )
            .unwrap()
            .is_none()
    );
}

#[test]
fn headless_model_and_pty_session_agree_queries_do_not_retain_images() {
    // A query loads and validates its payload but never retains the image
    // (Kitty's `remove_images` after a query). A subsequent transmit is the
    // only retained resource in both the headless model and the PTY session.
    let stream =
        b"\x1b_Ga=q,i=1,t=d,s=1,v=1,f=24,q=2;MTIz\x1b\\\x1b_Ga=T,f=24,i=2,c=1,r=1,q=2;AQID\x1b\\";
    let model = HeadlessKittyTerminal::replay(stream).unwrap();
    assert_eq!(model.resource_count(), 1);
    assert_eq!(model.placement_count(), 1);
    assert_eq!(model.actions(), &["query", "transmit"]);

    let script = r"printf '\033_Ga=q,i=1,t=d,s=1,v=1,f=24,q=2;MTIz\033\\\033_Ga=T,f=24,i=2,c=1,r=1,q=2;AQID\033\\'";
    let mut session = TerminalSession::spawn_with_session_id(
        SessionId::new(704),
        Some("sh"),
        &["-c", script],
        TerminalSize::new(20, 4),
    )
    .expect("could not spawn query non-retention fixture");
    let area = Rect::new(0, 0, 20, 4);
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && session.graphics(area).is_empty() {
        session
            .poll_output()
            .expect("query non-retention fixture PTY failed");
        thread::sleep(Duration::from_millis(5));
    }
    // Only the transmit is retained and rendered.
    let submissions = session.graphics(area);
    assert_eq!(submissions.len(), 1);
    assert_eq!(submissions[0].resource().image(), 2);
    session
        .shutdown()
        .expect("could not shut down query non-retention fixture");
}

#[test]
fn headless_model_allocates_and_resolves_image_numbers() {
    // Two numbered transmits allocate two distinct ids; the later a=p with
    // I=7 resolves to the newest (highest id) image.
    let stream = b"\x1b_Ga=t,f=24,I=7,q=2;AQID\x1b\\\x1b_Ga=t,f=24,I=7,q=2;BAUG\x1b\\\x1b_Ga=p,I=7,c=1,r=1,q=2\x1b\\";
    let model = HeadlessKittyTerminal::replay(stream).unwrap();
    assert_eq!(model.resource_count(), 2);
    assert_eq!(model.placement_count(), 1);
    assert_eq!(model.placements()[0].image_id, 2);
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
fn repeated_acknowledged_cleanup_releases_placements_but_retains_data() {
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
    // The backend's tracking is fully cleaned up (graphics_gc == 32), while
    // the outer terminal retains the image data: lowercase `d=i` keeps it so
    // a scrolled-away image re-displays without retransmission (verified
    // against a real Kitty).
    assert_eq!(model.resource_count(), 32);
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
        cell_size: None,
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
    assert_eq!(backend.writer(), b"\x1b[1;1H[image:11]\x1b[?25l");
    let model = HeadlessKittyTerminal::replay(backend.writer()).unwrap();
    assert_eq!(model.text(), "[image:11]");
    assert_eq!(model.resource_count(), 0);
}

#[test]
fn grid_moved_matches_the_mutation_stream_scroll_move() {
    // Workstream 9 parity: the composed grid's per-cell image references and
    // the session's mutation-driven command stream must agree on which
    // placement moved when a scroll relocates an image.
    let surface = Rect::new(0, 0, 8, 6);
    let mut store = SessionGraphicsStore::new(SessionId::new(0));
    store
        .apply_kitty_command_with_context(
            b"a=T,f=24,i=7,c=2,r=1,C=1,q=2",
            b"AQID",
            (0, 2),
            (10, 20),
        )
        .expect("fixture image should transmit");

    let before = store.visible_submissions(surface);
    assert_eq!(before.len(), 1, "one placement is visible");
    let mut compositor = Compositor::new();
    let mut first = Scene::new(surface);
    first.add_image_layer(before[0].clone());
    first.annotate_image_cells();
    compositor.diff(&first);

    store.record_scroll(1);
    let deltas = store.drain_graphics_deltas(
        surface,
        1,
        GraphicsScreen::Primary,
        GraphicsScrollRegion::unbounded(),
        0,
        0,
    );
    assert_eq!(deltas.changed.len(), 1, "the scroll emits one place move");
    assert_eq!(deltas.removed.len(), 0);

    let after = store.visible_submissions_at(surface, 1);
    assert_eq!(after.len(), 1);
    let mut second = Scene::new(surface);
    second.add_image_layer(after[0].clone());
    second.annotate_image_cells();
    let diff = compositor.diff(&second);

    let grid = diff.grid_graphics();
    assert_eq!(grid.moved.len(), 1, "the grid reports the same relocation");
    assert!(grid.appeared.is_empty());
    assert!(grid.removed.is_empty());
    assert_eq!(
        grid.moved[0].placement().key(),
        deltas.changed[0].placement().key(),
        "grid diff and mutation stream agree on the moved placement"
    );
    assert_eq!(grid.moved[0].placement().key(), after[0].placement().key());
}

#[test]
fn canonical_line_is_the_invariant_identity_across_full_screen_scrolls() {
    // Workstream 10 phase 1 parity harness: the signed-row projection (the
    // buffer's `start_row`, mutated by scrolls) and the canonical-line
    // projection (`canonical_line - current_scrollback`) must agree on the
    // placement's viewport position after every scroll, with the canonical
    // line itself invariant — it is the placement's mutation-time identity.
    let mut store = SessionGraphicsStore::new(SessionId::new(0));
    let creation_scrollback = 3;
    store
        .apply_kitty_command_with_grid_context(
            b"a=T,f=24,i=5,c=2,r=1,C=1,q=2",
            b"AQID",
            (0, 2),
            (10, 20),
            creation_scrollback,
        )
        .expect("fixture image should transmit and place");

    let placements: Vec<&ImagePlacement> = store.buffer_placements().collect();
    assert_eq!(
        placements.len(),
        1,
        "one placement from the combined transmit+place"
    );
    let canonical = creation_scrollback as i64 + 2;
    assert_eq!(
        placements[0].canonical_line, canonical,
        "the canonical line is the absolute logical line at creation"
    );
    assert_eq!(placements[0].start_row, 2);

    store.record_scroll(2);
    let placements: Vec<&ImagePlacement> = store.buffer_placements().collect();
    assert_eq!(
        placements[0].canonical_line, canonical,
        "the canonical line is invariant under full-screen scroll"
    );
    assert_eq!(
        placements[0].start_row, 0,
        "the signed row tracks the moving viewport position"
    );
    let current_scrollback = creation_scrollback + 2;
    assert_eq!(
        i64::from(placements[0].start_row),
        placements[0].canonical_line - current_scrollback as i64,
        "parity: signed-row projection == canonical-line projection"
    );
    // Copy the identity out before the drain mutates the store.
    let outer_placement_id = placements[0].outer_placement_id;
    drop(placements);

    // The mutation drain still emits the move under the stable outer
    // placement id — the identity the outer terminal uses to relocate it.
    let deltas = store.drain_graphics_deltas(
        Rect::new(0, 0, 8, 6),
        current_scrollback,
        GraphicsScreen::Primary,
        GraphicsScrollRegion::unbounded(),
        0,
        0,
    );
    assert_eq!(deltas.changed.len(), 1, "the scroll emits one place move");
    assert_eq!(
        deltas.changed[0].placement().outer_placement_id(),
        outer_placement_id,
        "the move reuses the stable outer placement id"
    );
}

/// Scans `scene` for the first cell carrying an image reference and returns
/// its row and that row's logical-line tag.
fn image_row_and_line_tag(scene: &Scene) -> (u16, i64) {
    for y in 0..scene.area().height {
        for x in 0..scene.area().width {
            if scene.image_ref_at(x, y) != 0 {
                return (y, scene.line_tag_at(y));
            }
        }
    }
    panic!("no image reference in scene");
}

#[test]
fn scene_line_tags_agree_with_placement_canonical_lines_across_a_scroll() {
    // Workstream 10 phase 1 e2e: the rendered scene's line tags (absolute
    // logical lines, oldest-history-relative) must be invariant under a
    // full-screen scroll — the same property as the placement's canonical
    // line — so the row displaying the image carries the same tag before and
    // after the scroll, and the image moves up with its text.
    let script = r"printf 'a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n'; printf '\033[4;1H'; printf '\033_Ga=T,f=24,i=77,s=1,v=1,c=1,r=1,C=1,q=2;/wAA\033\\'; printf '\033[8;1H'; cat";
    let mut session = TerminalSession::spawn_with_session_id(
        SessionId::new(77),
        Some("sh"),
        &["-c", script],
        TerminalSize::new(6, 8),
    )
    .expect("could not spawn canonical-line tag fixture");
    let area = Rect::new(0, 0, 6, 8);
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && session.graphics(area).is_empty() {
        session
            .poll_output()
            .expect("canonical-line tag fixture PTY failed");
        thread::sleep(Duration::from_millis(5));
    }
    let mut scene = session.render(area, false);
    for submission in session.graphics(area) {
        scene.add_image_layer(submission);
    }
    scene.annotate_image_cells();
    let (row_before, tag_before) = image_row_and_line_tag(&scene);
    assert_eq!(row_before, 3, "the image was placed at row 3");
    assert_eq!(
        tag_before, 6,
        "canonical = row 3 + 3 lines of history (the 8th line's newline scrolls)"
    );

    // `cat` echoes the pasted newline, and the PTY line discipline (ECHO +
    // ONLCR) doubles it into two linefeeds: the screen scrolls up two rows,
    // carrying the placement (and its tag) up with the text.
    session
        .write_paste("\n")
        .expect("paste one line into the fixture");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        session
            .poll_output()
            .expect("canonical-line tag fixture PTY failed");
        let mut scene = session.render(area, false);
        for submission in session.graphics(area) {
            scene.add_image_layer(submission);
        }
        scene.annotate_image_cells();
        let (row_after, tag_after) = image_row_and_line_tag(&scene);
        if row_after == 1 {
            assert_eq!(
                tag_after, tag_before,
                "the line tag is invariant under the full-screen scroll"
            );
            assert_eq!(
                row_after,
                row_before - 2,
                "the image moved up two rows with its text"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "image never reached row 1 after the scroll"
        );
        thread::sleep(Duration::from_millis(5));
    }
    session
        .shutdown()
        .expect("could not shut down canonical-line tag fixture");
}

#[test]
fn session_captures_the_discovered_outer_cell_size_on_placements() {
    // Workstream 10 phase 2 end-to-end: when the CSI 16t probe delivers the
    // outer terminal's character cell size, placements capture it (instead of
    // the child-side zero size), so occlusion clipping becomes pixel-exact.
    let mut session = TerminalSession::spawn_with_session_id(
        SessionId::new(21),
        Some("sh"),
        &[],
        TerminalSize::new(8, 6),
    )
    .expect("could not spawn outer-cell-size fixture");
    session.set_outer_cell_size(Some((10, 20)));
    session
        .write_paste("printf '\\033_Ga=T,f=24,i=21,c=2,r=1,C=1,q=2;AP8A\\033\\\\'\n")
        .expect("paste the kitty placement script");
    let area = Rect::new(0, 0, 8, 6);
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && session.graphics(area).is_empty() {
        session
            .poll_output()
            .expect("outer-cell-size fixture PTY failed");
        thread::sleep(Duration::from_millis(5));
    }
    let submissions = session.graphics(area);
    assert_eq!(submissions.len(), 1, "one placement reaches the surface");
    assert_eq!(
        submissions[0].placement().cell_width_pixels(),
        10,
        "the placement captures the discovered outer cell width"
    );
    assert_eq!(
        submissions[0].placement().cell_height_pixels(),
        20,
        "the placement captures the discovered outer cell height"
    );
    session
        .shutdown()
        .expect("could not shut down outer-cell-size fixture");
}

#[test]
fn view_scroll_is_a_grid_displacement_tracked_by_both_authorities() {
    // Workstream 9 phase 4: navigating the scrollback view is pure view math
    // on the store, but it *is* a displacement of the displayed grid — and
    // both the mutation stream's projection move and the composed grid's
    // per-cell references must report the same relocation.
    let surface = Rect::new(0, 0, 8, 6);
    let mut store = SessionGraphicsStore::new(SessionId::new(0));
    store
        .apply_kitty_command_with_context(
            b"a=T,f=24,i=7,c=2,r=1,C=1,q=2",
            b"AQID",
            (0, 2),
            (10, 20),
        )
        .expect("fixture image should transmit");

    let before = store.visible_submissions(surface);
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].placement().y(), 2);
    let mut compositor = Compositor::new();
    let mut first = Scene::new(surface);
    first.add_image_layer(before[0].clone());
    first.annotate_image_cells();
    compositor.diff(&first);

    // Two rows scroll off the top: the placement moves up with the text, and
    // the grid diff reports the same relocation as the mutation move.
    store.record_scroll(2);
    let scrolled = store.drain_graphics_deltas(
        surface,
        2,
        GraphicsScreen::Primary,
        GraphicsScrollRegion::unbounded(),
        0,
        0,
    );
    assert_eq!(scrolled.changed.len(), 1, "the scroll emits one move");
    assert_eq!(
        scrolled.changed[0].placement().y(),
        0,
        "scrolled to the top row"
    );
    let scrolled_visible = store.visible_submissions_at(surface, 2);
    assert_eq!(scrolled_visible[0].placement().y(), 0);
    let mut scrolled_scene = Scene::new(surface);
    scrolled_scene.add_image_layer(scrolled_visible[0].clone());
    scrolled_scene.annotate_image_cells();
    let scrolled_diff = compositor.diff(&scrolled_scene);
    assert_eq!(
        scrolled_diff.grid_graphics().moved.len(),
        1,
        "the grid sees the scroll"
    );
    assert_eq!(
        scrolled_diff.grid_graphics().moved[0].placement().key(),
        scrolled.changed[0].placement().key(),
        "grid diff and mutation move agree on the scroll"
    );

    // The user scrolls the view back two rows: the placement returns to its
    // original grid row (pure view math on the store), and the drain emits
    // the projection move while the grid reports the same relocation.
    let navigated = store.drain_graphics_deltas(
        surface,
        2,
        GraphicsScreen::Primary,
        GraphicsScrollRegion::unbounded(),
        0,
        2,
    );
    assert_eq!(
        navigated.changed.len(),
        1,
        "view navigation emits one projection move"
    );
    assert_eq!(
        navigated.changed[0].placement().y(),
        2,
        "back at the original row"
    );

    let after = store.visible_submissions_with_scroll_state(
        surface,
        2,
        GraphicsScreen::Primary,
        GraphicsScrollRegion::unbounded(),
        0,
        2,
    );
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].placement().y(), 2);
    let mut second = Scene::new(surface);
    second.add_image_layer(after[0].clone());
    second.annotate_image_cells();
    let diff = compositor.diff(&second);

    let grid = diff.grid_graphics();
    assert_eq!(grid.moved.len(), 1, "the grid sees the view displacement");
    assert!(grid.appeared.is_empty());
    assert!(grid.removed.is_empty());
    assert_eq!(
        grid.moved[0].placement().key(),
        navigated.changed[0].placement().key(),
        "grid diff and projection move agree under view navigation"
    );
}
