//! The publisher protocol, `terminal-relay.publisher.v1` (spec §6.1).

use super::frames::{FrameError, decode_publisher_frame, encode_publisher_input_frame};
use super::messages::{
    Inbound, ProtocolVersion, PublisherLimits, PublisherMessage, ServerMessage, classify, close,
    error_code,
};
use super::registry::{
    ConnectionPermit, OpenOutcome, OpenRequest, PublisherDelivery, PublisherLease, Registry,
    TerminalOrigin,
};
use super::terminal::{AppendOutcome, TerminalHandle};
use super::wsio::{Heartbeat, WsWriter, spawn_writer, split};
use crate::db::repo::{self, Device};
use crate::metrics;
use crate::settings::Snapshot;
use crate::settings::defs::keys;
use axum::extract::ws::{Message, WebSocket};
use bytes::Bytes;
use futures_util::StreamExt;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

/// Limits captured at connect time and advertised in `ready`.
///
/// A live connection keeps these values, except that a later *reduction* is applied
/// immediately, because the specification requires reductions needed to prevent
/// resource exhaustion to take effect on existing connections promptly (spec §5.5).
#[derive(Clone, Copy)]
struct NegotiatedLimits {
    max_frame_bytes: u64,
    max_unacked_bytes: u64,
    max_control_bytes: u64,
}

impl NegotiatedLimits {
    fn from(snapshot: &Snapshot) -> Self {
        Self {
            max_frame_bytes: snapshot.u64(keys::LIMITS_MAX_OUTPUT_FRAME_BYTES),
            max_unacked_bytes: snapshot.u64(keys::LIMITS_MAX_UNACKED_OUTPUT_BYTES),
            max_control_bytes: snapshot.u64(keys::LIMITS_MAX_CONTROL_MESSAGE_BYTES),
        }
    }

    fn effective(&self, snapshot: &Snapshot) -> Self {
        Self {
            max_frame_bytes: self
                .max_frame_bytes
                .min(snapshot.u64(keys::LIMITS_MAX_OUTPUT_FRAME_BYTES)),
            max_unacked_bytes: self
                .max_unacked_bytes
                .min(snapshot.u64(keys::LIMITS_MAX_UNACKED_OUTPUT_BYTES)),
            max_control_bytes: self
                .max_control_bytes
                .min(snapshot.u64(keys::LIMITS_MAX_CONTROL_MESSAGE_BYTES)),
        }
    }
}

/// Oversized frames beyond this count in one connection close it, satisfying the
/// requirement to close publishers that *continue* exceeding negotiated limits.
const OVERSIZE_STRIKES: u32 = 3;

pub struct PublisherContext {
    pub registry: Arc<Registry>,
    pub device: Device,
    pub connection_id: String,
    pub shutdown: tokio::sync::watch::Receiver<bool>,
    pub version: ProtocolVersion,
    /// Inbound terminal input, present only on version 2 connections.
    pub input_rx: Option<tokio::sync::mpsc::Receiver<PublisherDelivery>>,
}

pub async fn handle(
    socket: WebSocket,
    context: PublisherContext,
    lease: PublisherLease,
    _permit: ConnectionPermit,
) {
    let PublisherContext {
        registry,
        device,
        connection_id,
        mut shutdown,
        version,
        input_rx,
    } = context;
    // A version 1 connection has no channel, so the select arm below never fires.
    let mut input_rx = input_rx;

    metrics::PUBLISHER_CONNECTIONS.inc();
    metrics::PUBLISHER_CONNECTIONS_TOTAL.inc();

    let snapshot = registry.settings().snapshot();
    let negotiated = NegotiatedLimits::from(&snapshot);

    let (sink, mut stream) = split(socket);
    let (writer, writer_task) = spawn_writer(sink, 256);

    let ready = ServerMessage::Ready {
        connection_id: connection_id.clone(),
        protocol: version.publisher_subprotocol(),
        device_id: Some(device.device_id),
        limits: PublisherLimits {
            max_output_frame_bytes: negotiated.max_frame_bytes,
            max_unacked_output_bytes: negotiated.max_unacked_bytes,
            max_control_message_bytes: negotiated.max_control_bytes,
            max_active_terminals: snapshot.u64(keys::LIMITS_MAX_ACTIVE_TERMINALS_PER_DEVICE),
            replay_capacity_bytes: snapshot.replay_capacity() as u64,
            heartbeat_interval_seconds: snapshot.u64(keys::WEBSOCKET_HEARTBEAT_INTERVAL_SECONDS),
            heartbeat_timeout_seconds: snapshot.u64(keys::WEBSOCKET_HEARTBEAT_TIMEOUT_SECONDS),
            max_input_frame_bytes: version
                .supports_input()
                .then(|| snapshot.u64(keys::LIMITS_MAX_INPUT_FRAME_BYTES)),
        },
        settings_revision: snapshot.revision,
    };
    writer.send(&ready).await;

    tracing::info!(
        event = "publisher_connected",
        protocol = version.publisher_subprotocol(),
        connection_id = %connection_id,
        device_id = %device.device_id,
        identity_id = %device.identity_id,
        settings_revision = snapshot.revision,
        "publisher relay connection established"
    );

    let mut session = Session {
        registry: Arc::clone(&registry),
        device: device.clone(),
        connection_id: connection_id.clone(),
        writer: writer.clone(),
        negotiated,
        version,
        terminals: HashMap::new(),
        ack_tasks: tokio::task::JoinSet::new(),
        oversize_strikes: 0,
        bytes_in: 0,
        frames_in: 0,
    };

    let mut heartbeat = Heartbeat::new(
        snapshot.duration_secs(keys::WEBSOCKET_HEARTBEAT_INTERVAL_SECONDS),
        snapshot.duration_secs(keys::WEBSOCKET_HEARTBEAT_TIMEOUT_SECONDS),
    );
    let mut recheck =
        tokio::time::interval(snapshot.duration_secs(keys::SECURITY_REVOCATION_RECHECK_SECONDS));
    recheck.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        let snapshot = registry.settings().snapshot();
        heartbeat.tighten(
            snapshot.duration_secs(keys::WEBSOCKET_HEARTBEAT_INTERVAL_SECONDS),
            snapshot.duration_secs(keys::WEBSOCKET_HEARTBEAT_TIMEOUT_SECONDS),
        );

        tokio::select! {
            biased;

            _ = lease.supersede.notified() => {
                // Either a newer connection took the device, or the device was revoked.
                writer.fail(
                    &ServerMessage::error(
                        error_code::SUPERSEDED,
                        "another publisher connection took over this device, or the device was revoked",
                    ),
                    close::SUPERSEDED,
                    "superseded",
                ).await;
                break;
            }

            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    writer.send(&ServerMessage::Notice {
                        code: error_code::SERVER_SHUTDOWN,
                        message: "server is shutting down; reconnect and resume from the offsets returned by terminal.opened".to_string(),
                    }).await;
                    writer.close(close::SERVER_SHUTDOWN, "server_shutdown").await;
                    break;
                }
            }

            _ = recheck.tick() => {
                if session.principal_revoked().await {
                    metrics::REVOCATION_ENFORCED_DISCONNECTS.inc();
                    writer.fail(
                        &ServerMessage::error(error_code::REVOKED, "device credential was revoked"),
                        close::REVOKED,
                        "revoked",
                    ).await;
                    break;
                }
            }

            // Terminal input bound for this device (spec §6.3). Only ever present on
            // a version 2 connection.
            delivery = async {
                match input_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    // Never resolves, so a version 1 connection ignores this arm.
                    None => std::future::pending().await,
                }
            } => {
                let Some(delivery) = delivery else { break };
                let sent = match delivery {
                    PublisherDelivery::Input { terminal_id, relay_sequence, payload } => {
                        let frame = encode_publisher_input_frame(
                            terminal_id,
                            relay_sequence,
                            &payload,
                        );
                        metrics::INPUT_BYTES_DELIVERED.add(payload.len() as u64);
                        metrics::INPUT_FRAMES_DELIVERED.inc();
                        writer.send_binary(frame).await
                    }
                    PublisherDelivery::ResizeRequest { terminal_id, cols, rows } => {
                        metrics::RESIZE_REQUESTS_FORWARDED.inc();
                        writer.send(&ServerMessage::TerminalResizeRequest {
                            terminal_id,
                            cols,
                            rows,
                        }).await
                    }
                    PublisherDelivery::OpenRequestDelivery { request_id, label, cols, rows } => {
                        writer.send(&ServerMessage::TerminalOpenRequest {
                            request_id,
                            label,
                            cols,
                            rows,
                        }).await
                    }
                };
                if !sent {
                    break;
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

            incoming = stream.next() => {
                let Some(incoming) = incoming else { break };
                let message = match incoming {
                    Ok(message) => message,
                    Err(e) => {
                        tracing::debug!(
                            event = "publisher_stream_error",
                            connection_id = %session.connection_id,
                            error = %e,
                        );
                        break;
                    }
                };
                heartbeat.touch();

                match message {
                    Message::Binary(bytes) => {
                        if !session.on_binary(bytes, &snapshot).await {
                            break;
                        }
                    }
                    Message::Text(text) => {
                        if !session.on_text(text.as_str(), &snapshot).await {
                            break;
                        }
                    }
                    Message::Ping(_) | Message::Pong(_) => {}
                    Message::Close(_) => break,
                }
            }
        }
    }

    let frames_in = session.frames_in;
    let bytes_in = session.bytes_in;

    // Every writer handle must be released before waiting on the writer task, which
    // only ends once all senders are gone. `Session` and each acknowledgement task
    // hold a clone, so a lingering one would keep this task — and with it the
    // publisher lease that starts the reconnect grace period — alive forever.
    session.ack_tasks.shutdown().await;
    drop(session);
    drop(writer);
    let _ = tokio::time::timeout(Duration::from_secs(5), writer_task).await;

    metrics::PUBLISHER_CONNECTIONS.dec();
    tracing::info!(
        event = "publisher_disconnected_connection",
        connection_id = %connection_id,
        device_id = %device.device_id,
        frames = frames_in,
        bytes = bytes_in,
        "publisher relay connection closed"
    );
    // Dropping the lease starts the reconnect grace period for this device.
    drop(lease);
}

struct Session {
    registry: Arc<Registry>,
    device: Device,
    connection_id: String,
    writer: WsWriter,
    negotiated: NegotiatedLimits,
    version: ProtocolVersion,
    terminals: HashMap<Uuid, Arc<TerminalHandle>>,
    ack_tasks: tokio::task::JoinSet<()>,
    oversize_strikes: u32,
    bytes_in: u64,
    frames_in: u64,
}

impl Session {
    async fn principal_revoked(&self) -> bool {
        let device_id = self.device.device_id;
        let db = self.registry.db().clone();
        match db.call(move |conn| repo::get_device(conn, device_id)).await {
            Ok(Some(device)) => device.revoked_at.is_some(),
            Ok(None) => true,
            Err(e) => {
                tracing::warn!(event = "revocation_check_failed", error = %e);
                false
            }
        }
    }

    /// Resolve a terminal this device is allowed to publish to.
    async fn lookup(&mut self, terminal_id: Uuid) -> Option<Arc<TerminalHandle>> {
        if let Some(handle) = self.terminals.get(&terminal_id) {
            return Some(Arc::clone(handle));
        }
        let handle = match self.registry.get_or_load(terminal_id).await {
            Ok(Some(handle)) => handle,
            Ok(None) => return None,
            Err(e) => {
                tracing::warn!(event = "terminal_load_failed", error = %e, terminal_id = %terminal_id);
                return None;
            }
        };
        // A device may publish only to its own terminals (spec §4.4).
        if handle.device_id != self.device.device_id {
            return None;
        }
        self.terminals.insert(terminal_id, Arc::clone(&handle));
        Some(handle)
    }

    async fn on_binary(&mut self, bytes: Bytes, snapshot: &Snapshot) -> bool {
        let frame = match decode_publisher_frame(&bytes) {
            Ok(frame) => frame,
            Err(e) => {
                // Malformed or unknown frame: error, then close code 1002 (spec §6).
                let code = match e {
                    FrameError::TooShort => error_code::INVALID_MESSAGE,
                    FrameError::UnknownType(_) => error_code::UNKNOWN_MESSAGE_TYPE,
                };
                self.writer
                    .fail(
                        &ServerMessage::error(code, e.to_string()),
                        close::PROTOCOL_ERROR,
                        "protocol_error",
                    )
                    .await;
                return false;
            }
        };

        let limits = self.negotiated.effective(snapshot);
        let payload_len = frame.payload.len() as u64;

        if payload_len > limits.max_frame_bytes {
            metrics::OVERSIZED_FRAMES_REJECTED.inc();
            self.oversize_strikes += 1;
            let message = ServerMessage::terminal_error(
                error_code::LIMIT_EXCEEDED,
                format!(
                    "output frame of {payload_len} bytes exceeds the negotiated maximum of {} bytes",
                    limits.max_frame_bytes
                ),
                frame.terminal_id,
            );
            if self.oversize_strikes >= OVERSIZE_STRIKES {
                self.writer
                    .fail(&message, close::LIMIT_EXCEEDED, "limit_exceeded")
                    .await;
                return false;
            }
            self.writer.send(&message).await;
            return true;
        }

        let Some(handle) = self.lookup(frame.terminal_id).await else {
            self.writer
                .send(&ServerMessage::terminal_error(
                    error_code::TERMINAL_NOT_FOUND,
                    "unknown terminal for this device",
                    frame.terminal_id,
                ))
                .await;
            return true;
        };

        let capacity = snapshot.replay_capacity();
        if !self
            .apply_backpressure(&handle, payload_len, capacity, &limits, snapshot)
            .await
        {
            return false;
        }

        match handle.append(frame.expected_offset, &frame.payload, capacity) {
            AppendOutcome::Accepted {
                next_offset,
                durable_offset,
            } => {
                self.frames_in += 1;
                self.bytes_in += payload_len;
                metrics::OUTPUT_BYTES_DELIVERED.add(payload_len * handle.subscriber_count() as u64);

                // Nudge the checkpoint task once the dirty threshold is reached; the
                // hot path itself never touches the database.
                if next_offset.saturating_sub(durable_offset)
                    >= snapshot.u64(keys::PERSISTENCE_FLUSH_BYTES)
                {
                    self.registry.request_flush();
                }
                true
            }
            AppendOutcome::OffsetMismatch {
                next_offset,
                durable_offset,
            } => {
                // Nothing was appended and no state changed, so the publisher can
                // retry deterministically (spec §6.1).
                self.writer
                    .send(&ServerMessage::offset_mismatch(
                        frame.terminal_id,
                        next_offset,
                        durable_offset,
                    ))
                    .await;
                true
            }
            AppendOutcome::NotOpen => {
                self.writer
                    .send(&ServerMessage::terminal_error(
                        error_code::TERMINAL_CLOSED,
                        "terminal is closed and cannot accept output",
                        frame.terminal_id,
                    ))
                    .await;
                true
            }
        }
    }

    /// Hold off accepting a frame while the unacknowledged window is full.
    ///
    /// Returns false when the connection must be closed. Waiting here — rather than
    /// evicting — is what guarantees dirty bytes are never lost to replay-window
    /// eviction (spec §7.2).
    async fn apply_backpressure(
        &self,
        handle: &Arc<TerminalHandle>,
        payload_len: u64,
        capacity: usize,
        limits: &NegotiatedLimits,
        snapshot: &Snapshot,
    ) -> bool {
        let wait = snapshot.duration_ms(keys::PERSISTENCE_BACKPRESSURE_WAIT_MS);

        // A frame at least as large as the whole window is accepted by the
        // specification, but only its tail can be retained, so everything already
        // dirty must be committed first.
        let target_dirty_headroom = if payload_len >= capacity as u64 {
            0
        } else {
            limits
                .max_unacked_bytes
                .min(capacity as u64)
                .saturating_sub(payload_len)
        };

        if handle.dirty_bytes() <= target_dirty_headroom {
            return true;
        }

        metrics::BACKPRESSURE_WAITS.inc();
        self.registry.request_flush();

        let offsets = handle.offsets();
        let target = offsets.next_offset.saturating_sub(target_dirty_headroom);
        handle.wait_for_durable(target, wait).await;

        if handle.dirty_bytes() <= target_dirty_headroom {
            return true;
        }

        metrics::BACKPRESSURE_TIMEOUTS.inc();
        if self.registry.storage_failing() {
            self.writer
                .fail(
                    &ServerMessage::terminal_error(
                        error_code::STORAGE_UNAVAILABLE,
                        "durable storage is unavailable; output cannot be acknowledged",
                        handle.terminal_id,
                    ),
                    close::STORAGE_UNAVAILABLE,
                    "storage_unavailable",
                )
                .await;
        } else {
            self.writer
                .fail(
                    &ServerMessage::terminal_error(
                        error_code::LIMIT_EXCEEDED,
                        "publisher exceeded its unacknowledged output window",
                        handle.terminal_id,
                    ),
                    close::LIMIT_EXCEEDED,
                    "limit_exceeded",
                )
                .await;
        }
        false
    }

    async fn on_text(&mut self, text: &str, snapshot: &Snapshot) -> bool {
        let limits = self.negotiated.effective(snapshot);
        if text.len() as u64 > limits.max_control_bytes {
            self.writer
                .fail(
                    &ServerMessage::error(
                        error_code::LIMIT_EXCEEDED,
                        "control message exceeds the negotiated maximum size",
                    ),
                    close::LIMIT_EXCEEDED,
                    "limit_exceeded",
                )
                .await;
            return false;
        }

        match classify::<PublisherMessage>(text) {
            Inbound::Ignorable(kind) => {
                tracing::debug!(
                    event = "control_message_ignored",
                    connection_id = %self.connection_id,
                    message_type = %kind,
                );
                true
            }
            Inbound::Rejected(detail) => {
                self.writer
                    .fail(
                        &ServerMessage::error(error_code::INVALID_MESSAGE, detail),
                        close::PROTOCOL_ERROR,
                        "protocol_error",
                    )
                    .await;
                false
            }
            Inbound::Message(message) => self.on_control(message, snapshot).await,
        }
    }

    async fn on_control(&mut self, message: PublisherMessage, snapshot: &Snapshot) -> bool {
        match message {
            PublisherMessage::Pong => true,
            PublisherMessage::Capabilities {
                terminal_open_requests,
            } => self.on_capabilities(terminal_open_requests).await,
            PublisherMessage::Open {
                request_id,
                local_ref,
                label,
                cols,
                rows,
                term,
                process_label,
                accepts_input,
                in_reply_to,
            } => {
                self.on_open(
                    request_id,
                    local_ref,
                    label,
                    cols,
                    rows,
                    term,
                    process_label,
                    accepts_input.unwrap_or(false),
                    in_reply_to,
                    snapshot,
                )
                .await
            }
            PublisherMessage::OpenDeclined {
                in_reply_to,
                reason,
                detail,
            } => self.on_open_declined(in_reply_to, reason, detail).await,
            PublisherMessage::Resize {
                terminal_id,
                cols,
                rows,
            } => self.on_resize(terminal_id, cols, rows, snapshot).await,
            PublisherMessage::Close {
                terminal_id,
                reason,
            } => self.on_close(terminal_id, reason).await,
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn on_open(
        &mut self,
        request_id: String,
        local_ref: String,
        label: Option<String>,
        cols: Option<u32>,
        rows: Option<u32>,
        term: Option<String>,
        process_label: Option<String>,
        accepts_input: bool,
        in_reply_to: Option<String>,
        snapshot: &Snapshot,
    ) -> bool {
        let label = label.unwrap_or_default();

        // A version 1 publisher has no frame in which input could be delivered, so a
        // terminal that claimed to accept it could never be written to (spec §6.1).
        if accepts_input && !self.version.supports_input() {
            self.writer
                .send(&ServerMessage::Error {
                    code: error_code::VALIDATION_FAILED,
                    message: "accepts_input requires the terminal-relay.publisher.v2 subprotocol"
                        .to_string(),
                    terminal_id: None,
                    request_id: Some(request_id),
                    next_offset: None,
                    durable_offset: None,
                })
                .await;
            return true;
        }
        // A version 1 publisher is never sent a request, so it can never be answering
        // one. Refused rather than ignored, so a confused peer learns it.
        if in_reply_to.is_some() && !self.version.supports_input() {
            self.writer
                .send(&ServerMessage::Error {
                    code: error_code::VALIDATION_FAILED,
                    message: "in_reply_to requires the terminal-relay.publisher.v2 subprotocol"
                        .to_string(),
                    terminal_id: None,
                    request_id: Some(request_id),
                    next_offset: None,
                    durable_offset: None,
                })
                .await;
            return true;
        }

        // Who asked, according to the relay's own record. A publisher that echoes an
        // `in_reply_to` nobody is waiting on gets `None` here and its terminal is
        // recorded as ordinary publisher-initiated: it cannot attribute a shell to a
        // principal that never asked for one (spec §4.6).
        let requested_by_principal = in_reply_to.as_deref().and_then(|id| {
            self.registry
                .open_request_principal(self.device.device_id, id)
        });
        let origin = if requested_by_principal.is_some() {
            TerminalOrigin::Request
        } else {
            TerminalOrigin::Publisher
        };

        if let Err(detail) =
            validate_open(&local_ref, &label, cols, rows, term.as_deref(), snapshot)
        {
            // Whoever is waiting on the HTTP side must learn now. Without this they wait
            // out the full open-request timeout for an answer that has already arrived.
            self.resolve_open(
                in_reply_to.as_deref(),
                OpenOutcome::Failed {
                    code: error_code::VALIDATION_FAILED,
                    message: detail.clone(),
                },
            );
            self.writer
                .send(&ServerMessage::Error {
                    code: error_code::VALIDATION_FAILED,
                    message: detail,
                    terminal_id: None,
                    request_id: Some(request_id),
                    next_offset: None,
                    durable_offset: None,
                })
                .await;
            return true;
        }

        // The process label can carry host-local detail, so it is only stored when an
        // operator has explicitly enabled it (spec §3.3).
        let process_label = if snapshot.bool(keys::TERMINAL_ALLOW_PROCESS_LABEL) {
            process_label
                .filter(|p| p.len() <= snapshot.usize(keys::LIMITS_MAX_PROCESS_LABEL_BYTES))
        } else {
            None
        };

        let request = OpenRequest {
            local_ref: local_ref.clone(),
            label,
            cols,
            rows,
            term,
            process_label,
            accepts_input,
            origin,
            requested_by_principal,
        };
        match self.registry.open_terminal(&self.device, request).await {
            Ok((handle, deduplicated)) => {
                let offsets = handle.offsets();
                self.terminals
                    .insert(handle.terminal_id, Arc::clone(&handle));
                self.spawn_ack_task(Arc::clone(&handle));

                tracing::info!(
                    event = "terminal_opened",
                    connection_id = %self.connection_id,
                    device_id = %self.device.device_id,
                    terminal_id = %handle.terminal_id,
                    deduplicated,
                    next_offset = offsets.next_offset,
                    accepts_input = handle.accepts_input(),
                    origin = origin.as_str(),
                    "terminal open acknowledged"
                );

                self.resolve_open(
                    in_reply_to.as_deref(),
                    OpenOutcome::Opened {
                        terminal_id: handle.terminal_id,
                        deduplicated,
                    },
                );

                self.writer
                    .send(&ServerMessage::TerminalOpened {
                        request_id,
                        terminal_id: handle.terminal_id,
                        local_ref,
                        next_offset: offsets.next_offset,
                        durable_offset: offsets.durable_offset,
                        earliest_offset: offsets.earliest_offset,
                        deduplicated,
                        accepts_input: handle.accepts_input(),
                    })
                    .await;
                true
            }
            Err(e) => {
                // The third exit, and the one easiest to forget: the publisher agreed to
                // open and the relay itself refused. A caller left waiting here would
                // sit out the whole timeout for a `limit_exceeded` already decided.
                self.resolve_open(
                    in_reply_to.as_deref(),
                    OpenOutcome::Failed {
                        code: error_code::LIMIT_EXCEEDED,
                        message: e.message.clone(),
                    },
                );
                self.writer
                    .send(&ServerMessage::Error {
                        code: error_code::LIMIT_EXCEEDED,
                        message: e.message,
                        terminal_id: None,
                        request_id: Some(request_id),
                        next_offset: None,
                        durable_offset: None,
                    })
                    .await;
                true
            }
        }
    }

    /// Publishes an outcome for a request, if this open was answering one.
    ///
    /// An `in_reply_to` matching nothing is logged and dropped: it is either a stale
    /// answer to a request that already timed out, or a forgery, and neither may touch
    /// a waiter that does not exist.
    fn resolve_open(&self, in_reply_to: Option<&str>, outcome: OpenOutcome) {
        let Some(request_id) = in_reply_to else {
            return;
        };
        if !self
            .registry
            .resolve_open_request(self.device.device_id, request_id, outcome)
        {
            tracing::debug!(
                device_id = %self.device.device_id,
                "publisher answered a terminal-open request nobody is waiting on"
            );
        }
    }

    /// A publisher refusing to open a terminal (spec §4.6).
    ///
    /// The reason is echoed to the caller from a closed set; the free-text detail is
    /// for the operator's log only, because a publisher must not be able to write
    /// arbitrary text into a phone's screen.
    async fn on_open_declined(
        &mut self,
        in_reply_to: String,
        reason: String,
        detail: Option<String>,
    ) -> bool {
        const KNOWN: &[&str] = &[
            "not_permitted",
            "unsupported",
            "busy",
            "limit_reached",
            "internal_error",
        ];
        let reason = if KNOWN.contains(&reason.as_str()) {
            reason
        } else {
            "internal_error".to_string()
        };
        tracing::info!(
            device_id = %self.device.device_id,
            reason = %reason,
            detail = detail.as_deref().map(|d| &d[..d.len().min(200)]).unwrap_or(""),
            "publisher declined a terminal-open request"
        );
        metrics::TERMINAL_OPEN_REQUESTS_DECLINED.inc();
        self.resolve_open(Some(&in_reply_to), OpenOutcome::Declined { reason });
        true
    }

    /// Records what this connection is willing to be asked to do (spec §4.6).
    ///
    /// The assertion is scoped to this connection: a reconnect starts at "no", so a
    /// machine whose owner turned the capability off between connections cannot be
    /// asked on the strength of an older assertion.
    async fn on_capabilities(&mut self, terminal_open_requests: bool) -> bool {
        // A version 1 publisher has no way to be asked — the request travels on the
        // version 2 delivery channel — so an assertion from one could only ever be a
        // lie or a mistake, and either way it is refused rather than ignored (spec §12).
        if terminal_open_requests && !self.version.supports_input() {
            self.writer
                .send(&ServerMessage::Error {
                    code: error_code::VALIDATION_FAILED,
                    message: "terminal_open_requests requires the terminal-relay.publisher.v2 \
                              subprotocol"
                        .to_string(),
                    terminal_id: None,
                    request_id: None,
                    next_offset: None,
                    durable_offset: None,
                })
                .await;
            return true;
        }
        self.registry.set_publisher_open_requests(
            self.device.device_id,
            &self.connection_id,
            terminal_open_requests,
        );
        tracing::info!(
            device_id = %self.device.device_id,
            terminal_open_requests,
            "publisher capabilities"
        );
        true
    }

    async fn on_resize(
        &mut self,
        terminal_id: Uuid,
        cols: u32,
        rows: u32,
        snapshot: &Snapshot,
    ) -> bool {
        if cols == 0
            || rows == 0
            || cols > snapshot.u32(keys::LIMITS_MAX_TERMINAL_COLS)
            || rows > snapshot.u32(keys::LIMITS_MAX_TERMINAL_ROWS)
        {
            self.writer
                .send(&ServerMessage::terminal_error(
                    error_code::VALIDATION_FAILED,
                    "terminal dimensions are out of range",
                    terminal_id,
                ))
                .await;
            return true;
        }

        let Some(handle) = self.lookup(terminal_id).await else {
            self.writer
                .send(&ServerMessage::terminal_error(
                    error_code::TERMINAL_NOT_FOUND,
                    "unknown terminal for this device",
                    terminal_id,
                ))
                .await;
            return true;
        };

        if !handle.resize(cols, rows) {
            self.writer
                .send(&ServerMessage::terminal_error(
                    error_code::TERMINAL_CLOSED,
                    "terminal is closed",
                    terminal_id,
                ))
                .await;
            return true;
        }

        let db = self.registry.db().clone();
        if let Err(e) = db
            .call(move |conn| repo::update_terminal_size(conn, terminal_id, cols, rows))
            .await
        {
            tracing::warn!(event = "resize_persist_failed", error = %e, terminal_id = %terminal_id);
        }
        true
    }

    async fn on_close(&mut self, terminal_id: Uuid, reason: Option<String>) -> bool {
        let Some(handle) = self.lookup(terminal_id).await else {
            self.writer
                .send(&ServerMessage::terminal_error(
                    error_code::TERMINAL_NOT_FOUND,
                    "unknown terminal for this device",
                    terminal_id,
                ))
                .await;
            return true;
        };

        let reason = sanitise_reason(reason);
        if handle.begin_close(&reason) {
            tracing::info!(
                event = "terminal_close_requested",
                connection_id = %self.connection_id,
                terminal_id = %terminal_id,
                reason = %reason,
                "terminal closing; committing final output"
            );
            // A close is an immediate checkpoint trigger (spec §7.2).
            self.registry.request_flush();
        }
        true
    }

    /// Acknowledge durability for one terminal.
    ///
    /// The acknowledgement is cumulative and is only ever sent after the batch
    /// containing those bytes has committed, so the publisher can safely release
    /// everything below `durable_offset` (spec §6.1).
    fn spawn_ack_task(&mut self, handle: Arc<TerminalHandle>) {
        let writer = self.writer.clone();
        self.ack_tasks.spawn(async move {
            let mut acked = 0u64;
            loop {
                let offsets = handle.offsets();
                if offsets.durable_offset > acked {
                    acked = offsets.durable_offset;
                    let sent = writer
                        .send(&ServerMessage::OutputAck {
                            terminal_id: handle.terminal_id,
                            durable_offset: offsets.durable_offset,
                            next_offset: offsets.next_offset,
                        })
                        .await;
                    if !sent {
                        return;
                    }
                    continue;
                }
                if handle.is_retired() && offsets.durable_offset <= acked {
                    return;
                }
                // Wake on the next durability advance, with a bounded poll so a
                // retired terminal cannot leave this task parked forever.
                handle
                    .wait_for_durable(offsets.durable_offset + 1, Duration::from_secs(5))
                    .await;
            }
        });
    }
}

fn validate_open(
    local_ref: &str,
    label: &str,
    cols: Option<u32>,
    rows: Option<u32>,
    term: Option<&str>,
    snapshot: &Snapshot,
) -> Result<(), String> {
    if local_ref.trim().is_empty() {
        return Err("local_ref must not be empty".to_string());
    }
    if local_ref.len() > snapshot.usize(keys::LIMITS_MAX_LOCAL_REF_BYTES) {
        return Err("local_ref is too long".to_string());
    }
    if label.len() > snapshot.usize(keys::LIMITS_MAX_LABEL_BYTES) {
        return Err("label is too long".to_string());
    }
    if let Some(term) = term
        && term.len() > snapshot.usize(keys::LIMITS_MAX_TERM_BYTES)
    {
        return Err("term is too long".to_string());
    }
    if let Some(cols) = cols
        && (cols == 0 || cols > snapshot.u32(keys::LIMITS_MAX_TERMINAL_COLS))
    {
        return Err("cols is out of range".to_string());
    }
    if let Some(rows) = rows
        && (rows == 0 || rows > snapshot.u32(keys::LIMITS_MAX_TERMINAL_ROWS))
    {
        return Err("rows is out of range".to_string());
    }
    Ok(())
}

/// Close reasons are echoed to subscribers, so they are length-bounded and stripped
/// of control characters.
fn sanitise_reason(reason: Option<String>) -> String {
    let reason = reason.unwrap_or_else(|| "closed_by_publisher".to_string());
    let cleaned: String = reason
        .chars()
        .filter(|c| !c.is_control())
        .take(128)
        .collect();
    if cleaned.trim().is_empty() {
        "closed_by_publisher".to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasons_are_bounded_and_stripped() {
        assert_eq!(sanitise_reason(None), "closed_by_publisher");
        assert_eq!(sanitise_reason(Some("  ".into())), "closed_by_publisher");
        assert_eq!(
            sanitise_reason(Some("process\n_exited".into())),
            "process_exited"
        );
        assert_eq!(sanitise_reason(Some("x".repeat(500))).len(), 128);
    }
}
