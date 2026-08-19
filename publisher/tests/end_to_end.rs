//! Publish a terminal and read it back through the relay.
//!
//! Everything else is unit-tested in isolation; this is the only test that says the
//! parts agree — that bytes leaving a pseudo-terminal here arrive, in order and
//! unaltered, at something subscribing the way the Android client does.
//!
//! Needs a running relay. `HYPETERM_TEST_RELAY=http://127.0.0.1:9080 cargo test`;
//! without it the test skips rather than fails, because a relay is not something a
//! checkout can assume (`just up local` in ../server starts one).

use futures_util::{SinkExt as _, StreamExt as _};
use hypeterm_publish::{api, crypto::KeyPair, protocol, pty, publish, session, state};
use portable_pty::CommandBuilder;
use std::time::Duration;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::Message;

const MIRROR_V2: &str = "terminal-relay.mirror.v2";

fn relay_url() -> Option<String> {
    match std::env::var("HYPETERM_TEST_RELAY") {
        Ok(url) if !url.trim().is_empty() => Some(url),
        _ => {
            eprintln!(
                "SKIP: set HYPETERM_TEST_RELAY to a running relay \
                 (cd ../server && just port=9080 health_port=9081 up local)"
            );
            None
        }
    }
}

/// Opens one mirrored terminal on `link`, driven exactly as `run` drives it —
/// through the offset and retention machinery, not around it.
async fn mirror_on(link: &session::Link, label: &str) -> publish::Mirror {
    let terminal = link
        .open(session::TerminalSpec {
            local_ref: uuid::Uuid::new_v4().to_string(),
            label: label.to_string(),
            cols: 80,
            rows: 24,
            term: "xterm-256color".into(),
            in_reply_to: None,
        })
        .await
        .expect("the relay connection accepts a terminal");
    let (requests, events) = publish::direct(terminal);
    publish::start(requests, events)
}

fn connect(stored: &state::State, device: KeyPair) -> session::Link {
    // These tests never host a terminal for a subscriber, so the request channel is
    // dropped: a dropped receiver makes the publisher decline by construction.
    session::start(
        session::Config {
            relay_url: stored.relay_url.clone(),
            device_id: stored.device_id.clone(),
            // No sweeping: each test enrols its own identity, and a sweep would only
            // add a round trip.
            identity_key: None,
            allow_remote_open: false,
        },
        device,
    )
    .0
}

/// The terminal_id the relay gave a label, once it appears in the list.
async fn await_terminal(client: &api::Client, token: &str, label: &str) -> String {
    for _ in 0..50 {
        let terminals = client.terminals(token).await.expect("lists terminals");
        if let Some(found) = terminals.iter().find(|t| t.label == label) {
            return found.terminal_id.clone();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the terminal labelled {label:?} was never published");
}

/// Subscribes the way the Android client does and types `text` into the terminal.
async fn subscribe_and_type(
    relay_url: &str,
    token: &str,
    terminal_id: &str,
    text: &str,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let url = format!(
        "{}/v1/terminals/{terminal_id}/mirror",
        // Both schemes, or an https relay silently stays https and the upgrade fails.
        relay_url
            .replacen("https://", "wss://", 1)
            .replacen("http://", "ws://", 1)
    );
    let mut request = url.as_str().into_client_request().expect("request");
    request.headers_mut().insert(
        "Authorization",
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    );
    request.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        HeaderValue::from_static(MIRROR_V2),
    );
    let (mut stream, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("mirror connects");
    stream
        .send(Message::Text(
            r#"{"type":"subscribe","from_offset":0}"#.into(),
        ))
        .await
        .expect("subscribes");

    // Wait for the subscription to be acknowledged before typing: input sent before
    // the relay has a subscription would be refused, not queued (spec §4.5).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), stream.next()).await {
            Ok(Some(Ok(Message::Text(text)))) if text.contains("\"subscribed\"") => break,
            Ok(Some(Ok(_))) => continue,
            _ => break,
        }
    }

    // `0x02 | u64 client sequence | payload` — the mirror direction has no UUID.
    let mut frame = vec![0x02u8];
    frame.extend_from_slice(&1u64.to_be_bytes());
    frame.extend_from_slice(text.as_bytes());
    stream
        .send(Message::Binary(frame.into()))
        .await
        .expect("sends input");
    stream
}

/// The identity every test in this binary shares, and a token for it.
///
/// Shared because a relay allows only ten identity registrations an hour per source
/// address (`ratelimit.identity_registrations_per_hour_per_source`), and one per test
/// meant the suite could be run about twice an hour before it started failing with
/// `429` for reasons that had nothing to do with the code. The token is cached with it
/// for the same reason: minting one costs a proof-of-possession challenge, and those
/// are rate limited per key fingerprint.
///
/// Each test still registers its own *device*, which is what actually has to be
/// separate — the relay allows one publisher connection per device, so tests sharing
/// one would supersede each other.
struct SharedIdentity {
    seed: [u8; 32],
    identity_id: String,
    token: String,
}

static IDENTITY: tokio::sync::OnceCell<SharedIdentity> = tokio::sync::OnceCell::const_new();

async fn shared_identity(relay: &str) -> &'static SharedIdentity {
    IDENTITY
        .get_or_init(|| async {
            let client = api::Client::new(relay).expect("client");
            let identity = KeyPair::generate();
            let registered = client
                .register_identity(&identity)
                .await
                .expect("registers an identity");
            let token = client
                .identity_token(&identity)
                .await
                .expect("identity token");
            SharedIdentity {
                seed: identity.seed(),
                identity_id: registered.identity_id,
                token: token.access_token,
            }
        })
        .await
}

/// Registers a fresh publisher device under the shared identity.
///
/// Returns the state a `run` would have written, a token for the owning identity, and
/// the device's key.
async fn enroll(relay: &str) -> (state::State, String, KeyPair) {
    let shared = shared_identity(relay).await;
    let identity = KeyPair::from_seed(&shared.seed).expect("the shared identity key");
    let client = api::Client::new(relay).expect("client");

    let device = KeyPair::generate();
    let registered_device = client
        .register_device(
            &shared.token,
            &shared.identity_id,
            &device.public_key_base64(),
            Some(&device),
            "test-publisher",
            "publisher",
        )
        .await
        .expect("registers a publisher device");

    let mut stored = state::State {
        relay_url: relay.trim_end_matches('/').to_string(),
        device_id: registered_device.device_id,
        ..Default::default()
    };
    stored.set_identity_key(&identity);
    // The relay's id for an identity is the fingerprint of its key, so this is the
    // value `set_identity_key` just derived — taken from the relay rather than
    // recomputed, so a disagreement would show up here rather than at connect time.
    stored.identity_id = shared.identity_id.clone();
    stored.set_device_key(&device);
    (stored, shared.token.clone(), device)
}

/// Subscribes the way the Android client does and collects output until `wanted`
/// appears or the deadline passes.
async fn mirror_until(
    relay: &str,
    identity_token: &str,
    terminal_id: &str,
    wanted: &str,
    deadline: Duration,
) -> String {
    let url = format!(
        "{}/v1/terminals/{terminal_id}/mirror",
        relay
            .trim_end_matches('/')
            .replacen("http://", "ws://", 1)
            .replacen("https://", "wss://", 1)
    );
    let mut request = url.as_str().into_client_request().expect("request");
    request.headers_mut().insert(
        "Authorization",
        HeaderValue::from_str(&format!("Bearer {identity_token}")).unwrap(),
    );
    request.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        HeaderValue::from_static(MIRROR_V2),
    );

    let (mut stream, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("mirror connects");
    stream
        .send(Message::Text(
            r#"{"type":"subscribe","from_offset":0}"#.into(),
        ))
        .await
        .expect("subscribes");

    let mut seen = Vec::new();
    let _ = tokio::time::timeout(deadline, async {
        while let Some(Ok(frame)) = stream.next().await {
            if let Message::Binary(bytes) = frame {
                // Mirror framing is `0x01 | u64 start offset | payload` — no terminal
                // UUID, unlike the publisher direction.
                if bytes.len() >= 9 && bytes[0] == protocol::FRAME_TYPE_OUTPUT {
                    seen.extend_from_slice(&bytes[9..]);
                }
            }
            if String::from_utf8_lossy(&seen).contains(wanted) {
                break;
            }
        }
    })
    .await;
    String::from_utf8_lossy(&seen).to_string()
}

#[tokio::test]
async fn output_published_here_arrives_at_a_subscriber() {
    let Some(relay) = relay_url() else { return };
    let (stored, identity_token, device) = enroll(&relay).await;

    let marker = format!("marker-{}", uuid::Uuid::new_v4());
    let mut command = CommandBuilder::new("/bin/sh");
    command.args(["-c", &format!("echo {marker}; sleep 30")]);
    command.env("TERM", "xterm-256color");

    let (terminal, mut output) = pty::spawn(command, 80, 24).expect("pty");
    let link = connect(&stored, device);
    let mirror = mirror_on(&link, "end-to-end").await;

    // Forward the terminal's output for the duration of the test.
    let pump = tokio::spawn(async move {
        while let Some(chunk) = output.chunks.recv().await {
            if !mirror.publish(chunk).await {
                break;
            }
        }
    });

    // Find the terminal the way a client would: by listing them.
    let client = api::Client::new(&stored.relay_url).expect("client");
    let token = identity_token;
    let mut terminal_id = String::new();
    for _ in 0..50 {
        let terminals = client.terminals(&token).await.expect("lists terminals");
        if let Some(found) = terminals.iter().find(|t| t.label == "end-to-end") {
            terminal_id = found.terminal_id.clone();
            assert!(
                found.accepts_input,
                "a publisher on version 2 must offer input, or the phone can never type"
            );
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!terminal_id.is_empty(), "the terminal was never published");

    let seen = mirror_until(
        &stored.relay_url,
        &token,
        &terminal_id,
        &marker,
        Duration::from_secs(10),
    )
    .await;
    assert!(
        seen.contains(&marker),
        "the subscriber never saw the terminal's output; got {seen:?}"
    );

    pump.abort();
    drop(terminal);
}

#[tokio::test]
async fn a_subscriber_can_type_into_the_terminal() {
    let Some(relay) = relay_url() else { return };
    let (stored, identity_token, device) = enroll(&relay).await;

    // An interactive shell with no prompt, so what comes back is the echo of what was
    // typed and its result, and nothing else to confuse the assertion.
    let mut command = CommandBuilder::new("/bin/sh");
    command.args(["-i"]);
    command.env("TERM", "dumb");
    command.env("PS1", "");

    let (terminal, mut output) = pty::spawn(command, 80, 24).expect("pty");
    let link = connect(&stored, device);
    let mut mirror = mirror_on(&link, "input-test").await;

    let client = api::Client::new(&stored.relay_url).expect("client");
    let token = identity_token;

    // Drive the terminal from the publisher side, and apply whatever a subscriber
    // sends — which is exactly what `hypeterm-publish run` does.
    let events = tokio::spawn(async move {
        loop {
            tokio::select! {
                chunk = output.chunks.recv() => {
                    let Some(chunk) = chunk else { break };
                    if !mirror.publish(chunk).await { break }
                }
                notice = mirror.notices.recv() => {
                    match notice {
                        Some(publish::Notice::Input(bytes)) => {
                            if !terminal.write(bytes).await { break }
                        }
                        Some(publish::Notice::Ended(_)) | None => break,
                        _ => {}
                    }
                }
            }
        }
    });

    let terminal_id = await_terminal(&client, &token, "input-test").await;
    let marker = "typed-round-trip";
    let mut stream = subscribe_and_type(
        &stored.relay_url,
        &token,
        &terminal_id,
        &format!("echo {marker}\n"),
    )
    .await;

    let mut seen = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(Ok(message)) = stream.next().await {
            if let Message::Binary(bytes) = message
                && bytes.len() >= 9
                && bytes[0] == protocol::FRAME_TYPE_OUTPUT
            {
                seen.extend_from_slice(&bytes[9..]);
            }
            // The shell echoes the command and then its output, so the marker appears
            // twice once it has actually run.
            if String::from_utf8_lossy(&seen).matches(marker).count() >= 2 {
                break;
            }
        }
    })
    .await;

    let text = String::from_utf8_lossy(&seen);
    assert!(
        text.matches(marker).count() >= 2,
        "the typed command never reached the shell; saw {text:?}"
    );

    events.abort();
}

#[tokio::test]
async fn several_terminals_are_mirrored_at_once_and_input_reaches_the_right_one() {
    let Some(relay) = relay_url() else { return };
    let (stored, identity_token, device) = enroll(&relay).await;

    // The whole point of the daemon: one publisher connection, several terminals. A
    // device may hold only one connection (relay spec §6.1), and before this a second
    // terminal took the device over from the first — which stopped mirroring while the
    // relay went on routing input to the *device*, so a phone attached to the older
    // one typed into nothing, silently and with no error anywhere.
    let link = connect(&stored, device);

    let mut shells = Vec::new();
    for name in ["alpha", "beta"] {
        let mut command = CommandBuilder::new("/bin/sh");
        command.args(["-i"]);
        command.env("TERM", "dumb");
        command.env("PS1", "");
        let (terminal, mut output) = pty::spawn(command, 80, 24).expect("pty");
        let mut mirror = mirror_on(&link, name).await;
        let pump = tokio::spawn(async move {
            loop {
                tokio::select! {
                    chunk = output.chunks.recv() => {
                        let Some(chunk) = chunk else { break };
                        if !mirror.publish(chunk).await { break }
                    }
                    notice = mirror.notices.recv() => {
                        match notice {
                            Some(publish::Notice::Input(bytes)) => {
                                if !terminal.write(bytes).await { break }
                            }
                            Some(publish::Notice::Ended(_)) | None => break,
                            _ => {}
                        }
                    }
                }
            }
        });
        shells.push(pump);
    }

    let client = api::Client::new(&stored.relay_url).expect("client");
    let token = identity_token;
    let alpha = await_terminal(&client, &token, "alpha").await;
    let beta = await_terminal(&client, &token, "beta").await;
    assert_ne!(alpha, beta, "two shells must be two terminals");

    let open: Vec<String> = client
        .terminals(&token)
        .await
        .expect("lists terminals")
        .into_iter()
        .filter(|t| t.state == "open")
        .map(|t| t.label)
        .collect();
    assert!(
        open.contains(&"alpha".to_string()) && open.contains(&"beta".to_string()),
        "both terminals must be open at once, got {open:?}"
    );

    // Type into beta only. Before multiplexing, input for anything but the one
    // terminal the live connection happened to own was dropped on the floor.
    let marker = "only-in-beta";
    let mut stream = subscribe_and_type(
        &stored.relay_url,
        &token,
        &beta,
        &format!("echo {marker}\n"),
    )
    .await;

    let mut seen = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(Ok(message)) = stream.next().await {
            if let Message::Binary(bytes) = message
                && bytes.len() >= 9
                && bytes[0] == protocol::FRAME_TYPE_OUTPUT
            {
                seen.extend_from_slice(&bytes[9..]);
            }
            if String::from_utf8_lossy(&seen).matches(marker).count() >= 2 {
                break;
            }
        }
    })
    .await;
    let text = String::from_utf8_lossy(&seen);
    assert!(
        text.matches(marker).count() >= 2,
        "the keystrokes never reached beta's shell; saw {text:?}"
    );

    // And alpha must not have seen a byte of it: routing input by device rather than
    // by terminal is exactly the bug this replaces.
    let alpha_output = mirror_until(
        &stored.relay_url,
        &token,
        &alpha,
        marker,
        Duration::from_secs(2),
    )
    .await;
    assert!(
        !alpha_output.contains(marker),
        "beta's keystrokes reached alpha's shell: {alpha_output:?}"
    );

    for pump in shells {
        pump.abort();
    }
}

// --------------------------------------------- the daemon, killed mid-stream

/// The terminal_id of the first terminal whose label starts with `prefix`, or `None`
/// if none appears in time.
///
/// A prefix, because `run` appends the working directory and process id so that a row
/// of tabs is tellable apart. Bounded, and `None` rather than a panic, so a test that
/// does not get what it expected can say why — with the child's own stderr — instead
/// of waiting for ever.
async fn await_terminal_prefixed(
    client: &api::Client,
    token: &str,
    prefix: &str,
) -> Option<String> {
    for _ in 0..150 {
        let terminals = client.terminals(token).await.expect("lists terminals");
        if let Some(found) = terminals.iter().find(|t| t.label.starts_with(prefix)) {
            return Some(found.terminal_id.clone());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    None
}

/// Reads a terminal the way the Android client does, checking as it goes that the
/// stream it is handed is contiguous.
///
/// The offset check is the point. Comparing the bytes at the end would catch output
/// that went missing, but a stream that skipped forward and one that repeated itself
/// can both still contain every line — and at the protocol level both are corruption
/// that no subscriber could ever recover from. Each frame must begin exactly where the
/// last one ended.
async fn collect_contiguous(
    relay_url: &str,
    token: &str,
    terminal_id: &str,
    want_bytes: usize,
    within: Duration,
) -> Result<Vec<u8>, String> {
    let url = format!(
        "{}/v1/terminals/{terminal_id}/mirror",
        relay_url
            .replacen("https://", "wss://", 1)
            .replacen("http://", "ws://", 1)
    );
    let mut request = url.as_str().into_client_request().expect("request");
    request.headers_mut().insert(
        "Authorization",
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    );
    request.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        HeaderValue::from_static(MIRROR_V2),
    );
    let (mut stream, _) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|e| format!("the mirror would not connect: {e}"))?;
    stream
        .send(Message::Text(
            r#"{"type":"subscribe","from_offset":0}"#.into(),
        ))
        .await
        .map_err(|e| format!("subscribing: {e}"))?;

    let mut seen: Vec<u8> = Vec::new();
    let outcome = tokio::time::timeout(within, async {
        while let Some(Ok(message)) = stream.next().await {
            match message {
                Message::Binary(bytes) if bytes.len() >= 9 => {
                    if bytes[0] != protocol::FRAME_TYPE_OUTPUT {
                        continue;
                    }
                    let start = u64::from_be_bytes(bytes[1..9].try_into().unwrap());
                    if start != seen.len() as u64 {
                        return Err(format!(
                            "the stream is not contiguous: a frame starts at {start} \
                             with {} bytes already read",
                            seen.len()
                        ));
                    }
                    seen.extend_from_slice(&bytes[9..]);
                    if seen.len() >= want_bytes {
                        return Ok(());
                    }
                }
                // A gap means the relay could not give a contiguous stream at all,
                // which is the failure this whole arrangement exists to avoid.
                Message::Text(text) if text.contains("\"gap\"") => {
                    return Err(format!("the relay reported a gap: {text}"));
                }
                _ => {}
            }
        }
        Err("the mirror closed before the output was complete".to_string())
    })
    .await;

    match outcome {
        Ok(Ok(())) => Ok(seen),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(format!(
            "timed out with {} of {want_bytes} bytes",
            seen.len()
        )),
    }
}

/// A temporary directory short enough to hold a unix socket path.
///
/// `sun_path` is 108 bytes, and a socket lives under this directory with a sixteen-hex
/// name, so a deep `$TMPDIR` — a build sandbox's, typically — cannot be used. The
/// daemon refuses such a path rather than binding a truncated one, so a test that
/// ignored this would fail as "nothing was ever published" and say nothing about why.
fn short_tempdir() -> tempfile::TempDir {
    for base in [std::env::temp_dir(), std::path::PathBuf::from("/tmp")] {
        if base.as_os_str().len() > 40 {
            continue;
        }
        if let Ok(dir) = tempfile::Builder::new()
            .prefix("hypeterm-test")
            .tempdir_in(&base)
        {
            return dir;
        }
    }
    panic!("no temporary directory short enough for a unix socket path");
}

/// Stops a hosted terminal and any daemon it started.
///
/// Used on every exit from the test below, not only the successful one: a daemon
/// outlives the `run` that spawned it by design, so a test that failed early and left
/// one behind would hold a relay connection for this device until it idled out.
async fn stop_everything(child: &mut std::process::Child, paths: &hypeterm_publish::ipc::Paths) {
    let _ = child.kill();
    let _ = child.wait();
    if let Some(survivor) = hypeterm_publish::daemon::probe(paths).await {
        kill_now(survivor.pid);
    }
}

/// Kills a process outright and waits for it to be gone.
fn kill_now(pid: u32) {
    let killed = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .expect("kill runs");
    assert!(killed.success(), "could not kill pid {pid}");
}

/// The daemon is killed halfway through a terminal's output, and the stream survives.
///
/// This is the property the whole arrangement is built around, and the reason the
/// retained bytes live in `run` rather than in the daemon: if they lived in the daemon,
/// the daemon dying would leave the relay's offsets contiguous while the bytes behind
/// them were gone — a hole that nothing downstream could ever detect. So the assertion
/// is not "the terminal came back" but "every byte the shell wrote arrived, once, in
/// order, in one unbroken stream".
///
/// Unix only: there is no daemon on Windows (`run` publishes directly there).
#[cfg(unix)]
#[tokio::test]
async fn a_terminal_survives_its_daemon_being_killed_without_a_gap() {
    use std::os::unix::fs::PermissionsExt as _;

    let Some(relay) = relay_url() else { return };
    let (stored, identity_token, _device) = enroll(&relay).await;

    // A runtime directory of this test's own, so it can neither find nor disturb a
    // daemon the developer is actually using. `run` accepts it through the same
    // XDG_RUNTIME_DIR the real thing prefers.
    let home = short_tempdir();
    let runtime = home.path().join("run");
    std::fs::create_dir_all(&runtime).expect("runtime dir");
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).expect("0700");
    let state_file = home.path().join("publisher.json");
    state::save(&state_file, &stored).expect("writes the state file");
    let child_log = home.path().join("run.log");

    let paths =
        hypeterm_publish::ipc::Paths::in_dir(&runtime, &stored.relay_url, &stored.device_id)
            .expect("socket paths");

    // Ten lines, a pause long enough to kill the daemon in, then ten more. What the
    // second half proves is that a shell writing *after* its daemon died still reaches
    // the same stream, at the offset the first half left off.
    let script = "i=0; while [ $i -lt 10 ]; do echo line-$i; i=$((i+1)); done; \
                  sleep 5; \
                  while [ $i -lt 20 ]; do echo line-$i; i=$((i+1)); done";
    let label = format!("daemon-kill-{}", uuid::Uuid::new_v4());
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_hypeterm-publish"))
        .arg("--state-file")
        .arg(&state_file)
        .args(["run", "--label", &label, "--"])
        .args(["/bin/sh", "-c", script])
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("HYPETERM_LOG", "warn")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        // Kept, not discarded: when this goes wrong it goes wrong as "nothing was ever
        // published", and the reason is always in here.
        .stderr(std::fs::File::create(&child_log).expect("child log"))
        .spawn()
        .expect("starts hypeterm-publish run");

    let client = api::Client::new(&stored.relay_url).expect("client");
    let token = identity_token;
    let Some(terminal_id) = await_terminal_prefixed(&client, &token, &label).await else {
        let _ = child.kill();
        panic!(
            "the terminal was never published; the run said: {}",
            std::fs::read_to_string(&child_log).unwrap_or_default()
        );
    };

    // Subscribed before the daemon dies and never reconnected, so a gap or a repeat
    // would have to get past this one connection.
    let expected: String = (0..20).map(|i| format!("line-{i}\r\n")).collect();
    let reading = tokio::spawn({
        let relay_url = stored.relay_url.clone();
        let token = token.clone();
        let terminal_id = terminal_id.clone();
        let want = expected.len();
        async move {
            collect_contiguous(
                &relay_url,
                &token,
                &terminal_id,
                want,
                Duration::from_secs(30),
            )
            .await
        }
    });

    // Let the first half get out, then take the daemon away.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let running = hypeterm_publish::daemon::probe(&paths)
        .await
        .expect("run started a daemon");
    kill_now(running.pid);

    let outcome = reading.await.expect("the reader task finishes");
    // Before the assertions, so that a failing one still tidies up.
    stop_everything(&mut child, &paths).await;

    let seen = match outcome {
        Ok(seen) => seen,
        Err(error) => panic!(
            "the stream did not survive the daemon's death: {error}\nthe run said: {}",
            std::fs::read_to_string(&child_log).unwrap_or_default()
        ),
    };
    assert_eq!(
        String::from_utf8_lossy(&seen),
        expected,
        "the shell's output did not survive its daemon being killed"
    );

    // That it is the *same* terminal rather than a second one standing in for it needs
    // no separate check, and could not honestly be made with one: the relay lists open
    // terminals, and by now this one has closed — asking again would wait for ever. It
    // is already proved above. The subscription was opened on this terminal_id before
    // the daemon died and never reconnected, so every one of those bytes arrived on
    // it; a replacement terminal would have started at offset zero somewhere else and
    // this collector would have timed out holding only the first half.
}
