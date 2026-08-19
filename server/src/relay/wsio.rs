//! Shared WebSocket plumbing: a single writer task per connection, plus heartbeat
//! bookkeeping.
//!
//! Both protocols write from several places at once — control replies, relayed
//! output, durability notices, acknowledgements and pings — so all writes funnel
//! through one task. That keeps frame order deterministic and means no code path
//! needs a lock on the sink.

use super::messages::ServerMessage;
use axum::extract::ws::{CloseFrame, Message, WebSocket};
use bytes::Bytes;
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

pub enum WsOut {
    Text(String),
    Binary(Bytes),
    Ping,
    /// Sends a close frame and ends the writer task.
    Close {
        code: u16,
        reason: String,
    },
}

#[derive(Clone)]
pub struct WsWriter {
    tx: mpsc::Sender<WsOut>,
}

impl WsWriter {
    /// Queue a control message. Returns false when the connection is already gone.
    pub async fn send(&self, message: &ServerMessage) -> bool {
        self.tx.send(WsOut::Text(message.to_text())).await.is_ok()
    }

    pub async fn send_binary(&self, bytes: Bytes) -> bool {
        self.tx.send(WsOut::Binary(bytes)).await.is_ok()
    }

    pub async fn ping(&self) -> bool {
        self.tx.send(WsOut::Ping).await.is_ok()
    }

    /// Send an error control message and then close, which is the sequence the
    /// specification requires for protocol and application failures (spec §6).
    pub async fn fail(&self, message: &ServerMessage, code: u16, reason: &str) {
        self.send(message).await;
        self.close(code, reason).await;
    }

    pub async fn close(&self, code: u16, reason: &str) {
        let _ = self
            .tx
            .send(WsOut::Close {
                code,
                reason: reason.to_string(),
            })
            .await;
    }
}

pub fn spawn_writer(
    mut sink: SplitSink<WebSocket, Message>,
    queue: usize,
) -> (WsWriter, JoinHandle<()>) {
    let (tx, mut rx) = mpsc::channel::<WsOut>(queue.max(1));
    let handle = tokio::spawn(async move {
        while let Some(out) = rx.recv().await {
            let result = match out {
                WsOut::Text(text) => sink.send(Message::Text(text.into())).await,
                WsOut::Binary(bytes) => sink.send(Message::Binary(bytes)).await,
                WsOut::Ping => sink.send(Message::Ping(Bytes::new())).await,
                WsOut::Close { code, reason } => {
                    let _ = sink
                        .send(Message::Close(Some(CloseFrame {
                            code,
                            reason: reason.into(),
                        })))
                        .await;
                    let _ = sink.flush().await;
                    return;
                }
            };
            if result.is_err() {
                return;
            }
        }
        let _ = sink.flush().await;
    });
    (WsWriter { tx }, handle)
}

pub fn split(
    socket: WebSocket,
) -> (
    SplitSink<WebSocket, Message>,
    futures_util::stream::SplitStream<WebSocket>,
) {
    socket.split()
}

/// Tracks liveness for the ping/pong heartbeat both protocols require (spec §6).
pub struct Heartbeat {
    last_seen: Instant,
    interval: Duration,
    timeout: Duration,
}

impl Heartbeat {
    pub fn new(interval: Duration, timeout: Duration) -> Self {
        Self {
            last_seen: Instant::now(),
            interval,
            timeout,
        }
    }

    pub fn touch(&mut self) {
        self.last_seen = Instant::now();
    }

    pub fn interval(&self) -> Duration {
        self.interval
    }

    pub fn expired(&self) -> bool {
        self.last_seen.elapsed() > self.timeout
    }

    /// Re-read the heartbeat settings. Both are declared
    /// `ConnectionRenegotiate`, so a live connection keeps its negotiated cadence
    /// unless the new value is shorter, which is applied at once.
    pub fn tighten(&mut self, interval: Duration, timeout: Duration) {
        self.interval = self.interval.min(interval);
        self.timeout = self.timeout.min(timeout);
    }
}
