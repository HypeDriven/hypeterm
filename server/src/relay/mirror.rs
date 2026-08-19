//! The mirror protocol, `terminal-relay.mirror.v1` (spec §6.2).

use super::frames::{decode_mirror_input_frame, encode_mirror_frame};
use super::messages::{
    Inbound, MirrorMessage, ProtocolVersion, PublisherLimits, ServerMessage, classify, close,
    error_code,
};
use super::registry::{ConnectionPermit, InputDeliveryError, PublisherDelivery, Registry};
use super::terminal::TerminalHandle;
use super::terminal::{MirrorEvent, SubscribeError, Subscription, subscribed_message};
use super::wsio::{Heartbeat, WsWriter, spawn_writer, split};
use crate::metrics;
use crate::settings::Snapshot;
use crate::settings::defs::keys;
use axum::extract::ws::{Message, WebSocket};
use futures_util::StreamExt;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;
use uuid::Uuid;

pub struct MirrorContext {
    pub registry: Arc<Registry>,
    pub terminal_id: Uuid,
    pub identity_id: String,
    pub connection_id: String,
    pub shutdown: tokio::sync::watch::Receiver<bool>,
    pub version: ProtocolVersion,
    /// True when the subscriber's token carries `terminals:input`. Necessary but not
    /// sufficient: the other conditions of spec §4.5 are re-checked per frame.
    pub may_send_input: bool,
    /// Identifies the subscriber for rate limiting, without becoming a metric label.
    pub principal_id: String,
}

pub async fn handle(socket: WebSocket, context: MirrorContext, _permit: ConnectionPermit) {
    let MirrorContext {
        registry,
        terminal_id,
        identity_id,
        connection_id,
        mut shutdown,
        version,
        may_send_input,
        principal_id,
    } = context;

    metrics::MIRROR_CONNECTIONS.inc();
    metrics::MIRROR_CONNECTIONS_TOTAL.inc();

    let snapshot = registry.settings().snapshot();
    let (sink, mut stream) = split(socket);
    let (writer, writer_task) = spawn_writer(
        sink,
        snapshot
            .usize(keys::MIRROR_SUBSCRIBER_QUEUE_MESSAGES)
            .clamp(16, 4096),
    );

    writer
        .send(&ServerMessage::Ready {
            connection_id: connection_id.clone(),
            protocol: version.mirror_subprotocol(),
            device_id: None,
            limits: PublisherLimits {
                max_output_frame_bytes: snapshot.u64(keys::MIRROR_REPLAY_CHUNK_BYTES),
                max_unacked_output_bytes: 0,
                max_control_message_bytes: snapshot.u64(keys::LIMITS_MAX_CONTROL_MESSAGE_BYTES),
                max_active_terminals: 1,
                replay_capacity_bytes: snapshot.replay_capacity() as u64,
                heartbeat_interval_seconds: snapshot
                    .u64(keys::WEBSOCKET_HEARTBEAT_INTERVAL_SECONDS),
                heartbeat_timeout_seconds: snapshot.u64(keys::WEBSOCKET_HEARTBEAT_TIMEOUT_SECONDS),
                max_input_frame_bytes: version
                    .supports_input()
                    .then(|| snapshot.u64(keys::LIMITS_MAX_INPUT_FRAME_BYTES)),
            },
            settings_revision: snapshot.revision,
        })
        .await;

    // Exactly one subscription message opens the stream (spec §6.2).
    let handshake_timeout = snapshot.duration_secs(keys::WEBSOCKET_HANDSHAKE_TIMEOUT_SECONDS);
    let requested = match tokio::time::timeout(
        handshake_timeout,
        read_subscribe(&mut stream, &writer, &snapshot),
    )
    .await
    {
        Ok(Some(offset)) => offset,
        Ok(None) => {
            finish(writer, writer_task).await;
            return;
        }
        Err(_) => {
            writer
                .fail(
                    &ServerMessage::error(
                        error_code::HANDSHAKE_TIMEOUT,
                        "no subscribe message arrived before the handshake deadline",
                    ),
                    close::HANDSHAKE_TIMEOUT,
                    "handshake_timeout",
                )
                .await;
            finish(writer, writer_task).await;
            return;
        }
    };

    let handle = match registry.get_or_load(terminal_id).await {
        Ok(Some(handle)) => handle,
        Ok(None) => {
            writer
                .fail(
                    &ServerMessage::error(error_code::TERMINAL_NOT_FOUND, "terminal not found"),
                    close::NOT_FOUND,
                    "not_found",
                )
                .await;
            finish(writer, writer_task).await;
            return;
        }
        Err(e) => {
            writer
                .fail(
                    &ServerMessage::error(error_code::STORAGE_UNAVAILABLE, e.message),
                    close::STORAGE_UNAVAILABLE,
                    "storage_unavailable",
                )
                .await;
            finish(writer, writer_task).await;
            return;
        }
    };

    // Terminals are private to their owning identity; a non-owner is told the
    // terminal does not exist (spec §4.4).
    if handle.identity_id != identity_id {
        writer
            .fail(
                &ServerMessage::error(error_code::TERMINAL_NOT_FOUND, "terminal not found"),
                close::NOT_FOUND,
                "not_found",
            )
            .await;
        finish(writer, writer_task).await;
        return;
    }

    let subscription = match handle.subscribe(
        requested,
        snapshot.usize(keys::MIRROR_REPLAY_CHUNK_BYTES),
        snapshot.usize(keys::MIRROR_SUBSCRIBER_QUEUE_MESSAGES),
        snapshot.u64(keys::MIRROR_SUBSCRIBER_QUEUE_BYTES),
    ) {
        Ok(subscription) => subscription,
        Err(SubscribeError::OffsetAhead {
            next_offset,
            durable_offset,
        }) => {
            writer
                .fail(
                    &ServerMessage::Error {
                        code: error_code::OFFSET_AHEAD,
                        message: "requested offset is beyond the terminal's next_offset"
                            .to_string(),
                        terminal_id: Some(terminal_id),
                        request_id: None,
                        next_offset: Some(next_offset),
                        durable_offset: Some(durable_offset),
                    },
                    close::OFFSET_AHEAD,
                    "offset_ahead",
                )
                .await;
            finish(writer, writer_task).await;
            return;
        }
    };

    let subscriber_id = subscription.subscriber_id;
    let requested_offset = requested.unwrap_or(subscription.offsets.earliest_offset);

    tracing::info!(
        event = "mirror_subscribed",
        connection_id = %connection_id,
        terminal_id = %terminal_id,
        identity_id = %identity_id,
        requested_from_offset = requested_offset,
        replay_start_offset = subscription.replay_start_offset,
        next_offset = subscription.offsets.next_offset,
        gap = subscription.gap.is_some(),
        "mirror subscription established"
    );

    writer
        .send(&subscribed_message(
            terminal_id,
            requested_offset,
            &subscription,
            version.supports_input().then(|| {
                let accepts = handle.accepts_input();
                (
                    accepts,
                    may_send_input
                        && accepts
                        && input_currently_available(&registry, &handle, &snapshot),
                )
            }),
        ))
        .await;

    // A gap notice precedes the replay it describes (spec §6.2).
    if let Some((requested_from, available_from)) = subscription.gap {
        writer
            .send(&ServerMessage::Gap {
                terminal_id,
                requested_from_offset: requested_from,
                available_from_offset: available_from,
            })
            .await;
    }

    let Subscription {
        mut receiver,
        queued_bytes,
        signal,
        replay,
        ..
    } = subscription;

    for (start_offset, payload) in replay {
        let len = payload.len() as u64;
        if !writer
            .send_binary(encode_mirror_frame(start_offset, &payload))
            .await
        {
            handle.unsubscribe(subscriber_id);
            finish(writer, writer_task).await;
            return;
        }
        metrics::OUTPUT_BYTES_REPLAYED.add(len);
        metrics::OUTPUT_BYTES_DELIVERED.add(len);
    }

    let mut heartbeat = Heartbeat::new(
        snapshot.duration_secs(keys::WEBSOCKET_HEARTBEAT_INTERVAL_SECONDS),
        snapshot.duration_secs(keys::WEBSOCKET_HEARTBEAT_TIMEOUT_SECONDS),
    );
    let mut input_state = InputState {
        expected_sequence: 1,
        accepted_through: 0,
        frames: Bucket::new(
            snapshot.int(keys::RATELIMIT_INPUT_FRAMES_PER_MINUTE_PER_SUBSCRIBER) as f64,
        ),
        bytes: Bucket::new(
            snapshot.int(keys::RATELIMIT_INPUT_BYTES_PER_MINUTE_PER_SUBSCRIBER) as f64,
        ),
    };
    let mut closed_normally = false;

    loop {
        let snapshot = registry.settings().snapshot();
        heartbeat.tighten(
            snapshot.duration_secs(keys::WEBSOCKET_HEARTBEAT_INTERVAL_SECONDS),
            snapshot.duration_secs(keys::WEBSOCKET_HEARTBEAT_TIMEOUT_SECONDS),
        );

        tokio::select! {
            biased;

            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    writer.send(&ServerMessage::Notice {
                        code: error_code::SERVER_SHUTDOWN,
                        message: "server is shutting down; reconnect and resume from your last processed offset".to_string(),
                    }).await;
                    writer.close(close::SERVER_SHUTDOWN, "server_shutdown").await;
                    break;
                }
            }

            event = receiver.recv() => {
                match event {
                    Some(event) => {
                        if let MirrorEvent::Output { payload, .. } = &event {
                            queued_bytes.fetch_sub(payload.len() as u64, Ordering::AcqRel);
                        }
                        match deliver(&writer, terminal_id, event).await {
                            Delivery::Continue => {}
                            Delivery::Closed => {
                                closed_normally = true;
                                break;
                            }
                            Delivery::Failed => break,
                        }
                    }
                    None => {
                        // The sender was dropped: read the out-of-band reason.
                        let termination =
                            signal.terminate.lock().unwrap_or_else(|e| e.into_inner()).clone();
                        if signal.overflowed.load(Ordering::Acquire) {
                            metrics::SLOW_CONSUMER_DISCONNECTS.inc();
                            let termination = super::terminal::Termination::slow_consumer();
                            tracing::info!(
                                event = "mirror_slow_consumer",
                                connection_id = %connection_id,
                                terminal_id = %terminal_id,
                                "closing a subscriber that exceeded its outbound queue bound"
                            );
                            writer.fail(
                                &ServerMessage::terminal_error(termination.error_code, termination.message, terminal_id),
                                termination.close_code,
                                "slow_consumer",
                            ).await;
                        } else if let Some(termination) = termination {
                            writer.fail(
                                &ServerMessage::terminal_error(termination.error_code, termination.message, terminal_id),
                                termination.close_code,
                                "terminated",
                            ).await;
                        } else {
                            writer.close(close::GOING_AWAY, "going_away").await;
                        }
                        break;
                    }
                }
            }

            incoming = stream.next() => {
                let Some(incoming) = incoming else { break };
                let Ok(message) = incoming else { break };
                heartbeat.touch();
                match message {
                    Message::Text(text) => {
                        match classify::<MirrorMessage>(text.as_str()) {
                            Inbound::Message(MirrorMessage::Pong) => {}
                            Inbound::Message(MirrorMessage::ResizeRequest { cols, rows }) => {
                                handle_resize_request(
                                    cols,
                                    rows,
                                    &writer,
                                    &registry,
                                    &handle,
                                    version,
                                    may_send_input,
                                ).await;
                            }
                            Inbound::Message(MirrorMessage::Subscribe { .. }) => {
                                // Exactly one subscription per connection.
                                writer.fail(
                                    &ServerMessage::error(
                                        error_code::ALREADY_SUBSCRIBED,
                                        "this connection is already subscribed",
                                    ),
                                    close::PROTOCOL_ERROR,
                                    "protocol_error",
                                ).await;
                                break;
                            }
                            Inbound::Ignorable(kind) => {
                                tracing::debug!(event = "control_message_ignored", message_type = %kind);
                            }
                            Inbound::Rejected(detail) => {
                                writer.fail(
                                    &ServerMessage::error(error_code::INVALID_MESSAGE, detail),
                                    close::PROTOCOL_ERROR,
                                    "protocol_error",
                                ).await;
                                break;
                            }
                        }
                    }
                    Message::Binary(frame) => {
                        if !version.supports_input() {
                            // A version 1 mirror is output only, so a binary frame is
                            // a protocol violation rather than input.
                            writer.fail(
                                &ServerMessage::error(
                                    error_code::INVALID_MESSAGE,
                                    "terminal-relay.mirror.v1 subscribers must not send binary frames",
                                ),
                                close::PROTOCOL_ERROR,
                                "protocol_error",
                            ).await;
                            break;
                        }
                        if !handle_input_frame(
                            &frame,
                            &writer,
                            &registry,
                            &handle,
                            &mut input_state,
                            may_send_input,
                            &connection_id,
                            &principal_id,
                        ).await {
                            break;
                        }
                    }
                    Message::Ping(_) | Message::Pong(_) => {}
                    Message::Close(_) => break,
                }
            }

            _ = tokio::time::sleep(heartbeat.interval()) => {
                if heartbeat.expired() {
                    writer.close(close::HEARTBEAT_TIMEOUT, "heartbeat_timeout").await;
                    break;
                }
                if !writer.ping().await {
                    break;
                }
            }
        }
    }

    handle.unsubscribe(subscriber_id);
    if closed_normally {
        writer.close(close::NORMAL, "terminal_closed").await;
    }
    finish(writer, writer_task).await;

    metrics::MIRROR_CONNECTIONS.dec();
    tracing::info!(
        event = "mirror_disconnected",
        connection_id = %connection_id,
        terminal_id = %terminal_id,
        "mirror subscription closed"
    );
}

/// Validate and forward one input frame.
///
/// Every condition of spec §4.5 is re-checked here rather than trusted from the
/// handshake, because three of the four can change while a subscription is open: an
/// operator can disable input, the publisher can disconnect, and the terminal can
/// close. Returns false when the connection must end.
#[allow(clippy::too_many_arguments)]
async fn handle_input_frame(
    frame: &bytes::Bytes,
    writer: &WsWriter,
    registry: &Arc<Registry>,
    handle: &Arc<TerminalHandle>,
    state: &mut InputState,
    may_send_input: bool,
    connection_id: &str,
    principal_id: &str,
) -> bool {
    let terminal_id = handle.terminal_id;
    // Taken here, not at the top of the connection loop: a loop-level snapshot is
    // captured before awaiting the next frame, so an operator disabling input would
    // not take effect until the frame after next. Input is a security control and
    // must apply at once (spec §4.5), and one snapshot per frame is still one
    // immutable snapshot per operation.
    let snapshot = registry.settings().snapshot();

    let decoded = match decode_mirror_input_frame(frame) {
        Ok(decoded) => decoded,
        Err(e) => {
            writer
                .fail(
                    &ServerMessage::error(error_code::INVALID_MESSAGE, e.to_string()),
                    close::PROTOCOL_ERROR,
                    "protocol_error",
                )
                .await;
            return false;
        }
    };

    // Refusals that leave the subscription open, in the order of spec §4.5.
    let refusal = if !snapshot.bool(keys::FEATURES_INPUT_ENABLED) {
        Some((
            error_code::INPUT_DISABLED,
            "terminal input is disabled by the operator".to_string(),
        ))
    } else if !may_send_input {
        Some((
            error_code::INPUT_FORBIDDEN,
            "this subscription does not hold the terminals:input scope".to_string(),
        ))
    } else if !handle.accepts_input() {
        Some((
            error_code::INPUT_NOT_ACCEPTED,
            "the publishing device did not opt in to terminal input".to_string(),
        ))
    } else if !matches!(handle.lifecycle(), super::terminal::Lifecycle::Open) {
        Some((
            error_code::TERMINAL_CLOSED,
            "terminal is closed".to_string(),
        ))
    } else if decoded.payload.is_empty() {
        Some((
            error_code::INVALID_MESSAGE,
            "zero-length input frames must not be sent".to_string(),
        ))
    } else if decoded.payload.len() as u64 > snapshot.u64(keys::LIMITS_MAX_INPUT_FRAME_BYTES) {
        Some((
            error_code::LIMIT_EXCEEDED,
            format!(
                "input frame exceeds the negotiated maximum of {} bytes",
                snapshot.u64(keys::LIMITS_MAX_INPUT_FRAME_BYTES)
            ),
        ))
    } else if decoded.client_sequence != state.expected_sequence {
        Some((
            error_code::INPUT_SEQUENCE_MISMATCH,
            format!("expected client sequence {}", state.expected_sequence),
        ))
    } else if !state.frames.take(
        1.0,
        snapshot.int(keys::RATELIMIT_INPUT_FRAMES_PER_MINUTE_PER_SUBSCRIBER) as f64,
    ) || !state.bytes.take(
        decoded.payload.len() as f64,
        snapshot.int(keys::RATELIMIT_INPUT_BYTES_PER_MINUTE_PER_SUBSCRIBER) as f64,
    ) {
        metrics::INPUT_RATE_LIMITED.inc();
        Some((
            error_code::RATE_LIMITED,
            "input rate limit exceeded for this subscription".to_string(),
        ))
    } else {
        None
    };

    if let Some((code, message)) = refusal {
        metrics::INPUT_FRAMES_REFUSED.inc();
        // Byte counts only: input payloads are never logged (spec §9).
        tracing::debug!(
            event = "input_refused",
            connection_id = %connection_id,
            terminal_id = %terminal_id,
            principal = %principal_id,
            code,
            bytes = decoded.payload.len(),
        );
        writer
            .send(&ServerMessage::terminal_error(code, message, terminal_id))
            .await;
        return true;
    }

    // The sequence is claimed only once the frame is about to be delivered, so a
    // refused frame can be retried with the same sequence.
    let relay_sequence = handle.next_input_sequence();
    let payload_len = decoded.payload.len();

    match registry.deliver_to_publisher(
        handle.device_id,
        PublisherDelivery::Input {
            terminal_id,
            relay_sequence,
            payload: decoded.payload,
        },
    ) {
        Ok(()) => {
            state.expected_sequence += 1;
            state.accepted_through = decoded.client_sequence;
            tracing::debug!(
                event = "input_delivered",
                connection_id = %connection_id,
                terminal_id = %terminal_id,
                principal = %principal_id,
                relay_sequence,
                bytes = payload_len,
            );
            writer
                .send(&ServerMessage::InputAck {
                    accepted_through: state.accepted_through,
                    relay_sequence,
                })
                .await;
            true
        }
        Err(error) => {
            metrics::INPUT_FRAMES_REFUSED.inc();
            let (code, message) = match error {
                InputDeliveryError::NoPublisher => (
                    error_code::INPUT_UNDELIVERABLE,
                    "no version 2 publisher is connected for this terminal's device",
                ),
                InputDeliveryError::Backpressure => (
                    error_code::INPUT_BACKPRESSURE,
                    "the publisher is not keeping up with input; try again",
                ),
            };
            writer
                .send(&ServerMessage::terminal_error(code, message, terminal_id))
                .await;
            true
        }
    }
}

/// Forward a resize request. The publisher decides; the relay only relays.
#[allow(clippy::too_many_arguments)]
async fn handle_resize_request(
    cols: u32,
    rows: u32,
    writer: &WsWriter,
    registry: &Arc<Registry>,
    handle: &Arc<TerminalHandle>,
    version: ProtocolVersion,
    may_send_input: bool,
) {
    let terminal_id = handle.terminal_id;
    let snapshot = registry.settings().snapshot();

    let refusal = if !version.supports_input() {
        Some("resize requests require the terminal-relay.mirror.v2 subprotocol")
    } else if !snapshot.bool(keys::FEATURES_CLIENT_RESIZE_ENABLED) {
        Some("client-initiated resize is disabled by the operator")
    } else if !may_send_input || !handle.accepts_input() {
        Some("this subscription may not drive this terminal")
    } else if cols == 0
        || rows == 0
        || cols > snapshot.u32(keys::LIMITS_MAX_TERMINAL_COLS)
        || rows > snapshot.u32(keys::LIMITS_MAX_TERMINAL_ROWS)
    {
        Some("requested terminal dimensions are out of range")
    } else {
        None
    };

    if let Some(message) = refusal {
        writer
            .send(&ServerMessage::terminal_error(
                error_code::RESIZE_REFUSED,
                message,
                terminal_id,
            ))
            .await;
        return;
    }

    if registry
        .deliver_to_publisher(
            handle.device_id,
            PublisherDelivery::ResizeRequest {
                terminal_id,
                cols,
                rows,
            },
        )
        .is_err()
    {
        writer
            .send(&ServerMessage::terminal_error(
                error_code::INPUT_UNDELIVERABLE,
                "no version 2 publisher is connected for this terminal's device",
                terminal_id,
            ))
            .await;
    }
}

/// Per-subscription input state (spec §6.3).
struct InputState {
    /// The client sequence this subscription expects next; starts at 1.
    expected_sequence: u64,
    accepted_through: u64,
    frames: Bucket,
    bytes: Bucket,
}

/// A token bucket, refilled continuously, used for the per-subscriber input limits.
struct Bucket {
    tokens: f64,
    capacity: f64,
    refill_per_second: f64,
    last: Instant,
}

impl Bucket {
    fn new(per_minute: f64) -> Self {
        Self {
            tokens: per_minute,
            capacity: per_minute,
            refill_per_second: per_minute / 60.0,
            last: Instant::now(),
        }
    }

    /// Re-read the limit each time so a settings change applies without a reconnect.
    fn take(&mut self, amount: f64, per_minute: f64) -> bool {
        if (per_minute - self.capacity).abs() > f64::EPSILON {
            self.capacity = per_minute;
            self.refill_per_second = per_minute / 60.0;
            self.tokens = self.tokens.min(per_minute);
        }
        let elapsed = self.last.elapsed().as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.refill_per_second).min(self.capacity);
        self.last = Instant::now();
        if self.tokens >= amount {
            self.tokens -= amount;
            true
        } else {
            false
        }
    }
}

/// Whether every condition in spec §4.5 currently holds for this subscription, other
/// than the token scope the caller already checked.
fn input_currently_available(
    registry: &Arc<Registry>,
    handle: &Arc<TerminalHandle>,
    snapshot: &Snapshot,
) -> bool {
    snapshot.bool(keys::FEATURES_INPUT_ENABLED)
        && handle.accepts_input()
        && registry.publisher_accepts_input(handle.device_id)
}

enum Delivery {
    Continue,
    Closed,
    Failed,
}

async fn deliver(writer: &WsWriter, terminal_id: Uuid, event: MirrorEvent) -> Delivery {
    let sent = match event {
        MirrorEvent::Output {
            start_offset,
            payload,
        } => {
            // Zero-length output frames are never sent (spec §6.2).
            if payload.is_empty() {
                return Delivery::Continue;
            }
            let len = payload.len() as u64;
            let ok = writer
                .send_binary(encode_mirror_frame(start_offset, &payload))
                .await;
            if ok {
                metrics::OUTPUT_BYTES_DELIVERED.add(len);
            }
            ok
        }
        MirrorEvent::Durable { durable_offset } => {
            writer
                .send(&ServerMessage::Durable { durable_offset })
                .await
        }
        MirrorEvent::Resize { cols, rows } => {
            writer
                .send(&ServerMessage::TerminalResize {
                    terminal_id,
                    cols,
                    rows,
                })
                .await
        }
        MirrorEvent::Closed {
            reason,
            next_offset,
            durable_offset,
        } => {
            let ok = writer
                .send(&ServerMessage::TerminalClosed {
                    terminal_id,
                    reason,
                    next_offset,
                    durable_offset,
                })
                .await;
            return if ok {
                Delivery::Closed
            } else {
                Delivery::Failed
            };
        }
    };
    if sent {
        Delivery::Continue
    } else {
        Delivery::Failed
    }
}

/// Read the single expected `subscribe` message. Returns `None` when the connection
/// failed and has already been told why.
async fn read_subscribe(
    stream: &mut futures_util::stream::SplitStream<WebSocket>,
    writer: &WsWriter,
    snapshot: &crate::settings::Snapshot,
) -> Option<Option<u64>> {
    loop {
        let incoming = stream.next().await?;
        let Ok(message) = incoming else { return None };
        match message {
            Message::Text(text) => {
                if text.len() as u64 > snapshot.u64(keys::LIMITS_MAX_CONTROL_MESSAGE_BYTES) {
                    writer
                        .fail(
                            &ServerMessage::error(
                                error_code::LIMIT_EXCEEDED,
                                "control message exceeds the maximum size",
                            ),
                            close::LIMIT_EXCEEDED,
                            "limit_exceeded",
                        )
                        .await;
                    return None;
                }
                match classify::<MirrorMessage>(text.as_str()) {
                    Inbound::Message(MirrorMessage::Subscribe { from_offset }) => {
                        return Some(from_offset);
                    }
                    // Anything other than the opening subscribe is ignored until the
                    // subscription exists; a resize before it has no terminal to size.
                    Inbound::Message(MirrorMessage::Pong)
                    | Inbound::Message(MirrorMessage::ResizeRequest { .. }) => continue,
                    Inbound::Ignorable(_) => continue,
                    Inbound::Rejected(detail) => {
                        writer
                            .fail(
                                &ServerMessage::error(error_code::INVALID_MESSAGE, detail),
                                close::PROTOCOL_ERROR,
                                "protocol_error",
                            )
                            .await;
                        return None;
                    }
                }
            }
            Message::Binary(_) => {
                writer
                    .fail(
                        &ServerMessage::error(
                            error_code::INVALID_MESSAGE,
                            "the first message must be a subscribe control message",
                        ),
                        close::PROTOCOL_ERROR,
                        "protocol_error",
                    )
                    .await;
                return None;
            }
            Message::Ping(_) | Message::Pong(_) => continue,
            Message::Close(_) => return None,
        }
    }
}

async fn finish(writer: WsWriter, writer_task: tokio::task::JoinHandle<()>) {
    // Dropping the last writer handle is what ends the writer task; the timeout is a
    // guard so a wedged socket cannot hold this connection's resources open.
    drop(writer);
    if tokio::time::timeout(std::time::Duration::from_secs(5), writer_task)
        .await
        .is_err()
    {
        tracing::debug!(
            event = "mirror_writer_timeout",
            "writer task did not finish in time"
        );
    }
}
