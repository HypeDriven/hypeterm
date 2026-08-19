//! Binary frame layouts (spec §6.1, §6.2).
//!
//! The two directions differ, which is easy to get wrong: a publisher frame carries
//! the terminal UUID because one connection multiplexes many terminals, while a
//! mirror frame does not because the subscription is already bound to one terminal.
//!
//! ```text
//! publisher -> server            server -> mirror
//! byte  0     0x01               byte 0     0x01
//! bytes 1-16  terminal UUID      bytes 1-8  start offset (u64 BE)
//! bytes 17-24 expected offset    bytes 9..  payload
//! bytes 25..  payload
//! ```

use bytes::Bytes;
use uuid::Uuid;

pub const FRAME_TYPE_OUTPUT: u8 = 0x01;
/// Terminal input, subprotocol version 2 only (spec §6.3).
pub const FRAME_TYPE_INPUT: u8 = 0x02;

pub const PUBLISHER_HEADER_LEN: usize = 1 + 16 + 8;
pub const MIRROR_HEADER_LEN: usize = 1 + 8;
/// Input frames use the same header shapes as output in each direction: the
/// subscriber sends type + sequence, and the relay adds the terminal UUID on the way
/// to the publisher, which multiplexes many terminals over one connection.
pub const MIRROR_INPUT_HEADER_LEN: usize = 1 + 8;
pub const PUBLISHER_INPUT_HEADER_LEN: usize = 1 + 16 + 8;

#[derive(Debug, PartialEq)]
pub struct PublisherOutputFrame {
    pub terminal_id: Uuid,
    /// The offset the publisher believes is next for this terminal.
    pub expected_offset: u64,
    pub payload: Bytes,
}

#[derive(Debug, PartialEq, Eq)]
pub enum FrameError {
    /// Shorter than the fixed header.
    TooShort,
    /// A frame type this protocol version does not define.
    UnknownType(u8),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::TooShort => write!(f, "binary frame is shorter than its header"),
            FrameError::UnknownType(t) => write!(f, "unknown binary frame type 0x{t:02x}"),
        }
    }
}

pub fn decode_publisher_frame(bytes: &Bytes) -> Result<PublisherOutputFrame, FrameError> {
    if bytes.len() < PUBLISHER_HEADER_LEN {
        return Err(FrameError::TooShort);
    }
    if bytes[0] != FRAME_TYPE_OUTPUT {
        return Err(FrameError::UnknownType(bytes[0]));
    }
    let mut uuid_bytes = [0u8; 16];
    uuid_bytes.copy_from_slice(&bytes[1..17]);
    let mut offset_bytes = [0u8; 8];
    offset_bytes.copy_from_slice(&bytes[17..25]);

    Ok(PublisherOutputFrame {
        terminal_id: Uuid::from_bytes(uuid_bytes),
        expected_offset: u64::from_be_bytes(offset_bytes),
        // slice_ref keeps the payload as a view into the original buffer, so
        // relaying to subscribers never copies the terminal output.
        payload: bytes.slice(PUBLISHER_HEADER_LEN..),
    })
}

pub fn encode_publisher_frame(terminal_id: Uuid, expected_offset: u64, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(PUBLISHER_HEADER_LEN + payload.len());
    out.push(FRAME_TYPE_OUTPUT);
    out.extend_from_slice(terminal_id.as_bytes());
    out.extend_from_slice(&expected_offset.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// Subscriber input, mirror -> server (spec §6.3).
#[derive(Debug, PartialEq)]
pub struct MirrorInputFrame {
    /// Per-connection sequence, starting at 1 and increasing by exactly one.
    pub client_sequence: u64,
    pub payload: Bytes,
}

pub fn decode_mirror_input_frame(bytes: &Bytes) -> Result<MirrorInputFrame, FrameError> {
    if bytes.len() < MIRROR_INPUT_HEADER_LEN {
        return Err(FrameError::TooShort);
    }
    if bytes[0] != FRAME_TYPE_INPUT {
        return Err(FrameError::UnknownType(bytes[0]));
    }
    let mut sequence = [0u8; 8];
    sequence.copy_from_slice(&bytes[1..9]);
    Ok(MirrorInputFrame {
        client_sequence: u64::from_be_bytes(sequence),
        payload: bytes.slice(MIRROR_INPUT_HEADER_LEN..),
    })
}

/// Used by clients and by tests to build an input frame.
pub fn encode_mirror_input_frame(client_sequence: u64, payload: &[u8]) -> Bytes {
    let mut out = Vec::with_capacity(MIRROR_INPUT_HEADER_LEN + payload.len());
    out.push(FRAME_TYPE_INPUT);
    out.extend_from_slice(&client_sequence.to_be_bytes());
    out.extend_from_slice(payload);
    Bytes::from(out)
}

/// Input as delivered to the publisher, server -> publisher (spec §6.3).
pub fn encode_publisher_input_frame(
    terminal_id: Uuid,
    relay_sequence: u64,
    payload: &[u8],
) -> Bytes {
    let mut out = Vec::with_capacity(PUBLISHER_INPUT_HEADER_LEN + payload.len());
    out.push(FRAME_TYPE_INPUT);
    out.extend_from_slice(terminal_id.as_bytes());
    out.extend_from_slice(&relay_sequence.to_be_bytes());
    out.extend_from_slice(payload);
    Bytes::from(out)
}

#[derive(Debug, PartialEq)]
pub struct PublisherInputFrame {
    pub terminal_id: Uuid,
    pub relay_sequence: u64,
    pub payload: Bytes,
}

/// Used by publisher implementations and tests.
pub fn decode_publisher_input_frame(bytes: &Bytes) -> Result<PublisherInputFrame, FrameError> {
    if bytes.len() < PUBLISHER_INPUT_HEADER_LEN {
        return Err(FrameError::TooShort);
    }
    if bytes[0] != FRAME_TYPE_INPUT {
        return Err(FrameError::UnknownType(bytes[0]));
    }
    let mut uuid_bytes = [0u8; 16];
    uuid_bytes.copy_from_slice(&bytes[1..17]);
    let mut sequence = [0u8; 8];
    sequence.copy_from_slice(&bytes[17..25]);
    Ok(PublisherInputFrame {
        terminal_id: Uuid::from_bytes(uuid_bytes),
        relay_sequence: u64::from_be_bytes(sequence),
        payload: bytes.slice(PUBLISHER_INPUT_HEADER_LEN..),
    })
}

pub fn encode_mirror_frame(start_offset: u64, payload: &[u8]) -> Bytes {
    let mut out = Vec::with_capacity(MIRROR_HEADER_LEN + payload.len());
    out.push(FRAME_TYPE_OUTPUT);
    out.extend_from_slice(&start_offset.to_be_bytes());
    out.extend_from_slice(payload);
    Bytes::from(out)
}

#[derive(Debug, PartialEq)]
pub struct MirrorOutputFrame {
    pub start_offset: u64,
    pub payload: Bytes,
}

/// Used by tests and by any client implementation built on this crate.
pub fn decode_mirror_frame(bytes: &Bytes) -> Result<MirrorOutputFrame, FrameError> {
    if bytes.len() < MIRROR_HEADER_LEN {
        return Err(FrameError::TooShort);
    }
    if bytes[0] != FRAME_TYPE_OUTPUT {
        return Err(FrameError::UnknownType(bytes[0]));
    }
    let mut offset_bytes = [0u8; 8];
    offset_bytes.copy_from_slice(&bytes[1..9]);
    Ok(MirrorOutputFrame {
        start_offset: u64::from_be_bytes(offset_bytes),
        payload: bytes.slice(MIRROR_HEADER_LEN..),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publisher_frame_round_trip() {
        let id = Uuid::new_v4();
        let encoded = encode_publisher_frame(id, 48_120, b"\x1b[31mred\x00\xff");
        let decoded = decode_publisher_frame(&Bytes::from(encoded)).unwrap();
        assert_eq!(decoded.terminal_id, id);
        assert_eq!(decoded.expected_offset, 48_120);
        // Invalid UTF-8 and control bytes survive untouched.
        assert_eq!(decoded.payload.as_ref(), b"\x1b[31mred\x00\xff");
    }

    #[test]
    fn mirror_frame_round_trip() {
        let encoded = encode_mirror_frame(u64::MAX - 1, b"\xff\xfe");
        let decoded = decode_mirror_frame(&encoded).unwrap();
        assert_eq!(decoded.start_offset, u64::MAX - 1);
        assert_eq!(decoded.payload.as_ref(), b"\xff\xfe");
    }

    #[test]
    fn header_offsets_match_the_specification() {
        let id = Uuid::from_bytes([7u8; 16]);
        let frame = encode_publisher_frame(id, 1, b"p");
        assert_eq!(frame[0], 0x01);
        assert_eq!(&frame[1..17], &[7u8; 16]);
        assert_eq!(&frame[17..25], &1u64.to_be_bytes());
        assert_eq!(&frame[25..], b"p");

        let mirror = encode_mirror_frame(2, b"q");
        assert_eq!(mirror[0], 0x01);
        assert_eq!(&mirror[1..9], &2u64.to_be_bytes());
        assert_eq!(&mirror[9..], b"q");
    }

    #[test]
    fn zero_length_publisher_payload_decodes_as_empty() {
        let encoded = encode_publisher_frame(Uuid::nil(), 0, b"");
        let decoded = decode_publisher_frame(&Bytes::from(encoded)).unwrap();
        assert!(decoded.payload.is_empty());
    }

    #[test]
    fn input_frames_round_trip_in_both_directions() {
        let encoded = encode_mirror_input_frame(1, b"\x03ls -l\r");
        let decoded = decode_mirror_input_frame(&encoded).unwrap();
        assert_eq!(decoded.client_sequence, 1);
        assert_eq!(decoded.payload.as_ref(), b"\x03ls -l\r");

        let id = Uuid::new_v4();
        let delivered = encode_publisher_input_frame(id, 913, b"\x1b[A");
        let parsed = decode_publisher_input_frame(&delivered).unwrap();
        assert_eq!(parsed.terminal_id, id);
        assert_eq!(parsed.relay_sequence, 913);
        assert_eq!(parsed.payload.as_ref(), b"\x1b[A");
    }

    #[test]
    fn input_and_output_frame_types_are_distinct() {
        // A version 1 peer that receives an input frame must be able to tell it is not
        // output, rather than appending control bytes to the stream.
        let input = encode_mirror_input_frame(1, b"x");
        assert_eq!(input[0], 0x02);
        assert_eq!(encode_mirror_frame(0, b"x")[0], 0x01);
        assert_eq!(
            decode_mirror_frame(&input),
            Err(FrameError::UnknownType(0x02))
        );
    }

    #[test]
    fn rejects_short_and_unknown_frames() {
        assert_eq!(
            decode_publisher_frame(&Bytes::from_static(b"\x01")),
            Err(FrameError::TooShort)
        );
        let mut bad = vec![0x02];
        bad.extend_from_slice(&[0u8; 24]);
        assert_eq!(
            decode_publisher_frame(&Bytes::from(bad)),
            Err(FrameError::UnknownType(0x02))
        );
        assert_eq!(
            decode_mirror_frame(&Bytes::from_static(b"\x01\x00")),
            Err(FrameError::TooShort)
        );
    }
}
