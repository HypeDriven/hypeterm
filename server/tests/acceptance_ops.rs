//! Operational behaviour: live listener rebind, in-process TLS, transport security,
//! retention, storage failure and heartbeats (spec §4.1, §5.4, §7.3, §8, §10).

mod support;

use serde_json::json;
use std::time::Duration;
use support::{Api, Mirror, OPERATOR_TOKEN, Publisher, TestServer, eventually};
use terminal_relay::settings::defs::keys;

// -------------------------------------------------------------- listener rebind

#[tokio::test(flavor = "multi_thread")]
async fn changing_the_listen_address_rebinds_without_a_restart() {
    let server = TestServer::start().await;
    let api = Api::new(&server);
    assert_eq!(api.get("/healthz", None).await.status, 200);

    // Pick a free port, then move the listener to it. The specification requires a
    // safe live rebind rather than a process restart (spec §8.1).
    let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("probe bind");
    let target = probe.local_addr().unwrap();
    drop(probe);

    server
        .patch_settings(vec![(
            keys::SERVER_LISTEN_ADDRESS,
            json!(target.to_string()),
        )])
        .expect("move the listener");

    let moved = eventually(Duration::from_secs(15), || async {
        reqwest::Client::new()
            .get(format!("http://{target}/healthz"))
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    })
    .await;
    assert!(moved, "the listener should rebind to the new address");

    // The previous address stops accepting once the drain completes.
    let old_closed = eventually(Duration::from_secs(15), || async {
        reqwest::Client::new()
            .get(format!("{}/healthz", api.base))
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .is_err()
    })
    .await;
    assert!(old_closed, "the superseded listener should stop accepting");

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_isolated_health_listener_serves_only_health_endpoints() {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("probe bind");
    let health_addr = probe.local_addr().unwrap();
    drop(probe);

    let server = TestServer::start_in(
        tempfile::tempdir().expect("temp").keep(),
        Some(vec![(
            keys::SERVER_HEALTH_LISTEN_ADDRESS,
            json!(health_addr.to_string()),
        )]),
    )
    .await;

    let client = reqwest::Client::new();
    let ready = eventually(Duration::from_secs(15), || async {
        client
            .get(format!("http://{health_addr}/healthz"))
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    })
    .await;
    assert!(ready, "the isolated health listener should serve /healthz");

    assert!(
        client
            .get(format!("http://{health_addr}/readyz"))
            .send()
            .await
            .expect("readyz")
            .status()
            .is_success()
    );

    // Nothing else is exposed there: no API, no metrics.
    for path in ["/v1/devices", "/metrics", "/v1/admin/settings"] {
        let response = client
            .get(format!("http://{health_addr}{path}"))
            .send()
            .await
            .expect("request");
        assert_eq!(
            response.status(),
            404,
            "{path} must not be served on the health listener"
        );
    }

    server.shutdown().await;
}

// ------------------------------------------------------------ transport security

#[tokio::test(flavor = "multi_thread")]
async fn plain_http_is_refused_when_the_loopback_exemption_is_disabled() {
    let server = TestServer::start().await;
    let api = Api::new(&server);

    // With TLS off and the development exemption disabled, plain HTTP is refused
    // (spec §4.1).
    server
        .patch_settings(vec![(keys::SECURITY_ALLOW_INSECURE_LOOPBACK, json!(false))])
        .expect("disable the loopback exemption");

    let response = api.get("/v1/devices", None).await;
    assert_eq!(response.status, 403);
    assert_eq!(response.error_code(), Some("insecure_transport"));

    // A trusted proxy asserting https is accepted.
    server
        .patch_settings(vec![
            (keys::SECURITY_TRUSTED_PROXY_ENABLED, json!(true)),
            (
                keys::SECURITY_TRUSTED_PROXY_NETWORKS,
                json!(["127.0.0.0/8"]),
            ),
        ])
        .expect("trust the loopback proxy");

    let forwarded = api
        .request(
            reqwest::Method::GET,
            "/v1/devices",
            None,
            None,
            &[("x-forwarded-proto", "https")],
        )
        .await;
    // Now it fails authentication rather than transport security.
    assert_eq!(forwarded.status, 401);

    // A forwarded header from an *untrusted* peer is ignored.
    server
        .patch_settings(vec![(
            keys::SECURITY_TRUSTED_PROXY_NETWORKS,
            json!(["10.0.0.0/8"]),
        )])
        .expect("narrow the trusted network");
    let spoofed = api
        .request(
            reqwest::Method::GET,
            "/v1/devices",
            None,
            None,
            &[("x-forwarded-proto", "https")],
        )
        .await;
    assert_eq!(spoofed.status, 403);
    assert_eq!(spoofed.error_code(), Some("insecure_transport"));

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn tls_is_terminated_in_process_when_configured() {
    let dir = tempfile::tempdir().expect("temp").keep();
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");

    let generated = std::process::Command::new("openssl")
        .args([
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-days",
            "1",
            "-subj",
            "/CN=localhost",
            "-addext",
            "subjectAltName=DNS:localhost,IP:127.0.0.1",
            "-keyout",
        ])
        .arg(&key_path)
        .arg("-out")
        .arg(&cert_path)
        .output();

    match generated {
        Ok(output) if output.status.success() => {}
        _ => {
            eprintln!("skipping: openssl is unavailable for generating a test certificate");
            return;
        }
    }

    let server = TestServer::start_in(
        dir,
        Some(vec![
            (keys::SERVER_TLS_ENABLED, json!(true)),
            (
                keys::SERVER_TLS_CERTIFICATE_PATH,
                json!(cert_path.to_string_lossy()),
            ),
            (
                keys::SERVER_TLS_PRIVATE_KEY_PATH,
                json!(key_path.to_string_lossy()),
            ),
            // With real TLS the loopback exemption is unnecessary.
            (keys::SECURITY_ALLOW_INSECURE_LOOPBACK, json!(false)),
        ]),
    )
    .await;

    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(Duration::from_secs(10))
        .build()
        .expect("tls client");

    let response = client
        .get(format!("https://{}/healthz", server.addr))
        .send()
        .await
        .expect("https request");
    assert!(
        response.status().is_success(),
        "TLS termination should serve /healthz"
    );

    // A request over TLS satisfies the secure-transport requirement.
    let api = client
        .get(format!("https://{}/v1/devices", server.addr))
        .send()
        .await
        .expect("https request");
    assert_eq!(
        api.status(),
        401,
        "expected an authentication failure, not a transport failure"
    );

    // Plain HTTP against the TLS listener does not succeed.
    let plain = reqwest::Client::new()
        .get(format!("http://{}/healthz", server.addr))
        .timeout(Duration::from_secs(3))
        .send()
        .await;
    assert!(
        plain.is_err(),
        "the TLS listener must not serve plaintext HTTP"
    );

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_tls_settings_are_rejected_before_they_are_committed() {
    let server = TestServer::start().await;

    // Enabling TLS without usable material is refused, so a bad revision can never
    // take the listener down.
    let missing_paths = server.patch_settings(vec![(keys::SERVER_TLS_ENABLED, json!(true))]);
    assert!(missing_paths.is_err());
    assert!(missing_paths.unwrap_err().contains("required"));

    let unreadable = server.patch_settings(vec![
        (keys::SERVER_TLS_ENABLED, json!(true)),
        (
            keys::SERVER_TLS_CERTIFICATE_PATH,
            json!("/nonexistent/cert.pem"),
        ),
        (
            keys::SERVER_TLS_PRIVATE_KEY_PATH,
            json!("/nonexistent/key.pem"),
        ),
    ]);
    assert!(unreadable.is_err());
    assert!(unreadable.unwrap_err().contains("unreadable"));

    // The listener is untouched.
    let api = Api::new(&server);
    assert_eq!(api.get("/healthz", None).await.status, 200);

    server.shutdown().await;
}

// ------------------------------------------------------------------- retention

#[tokio::test(flavor = "multi_thread")]
async fn closed_terminals_are_deleted_after_their_retention_window() {
    let server = TestServer::start().await;
    server
        .patch_settings(vec![
            (keys::TERMINAL_CLOSED_RETENTION_SECONDS, json!(60)),
            (keys::PERSISTENCE_RETENTION_SWEEP_INTERVAL_SECONDS, json!(5)),
        ])
        .expect("shorten retention");

    let api = Api::new(&server);
    let alice = api.provision().await;

    let mut publisher = Publisher::connect(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    let terminal_id = publisher.open_terminal_id("pty0").await;
    publisher.send_output(terminal_id, 0, b"transient").await;
    publisher.wait_ack(terminal_id, 9).await;
    publisher
        .close_terminal(terminal_id, "process_exited")
        .await;

    let closed = eventually(Duration::from_secs(15), || async {
        api.get(
            &format!("/v1/terminals/{terminal_id}"),
            Some(&alice.identity_token),
        )
        .await
        .body["state"]
            == "closed"
    })
    .await;
    assert!(closed, "the terminal should close");

    // A closed terminal is still readable inside its retention window.
    let mut mirror = Mirror::connect(&server, terminal_id, &alice.identity_token)
        .await
        .unwrap();
    let subscribed = mirror.subscribe(Some(0)).await;
    assert_eq!(subscribed["terminal_state"], "closed");
    let stream = mirror.collect(9, Duration::from_secs(10)).await;
    assert_eq!(stream.bytes, b"transient");
    drop(mirror);

    // Age the close timestamp past the window, then let the sweep run.
    let conn = rusqlite::Connection::open(server.data_dir.join("relay.db")).unwrap();
    conn.execute(
        "UPDATE terminals SET closed_at = ?2 WHERE terminal_id = ?1",
        rusqlite::params![
            terminal_id.to_string(),
            terminal_relay::util::to_rfc3339(
                terminal_relay::util::now() - chrono::Duration::hours(2)
            )
        ],
    )
    .unwrap();
    drop(conn);

    let deleted = eventually(Duration::from_secs(30), || async {
        api.get(
            &format!("/v1/terminals/{terminal_id}"),
            Some(&alice.identity_token),
        )
        .await
        .status
            == 404
    })
    .await;
    assert!(deleted, "an expired closed terminal should be deleted");

    // Its replay payload went with it.
    let conn = rusqlite::Connection::open(server.data_dir.join("relay.db")).unwrap();
    let remaining: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM terminal_output WHERE terminal_id = ?1",
            rusqlite::params![terminal_id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(remaining, 0);

    server.shutdown().await;
}

// -------------------------------------------------------------- storage failure

#[tokio::test(flavor = "multi_thread")]
async fn a_commit_failure_withholds_acknowledgement_and_fails_readiness() {
    let server = TestServer::start().await;
    server
        .patch_settings(vec![
            // Fail fast rather than waiting on the lock.
            (keys::PERSISTENCE_SQLITE_BUSY_TIMEOUT_MS, json!(0)),
            (keys::PERSISTENCE_COMMIT_RETRY_MAX_ATTEMPTS, json!(1)),
            (keys::PERSISTENCE_COMMIT_RETRY_INITIAL_MS, json!(1)),
            (keys::PERSISTENCE_BACKPRESSURE_WAIT_MS, json!(500)),
            // A small unacknowledged window so backpressure engages quickly.
            (keys::LIMITS_MAX_UNACKED_OUTPUT_BYTES, json!(65_536)),
            (keys::LIMITS_MAX_OUTPUT_FRAME_BYTES, json!(65_536)),
            (keys::PERSISTENCE_FLUSH_BYTES, json!(4096)),
            (keys::PERSISTENCE_FLUSH_INTERVAL_MS, json!(50)),
        ])
        .expect("configure fast failure");

    let api = Api::new(&server);
    let alice = api.provision().await;

    let mut publisher = Publisher::connect(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    let terminal_id = publisher.open_terminal_id("pty0").await;

    // Hold an exclusive write lock so every checkpoint transaction fails.
    let blocker = rusqlite::Connection::open(server.data_dir.join("relay.db")).unwrap();
    blocker.execute_batch("BEGIN EXCLUSIVE").unwrap();

    // Publish past the unacknowledged window.
    let chunk = vec![b'k'; 32_768];
    let mut offset = 0u64;
    for _ in 0..6 {
        publisher.send_output(terminal_id, offset, &chunk).await;
        offset += chunk.len() as u64;
    }

    // No false acknowledgement, and the publisher is told storage is unavailable
    // (spec §7.2, §10).
    let error = publisher
        .expect_message("error", Duration::from_secs(20))
        .await;
    assert_eq!(
        error["code"], "storage_unavailable",
        "expected storage_unavailable, got {error:?}"
    );
    let code = publisher.expect_close(Duration::from_secs(10)).await;
    assert_eq!(code, Some(4004));

    // Readiness reports the degradation while liveness stays up (spec §5.4).
    let unready = eventually(Duration::from_secs(20), || async {
        api.get("/readyz", None).await.status == 503
    })
    .await;
    assert!(
        unready,
        "readiness must fail while durable storage is failing"
    );
    assert_eq!(api.get("/healthz", None).await.status, 200);

    // Release the lock; the service recovers and commits the retained output.
    blocker.execute_batch("ROLLBACK").unwrap();
    drop(blocker);

    let recovered = eventually(Duration::from_secs(30), || async {
        api.get("/readyz", None).await.status == 200
    })
    .await;
    assert!(
        recovered,
        "readiness should recover once storage works again"
    );

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unsatisfiable_storage_quota_fails_readiness() {
    let server = TestServer::start().await;
    server
        .patch_settings(vec![
            (keys::PERSISTENCE_STORAGE_QUOTA_BYTES, json!(1_048_576)),
            (keys::PERSISTENCE_RETENTION_SWEEP_INTERVAL_SECONDS, json!(5)),
        ])
        .expect("tiny quota");

    let api = Api::new(&server);
    let alice = api.provision().await;
    let mut publisher = Publisher::connect(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    let terminal_id = publisher.open_terminal_id("pty0").await;

    // Fill well past the quota in a terminal that is still open, which must not be
    // trimmed below its configured replay window to satisfy a global quota (spec §7.3).
    let chunk = vec![b'q'; 200_000];
    let mut offset = 0u64;
    for _ in 0..8 {
        publisher.send_output(terminal_id, offset, &chunk).await;
        offset += chunk.len() as u64;
        publisher.wait_ack(terminal_id, offset).await;
    }

    let unready = eventually(Duration::from_secs(30), || async {
        api.get("/readyz", None).await.status == 503
    })
    .await;
    assert!(
        unready,
        "an unsatisfiable quota must fail readiness rather than under-retain"
    );

    // The open terminal kept its full window.
    let response = api
        .get(
            &format!("/v1/terminals/{terminal_id}"),
            Some(&alice.identity_token),
        )
        .await;
    assert_eq!(response.body["retained_bytes"], 1_500_000);

    server.shutdown().await;
}

// ------------------------------------------------------------------- heartbeats

#[tokio::test(flavor = "multi_thread")]
async fn an_unresponsive_connection_is_closed_after_the_heartbeat_timeout() {
    let server = TestServer::start().await;
    server
        .patch_settings(vec![
            (keys::WEBSOCKET_HEARTBEAT_INTERVAL_SECONDS, json!(1)),
            (keys::WEBSOCKET_HEARTBEAT_TIMEOUT_SECONDS, json!(2)),
        ])
        .expect("fast heartbeat");

    let api = Api::new(&server);
    let alice = api.provision().await;
    let mut publisher = Publisher::connect(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    publisher.open_terminal_id("pty0").await;

    // Stop reading and stop responding: the client answers no pings from here on.
    tokio::time::sleep(Duration::from_secs(6)).await;

    // Draining now shows the connection was closed for being unresponsive.
    let code = publisher.expect_close(Duration::from_secs(10)).await;
    assert_eq!(
        code,
        Some(4009),
        "expected the heartbeat_timeout close code"
    );

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_mirror_handshake_without_a_subscribe_message_times_out() {
    let server = TestServer::start().await;
    server
        .patch_settings(vec![(keys::WEBSOCKET_HANDSHAKE_TIMEOUT_SECONDS, json!(1))])
        .expect("fast handshake timeout");

    let api = Api::new(&server);
    let alice = api.provision().await;
    let mut publisher = Publisher::connect(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    let terminal_id = publisher.open_terminal_id("pty0").await;

    // Connect but never subscribe.
    let mut mirror = Mirror::connect(&server, terminal_id, &alice.identity_token)
        .await
        .unwrap();
    let stream = mirror.drain(Duration::from_secs(10)).await;
    assert_eq!(
        stream.close_code,
        Some(4014),
        "expected the handshake_timeout close code"
    );

    server.shutdown().await;
}

// -------------------------------------------------------------- feature switches

#[tokio::test(flavor = "multi_thread")]
async fn feature_switches_disable_their_surfaces() {
    let server = TestServer::start().await;
    let api = Api::new(&server);
    let alice = api.provision().await;

    let mut publisher = Publisher::connect(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    let terminal_id = publisher.open_terminal_id("pty0").await;
    drop(publisher);

    server
        .patch_settings(vec![
            (keys::FEATURES_PUBLISH_ENABLED, json!(false)),
            (keys::FEATURES_MIRROR_ENABLED, json!(false)),
            (keys::FEATURES_METRICS_ENDPOINT_ENABLED, json!(false)),
            (keys::FEATURES_IDENTITY_REGISTRATION_ENABLED, json!(false)),
        ])
        .expect("disable features");

    assert!(
        Publisher::connect(&server, alice.device_id, &alice.device_token)
            .await
            .is_err()
    );
    assert!(
        Mirror::connect(&server, terminal_id, &alice.identity_token)
            .await
            .is_err()
    );

    let metrics = api.get("/metrics", Some(OPERATOR_TOKEN)).await;
    assert_eq!(metrics.status, 404);
    assert_eq!(metrics.error_code(), Some("feature_disabled"));

    let key = support::Key::random();
    let challenge = api
        .post(
            "/v1/auth/challenges",
            None,
            &json!({ "operation": "register_identity", "key": key.key_json() }),
        )
        .await;
    assert_eq!(challenge.status, 403);
    assert_eq!(challenge.error_code(), Some("feature_disabled"));

    // Re-enabling restores the surfaces, with no restart involved.
    server
        .patch_settings(vec![
            (keys::FEATURES_PUBLISH_ENABLED, json!(true)),
            (keys::FEATURES_MIRROR_ENABLED, json!(true)),
        ])
        .expect("re-enable features");
    assert!(
        Publisher::connect(&server, alice.device_id, &alice.device_token)
            .await
            .is_ok()
    );

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_replay_capacity_reduction_applies_to_resident_buffers_at_once() {
    let server = TestServer::start().await;
    let api = Api::new(&server);
    let alice = api.provision().await;

    let mut publisher = Publisher::connect(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    let terminal_id = publisher.open_terminal_id("pty0").await;

    publisher
        .send_output(terminal_id, 0, &vec![b'w'; 100_000])
        .await;
    publisher.wait_ack(terminal_id, 100_000).await;

    let before = api
        .get(
            &format!("/v1/terminals/{terminal_id}"),
            Some(&alice.identity_token),
        )
        .await;
    assert_eq!(before.body["retained_bytes"], 100_000);

    // A reduction bounds memory immediately, without waiting for the next append.
    server
        .patch_settings(vec![(keys::TERMINAL_REPLAY_CAPACITY_BYTES, json!(8192))])
        .expect("shrink the window");

    let shrunk = eventually(Duration::from_secs(10), || async {
        api.get(
            &format!("/v1/terminals/{terminal_id}"),
            Some(&alice.identity_token),
        )
        .await
        .body["retained_bytes"]
            == 8192
    })
    .await;
    assert!(shrunk, "a replay capacity reduction should evict at once");

    let response = api
        .get(
            &format!("/v1/terminals/{terminal_id}"),
            Some(&alice.identity_token),
        )
        .await;
    assert_eq!(
        response.body["next_offset"], 100_000,
        "offsets are unaffected by eviction"
    );
    assert_eq!(response.body["earliest_offset"], 100_000 - 8192);

    server.shutdown().await;
}

// --------------------------------------------------------------- schema upgrade

#[tokio::test(flavor = "multi_thread")]
async fn an_existing_database_is_migrated_to_the_current_schema() {
    // A database created before protocol version 2 has no `devices.role` and no
    // `terminals.accepts_input`. `CREATE TABLE IF NOT EXISTS` does nothing to a table
    // that already exists, so the upgrade path is the migration step, not the schema.
    let dir = tempfile::tempdir().expect("temp").keep();
    let db_path = dir.join("relay.db");

    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE identities (
                 identity_id TEXT PRIMARY KEY,
                 algorithm   TEXT NOT NULL,
                 public_key  BLOB NOT NULL,
                 created_at  TEXT NOT NULL,
                 UNIQUE (algorithm, public_key)
             );
             CREATE TABLE devices (
                 device_id       TEXT PRIMARY KEY,
                 identity_id     TEXT NOT NULL REFERENCES identities (identity_id),
                 algorithm       TEXT NOT NULL,
                 public_key      BLOB NOT NULL,
                 key_fingerprint TEXT NOT NULL UNIQUE,
                 name            TEXT NOT NULL,
                 created_at      TEXT NOT NULL,
                 last_seen_at    TEXT,
                 revoked_at      TEXT
             );
             CREATE TABLE terminals (
                 terminal_id      TEXT PRIMARY KEY,
                 device_id        TEXT NOT NULL REFERENCES devices (device_id),
                 identity_id      TEXT NOT NULL,
                 label            TEXT NOT NULL,
                 local_ref        TEXT NOT NULL,
                 state            TEXT NOT NULL CHECK (state IN ('open', 'closed')),
                 cols             INTEGER,
                 rows             INTEGER,
                 term             TEXT,
                 process_label    TEXT,
                 created_at       TEXT NOT NULL,
                 last_activity_at TEXT NOT NULL,
                 closed_at        TEXT,
                 close_reason     TEXT,
                 durable_offset   INTEGER NOT NULL DEFAULT 0,
                 earliest_offset  INTEGER NOT NULL DEFAULT 0
             );",
        )
        .unwrap();

        // A device and terminal written by the older build.
        conn.execute_batch(
            "INSERT INTO identities VALUES ('legacy-identity', 'ed25519', X'00', '2026-01-01T00:00:00Z');
             INSERT INTO devices VALUES ('11111111-1111-4111-8111-111111111111', 'legacy-identity',
                 'ed25519', X'00', 'legacy-fingerprint', 'old workstation',
                 '2026-01-01T00:00:00Z', NULL, NULL);
             INSERT INTO terminals VALUES ('22222222-2222-4222-8222-222222222222',
                 '11111111-1111-4111-8111-111111111111', 'legacy-identity', 'shell', 'pty0',
                 'closed', 80, 24, 'xterm-256color', NULL,
                 '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', '2026-01-01T00:01:00Z',
                 'process_exited', 0, 0);",
        )
        .unwrap();
    }

    // Opening it applies the migration in place.
    let server = TestServer::start_in(dir, None).await;

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let role: String = conn
        .query_row(
            "SELECT role FROM devices WHERE device_id = '11111111-1111-4111-8111-111111111111'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        role, "publisher",
        "an existing device keeps its version 1 meaning"
    );

    let accepts_input: i64 = conn
        .query_row(
            "SELECT accepts_input FROM terminals WHERE terminal_id = '22222222-2222-4222-8222-222222222222'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        accepts_input, 0,
        "an existing terminal does not silently accept input"
    );
    drop(conn);

    // The migrated database is fully usable: readiness passes and new work succeeds.
    let api = Api::new(&server);
    assert_eq!(api.get("/readyz", None).await.status, 200);

    let alice = api.provision().await;
    let mut publisher = Publisher::connect_v2(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    let terminal_id = publisher.open_input_terminal_id("pty0").await;
    publisher
        .send_output(terminal_id, 0, b"after upgrade")
        .await;
    publisher.wait_ack(terminal_id, 13).await;

    server.shutdown().await;
}
