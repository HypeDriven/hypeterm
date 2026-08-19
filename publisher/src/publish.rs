//! Publishing one hosted terminal, from the side that owns its bytes.
//!
//! Between the pseudo-terminal and whatever carries frames to the relay sits exactly
//! one piece of state that must not be lost: this terminal's offset and the bytes it
//! has not yet had acknowledged (`crate::stream`). That state lives here, next to the
//! shell, so it survives anything that can happen to the connection — including the
//! multiplexing daemon being restarted underneath a running shell.
//!
//! The transport is deliberately abstract: the same driver runs whether the frames go
//! straight out of this process to the relay or through a local socket to a daemon
//! that is also carrying half a dozen other terminals.

use tokio::sync::mpsc;

use crate::session::{Event, Limits};
use crate::stream::{Outgoing, Resync};

/// What the driver asks of the transport.
#[derive(Debug)]
pub enum Request {
    Output { start_offset: u64, bytes: Vec<u8> },
    Resize { cols: u16, rows: u16 },
    Close { reason: String },
}

/// What the terminal's owner — the process with the window and the keyboard — needs
/// to hear about.
#[derive(Debug)]
pub enum Notice {
    /// Keystrokes to write into the pseudo-terminal.
    Input(Vec<u8>),
    /// A size a subscriber asked for. The owner decides (spec §6.5).
    Resize { cols: u16, rows: u16 },
    /// Mirroring has ended, with a reason worth showing a person. The shell is not
    /// affected: losing the mirror is not a reason to take away a terminal someone is
    /// working in.
    Ended(String),
}

/// Room for keystrokes and lifecycle notices while the owner is busy.
///
/// Generous because the alternative to buffering is losing input, and small enough
/// that an owner which has genuinely stopped reading is noticed rather than fed for
/// ever.
const NOTICE_QUEUE: usize = 1024;
const CHUNK_QUEUE: usize = 8;

/// The owner's handle on its mirror.
pub struct Mirror {
    /// One sender for both output and resizes, and `Option` so that dropping it is
    /// what tells the driver there is no more. A second clone kept for resizes would
    /// hold the channel open after `begin_shutdown`, and the driver would wait for an
    /// end that never came — so the terminal would never be closed at the relay.
    owner: Option<mpsc::Sender<FromOwner>>,
    pub notices: mpsc::Receiver<Notice>,
}

enum FromOwner {
    Output(Vec<u8>),
    Resize { cols: u16, rows: u16 },
}

impl Mirror {
    /// Hands `chunk` to the mirror. Returns false once mirroring has ended.
    ///
    /// Awaiting is the back pressure that keeps the byte stream honest: when the relay
    /// is behind, the shell writing into the pseudo-terminal waits. Dropping bytes
    /// instead would not merely lose a line — every later offset would be out of step,
    /// permanently, for every subscriber.
    pub async fn publish(&self, chunk: Vec<u8>) -> bool {
        let Some(owner) = &self.owner else {
            return false;
        };
        owner.send(FromOwner::Output(chunk)).await.is_ok()
    }

    /// Declares the terminal's authoritative size (spec §6.5).
    pub async fn resize(&self, cols: u16, rows: u16) -> bool {
        let Some(owner) = &self.owner else {
            return false;
        };
        owner.send(FromOwner::Resize { cols, rows }).await.is_ok()
    }

    /// Stops publishing. The driver closes the terminal at the relay and then reports
    /// `Notice::Ended`, which the caller should wait for: a terminal that is merely
    /// abandoned stays open for the relay's whole reconnect grace period, and anything
    /// typed into it in the meantime goes nowhere.
    pub fn begin_shutdown(&mut self) {
        self.owner = None;
    }
}

/// Runs the stream for one terminal over `requests`/`events`.
pub fn start(requests: mpsc::Sender<Request>, events: mpsc::Receiver<Event>) -> Mirror {
    let (owner_tx, owner_rx) = mpsc::channel::<FromOwner>(CHUNK_QUEUE);
    let (notice_tx, notice_rx) = mpsc::channel::<Notice>(NOTICE_QUEUE);

    tokio::spawn(drive(owner_rx, requests, events, notice_tx));

    Mirror {
        owner: Some(owner_tx),
        notices: notice_rx,
    }
}

/// Drives a terminal that this process publishes itself, with no daemon in between.
///
/// The relay connection is in this process, so the two halves still go in separate
/// tasks: `publish` waits when the relay is behind, and keystrokes must keep arriving
/// while it does.
pub fn direct(
    terminal: crate::session::Terminal,
) -> (mpsc::Sender<Request>, mpsc::Receiver<Event>) {
    let (mut sink, events) = terminal.split();
    let (request_tx, mut requests) = mpsc::channel::<Request>(64);
    tokio::spawn(async move {
        while let Some(request) = requests.recv().await {
            match request {
                Request::Output {
                    start_offset,
                    bytes,
                } => {
                    if !sink.publish(start_offset, bytes).await {
                        break;
                    }
                }
                Request::Resize { cols, rows } => {
                    sink.resize(cols, rows).await;
                }
                Request::Close { .. } => sink.begin_shutdown(),
            }
        }
    });
    (request_tx, events)
}

async fn drive(
    mut owner: mpsc::Receiver<FromOwner>,
    requests: mpsc::Sender<Request>,
    mut events: mpsc::Receiver<Event>,
    notices: mpsc::Sender<Notice>,
) {
    let mut stream = Outgoing::new();
    let mut max_frame = 64 * 1024usize;
    let mut owner_gone = false;
    // Latched once the close has gone out. Without it the arm below would keep firing
    // — a closed channel yields `None` immediately, for ever — and the loop would spin
    // sending closes instead of waiting for the answer to the first one.
    let mut closed = false;
    // One chunk taken from the owner before the relay said where the stream is. Held
    // rather than left in the queue so that the queue can still deliver the *end* of
    // it: a shell that exits while the relay is unreachable has to be noticed, or its
    // terminal is never closed.
    let mut waiting: Option<Vec<u8>> = None;

    loop {
        if let Some(chunk) = waiting.take() {
            if stream.may_send() {
                match stream.send(&chunk) {
                    Some(start) => {
                        if !send_output(&requests, start, chunk, max_frame).await {
                            return;
                        }
                    }
                    None => waiting = Some(chunk),
                }
            } else {
                waiting = Some(chunk);
            }
        }

        if owner_gone && !closed && waiting.is_none() && stream.unacked() == 0 {
            closed = true;
            let _ = requests
                .send(Request::Close {
                    reason: "process_exited".to_string(),
                })
                .await;
            // The transport answers with `Ended`, which is what the owner waits for.
        }

        tokio::select! {
            // Not biased: a shell producing output without pause must not be able to
            // starve the arm that carries keystrokes towards it.
            event = events.recv() => {
                let Some(event) = event else {
                    let _ = notices.try_send(Notice::Ended("mirroring stopped".into()));
                    return;
                };
                match event {
                    Event::Attached { next_offset, limits, .. } => {
                        apply_limits(&mut stream, &mut max_frame, limits);
                        match stream.attached(next_offset) {
                            Resync::Retransmit { start, bytes } => {
                                tracing::info!(
                                    bytes = bytes.len(),
                                    "retransmitting output the relay had not committed"
                                );
                                if !send_output(&requests, start, bytes, max_frame).await {
                                    return;
                                }
                            }
                            Resync::Nothing => {}
                            Resync::Unrecoverable => {
                                unrecoverable(&requests, &notices).await;
                                return;
                            }
                        }
                    }
                    Event::Ack { durable_offset } => stream.acknowledge(durable_offset),
                    Event::Mismatch { next_offset } => match stream.mismatch(next_offset) {
                        Resync::Retransmit { start, bytes } => {
                            tracing::warn!(offset = start, "resynchronising after an offset mismatch");
                            if !send_output(&requests, start, bytes, max_frame).await {
                                return;
                            }
                        }
                        Resync::Nothing => {}
                        Resync::Unrecoverable => {
                            unrecoverable(&requests, &notices).await;
                            return;
                        }
                    },
                    Event::Detached => stream.detached(),
                    Event::Input(bytes) => {
                        if notices.try_send(Notice::Input(bytes)).is_err() {
                            tracing::error!("the terminal stopped reading its input");
                            return;
                        }
                    }
                    Event::ResizeRequest { cols, rows } => {
                        let _ = notices.try_send(Notice::Resize { cols, rows });
                    }
                    Event::Ended(reason) => {
                        let _ = notices.try_send(Notice::Ended(reason));
                        return;
                    }
                }
            }

            // Polled whatever the stream's state, so that a shell ending while the
            // relay is unreachable is still noticed. What the stream's state decides is
            // whether the chunk can go out now or has to wait in `waiting` — output is
            // never drawn past the relay's unacknowledged window, because overrunning
            // it closes the whole connection, and on a shared connection that is
            // everybody's terminal rather than just this one.
            from_owner = owner.recv(), if !owner_gone && waiting.is_none() => {
                match from_owner {
                    Some(FromOwner::Output(chunk)) => {
                        if stream.may_send() {
                            let Some(start) = stream.send(&chunk) else {
                                waiting = Some(chunk);
                                continue;
                            };
                            if !send_output(&requests, start, chunk, max_frame).await {
                                return;
                            }
                        } else {
                            waiting = Some(chunk);
                        }
                    }
                    Some(FromOwner::Resize { cols, rows }) => {
                        if requests.send(Request::Resize { cols, rows }).await.is_err() {
                            return;
                        }
                    }
                    None => owner_gone = true,
                }
            }
        }
    }
}

fn apply_limits(stream: &mut Outgoing, max_frame: &mut usize, limits: Limits) {
    stream.set_window(limits.max_unacked_output_bytes);
    *max_frame = limits.max_output_frame_bytes.max(1024) as usize;
}

async fn send_output(
    requests: &mpsc::Sender<Request>,
    start: u64,
    bytes: Vec<u8>,
    max_frame: usize,
) -> bool {
    let mut offset = start;
    for slice in bytes.chunks(max_frame) {
        if requests
            .send(Request::Output {
                start_offset: offset,
                bytes: slice.to_vec(),
            })
            .await
            .is_err()
        {
            return false;
        }
        offset += slice.len() as u64;
    }
    true
}

/// The relay wants bytes this side no longer has. Splicing the stream would be
/// undetectable downstream, so the terminal is closed instead and the person told.
async fn unrecoverable(requests: &mpsc::Sender<Request>, notices: &mpsc::Sender<Notice>) {
    tracing::error!("the relay asked to resume from output that is no longer retained");
    let _ = requests
        .send(Request::Close {
            reason: "publisher_desynchronised".to_string(),
        })
        .await;
    let _ = notices.try_send(Notice::Ended(
        "the mirror lost its place in this terminal's output and stopped rather than \
         show a gap"
            .into(),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> Limits {
        Limits {
            // The relay's schema floor for a frame is 1024 bytes, and the driver holds
            // to it, so a test that asks for less is testing nothing real.
            max_output_frame_bytes: 1024,
            max_unacked_output_bytes: 64 * 1024,
        }
    }

    async fn next_request(rx: &mut mpsc::Receiver<Request>) -> Request {
        tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("a request within two seconds")
            .expect("the driver is still running")
    }

    #[tokio::test]
    async fn output_waits_for_the_relay_to_say_where_the_stream_is() {
        let (request_tx, mut requests) = mpsc::channel(16);
        let (event_tx, events) = mpsc::channel(16);
        let mirror = start(request_tx, events);

        // Published before the relay has attached anything: nothing may be framed yet,
        // because a guessed start offset is refused rather than merged.
        assert!(mirror.publish(b"early".to_vec()).await);
        tokio::task::yield_now().await;
        assert!(requests.try_recv().is_err());

        event_tx
            .send(Event::Attached {
                terminal_id: uuid::Uuid::from_u128(1),
                next_offset: 0,
                limits: limits(),
            })
            .await
            .unwrap();

        // It waited rather than being dropped: the shell wrote it, so the mirror owes
        // it to every subscriber, and it belongs at the front of the stream.
        match next_request(&mut requests).await {
            Request::Output {
                start_offset,
                bytes,
            } => {
                assert_eq!(start_offset, 0);
                assert_eq!(bytes, b"early");
            }
            other => panic!("expected the queued output, got {other:?}"),
        }

        assert!(mirror.publish(b"later".to_vec()).await);
        match next_request(&mut requests).await {
            Request::Output {
                start_offset,
                bytes,
            } => {
                assert_eq!(start_offset, 5);
                assert_eq!(bytes, b"later");
            }
            other => panic!("expected output, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn frames_are_split_to_the_relays_limit() {
        let (request_tx, mut requests) = mpsc::channel(16);
        let (event_tx, events) = mpsc::channel(16);
        let mirror = start(request_tx, events);
        event_tx
            .send(Event::Attached {
                terminal_id: uuid::Uuid::from_u128(1),
                next_offset: 0,
                limits: limits(),
            })
            .await
            .unwrap();

        let payload: Vec<u8> = (0..1500u32).map(|i| (i % 251) as u8).collect();
        mirror.publish(payload.clone()).await;

        // Offsets must stay contiguous across the split, or every frame after the
        // first is refused for a mismatch.
        match next_request(&mut requests).await {
            Request::Output {
                start_offset,
                bytes,
            } => {
                assert_eq!(start_offset, 0);
                assert_eq!(bytes, payload[..1024]);
            }
            other => panic!("expected output, got {other:?}"),
        }
        match next_request(&mut requests).await {
            Request::Output {
                start_offset,
                bytes,
            } => {
                assert_eq!(start_offset, 1024);
                assert_eq!(bytes, payload[1024..]);
            }
            other => panic!("expected output, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_reconnect_resends_what_was_never_committed() {
        let (request_tx, mut requests) = mpsc::channel(16);
        let (event_tx, events) = mpsc::channel(16);
        let mirror = start(request_tx, events);
        let attach = |offset| Event::Attached {
            terminal_id: uuid::Uuid::from_u128(1),
            next_offset: offset,
            limits: Limits {
                max_output_frame_bytes: 1024,
                max_unacked_output_bytes: 64 * 1024,
            },
        };

        event_tx.send(attach(0)).await.unwrap();
        mirror.publish(b"hello world".to_vec()).await;
        assert!(matches!(
            next_request(&mut requests).await,
            Request::Output { .. }
        ));

        event_tx
            .send(Event::Ack { durable_offset: 6 })
            .await
            .unwrap();
        event_tx.send(Event::Detached).await.unwrap();
        // The relay restarted and fell back to what it had committed. The bytes are
        // here, in the process that owns the shell, which is the point of holding them
        // here rather than wherever the connection happens to live.
        event_tx.send(attach(6)).await.unwrap();

        match next_request(&mut requests).await {
            Request::Output {
                start_offset,
                bytes,
            } => {
                assert_eq!(start_offset, 6);
                assert_eq!(bytes, b"world");
            }
            other => panic!("expected a retransmission, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn input_reaches_the_owner() {
        let (request_tx, _requests) = mpsc::channel(16);
        let (event_tx, events) = mpsc::channel(16);
        let mut mirror = start(request_tx, events);
        event_tx.send(Event::Input(b"ls\r".to_vec())).await.unwrap();
        let notice = tokio::time::timeout(std::time::Duration::from_secs(2), mirror.notices.recv())
            .await
            .expect("a notice")
            .expect("still running");
        assert!(matches!(notice, Notice::Input(bytes) if bytes == b"ls\r"));
    }

    #[tokio::test]
    async fn a_terminal_that_ends_is_closed_once_and_only_once() {
        let (request_tx, mut requests) = mpsc::channel(64);
        let (event_tx, events) = mpsc::channel(16);
        let mirror = start(request_tx, events);
        event_tx
            .send(Event::Attached {
                terminal_id: uuid::Uuid::from_u128(1),
                next_offset: 0,
                limits: limits(),
            })
            .await
            .unwrap();
        mirror.publish(b"bye".to_vec()).await;
        assert!(matches!(
            next_request(&mut requests).await,
            Request::Output { .. }
        ));
        event_tx
            .send(Event::Ack { durable_offset: 3 })
            .await
            .unwrap();

        // The shell is gone.
        drop(mirror);
        match next_request(&mut requests).await {
            Request::Close { reason } => assert_eq!(reason, "process_exited"),
            other => panic!("expected a close, got {other:?}"),
        }

        // Exactly one. A closed channel answers `None` the instant it is polled, so a
        // driver that did not latch this would sit in a tight loop sending closes.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            requests.try_recv().is_err(),
            "the close was sent more than once"
        );
    }

    #[tokio::test]
    async fn losing_the_place_in_the_stream_closes_the_terminal_rather_than_splicing() {
        let (request_tx, mut requests) = mpsc::channel(16);
        let (event_tx, events) = mpsc::channel(16);
        let mut mirror = start(request_tx, events);
        event_tx
            .send(Event::Attached {
                terminal_id: uuid::Uuid::from_u128(1),
                next_offset: 0,
                limits: Limits {
                    max_output_frame_bytes: 1024,
                    max_unacked_output_bytes: 64 * 1024,
                },
            })
            .await
            .unwrap();
        mirror.publish(b"0123456789".to_vec()).await;
        assert!(matches!(
            next_request(&mut requests).await,
            Request::Output { .. }
        ));
        event_tx
            .send(Event::Ack { durable_offset: 9 })
            .await
            .unwrap();
        tokio::task::yield_now().await;

        // The relay asks to resume from before what has been released. A gap here
        // would be invisible to every subscriber for ever, so the terminal ends.
        event_tx
            .send(Event::Mismatch { next_offset: 2 })
            .await
            .unwrap();

        match next_request(&mut requests).await {
            Request::Close { reason } => assert_eq!(reason, "publisher_desynchronised"),
            other => panic!("expected a close, got {other:?}"),
        }
        let notice = tokio::time::timeout(std::time::Duration::from_secs(2), mirror.notices.recv())
            .await
            .expect("a notice")
            .expect("still running");
        assert!(matches!(notice, Notice::Ended(_)));
    }
}
