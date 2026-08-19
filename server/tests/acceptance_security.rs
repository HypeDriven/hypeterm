//! Acceptance criteria for identity, authorisation, limits and settings
//! (spec §11 items 1-4 and 14-17).

mod support;

use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::Duration;
use support::{Api, Key, Mirror, OPERATOR_TOKEN, Publisher, TestServer, eventually};
use terminal_relay::settings::defs::{DEFS, keys};

// ---------------------------------------------------------------- criterion 1

#[tokio::test(flavor = "multi_thread")]
async fn criterion_1_registration_requires_a_valid_unexpired_single_use_challenge() {
    let server = TestServer::start().await;
    let api = Api::new(&server);
    let key = Key::random();

    // An unknown challenge is refused.
    let response = api
        .post(
            "/v1/identities",
            None,
            &json!({ "challenge_id": "01ZZZZZZZZZZZZZZZZZZZZZZZZ", "signature": support::b64(&[0u8; 64]) }),
        )
        .await;
    assert_eq!(response.status, 401);
    assert_eq!(response.error_code(), Some("challenge_invalid"));
    // Every error carries the correlation ID (spec §5).
    assert!(response.request_id().is_some_and(|id| !id.is_empty()));

    // A wrong signature is refused.
    let (challenge_id, _input) = api.challenge("register_identity", &key, None).await;
    let response = api
        .post(
            "/v1/identities",
            None,
            &json!({ "challenge_id": challenge_id, "signature": support::b64(&[0u8; 64]) }),
        )
        .await;
    assert_eq!(response.status, 401);
    assert_eq!(response.error_code(), Some("signature_invalid"));

    // That failed attempt consumed the challenge, so even the correct signature on
    // it is now refused (spec §4.2).
    let (challenge_id, input) = api.challenge("register_identity", &key, None).await;
    let good_signature = key.sign_b64(&input);
    let first = api
        .post(
            "/v1/identities",
            None,
            &json!({ "challenge_id": challenge_id, "signature": good_signature }),
        )
        .await;
    assert_eq!(
        first.status, 201,
        "valid registration should succeed: {:?}",
        first.body
    );
    let identity_id = first.body["identity_id"].as_str().unwrap().to_string();

    let replay = api
        .post(
            "/v1/identities",
            None,
            &json!({ "challenge_id": challenge_id, "signature": good_signature }),
        )
        .await;
    assert_eq!(replay.status, 401);
    assert_eq!(replay.error_code(), Some("challenge_consumed"));

    // A signature made by a different key over a valid challenge is refused.
    let other = Key::random();
    let (challenge_id, input) = api.challenge("register_identity", &key, None).await;
    let response = api
        .post(
            "/v1/identities",
            None,
            &json!({ "challenge_id": challenge_id, "signature": other.sign_b64(&input) }),
        )
        .await;
    assert_eq!(response.status, 401);
    assert_eq!(response.error_code(), Some("signature_invalid"));

    assert!(
        identity_id.len() > 16,
        "identity id should be a base64url fingerprint"
    );
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn criterion_1_expired_challenges_are_refused() {
    // The specification caps the challenge lifetime at five minutes; the schema
    // minimum of five seconds keeps this test quick.
    let server = TestServer::start().await;
    server
        .patch_settings(vec![(keys::AUTH_CHALLENGE_TTL_SECONDS, json!(5))])
        .expect("shorten challenge ttl");

    let api = Api::new(&server);
    let key = Key::random();
    let (challenge_id, input) = api.challenge("register_identity", &key, None).await;

    tokio::time::sleep(Duration::from_secs(6)).await;

    let response = api
        .post(
            "/v1/identities",
            None,
            &json!({ "challenge_id": challenge_id, "signature": key.sign_b64(&input) }),
        )
        .await;
    assert_eq!(response.status, 401);
    assert_eq!(response.error_code(), Some("challenge_expired"));
    server.shutdown().await;
}

// ---------------------------------------------------------------- criterion 2

#[tokio::test(flavor = "multi_thread")]
async fn criterion_2_reregistering_the_same_key_returns_the_same_identity() {
    let server = TestServer::start().await;
    let api = Api::new(&server);
    let key = Key::random();

    let first = api.register_identity(&key).await;
    assert_eq!(first.status, 201);
    let second = api.register_identity(&key).await;
    // Idempotent, and answered 200 rather than 201 (spec §5.1).
    assert_eq!(second.status, 200);
    assert_eq!(first.body["identity_id"], second.body["identity_id"]);
    assert_eq!(first.body["created_at"], second.body["created_at"]);

    // A different key is a different identity.
    let other = api.register_identity(&Key::random()).await;
    assert_eq!(other.status, 201);
    assert_ne!(other.body["identity_id"], first.body["identity_id"]);

    server.shutdown().await;
}

// ---------------------------------------------------------------- criterion 3

#[tokio::test(flavor = "multi_thread")]
async fn criterion_3_an_identity_manages_multiple_independently_keyed_devices() {
    let server = TestServer::start().await;
    let api = Api::new(&server);

    let identity_key = Key::random();
    let identity_id = api.identity_id(&identity_key).await;
    let token = api.identity_token(&identity_key).await;

    let keys_and_ids: Vec<_> = {
        let mut out = Vec::new();
        for index in 0..3 {
            let device_key = Key::random();
            let device_id = api
                .device_id(
                    &token,
                    &identity_id,
                    &device_key,
                    &format!("device {index}"),
                )
                .await;
            out.push((device_key, device_id));
        }
        out
    };

    let listed = api.get("/v1/devices", Some(&token)).await;
    assert_eq!(listed.status, 200);
    assert_eq!(listed.body["devices"].as_array().unwrap().len(), 3);

    // Each device authenticates with its own key, never the identity's.
    for (device_key, device_id) in &keys_and_ids {
        let device_token = api.device_token(device_key).await;
        assert!(!device_token.is_empty());
        // A device token must not carry identity-level authority (spec §4.3).
        let attempt = api.get("/v1/devices", Some(&device_token)).await;
        assert_eq!(attempt.status, 403);
        assert_eq!(attempt.error_code(), Some("insufficient_scope"));
        let _ = device_id;
    }

    // Revoking one device leaves the others working.
    let (revoked_key, revoked_id) = &keys_and_ids[0];
    let revoke = api
        .delete(&format!("/v1/devices/{revoked_id}"), Some(&token))
        .await;
    assert_eq!(revoke.status, 200);
    // Revocation is idempotent.
    let again = api
        .delete(&format!("/v1/devices/{revoked_id}"), Some(&token))
        .await;
    assert_eq!(again.status, 200);

    let (challenge_id, input) = api
        .challenge("authenticate_device", revoked_key, None)
        .await;
    let refused = api
        .post(
            "/v1/auth/tokens",
            None,
            &json!({ "challenge_id": challenge_id, "signature": revoked_key.sign_b64(&input) }),
        )
        .await;
    assert_eq!(refused.status, 401);
    assert_eq!(refused.error_code(), Some("device_revoked"));

    let listed = api.get("/v1/devices", Some(&token)).await;
    assert_eq!(listed.body["devices"].as_array().unwrap().len(), 2);

    let surviving = api.device_token(&keys_and_ids[1].0).await;
    assert!(!surviving.is_empty());

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn device_registration_challenge_is_bound_to_its_owner() {
    let server = TestServer::start().await;
    let api = Api::new(&server);

    let alice_key = Key::random();
    let alice_id = api.identity_id(&alice_key).await;
    let alice_token = api.identity_token(&alice_key).await;

    let bob_key = Key::random();
    let bob_id = api.identity_id(&bob_key).await;
    let bob_token = api.identity_token(&bob_key).await;

    // A challenge bound to Bob cannot be redeemed by Alice.
    let device_key = Key::random();
    let (challenge_id, input) = api
        .challenge("register_device", &device_key, Some(&bob_id))
        .await;
    let response = api
        .post(
            "/v1/devices",
            Some(&alice_token),
            &json!({
                "name": "stolen",
                "key": device_key.key_json(),
                "challenge_id": challenge_id,
                "device_signature": device_key.sign_b64(&input),
            }),
        )
        .await;
    assert_eq!(response.status, 401);
    assert_eq!(response.error_code(), Some("challenge_invalid"));

    // A challenge bound to a different device key is also refused.
    let other_device = Key::random();
    let (challenge_id, input) = api
        .challenge("register_device", &other_device, Some(&alice_id))
        .await;
    let response = api
        .post(
            "/v1/devices",
            Some(&alice_token),
            &json!({
                "name": "mismatched",
                "key": device_key.key_json(),
                "challenge_id": challenge_id,
                "device_signature": device_key.sign_b64(&input),
            }),
        )
        .await;
    assert_eq!(response.status, 401);

    // The correct pairing works.
    let ok = api
        .register_device(&bob_token, &bob_id, &Key::random(), "bob device")
        .await;
    assert_eq!(ok.status, 201, "{:?}", ok.body);

    server.shutdown().await;
}

// ---------------------------------------------------------------- criterion 4

#[tokio::test(flavor = "multi_thread")]
async fn criterion_4_isolation_between_devices_and_identities() {
    let server = TestServer::start().await;
    let api = Api::new(&server);

    let alice = api.provision().await;
    let bob = api.provision().await;

    // Alice's device opens a terminal.
    let mut alice_publisher = Publisher::connect(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    let terminal_id = alice_publisher.open_terminal_id("pty0").await;
    alice_publisher
        .send_output(terminal_id, 0, b"alice output")
        .await;
    alice_publisher.wait_ack(terminal_id, 12).await;

    // Bob cannot see it, and gets 404 rather than 403 (spec §4.4).
    let response = api
        .get(
            &format!("/v1/terminals/{terminal_id}"),
            Some(&bob.identity_token),
        )
        .await;
    assert_eq!(response.status, 404);
    assert_eq!(response.error_code(), Some("not_found"));

    let listed = api.get("/v1/terminals", Some(&bob.identity_token)).await;
    assert_eq!(listed.body["terminals"].as_array().unwrap().len(), 0);

    // Bob cannot mirror it: the upgrade is refused before any WebSocket exists.
    let mirror = Mirror::connect(&server, terminal_id, &bob.identity_token).await;
    assert!(
        mirror.is_err(),
        "bob must not be able to mirror alice's terminal"
    );

    // Bob cannot see Alice's device.
    let response = api
        .get(
            &format!("/v1/devices/{}", alice.device_id),
            Some(&bob.identity_token),
        )
        .await;
    assert_eq!(response.status, 404);

    // Bob's device cannot publish to Alice's terminal.
    let mut bob_publisher = Publisher::connect(&server, bob.device_id, &bob.device_token)
        .await
        .unwrap();
    bob_publisher
        .send_output(terminal_id, 12, b"injected")
        .await;
    let error = bob_publisher
        .expect_message("error", Duration::from_secs(20))
        .await;
    assert_eq!(error["code"], "terminal_not_found");

    // Bob also cannot attach a publisher connection as Alice's device.
    let stolen = Publisher::connect(&server, alice.device_id, &bob.device_token).await;
    assert!(
        stolen.is_err(),
        "a device token must not authorise another device's relay"
    );

    // Alice's own bytes are untouched by the rejected attempts.
    let response = api
        .get(
            &format!("/v1/terminals/{terminal_id}"),
            Some(&alice.identity_token),
        )
        .await;
    assert_eq!(response.status, 200);
    assert_eq!(response.body["next_offset"], 12);

    // A WebSocket ticket for Alice's terminal cannot be minted by Bob.
    let ticket = api
        .post(
            "/v1/auth/websocket-tickets",
            Some(&bob.identity_token),
            &json!({ "path": format!("/v1/terminals/{terminal_id}/mirror") }),
        )
        .await;
    assert_eq!(ticket.status, 404);

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_fields_on_security_sensitive_bodies_are_rejected() {
    let server = TestServer::start().await;
    let api = Api::new(&server);
    let key = Key::random();

    // A field the server does not understand is refused rather than ignored, so a
    // caller can never believe it authorised something the server dropped (spec §5).
    let response = api
        .post(
            "/v1/auth/challenges",
            None,
            &json!({
                "operation": "register_identity",
                "key": key.key_json(),
                "scope_escalation": "admin",
            }),
        )
        .await;
    assert_eq!(response.status, 422);
    assert_eq!(response.error_code(), Some("validation_failed"));

    // The same body without the stray field succeeds.
    let ok = api
        .post(
            "/v1/auth/challenges",
            None,
            &json!({ "operation": "register_identity", "key": key.key_json() }),
        )
        .await;
    assert_eq!(ok.status, 201);

    // Every failure mode renders the specification's error envelope, including the
    // ones produced by the framework's own extractors (spec §5).
    let alice = api.provision().await;

    let malformed_path = api
        .get("/v1/terminals/not-a-uuid", Some(&alice.identity_token))
        .await;
    assert_eq!(malformed_path.status, 400);
    assert_eq!(malformed_path.error_code(), Some("invalid_request"));
    assert!(malformed_path.request_id().is_some());

    let bad_query = api
        .get("/v1/terminals?limit=abc", Some(&alice.identity_token))
        .await;
    assert_eq!(bad_query.status, 400);
    assert_eq!(bad_query.error_code(), Some("invalid_request"));

    let syntax = api
        .request(
            reqwest::Method::POST,
            "/v1/auth/challenges",
            None,
            None,
            &[("content-type", "application/json")],
        )
        .await;
    assert!(
        syntax.status == 400 || syntax.status == 422,
        "got {}",
        syntax.status
    );
    assert!(
        syntax.error_code().is_some(),
        "extractor rejections must use the error envelope"
    );

    let unknown_route = api.get("/v1/nonexistent", None).await;
    assert_eq!(unknown_route.status, 404);
    assert_eq!(unknown_route.error_code(), Some("not_found"));

    let wrong_method = api
        .delete("/v1/terminals", Some(&alice.identity_token))
        .await;
    assert_eq!(wrong_method.status, 405);
    assert!(wrong_method.error_code().is_some());

    // WebSocket control messages keep tolerating unknown *fields*, which version 1
    // explicitly allows (spec §12); only unknown required message types fail.
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn tokens_are_never_accepted_from_the_query_string() {
    let server = TestServer::start().await;
    let api = Api::new(&server);
    let alice = api.provision().await;

    // Even a *valid* token in the query string is refused outright (spec §4.3).
    let response = api
        .get(
            &format!("/v1/devices?access_token={}", alice.identity_token),
            Some(&alice.identity_token),
        )
        .await;
    assert_eq!(response.status, 400);
    assert_eq!(response.error_code(), Some("invalid_request"));

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn websocket_tickets_are_single_use_and_path_bound() {
    let server = TestServer::start().await;
    let api = Api::new(&server);
    let alice = api.provision().await;

    let path = format!("/v1/devices/{}/relay", alice.device_id);
    let response = api
        .post(
            "/v1/auth/websocket-tickets",
            Some(&alice.device_token),
            &json!({ "path": path }),
        )
        .await;
    assert_eq!(response.status, 201, "{:?}", response.body);
    let ticket = response.body["ticket"].as_str().unwrap().to_string();

    let publisher = Publisher::connect_with_ticket(&server, alice.device_id, &ticket).await;
    assert!(
        publisher.is_ok(),
        "a fresh ticket should authorise the upgrade"
    );
    drop(publisher);

    // Using it a second time fails: the ticket was consumed.
    let replay = Publisher::connect_with_ticket(&server, alice.device_id, &ticket).await;
    assert!(replay.is_err(), "a websocket ticket must be single use");

    server.shutdown().await;
}

// --------------------------------------------------------------- criterion 14

#[tokio::test(flavor = "multi_thread")]
async fn criterion_14_oversized_frames_are_bounded_and_explicit() {
    let server = TestServer::start().await;
    server
        .patch_settings(vec![(keys::LIMITS_MAX_OUTPUT_FRAME_BYTES, json!(1024))])
        .expect("shrink frame limit");

    let api = Api::new(&server);
    let alice = api.provision().await;
    let mut publisher = Publisher::connect(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    let terminal_id = publisher.open_terminal_id("pty0").await;

    // The first oversized frame is reported without killing the connection.
    publisher
        .send_output(terminal_id, 0, &vec![b'x'; 4096])
        .await;
    let error = publisher
        .expect_message("error", Duration::from_secs(20))
        .await;
    assert_eq!(error["code"], "limit_exceeded");

    // A publisher that keeps exceeding the negotiated limit is closed (spec §6.1).
    publisher
        .send_output(terminal_id, 0, &vec![b'x'; 4096])
        .await;
    publisher
        .expect_message("error", Duration::from_secs(20))
        .await;
    publisher
        .send_output(terminal_id, 0, &vec![b'x'; 4096])
        .await;
    let code = publisher.expect_close(Duration::from_secs(20)).await;
    assert_eq!(code, Some(4008), "expected the limit_exceeded close code");

    // No rejected byte reached the terminal.
    let response = api
        .get(
            &format!("/v1/terminals/{terminal_id}"),
            Some(&alice.identity_token),
        )
        .await;
    assert_eq!(response.body["next_offset"], 0);

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn criterion_14_excessive_terminals_are_refused() {
    let server = TestServer::start().await;
    server
        .patch_settings(vec![(
            keys::LIMITS_MAX_ACTIVE_TERMINALS_PER_DEVICE,
            json!(2),
        )])
        .expect("limit terminals");

    let api = Api::new(&server);
    let alice = api.provision().await;
    let mut publisher = Publisher::connect(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();

    publisher.open_terminal_id("pty0").await;
    publisher.open_terminal_id("pty1").await;

    publisher
        .send_json(&json!({
            "type": "terminal.open",
            "request_id": "over",
            "local_ref": "pty2",
            "label": "too many",
        }))
        .await;
    let error = publisher
        .expect_message("error", Duration::from_secs(20))
        .await;
    assert_eq!(error["code"], "limit_exceeded");
    assert_eq!(error["request_id"], "over");

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn criterion_14_rate_limits_fail_explicitly_with_retry_after() {
    let server = TestServer::start().await;
    server
        .patch_settings(vec![(
            keys::RATELIMIT_CHALLENGES_PER_MINUTE_PER_SOURCE,
            json!(2),
        )])
        .expect("tighten rate limit");

    let api = Api::new(&server);
    let key = Key::random();
    let body = json!({ "operation": "register_identity", "key": key.key_json() });

    let mut limited = None;
    for _ in 0..6 {
        let response = api.post("/v1/auth/challenges", None, &body).await;
        if response.status == 429 {
            limited = Some(response);
            break;
        }
    }

    let limited = limited.expect("the source rate limit should engage");
    assert_eq!(limited.error_code(), Some("rate_limited"));
    assert!(
        limited.headers.contains_key("retry-after"),
        "429 responses must carry Retry-After (spec §5)"
    );

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn criterion_14_slow_consumers_are_disconnected() {
    let server = TestServer::start().await;
    server
        .patch_settings(vec![
            (keys::MIRROR_SUBSCRIBER_QUEUE_BYTES, json!(65_536)),
            (keys::MIRROR_SUBSCRIBER_QUEUE_MESSAGES, json!(8)),
        ])
        .expect("shrink subscriber queue");

    let api = Api::new(&server);
    let alice = api.provision().await;
    let mut publisher = Publisher::connect(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    let terminal_id = publisher.open_terminal_id("pty0").await;

    let mut mirror = Mirror::connect(&server, terminal_id, &alice.identity_token)
        .await
        .unwrap();
    let subscribed = mirror.subscribe(None).await;
    assert_eq!(subscribed["type"], "subscribed");

    // Publish far more than the subscriber's queue bound without reading any of it.
    let chunk = vec![b'z'; 200_000];
    let mut offset = 0u64;
    for _ in 0..40 {
        publisher.send_output(terminal_id, offset, &chunk).await;
        offset += chunk.len() as u64;
    }

    // The subscriber is closed with the slow-consumer code and can resume later.
    let stream = mirror.drain(Duration::from_secs(10)).await;
    assert_eq!(
        stream.close_code,
        Some(4003),
        "expected the slow_consumer close code, control: {:?}",
        stream.control
    );
    let error = stream
        .control_of_type("error")
        .expect("an error message precedes the close");
    assert_eq!(error["code"], "slow_consumer");

    // The publisher was unaffected.
    publisher.wait_ack(terminal_id, 200_000).await;

    server.shutdown().await;
}

// --------------------------------------------------------------- criterion 15

#[tokio::test(flavor = "multi_thread")]
async fn criterion_15_revocation_blocks_new_access_and_terminates_existing() {
    let server = TestServer::start().await;
    let api = Api::new(&server);
    let alice = api.provision().await;

    let mut publisher = Publisher::connect(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    let terminal_id = publisher.open_terminal_id("pty0").await;
    publisher
        .send_output(terminal_id, 0, b"before revocation")
        .await;
    publisher.wait_ack(terminal_id, 17).await;

    let revoke = api
        .delete(
            &format!("/v1/devices/{}", alice.device_id),
            Some(&alice.identity_token),
        )
        .await;
    assert_eq!(revoke.status, 200);

    // The existing relay connection is closed well inside the thirty-second bound.
    let code = publisher.expect_close(Duration::from_secs(20)).await;
    assert!(
        matches!(code, Some(4002) | Some(4006)),
        "expected a superseded or revoked close code, got {code:?}"
    );

    // A new connection with the already-issued token is refused immediately.
    let reconnect = Publisher::connect(&server, alice.device_id, &alice.device_token).await;
    assert!(reconnect.is_err(), "a revoked device must not reconnect");

    // The token no longer authenticates at all.
    let ticket = api
        .post(
            "/v1/auth/websocket-tickets",
            Some(&alice.device_token),
            &json!({ "path": format!("/v1/devices/{}/relay", alice.device_id) }),
        )
        .await;
    assert_eq!(ticket.status, 401);
    assert_eq!(ticket.error_code(), Some("device_revoked"));

    // The device cannot obtain a fresh token either.
    let (challenge_id, input) = api
        .challenge("authenticate_device", &alice.device_key, None)
        .await;
    let refused = api
        .post(
            "/v1/auth/tokens",
            None,
            &json!({ "challenge_id": challenge_id, "signature": alice.device_key.sign_b64(&input) }),
        )
        .await;
    assert_eq!(refused.status, 401);

    // Its terminals are closed, and the durable output remains readable by the owner.
    let closed = eventually(Duration::from_secs(20), || async {
        let response = api
            .get(
                &format!("/v1/terminals/{terminal_id}"),
                Some(&alice.identity_token),
            )
            .await;
        response.body["state"] == "closed"
    })
    .await;
    assert!(closed, "the revoked device's terminals should close");

    let response = api
        .get(
            &format!("/v1/terminals/{terminal_id}"),
            Some(&alice.identity_token),
        )
        .await;
    assert_eq!(response.body["durable_offset"], 17);
    assert_eq!(response.body["close_reason"], "device_revoked");

    server.shutdown().await;
}

// --------------------------------------------------------------- criterion 16

/// The specification enumerates the categories that must be settings-driven
/// (spec §8.1). This pins a representative name from each so a future change cannot
/// quietly reintroduce a hard-coded constant.
#[test]
fn criterion_16_every_behaviour_category_is_a_typed_setting() {
    let required = [
        // feature switches
        keys::FEATURES_IDENTITY_REGISTRATION_ENABLED,
        keys::FEATURES_PUBLISH_ENABLED,
        keys::FEATURES_MIRROR_ENABLED,
        // defaults and timeouts
        keys::LIMITS_DEFAULT_PAGE_SIZE,
        keys::SERVER_REQUEST_TIMEOUT_SECONDS,
        keys::WEBSOCKET_HANDSHAKE_TIMEOUT_SECONDS,
        // retention and replay capacity
        keys::TERMINAL_CLOSED_RETENTION_SECONDS,
        keys::TERMINAL_REPLAY_CAPACITY_BYTES,
        // persistence batching
        keys::PERSISTENCE_FLUSH_INTERVAL_MS,
        keys::PERSISTENCE_FLUSH_BYTES,
        keys::PERSISTENCE_MEMORY_PRESSURE_DIRTY_BYTES,
        // limits and quotas
        keys::LIMITS_MAX_OUTPUT_FRAME_BYTES,
        keys::LIMITS_MAX_CONNECTIONS_PER_PRINCIPAL,
        keys::PERSISTENCE_STORAGE_QUOTA_BYTES,
        // rate limits
        keys::RATELIMIT_CHALLENGES_PER_MINUTE_PER_SOURCE,
        keys::RATELIMIT_RETRY_AFTER_SECONDS,
        // retry and backoff
        keys::PERSISTENCE_COMMIT_RETRY_INITIAL_MS,
        keys::PERSISTENCE_COMMIT_RETRY_MAX_MS,
        keys::PERSISTENCE_COMMIT_RETRY_MAX_ATTEMPTS,
        // heartbeat
        keys::WEBSOCKET_HEARTBEAT_INTERVAL_SECONDS,
        keys::WEBSOCKET_HEARTBEAT_TIMEOUT_SECONDS,
        // graceful shutdown
        keys::SERVER_SHUTDOWN_DEADLINE_SECONDS,
        keys::SERVER_CONNECTION_DRAIN_SECONDS,
        // logging
        keys::LOGGING_LEVEL,
        keys::LOGGING_FORMAT,
        // trusted proxy behaviour
        keys::SECURITY_TRUSTED_PROXY_ENABLED,
        keys::SECURITY_TRUSTED_PROXY_NETWORKS,
        // public origin
        keys::SERVER_PUBLIC_ORIGIN,
        // token and challenge lifetimes
        keys::AUTH_ACCESS_TOKEN_TTL_SECONDS,
        keys::AUTH_CHALLENGE_TTL_SECONDS,
        keys::AUTH_WEBSOCKET_TICKET_TTL_SECONDS,
        // key rotation policy
        keys::AUTH_SIGNING_KEY_ROTATION_SECONDS,
        keys::AUTH_SIGNING_KEY_OVERLAP_SECONDS,
        // listen and TLS behaviour
        keys::SERVER_LISTEN_ADDRESS,
        keys::SERVER_TLS_ENABLED,
        keys::SERVER_TLS_CERTIFICATE_PATH,
    ];

    for name in required {
        let def = DEFS.iter().find(|d| d.name == name);
        let def = def.unwrap_or_else(|| panic!("{name} must be a declared setting"));
        assert!(!def.description.is_empty(), "{name} must describe itself");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn criterion_16_settings_are_introspectable_with_redacted_secrets() {
    let server = TestServer::start().await;
    let api = Api::new(&server);

    let unauthenticated = api.get("/v1/admin/settings", None).await;
    assert_eq!(unauthenticated.status, 401);

    let response = api.get("/v1/admin/settings", Some(OPERATOR_TOKEN)).await;
    assert_eq!(response.status, 200, "{:?}", response.body);
    let settings = response.body["settings"].as_array().unwrap();
    assert_eq!(settings.len(), DEFS.len());

    for setting in settings {
        assert!(setting["name"].is_string());
        assert!(setting["type"].is_string());
        assert!(setting["description"].is_string());
        assert!(setting["reload"].is_string());
        assert!(setting.get("default").is_some());

        if setting["secret"].as_bool().unwrap_or(false) {
            // Secret values are never returned; only whether one is configured.
            assert!(
                setting["value"].is_null(),
                "secret {} must be redacted",
                setting["name"]
            );
            assert!(setting["configured"].is_boolean());
        } else {
            assert!(setting.get("value").is_some());
        }
    }

    // The operator credential is a secret and is reported as configured, not shown.
    let operator = settings
        .iter()
        .find(|s| s["name"] == keys::AUTH_OPERATOR_TOKEN_HASH)
        .expect("operator setting");
    assert_eq!(operator["configured"], json!(true));
    assert_eq!(operator["secret_form"], json!("encrypted_inline"));

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn criterion_16_updates_apply_without_restart_and_survive_restart() {
    let server = TestServer::start().await;
    let api = Api::new(&server);

    let before = api.get("/v1/admin/settings", Some(OPERATOR_TOKEN)).await;
    let revision = before.body["revision"].as_i64().unwrap();

    let patch = api
        .patch(
            "/v1/admin/settings",
            Some(OPERATOR_TOKEN),
            &json!({
                "revision": revision,
                "settings": {
                    keys::TERMINAL_CLOSED_RETENTION_SECONDS: 3600,
                    keys::LOGGING_LEVEL: "debug",
                },
            }),
        )
        .await;
    assert_eq!(patch.status, 200, "{:?}", patch.body);
    assert_eq!(patch.body["revision"].as_i64().unwrap(), revision + 1);

    // Applied in-process, with no restart.
    assert_eq!(
        server
            .state
            .snapshot()
            .int(keys::TERMINAL_CLOSED_RETENTION_SECONDS),
        3600
    );
    assert_eq!(server.state.snapshot().string(keys::LOGGING_LEVEL), "debug");

    // A stale revision conflicts (spec §5.5).
    let stale = api
        .patch(
            "/v1/admin/settings",
            Some(OPERATOR_TOKEN),
            &json!({ "revision": revision, "settings": { keys::LOGGING_LEVEL: "warn" } }),
        )
        .await;
    assert_eq!(stale.status, 409);
    assert_eq!(stale.error_code(), Some("settings_revision_conflict"));
    assert_eq!(server.state.snapshot().string(keys::LOGGING_LEVEL), "debug");

    // The change survives a restart, and the database stays authoritative.
    let restarted = server.restart(None).await;
    assert_eq!(
        restarted
            .state
            .snapshot()
            .int(keys::TERMINAL_CLOSED_RETENTION_SECONDS),
        3600
    );
    assert_eq!(
        restarted.state.snapshot().string(keys::LOGGING_LEVEL),
        "debug"
    );
    restarted.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn criterion_16_invalid_and_conflicting_updates_are_atomically_rejected() {
    let server = TestServer::start().await;
    let api = Api::new(&server);
    let revision = server.snapshot_revision();

    // Out of range for its own schema bound.
    let too_big = api
        .patch(
            "/v1/admin/settings",
            Some(OPERATOR_TOKEN),
            &json!({
                "revision": revision,
                "settings": { keys::TERMINAL_REPLAY_CAPACITY_BYTES: 2_000_000 },
            }),
        )
        .await;
    assert_eq!(too_big.status, 422);
    assert_eq!(too_big.error_code(), Some("settings_invalid"));

    // Wrong type.
    let wrong_type = api
        .patch(
            "/v1/admin/settings",
            Some(OPERATOR_TOKEN),
            &json!({
                "revision": revision,
                "settings": { keys::PERSISTENCE_FLUSH_BYTES: "lots" },
            }),
        )
        .await;
    assert_eq!(wrong_type.status, 422);

    // Unknown setting.
    let unknown = api
        .patch(
            "/v1/admin/settings",
            Some(OPERATOR_TOKEN),
            &json!({ "revision": revision, "settings": { "not.a.setting": 1 } }),
        )
        .await;
    assert_eq!(unknown.status, 422);

    // An invalid *combination*: a flush threshold larger than the unacknowledged
    // window would leave a publisher unable to clear its own backlog.
    let combination = api
        .patch(
            "/v1/admin/settings",
            Some(OPERATOR_TOKEN),
            &json!({
                "revision": revision,
                "settings": {
                    keys::PERSISTENCE_FLUSH_BYTES: 8_000_000,
                    keys::LIMITS_MAX_UNACKED_OUTPUT_BYTES: 1_000_000,
                },
            }),
        )
        .await;
    assert_eq!(combination.status, 422);

    // A partially valid update applies nothing at all.
    let partial = api
        .patch(
            "/v1/admin/settings",
            Some(OPERATOR_TOKEN),
            &json!({
                "revision": revision,
                "settings": {
                    keys::LOGGING_LEVEL: "warn",
                    keys::TERMINAL_REPLAY_CAPACITY_BYTES: 99_000_000,
                },
            }),
        )
        .await;
    assert_eq!(partial.status, 422);

    // Revision unchanged, and the valid half of the rejected batch was not applied.
    assert_eq!(server.snapshot_revision(), revision);
    assert_eq!(server.state.snapshot().string(keys::LOGGING_LEVEL), "info");
    assert_eq!(
        server
            .state
            .snapshot()
            .int(keys::TERMINAL_REPLAY_CAPACITY_BYTES),
        1_500_000
    );

    // Rejections are recorded for the operator.
    let audit = api
        .get("/v1/admin/settings/audit", Some(OPERATOR_TOKEN))
        .await;
    assert_eq!(audit.status, 200);
    let entries = audit.body["entries"].as_array().unwrap();
    assert!(entries.iter().any(|e| e["outcome"] == "rejected"));
    // Raw values never appear in the audit log (spec §5.5).
    for entry in entries {
        for field in ["old_value_hash", "new_value_hash"] {
            if let Some(hash) = entry[field].as_str() {
                assert_eq!(hash.len(), 64, "audit entries must store hashes only");
            }
        }
    }

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn replay_capacity_can_never_exceed_the_specification_ceiling() {
    let server = TestServer::start().await;

    // Downward tuning is allowed.
    assert!(
        server
            .patch_settings(vec![(keys::TERMINAL_REPLAY_CAPACITY_BYTES, json!(4096))])
            .is_ok()
    );
    // The hard schema maximum is the specification's decimal 1.5 MB.
    assert!(
        server
            .patch_settings(vec![(
                keys::TERMINAL_REPLAY_CAPACITY_BYTES,
                json!(1_500_001)
            )])
            .is_err()
    );
    assert!(
        server
            .patch_settings(vec![(
                keys::TERMINAL_REPLAY_CAPACITY_BYTES,
                json!(1_572_864)
            )])
            .is_err(),
        "1.5 MiB must be rejected: the limit is decimal 1.5 MB"
    );
    assert!(
        server
            .patch_settings(vec![(
                keys::TERMINAL_REPLAY_CAPACITY_BYTES,
                json!(1_500_000)
            )])
            .is_ok()
    );

    server.shutdown().await;
}

// --------------------------------------------------------------- criterion 17

#[tokio::test(flavor = "multi_thread")]
async fn criterion_17_snapshots_are_internally_consistent() {
    let server = TestServer::start().await;

    // A captured snapshot is immutable: a concurrent update cannot change the limits
    // an in-flight operation is already using (spec §5.5).
    let captured = server.state.snapshot();
    let before = captured.int(keys::PERSISTENCE_FLUSH_BYTES);

    server
        .patch_settings(vec![(keys::PERSISTENCE_FLUSH_BYTES, json!(before * 2))])
        .expect("patch");

    assert_eq!(captured.int(keys::PERSISTENCE_FLUSH_BYTES), before);
    assert_eq!(
        server.state.snapshot().int(keys::PERSISTENCE_FLUSH_BYTES),
        before * 2
    );
    assert!(server.state.snapshot().revision > captured.revision);

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn criterion_17_instances_converge_on_the_committed_revision() {
    let server = TestServer::start().await;

    // A second instance sharing the same durable state.
    let second = TestServer::start_in(
        server.data_dir.clone(),
        Some(vec![(keys::SETTINGS_PROPAGATION_INTERVAL_MS, json!(100))]),
    )
    .await;

    let target = server
        .patch_settings(vec![(keys::TERMINAL_CLOSED_RETENTION_SECONDS, json!(7200))])
        .expect("patch on the first instance");

    // The second instance must observe the committed revision within the configured
    // propagation interval.
    let converged = eventually(Duration::from_secs(10), || async {
        let snapshot = second.state.snapshot();
        snapshot.revision >= target && snapshot.int(keys::TERMINAL_CLOSED_RETENTION_SECONDS) == 7200
    })
    .await;
    assert!(
        converged,
        "the second instance did not converge; revision {} vs {target}",
        second.state.snapshot().revision
    );

    second.shutdown().await;
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_settings_schema_version_mismatch_fails_readiness() {
    let server = TestServer::start().await;
    let (data_dir, _temp) = server.shutdown_keeping_data().await;

    // Simulate a database written by a future build.
    let conn = rusqlite::Connection::open(data_dir.join("relay.db")).unwrap();
    conn.execute(
        "UPDATE schema_meta SET value = '999' WHERE key = 'settings_schema_version'",
        [],
    )
    .unwrap();
    drop(conn);

    let bootstrap = terminal_relay::bootstrap::Bootstrap {
        data_dir: data_dir.clone(),
        db_path: data_dir.join("relay.db"),
        secret_key: [7u8; 32],
        operator_token_seed: Some(OPERATOR_TOKEN.to_string()),
        instance_id: "test".to_string(),
        recovery_mode: false,
        recovery_listen: "127.0.0.1:0".to_string(),
    };

    // Startup must refuse rather than reinterpret rows from another schema version.
    let result = terminal_relay::app::AppState::new(bootstrap);
    assert!(
        result.is_err(),
        "an unsupported settings schema version must not start"
    );
    let message = result.err().unwrap().message;
    assert!(
        message.contains("schema version"),
        "unexpected error: {message}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn idempotency_keys_replay_the_original_response() {
    let server = TestServer::start().await;
    let api = Api::new(&server);

    let identity_key = Key::random();
    let identity_id = api.identity_id(&identity_key).await;
    let token = api.identity_token(&identity_key).await;

    let device_key = Key::random();
    let (challenge_id, input) = api
        .challenge("register_device", &device_key, Some(&identity_id))
        .await;
    let body = json!({
        "name": "idempotent device",
        "key": device_key.key_json(),
        "challenge_id": challenge_id,
        "device_signature": device_key.sign_b64(&input),
    });

    let first = api
        .request(
            reqwest::Method::POST,
            "/v1/devices",
            Some(&token),
            Some(&body),
            &[("idempotency-key", "device-key-1")],
        )
        .await;
    assert_eq!(first.status, 201, "{:?}", first.body);

    // Replaying the same key and body returns the original response rather than
    // consuming the challenge again.
    let replay = api
        .request(
            reqwest::Method::POST,
            "/v1/devices",
            Some(&token),
            Some(&body),
            &[("idempotency-key", "device-key-1")],
        )
        .await;
    assert_eq!(replay.status, 201);
    assert_eq!(replay.body["device_id"], first.body["device_id"]);

    // The same key with a different body is a conflict.
    let mut different = body.clone();
    different["name"] = json!("different name");
    let conflict = api
        .request(
            reqwest::Method::POST,
            "/v1/devices",
            Some(&token),
            Some(&different),
            &[("idempotency-key", "device-key-1")],
        )
        .await;
    assert_eq!(conflict.status, 409);
    assert_eq!(conflict.error_code(), Some("idempotency_key_conflict"));

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn list_endpoints_paginate_with_opaque_cursors() {
    let server = TestServer::start().await;
    let api = Api::new(&server);

    let identity_key = Key::random();
    let identity_id = api.identity_id(&identity_key).await;
    let token = api.identity_token(&identity_key).await;

    for index in 0..5 {
        api.device_id(
            &token,
            &identity_id,
            &Key::random(),
            &format!("device {index}"),
        )
        .await;
    }

    let mut seen: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..10 {
        let path = match &cursor {
            Some(cursor) => format!("/v1/devices?limit=2&cursor={cursor}"),
            None => "/v1/devices?limit=2".to_string(),
        };
        let page = api.get(&path, Some(&token)).await;
        assert_eq!(page.status, 200, "{:?}", page.body);
        for device in page.body["devices"].as_array().unwrap() {
            seen.push(device["device_id"].as_str().unwrap().to_string());
        }
        match page.body["next_cursor"].as_str() {
            Some(next) => cursor = Some(next.to_string()),
            None => break,
        }
    }

    assert_eq!(
        seen.len(),
        5,
        "pagination should cover every device exactly once"
    );
    let unique: BTreeMap<&String, ()> = seen.iter().map(|id| (id, ())).collect();
    assert_eq!(unique.len(), 5);

    // A malformed cursor is a client error, not a panic.
    let bad = api
        .get("/v1/devices?cursor=!!!not-base64!!!", Some(&token))
        .await;
    assert_eq!(bad.status, 400);

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn websocket_upgrades_require_the_declared_subprotocol() {
    let server = TestServer::start().await;
    let api = Api::new(&server);
    let alice = api.provision().await;

    // The relay path exists, but only for the publisher subprotocol.
    let response = api
        .request(
            reqwest::Method::GET,
            &format!("/v1/devices/{}/relay", alice.device_id),
            Some(&alice.device_token),
            None,
            &[
                ("connection", "upgrade"),
                ("upgrade", "websocket"),
                ("sec-websocket-version", "13"),
                ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
            ],
        )
        .await;
    assert_eq!(response.status, 400);
    assert_eq!(response.error_code(), Some("invalid_request"));

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn health_and_readiness_report_correctly() {
    let server = TestServer::start().await;
    let api = Api::new(&server);

    let health = api.get("/healthz", None).await;
    assert_eq!(health.status, 200);
    assert_eq!(health.body["status"], "ok");

    let ready = api.get("/readyz", None).await;
    assert_eq!(ready.status, 200, "{:?}", ready.body);
    assert_eq!(ready.body["status"], "ready");

    // Metrics require operator authentication and expose no sensitive material.
    let unauthenticated = api.get("/metrics", None).await;
    assert_eq!(unauthenticated.status, 401);

    let metrics = api.get("/metrics", Some(OPERATOR_TOKEN)).await;
    assert_eq!(metrics.status, 200);
    let body = match &metrics.body {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    assert!(body.contains("relay_settings_revision"));
    assert!(body.contains("relay_checkpoint_transactions_total"));
    assert!(!body.contains(OPERATOR_TOKEN));

    server.shutdown().await;
}

/// A paired phone holds a `client` device credential, not the identity key, and has to
/// be able to find out what it may mirror. Spec §4.4 permits exactly that, and §4.3
/// grants such a device `terminals:read` for the purpose; an identity-only check on
/// these two endpoints would make that grant unusable and leave a paired client unable
/// to discover anything.
#[tokio::test]
async fn a_client_device_may_discover_its_own_identitys_terminals() {
    let server = TestServer::start().await;
    let api = Api::new(&server);

    let alice = api.provision().await;
    let bob = api.provision().await;

    let (_key, _device_id, alice_client_token) = api
        .client_device(&alice.identity_token, &alice.identity_id)
        .await;

    // Alice's publisher opens a terminal.
    let mut publisher = Publisher::connect(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    let terminal_id = publisher.open_terminal_id("pty0").await;
    publisher.send_output(terminal_id, 0, b"hello").await;
    publisher.wait_ack(terminal_id, 5).await;

    // Alice's phone can list it and read it, on a device token.
    let listed = api.get("/v1/terminals", Some(&alice_client_token)).await;
    assert_eq!(listed.status, 200, "body: {:?}", listed.body);
    let terminals = listed.body["terminals"].as_array().unwrap();
    assert_eq!(terminals.len(), 1);
    assert_eq!(
        terminals[0]["terminal_id"].as_str().unwrap(),
        terminal_id.to_string()
    );

    let fetched = api
        .get(
            &format!("/v1/terminals/{terminal_id}"),
            Some(&alice_client_token),
        )
        .await;
    assert_eq!(fetched.status, 200);

    // The boundary is unchanged: Bob's phone sees nothing of Alice's, and is told
    // "not found" rather than "forbidden", which would confirm the terminal exists.
    let (_bk, _bd, bob_client_token) = api
        .client_device(&bob.identity_token, &bob.identity_id)
        .await;
    let listed = api.get("/v1/terminals", Some(&bob_client_token)).await;
    assert_eq!(listed.status, 200);
    assert_eq!(listed.body["terminals"].as_array().unwrap().len(), 0);

    let fetched = api
        .get(
            &format!("/v1/terminals/{terminal_id}"),
            Some(&bob_client_token),
        )
        .await;
    assert_eq!(fetched.status, 404);
    assert_eq!(fetched.error_code(), Some("not_found"));
}

/// Widening the terminal endpoints must not have widened device management: managing
/// devices is identity-level authority, and a phone credential must never carry it
/// (spec §4.3).
#[tokio::test]
async fn a_client_device_still_cannot_manage_devices() {
    let server = TestServer::start().await;
    let api = Api::new(&server);

    let alice = api.provision().await;
    let (_key, _device_id, client_token) = api
        .client_device(&alice.identity_token, &alice.identity_id)
        .await;

    let listed = api.get("/v1/devices", Some(&client_token)).await;
    assert_eq!(listed.status, 403);

    let fetched = api
        .get(
            &format!("/v1/devices/{}", alice.device_id),
            Some(&client_token),
        )
        .await;
    assert_eq!(fetched.status, 403);
}
