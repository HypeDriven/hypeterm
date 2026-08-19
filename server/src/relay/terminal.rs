//! In-memory terminal state: the replay ring, the offset trio, and subscriber fan-out.
//!
//! Everything that must be atomic with respect to readers — appending bytes,
//! advancing `next_offset`, evicting old bytes, and queueing the same bytes to
//! subscribers — happens inside one critical section (spec §7.4). A subscriber
//! therefore observes either the state before an append or the state after it, never
//! a partially updated offset or a non-contiguous replay range.

use super::messages::{ServerMessage, close, error_code};
use super::ring::Ring;
use crate::db::repo::{TerminalRow, TerminalState};
use crate::metrics;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::{Notify, mpsc};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lifecycle {
    Open,
    /// Closing: no further output is accepted, and the close is committed with the
    /// final bytes before subscribers are told.
    Closing(String),
    Closed(String),
}

impl Lifecycle {
    pub fn as_state(&self) -> TerminalState {
        match self {
            Lifecycle::Open | Lifecycle::Closing(_) => TerminalState::Open,
            Lifecycle::Closed(_) => TerminalState::Closed,
        }
    }

    pub fn as_str(&self) -> &'static str {
        self.as_state().as_str()
    }
}

/// An event delivered to one mirror subscriber, in stream order.
#[derive(Debug, Clone)]
pub enum MirrorEvent {
    Output {
        start_offset: u64,
        payload: Bytes,
    },
    Durable {
        durable_offset: u64,
    },
    Resize {
        cols: u32,
        rows: u32,
    },
    Closed {
        reason: String,
        next_offset: u64,
        durable_offset: u64,
    },
}

impl MirrorEvent {
    fn queue_cost(&self) -> u64 {
        match self {
            MirrorEvent::Output { payload, .. } => payload.len() as u64,
            _ => 0,
        }
    }
}

/// Out-of-band signal for conditions that must reach a subscriber even when its
/// queue is full, so a slow consumer can still be told why it was dropped.
#[derive(Debug, Default)]
pub struct SubscriberSignal {
    pub overflowed: AtomicBool,
    pub terminate: Mutex<Option<Termination>>,
}

#[derive(Debug, Clone)]
pub struct Termination {
    pub close_code: u16,
    pub error_code: &'static str,
    pub message: String,
}

impl Termination {
    pub fn slow_consumer() -> Self {
        Self {
            close_code: close::SLOW_CONSUMER,
            error_code: error_code::SLOW_CONSUMER,
            message: "subscriber exceeded its outbound queue bound; reconnect from the last processed offset"
                .to_string(),
        }
    }

    pub fn server_shutdown() -> Self {
        Self {
            close_code: close::SERVER_SHUTDOWN,
            error_code: error_code::SERVER_SHUTDOWN,
            message: "server is shutting down; reconnect and resume from the last processed offset"
                .to_string(),
        }
    }

    pub fn revoked() -> Self {
        Self {
            close_code: close::REVOKED,
            error_code: error_code::REVOKED,
            message: "credential was revoked".to_string(),
        }
    }
}

struct Subscriber {
    id: u64,
    tx: mpsc::Sender<MirrorEvent>,
    queued_bytes: Arc<AtomicU64>,
    max_queued_bytes: u64,
    signal: Arc<SubscriberSignal>,
}

struct TerminalInner {
    ring: Ring,
    /// Offset immediately after the last byte accepted into memory.
    next_offset: u64,
    /// Offset immediately after the last byte committed to the database.
    durable_offset: u64,
    dirty_since: Option<Instant>,
    frames_since_checkpoint: u64,
    lifecycle: Lifecycle,
    label: String,
    cols: Option<u32>,
    rows: Option<u32>,
    term: Option<String>,
    last_activity: DateTime<Utc>,
    /// The publisher's opt-in to receiving input (spec §4.5).
    accepts_input: bool,
    /// Per-terminal input sequence, starting at 1. Deliberately not durable: input is
    /// never persisted and is never replayed across a reconnect (spec §6.3).
    next_input_sequence: u64,
    subscribers: Vec<Subscriber>,
    next_subscriber_id: u64,
    /// Set when the terminal's close has been committed and fanned out, so the
    /// registry can retire it.
    retired: bool,
}

pub struct TerminalHandle {
    pub terminal_id: Uuid,
    pub device_id: Uuid,
    pub identity_id: String,
    pub local_ref: String,
    pub created_at: DateTime<Utc>,
    inner: Mutex<TerminalInner>,
    /// Woken whenever `durable_offset` advances or the lifecycle changes.
    durable_notify: Notify,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Offsets {
    pub earliest_offset: u64,
    pub next_offset: u64,
    pub durable_offset: u64,
}

impl Offsets {
    pub fn retained_bytes(&self) -> u64 {
        self.next_offset.saturating_sub(self.earliest_offset)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum AppendOutcome {
    Accepted {
        next_offset: u64,
        durable_offset: u64,
    },
    /// The frame's start offset is not the authoritative `next_offset`; nothing was
    /// appended and no terminal state changed (spec §6.1).
    OffsetMismatch {
        next_offset: u64,
        durable_offset: u64,
    },
    NotOpen,
}

pub struct Subscription {
    pub subscriber_id: u64,
    pub receiver: mpsc::Receiver<MirrorEvent>,
    pub queued_bytes: Arc<AtomicU64>,
    pub signal: Arc<SubscriberSignal>,
    /// Replay frames, already split to the configured chunk size, covering
    /// `[replay_start_offset, next_offset)` as of the moment of subscription.
    pub replay: Vec<(u64, Bytes)>,
    pub replay_start_offset: u64,
    pub offsets: Offsets,
    pub gap: Option<(u64, u64)>,
    pub lifecycle: Lifecycle,
    pub label: String,
    pub cols: Option<u32>,
    pub rows: Option<u32>,
    pub term: Option<String>,
}

#[derive(Debug)]
pub enum SubscribeError {
    /// Requested an offset the publisher has not produced. Can happen after a
    /// restart, when a subscriber saw memory-resident bytes that were rolled back to
    /// `durable_offset` (spec §6.2).
    OffsetAhead {
        next_offset: u64,
        durable_offset: u64,
    },
}

/// Work handed to the checkpoint task for one terminal.
pub struct PendingCheckpoint {
    pub terminal_id: Uuid,
    pub chunk_start: u64,
    pub chunk: Vec<u8>,
    pub earliest_offset: u64,
    pub durable_target: u64,
    pub close_reason: Option<String>,
    pub last_activity: DateTime<Utc>,
    pub dirty_age_seconds: f64,
    pub frames: u64,
}

impl TerminalHandle {
    pub fn new_open(row: &TerminalRow, capacity: usize) -> Self {
        Self::from_row(row, capacity, Vec::new())
    }

    /// Rebuild from durable state. `next_offset` starts at `durable_offset`, so any
    /// bytes that were relayed live but never committed are naturally retransmitted
    /// by the publisher after it reconnects (spec §7.2).
    pub fn from_row(row: &TerminalRow, capacity: usize, retained: Vec<u8>) -> Self {
        let ring = Ring::from_retained(capacity, row.earliest_offset, retained);
        let next_offset = ring.end_offset().max(row.durable_offset);
        let lifecycle = match row.state {
            TerminalState::Open => Lifecycle::Open,
            TerminalState::Closed => Lifecycle::Closed(
                row.close_reason
                    .clone()
                    .unwrap_or_else(|| "closed".to_string()),
            ),
        };
        metrics::TERMINALS_RESIDENT.inc();
        metrics::REPLAY_BYTES_RESIDENT.add(ring.len() as i64);

        Self {
            terminal_id: row.terminal_id,
            device_id: row.device_id,
            identity_id: row.identity_id.clone(),
            local_ref: row.local_ref.clone(),
            created_at: row.created_at,
            inner: Mutex::new(TerminalInner {
                ring,
                next_offset,
                durable_offset: row.durable_offset,
                dirty_since: None,
                frames_since_checkpoint: 0,
                lifecycle,
                label: row.label.clone(),
                cols: row.cols,
                rows: row.rows,
                term: row.term.clone(),
                last_activity: row.last_activity_at,
                accepts_input: row.accepts_input,
                next_input_sequence: 1,
                subscribers: Vec::new(),
                next_subscriber_id: 1,
                retired: false,
            }),
            durable_notify: Notify::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, TerminalInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn offsets(&self) -> Offsets {
        let inner = self.lock();
        Offsets {
            earliest_offset: inner.ring.earliest_offset(),
            next_offset: inner.next_offset,
            durable_offset: inner.durable_offset,
        }
    }

    pub fn lifecycle(&self) -> Lifecycle {
        self.lock().lifecycle.clone()
    }

    pub fn is_retired(&self) -> bool {
        self.lock().retired
    }

    pub fn metadata(&self) -> (String, Option<u32>, Option<u32>, Option<String>) {
        let inner = self.lock();
        (
            inner.label.clone(),
            inner.cols,
            inner.rows,
            inner.term.clone(),
        )
    }

    /// Whether the publishing device opted in to input for this terminal.
    pub fn accepts_input(&self) -> bool {
        self.lock().accepts_input
    }

    /// Claim the next relay input sequence. Only called once a frame is about to be
    /// delivered, so the sequence counts delivered frames rather than attempts.
    pub fn next_input_sequence(&self) -> u64 {
        let mut inner = self.lock();
        let sequence = inner.next_input_sequence;
        inner.next_input_sequence += 1;
        inner.last_activity = crate::util::now();
        sequence
    }

    pub fn dirty_bytes(&self) -> u64 {
        let inner = self.lock();
        inner.next_offset.saturating_sub(inner.durable_offset)
    }

    pub fn dirty_since(&self) -> Option<Instant> {
        self.lock().dirty_since
    }

    pub fn subscriber_count(&self) -> usize {
        self.lock().subscribers.len()
    }

    pub fn update_metadata(
        &self,
        label: Option<String>,
        cols: Option<u32>,
        rows: Option<u32>,
        term: Option<String>,
    ) {
        let mut inner = self.lock();
        if let Some(label) = label {
            inner.label = label;
        }
        if cols.is_some() {
            inner.cols = cols;
        }
        if rows.is_some() {
            inner.rows = rows;
        }
        if term.is_some() {
            inner.term = term;
        }
    }

    /// Append one output frame.
    ///
    /// `capacity` comes from the caller's settings snapshot, so a replay-capacity
    /// change takes effect on the next append without restarting anything.
    pub fn append(&self, expected_offset: u64, payload: &Bytes, capacity: usize) -> AppendOutcome {
        let mut inner = self.lock();

        if inner.lifecycle != Lifecycle::Open {
            return AppendOutcome::NotOpen;
        }
        if expected_offset != inner.next_offset {
            metrics::OFFSET_MISMATCHES.inc();
            return AppendOutcome::OffsetMismatch {
                next_offset: inner.next_offset,
                durable_offset: inner.durable_offset,
            };
        }

        let evicted_by_resize = inner.ring.set_capacity(capacity);
        let start_offset = inner.next_offset;

        if payload.is_empty() {
            // Nothing to append; offsets are unchanged and no frame is fanned out.
            return AppendOutcome::Accepted {
                next_offset: inner.next_offset,
                durable_offset: inner.durable_offset,
            };
        }

        let evicted = inner.ring.append(payload) + evicted_by_resize;
        inner.next_offset += payload.len() as u64;
        debug_assert_eq!(inner.next_offset, inner.ring.end_offset());

        if inner.dirty_since.is_none() {
            inner.dirty_since = Some(Instant::now());
        }
        inner.frames_since_checkpoint += 1;
        inner.last_activity = crate::util::now();

        metrics::OUTPUT_BYTES_ACCEPTED.add(payload.len() as u64);
        metrics::REPLAY_BYTES_RESIDENT.add(payload.len() as i64 - evicted as i64);
        if evicted > 0 {
            metrics::EVICTIONS.inc();
            metrics::EVICTED_BYTES.add(evicted as u64);
        }

        // Fan out inside the same critical section, so subscriber order matches
        // append order exactly.
        inner.fan_out(MirrorEvent::Output {
            start_offset,
            payload: payload.clone(),
        });

        AppendOutcome::Accepted {
            next_offset: inner.next_offset,
            durable_offset: inner.durable_offset,
        }
    }

    pub fn resize(&self, cols: u32, rows: u32) -> bool {
        let mut inner = self.lock();
        if inner.lifecycle != Lifecycle::Open {
            return false;
        }
        inner.cols = Some(cols);
        inner.rows = Some(rows);
        inner.last_activity = crate::util::now();
        inner.fan_out(MirrorEvent::Resize { cols, rows });
        true
    }

    /// Register a subscriber and capture its replay range in the same critical
    /// section, which is what guarantees no gap and no duplication at the
    /// replay-to-live boundary (spec §6.2).
    pub fn subscribe(
        &self,
        from_offset: Option<u64>,
        chunk_bytes: usize,
        queue_messages: usize,
        queue_bytes: u64,
    ) -> Result<Subscription, SubscribeError> {
        let mut inner = self.lock();

        let earliest = inner.ring.earliest_offset();
        let next = inner.next_offset;
        let durable = inner.durable_offset;
        let requested = from_offset.unwrap_or(earliest);

        if requested > next {
            metrics::OFFSET_AHEAD_REJECTIONS.inc();
            return Err(SubscribeError::OffsetAhead {
                next_offset: next,
                durable_offset: durable,
            });
        }

        let (replay_start, gap) = if requested < earliest {
            metrics::REPLAY_GAPS.inc();
            (earliest, Some((requested, earliest)))
        } else {
            (requested, None)
        };

        let mut replay = Vec::new();
        let chunk_bytes = chunk_bytes.max(1);
        let mut cursor = replay_start;
        while cursor < next {
            let end = (cursor + chunk_bytes as u64).min(next);
            match inner.ring.read_range(cursor, end) {
                Some(bytes) => replay.push((cursor, Bytes::from(bytes))),
                None => break,
            }
            cursor = end;
        }

        let (tx, receiver) = mpsc::channel(queue_messages.max(1));
        let queued_bytes = Arc::new(AtomicU64::new(0));
        let signal = Arc::new(SubscriberSignal::default());
        let subscriber_id = inner.next_subscriber_id;
        inner.next_subscriber_id += 1;
        inner.subscribers.push(Subscriber {
            id: subscriber_id,
            tx,
            queued_bytes: Arc::clone(&queued_bytes),
            max_queued_bytes: queue_bytes,
            signal: Arc::clone(&signal),
        });

        Ok(Subscription {
            subscriber_id,
            receiver,
            queued_bytes,
            signal,
            replay,
            replay_start_offset: replay_start,
            offsets: Offsets {
                earliest_offset: earliest,
                next_offset: next,
                durable_offset: durable,
            },
            gap,
            lifecycle: inner.lifecycle.clone(),
            label: inner.label.clone(),
            cols: inner.cols,
            rows: inner.rows,
            term: inner.term.clone(),
        })
    }

    pub fn unsubscribe(&self, subscriber_id: u64) {
        let mut inner = self.lock();
        inner.subscribers.retain(|s| s.id != subscriber_id);
    }

    /// Begin closing. Returns false when the terminal is already closing or closed.
    pub fn begin_close(&self, reason: &str) -> bool {
        let mut inner = self.lock();
        if inner.lifecycle != Lifecycle::Open {
            return false;
        }
        inner.lifecycle = Lifecycle::Closing(reason.to_string());
        inner.last_activity = crate::util::now();
        // Wake anyone waiting on durability so they observe the lifecycle change.
        self.durable_notify.notify_waiters();
        true
    }

    /// Snapshot the work a checkpoint must commit for this terminal.
    ///
    /// Returns `None` when there is nothing to do. The dirty range is always
    /// `[durable_offset, next_offset)`, intersected with what is still retained: an
    /// oversized frame can push `earliest_offset` past `durable_offset`, and only
    /// retained bytes can be persisted.
    pub fn take_checkpoint(&self) -> Option<PendingCheckpoint> {
        let inner = self.lock();

        let closing = matches!(inner.lifecycle, Lifecycle::Closing(_));
        let has_dirty = inner.next_offset > inner.durable_offset;
        if !has_dirty && !closing {
            return None;
        }

        let from = inner.durable_offset.max(inner.ring.earliest_offset());
        let to = inner.next_offset;
        let chunk = if to > from {
            inner.ring.read_range(from, to).unwrap_or_default()
        } else {
            Vec::new()
        };

        let close_reason = match &inner.lifecycle {
            Lifecycle::Closing(reason) => Some(reason.clone()),
            _ => None,
        };

        Some(PendingCheckpoint {
            terminal_id: self.terminal_id,
            chunk_start: from,
            chunk,
            earliest_offset: inner.ring.earliest_offset(),
            durable_target: to,
            close_reason,
            last_activity: inner.last_activity,
            dirty_age_seconds: inner
                .dirty_since
                .map(|since| since.elapsed().as_secs_f64())
                .unwrap_or(0.0),
            frames: inner.frames_since_checkpoint,
        })
    }

    /// Publish a committed checkpoint: advance `durable_offset` and tell everyone.
    pub fn commit_durable(&self, durable_target: u64, closed: Option<String>) {
        let mut inner = self.lock();

        if durable_target > inner.durable_offset {
            inner.durable_offset = durable_target;
            inner.fan_out(MirrorEvent::Durable {
                durable_offset: durable_target,
            });
        }
        if inner.durable_offset >= inner.next_offset {
            inner.dirty_since = None;
        }
        inner.frames_since_checkpoint = 0;

        if let Some(reason) = closed {
            inner.lifecycle = Lifecycle::Closed(reason.clone());
            // Subscribers learn of the close only after every accepted byte has been
            // queued ahead of it and committed (spec §6.2).
            let next_offset = inner.next_offset;
            let durable_offset = inner.durable_offset;
            inner.fan_out(MirrorEvent::Closed {
                reason,
                next_offset,
                durable_offset,
            });
            inner.retired = true;
            metrics::TERMINALS_CLOSED_TOTAL.inc();
        }

        drop(inner);
        self.durable_notify.notify_waiters();
    }

    /// Wait until `durable_offset` reaches `target`, the terminal stops being open,
    /// or the deadline passes. Returns the durable offset observed.
    pub async fn wait_for_durable(&self, target: u64, timeout: std::time::Duration) -> u64 {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            {
                let inner = self.lock();
                if inner.durable_offset >= target || inner.lifecycle != Lifecycle::Open {
                    return inner.durable_offset;
                }
            }
            let notified = self.durable_notify.notified();
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return self.lock().durable_offset;
            }
        }
    }

    /// Drop every subscriber with an out-of-band reason (shutdown, revocation).
    pub fn terminate_subscribers(&self, termination: Termination) {
        let mut inner = self.lock();
        for subscriber in inner.subscribers.drain(..) {
            *subscriber
                .signal
                .terminate
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(termination.clone());
            drop(subscriber.tx);
        }
    }

    /// Apply a reduced replay capacity immediately.
    ///
    /// `terminal.replay_capacity_bytes` is declared `Immediate`, and a reduction
    /// bounds memory, so it is applied to resident buffers as soon as the revision is
    /// observed rather than waiting for the next append.
    pub fn shrink_to_capacity(&self, capacity: usize) {
        let mut inner = self.lock();
        if capacity >= inner.ring.capacity() {
            return;
        }
        let evicted = inner.ring.set_capacity(capacity);
        if evicted > 0 {
            metrics::EVICTIONS.inc();
            metrics::EVICTED_BYTES.add(evicted as u64);
            metrics::REPLAY_BYTES_RESIDENT.sub(evicted as i64);
        }
    }

    /// Called when the handle leaves the resident set.
    pub fn on_evict(&self) {
        let inner = self.lock();
        metrics::TERMINALS_RESIDENT.dec();
        metrics::REPLAY_BYTES_RESIDENT.sub(inner.ring.len() as i64);
    }
}

impl TerminalInner {
    /// Queue an event to every subscriber, dropping any that has exceeded its
    /// bound. Non-blocking, so it is safe inside the critical section.
    fn fan_out(&mut self, event: MirrorEvent) {
        let cost = event.queue_cost();
        let mut dropped = Vec::new();

        for subscriber in &self.subscribers {
            let queued = subscriber.queued_bytes.load(Ordering::Acquire);
            if queued + cost > subscriber.max_queued_bytes {
                subscriber.signal.overflowed.store(true, Ordering::Release);
                dropped.push(subscriber.id);
                continue;
            }
            subscriber.queued_bytes.fetch_add(cost, Ordering::AcqRel);
            match subscriber.tx.try_send(event.clone()) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    subscriber.queued_bytes.fetch_sub(cost, Ordering::AcqRel);
                    subscriber.signal.overflowed.store(true, Ordering::Release);
                    dropped.push(subscriber.id);
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    subscriber.queued_bytes.fetch_sub(cost, Ordering::AcqRel);
                    dropped.push(subscriber.id);
                }
            }
        }

        if !dropped.is_empty() {
            // Dropping the sender is what wakes the subscriber task, which then reads
            // the signal and closes with the right code.
            self.subscribers.retain(|s| !dropped.contains(&s.id));
        }
    }
}

/// Build the `subscribed` control message for a subscription.
///
/// `input` is `None` for a version 1 subscriber, which never sees the input fields,
/// and `Some((accepts_input, input_available))` for version 2 (spec §6.2).
pub fn subscribed_message(
    terminal_id: Uuid,
    requested: u64,
    sub: &Subscription,
    input: Option<(bool, bool)>,
) -> ServerMessage {
    ServerMessage::Subscribed {
        terminal_id,
        requested_from_offset: requested,
        replay_start_offset: sub.replay_start_offset,
        next_offset: sub.offsets.next_offset,
        durable_offset: sub.offsets.durable_offset,
        earliest_offset: sub.offsets.earliest_offset,
        terminal_state: sub.lifecycle.as_str(),
        label: sub.label.clone(),
        cols: sub.cols,
        rows: sub.rows,
        term: sub.term.clone(),
        accepts_input: input.map(|(accepts, _)| accepts),
        input_available: input.map(|(_, available)| available),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::repo::TerminalState;

    fn row() -> TerminalRow {
        TerminalRow {
            terminal_id: Uuid::new_v4(),
            device_id: Uuid::new_v4(),
            identity_id: "identity".to_string(),
            label: "shell".to_string(),
            local_ref: "pty0".to_string(),
            state: TerminalState::Open,
            cols: Some(80),
            rows: Some(24),
            term: Some("xterm-256color".to_string()),
            process_label: None,
            accepts_input: false,
            created_at: crate::util::now(),
            last_activity_at: crate::util::now(),
            closed_at: None,
            close_reason: None,
            durable_offset: 0,
            earliest_offset: 0,
        }
    }

    #[test]
    fn append_requires_the_authoritative_offset() {
        let handle = TerminalHandle::new_open(&row(), 1024);
        assert!(matches!(
            handle.append(0, &Bytes::from_static(b"abc"), 1024),
            AppendOutcome::Accepted {
                next_offset: 3,
                durable_offset: 0
            }
        ));
        // A stale or skipped offset changes nothing.
        assert!(matches!(
            handle.append(2, &Bytes::from_static(b"zz"), 1024),
            AppendOutcome::OffsetMismatch {
                next_offset: 3,
                durable_offset: 0
            }
        ));
        assert!(matches!(
            handle.append(9, &Bytes::from_static(b"zz"), 1024),
            AppendOutcome::OffsetMismatch { next_offset: 3, .. }
        ));
        assert_eq!(handle.offsets().next_offset, 3);
    }

    #[tokio::test]
    async fn subscriber_sees_replay_then_live_without_a_seam() {
        let handle = TerminalHandle::new_open(&row(), 1024);
        handle.append(0, &Bytes::from_static(b"first"), 1024);

        let mut sub = handle.subscribe(None, 1024, 16, 1 << 20).unwrap();
        assert_eq!(sub.replay.len(), 1);
        assert_eq!(sub.replay[0], (0, Bytes::from_static(b"first")));

        handle.append(5, &Bytes::from_static(b"second"), 1024);
        let event = sub.receiver.recv().await.unwrap();
        match event {
            MirrorEvent::Output {
                start_offset,
                payload,
            } => {
                assert_eq!(start_offset, 5);
                assert_eq!(payload.as_ref(), b"second");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn subscribing_below_the_window_reports_a_gap() {
        let handle = TerminalHandle::new_open(&row(), 8);
        handle.append(0, &Bytes::from_static(b"0123456789"), 8);
        let sub = handle.subscribe(Some(0), 1024, 16, 1 << 20).unwrap();
        assert_eq!(sub.gap, Some((0, 2)));
        assert_eq!(sub.replay_start_offset, 2);
        assert_eq!(sub.offsets.retained_bytes(), 8);
    }

    #[test]
    fn subscribing_beyond_next_offset_fails() {
        let handle = TerminalHandle::new_open(&row(), 1024);
        handle.append(0, &Bytes::from_static(b"abc"), 1024);
        assert!(matches!(
            handle.subscribe(Some(4), 1024, 16, 1 << 20),
            Err(SubscribeError::OffsetAhead {
                next_offset: 3,
                durable_offset: 0
            })
        ));
        // Exactly next_offset is allowed and yields an empty replay.
        let sub = handle.subscribe(Some(3), 1024, 16, 1 << 20).unwrap();
        assert!(sub.replay.is_empty());
    }

    #[tokio::test]
    async fn slow_subscriber_is_dropped_with_a_reason() {
        let handle = TerminalHandle::new_open(&row(), 1 << 20);
        // A 16-byte queue bound cannot hold a 32-byte frame.
        let sub = handle.subscribe(None, 1024, 4, 16).unwrap();
        handle.append(0, &Bytes::from(vec![b'x'; 32]), 1 << 20);
        assert!(sub.signal.overflowed.load(Ordering::Acquire));
        assert_eq!(handle.subscriber_count(), 0);
    }

    #[test]
    fn checkpoint_covers_the_dirty_range_then_clears() {
        let handle = TerminalHandle::new_open(&row(), 1024);
        assert!(handle.take_checkpoint().is_none());

        handle.append(0, &Bytes::from_static(b"hello"), 1024);
        let checkpoint = handle.take_checkpoint().unwrap();
        assert_eq!(checkpoint.chunk_start, 0);
        assert_eq!(checkpoint.chunk, b"hello");
        assert_eq!(checkpoint.durable_target, 5);

        handle.commit_durable(5, None);
        assert_eq!(handle.offsets().durable_offset, 5);
        assert_eq!(handle.dirty_bytes(), 0);
        assert!(handle.take_checkpoint().is_none());
    }

    #[test]
    fn checkpoint_after_an_oversized_frame_persists_only_retained_bytes() {
        let handle = TerminalHandle::new_open(&row(), 4);
        handle.append(0, &Bytes::from_static(b"0123456789"), 4);
        let checkpoint = handle.take_checkpoint().unwrap();
        // Only the newest retained suffix can be committed; offsets still advanced
        // by the frame's full length.
        assert_eq!(checkpoint.chunk_start, 6);
        assert_eq!(checkpoint.chunk, b"6789");
        assert_eq!(checkpoint.durable_target, 10);
        assert_eq!(checkpoint.earliest_offset, 6);
    }

    #[tokio::test]
    async fn close_is_announced_after_the_final_commit() {
        let handle = TerminalHandle::new_open(&row(), 1024);
        handle.append(0, &Bytes::from_static(b"bye"), 1024);
        let mut sub = handle.subscribe(Some(3), 1024, 16, 1 << 20).unwrap();

        assert!(handle.begin_close("process_exited"));
        // Still not visible to subscribers until the checkpoint commits.
        assert!(sub.receiver.try_recv().is_err());

        let checkpoint = handle.take_checkpoint().unwrap();
        assert_eq!(checkpoint.close_reason.as_deref(), Some("process_exited"));
        handle.commit_durable(3, checkpoint.close_reason);

        let mut saw_durable = false;
        loop {
            match sub.receiver.recv().await.unwrap() {
                MirrorEvent::Durable { durable_offset } => {
                    assert_eq!(durable_offset, 3);
                    saw_durable = true;
                }
                MirrorEvent::Closed {
                    reason,
                    next_offset,
                    durable_offset,
                } => {
                    assert_eq!(reason, "process_exited");
                    assert_eq!(next_offset, 3);
                    assert_eq!(durable_offset, 3);
                    break;
                }
                other => panic!("unexpected event: {other:?}"),
            }
        }
        assert!(saw_durable);
        assert!(handle.is_retired());
        assert!(matches!(
            handle.append(3, &Bytes::from_static(b"x"), 1024),
            AppendOutcome::NotOpen
        ));
    }

    #[test]
    fn restart_rolls_next_offset_back_to_durable() {
        let mut r = row();
        r.durable_offset = 10;
        r.earliest_offset = 4;
        let handle = TerminalHandle::from_row(&r, 1024, b"456789".to_vec());
        let offsets = handle.offsets();
        assert_eq!(offsets.durable_offset, 10);
        assert_eq!(offsets.next_offset, 10);
        assert_eq!(offsets.earliest_offset, 4);
        // The publisher resumes at 10 and retransmits anything above it.
        assert!(matches!(
            handle.append(10, &Bytes::from_static(b"ab"), 1024),
            AppendOutcome::Accepted {
                next_offset: 12,
                ..
            }
        ));
    }
}
