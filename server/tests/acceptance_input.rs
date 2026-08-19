//! Acceptance criteria for bidirectional terminal input (spec §11 items 19-23).
//!
//! These cover the protocol version 2 additions: input reaching the publishing
//! device, the four independent authorization conditions of §4.5, the guarantee that
//! input never touches durable state, and the promise that version 1 peers are
//! unaffected.

mod support;

use serde_json::json;
use std::time::Duration;
use support::{Api, Key, Mirror, OPERATOR_TOKEN, Publisher, TestServer, eventually};
use terminal_relay::settings::defs::keys;

// --------------------------------------------------------------- criterion 19

#[tokio::test(flavor = "multi_thread")]
async fn criterion_19_input_reaches_the_publisher_and_its_echo_returns_as_output() {
    let server = TestServer::start().await;
    let api = Api::new(&server);
    let alice = api.provision().await;

    let mut publisher = Publisher::connect_v2(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    let terminal_id = publisher.open_input_terminal_id("pty0").await;

    let mut mirror = Mirror::connect_v2(&server, terminal_id, &alice.identity_token)
        .await
        .unwrap();
    let subscribed = mirror.subscribe(Some(0)).await;
    assert_eq!(subscribed["accepts_input"], json!(true));
    assert_eq!(
        subscribed["input_available"],
        json!(true),
        "every condition of spec §4.5 holds, so input should be available"
    );

    // Type a command, one frame per keystroke burst.
    mirror.send_input(1, b"ls -l").await;
    mirror.send_input(2, b"\r").await;

    // Both frames arrive at the device, in order, exactly once.
    let (id, first_seq, first) = publisher.next_input(Duration::from_secs(10)).await.unwrap();
    assert_eq!(id, terminal_id);
    assert_eq!(first_seq, 1, "the relay sequence starts at 1");
    assert_eq!(first, b"ls -l");

    let (_, second_seq, second) = publisher.next_input(Duration::from_secs(10)).await.unwrap();
    assert_eq!(second_seq, 2);
    assert_eq!(second, b"\r");

    // The client is told exactly how much was accepted.
    let ack = mirror
        .expect_message("input.ack", Duration::from_secs(10))
        .await;
    assert!(ack["accepted_through"].as_u64().unwrap() >= 1);

    // The device echoes what it received, and every subscriber sees it as output.
    publisher.send_output(terminal_id, 0, b"ls -l\r\n").await;
    let stream = mirror.collect(7, Duration::from_secs(10)).await;
    assert_eq!(
        stream.bytes, b"ls -l\r\n",
        "the echo returns through the output stream"
    );

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn input_bytes_are_conveyed_without_interpretation() {
    let server = TestServer::start().await;
    let api = Api::new(&server);
    let alice = api.provision().await;

    let mut publisher = Publisher::connect_v2(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    let terminal_id = publisher.open_input_terminal_id("pty0").await;
    let mut mirror = Mirror::connect_v2(&server, terminal_id, &alice.identity_token)
        .await
        .unwrap();
    mirror.subscribe(Some(0)).await;

    // Control bytes, escape sequences, invalid UTF-8 and a bracketed paste: all of it
    // is opaque to the relay.
    let payloads: Vec<Vec<u8>> = vec![
        vec![0x03],                           // Ctrl+C
        b"\x1b[A".to_vec(),                   // cursor up
        b"\x1b[200~pasted\x1b[201~".to_vec(), // bracketed paste
        vec![0xff, 0xfe, 0x00, 0x07],         // invalid UTF-8 and NUL
        "café ☕".as_bytes().to_vec(),        // multi-byte UTF-8
    ];

    for (index, payload) in payloads.iter().enumerate() {
        mirror.send_input(index as u64 + 1, payload).await;
    }

    for (index, expected) in payloads.iter().enumerate() {
        let (_, sequence, received) = publisher
            .next_input(Duration::from_secs(10))
            .await
            .unwrap_or_else(|| panic!("input frame {index} never arrived"));
        assert_eq!(sequence, index as u64 + 1);
        assert_eq!(&received, expected, "input must be delivered byte for byte");
    }

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn input_sequences_must_be_contiguous() {
    let server = TestServer::start().await;
    let api = Api::new(&server);
    let alice = api.provision().await;

    let mut publisher = Publisher::connect_v2(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    let terminal_id = publisher.open_input_terminal_id("pty0").await;
    let mut mirror = Mirror::connect_v2(&server, terminal_id, &alice.identity_token)
        .await
        .unwrap();
    mirror.subscribe(Some(0)).await;

    // A skipped sequence is refused and not delivered, so the client learns precisely
    // what the relay accepted rather than guessing (spec §6.3).
    mirror.send_input(5, b"skipped").await;
    let error = mirror
        .expect_message("error", Duration::from_secs(10))
        .await;
    assert_eq!(error["code"], "input_sequence_mismatch");
    assert!(error["message"].as_str().unwrap().contains('1'));

    // The correct sequence is still accepted afterwards.
    mirror.send_input(1, b"ok").await;
    let (_, _, delivered) = publisher.next_input(Duration::from_secs(10)).await.unwrap();
    assert_eq!(delivered, b"ok");

    // A replayed sequence is refused rather than duplicated.
    mirror.send_input(1, b"ok").await;
    let error = mirror
        .expect_message("error", Duration::from_secs(10))
        .await;
    assert_eq!(error["code"], "input_sequence_mismatch");

    server.shutdown().await;
}

// --------------------------------------------------------------- criterion 20

#[tokio::test(flavor = "multi_thread")]
async fn criterion_20_input_without_the_publisher_opt_in_is_refused() {
    let server = TestServer::start().await;
    let api = Api::new(&server);
    let alice = api.provision().await;

    let mut publisher = Publisher::connect_v2(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    // Opened *without* accepts_input: a broadcast-only terminal.
    let terminal_id = publisher.open_terminal_id("pty0").await;

    let mut mirror = Mirror::connect_v2(&server, terminal_id, &alice.identity_token)
        .await
        .unwrap();
    let subscribed = mirror.subscribe(Some(0)).await;
    assert_eq!(subscribed["accepts_input"], json!(false));
    assert_eq!(subscribed["input_available"], json!(false));

    mirror.send_input(1, b"whoami\r").await;
    let error = mirror
        .expect_message("error", Duration::from_secs(10))
        .await;
    assert_eq!(error["code"], "input_not_accepted");

    // Nothing reached the device.
    assert!(
        publisher
            .next_input(Duration::from_millis(500))
            .await
            .is_none(),
        "a terminal that never opted in must not receive input"
    );

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn criterion_20_input_without_the_scope_is_refused() {
    let server = TestServer::start().await;
    // Remove the input scope from identity tokens: the caller can still read.
    server
        .patch_settings(vec![(
            keys::AUTH_IDENTITY_TOKEN_SCOPES,
            json!([
                "devices:read",
                "devices:write",
                "terminals:read",
                "terminals:mirror"
            ]),
        )])
        .expect("drop the input scope");

    let api = Api::new(&server);
    let alice = api.provision().await;

    let mut publisher = Publisher::connect_v2(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    let terminal_id = publisher.open_input_terminal_id("pty0").await;

    let mut mirror = Mirror::connect_v2(&server, terminal_id, &alice.identity_token)
        .await
        .unwrap();
    let subscribed = mirror.subscribe(Some(0)).await;
    // The terminal accepts input, but this subscription may not send it.
    assert_eq!(subscribed["accepts_input"], json!(true));
    assert_eq!(subscribed["input_available"], json!(false));

    mirror.send_input(1, b"x").await;
    let error = mirror
        .expect_message("error", Duration::from_secs(10))
        .await;
    assert_eq!(error["code"], "input_forbidden");
    assert!(
        publisher
            .next_input(Duration::from_millis(500))
            .await
            .is_none()
    );

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn criterion_20_an_operator_can_disable_input_on_live_connections() {
    let server = TestServer::start().await;
    let api = Api::new(&server);
    let alice = api.provision().await;

    let mut publisher = Publisher::connect_v2(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    let terminal_id = publisher.open_input_terminal_id("pty0").await;
    let mut mirror = Mirror::connect_v2(&server, terminal_id, &alice.identity_token)
        .await
        .unwrap();
    mirror.subscribe(Some(0)).await;

    mirror.send_input(1, b"before").await;
    let (_, _, delivered) = publisher.next_input(Duration::from_secs(10)).await.unwrap();
    assert_eq!(delivered, b"before");

    // Input is a security control, so disabling it takes effect on the connection
    // that is already open rather than at its next reconnect (spec §4.5).
    server
        .patch_settings(vec![(keys::FEATURES_INPUT_ENABLED, json!(false))])
        .expect("disable input");

    mirror.send_input(2, b"after").await;
    let error = mirror
        .expect_message("error", Duration::from_secs(10))
        .await;
    assert_eq!(error["code"], "input_disabled");
    assert!(
        publisher
            .next_input(Duration::from_millis(500))
            .await
            .is_none()
    );

    // Output still flows: only the input direction was withdrawn.
    publisher
        .send_output(terminal_id, 0, b"still readable")
        .await;
    let stream = mirror.collect(14, Duration::from_secs(10)).await;
    assert_eq!(stream.bytes, b"still readable");

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn criterion_20_input_is_never_queued_for_a_disconnected_publisher() {
    let server = TestServer::start().await;
    server
        .patch_settings(vec![(
            keys::TERMINAL_PUBLISHER_RECONNECT_GRACE_SECONDS,
            json!(3600),
        )])
        .expect("long grace so the terminal stays open");

    let api = Api::new(&server);
    let alice = api.provision().await;

    let mut publisher = Publisher::connect_v2(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    let terminal_id = publisher.open_input_terminal_id("pty0").await;
    let mut mirror = Mirror::connect_v2(&server, terminal_id, &alice.identity_token)
        .await
        .unwrap();
    mirror.subscribe(Some(0)).await;

    // The device drops off; the terminal stays open through its grace period.
    drop(publisher);
    let unavailable = eventually(Duration::from_secs(15), || async {
        !server
            .state
            .registry
            .publisher_accepts_input(alice.device_id)
    })
    .await;
    assert!(unavailable, "the publisher slot should be released");

    mirror.send_input(1, b"into the void").await;
    let error = mirror
        .expect_message("error", Duration::from_secs(10))
        .await;
    assert_eq!(error["code"], "input_undeliverable");

    // Reconnect: the refused keystroke must NOT have been buffered and replayed.
    let device_token = api.device_token(&alice.device_key).await;
    let mut publisher = Publisher::connect_v2(&server, alice.device_id, &device_token)
        .await
        .unwrap();
    publisher.open_input_terminal_id("pty0").await;
    assert!(
        publisher
            .next_input(Duration::from_millis(750))
            .await
            .is_none(),
        "input must never be queued for a disconnected publisher"
    );

    // Fresh input after the reconnect works, starting a new client sequence.
    let mut mirror = Mirror::connect_v2(&server, terminal_id, &alice.identity_token)
        .await
        .unwrap();
    mirror.subscribe(Some(0)).await;
    mirror.send_input(1, b"live again").await;
    let (_, _, delivered) = publisher.next_input(Duration::from_secs(10)).await.unwrap();
    assert_eq!(delivered, b"live again");

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn criterion_20_a_non_owner_can_neither_read_nor_write() {
    let server = TestServer::start().await;
    let api = Api::new(&server);
    let alice = api.provision().await;
    let bob = api.provision().await;

    let mut publisher = Publisher::connect_v2(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    let terminal_id = publisher.open_input_terminal_id("pty0").await;

    // Bob cannot even open a version 2 mirror on Alice's terminal.
    assert!(
        Mirror::connect_v2(&server, terminal_id, &bob.identity_token)
            .await
            .is_err(),
        "a non-owner must not reach another identity's terminal"
    );

    // Bob's publisher device cannot masquerade as a writer either.
    assert!(
        Mirror::connect_v2(&server, terminal_id, &bob.device_token)
            .await
            .is_err()
    );

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn oversized_and_empty_input_frames_are_refused() {
    let server = TestServer::start().await;
    server
        .patch_settings(vec![(keys::LIMITS_MAX_INPUT_FRAME_BYTES, json!(64))])
        .expect("shrink the input frame limit");

    let api = Api::new(&server);
    let alice = api.provision().await;
    let mut publisher = Publisher::connect_v2(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    let terminal_id = publisher.open_input_terminal_id("pty0").await;
    let mut mirror = Mirror::connect_v2(&server, terminal_id, &alice.identity_token)
        .await
        .unwrap();
    mirror.subscribe(Some(0)).await;

    mirror.send_input(1, &[b'x'; 128]).await;
    let error = mirror
        .expect_message("error", Duration::from_secs(10))
        .await;
    assert_eq!(error["code"], "limit_exceeded");

    mirror.send_input(1, b"").await;
    let error = mirror
        .expect_message("error", Duration::from_secs(10))
        .await;
    assert_eq!(error["code"], "invalid_message");

    // The connection stays usable, and the sequence was not consumed by refusals.
    mirror.send_input(1, b"fits").await;
    let (_, _, delivered) = publisher.next_input(Duration::from_secs(10)).await.unwrap();
    assert_eq!(delivered, b"fits");

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn input_is_rate_limited_per_subscriber() {
    let server = TestServer::start().await;
    server
        .patch_settings(vec![(
            keys::RATELIMIT_INPUT_FRAMES_PER_MINUTE_PER_SUBSCRIBER,
            json!(60),
        )])
        .expect("tighten the input rate limit");

    let api = Api::new(&server);
    let alice = api.provision().await;
    let mut publisher = Publisher::connect_v2(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    let terminal_id = publisher.open_input_terminal_id("pty0").await;
    let mut mirror = Mirror::connect_v2(&server, terminal_id, &alice.identity_token)
        .await
        .unwrap();
    mirror.subscribe(Some(0)).await;

    // Burst past the bucket. The limit fails frames explicitly rather than dropping
    // them, because a silently discarded keystroke is invisible to the user.
    let mut refused = None;
    let mut sequence = 1u64;
    for _ in 0..120 {
        mirror.send_input(sequence, b"k").await;
        let reply = mirror.next_json().await.expect("a reply per frame");
        match reply["type"].as_str() {
            Some("input.ack") => sequence += 1,
            Some("error") => {
                refused = Some(reply);
                break;
            }
            other => panic!("unexpected reply: {other:?}"),
        }
    }

    let refused = refused.expect("the input rate limit should engage");
    assert_eq!(refused["code"], "rate_limited");

    server.shutdown().await;
}

// --------------------------------------------------------------- criterion 21

#[tokio::test(flavor = "multi_thread")]
async fn criterion_21_input_never_enters_the_replay_buffer_or_durable_state() {
    let server = TestServer::start().await;
    let api = Api::new(&server);
    let alice = api.provision().await;

    let mut publisher = Publisher::connect_v2(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    let terminal_id = publisher.open_input_terminal_id("pty0").await;
    let mut mirror = Mirror::connect_v2(&server, terminal_id, &alice.identity_token)
        .await
        .unwrap();
    mirror.subscribe(Some(0)).await;

    // A password is exactly the case that must never be retained anywhere.
    mirror.send_input(1, b"hunter2\r").await;
    publisher.next_input(Duration::from_secs(10)).await.unwrap();

    // Only the terminal's own output advances offsets.
    publisher.send_output(terminal_id, 0, b"Password: ").await;
    publisher.wait_ack(terminal_id, 10).await;

    let response = api
        .get(
            &format!("/v1/terminals/{terminal_id}"),
            Some(&alice.identity_token),
        )
        .await;
    assert_eq!(
        response.body["next_offset"], 10,
        "input must not advance any offset"
    );
    assert_eq!(response.body["retained_bytes"], 10);

    // Nothing resembling the input reached durable storage.
    api.post("/v1/admin/flush", Some(OPERATOR_TOKEN), &json!({}))
        .await;
    let conn = rusqlite::Connection::open(server.data_dir.join("relay.db")).unwrap();
    let mut stmt = conn
        .prepare("SELECT bytes FROM terminal_output WHERE terminal_id = ?1")
        .unwrap();
    let rows: Vec<Vec<u8>> = stmt
        .query_map(rusqlite::params![terminal_id.to_string()], |row| {
            row.get::<_, Vec<u8>>(0)
        })
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    for chunk in &rows {
        assert!(
            !chunk.windows(7).any(|w| w == b"hunter2"),
            "terminal input must never be persisted"
        );
    }
    assert_eq!(rows.concat(), b"Password: ");

    // A reconnecting subscriber replays output only.
    let mut replay = Mirror::connect_v2(&server, terminal_id, &alice.identity_token)
        .await
        .unwrap();
    replay.subscribe(Some(0)).await;
    let stream = replay.collect(10, Duration::from_secs(10)).await;
    assert_eq!(stream.bytes, b"Password: ");

    server.shutdown().await;
}

// --------------------------------------------------------------- criterion 22

#[tokio::test(flavor = "multi_thread")]
async fn criterion_22_version_1_peers_are_unaffected() {
    let server = TestServer::start().await;
    let api = Api::new(&server);
    let alice = api.provision().await;

    // A version 1 publisher cannot claim to accept input, because it has no frame in
    // which input could ever be delivered (spec §6.1).
    let mut v1_publisher = Publisher::connect(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    let refused = v1_publisher.open_terminal_accepting_input("pty0").await;
    assert_eq!(refused["type"], "error");
    assert_eq!(refused["code"], "validation_failed");

    // Its ordinary terminals still work exactly as before.
    let terminal_id = v1_publisher.open_terminal_id("pty1").await;
    v1_publisher.send_output(terminal_id, 0, b"unchanged").await;
    v1_publisher.wait_ack(terminal_id, 9).await;

    // A version 1 mirror sees no input fields at all.
    let mut v1_mirror = Mirror::connect(&server, terminal_id, &alice.identity_token)
        .await
        .unwrap();
    let subscribed = v1_mirror.subscribe(Some(0)).await;
    assert!(
        subscribed.get("accepts_input").is_none(),
        "version 1 must not see version 2 fields: {subscribed:?}"
    );
    assert!(subscribed.get("input_available").is_none());

    let stream = v1_mirror.collect(9, Duration::from_secs(10)).await;
    assert_eq!(stream.bytes, b"unchanged");

    // A version 1 mirror sending a binary frame is still a protocol violation.
    v1_mirror.send_input(1, b"nope").await;
    let stream = v1_mirror.drain(Duration::from_secs(20)).await;
    assert_eq!(stream.close_code, Some(1002));

    // A version 2 mirror on a version 1 publisher's terminal attaches read-only
    // rather than failing, so a client can attach and say so in its UI.
    let mut v2_mirror = Mirror::connect_v2(&server, terminal_id, &alice.identity_token)
        .await
        .unwrap();
    let subscribed = v2_mirror.subscribe(Some(0)).await;
    assert_eq!(subscribed["type"], "subscribed");
    assert_eq!(subscribed["accepts_input"], json!(false));
    assert_eq!(subscribed["input_available"], json!(false));

    server.shutdown().await;
}

// --------------------------------------------------------------- criterion 23

#[tokio::test(flavor = "multi_thread")]
async fn criterion_23_a_client_device_works_without_the_root_key() {
    let server = TestServer::start().await;
    let api = Api::new(&server);

    let identity_key = Key::random();
    let identity_id = api.identity_id(&identity_key).await;
    let identity_token = api.identity_token(&identity_key).await;

    // The machine that runs the shell.
    let publisher_key = Key::random();
    let publisher_device = api
        .device_id(&identity_token, &identity_id, &publisher_key, "workstation")
        .await;
    let publisher_token = api.device_token(&publisher_key).await;

    // The phone: its own key, registered as a client. The identity's root private key
    // never leaves the owner's machine (spec §3.2).
    let (_client_key, client_device, client_token) =
        api.client_device(&identity_token, &identity_id).await;

    let mut publisher = Publisher::connect_v2(&server, publisher_device, &publisher_token)
        .await
        .unwrap();
    let terminal_id = publisher.open_input_terminal_id("pty0").await;
    publisher.send_output(terminal_id, 0, b"$ ").await;

    // The phone mirrors and writes using only its own credential.
    let mut mirror = Mirror::connect_v2(&server, terminal_id, &client_token)
        .await
        .unwrap();
    let subscribed = mirror.subscribe(Some(0)).await;
    assert_eq!(subscribed["input_available"], json!(true));

    let stream = mirror.collect(2, Duration::from_secs(10)).await;
    assert_eq!(stream.bytes, b"$ ");

    mirror.send_input(1, b"uptime\r").await;
    let (_, _, delivered) = publisher.next_input(Duration::from_secs(10)).await.unwrap();
    assert_eq!(delivered, b"uptime\r");

    // A client device holds no publishing or device-management authority.
    assert!(
        Publisher::connect_v2(&server, client_device, &client_token)
            .await
            .is_err(),
        "a client-role device must not be able to publish"
    );
    let listed = api.get("/v1/devices", Some(&client_token)).await;
    assert_eq!(listed.status, 403);

    // Revoking the phone ends its access without touching the workstation.
    let revoke = api
        .delete(
            &format!("/v1/devices/{client_device}"),
            Some(&identity_token),
        )
        .await;
    assert_eq!(revoke.status, 200);

    assert!(
        Mirror::connect_v2(&server, terminal_id, &client_token)
            .await
            .is_err(),
        "a revoked client must lose access immediately"
    );
    // The publisher is untouched.
    publisher.send_output(terminal_id, 2, b"ok").await;
    publisher.wait_ack(terminal_id, 4).await;

    server.shutdown().await;
}

// ----------------------------------------------------------------- resize path

#[tokio::test(flavor = "multi_thread")]
async fn a_client_may_request_a_resize_which_the_publisher_decides() {
    let server = TestServer::start().await;
    let api = Api::new(&server);
    let alice = api.provision().await;

    let mut publisher = Publisher::connect_v2(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    let terminal_id = publisher.open_input_terminal_id("pty0").await;
    let mut mirror = Mirror::connect_v2(&server, terminal_id, &alice.identity_token)
        .await
        .unwrap();
    mirror.subscribe(Some(0)).await;

    // The phone rotates and asks for a size that fits.
    mirror
        .send_json(&json!({ "type": "terminal.resize_request", "cols": 100, "rows": 30 }))
        .await;

    let forwarded = publisher
        .expect_message("terminal.resize_request", Duration::from_secs(10))
        .await;
    assert_eq!(forwarded["cols"], 100);
    assert_eq!(forwarded["rows"], 30);
    assert_eq!(forwarded["terminal_id"], json!(terminal_id));

    // The publisher remains the authority: it applies the size and everyone sees it.
    publisher
        .send_json(&json!({
            "type": "terminal.resize",
            "terminal_id": terminal_id,
            "cols": 100,
            "rows": 30,
        }))
        .await;
    let resize = mirror
        .expect_message("terminal.resize", Duration::from_secs(10))
        .await;
    assert_eq!(resize["cols"], 100);

    // An operator can withdraw client-initiated resize on its own.
    server
        .patch_settings(vec![(keys::FEATURES_CLIENT_RESIZE_ENABLED, json!(false))])
        .expect("disable client resize");
    mirror
        .send_json(&json!({ "type": "terminal.resize_request", "cols": 80, "rows": 24 }))
        .await;
    let error = mirror
        .expect_message("error", Duration::from_secs(10))
        .await;
    assert_eq!(error["code"], "resize_refused");

    server.shutdown().await;
}

// ------------------------------------------------------------- settings guards

#[tokio::test(flavor = "multi_thread")]
async fn scope_settings_cannot_grant_authority_a_role_may_not_hold() {
    let server = TestServer::start().await;

    // A publisher must never gain input or identity-level authority.
    let escalated = server.patch_settings(vec![(
        keys::AUTH_DEVICE_TOKEN_SCOPES,
        json!(["terminals:publish", "terminals:input"]),
    )]);
    assert!(escalated.is_err());
    assert!(escalated.unwrap_err().contains("terminals:input"));

    // A client must never gain the ability to publish or manage devices.
    let publishing_client = server.patch_settings(vec![(
        keys::AUTH_CLIENT_TOKEN_SCOPES,
        json!(["terminals:mirror", "terminals:publish"]),
    )]);
    assert!(publishing_client.is_err());

    let managing_client = server.patch_settings(vec![(
        keys::AUTH_CLIENT_TOKEN_SCOPES,
        json!(["terminals:mirror", "devices:write"]),
    )]);
    assert!(managing_client.is_err());

    // An unknown scope is rejected outright.
    let unknown = server.patch_settings(vec![(
        keys::AUTH_IDENTITY_TOKEN_SCOPES,
        json!(["terminals:teleport"]),
    )]);
    assert!(unknown.is_err());

    server.shutdown().await;
}

// ------------------------------------------- spec §4.6: the capability handshake

#[tokio::test(flavor = "multi_thread")]
async fn a_version_1_publisher_cannot_assert_the_open_request_capability() {
    let server = TestServer::start().await;
    let api = Api::new(&server);
    let alice = api.provision().await;

    let mut publisher = Publisher::connect(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();

    // A version 1 connection has no channel the request could travel on, so the
    // assertion is refused rather than ignored — a machine must never believe it has
    // opted in to something the relay silently dropped.
    publisher
        .send_json(&json!({
            "type": "publisher.capabilities",
            "terminal_open_requests": true,
        }))
        .await;

    let error = publisher
        .expect_message("error", Duration::from_secs(5))
        .await;
    assert_eq!(error["code"], json!("validation_failed"));

    // ...and the connection survives it: a refused capability is not a protocol fault.
    let terminal_id = publisher.open_terminal_id("pty0").await;
    assert!(!terminal_id.is_nil());

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_open_request_capability_does_not_survive_a_reconnect() {
    let server = TestServer::start().await;
    let api = Api::new(&server);
    let alice = api.provision().await;

    let mut publisher = Publisher::connect_v2(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    publisher
        .send_json(&json!({
            "type": "publisher.capabilities",
            "terminal_open_requests": true,
        }))
        .await;
    // Round-trip something to be sure the assertion was processed before superseding.
    let _ = publisher.open_input_terminal_id("pty0").await;

    // The assertion describes the machine's policy *now*. A machine whose owner turned
    // it off between connections must not still be reachable on the strength of the
    // old one, so a fresh connection starts at "no" and has to say so again.
    let mut replacement = Publisher::connect_v2(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    let terminal_id = replacement.open_input_terminal_id("pty1").await;
    assert!(!terminal_id.is_nil());

    server.shutdown().await;
}
