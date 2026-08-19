//! The publisher's relay connection (relay spec §6.1, §6.3, §6.5).
//!
//! One connection, many terminals. A device may hold only one publisher connection
//! (spec §6.1) — a second takes the device over from the first — but that connection
//! is built to carry many: every publisher frame names a terminal by UUID precisely so
//! it can. This module is the multiplexer, and it is deliberately thin. It owns the
//! socket, the terminal-id routing table and the retry loop, and it owns **no byte
//! state at all**: offsets and retained bytes belong to whatever owns the
//! pseudo-terminal (see `crate::stream`), so that this process dying can interrupt a
//! mirror but can never put a hole in one.
//!
//! Two rules shape the loop. Nothing that arrives for one terminal may disturb
//! another — an acknowledgement, a refusal, a close and a resize all name a terminal,
//! and all are routed by it. And the reader must never wait on the writer: the relay
//! answers acknowledgements, input and pings on the same socket, so a loop that parked
//! inside `sink.send` would stall the very messages that unblock it.

use futures_util::{SinkExt as _, StreamExt as _};
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::Message;
use uuid::Uuid;

use crate::api;
use crate::crypto::KeyPair;
use crate::protocol::{
    SUBPROTOCOL_V2, ServerMessage, decode_input_frame, encode_output_frame, parse_server_message,
};

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("{0}")]
    Connect(String),
    #[error("the relay refused the publisher connection: {0}")]
    Refused(String),
    #[error("the relay closed the connection: {0}")]
    Closed(String),
    #[error("{0}")]
    Api(#[from] api::ApiError),
}

type Result<T> = std::result::Result<T, SessionError>;

pub struct Config {
    pub relay_url: String,
    pub device_id: String,
    /// The identity key, used only to list what this device left open so it can be
    /// tidied up. `None` means "do not tidy up", which is always safe.
    pub identity_key: Option<KeyPair>,
    /// Whether this machine's owner has allowed a paired subscriber to ask it to open a
    /// terminal (relay spec §4.6). Off unless somebody sitting here turned it on, and
    /// asserted afresh on every connection so the relay can never hold a stale yes.
    pub allow_remote_open: bool,
}

/// One terminal joining the connection.
#[derive(Clone, Debug)]
pub struct TerminalSpec {
    /// Stable for the life of the hosted shell, across relay reconnects: it is what
    /// lets the relay recognise a reopened terminal as the same one rather than
    /// creating a second (spec §6.1). Minted fresh per hosted shell and never derived
    /// from anything reusable — two shells sharing a `local_ref` would be deduplicated
    /// onto one offset stream and interleave, byte by byte.
    pub local_ref: String,
    pub label: String,
    pub cols: u16,
    pub rows: u16,
    pub term: String,
    /// The `request_id` of a `terminal.open_request` this terminal answers, if it
    /// answers one (relay spec §4.6). One-shot: echoed on the first `terminal.open`
    /// and never on a reconnect re-open, or a stale request would be answered twice.
    pub in_reply_to: Option<String>,
}

/// The relay's limits for this connection, as advertised in `ready`.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_output_frame_bytes: u64,
    pub max_unacked_output_bytes: u64,
}

/// What the relay says about one terminal.
#[derive(Debug)]
pub enum Event {
    /// The relay has this terminal open, and its stream continues at `next_offset`.
    /// Sent on the first open and again after every reconnect, because the relay is
    /// the authority on where the stream is and its answer can move backwards to
    /// whatever it committed (spec §6.1).
    Attached {
        terminal_id: Uuid,
        next_offset: u64,
        limits: Limits,
    },
    /// Bytes the relay has committed; everything below this can be forgotten.
    Ack { durable_offset: u64 },
    /// Keystrokes for the pseudo-terminal.
    Input(Vec<u8>),
    /// A size a subscriber would like. The publisher owns the terminal and decides
    /// (spec §6.5); whoever owns the window makes that call, not this module.
    ResizeRequest { cols: u16, rows: u16 },
    /// The relay refused a frame and says the stream is here instead.
    Mismatch { next_offset: u64 },
    /// The connection is gone. Nothing may be framed until the next `Attached`.
    Detached,
    /// This terminal is no longer mirrored, with a reason worth showing a person.
    Ended(String),
}

/// What the local side asks of the relay connection.
enum Control {
    Open {
        spec: TerminalSpec,
        events: mpsc::Sender<Event>,
    },
    Output {
        local_ref: String,
        start_offset: u64,
        bytes: Vec<u8>,
    },
    /// The authoritative size, which only the publisher may declare (spec §6.5).
    Resize {
        local_ref: String,
        cols: u16,
        rows: u16,
    },
    Close {
        local_ref: String,
        reason: String,
    },
    /// Refuse a terminal-open request (relay spec §4.6).
    Decline {
        request_id: String,
        reason: &'static str,
    },
}

/// A subscriber asking this machine to open a terminal (relay spec §4.6).
///
/// Carried to whoever can actually host one. Nothing here decides that it *will* be
/// honoured: every check that matters happens in the process that would spawn.
#[derive(Clone, Debug)]
pub struct OpenRequest {
    pub request_id: String,
    pub label: Option<String>,
    pub cols: Option<u16>,
    pub rows: Option<u16>,
}

/// Messages queued for the socket. The writer owns the sink so the reader never has to
/// wait for it.
const WRITER_QUEUE: usize = 256;
/// Slots the reader keeps back for its own replies — pongs above all, since a dropped
/// pong is a heartbeat timeout and a dropped connection.
const WRITER_RESERVE: usize = 32;

/// How long the orphan sweep waits after a connection comes up before deciding what
/// has nothing behind it.
///
/// Not zero, and this is the whole reason the constant exists. When a daemon is
/// replaced, its clients reattach one by one over the following moment; a sweep that
/// ran the instant the socket came up would find their terminals still open, decide
/// nothing was behind them, and close the very terminals it was about to reattach —
/// each shell would then reappear as a *new* terminal at offset zero, with its
/// scrollback gone and a second row on the phone. Waiting is also cheap: an orphan
/// lingering a few seconds costs nothing, and the relay closes it anyway once its own
/// reconnect grace expires.
const ORPHAN_SETTLE: Duration = Duration::from_secs(5);

/// A handle on the connection, from which terminals are opened.
#[derive(Clone)]
pub struct Link {
    control: mpsc::Sender<Control>,
}

impl Link {
    /// Registers a terminal on this connection.
    ///
    /// Returns immediately and does not wait for the relay: the hosted shell must
    /// start whether or not the relay is reachable, and `Event::Attached` arrives when
    /// it is. `None` once the connection has ended for good.
    pub async fn open(&self, spec: TerminalSpec) -> Option<Terminal> {
        let local_ref = spec.local_ref.clone();
        let (event_tx, event_rx) = mpsc::channel::<Event>(256);
        self.control
            .send(Control::Open {
                spec,
                events: event_tx,
            })
            .await
            .ok()?;

        Some(Terminal {
            sink: TerminalSink {
                local_ref,
                control: Some(self.control.clone()),
            },
            events: event_rx,
        })
    }

    /// Refuses a terminal-open request. The reason is from the closed set the relay
    /// understands; anything else it treats as an internal error.
    pub async fn decline(&self, request_id: String, reason: &'static str) {
        let _ = self
            .control
            .send(Control::Decline { request_id, reason })
            .await;
    }
}

/// One mirrored terminal on the shared connection.
pub struct Terminal {
    pub sink: TerminalSink,
    pub events: mpsc::Receiver<Event>,
}

impl Terminal {
    /// Separates sending from receiving.
    ///
    /// The two halves belong in different tasks wherever a terminal is being carried
    /// for someone else: `publish` waits when the relay is behind, and if that also
    /// held up the events it would be the keystrokes heading *towards* the busy shell
    /// that stopped arriving.
    pub fn split(self) -> (TerminalSink, mpsc::Receiver<Event>) {
        (self.sink, self.events)
    }

    pub async fn publish(&self, start_offset: u64, bytes: Vec<u8>) -> bool {
        self.sink.publish(start_offset, bytes).await
    }

    pub async fn resize(&self, cols: u16, rows: u16) -> bool {
        self.sink.resize(cols, rows).await
    }

    pub fn begin_shutdown(&mut self) {
        self.sink.begin_shutdown();
    }
}

/// The sending half of one terminal.
pub struct TerminalSink {
    local_ref: String,
    /// Cleared by `begin_shutdown`: there is nothing more to publish for this terminal.
    control: Option<mpsc::Sender<Control>>,
}

impl TerminalSink {
    /// Publishes `bytes`, which begin at `start_offset` in this terminal's stream.
    ///
    /// Awaiting here is deliberate back pressure: when the relay is not keeping up,
    /// the caller — and therefore the shell writing into the pseudo-terminal — waits.
    /// Dropping the bytes instead would corrupt the stream for every subscriber, and
    /// no later offset would ever line up again.
    pub async fn publish(&self, start_offset: u64, bytes: Vec<u8>) -> bool {
        if bytes.is_empty() {
            return true;
        }
        let Some(control) = &self.control else {
            return false;
        };
        control
            .send(Control::Output {
                local_ref: self.local_ref.clone(),
                start_offset,
                bytes,
            })
            .await
            .is_ok()
    }

    /// Declares a new authoritative size. Subscribers are told; they do not decide.
    pub async fn resize(&self, cols: u16, rows: u16) -> bool {
        let Some(control) = &self.control else {
            return false;
        };
        control
            .send(Control::Resize {
                local_ref: self.local_ref.clone(),
                cols,
                rows,
            })
            .await
            .is_ok()
    }

    /// Stops publishing and lets the connection close the terminal at the relay.
    ///
    /// The caller should then wait for `Event::Ended`: a terminal that is merely
    /// abandoned stays open for the relay's whole reconnect grace period, during which
    /// a subscriber can attach to it and type into nothing.
    pub fn begin_shutdown(&mut self) {
        let Some(control) = self.control.take() else {
            return;
        };
        let close = Control::Close {
            local_ref: self.local_ref.clone(),
            reason: "process_exited".to_string(),
        };
        match control.try_send(close) {
            Ok(()) => {}
            // Nobody is listening any more, so there is nothing to tell.
            Err(mpsc::error::TrySendError::Closed(_)) => {}
            Err(mpsc::error::TrySendError::Full(close)) => {
                // Momentarily full. Dropping this would leave the terminal open at the
                // relay for its whole reconnect grace period with nothing behind it,
                // listed on the phone and swallowing anything typed into it — so hand
                // it to a task that is allowed to wait, since a `Drop` is not.
                match tokio::runtime::Handle::try_current() {
                    Ok(handle) => {
                        handle.spawn(async move {
                            let _ = control.send(close).await;
                        });
                    }
                    Err(_) => tracing::error!(
                        local_ref = %self.local_ref,
                        "could not tell the relay this terminal closed"
                    ),
                }
            }
        }
    }
}

impl Drop for TerminalSink {
    fn drop(&mut self) {
        // A dropped handle means the hosted shell is gone. Saying so is what keeps the
        // phone's list honest: an abandoned terminal stays open for the relay's whole
        // grace period, and anything typed into it in the meantime goes nowhere.
        self.begin_shutdown();
    }
}

/// Starts the relay connection and keeps it up, retrying on failure.
///
/// The device key is held rather than a token, because tokens expire in minutes and a
/// reconnect hours later has to be able to mint a fresh one on its own.
pub fn start(config: Config, device_key: KeyPair) -> (Link, mpsc::Receiver<OpenRequest>) {
    let (control_tx, control_rx) = mpsc::channel::<Control>(256);
    // Small on purpose: a backlog of requests to spawn shells is not something to
    // accumulate. A full channel is answered `busy` rather than queued.
    let (open_tx, open_rx) = mpsc::channel::<OpenRequest>(8);
    tokio::spawn(async move {
        let mut multiplexer = Multiplexer {
            open_requests: Some(open_tx),
            ..Multiplexer::default()
        };
        let reason = multiplexer.maintain(config, device_key, control_rx).await;
        multiplexer.end_all(&reason).await;
    });
    (
        Link {
            control: control_tx,
        },
        open_rx,
    )
}

// ------------------------------------------------------------------- per terminal

struct Hosted {
    spec: TerminalSpec,
    /// Assigned by the relay on `terminal.opened`; cleared on a disconnect so nothing
    /// is ever framed against a stale id.
    terminal_id: Option<Uuid>,
    events: mpsc::Sender<Event>,
    /// Set once the local side has finished. The terminal is closed at the relay and
    /// forgotten on the next pass.
    closing: Option<String>,
}

// ------------------------------------------------------------------- multiplexer

#[derive(Default)]
struct Multiplexer {
    terminals: HashMap<String, Hosted>,
    /// Opened terminals, so the orphan sweep can subtract them. Kept for the life of
    /// the connection rather than the terminal: an id closed a moment ago must not be
    /// re-closed as though it belonged to someone else.
    opened: Vec<Uuid>,
    /// Where terminal-open requests go. `None` when nobody is listening, which is how
    /// a caller that cannot host anything refuses by construction.
    open_requests: Option<mpsc::Sender<OpenRequest>>,
}

impl Multiplexer {
    async fn end_all(&mut self, reason: &str) {
        for (_, hosted) in std::mem::take(&mut self.terminals) {
            Self::deliver_final(&hosted, Event::Ended(reason.to_string()));
        }
    }

    fn local_ref_of(&self, terminal_id: Uuid) -> Option<String> {
        self.terminals
            .values()
            .find(|hosted| hosted.terminal_id == Some(terminal_id))
            .map(|hosted| hosted.spec.local_ref.clone())
    }

    /// Deliver to one terminal, and only that one.
    ///
    /// Never awaits, and every event this loop sends goes through here. The owner of a
    /// terminal blocks on `publish` when the socket is behind; if delivering to it
    /// blocked in turn, the two would wait on each other for ever — the owner for room
    /// to send, this loop for room to hand over the acknowledgement that would make
    /// room. A queue that is genuinely full means a terminal has stopped reading,
    /// which is reported rather than papered over.
    fn deliver_to(&mut self, terminal_id: Uuid, event: Event) {
        let Some(local_ref) = self.local_ref_of(terminal_id) else {
            tracing::warn!(
                %terminal_id,
                "a relay message named a terminal this connection does not own"
            );
            return;
        };
        self.deliver(&local_ref, event);
    }

    fn deliver(&mut self, local_ref: &str, event: Event) {
        let Some(hosted) = self.terminals.get(local_ref) else {
            return;
        };
        match hosted.events.try_send(event) {
            Ok(()) => {}
            // The owner is gone; its close is already on the way.
            Err(mpsc::error::TrySendError::Closed(_)) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::error!(
                    local_ref,
                    "a terminal stopped reading its events; closing it rather than \
                     losing keystrokes silently"
                );
                if let Some(hosted) = self.terminals.get_mut(local_ref) {
                    hosted
                        .closing
                        .get_or_insert_with(|| "publisher_unresponsive".to_string());
                }
            }
        }
    }

    /// Delivery for a terminal that has just been taken out of the map, where there is
    /// no queue left to mark as closing.
    fn deliver_final(hosted: &Hosted, event: Event) {
        let _ = hosted.events.try_send(event);
    }

    fn idle(&self) -> bool {
        self.terminals.is_empty()
    }

    async fn maintain(
        &mut self,
        config: Config,
        device_key: KeyPair,
        mut control: mpsc::Receiver<Control>,
    ) -> String {
        let mut attempt = 0u32;
        loop {
            match self
                .connect_and_publish(&config, &device_key, &mut control)
                .await
            {
                Ok(reason) => return reason,
                Err(error) => {
                    attempt = attempt.saturating_add(1);
                    let delay = Duration::from_millis((500u64 << attempt.min(6)).min(30_000));
                    tracing::warn!(
                        error = %error,
                        retry_in_ms = delay.as_millis() as u64,
                        terminals = self.terminals.len(),
                        "the relay connection failed"
                    );
                    // Nothing may be framed until the relay says again where each
                    // stream continues, and everything not yet committed is held by
                    // the terminal's owner, which is what makes that safe.
                    for hosted in self.terminals.values_mut() {
                        hosted.terminal_id = None;
                        let _ = hosted.events.try_send(Event::Detached);
                    }
                    self.opened.clear();

                    // Keep accepting local work during the wait: a terminal opened
                    // mid-outage is published as soon as the relay is back, and one
                    // that ends mid-outage is forgotten rather than reopened.
                    let deadline = tokio::time::Instant::now() + delay;
                    let mut closed = false;
                    loop {
                        tokio::select! {
                            _ = tokio::time::sleep_until(deadline) => break,
                            received = control.recv(), if !closed => match received {
                                Some(message) => self.stash(message).await,
                                None => closed = true,
                            },
                        }
                    }
                    self.terminals.retain(|_, hosted| hosted.closing.is_none());
                    if closed && self.idle() {
                        return "the last terminal ended".into();
                    }
                }
            }
        }
    }

    /// Apply a control message that arrived while there was no connection.
    async fn stash(&mut self, control: Control) {
        match control {
            Control::Open { spec, events } => {
                let local_ref = spec.local_ref.clone();
                self.terminals.insert(
                    local_ref,
                    Hosted {
                        spec,
                        terminal_id: None,
                        events,
                        closing: None,
                    },
                );
            }
            Control::Resize {
                local_ref,
                cols,
                rows,
            } => {
                if let Some(hosted) = self.terminals.get_mut(&local_ref) {
                    hosted.spec.cols = cols;
                    hosted.spec.rows = rows;
                }
            }
            Control::Close { local_ref, reason } => {
                if let Some(hosted) = self.terminals.get_mut(&local_ref) {
                    hosted.closing.get_or_insert(reason);
                    Self::deliver_final(hosted, Event::Ended("the terminal ended".into()));
                }
            }
            // Nothing to decline to: with no connection there is nobody waiting, and
            // the relay has already failed the request for a disconnected device.
            Control::Decline { .. } => {}
            // Nothing is framed while disconnected. The bytes are retained by the
            // terminal's owner and re-offered from the relay's authoritative offset
            // once it says where the stream continues, so dropping this is not a loss.
            Control::Output { .. } => {}
        }
    }

    async fn connect_and_publish(
        &mut self,
        config: &Config,
        device_key: &KeyPair,
        control: &mut mpsc::Receiver<Control>,
    ) -> Result<String> {
        crate::tls::ensure_provider();
        let client = api::Client::new(&config.relay_url)?;
        let token = client.device_token(device_key).await?;

        let ws_url = websocket_url(&config.relay_url, &config.device_id);
        let mut request = ws_url
            .as_str()
            .into_client_request()
            .map_err(|e| SessionError::Connect(e.to_string()))?;
        request.headers_mut().insert(
            "Authorization",
            HeaderValue::from_str(&format!("Bearer {}", token.access_token))
                .map_err(|e| SessionError::Connect(e.to_string()))?,
        );
        request.headers_mut().insert(
            "Sec-WebSocket-Protocol",
            HeaderValue::from_static(SUBPROTOCOL_V2),
        );

        let (stream, response) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|e| SessionError::Connect(e.to_string()))?;

        // Version 2 is what carries input. Without it every terminal here would be
        // read-only and every keystroke from the phone silently discarded, so refuse
        // rather than mirror something that cannot be typed into.
        let negotiated = response
            .headers()
            .get("sec-websocket-protocol")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        if negotiated != SUBPROTOCOL_V2 {
            return Err(SessionError::Refused(format!(
                "the relay selected {negotiated:?}, which cannot carry terminal input"
            )));
        }

        // Listed before anything of ours is opened, so the list cannot name a terminal
        // this connection is about to reattach to. What it is *for* is decided after
        // the opens, by subtracting the ids the relay hands back.
        let previously_open = self.list_open(config).await;

        let (sink, mut source) = stream.split();
        let (writer, writer_task) = spawn_writer(sink);

        // ------------------------------------------------------------------- ready
        let limits = loop {
            let message = next_text(&mut source).await?;
            match parse_server_message(&message) {
                Some(ServerMessage::Ready { limits, .. }) => break limits,
                Some(ServerMessage::Error { code, message, .. }) => {
                    writer_task.abort();
                    return Err(SessionError::Refused(format!("{code}: {message}")));
                }
                _ => continue,
            }
        };

        // Asserted after `ready` and on every reconnect: the relay rebuilds its view of
        // this device per connection, and a capability it remembered across one would be
        // a yes this machine never gave (relay spec §4.6).
        if config.allow_remote_open {
            let capabilities = serde_json::json!({
                "type": "publisher.capabilities",
                "optional": true,
                "terminal_open_requests": true,
            });
            writer
                .send(Message::Text(capabilities.to_string().into()))
                .await;
        }

        let negotiated_limits = Limits {
            max_output_frame_bytes: limits.max_output_frame_bytes.max(1024),
            max_unacked_output_bytes: limits.max_unacked_output_bytes.max(64 * 1024),
        };
        let max_terminals = if limits.max_active_terminals == 0 {
            u64::MAX
        } else {
            limits.max_active_terminals
        };

        let outcome = self
            .run_connection(
                control,
                &mut source,
                &writer,
                negotiated_limits,
                max_terminals,
                previously_open,
                config.allow_remote_open,
            )
            .await;

        drop(writer);
        let _ = tokio::time::timeout(Duration::from_secs(3), writer_task).await;
        outcome
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_connection<S>(
        &mut self,
        control: &mut mpsc::Receiver<Control>,
        source: &mut S,
        writer: &Writer,
        limits: Limits,
        max_terminals: u64,
        previously_open: Vec<Uuid>,
        allow_remote_open: bool,
    ) -> Result<String>
    where
        S: futures_util::Stream<
                Item = std::result::Result<Message, tokio_tungstenite::tungstenite::Error>,
            > + Unpin,
    {
        // Every terminal is (re)opened up front: after a reconnect the relay knows
        // them by `local_ref` and hands back the same ids, which is what makes a
        // dropped connection an interruption rather than a new set of terminals.
        for local_ref in self.terminals.keys().cloned().collect::<Vec<_>>() {
            self.send_open(writer, &local_ref).await?;
        }

        let mut accepting = true;
        let mut sweep_at =
            (!previously_open.is_empty()).then(|| tokio::time::Instant::now() + ORPHAN_SETTLE);

        loop {
            self.close_finished(writer).await?;
            if self.idle() && !accepting {
                return Ok("the last terminal ended".into());
            }

            // Room kept back so a pong is never the message that does not fit.
            let may_accept = accepting && writer.capacity() > WRITER_RESERVE;

            tokio::select! {
                biased;

                incoming = source.next() => {
                    let Some(frame) = incoming else {
                        return Err(SessionError::Closed("the relay hung up".into()));
                    };
                    let frame = frame.map_err(|e| SessionError::Closed(e.to_string()))?;
                    if let Some(reason) = self
                        .on_frame(frame, writer, limits, allow_remote_open)
                        .await?
                    {
                        return Ok(reason);
                    }
                }

                received = control.recv(), if may_accept => {
                    match received {
                        Some(message) => {
                            self.on_control(message, writer, limits, max_terminals).await?;
                        }
                        None => {
                            // Every handle is gone; each sent its close on the way out.
                            accepting = false;
                        }
                    }
                }

                _ = async { tokio::time::sleep_until(sweep_at.unwrap()).await },
                        if sweep_at.is_some() => {
                    sweep_at = None;
                    self.sweep_orphans(writer, &previously_open).await;
                }
            }
        }
    }

    /// Close what an earlier run left behind — anything open for this device that this
    /// connection does not own. A device holds one publisher connection, so there is
    /// no process behind them; leaving them listed would let a subscriber attach to a
    /// terminal and type into nothing.
    async fn sweep_orphans(&self, writer: &Writer, previously_open: &[Uuid]) {
        for orphan in previously_open {
            if self.opened.contains(orphan) {
                continue;
            }
            let close = serde_json::json!({
                "type": "terminal.close",
                "terminal_id": orphan,
                "reason": "publisher_replaced",
            });
            writer.send(Message::Text(close.to_string().into())).await;
            tracing::info!(terminal = %orphan, "closed a terminal left open by an earlier run");
        }
    }

    async fn on_control(
        &mut self,
        message: Control,
        writer: &Writer,
        limits: Limits,
        max_terminals: u64,
    ) -> Result<()> {
        match message {
            Control::Open { spec, events } => {
                if self.terminals.len() as u64 >= max_terminals {
                    let _ = events.try_send(Event::Ended(format!(
                        "this machine already has the relay's limit of \
                             {max_terminals} terminals open"
                    )));
                    return Ok(());
                }
                let local_ref = spec.local_ref.clone();
                if self.terminals.contains_key(&local_ref) {
                    // Two shells under one local_ref would be deduplicated onto one
                    // offset stream at the relay and interleave byte by byte.
                    let _ = events.try_send(Event::Ended(
                        "that terminal reference is already in use".into(),
                    ));
                    return Ok(());
                }
                self.terminals.insert(
                    local_ref.clone(),
                    Hosted {
                        spec,
                        terminal_id: None,
                        events,
                        closing: None,
                    },
                );
                self.send_open(writer, &local_ref).await?;
            }
            Control::Output {
                local_ref,
                start_offset,
                bytes,
            } => {
                let Some(terminal_id) = self
                    .terminals
                    .get(&local_ref)
                    .and_then(|hosted| hosted.terminal_id)
                else {
                    // Not attached: the bytes are retained by the terminal's owner and
                    // re-offered from the relay's authoritative offset on reattach.
                    return Ok(());
                };
                // Normally a no-op: the owner already splits to the limit it was told
                // in `Attached`. It stays as a backstop because a frame over the limit
                // is refused, and a refusal here would stall that terminal's stream.
                let mut offset = start_offset;
                for slice in bytes.chunks(limits.max_output_frame_bytes as usize) {
                    writer
                        .send(Message::Binary(
                            encode_output_frame(terminal_id, offset, slice).into(),
                        ))
                        .await;
                    offset += slice.len() as u64;
                }
            }
            Control::Resize {
                local_ref,
                cols,
                rows,
            } => {
                if let Some(hosted) = self.terminals.get_mut(&local_ref) {
                    hosted.spec.cols = cols;
                    hosted.spec.rows = rows;
                    if let Some(terminal_id) = hosted.terminal_id {
                        let resize = serde_json::json!({
                            "type": "terminal.resize",
                            "terminal_id": terminal_id,
                            "cols": cols,
                            "rows": rows,
                        });
                        writer.send(Message::Text(resize.to_string().into())).await;
                    }
                }
            }
            Control::Close { local_ref, reason } => {
                if let Some(hosted) = self.terminals.get_mut(&local_ref) {
                    hosted.closing.get_or_insert(reason);
                }
                // The local side is finished, so say so now rather than when the relay
                // has been told: the owner is waiting for this before it exits, and the
                // relay's half may have to wait for a `terminal.opened` first.
                self.deliver(&local_ref, Event::Ended("the terminal ended".into()));
            }
            Control::Decline { request_id, reason } => {
                let declined = serde_json::json!({
                    "type": "terminal.open_declined",
                    "in_reply_to": request_id,
                    "reason": reason,
                });
                writer
                    .send(Message::Text(declined.to_string().into()))
                    .await;
            }
        }
        Ok(())
    }

    /// Returns `Some(reason)` when the connection is over for good.
    async fn on_frame(
        &mut self,
        frame: Message,
        writer: &Writer,
        limits: Limits,
        allow_remote_open: bool,
    ) -> Result<Option<String>> {
        match frame {
            Message::Text(text) => match parse_server_message(&text) {
                Some(ServerMessage::TerminalOpened {
                    request_id,
                    terminal_id,
                    next_offset,
                    ..
                }) => {
                    // Correlated strictly by request_id, which is this side's
                    // local_ref: several terminals can be opening at once, and
                    // matching positionally would hand one shell another's stream.
                    match self.terminals.get_mut(&request_id) {
                        Some(hosted) => {
                            hosted.terminal_id = Some(terminal_id);
                            self.opened.push(terminal_id);
                            // Through `deliver`, not a bare send: if this one event
                            // were dropped the terminal would never learn where its
                            // stream is and would sit silent for ever.
                            self.deliver(
                                &request_id,
                                Event::Attached {
                                    terminal_id,
                                    next_offset,
                                    limits,
                                },
                            );
                            tracing::info!(%terminal_id, local_ref = %request_id, "publishing");
                        }
                        None => {
                            tracing::warn!(%terminal_id, %request_id, "the relay opened a terminal nothing asked for");
                        }
                    }
                }
                Some(ServerMessage::OutputAck {
                    terminal_id,
                    durable_offset,
                    ..
                }) => {
                    self.deliver_to(terminal_id, Event::Ack { durable_offset });
                }
                Some(ServerMessage::TerminalOpenRequest {
                    request_id,
                    label,
                    cols,
                    rows,
                }) => {
                    // Refused here as well as at the relay. The relay is not trusted to
                    // have checked: if it were compromised, or simply wrong, this is the
                    // check that still holds, in the process that would do the spawning.
                    let refusal = if !allow_remote_open {
                        Some("not_permitted")
                    } else {
                        match self.open_requests.as_ref() {
                            // Never awaited: this is the socket reader, and blocking it
                            // on a slow consumer would stall every terminal's output.
                            Some(sender) => sender
                                .try_send(OpenRequest {
                                    request_id: request_id.clone(),
                                    label,
                                    cols: cols.map(|v| v.clamp(1, u16::MAX as u32) as u16),
                                    rows: rows.map(|v| v.clamp(1, u16::MAX as u32) as u16),
                                })
                                .err()
                                .map(|_| "busy"),
                            None => Some("unsupported"),
                        }
                    };
                    if let Some(reason) = refusal {
                        tracing::info!(reason, "declining a terminal-open request");
                        let declined = serde_json::json!({
                            "type": "terminal.open_declined",
                            "in_reply_to": request_id,
                            "reason": reason,
                        });
                        writer
                            .send(Message::Text(declined.to_string().into()))
                            .await;
                    }
                }
                Some(ServerMessage::TerminalResizeRequest {
                    terminal_id,
                    cols,
                    rows,
                }) => {
                    self.deliver_to(
                        terminal_id,
                        Event::ResizeRequest {
                            cols: cols.clamp(1, u16::MAX as u32) as u16,
                            rows: rows.clamp(1, u16::MAX as u32) as u16,
                        },
                    );
                }
                Some(ServerMessage::TerminalClosed {
                    terminal_id,
                    reason,
                }) => {
                    if let Some(local_ref) = self.local_ref_of(terminal_id)
                        && let Some(hosted) = self.terminals.remove(&local_ref)
                    {
                        Self::deliver_final(
                            &hosted,
                            Event::Ended(format!("the relay closed the terminal: {reason}")),
                        );
                    }
                }
                Some(ServerMessage::Error {
                    code,
                    message,
                    terminal_id,
                    request_id,
                    next_offset,
                    ..
                }) => {
                    if code == "superseded" {
                        // A device may hold one publisher connection (spec §6.1) and a
                        // second one takes it over. Reconnecting would take it straight
                        // back, and the two would trade the device for as long as both
                        // ran, reopening every terminal on each round. Stopping is the
                        // only behaviour that converges.
                        return Ok(Some(
                            "another publisher on this machine took over; only one may \
                             publish at a time"
                                .into(),
                        ));
                    }
                    if !self
                        .on_terminal_error(
                            writer,
                            &code,
                            &message,
                            terminal_id,
                            request_id.as_deref(),
                            next_offset,
                        )
                        .await
                    {
                        return Err(SessionError::Refused(format!("{code}: {message}")));
                    }
                }
                Some(ServerMessage::Ping { at_unix_ms }) => {
                    let pong = serde_json::json!({"type": "pong", "at_unix_ms": at_unix_ms});
                    writer.send(Message::Text(pong.to_string().into())).await;
                }
                _ => {}
            },
            Message::Binary(bytes) => match decode_input_frame(&bytes) {
                // Routed by terminal, which is the point of multiplexing: the relay
                // addresses input to a *device*, and only this connection knows which
                // of its terminals owns it. Before this, input for anything but the
                // one terminal a process happened to own was dropped on the floor.
                Ok(input) => {
                    self.deliver_to(input.terminal_id, Event::Input(input.payload));
                }
                Err(error) => tracing::warn!(%error, "undecodable binary frame"),
            },
            Message::Ping(payload) => writer.send(Message::Pong(payload)).await,
            Message::Close(_) => {
                return Err(SessionError::Closed("the relay closed the socket".into()));
            }
            _ => {}
        }
        Ok(None)
    }

    /// Returns false when the error is about the connection rather than one terminal.
    async fn on_terminal_error(
        &mut self,
        writer: &Writer,
        code: &str,
        message: &str,
        terminal_id: Option<Uuid>,
        request_id: Option<&str>,
        authoritative: Option<u64>,
    ) -> bool {
        // An error naming one terminal must not take the others down with it. That is
        // the difference multiplexing makes: one tab's problem is one tab's problem.
        let local_ref = terminal_id
            .and_then(|id| self.local_ref_of(id))
            .or_else(|| {
                request_id
                    .filter(|id| self.terminals.contains_key(*id))
                    .map(str::to_string)
            });
        let Some(local_ref) = local_ref else {
            if terminal_id.is_some() || request_id.is_some() {
                // It names a terminal, just not one still here — a refusal in flight
                // when it closed, most likely. Tearing the connection down over that
                // would take every other terminal on it with it.
                tracing::info!(
                    ?terminal_id,
                    code,
                    "a relay error named a terminal this connection has already closed"
                );
                return true;
            }
            return false;
        };

        if code == "offset_mismatch"
            && let (Some(next_offset), Some(hosted)) =
                (authoritative, self.terminals.get(&local_ref))
        {
            // The relay did not append and has said exactly where it is. Its owner
            // holds the bytes and decides whether it can resume from there.
            let _ = hosted.events.try_send(Event::Mismatch { next_offset });
            return true;
        }

        tracing::warn!(local_ref = %local_ref, code, message, "the relay refused this terminal");
        if let Some(hosted) = self.terminals.remove(&local_ref) {
            if let Some(terminal_id) = hosted.terminal_id {
                let close = serde_json::json!({
                    "type": "terminal.close",
                    "terminal_id": terminal_id,
                    "reason": "publisher_error",
                });
                writer.send(Message::Text(close.to_string().into())).await;
            }
            Self::deliver_final(&hosted, Event::Ended(format!("{code}: {message}")));
        }
        true
    }

    async fn send_open(&self, writer: &Writer, local_ref: &str) -> Result<()> {
        let Some(hosted) = self.terminals.get(local_ref) else {
            return Ok(());
        };
        let open = serde_json::json!({
            "type": "terminal.open",
            // The local_ref doubles as the request id, so a reply can be matched back
            // to the terminal that asked without a second table to keep in step.
            "request_id": hosted.spec.local_ref,
            "local_ref": hosted.spec.local_ref,
            "label": hosted.spec.label,
            "cols": hosted.spec.cols,
            "rows": hosted.spec.rows,
            "term": hosted.spec.term,
            // Version 2 only, and the whole point of this tool: the phone can type.
            "accepts_input": true,
        });
        // Only on the *first* open. A reconnect re-opens the same terminal, and echoing
        // the request again would answer a request that was resolved long ago — or, on a
        // relay that has since reused nothing, answer a stranger's.
        if let (Some(request_id), None) = (&hosted.spec.in_reply_to, hosted.terminal_id) {
            if let Some(map) = open.as_object() {
                let mut open = map.clone();
                open.insert(
                    "in_reply_to".to_string(),
                    serde_json::Value::String(request_id.clone()),
                );
                writer
                    .send(Message::Text(
                        serde_json::Value::Object(open).to_string().into(),
                    ))
                    .await;
                return Ok(());
            }
        }
        writer.send(Message::Text(open.to_string().into())).await;
        Ok(())
    }

    /// Tell the relay about terminals whose local side has finished, and forget them.
    ///
    /// A terminal that has not been opened yet is deliberately left alone. Its
    /// `terminal.open` is already in flight, so the relay is about to have a terminal
    /// with nothing behind it; forgetting it here would mean never sending the close,
    /// and it would sit in the phone's list swallowing keystrokes until something else
    /// noticed. It is closed on the pass after its `terminal.opened` arrives instead.
    async fn close_finished(&mut self, writer: &Writer) -> Result<()> {
        let finished: Vec<String> = self
            .terminals
            .iter()
            .filter(|(_, hosted)| hosted.closing.is_some() && hosted.terminal_id.is_some())
            .map(|(local_ref, _)| local_ref.clone())
            .collect();

        for local_ref in finished {
            let Some(hosted) = self.terminals.remove(&local_ref) else {
                continue;
            };
            if let Some(terminal_id) = hosted.terminal_id {
                let close = serde_json::json!({
                    "type": "terminal.close",
                    "terminal_id": terminal_id,
                    "reason": hosted.closing.clone().unwrap_or_else(|| "closed".into()),
                });
                writer.send(Message::Text(close.to_string().into())).await;
            }
            Self::deliver_final(&hosted, Event::Ended("the terminal ended".into()));
        }
        Ok(())
    }

    /// Terminals this device currently has open at the relay.
    ///
    /// Best effort: failing to find them is not a reason to refuse to publish, so
    /// every error here answers "none".
    async fn list_open(&self, config: &Config) -> Vec<Uuid> {
        let Some(identity) = config.identity_key.as_ref() else {
            return Vec::new();
        };
        let Ok(client) = api::Client::new(&config.relay_url) else {
            return Vec::new();
        };
        let Ok(token) = client.identity_token(identity).await else {
            return Vec::new();
        };
        match client
            .terminals_filtered(&token.access_token, Some(&config.device_id))
            .await
        {
            Ok(terminals) => terminals
                .into_iter()
                .filter_map(|t| Uuid::parse_str(&t.terminal_id).ok())
                .collect(),
            Err(_) => Vec::new(),
        }
    }
}

// ------------------------------------------------------------------- socket writer

/// The write half, owned by its own task.
///
/// Splitting it out is what keeps acknowledgements, input and pings flowing while a
/// terminal is flooding: the reader never awaits the socket, so it can never be the
/// thing that stops the messages it is waiting for.
#[derive(Clone)]
struct Writer {
    tx: mpsc::Sender<Message>,
}

impl Writer {
    async fn send(&self, message: Message) {
        let _ = self.tx.send(message).await;
    }

    fn capacity(&self) -> usize {
        self.tx.capacity()
    }
}

fn spawn_writer<S>(mut sink: S) -> (Writer, tokio::task::JoinHandle<()>)
where
    S: futures_util::Sink<Message> + Unpin + Send + 'static,
{
    let (tx, mut rx) = mpsc::channel::<Message>(WRITER_QUEUE);
    let task = tokio::spawn(async move {
        while let Some(message) = rx.recv().await {
            if sink.send(message).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });
    (Writer { tx }, task)
}

async fn next_text<S>(source: &mut S) -> Result<String>
where
    S: futures_util::Stream<
            Item = std::result::Result<Message, tokio_tungstenite::tungstenite::Error>,
        > + Unpin,
{
    loop {
        let Some(frame) = source.next().await else {
            return Err(SessionError::Closed("the relay hung up".into()));
        };
        match frame.map_err(|e| SessionError::Closed(e.to_string()))? {
            Message::Text(text) => return Ok(text.to_string()),
            Message::Close(_) => {
                return Err(SessionError::Closed("the relay closed the socket".into()));
            }
            _ => continue,
        }
    }
}

/// `https://host` → `wss://host/v1/devices/{id}/relay`, and `http` → `ws`.
pub fn websocket_url(base: &str, device_id: &str) -> String {
    let base = base.trim_end_matches('/');
    let swapped = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        base.to_string()
    };
    format!("{swapped}/v1/devices/{device_id}/relay")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hosted(local_ref: &str) -> (Hosted, mpsc::Receiver<Event>) {
        let (events, rx) = mpsc::channel(16);
        (
            Hosted {
                spec: TerminalSpec {
                    local_ref: local_ref.to_string(),
                    label: local_ref.to_string(),
                    cols: 80,
                    rows: 24,
                    term: "xterm-256color".into(),
                    in_reply_to: None,
                },
                terminal_id: None,
                events,
                closing: None,
            },
            rx,
        )
    }

    #[test]
    fn the_websocket_url_follows_the_scheme() {
        assert_eq!(
            websocket_url("https://relay.example", "abc"),
            "wss://relay.example/v1/devices/abc/relay"
        );
        assert_eq!(
            websocket_url("http://127.0.0.1:9080/", "abc"),
            "ws://127.0.0.1:9080/v1/devices/abc/relay"
        );
    }

    #[tokio::test]
    async fn a_message_reaches_only_the_terminal_it_names() {
        let mut mux = Multiplexer::default();
        let (mut one, mut one_events) = hosted("one");
        let (mut two, mut two_events) = hosted("two");
        one.terminal_id = Some(Uuid::from_u128(1));
        two.terminal_id = Some(Uuid::from_u128(2));
        mux.terminals.insert("one".into(), one);
        mux.terminals.insert("two".into(), two);

        mux.deliver_to(Uuid::from_u128(2), Event::Input(b"ls\r".to_vec()));

        // Routing by id is what keeps one shell's keystrokes out of another's, which
        // is the failure the whole multiplexer exists to prevent.
        assert!(matches!(two_events.try_recv(), Ok(Event::Input(bytes)) if bytes == b"ls\r"));
        assert!(one_events.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_message_for_an_unknown_terminal_disturbs_nobody() {
        let mut mux = Multiplexer::default();
        let (mut one, mut one_events) = hosted("one");
        one.terminal_id = Some(Uuid::from_u128(1));
        mux.terminals.insert("one".into(), one);

        // A late frame from a previous connection must be ignored, not delivered to
        // whichever terminal happens to be around.
        mux.deliver_to(Uuid::from_u128(99), Event::Ack { durable_offset: 10 });
        assert!(one_events.try_recv().is_err());
    }

    #[tokio::test]
    async fn an_error_naming_one_terminal_leaves_the_others_alone() {
        let mut mux = Multiplexer::default();
        let (mut one, mut one_events) = hosted("one");
        let (mut two, mut two_events) = hosted("two");
        one.terminal_id = Some(Uuid::from_u128(1));
        two.terminal_id = Some(Uuid::from_u128(2));
        mux.terminals.insert("one".into(), one);
        mux.terminals.insert("two".into(), two);
        let (writer, _task) = spawn_writer(futures_util::sink::drain().sink_map_err(|_| ()));

        let handled = mux
            .on_terminal_error(
                &writer,
                "terminal_not_found",
                "gone",
                Some(Uuid::from_u128(1)),
                None,
                None,
            )
            .await;

        assert!(handled, "a per-terminal error must not end the connection");
        assert!(!mux.terminals.contains_key("one"));
        assert!(mux.terminals.contains_key("two"));
        assert!(matches!(one_events.try_recv(), Ok(Event::Ended(_))));
        assert!(two_events.try_recv().is_err());
    }

    #[tokio::test]
    async fn an_offset_mismatch_is_referred_to_the_terminal_that_holds_the_bytes() {
        let mut mux = Multiplexer::default();
        let (mut one, mut one_events) = hosted("one");
        one.terminal_id = Some(Uuid::from_u128(1));
        mux.terminals.insert("one".into(), one);
        let (writer, _task) = spawn_writer(futures_util::sink::drain().sink_map_err(|_| ()));

        let handled = mux
            .on_terminal_error(
                &writer,
                "offset_mismatch",
                "no",
                Some(Uuid::from_u128(1)),
                None,
                Some(4096),
            )
            .await;

        assert!(handled);
        // Still hosted: a mismatch is resynchronised, not fatal, and above all it does
        // not reconnect a socket that N-1 other terminals are using.
        assert!(mux.terminals.contains_key("one"));
        assert!(matches!(
            one_events.try_recv(),
            Ok(Event::Mismatch { next_offset: 4096 })
        ));
    }

    #[tokio::test]
    async fn an_error_about_the_connection_is_not_swallowed_as_a_terminal_error() {
        let mut mux = Multiplexer::default();
        let (writer, _task) = spawn_writer(futures_util::sink::drain().sink_map_err(|_| ()));
        let handled = mux
            .on_terminal_error(&writer, "unauthorized", "no", None, None, None)
            .await;
        assert!(!handled);
    }

    #[tokio::test]
    async fn a_terminal_that_ends_before_the_relay_opens_it_is_still_closed() {
        let mut mux = Multiplexer::default();
        let (mut one, _events) = hosted("one");
        one.closing = Some("process_exited".into());
        mux.terminals.insert("one".into(), one);

        let (tx, mut rx) = mpsc::channel::<Message>(8);
        let writer = Writer { tx };

        // Its `terminal.open` is already in flight, so forgetting it now would mean
        // the close is never sent and the relay is left with a terminal that has
        // nothing behind it.
        mux.close_finished(&writer).await.unwrap();
        assert!(mux.terminals.contains_key("one"), "kept until it has an id");
        assert!(rx.try_recv().is_err(), "nothing to close yet");

        mux.terminals.get_mut("one").unwrap().terminal_id = Some(Uuid::from_u128(5));
        mux.close_finished(&writer).await.unwrap();
        assert!(!mux.terminals.contains_key("one"));
        let text = match rx.try_recv().expect("a close") {
            Message::Text(text) => text.to_string(),
            other => panic!("expected a close message, got {other:?}"),
        };
        assert!(text.contains("terminal.close"), "{text}");
        assert!(text.contains(&Uuid::from_u128(5).to_string()), "{text}");
    }

    #[tokio::test]
    async fn the_orphan_sweep_spares_the_terminals_this_connection_opened() {
        let mut mux = Multiplexer::default();
        let mine = Uuid::from_u128(7);
        let stale = Uuid::from_u128(8);
        mux.opened.push(mine);

        let (tx, mut rx) = mpsc::channel::<Message>(8);
        let writer = Writer { tx };
        mux.sweep_orphans(&writer, &[mine, stale]).await;

        // Exactly one close, for the terminal nothing is behind. Closing `mine` would
        // take down a tab that is mirroring perfectly well.
        let sent = rx.try_recv().expect("one close");
        let text = match sent {
            Message::Text(text) => text.to_string(),
            other => panic!("expected a close message, got {other:?}"),
        };
        assert!(text.contains(&stale.to_string()), "{text}");
        assert!(rx.try_recv().is_err(), "only the orphan is closed");
    }
}
