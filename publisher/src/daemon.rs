//! The multiplexing daemon: one relay connection, many mirrored terminals.
//!
//! A device may hold only one publisher connection (spec §6.1). Before this existed,
//! that meant one mirrored terminal per machine: a second `hypeterm-publish run` took
//! the device over from the first, which stopped mirroring, while the relay went on
//! routing input to the *device* — so a phone attached to the older terminal typed
//! into nothing, silently. The daemon owns that one connection and every `run` hands
//! it one terminal, which is what lets a machine mirror as many as the relay allows.
//!
//! Unix only, deliberately. The case this exists for is a row of WSL tabs, where the
//! Linux build is the better choice anyway because the pseudo-terminal is then the one
//! the shell actually has rather than a ConPTY wrapping `wsl.exe`. On Windows `run`
//! still publishes directly, one terminal at a time.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use tokio::io::AsyncWriteExt as _;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, mpsc};

use crate::crypto::KeyPair;
use crate::ipc::{self, Frame, FromClient, FromDaemon, Paths, code};
use crate::publish;
use crate::session::{self, Event, TerminalSpec};

/// How long the daemon waits, with nothing to mirror, before standing down. Long
/// enough to survive closing one tab and opening another; short enough that a machine
/// nobody is mirroring on is not holding a relay connection open all day.
const IDLE_GRACE: Duration = Duration::from_secs(60);
/// How long `run` will keep trying to reach a daemon before giving up and hosting the
/// shell unmirrored.
const ATTACH_DEADLINE: Duration = Duration::from_secs(5);

/// Exit status for the daemon that loses the race to bind. Distinct so the spawner can
/// tell "another daemon already owns this device" — the expected outcome — from a
/// genuine failure.
pub const EXIT_ALREADY_RUNNING: i32 = 3;

// ------------------------------------------------------------------- the lock

/// The single-instance lock, held for as long as the daemon runs.
///
/// `flock` and not the socket file, because a socket cannot arbitrate: two processes
/// that both find no socket both bind, and whichever unlinks the other's leaves a
/// daemon serving an inode nobody can reach — two publisher connections for one
/// device, superseding each other in a loop, which is precisely what this all exists
/// to remove. The kernel drops a `flock` when the holder dies, so there is no stale
/// lock to clean up, and the lock file is never unlinked: doing so would let the next
/// process take a second, independent lock on a fresh inode.
pub struct Lock {
    _file: std::fs::File,
}

impl Lock {
    pub fn acquire(path: &Path) -> Result<Option<Self>, String> {
        use std::os::unix::fs::OpenOptionsExt as _;
        use std::os::unix::io::AsRawFd as _;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)
            .map_err(|e| format!("opening {}: {e}", path.display()))?;
        let taken = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if taken == 0 {
            return Ok(Some(Self { _file: file }));
        }
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EWOULDBLOCK) => Ok(None),
            _ => Err(format!("locking {}: {error}", path.display())),
        }
    }
}

// ------------------------------------------------------------------- the server

struct Shared {
    link: session::Link,
    /// Every `local_ref` a live client holds. The relay deduplicates opens by
    /// (device, local_ref), so admitting a second client under one would splice two
    /// shells onto a single offset stream.
    claimed: Mutex<HashSet<String>>,
    clients: AtomicU32,
    /// This machine's own policy for phone-initiated terminals. Read from the state
    /// file, never from the relay (relay spec §4.6 condition 2).
    remote: crate::state::RemoteOpenConfig,
    state_file: PathBuf,
    log: PathBuf,
    /// Children spawned for a phone-opened terminal that have not attached yet. They
    /// are invisible to `clients` until they do, and the idle stand-down would unlink
    /// the socket out from under them.
    hosting: AtomicU32,
    /// Local rate limit, independent of the relay's.
    last_spawns: Mutex<Vec<std::time::Instant>>,
}

/// At most this many spawns in the window below. Deliberately small: a person opening
/// tabs does not need more, and it bounds what a stolen phone can start before anyone
/// notices.
const REMOTE_OPEN_BURST: usize = 3;
const REMOTE_OPEN_WINDOW: Duration = Duration::from_secs(60);

/// Refuses a label rather than sanitising it.
///
/// The label is printed by `hypeterm-publish list` into a real terminal on this machine
/// and crosses an argv boundary. Stripping would leave the phone showing one string and
/// this machine another, which is the confusion an injection wants.
fn acceptable_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= 200
        && !label.chars().all(char::is_whitespace)
        && !label
            .chars()
            .any(|c| c.is_control() || ('\u{80}'..='\u{9F}').contains(&c))
}

pub async fn serve(
    paths: Paths,
    config: session::Config,
    device_key: KeyPair,
    remote_open: crate::state::RemoteOpenConfig,
    state_file: PathBuf,
) -> Result<(), String> {
    let Some(_lock) = Lock::acquire(&paths.lock)? else {
        // Not an error: another daemon already owns this device, which is the whole
        // point of the lock. The client that spawned us will reach it.
        tracing::info!("another daemon already owns this device");
        std::process::exit(EXIT_ALREADY_RUNNING);
    };

    let mut listener = bind(&paths.socket)?;
    tracing::info!(socket = %paths.socket.display(), "listening");

    let (link, mut open_requests) = session::start(config, device_key);
    let shared = Arc::new(Shared {
        link,
        claimed: Mutex::new(HashSet::new()),
        clients: AtomicU32::new(0),
        remote: remote_open,
        state_file: state_file.clone(),
        log: paths.log.clone(),
        hosting: AtomicU32::new(0),
        last_spawns: Mutex::new(Vec::new()),
    });
    let (done_tx, mut done_rx) = mpsc::channel::<()>(64);

    loop {
        // `hosting` as well as `clients`: a child spawned for a phone has not connected
        // back yet, so it does not count as a client, and standing down here would
        // unlink the socket it is about to attach to. It presents as a network fault.
        //
        // Remote open pins the daemon up entirely. The relay delivers an open request
        // over the publisher connection this process owns, so a machine that has stood
        // down cannot be asked for a terminal — and a machine with no terminal open is
        // exactly when someone asks. Standing down would mean the phone could only ask
        // for a second terminal, never a first, which is not a feature.
        let idle = shared.clients.load(Ordering::Acquire) == 0
            && shared.hosting.load(Ordering::Acquire) == 0
            && !shared.remote.enabled;
        let deadline = idle.then(|| tokio::time::Instant::now() + IDLE_GRACE);

        tokio::select! {
            request = open_requests.recv() => {
                match request {
                    Some(request) => {
                        let shared = Arc::clone(&shared);
                        tokio::spawn(async move { host_for_request(shared, request).await });
                    }
                    // The session ended for good; nothing more will be asked.
                    None => {}
                }
            }

            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        if !same_user(&stream) {
                            tracing::warn!("refused a connection from another user");
                            continue;
                        }
                        shared.clients.fetch_add(1, Ordering::AcqRel);
                        let shared = Arc::clone(&shared);
                        let done = done_tx.clone();
                        tokio::spawn(async move {
                            if let Err(error) = client(stream, Arc::clone(&shared)).await {
                                tracing::info!(%error, "a client connection ended");
                            }
                            shared.clients.fetch_sub(1, Ordering::AcqRel);
                            let _ = done.send(()).await;
                        });
                    }
                    Err(error) => {
                        tracing::warn!(%error, "accept failed");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }

            _ = async { tokio::time::sleep_until(deadline.unwrap()).await },
                    if deadline.is_some() => {
                // Unlink first, so nothing new can find the socket, then look once
                // more: a connect that the kernel already queued in the backlog has
                // *succeeded* from the client's point of view, and exiting under it
                // would look to that client like a daemon that died.
                let _ = std::fs::remove_file(&paths.socket);
                // A brief wait, not a single poll: a connection sitting in the backlog
                // has already *succeeded* from the client's side, but the first poll of
                // `accept` may only register interest rather than see it. Exiting under
                // such a client would look to it like a daemon that died the moment it
                // connected.
                match tokio::time::timeout(Duration::from_millis(250), listener.accept()).await {
                    Ok(Ok((stream, _))) => {
                        listener = bind(&paths.socket)?;
                        if same_user(&stream) {
                            shared.clients.fetch_add(1, Ordering::AcqRel);
                            let shared = Arc::clone(&shared);
                            let done = done_tx.clone();
                            tokio::spawn(async move {
                                let _ = client(stream, Arc::clone(&shared)).await;
                                shared.clients.fetch_sub(1, Ordering::AcqRel);
                                let _ = done.send(()).await;
                            });
                        }
                    }
                    _ => {
                        tracing::info!("nothing left to mirror; standing down");
                        return Ok(());
                    }
                }
            }

            _ = done_rx.recv() => {}
        }
    }
}

fn bind(socket: &Path) -> Result<UnixListener, String> {
    // Only ever reached while holding the lock, so an existing socket here is the
    // remains of a daemon that died, not a live one.
    let _ = std::fs::remove_file(socket);
    let listener =
        UnixListener::bind(socket).map_err(|e| format!("binding {}: {e}", socket.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600));
    }
    Ok(listener)
}

/// The directory is already private, so this is defence in depth rather than the
/// barrier — but the thing on the other end can publish to this machine's device, and
/// that is worth checking twice.
fn same_user(stream: &UnixStream) -> bool {
    match stream.peer_cred() {
        Ok(cred) => cred.uid() == unsafe { libc::geteuid() },
        Err(_) => false,
    }
}

/// One client, which is one terminal for the life of the connection.
async fn client(stream: UnixStream, shared: Arc<Shared>) -> Result<(), String> {
    let (mut reader, writer) = stream.into_split();
    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(256);

    // The writer is its own task throughout: the reader below blocks on `publish` when
    // the relay is behind, and if that also stopped keystrokes going out, a busy
    // terminal would be an untypable one.
    let writer_task = tokio::spawn(async move {
        let mut writer = writer;
        while let Some(frame) = out_rx.recv().await {
            if writer.write_all(&frame).await.is_err() {
                break;
            }
        }
        let _ = writer.shutdown().await;
    });

    let result = converse(&mut reader, &out_tx, &shared).await;
    drop(out_tx);
    let _ = tokio::time::timeout(Duration::from_secs(2), writer_task).await;
    result
}

async fn converse(
    reader: &mut tokio::net::unix::OwnedReadHalf,
    out: &mpsc::Sender<Vec<u8>>,
    shared: &Arc<Shared>,
) -> Result<(), String> {
    // ------------------------------------------------------------------- hello
    let hello: FromClient = match ipc::read_frame(reader, true).await {
        Ok(Frame::Control(payload)) => ipc::parse_control(&payload).map_err(|e| e.to_string())?,
        Ok(_) => return refuse(out, code::PROTOCOL_ERROR, "the first frame must be hello").await,
        Err(error) => return Err(error.to_string()),
    };
    let FromClient::Hello { ipc_version, .. } = hello else {
        return refuse(out, code::PROTOCOL_ERROR, "the first frame must be hello").await;
    };
    // Counted excluding this one, which is what lets a mismatched build know whether
    // asking the daemon to stand down would cost anybody else their mirror.
    let others = shared.clients.load(Ordering::Acquire).saturating_sub(1);
    send(
        out,
        &FromDaemon::HelloOk {
            ipc_version: ipc::IPC_VERSION,
            build: env!("CARGO_PKG_VERSION").to_string(),
            pid: std::process::id(),
            clients: others,
        },
    )
    .await;
    if ipc_version != ipc::IPC_VERSION {
        // Say so and let the client decide: it knows whether it can live without a
        // mirror, and this daemon knows it must not take other tabs down to make way.
        tracing::warn!(
            theirs = ipc_version,
            ours = ipc::IPC_VERSION,
            "a client speaks another version"
        );
    }

    // ------------------------------------------------------------------- open
    let open: FromClient = match ipc::read_frame(reader, true).await {
        Ok(Frame::Control(payload)) => ipc::parse_control(&payload).map_err(|e| e.to_string())?,
        Ok(_) => return refuse(out, code::PROTOCOL_ERROR, "expected open").await,
        Err(error) => return Err(error.to_string()),
    };
    let spec = match open {
        FromClient::Open {
            local_ref,
            label,
            cols,
            rows,
            term,
            in_reply_to,
        } => TerminalSpec {
            local_ref,
            label,
            cols,
            rows,
            term,
            // Carried through from the `run` this daemon spawned, so the answer reaches
            // the subscriber that is waiting on it. `None` for an ordinary local tab.
            in_reply_to,
        },
        FromClient::Retire => {
            // Terminals, not connections: a client that has attached but not yet
            // opened has nothing to lose, and one that has is the whole reason to
            // refuse. Standing down would drop the relay connection under it.
            let mirroring = shared.claimed.lock().await.len();
            if others == 0 && mirroring == 0 {
                send(
                    out,
                    &FromDaemon::Ended {
                        reason: "retiring so a newer build can take over".into(),
                    },
                )
                .await;
                // Exiting frees the lock and the socket; the client respawns.
                tokio::time::sleep(Duration::from_millis(50)).await;
                std::process::exit(0);
            }
            return refuse(
                out,
                code::RETIRE_REFUSED,
                "terminals are being mirrored through this daemon",
            )
            .await;
        }
        _ => return refuse(out, code::PROTOCOL_ERROR, "expected open").await,
    };

    {
        let mut claimed = shared.claimed.lock().await;
        if !claimed.insert(spec.local_ref.clone()) {
            return refuse(
                out,
                code::DUPLICATE_LOCAL_REF,
                "another terminal here already uses that reference",
            )
            .await;
        }
    }
    let guard = Claim {
        shared: Arc::clone(shared),
        local_ref: spec.local_ref.clone(),
    };

    let Some(terminal) = shared.link.open(spec).await else {
        return refuse(out, code::PROTOCOL_ERROR, "the relay connection has ended").await;
    };
    // Split so the two directions run in their own tasks: `publish` below waits when
    // the relay is behind, and keystrokes must keep arriving while it does.
    let (mut sink, mut events) = terminal.split();

    let events_out = out.clone();
    let events_task = tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            let message = match event {
                Event::Attached {
                    terminal_id,
                    next_offset,
                    limits,
                } => FromDaemon::Attached {
                    terminal_id: terminal_id.to_string(),
                    next_offset,
                    max_output_frame_bytes: limits.max_output_frame_bytes,
                    max_unacked_output_bytes: limits.max_unacked_output_bytes,
                },
                Event::Ack { durable_offset } => FromDaemon::Ack { durable_offset },
                Event::Mismatch { next_offset } => FromDaemon::Mismatch { next_offset },
                Event::Detached => FromDaemon::Detached,
                Event::ResizeRequest { cols, rows } => FromDaemon::ResizeRequest { cols, rows },
                Event::Input(bytes) => {
                    if events_out.send(ipc::encode_input(&bytes)).await.is_err() {
                        break;
                    }
                    continue;
                }
                Event::Ended(reason) => {
                    send(&events_out, &FromDaemon::Ended { reason }).await;
                    break;
                }
            };
            send(&events_out, &message).await;
        }
    });

    // ------------------------------------------------------------------- the client
    let outcome = loop {
        match ipc::read_frame(reader, true).await {
            Ok(Frame::Output {
                start_offset,
                bytes,
            }) => {
                if bytes.is_empty() {
                    continue;
                }
                if !sink.publish(start_offset, bytes).await {
                    break Ok(());
                }
            }
            Ok(Frame::Control(payload)) => {
                let message: FromClient = match ipc::parse_control(&payload) {
                    Ok(message) => message,
                    Err(error) => break Err(error.to_string()),
                };
                match message {
                    FromClient::Resize { cols, rows } => {
                        sink.resize(cols, rows).await;
                    }
                    FromClient::Close { .. } => break Ok(()),
                    _ => {}
                }
            }
            Ok(Frame::Input(_)) => break Err("a client sent an input frame".into()),
            // EOF without a close: the tab was killed, or the shell died with it. With
            // the daemon holding the WebSocket there is no relay-side grace period to
            // catch that, so an abandoned terminal would stay listed — and swallow
            // keystrokes — until something else closed it.
            Err(ipc::IpcError::Io(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                break Ok(());
            }
            Err(error) => break Err(error.to_string()),
        }
    };

    // Closing the terminal at the relay is the last thing this connection does, and it
    // happens however the client left: cleanly, killed, or with its tab.
    sink.begin_shutdown();
    // Give the relay's `terminal.closed` a moment to reach the client before the
    // socket goes; it is only a courtesy, since the close itself is already in flight.
    let _ = tokio::time::timeout(Duration::from_millis(500), events_task).await;
    drop(guard);
    outcome
}

struct Claim {
    shared: Arc<Shared>,
    local_ref: String,
}

impl Drop for Claim {
    fn drop(&mut self) {
        let shared = Arc::clone(&self.shared);
        let local_ref = self.local_ref.clone();
        tokio::spawn(async move {
            shared.claimed.lock().await.remove(&local_ref);
        });
    }
}

async fn send(out: &mpsc::Sender<Vec<u8>>, message: &FromDaemon) {
    let _ = out.send(ipc::encode_control(message)).await;
}

async fn refuse(out: &mpsc::Sender<Vec<u8>>, code: &str, message: &str) -> Result<(), String> {
    send(
        out,
        &FromDaemon::Error {
            code: code.to_string(),
            message: message.to_string(),
        },
    )
    .await;
    Err(message.to_string())
}

// ------------------------------------------------------------------- starting one

/// Spawns a daemon that outlives the terminal that started it.
///
/// Two hops on purpose. The first child exits at once, so the caller has a short,
/// reapable process to wait on and never accumulates a zombie; the real daemon is its
/// child, `setsid`-ed into its own session so a Ctrl-C or a closed tab — both of which
/// signal the whole foreground process group — cannot take every other tab's mirror
/// down with it.
pub fn spawn(state_file: &Path, log: &Path) -> Result<(), String> {
    let mut command = detached_command(state_file, log)?;
    command.arg("daemon").arg("--detach");
    let mut child = command
        .spawn()
        .map_err(|e| format!("starting the daemon: {e}"))?;
    let _ = child.wait();
    Ok(())
}

/// The second hop: replaces this short-lived process with the daemon proper.
pub fn respawn_detached(state_file: &Path, log: &Path) -> Result<(), String> {
    let mut command = detached_command(state_file, log)?;
    command.arg("daemon").arg("--foreground");
    command
        .spawn()
        .map_err(|e| format!("starting the daemon: {e}"))?;
    Ok(())
}

/// Opens a terminal because a subscriber asked (relay spec §4.6).
///
/// Every check here is local. The relay has made its own, but this is the process that
/// would call `execve`, and it is the one that has to hold if the relay is compromised
/// or simply wrong.
async fn host_for_request(shared: Arc<Shared>, request: session::OpenRequest) {
    let decline = |reason: &'static str| {
        let shared = Arc::clone(&shared);
        let request_id = request.request_id.clone();
        async move { shared.link.decline(request_id, reason).await }
    };

    // 1. This machine's own switch. The one that holds under a compromised relay.
    if !shared.remote.enabled {
        decline("not_permitted").await;
        return;
    }

    // 2. A local rate limit, independent of the relay's.
    {
        let mut spawns = shared.last_spawns.lock().await;
        let now = std::time::Instant::now();
        spawns.retain(|at| now.duration_since(*at) < REMOTE_OPEN_WINDOW);
        if spawns.len() >= REMOTE_OPEN_BURST {
            drop(spawns);
            decline("busy").await;
            return;
        }
        spawns.push(now);
    }

    // 3. How many terminals a phone may hold here at once.
    let live = shared.claimed.lock().await.len() as u32 + shared.hosting.load(Ordering::Acquire);
    if live >= shared.remote.max_terminals {
        decline("limit_reached").await;
        return;
    }

    // 4. Re-validate the label rather than trusting the relay to have done it.
    let label = request.label.clone().unwrap_or_else(|| "phone".to_string());
    if !acceptable_label(&label) {
        decline("not_permitted").await;
        return;
    }

    let cols = request.cols.unwrap_or(80).clamp(1, u16::MAX);
    let rows = request.rows.unwrap_or(24).clamp(1, u16::MAX);

    // 5. Record the attempt *before* spawning, so a spawn that dies still leaves a
    // trace. The label is escaped precisely because it came off the network.
    let record = format!(
        "unix={} request={} label={:?} argv={:?} cwd={:?} cols={} rows={}\n",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default(),
        request.request_id,
        label,
        shared.remote.shell,
        shared.remote.cwd,
        cols,
        rows,
    );
    append_remote_open_log(&shared.log, &record);

    // 6. Spawn. Fixed argv built with `arg`, no shell anywhere on this path.
    shared.hosting.fetch_add(1, Ordering::AcqRel);
    let spawned = spawn_hosted_shell(&shared, &label, &request.request_id, cols, rows);
    match spawned {
        Ok(()) => {
            // The child answers the request itself, by echoing `in_reply_to` on its own
            // `terminal.open`. If it never gets that far the relay times the caller out,
            // which is the honest answer: something may still have started here.
            let shared = Arc::clone(&shared);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(30)).await;
                shared.hosting.fetch_sub(1, Ordering::AcqRel);
            });
        }
        Err(error) => {
            shared.hosting.fetch_sub(1, Ordering::AcqRel);
            tracing::warn!(%error, "could not host a terminal for a subscriber");
            append_remote_open_log(&shared.log, &format!("  failed: {error}\n"));
            decline("internal_error").await;
        }
    }
}

fn spawn_hosted_shell(
    shared: &Arc<Shared>,
    label: &str,
    request_id: &str,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    let mut command = detached_command(&shared.state_file, &shared.log)?;
    // Not the daemon's inherited environment and not `/`: the daemon was started by
    // whichever `run` happened to be first, and "open me a shell" means the shell and
    // directory the machine's owner recorded when they turned this on.
    if !shared.remote.cwd.is_empty() {
        command.current_dir(&shared.remote.cwd);
    } else if let Ok(home) = std::env::var("HOME") {
        command.current_dir(home);
    }
    if let Some(program) = shared.remote.shell.first() {
        command.env("SHELL", program);
    }
    command.arg("run");
    command.arg("--label").arg(label);
    command.arg("--in-reply-to").arg(request_id);
    command.arg("--cols").arg(cols.to_string());
    command.arg("--rows").arg(rows.to_string());
    if shared.remote.shell.len() > 1 || !shared.remote.shell.is_empty() {
        command.arg("--");
        for argument in &shared.remote.shell {
            command.arg(argument);
        }
    }
    command.spawn().map(|_| ()).map_err(|e| e.to_string())
}

/// Appends to a private log beside the daemon's own.
///
/// A process that appeared on this machine because a phone asked should be findable
/// afterwards without reading the relay's records.
fn append_remote_open_log(log: &Path, line: &str) {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let path = log.with_file_name("remote-opens.log");
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
    {
        let _ = file.write_all(line.as_bytes());
    }
}

fn detached_command(state_file: &Path, log: &Path) -> Result<std::process::Command, String> {
    use std::os::unix::fs::OpenOptionsExt as _;
    use std::os::unix::process::CommandExt as _;

    let exe = std::env::current_exe().map_err(|e| format!("finding this program: {e}"))?;
    let mut command = std::process::Command::new(exe);
    command.arg("--state-file").arg(state_file);
    command.stdin(std::process::Stdio::null());
    command.stdout(std::process::Stdio::null());
    // Never the caller's stderr: `run` puts its terminal into raw mode, and a stray
    // reconnect warning written into the middle of a screen corrupts a session that is
    // otherwise working perfectly.
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(log)
    {
        Ok(file) => command.stderr(std::process::Stdio::from(file)),
        Err(_) => command.stderr(std::process::Stdio::null()),
    };
    // Not the tab's directory: holding it open would stop a filesystem unmounting long
    // after the tab that started this had gone.
    command.current_dir("/");
    unsafe {
        command.pre_exec(|| {
            // EPERM means it is already a session leader, which is just as good.
            libc::setsid();
            Ok(())
        });
    }
    Ok(command)
}

// ------------------------------------------------------------------- attaching

/// Attaches one terminal to the daemon, starting one if none is running, and keeps it
/// attached across a daemon restart.
///
/// Never falls back to publishing directly. A second publisher connection would
/// supersede the daemon's (spec §6.1), the daemon would stop for good — correctly,
/// since reconnecting would have the two trade the device for ever — and every other
/// tab on this machine would lose its mirror because of one transient socket error.
/// One unmirrored tab is a far smaller loss than that, so failure here is reported and
/// the shell is hosted anyway.
pub async fn attach(
    paths: &Paths,
    state_file: &Path,
    spec: TerminalSpec,
) -> Result<(mpsc::Sender<publish::Request>, mpsc::Receiver<Event>), String> {
    // The first attempt is the caller's to hear about: if there is no daemon and one
    // cannot be started, the person deserves to be told before their shell opens.
    let stream = handshake(paths, state_file, &spec).await?;

    let (event_tx, event_rx) = mpsc::channel::<Event>(256);
    let (request_tx, request_rx) = mpsc::channel::<publish::Request>(64);
    tokio::spawn(carry(
        paths.clone(),
        state_file.to_path_buf(),
        spec,
        stream,
        event_tx,
        request_rx,
    ));
    Ok((request_tx, event_rx))
}

/// Why one attachment ended.
enum Ended {
    /// The socket failed. The terminal is still ours: reattach and carry on, and the
    /// bytes the relay never committed go out again from this process's own retained
    /// buffer. That is what makes a daemon restart an interruption rather than a hole.
    Transport,
    /// The daemon or the relay ended this terminal. Reattaching would reopen a
    /// terminal that is meant to be closed.
    Final(String),
}

/// Keeps one terminal attached for as long as the shell lives.
async fn carry(
    paths: Paths,
    state_file: PathBuf,
    spec: TerminalSpec,
    first: UnixStream,
    event_tx: mpsc::Sender<Event>,
    mut requests: mpsc::Receiver<publish::Request>,
) {
    let mut stream = first;

    loop {
        let (read_half, write_half) = stream.into_split();
        let dead = Arc::new(tokio::sync::Notify::new());

        // Reader and writer are separate tasks throughout. If one task both wrote and
        // read, a write that blocked because the daemon was busy would stop this side
        // reading — and the daemon, blocked writing to a peer that had stopped reading,
        // would stop reading in turn. Two tasks, no cycle.
        let reader = {
            let dead = Arc::clone(&dead);
            let event_tx = event_tx.clone();
            tokio::spawn(async move {
                let ended = read_from_daemon(read_half, &event_tx).await;
                // notify_one, not notify_waiters: this fires once, and the loop it has
                // to reach spends most of its time between two `notified()` futures
                // rather than inside one. `notify_waiters` wakes only whoever happens
                // to be waiting at that instant and leaves nothing behind, so a tab
                // whose shell was quiet would never learn the daemon had gone.
                dead.notify_one();
                ended
            })
        };
        let (frames_tx, frames_rx) = mpsc::channel::<Vec<u8>>(8);
        let writer = tokio::spawn(write_to_daemon(write_half, frames_rx));

        // The reader knows more than the socket does: whether the daemon *ended* this
        // terminal, which is the difference between reattaching and not.
        let ended = match forward_requests(&mut requests, &frames_tx, &dead).await {
            Some(ended) => {
                reader.abort();
                ended
            }
            None => reader.await.unwrap_or(Ended::Transport),
        };
        writer.abort();

        if let Ended::Final(reason) = ended {
            let _ = event_tx.send(Event::Ended(reason)).await;
            return;
        }

        // Nothing may be framed until a daemon says again where the stream continues.
        if event_tx.send(Event::Detached).await.is_err() {
            return;
        }
        tracing::warn!("the mirroring daemon stopped; reattaching");

        stream = match reattach(&paths, &state_file, &spec).await {
            Some(next) => next,
            None => {
                let _ = event_tx
                    .send(Event::Ended(
                        "the mirroring daemon on this machine stopped and could not be \
                         restarted"
                            .into(),
                    ))
                    .await;
                return;
            }
        };
    }
}

/// Reattaches with backoff, under the same `local_ref`.
///
/// The same reference deliberately: the relay deduplicates opens by (device,
/// local_ref), so the terminal that comes back is the one that went away, resuming at
/// the offset it left off. The phone's list does not grow a second row, and the bytes
/// the relay never committed go out again from this process's own retained buffer —
/// which is exactly why that buffer lives here and not in the daemon.
async fn reattach(paths: &Paths, state_file: &Path, spec: &TerminalSpec) -> Option<UnixStream> {
    for attempt in 1..=8u32 {
        tokio::time::sleep(Duration::from_millis(
            (250u64 << attempt.min(7)).min(30_000),
        ))
        .await;
        match handshake(paths, state_file, spec).await {
            Ok(stream) => return Some(stream),
            Err(error) => tracing::warn!(%error, attempt, "could not reattach"),
        }
    }
    None
}

/// Moves the driver's requests onto the socket. Returns `Some` when the *driver* is
/// finished, `None` when the connection died under it.
async fn forward_requests(
    requests: &mut mpsc::Receiver<publish::Request>,
    frames: &mpsc::Sender<Vec<u8>>,
    dead: &Arc<tokio::sync::Notify>,
) -> Option<Ended> {
    loop {
        let request = tokio::select! {
            request = requests.recv() => request?,
            _ = dead.notified() => return None,
        };
        let (frame, last) = match request {
            publish::Request::Output {
                start_offset,
                bytes,
            } => (ipc::encode_output(start_offset, &bytes), false),
            publish::Request::Resize { cols, rows } => (
                ipc::encode_control(&FromClient::Resize { cols, rows }),
                false,
            ),
            publish::Request::Close { reason } => {
                (ipc::encode_control(&FromClient::Close { reason }), true)
            }
        };
        tokio::select! {
            sent = frames.send(frame) => {
                if sent.is_err() {
                    return None;
                }
            }
            // The frame is dropped, and that is safe: the driver still holds those
            // bytes, and the next attachment resends from wherever the relay says it
            // actually got to.
            _ = dead.notified() => return None,
        }
        if last {
            // The close is the last thing this terminal has to say. Wait for the
            // daemon's answer rather than reattaching to a terminal that is ending.
            tokio::select! {
                request = requests.recv() => { request?; }
                _ = dead.notified() => {}
            }
            return Some(Ended::Final("the terminal ended".into()));
        }
    }
}

async fn write_to_daemon(
    mut writer: tokio::net::unix::OwnedWriteHalf,
    mut frames: mpsc::Receiver<Vec<u8>>,
) {
    while let Some(frame) = frames.recv().await {
        if writer.write_all(&frame).await.is_err() {
            break;
        }
    }
    let _ = writer.shutdown().await;
}

async fn read_from_daemon(
    mut reader: tokio::net::unix::OwnedReadHalf,
    event_tx: &mpsc::Sender<Event>,
) -> Ended {
    loop {
        match ipc::read_frame(&mut reader, false).await {
            Ok(Frame::Input(bytes)) => {
                if event_tx.send(Event::Input(bytes)).await.is_err() {
                    return Ended::Final("the terminal stopped listening".into());
                }
            }
            Ok(Frame::Control(payload)) => {
                let message: FromDaemon = match ipc::parse_control(&payload) {
                    Ok(message) => message,
                    // Unknown messages are ignored, never fatal (spec §12).
                    Err(_) => continue,
                };
                let event = match message {
                    FromDaemon::Attached {
                        terminal_id,
                        next_offset,
                        max_output_frame_bytes,
                        max_unacked_output_bytes,
                    } => Event::Attached {
                        terminal_id: uuid::Uuid::parse_str(&terminal_id).unwrap_or_default(),
                        next_offset,
                        limits: session::Limits {
                            max_output_frame_bytes,
                            max_unacked_output_bytes,
                        },
                    },
                    FromDaemon::Ack { durable_offset } => Event::Ack { durable_offset },
                    FromDaemon::Mismatch { next_offset } => Event::Mismatch { next_offset },
                    FromDaemon::Detached => Event::Detached,
                    FromDaemon::ResizeRequest { cols, rows } => Event::ResizeRequest { cols, rows },
                    // The relay or the daemon ended this terminal. Reattaching would
                    // reopen something that is meant to be closed.
                    FromDaemon::Ended { reason } => return Ended::Final(reason),
                    FromDaemon::Error { code, message } => {
                        return Ended::Final(format!("{code}: {message}"));
                    }
                    _ => continue,
                };
                if event_tx.send(event).await.is_err() {
                    return Ended::Final("the terminal stopped listening".into());
                }
            }
            Ok(Frame::Output { .. }) => {
                return Ended::Final("the daemon sent output to a publisher".into());
            }
            // The socket failed. The shell has not: the caller reattaches, and the
            // bytes the relay never committed are still held here.
            Err(_) => return Ended::Transport,
        }
    }
}

/// Connects, greets, and asks for this terminal to be published.
async fn handshake(
    paths: &Paths,
    state_file: &Path,
    spec: &TerminalSpec,
) -> Result<UnixStream, String> {
    let mut stream = connect_or_start(paths, state_file).await?;

    stream
        .write_all(&ipc::encode_control(&FromClient::Hello {
            ipc_version: ipc::IPC_VERSION,
            build: env!("CARGO_PKG_VERSION").to_string(),
            pid: std::process::id(),
        }))
        .await
        .map_err(|e| format!("greeting the daemon: {e}"))?;

    let greeting: FromDaemon = match ipc::read_frame(&mut stream, false).await {
        Ok(Frame::Control(payload)) => ipc::parse_control(&payload).map_err(|e| e.to_string())?,
        Ok(_) => return Err("the daemon did not greet this terminal".into()),
        Err(error) => return Err(format!("greeting the daemon: {error}")),
    };
    match greeting {
        FromDaemon::HelloOk {
            ipc_version,
            build,
            pid,
            ..
        } if ipc_version != ipc::IPC_VERSION => {
            return Err(format!(
                "a hypeterm-publish daemon from build {build} (pid {pid}) is already \
                 mirroring on this machine and speaks a different local protocol. \
                 Stop it with: hypeterm-publish daemon --stop"
            ));
        }
        FromDaemon::HelloOk { .. } => {}
        other => return Err(format!("the daemon answered {other:?} to a greeting")),
    }

    stream
        .write_all(&ipc::encode_control(&FromClient::Open {
            local_ref: spec.local_ref.clone(),
            label: spec.label.clone(),
            cols: spec.cols,
            rows: spec.rows,
            term: spec.term.clone(),
            in_reply_to: spec.in_reply_to.clone(),
        }))
        .await
        .map_err(|e| format!("asking the daemon to publish: {e}"))?;

    Ok(stream)
}

async fn connect_or_start(paths: &Paths, state_file: &Path) -> Result<UnixStream, String> {
    if let Ok(stream) = UnixStream::connect(&paths.socket).await {
        return Ok(stream);
    }
    // Spawn unconditionally rather than checking first: two tabs opening at the same
    // instant would both see nothing and both spawn, and the loser exits harmlessly
    // because the lock, not the socket, decides who serves.
    spawn(state_file, &paths.log)?;

    let deadline = tokio::time::Instant::now() + ATTACH_DEADLINE;
    let mut wait = Duration::from_millis(20);
    loop {
        if let Ok(stream) = UnixStream::connect(&paths.socket).await {
            return Ok(stream);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "no mirroring daemon answered on {} within {} seconds",
                paths.socket.display(),
                ATTACH_DEADLINE.as_secs()
            ));
        }
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(Duration::from_millis(250));
    }
}

/// What a running daemon says about itself, or `None` when none is running.
///
/// Asked over the socket rather than inferred from the lock, because the answer is the
/// daemon's own: its pid and its build are what a person needs to find its log, and
/// what a test needs to be able to stop it.
#[derive(Debug, Clone)]
pub struct Running {
    pub pid: u32,
    pub build: String,
    pub ipc_version: u32,
    /// Clients attached other than this probe.
    pub clients: u32,
}

pub async fn probe(paths: &Paths) -> Option<Running> {
    let mut stream = UnixStream::connect(&paths.socket).await.ok()?;
    stream
        .write_all(&ipc::encode_control(&FromClient::Hello {
            ipc_version: ipc::IPC_VERSION,
            build: env!("CARGO_PKG_VERSION").to_string(),
            pid: std::process::id(),
        }))
        .await
        .ok()?;
    match ipc::read_frame(&mut stream, false).await {
        Ok(Frame::Control(payload)) => match ipc::parse_control::<FromDaemon>(&payload) {
            Ok(FromDaemon::HelloOk {
                ipc_version,
                build,
                pid,
                clients,
            }) => Some(Running {
                pid,
                build,
                ipc_version,
                clients,
            }),
            _ => None,
        },
        _ => None,
    }
}

/// Asks a running daemon to stand down. Used by `hypeterm-publish daemon --stop`.
pub async fn stop(paths: &Paths) -> Result<bool, String> {
    let Ok(mut stream) = UnixStream::connect(&paths.socket).await else {
        return Ok(false);
    };
    stream
        .write_all(&ipc::encode_control(&FromClient::Hello {
            ipc_version: ipc::IPC_VERSION,
            build: env!("CARGO_PKG_VERSION").to_string(),
            pid: std::process::id(),
        }))
        .await
        .map_err(|e| e.to_string())?;
    let (mut reader, mut writer) = stream.into_split();
    let _ = ipc::read_frame(&mut reader, false).await;
    writer
        .write_all(&ipc::encode_control(&FromClient::Retire))
        .await
        .map_err(|e| e.to_string())?;
    match ipc::read_frame(&mut reader, false).await {
        Ok(Frame::Control(payload)) => match ipc::parse_control::<FromDaemon>(&payload) {
            Ok(FromDaemon::Ended { .. }) => Ok(true),
            Ok(FromDaemon::Error { message, .. }) => Err(message),
            _ => Ok(false),
        },
        _ => Ok(false),
    }
}

/// Where a daemon's log lives, for `status` to print.
pub fn log_path(paths: &Paths) -> PathBuf {
    paths.log.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_label_with_control_characters_is_refused_not_sanitised() {
        // It reaches argv and is printed into a real terminal on this machine. Every
        // one of these is a way to make the phone's view and this machine's disagree.
        assert!(!acceptable_label("build\u{1b}]0;pwned\u{7}"));
        assert!(!acceptable_label("two\nlines"));
        assert!(!acceptable_label("bell\u{7}"));
        assert!(!acceptable_label("\u{85}next-line"));
        assert!(!acceptable_label(""));
        assert!(!acceptable_label("   "));
        assert!(!acceptable_label(&"x".repeat(201)));
    }

    #[test]
    fn an_ordinary_label_is_accepted_unchanged() {
        assert!(acceptable_label("phone"));
        assert!(acceptable_label("build shell"));
        // Not ASCII-only: a name in somebody's own language is not an attack.
        assert!(acceptable_label("студия"));
        assert!(acceptable_label("ビルド"));
    }

    #[test]
    fn the_hosted_command_is_argv_and_never_a_shell_string() {
        // The whole of this path is `Command::arg`. If this file ever grows a shell
        // invocation or a formatted command string, a phone-supplied value becomes a
        // way to run something — the one thing this design must not allow.
        //
        // The needles are assembled at run time so this assertion cannot match its own
        // source and pass for the wrong reason.
        let source = include_str!("daemon.rs");
        for needle in [
            format!("{}{}", "sh -", "c"),
            format!("{}{}", ".arg(\"-", "c\")"),
            format!("{}{}", "Command::new(\"", "sh\")"),
        ] {
            assert!(
                !source.contains(&needle),
                "the spawn path must never construct a shell command: found {needle}"
            );
        }
    }
}
