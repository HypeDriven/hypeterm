//! JSON control messages carried in text frames (spec §6).

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const SUBPROTOCOL_PUBLISHER_V1: &str = "terminal-relay.publisher.v1";
pub const SUBPROTOCOL_PUBLISHER_V2: &str = "terminal-relay.publisher.v2";
pub const SUBPROTOCOL_MIRROR_V1: &str = "terminal-relay.mirror.v1";
pub const SUBPROTOCOL_MIRROR_V2: &str = "terminal-relay.mirror.v2";

/// Negotiated protocol version. Version 2 adds terminal input (spec §6); a version 1
/// peer must observe no behavioural change from its presence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ProtocolVersion {
    V1,
    V2,
}

impl ProtocolVersion {
    /// Highest version offered by the client, or `None` if it offered neither.
    pub fn negotiate(offered: &str, v1: &str, v2: &str) -> Option<Self> {
        let mut best = None;
        for candidate in offered.split(',') {
            let candidate = candidate.trim();
            if candidate == v2 {
                return Some(ProtocolVersion::V2);
            }
            if candidate == v1 {
                best = Some(ProtocolVersion::V1);
            }
        }
        best
    }

    pub fn publisher_subprotocol(&self) -> &'static str {
        match self {
            ProtocolVersion::V1 => SUBPROTOCOL_PUBLISHER_V1,
            ProtocolVersion::V2 => SUBPROTOCOL_PUBLISHER_V2,
        }
    }

    pub fn mirror_subprotocol(&self) -> &'static str {
        match self {
            ProtocolVersion::V1 => SUBPROTOCOL_MIRROR_V1,
            ProtocolVersion::V2 => SUBPROTOCOL_MIRROR_V2,
        }
    }

    pub fn supports_input(&self) -> bool {
        *self >= ProtocolVersion::V2
    }
}

/// Application close codes, in the private range the specification reserves for
/// implementation-defined conditions (§6). Protocol-level violations use 1002.
pub mod close {
    pub const PROTOCOL_ERROR: u16 = 1002;
    pub const NORMAL: u16 = 1000;
    pub const GOING_AWAY: u16 = 1001;

    pub const UNAUTHORIZED: u16 = 4001;
    pub const SUPERSEDED: u16 = 4002;
    pub const SLOW_CONSUMER: u16 = 4003;
    pub const STORAGE_UNAVAILABLE: u16 = 4004;
    pub const OFFSET_AHEAD: u16 = 4005;
    pub const REVOKED: u16 = 4006;
    pub const SERVER_SHUTDOWN: u16 = 4007;
    pub const LIMIT_EXCEEDED: u16 = 4008;
    pub const HEARTBEAT_TIMEOUT: u16 = 4009;
    pub const NOT_FOUND: u16 = 4011;
    pub const RATE_LIMITED: u16 = 4012;
    pub const FEATURE_DISABLED: u16 = 4013;
    pub const HANDSHAKE_TIMEOUT: u16 = 4014;
    pub const TERMINAL_CLOSED: u16 = 4015;
}

/// Error codes carried in `error` control messages.
pub mod error_code {
    pub const OFFSET_MISMATCH: &str = "offset_mismatch";
    pub const OFFSET_AHEAD: &str = "offset_ahead";
    pub const STORAGE_UNAVAILABLE: &str = "storage_unavailable";
    pub const TERMINAL_NOT_FOUND: &str = "terminal_not_found";
    pub const TERMINAL_CLOSED: &str = "terminal_closed";
    pub const LIMIT_EXCEEDED: &str = "limit_exceeded";
    pub const INVALID_MESSAGE: &str = "invalid_message";
    pub const UNKNOWN_MESSAGE_TYPE: &str = "unknown_message_type";
    pub const VALIDATION_FAILED: &str = "validation_failed";
    pub const SUPERSEDED: &str = "superseded";
    pub const SLOW_CONSUMER: &str = "slow_consumer";
    pub const REVOKED: &str = "revoked";
    pub const SERVER_SHUTDOWN: &str = "server_shutdown";
    pub const FEATURE_DISABLED: &str = "feature_disabled";
    pub const ALREADY_SUBSCRIBED: &str = "already_subscribed";
    pub const HANDSHAKE_TIMEOUT: &str = "handshake_timeout";
    // Input refusals (spec §6.3). The first two are transient; the rest are not.
    pub const INPUT_UNDELIVERABLE: &str = "input_undeliverable";
    pub const INPUT_BACKPRESSURE: &str = "input_backpressure";
    pub const INPUT_NOT_ACCEPTED: &str = "input_not_accepted";
    pub const INPUT_FORBIDDEN: &str = "input_forbidden";
    pub const INPUT_DISABLED: &str = "input_disabled";
    pub const INPUT_SEQUENCE_MISMATCH: &str = "input_sequence_mismatch";
    pub const RESIZE_REFUSED: &str = "resize_refused";
    pub const RATE_LIMITED: &str = "rate_limited";
}

// ------------------------------------------------------------- publisher inbound

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum PublisherMessage {
    #[serde(rename = "terminal.open")]
    Open {
        request_id: String,
        local_ref: String,
        #[serde(default)]
        label: Option<String>,
        #[serde(default)]
        cols: Option<u32>,
        #[serde(default)]
        rows: Option<u32>,
        #[serde(default)]
        term: Option<String>,
        #[serde(default)]
        process_label: Option<String>,
        /// Opt-in to receiving terminal input (spec §4.5). Version 2 publishers only.
        #[serde(default)]
        accepts_input: Option<bool>,
        /// Echoes the `request_id` of a `terminal.open_request` this open answers
        /// (spec §4.6). Absent for a terminal the machine's own owner started.
        #[serde(default)]
        in_reply_to: Option<String>,
    },
    #[serde(rename = "terminal.resize")]
    Resize {
        terminal_id: Uuid,
        cols: u32,
        rows: u32,
    },
    #[serde(rename = "terminal.close")]
    Close {
        terminal_id: Uuid,
        #[serde(default)]
        reason: Option<String>,
    },
    /// Refuses a `terminal.open_request` (spec §4.6).
    ///
    /// A decline is a normal answer, not a protocol fault: the machine's own policy is
    /// the final word on whether it spawns anything.
    #[serde(rename = "terminal.open_declined")]
    OpenDeclined {
        in_reply_to: String,
        reason: String,
        /// Operator-facing only. Never forwarded to the requester, which would let a
        /// publisher write into a phone's UI (spec §4.6).
        #[serde(default)]
        detail: Option<String>,
    },
    /// What this connection is willing to be asked to do (spec §4.6).
    ///
    /// Connection-scoped and re-sendable: it is an assertion about the machine's
    /// current policy, not a property of the build, so it is deliberately not a
    /// subprotocol bump. A relay that predates it classifies the message as ignorable
    /// and simply never asks.
    #[serde(rename = "publisher.capabilities")]
    Capabilities {
        /// Whether this machine's owner has allowed subscribers to ask it to open a
        /// terminal. Absent means no: the capability is never inferred.
        #[serde(default)]
        terminal_open_requests: bool,
    },
    /// Responds to a server ping at the application layer. Optional: the transport
    /// heartbeat is the authoritative liveness check.
    #[serde(rename = "pong")]
    Pong,
}

// ---------------------------------------------------------------- mirror inbound

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum MirrorMessage {
    #[serde(rename = "subscribe")]
    Subscribe {
        /// Offset of the next byte the client wants. Omitted requests the whole
        /// retained replay window.
        #[serde(default)]
        from_offset: Option<u64>,
    },
    /// A subscriber with input authority asking the publisher to resize (spec §6.3).
    #[serde(rename = "terminal.resize_request")]
    ResizeRequest { cols: u32, rows: u32 },
    #[serde(rename = "pong")]
    Pong,
}

/// Outcome of classifying an inbound control frame.
pub enum Inbound<T> {
    Message(T),
    /// A type this version does not define, explicitly marked ignorable by the
    /// sender. Version 1 may gain new ignorable control messages (spec §12), so
    /// these are logged and skipped rather than closing the connection.
    Ignorable(String),
    /// An unknown type that was not marked ignorable, or a malformed body.
    Rejected(String),
}

/// Classify a text frame.
///
/// A peer may mark a new message type as ignorable by including `"optional": true`.
/// Anything else unknown is treated as required and fails the connection, so
/// security-relevant behaviour never depends on a silent skip.
pub fn classify<T: serde::de::DeserializeOwned>(text: &str) -> Inbound<T> {
    let value: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => return Inbound::Rejected(format!("control message is not valid JSON: {e}")),
    };
    if !value.is_object() {
        return Inbound::Rejected("control message must be a JSON object".to_string());
    }
    let Some(kind) = value.get("type").and_then(|v| v.as_str()) else {
        return Inbound::Rejected("control message is missing a type".to_string());
    };
    let kind = kind.to_string();

    match serde_json::from_value::<T>(value.clone()) {
        Ok(message) => Inbound::Message(message),
        Err(e) => {
            let optional = value
                .get("optional")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if optional {
                Inbound::Ignorable(kind)
            } else {
                Inbound::Rejected(format!("{kind}: {e}"))
            }
        }
    }
}

// --------------------------------------------------------------- server outbound

/// Protocol limits advertised at connect time. A connection keeps these values
/// until it reconnects, except that later reductions still apply (see
/// `Reload::ConnectionRenegotiate`).
#[derive(Debug, Clone, Serialize)]
pub struct PublisherLimits {
    pub max_output_frame_bytes: u64,
    pub max_unacked_output_bytes: u64,
    pub max_control_message_bytes: u64,
    pub max_active_terminals: u64,
    pub replay_capacity_bytes: u64,
    pub heartbeat_interval_seconds: u64,
    pub heartbeat_timeout_seconds: u64,
    /// Omitted for version 1 peers, which never exchange input.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_input_frame_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum ServerMessage {
    #[serde(rename = "ready")]
    Ready {
        connection_id: String,
        protocol: &'static str,
        device_id: Option<Uuid>,
        limits: PublisherLimits,
        settings_revision: i64,
    },
    #[serde(rename = "terminal.opened")]
    TerminalOpened {
        request_id: String,
        terminal_id: Uuid,
        local_ref: String,
        next_offset: u64,
        durable_offset: u64,
        earliest_offset: u64,
        deduplicated: bool,
        accepts_input: bool,
    },
    /// Asks the publisher to open a terminal (spec §4.6).
    ///
    /// Carries no command, environment, working directory or `TERM`: the publishing
    /// machine alone decides what runs. Only a label and an initial geometry.
    #[serde(rename = "terminal.open_request")]
    TerminalOpenRequest {
        request_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cols: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        rows: Option<u32>,
    },
    #[serde(rename = "output.ack")]
    OutputAck {
        terminal_id: Uuid,
        durable_offset: u64,
        next_offset: u64,
    },
    #[serde(rename = "subscribed")]
    Subscribed {
        terminal_id: Uuid,
        requested_from_offset: u64,
        replay_start_offset: u64,
        next_offset: u64,
        durable_offset: u64,
        earliest_offset: u64,
        terminal_state: &'static str,
        label: String,
        cols: Option<u32>,
        rows: Option<u32>,
        term: Option<String>,
        /// Both omitted for version 1 subscribers (spec §6.2).
        #[serde(skip_serializing_if = "Option::is_none")]
        accepts_input: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        input_available: Option<bool>,
    },
    #[serde(rename = "gap")]
    Gap {
        terminal_id: Uuid,
        requested_from_offset: u64,
        available_from_offset: u64,
    },
    #[serde(rename = "durable")]
    Durable { durable_offset: u64 },
    /// Cumulative acknowledgement of a subscriber's input, sent only once the frame
    /// has been handed to the publisher's connection (spec §6.3).
    #[serde(rename = "input.ack")]
    InputAck {
        accepted_through: u64,
        relay_sequence: u64,
    },
    /// A subscriber's resize request, forwarded to the publisher, which decides.
    #[serde(rename = "terminal.resize_request")]
    TerminalResizeRequest {
        terminal_id: Uuid,
        cols: u32,
        rows: u32,
    },
    #[serde(rename = "terminal.resize")]
    TerminalResize {
        terminal_id: Uuid,
        cols: u32,
        rows: u32,
    },
    #[serde(rename = "terminal.closed")]
    TerminalClosed {
        terminal_id: Uuid,
        reason: String,
        next_offset: u64,
        durable_offset: u64,
    },
    #[serde(rename = "notice")]
    Notice { code: &'static str, message: String },
    #[serde(rename = "error")]
    Error {
        code: &'static str,
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        terminal_id: Option<Uuid>,
        #[serde(skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        next_offset: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        durable_offset: Option<u64>,
    },
    #[serde(rename = "ping")]
    Ping { at_unix_ms: i64 },
}

impl ServerMessage {
    pub fn to_text(&self) -> String {
        serde_json::to_string(self).expect("server control messages always serialise")
    }

    pub fn error(code: &'static str, message: impl Into<String>) -> Self {
        ServerMessage::Error {
            code,
            message: message.into(),
            terminal_id: None,
            request_id: None,
            next_offset: None,
            durable_offset: None,
        }
    }

    pub fn terminal_error(
        code: &'static str,
        message: impl Into<String>,
        terminal_id: Uuid,
    ) -> Self {
        ServerMessage::Error {
            code,
            message: message.into(),
            terminal_id: Some(terminal_id),
            request_id: None,
            next_offset: None,
            durable_offset: None,
        }
    }

    pub fn offset_mismatch(terminal_id: Uuid, next_offset: u64, durable_offset: u64) -> Self {
        ServerMessage::Error {
            code: error_code::OFFSET_MISMATCH,
            message: "output frame start offset does not match the authoritative next_offset"
                .to_string(),
            terminal_id: Some(terminal_id),
            request_id: None,
            next_offset: Some(next_offset),
            durable_offset: Some(durable_offset),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_publisher_open() {
        let text = r#"{"type":"terminal.open","request_id":"r1","local_ref":"pty0",
                        "label":"build shell","cols":120,"rows":40,"term":"xterm-256color"}"#;
        match classify::<PublisherMessage>(text) {
            Inbound::Message(PublisherMessage::Open {
                request_id,
                local_ref,
                cols,
                ..
            }) => {
                assert_eq!(request_id, "r1");
                assert_eq!(local_ref, "pty0");
                assert_eq!(cols, Some(120));
            }
            _ => panic!("expected an open message"),
        }
    }

    #[test]
    fn unknown_fields_are_ignored_within_version_one() {
        let text = r#"{"type":"terminal.resize","terminal_id":"9ca8a5f0-1d27-4d77-af11-d40c420568d2",
                        "cols":10,"rows":20,"future_field":"ignored"}"#;
        assert!(matches!(
            classify::<PublisherMessage>(text),
            Inbound::Message(PublisherMessage::Resize {
                cols: 10,
                rows: 20,
                ..
            })
        ));
    }

    #[test]
    fn unknown_required_type_is_rejected_but_optional_is_ignorable() {
        assert!(matches!(
            classify::<PublisherMessage>(r#"{"type":"terminal.teleport"}"#),
            Inbound::Rejected(_)
        ));
        assert!(matches!(
            classify::<PublisherMessage>(r#"{"type":"terminal.hint","optional":true}"#),
            Inbound::Ignorable(_)
        ));
    }

    #[test]
    fn malformed_input_is_rejected() {
        assert!(matches!(
            classify::<PublisherMessage>("not json"),
            Inbound::Rejected(_)
        ));
        assert!(matches!(
            classify::<PublisherMessage>("[]"),
            Inbound::Rejected(_)
        ));
        assert!(matches!(
            classify::<PublisherMessage>(r#"{"no":"type"}"#),
            Inbound::Rejected(_)
        ));
    }

    #[test]
    fn subscribe_offset_is_optional() {
        assert!(matches!(
            classify::<MirrorMessage>(r#"{"type":"subscribe"}"#),
            Inbound::Message(MirrorMessage::Subscribe { from_offset: None })
        ));
        assert!(matches!(
            classify::<MirrorMessage>(r#"{"type":"subscribe","from_offset":48120}"#),
            Inbound::Message(MirrorMessage::Subscribe {
                from_offset: Some(48_120)
            })
        ));
    }

    #[test]
    fn error_messages_carry_authoritative_offsets() {
        let id = Uuid::new_v4();
        let text = ServerMessage::offset_mismatch(id, 100, 80).to_text();
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["type"], "error");
        assert_eq!(parsed["code"], "offset_mismatch");
        assert_eq!(parsed["next_offset"], 100);
        assert_eq!(parsed["durable_offset"], 80);
    }
}
