//! Asking a publishing device to open a terminal (spec §4.6, §5.2).
//!
//! The dangerous half of this feature lives on the publishing machine; what these
//! cover is the relay's side of it — that it refuses unless every condition holds, that
//! it never invents a terminal itself, and that a retry cannot produce two shells.

mod support;

use serde_json::json;
use std::time::Duration;
use support::{Api, Publisher, TestServer};
use terminal_relay::settings::defs::keys;
use uuid::Uuid;

/// Everything switched on: the operator setting and the scope. The publisher's own
/// opt-in is asserted per test, because that is usually what is under test.
async fn enabled_server() -> TestServer {
    let server = TestServer::start().await;
    server
        .patch_settings(vec![
            (keys::FEATURES_TERMINAL_CREATE_ENABLED, json!(true)),
            (
                keys::AUTH_IDENTITY_TOKEN_SCOPES,
                json!([
                    "devices:read",
                    "devices:write",
                    "terminals:read",
                    "terminals:mirror",
                    "terminals:input",
                    "terminals:create"
                ]),
            ),
        ])
        .expect("settings");
    server
}

#[tokio::test(flavor = "multi_thread")]
async fn a_phone_can_ask_a_machine_to_open_a_terminal() {
    let server = enabled_server().await;
    let api = Api::new(&server);
    let alice = api.provision().await;

    let mut publisher = Publisher::connect_v2(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    publisher.send_capabilities(true).await;

    let created = tokio::spawn({
        let api = Api::new(&server);
        let token = alice.identity_token.clone();
        let device_id = alice.device_id;
        async move {
            api.create_terminal(device_id, &token, &json!({"label": "phone"}), "key-1")
                .await
        }
    });

    let request = publisher.expect_open_request(Duration::from_secs(5)).await;
    let request_id = request["request_id"]
        .as_str()
        .expect("request_id")
        .to_string();
    assert_eq!(request["label"], json!("phone"));
    // The request carries no command, environment or working directory: the machine
    // alone decides what runs (spec §4.6).
    assert!(request.get("command").is_none());
    assert!(request.get("term").is_none());

    let terminal_id = publisher
        .answer_open_request(&request_id, "pty-phone")
        .await;
    let response = created.await.unwrap();

    assert_eq!(response.status, 201, "body: {:?}", response.body);
    assert_eq!(
        response.body["terminal_id"].as_str().unwrap(),
        terminal_id.to_string()
    );
    assert_eq!(
        response.headers.get("location").unwrap().to_str().unwrap(),
        format!("/v1/terminals/{terminal_id}")
    );
    // The local_ref is the publisher's own, never one the relay invented: a relay-chosen
    // value could collide with a live terminal and splice two shells onto one stream.
    assert_eq!(response.body["local_ref"], json!("pty-phone"));

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_carrying_a_command_is_refused_outright() {
    let server = enabled_server().await;
    let api = Api::new(&server);
    let alice = api.provision().await;

    let mut publisher = Publisher::connect_v2(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    publisher.send_capabilities(true).await;

    // The security regression test for the whole feature. If this ever passes with a
    // 2xx, a phone can choose what runs on somebody's machine.
    let response = api
        .create_terminal(
            alice.device_id,
            &alice.identity_token,
            &json!({"label": "x", "command": "rm -rf /"}),
            "key-cmd",
        )
        .await;
    assert_eq!(response.status, 400, "body: {:?}", response.body);
    assert_eq!(response.error_code(), Some("invalid_request"));

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_machine_that_never_opted_in_is_never_asked() {
    let server = enabled_server().await;
    let api = Api::new(&server);
    let alice = api.provision().await;

    // Connected, and deliberately silent about the capability.
    let _publisher = Publisher::connect_v2(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();

    let response = api
        .create_terminal(
            alice.device_id,
            &alice.identity_token,
            &json!({}),
            "key-optout",
        )
        .await;
    assert_eq!(response.status, 503, "body: {:?}", response.body);
    assert_eq!(response.error_code(), Some("publisher_unavailable"));

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_feature_switch_refuses_before_anybody_is_asked() {
    let server = TestServer::start().await;
    server
        .patch_settings(vec![(
            keys::AUTH_IDENTITY_TOKEN_SCOPES,
            json!([
                "devices:read",
                "devices:write",
                "terminals:read",
                "terminals:mirror",
                "terminals:input",
                "terminals:create"
            ]),
        )])
        .expect("settings");
    let api = Api::new(&server);
    let alice = api.provision().await;

    let mut publisher = Publisher::connect_v2(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    publisher.send_capabilities(true).await;

    // Default is off, so an upgrade never grants the capability (spec §4.6 condition 3).
    let response = api
        .create_terminal(
            alice.device_id,
            &alice.identity_token,
            &json!({}),
            "key-off",
        )
        .await;
    assert_eq!(response.status, 403, "body: {:?}", response.body);
    assert_eq!(response.error_code(), Some("feature_disabled"));

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn without_the_scope_the_request_is_refused() {
    let server = TestServer::start().await;
    server
        .patch_settings(vec![(keys::FEATURES_TERMINAL_CREATE_ENABLED, json!(true))])
        .expect("settings");
    let api = Api::new(&server);
    let alice = api.provision().await;

    // The scope is in no principal's defaults, so this token does not carry it.
    let response = api
        .create_terminal(
            alice.device_id,
            &alice.identity_token,
            &json!({}),
            "key-scope",
        )
        .await;
    assert_eq!(response.status, 403, "body: {:?}", response.body);
    assert_eq!(response.error_code(), Some("insufficient_scope"));

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_idempotency_key_is_required() {
    let server = enabled_server().await;
    let api = Api::new(&server);
    let alice = api.provision().await;

    let response = api
        .post(
            &format!("/v1/devices/{}/terminals", alice.device_id),
            Some(&alice.identity_token),
            &json!({}),
        )
        .await;
    assert_eq!(response.status, 400, "body: {:?}", response.body);
    assert_eq!(response.error_code(), Some("idempotency_key_required"));

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_decline_never_forwards_the_publishers_own_words() {
    let server = enabled_server().await;
    let api = Api::new(&server);
    let alice = api.provision().await;

    let mut publisher = Publisher::connect_v2(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    publisher.send_capabilities(true).await;

    let created = tokio::spawn({
        let api = Api::new(&server);
        let token = alice.identity_token.clone();
        let device_id = alice.device_id;
        async move {
            api.create_terminal(device_id, &token, &json!({}), "key-decline")
                .await
        }
    });

    let request = publisher.expect_open_request(Duration::from_secs(5)).await;
    let request_id = request["request_id"].as_str().unwrap().to_string();
    publisher
        .decline_open_request(&request_id, "not_permitted", "SECRET-HOSTNAME-DETAIL")
        .await;

    let response = created.await.unwrap();
    assert_eq!(response.status, 502, "body: {:?}", response.body);
    assert_eq!(response.error_code(), Some("publisher_declined"));
    // A publisher must not be able to write arbitrary text onto a phone's screen.
    assert!(
        !response.body.to_string().contains("SECRET-HOSTNAME-DETAIL"),
        "the publisher's detail leaked into the response: {:?}",
        response.body
    );

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_publisher_that_disconnects_ends_the_wait_at_once() {
    let server = enabled_server().await;
    let api = Api::new(&server);
    let alice = api.provision().await;

    let mut publisher = Publisher::connect_v2(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    publisher.send_capabilities(true).await;

    let created = tokio::spawn({
        let api = Api::new(&server);
        let token = alice.identity_token.clone();
        let device_id = alice.device_id;
        async move {
            api.create_terminal(device_id, &token, &json!({}), "key-gone")
                .await
        }
    });

    let _ = publisher.expect_open_request(Duration::from_secs(5)).await;
    drop(publisher);

    // 503, not the 504 the timeout would eventually give: a request is never left
    // pending for a device that is not connected (spec §4.6 condition 4).
    let response = created.await.unwrap();
    assert_eq!(response.status, 503, "body: {:?}", response.body);
    assert_eq!(response.error_code(), Some("publisher_unavailable"));

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn two_concurrent_requests_with_one_key_open_one_terminal() {
    let server = enabled_server().await;
    let api = Api::new(&server);
    let alice = api.provision().await;

    let mut publisher = Publisher::connect_v2(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    publisher.send_capabilities(true).await;

    // The stored idempotency record is only written after success, so it alone cannot
    // make two concurrent retries converge. If the pending table did not join them,
    // this spawns two shells for one request.
    //
    // Both clients are built before either request starts: constructing one costs
    // enough that doing it inside the tasks lets the first finish before the second
    // begins, which would test a different thing entirely.
    let (api_one, api_two) = (Api::new(&server), Api::new(&server));
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let first = tokio::spawn({
        let token = alice.identity_token.clone();
        let device_id = alice.device_id;
        let barrier = std::sync::Arc::clone(&barrier);
        async move {
            barrier.wait().await;
            api_one
                .create_terminal(device_id, &token, &json!({}), "key-same")
                .await
        }
    });
    let second = tokio::spawn({
        let token = alice.identity_token.clone();
        let device_id = alice.device_id;
        let barrier = std::sync::Arc::clone(&barrier);
        async move {
            barrier.wait().await;
            api_two
                .create_terminal(device_id, &token, &json!({}), "key-same")
                .await
        }
    });

    let request = publisher.expect_open_request(Duration::from_secs(5)).await;
    let request_id = request["request_id"].as_str().unwrap().to_string();
    let terminal_id = publisher.answer_open_request(&request_id, "pty-once").await;

    let a = first.await.unwrap();
    let b = second.await.unwrap();
    for response in [&a, &b] {
        assert!(
            response.status == 201 || response.status == 200,
            "body: {:?}",
            response.body
        );
        assert_eq!(
            response.body["terminal_id"].as_str().unwrap(),
            terminal_id.to_string()
        );
    }

    // Exactly one terminal exists for this identity.
    let listed = api.get("/v1/terminals", Some(&alice.identity_token)).await;
    assert_eq!(listed.body["terminals"].as_array().unwrap().len(), 1);

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn another_identitys_device_is_not_found_rather_than_forbidden() {
    let server = enabled_server().await;
    let api = Api::new(&server);
    let alice = api.provision().await;
    let bob = api.provision().await;

    let response = api
        .create_terminal(bob.device_id, &alice.identity_token, &json!({}), "key-bob")
        .await;
    // 404, never 403: a 403 would confirm the device exists to somebody who may not
    // know that (spec §4.4).
    assert_eq!(response.status, 404, "body: {:?}", response.body);

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_device_is_not_found() {
    let server = enabled_server().await;
    let api = Api::new(&server);
    let alice = api.provision().await;

    let response = api
        .create_terminal(
            Uuid::new_v4(),
            &alice.identity_token,
            &json!({}),
            "key-ghost",
        )
        .await;
    assert_eq!(response.status, 404, "body: {:?}", response.body);

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_label_with_control_characters_is_refused_not_stripped() {
    let server = enabled_server().await;
    let api = Api::new(&server);
    let alice = api.provision().await;

    let mut publisher = Publisher::connect_v2(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    publisher.send_capabilities(true).await;

    // The label is printed into the machine owner's own terminal and crosses an argv
    // boundary. Stripping would leave the phone showing one string and the laptop
    // another, which is the confusion an injection wants.
    let response = api
        .create_terminal(
            alice.device_id,
            &alice.identity_token,
            &json!({"label": "build\u{1b}[2Jrm -rf"}),
            "key-label",
        )
        .await;
    assert_eq!(response.status, 422, "body: {:?}", response.body);
    assert_eq!(response.error_code(), Some("validation_failed"));

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn retrying_a_succeeded_key_returns_the_same_terminal_without_asking_again() {
    let server = enabled_server().await;
    let api = Api::new(&server);
    let alice = api.provision().await;

    let mut publisher = Publisher::connect_v2(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    publisher.send_capabilities(true).await;

    let created = tokio::spawn({
        let api = Api::new(&server);
        let token = alice.identity_token.clone();
        let device_id = alice.device_id;
        async move {
            api.create_terminal(device_id, &token, &json!({}), "key-retry")
                .await
        }
    });
    let request = publisher.expect_open_request(Duration::from_secs(5)).await;
    let request_id = request["request_id"].as_str().unwrap().to_string();
    let terminal_id = publisher
        .answer_open_request(&request_id, "pty-retry")
        .await;
    let first = created.await.unwrap();
    assert_eq!(first.status, 201, "body: {:?}", first.body);

    // The sequential half of the same guarantee, and the deterministic one: once the
    // record is stored, the same key is answered from it. The machine is never asked a
    // second time, so a phone retrying on a flaky connection cannot collect shells.
    let again = api
        .create_terminal(
            alice.device_id,
            &alice.identity_token,
            &json!({}),
            "key-retry",
        )
        .await;
    assert_eq!(
        again.body["terminal_id"].as_str().unwrap(),
        terminal_id.to_string()
    );

    let listed = api.get("/v1/terminals", Some(&alice.identity_token)).await;
    assert_eq!(listed.body["terminals"].as_array().unwrap().len(), 1);

    server.shutdown().await;
}
