#[path = "support/headless_kitty.rs"]
mod headless_kitty;

use cmdash::{
    Backend, BackendCapabilities, CrosstermBackend, GraphicsCapabilityConfidence,
    GraphicsCapabilitySource, GraphicsSubmission, GraphicsSubmissionStatus, SessionGraphicsStore,
    SessionId,
};
use headless_kitty::HeadlessKittyTerminal;
use ratatui::layout::Rect;

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

fn assert_rendered(status: GraphicsSubmissionStatus, resources: usize, placements: usize) {
    assert_eq!(
        status,
        GraphicsSubmissionStatus::Rendered {
            resources,
            placements,
        }
    );
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
