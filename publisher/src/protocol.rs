//! The publisher side of the relay's WebSocket protocol (relay spec §6.1, §6.3).
//!
//! Frame layouts are asymmetric and easy to confuse: a publisher frame carries the
//! terminal UUID, because one connection multiplexes many terminals, while a mirror
//! frame does not. `tests/protocol.rs` checks these encoders against the relay's own.

use serde::Deserialize;
use uuid::Uuid;

pub const SUBPROTOCOL_V2: &str = "terminal-relay.publisher.v2";

pub const FRAME_TYPE_OUTPUT: u8 = 0x01;
pub const FRAME_TYPE_INPUT: u8 = 0x02;

/// `0x01 | terminal UUID (16) | expected start offset (u64 BE) | payload`.
pub const PUBLISHER_HEADER_LEN: usize = 1 + 16 + 8;
/// `0x02 | terminal UUID (16) | relay sequence (u64 BE) | payload`.
pub const PUBLISHER_INPUT_HEADER_LEN: usize = 1 + 16 + 8;

pub fn encode_output_frame(terminal_id: Uuid, expected_offset: u64, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(PUBLISHER_HEADER_LEN + payload.len());
    out.push(FRAME_TYPE_OUTPUT);
    out.extend_from_slice(terminal_id.as_bytes());
    out.extend_from_slice(&expected_offset.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

#[derive(Debug, PartialEq, Eq)]
pub struct InputFrame {
    pub terminal_id: Uuid,
    /// Per-terminal relay sequence. Lets a publisher notice loss across a reconnect;
    /// it is not durable and resets when the terminal is re-opened (spec §6.3).
    pub relay_sequence: u64,
    pub payload: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum FrameError {
    TooShort,
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

pub fn decode_input_frame(bytes: &[u8]) -> Result<InputFrame, FrameError> {
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
    Ok(InputFrame {
        terminal_id: Uuid::from_bytes(uuid_bytes),
        relay_sequence: u64::from_be_bytes(sequence),
        payload: bytes[PUBLISHER_INPUT_HEADER_LEN..].to_vec(),
    })
}

// ------------------------------------------------------------- control messages

#[derive(Debug, Clone, Deserialize)]
pub struct Limits {
    pub max_output_frame_bytes: u64,
    pub max_unacked_output_bytes: u64,
    #[serde(default)]
    pub max_active_terminals: u64,
    #[serde(default)]
    pub heartbeat_interval_seconds: u64,
    #[serde(default)]
    pub heartbeat_timeout_seconds: u64,
    #[serde(default)]
    pub max_input_frame_bytes: Option<u64>,
}

/// Messages the relay sends a publisher.
///
/// Unknown types are ignored rather than rejected: the relay is allowed to add
/// messages, and a publisher that fell over on an unrecognised one would break on
/// every server upgrade (spec §12).
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum ServerMessage {
    #[serde(rename = "ready")]
    Ready {
        connection_id: String,
        limits: Limits,
    },
    #[serde(rename = "terminal.opened")]
    TerminalOpened {
        request_id: String,
        terminal_id: Uuid,
        next_offset: u64,
        durable_offset: u64,
        #[serde(default)]
        accepts_input: bool,
    },
    #[serde(rename = "output.ack")]
    OutputAck {
        terminal_id: Uuid,
        durable_offset: u64,
        next_offset: u64,
    },
    /// A subscriber asked this machine to open a terminal (relay spec §4.6).
    ///
    /// Carries no command, environment or working directory — the machine decides all
    /// of that — and is refused outright unless its owner turned the capability on.
    #[serde(rename = "terminal.open_request")]
    TerminalOpenRequest {
        request_id: String,
        #[serde(default)]
        label: Option<String>,
        #[serde(default)]
        cols: Option<u32>,
        #[serde(default)]
        rows: Option<u32>,
    },
    /// A subscriber asked for a size. The publisher owns the terminal and decides
    /// (spec §6.5): it may apply the size, or ignore the request.
    #[serde(rename = "terminal.resize_request")]
    TerminalResizeRequest {
        terminal_id: Uuid,
        cols: u32,
        rows: u32,
    },
    #[serde(rename = "terminal.closed")]
    TerminalClosed { terminal_id: Uuid, reason: String },
    #[serde(rename = "notice")]
    Notice { code: String, message: String },
    #[serde(rename = "error")]
    Error {
        code: String,
        message: String,
        #[serde(default)]
        terminal_id: Option<Uuid>,
        #[serde(default)]
        request_id: Option<String>,
        /// Present on `offset_mismatch`: the authority on where to resume.
        #[serde(default)]
        next_offset: Option<u64>,
        #[serde(default)]
        durable_offset: Option<u64>,
    },
    #[serde(rename = "ping")]
    Ping { at_unix_ms: i64 },
    #[serde(other)]
    Unrecognised,
}

pub fn parse_server_message(text: &str) -> Option<ServerMessage> {
    serde_json::from_str(text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_output_frame_has_the_publisher_header() {
        let id = Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef);
        let frame = encode_output_frame(id, 1234, b"hi");
        assert_eq!(frame[0], FRAME_TYPE_OUTPUT);
        assert_eq!(&frame[1..17], id.as_bytes());
        assert_eq!(u64::from_be_bytes(frame[17..25].try_into().unwrap()), 1234);
        assert_eq!(&frame[25..], b"hi");
    }

    #[test]
    fn an_input_frame_round_trips() {
        let id = Uuid::from_u128(9);
        let mut bytes = vec![FRAME_TYPE_INPUT];
        bytes.extend_from_slice(id.as_bytes());
        bytes.extend_from_slice(&7u64.to_be_bytes());
        bytes.extend_from_slice(b"ls\r");
        let frame = decode_input_frame(&bytes).expect("decodes");
        assert_eq!(frame.terminal_id, id);
        assert_eq!(frame.relay_sequence, 7);
        assert_eq!(frame.payload, b"ls\r");
    }

    #[test]
    fn a_mirror_shaped_input_frame_is_refused() {
        // The mirror header is 9 bytes, the publisher's is 25. Accepting the shorter
        // one would read the payload as a UUID and mis-route every keystroke.
        let mut bytes = vec![FRAME_TYPE_INPUT];
        bytes.extend_from_slice(&1u64.to_be_bytes());
        bytes.extend_from_slice(b"x");
        assert_eq!(decode_input_frame(&bytes), Err(FrameError::TooShort));
    }

    #[test]
    fn an_output_frame_is_not_mistaken_for_input() {
        let mut bytes = vec![FRAME_TYPE_OUTPUT];
        bytes.extend_from_slice(&[0u8; 24]);
        assert_eq!(
            decode_input_frame(&bytes),
            Err(FrameError::UnknownType(FRAME_TYPE_OUTPUT))
        );
    }

    #[test]
    fn an_unknown_control_message_is_ignored_not_fatal() {
        let parsed = parse_server_message(r#"{"type":"terminal.teleport","x":1}"#);
        assert!(matches!(parsed, Some(ServerMessage::Unrecognised)));
    }

    #[test]
    fn an_offset_mismatch_carries_the_authoritative_offsets() {
        let parsed = parse_server_message(
            r#"{"type":"error","code":"offset_mismatch","message":"no",
                "next_offset":4096,"durable_offset":2048}"#,
        )
        .expect("parses");
        match parsed {
            ServerMessage::Error {
                code,
                next_offset,
                durable_offset,
                ..
            } => {
                assert_eq!(code, "offset_mismatch");
                assert_eq!(next_offset, Some(4096));
                assert_eq!(durable_offset, Some(2048));
            }
            other => panic!("expected an error message, got {other:?}"),
        }
    }
}
