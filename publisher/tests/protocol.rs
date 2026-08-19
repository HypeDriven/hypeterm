//! Frame encodings checked against the relay's own, not against a second reading of
//! the specification.
//!
//! The two directions are asymmetric — a publisher frame carries the terminal UUID, a
//! mirror frame does not — and getting that wrong produces a stream that decodes into
//! plausible nonsense rather than an error. The relay crate is a dev-dependency for
//! exactly this: the shipped binary never links it, and these tests compare byte for
//! byte with the implementation that will be on the other end of the socket.

use bytes::Bytes;
use hypeterm_publish::protocol;
use terminal_relay::relay::frames;
use uuid::Uuid;

#[test]
fn output_frames_match_the_relays_encoder() {
    let terminal_id = Uuid::from_u128(0x0123_4567_89ab_cdef_fedc_ba98_7654_3210);
    for (offset, payload) in [
        (0u64, &b""[..]),
        (1, &b"x"[..]),
        (4096, &b"hello, terminal\r\n"[..]),
        // Past 2^32, where a 32-bit slip in either implementation would show.
        (0x1_0000_0000, &b"large offset"[..]),
        (u64::MAX, &b"the very end"[..]),
    ] {
        let ours = protocol::encode_output_frame(terminal_id, offset, payload);
        let theirs = frames::encode_publisher_frame(terminal_id, offset, payload);
        assert_eq!(ours, theirs, "offset {offset}");

        // And the relay decodes what we produced, which is the direction that matters.
        let decoded = frames::decode_publisher_frame(&Bytes::from(ours)).expect("decodes");
        assert_eq!(decoded.terminal_id, terminal_id);
        assert_eq!(decoded.expected_offset, offset);
        assert_eq!(&decoded.payload[..], payload);
    }
}

#[test]
fn input_frames_from_the_relay_decode_here() {
    let terminal_id = Uuid::from_u128(7);
    for (sequence, payload) in [
        (1u64, &b"a"[..]),
        (2, &b"\x1b[A"[..]),
        (0x1_0000_0000, &b"ls -la\r"[..]),
    ] {
        let encoded = frames::encode_publisher_input_frame(terminal_id, sequence, payload);
        let decoded = protocol::decode_input_frame(&encoded).expect("decodes");
        assert_eq!(decoded.terminal_id, terminal_id);
        assert_eq!(decoded.relay_sequence, sequence);
        assert_eq!(decoded.payload, payload);
    }
}

#[test]
fn a_mirror_frame_is_not_accepted_as_a_publisher_frame() {
    // The mirror direction has a 9-byte header and no UUID. Accepting one here would
    // read the payload's first sixteen bytes as a terminal id and misroute every
    // keystroke to a terminal that does not exist.
    let mirror = frames::encode_mirror_input_frame(1, b"this is definitely long enough");
    let decoded = protocol::decode_input_frame(&mirror);
    match decoded {
        Ok(frame) => assert_ne!(
            frame.payload, b"this is definitely long enough",
            "a mirror frame must not decode into a sensible publisher frame"
        ),
        Err(_) => {}
    }

    // The header lengths differ, which is the property that makes the two
    // distinguishable at all.
    assert_ne!(
        frames::MIRROR_INPUT_HEADER_LEN,
        protocol::PUBLISHER_INPUT_HEADER_LEN
    );
    assert_eq!(
        frames::PUBLISHER_INPUT_HEADER_LEN,
        protocol::PUBLISHER_INPUT_HEADER_LEN
    );
    assert_eq!(frames::PUBLISHER_HEADER_LEN, protocol::PUBLISHER_HEADER_LEN);
}

#[test]
fn frame_type_bytes_agree() {
    assert_eq!(frames::FRAME_TYPE_OUTPUT, protocol::FRAME_TYPE_OUTPUT);
    assert_eq!(frames::FRAME_TYPE_INPUT, protocol::FRAME_TYPE_INPUT);
}

// --------------------------------------- phone-initiated terminals (relay spec §4.6)

#[test]
fn a_terminal_open_request_is_understood_and_carries_no_command() {
    let parsed = protocol::parse_server_message(
        r#"{"type":"terminal.open_request","request_id":"abc","label":"phone","cols":120,"rows":40}"#,
    );
    match parsed {
        Some(protocol::ServerMessage::TerminalOpenRequest {
            request_id,
            label,
            cols,
            rows,
        }) => {
            assert_eq!(request_id, "abc");
            assert_eq!(label.as_deref(), Some("phone"));
            assert_eq!(cols, Some(120));
            assert_eq!(rows, Some(40));
        }
        other => panic!("expected a terminal.open_request, got {other:?}"),
    }
}

#[test]
fn an_open_request_that_smuggles_a_command_still_carries_no_command_here() {
    // The type has nowhere to put one, which is the point: even a relay that decided to
    // send a command could not hand this process one to run. The shape of the message
    // is itself the defence, not a check somebody has to remember to write.
    let parsed = protocol::parse_server_message(
        r#"{"type":"terminal.open_request","request_id":"abc","command":"rm -rf /"}"#,
    );
    match parsed {
        Some(protocol::ServerMessage::TerminalOpenRequest { request_id, .. }) => {
            assert_eq!(request_id, "abc");
        }
        other => panic!("expected a terminal.open_request, got {other:?}"),
    }
}

#[test]
fn a_genuinely_unknown_message_is_still_ignored() {
    // Older builds must keep working against a newer relay, so an unrecognised type is
    // not an error. This is the property that lets the capability be a message rather
    // than a subprotocol bump.
    let parsed = protocol::parse_server_message(r#"{"type":"terminal.teleport"}"#);
    assert!(matches!(
        parsed,
        Some(protocol::ServerMessage::Unrecognised) | None
    ));
}
