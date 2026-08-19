//! Acceptance criteria for the relay itself: framing, replay, buffering, durability
//! and restart behaviour (spec §11 items 5-13 and 18).

mod support;

use serde_json::json;
use std::time::Duration;
use support::{Api, Mirror, OPERATOR_TOKEN, Publisher, TestServer, eventually};
use terminal_relay::settings::defs::keys;
use uuid::Uuid;

/// Settings that stop the checkpoint task from flushing on its own, so tests can
/// control exactly when output becomes durable.
fn manual_flush_settings() -> Vec<(&'static str, serde_json::Value)> {
    vec![
        (keys::PERSISTENCE_FLUSH_INTERVAL_MS, json!(60_000)),
        (keys::PERSISTENCE_FLUSH_BYTES, json!(4_000_000)),
    ]
}

/// How many checkpoint transactions persisted output for one terminal.
///
/// Counted from that terminal's own chunk rows rather than from
/// `relay_checkpoint_transactions_total`: metric counters are process-global statics
/// shared by every test server in this binary, so a concurrently running test would
/// inflate the reading and make the assertion flaky. Each checkpoint appends at most
/// one chunk row per terminal, which is exactly the quantity this criterion is about.
fn checkpoints_for(server: &TestServer, terminal_id: Uuid) -> u64 {
    let conn = rusqlite::Connection::open(server.data_dir.join("relay.db")).expect("open db");
    conn.query_row(
        "SELECT COUNT(*) FROM terminal_output WHERE terminal_id = ?1",
        rusqlite::params![terminal_id.to_string()],
        |row| row.get::<_, i64>(0),
    )
    .expect("count chunks")
    .max(0) as u64
}

// ---------------------------------------------------------------- criterion 5

#[tokio::test(flavor = "multi_thread")]
async fn criterion_5_a_device_advertises_zero_one_or_many_terminals() {
    let server = TestServer::start().await;
    let api = Api::new(&server);
    let alice = api.provision().await;

    // Zero terminals: the connection is valid and lists nothing.
    let mut publisher = Publisher::connect(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    let listed = api.get("/v1/terminals", Some(&alice.identity_token)).await;
    assert_eq!(listed.body["terminals"].as_array().unwrap().len(), 0);

    // One terminal.
    let first = publisher.open_terminal_id("pty0").await;
    let listed = api.get("/v1/terminals", Some(&alice.identity_token)).await;
    assert_eq!(listed.body["terminals"].as_array().unwrap().len(), 1);

    // Many concurrent terminals on one connection, each with independent offsets.
    let second = publisher.open_terminal_id("pty1").await;
    let third = publisher.open_terminal_id("pty2").await;
    assert_ne!(first, second);
    assert_ne!(second, third);

    publisher.send_output(first, 0, b"one").await;
    publisher.send_output(second, 0, b"two-two").await;
    publisher.send_output(third, 0, b"three-three").await;
    publisher.wait_ack(third, 11).await;

    for (terminal_id, expected) in [(first, 3), (second, 7), (third, 11)] {
        let response = api
            .get(
                &format!("/v1/terminals/{terminal_id}"),
                Some(&alice.identity_token),
            )
            .await;
        assert_eq!(
            response.body["next_offset"], expected,
            "terminal {terminal_id}"
        );
    }

    let listed = api.get("/v1/terminals", Some(&alice.identity_token)).await;
    assert_eq!(listed.body["terminals"].as_array().unwrap().len(), 3);

    // Filters work.
    let open = api
        .get(
            &format!("/v1/terminals?state=open&device_id={}", alice.device_id),
            Some(&alice.identity_token),
        )
        .await;
    assert_eq!(open.body["terminals"].as_array().unwrap().len(), 3);
    let closed = api
        .get("/v1/terminals?state=closed", Some(&alice.identity_token))
        .await;
    assert_eq!(closed.body["terminals"].as_array().unwrap().len(), 0);

    // Opening the same local_ref again is idempotent (spec §3.3).
    let reopened = publisher.open_terminal("pty0").await;
    assert_eq!(reopened["terminal_id"].as_str().unwrap(), first.to_string());
    assert_eq!(reopened["deduplicated"], json!(true));

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_closed_terminal_id_is_never_reused() {
    let server = TestServer::start().await;
    let api = Api::new(&server);
    let alice = api.provision().await;

    let mut publisher = Publisher::connect(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    let first = publisher.open_terminal_id("pty0").await;
    publisher.send_output(first, 0, b"hello").await;
    publisher.wait_ack(first, 5).await;
    publisher.close_terminal(first, "process_exited").await;

    let closed = eventually(Duration::from_secs(10), || async {
        let response = api
            .get(
                &format!("/v1/terminals/{first}"),
                Some(&alice.identity_token),
            )
            .await;
        response.body["state"] == "closed"
    })
    .await;
    assert!(closed, "the terminal should close");

    // The same local_ref yields a *new* terminal starting at offset zero.
    let second = publisher.open_terminal("pty0").await;
    let second_id = second["terminal_id"].as_str().unwrap();
    assert_ne!(second_id, first.to_string());
    assert_eq!(second["next_offset"], 0);
    assert_eq!(second["deduplicated"], json!(false));

    server.shutdown().await;
}

// ---------------------------------------------------------------- criterion 6

#[tokio::test(flavor = "multi_thread")]
async fn criterion_6_arbitrary_binary_output_is_relayed_unmodified_and_in_order() {
    let server = TestServer::start().await;
    let api = Api::new(&server);
    let alice = api.provision().await;

    let mut publisher = Publisher::connect(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    let terminal_id = publisher.open_terminal_id("pty0").await;

    let mut mirror = Mirror::connect(&server, terminal_id, &alice.identity_token)
        .await
        .unwrap();
    let subscribed = mirror.subscribe(Some(0)).await;
    assert_eq!(subscribed["type"], "subscribed");

    // Every byte value, invalid UTF-8, embedded NULs, bare CR, and ANSI escapes.
    let mut payload: Vec<u8> = (0u8..=255).collect();
    payload.extend_from_slice(&[0xff, 0xfe, 0xfd]); // invalid UTF-8 sequences
    payload.extend_from_slice(b"\x1b[31mred\x1b[0m");
    payload.extend_from_slice(b"line1\rline2\nline3\r\n");
    payload.extend_from_slice(&[0x00, 0x07, 0x08, 0x1b]);

    let mut offset = 0u64;
    for chunk in payload.chunks(37) {
        publisher.send_output(terminal_id, offset, chunk).await;
        offset += chunk.len() as u64;
    }

    let stream = mirror.collect(payload.len(), Duration::from_secs(15)).await;
    assert_eq!(
        stream.bytes, payload,
        "terminal output must be relayed byte for byte with no transformation"
    );
    stream.assert_contiguous_from(0);

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn zero_length_output_frames_are_accepted_but_never_relayed() {
    let server = TestServer::start().await;
    let api = Api::new(&server);
    let alice = api.provision().await;

    let mut publisher = Publisher::connect(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    let terminal_id = publisher.open_terminal_id("pty0").await;

    let mut mirror = Mirror::connect(&server, terminal_id, &alice.identity_token)
        .await
        .unwrap();
    mirror.subscribe(Some(0)).await;

    publisher.send_output(terminal_id, 0, b"").await;
    publisher.send_output(terminal_id, 0, b"real").await;

    let stream = mirror.collect(4, Duration::from_secs(10)).await;
    assert_eq!(stream.bytes, b"real");
    // No zero-length frame was delivered (spec §6.2).
    assert!(stream.frames.iter().all(|(_, len)| *len > 0));
    assert_eq!(stream.frames.len(), 1);

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn malformed_binary_frames_close_with_protocol_error() {
    let server = TestServer::start().await;
    let api = Api::new(&server);
    let alice = api.provision().await;

    let mut publisher = Publisher::connect(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    publisher.open_terminal_id("pty0").await;

    // Shorter than the fixed 25-byte publisher header.
    publisher.send_raw(vec![0x01, 0x02, 0x03]).await;
    let error = publisher
        .expect_message("error", Duration::from_secs(20))
        .await;
    assert_eq!(error["code"], "invalid_message");
    let code = publisher.expect_close(Duration::from_secs(20)).await;
    assert_eq!(
        code,
        Some(1002),
        "malformed frames close with 1002 (spec §6)"
    );

    // An unknown frame type is treated the same way.
    let mut publisher = Publisher::connect(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    let mut frame = vec![0x7f];
    frame.extend_from_slice(&[0u8; 24]);
    publisher.send_raw(frame).await;
    let error = publisher
        .expect_message("error", Duration::from_secs(20))
        .await;
    assert_eq!(error["code"], "unknown_message_type");
    assert_eq!(
        publisher.expect_close(Duration::from_secs(20)).await,
        Some(1002)
    );

    server.shutdown().await;
}

// ---------------------------------------------------------------- criterion 7

#[tokio::test(flavor = "multi_thread")]
async fn criterion_7_replay_is_followed_by_live_bytes_with_no_seam() {
    let server = TestServer::start().await;
    let api = Api::new(&server);
    let alice = api.provision().await;

    let mut publisher = Publisher::connect(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    let terminal_id = publisher.open_terminal_id("pty0").await;

    // Ten frames before the subscriber exists.
    let mut expected: Vec<u8> = Vec::new();
    let mut offset = 0u64;
    for index in 0u8..10 {
        let chunk = vec![index; 5_000];
        publisher.send_output(terminal_id, offset, &chunk).await;
        offset += chunk.len() as u64;
        expected.extend_from_slice(&chunk);
    }
    publisher.wait_ack(terminal_id, offset).await;

    // Subscribe: everything so far must arrive as replay.
    let mut mirror = Mirror::connect(&server, terminal_id, &alice.identity_token)
        .await
        .unwrap();
    let subscribed = mirror.subscribe(None).await;
    assert_eq!(subscribed["replay_start_offset"], 0);
    assert_eq!(subscribed["next_offset"], offset);
    assert_eq!(subscribed["terminal_state"], "open");

    // Ten more frames afterwards, delivered live.
    for index in 10u8..20 {
        let chunk = vec![index; 5_000];
        publisher.send_output(terminal_id, offset, &chunk).await;
        offset += chunk.len() as u64;
        expected.extend_from_slice(&chunk);
    }

    let stream = mirror
        .collect(expected.len(), Duration::from_secs(20))
        .await;
    assert_eq!(
        stream.bytes.len(),
        expected.len(),
        "no bytes lost or duplicated"
    );
    assert_eq!(
        stream.bytes, expected,
        "replay and live bytes must join seamlessly"
    );
    stream.assert_contiguous_from(0);

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn durable_notices_only_ever_advance() {
    let server = TestServer::start().await;
    let api = Api::new(&server);
    let alice = api.provision().await;

    let mut publisher = Publisher::connect(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    let terminal_id = publisher.open_terminal_id("pty0").await;

    let mut mirror = Mirror::connect(&server, terminal_id, &alice.identity_token)
        .await
        .unwrap();
    mirror.subscribe(Some(0)).await;

    let mut offset = 0u64;
    for index in 0u8..5 {
        let chunk = vec![index; 1000];
        publisher.send_output(terminal_id, offset, &chunk).await;
        offset += 1000;
        publisher.wait_ack(terminal_id, offset).await;
    }

    let stream = mirror.collect(5000, Duration::from_secs(15)).await;
    assert!(
        !stream.durable_offsets.is_empty(),
        "subscribers must receive durable notices"
    );
    let mut previous = 0;
    for durable in &stream.durable_offsets {
        assert!(
            *durable >= previous,
            "durable_offset must be monotonic: {:?}",
            stream.durable_offsets
        );
        previous = *durable;
    }
    assert!(previous <= offset);

    server.shutdown().await;
}

// ---------------------------------------------------------------- criterion 8

#[tokio::test(flavor = "multi_thread")]
async fn criterion_8_a_subscriber_resumes_from_a_processed_offset() {
    let server = TestServer::start().await;
    let api = Api::new(&server);
    let alice = api.provision().await;

    let mut publisher = Publisher::connect(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    let terminal_id = publisher.open_terminal_id("pty0").await;

    publisher
        .send_output(terminal_id, 0, &vec![b'a'; 1000])
        .await;
    publisher
        .send_output(terminal_id, 1000, &vec![b'b'; 1000])
        .await;
    publisher.wait_ack(terminal_id, 2000).await;

    // Resume from the middle: only the unprocessed suffix arrives.
    let mut mirror = Mirror::connect(&server, terminal_id, &alice.identity_token)
        .await
        .unwrap();
    let subscribed = mirror.subscribe(Some(1000)).await;
    assert_eq!(subscribed["replay_start_offset"], 1000);
    let stream = mirror.collect(1000, Duration::from_secs(10)).await;
    assert_eq!(stream.bytes, vec![b'b'; 1000]);
    stream.assert_contiguous_from(1000);
    assert!(
        stream.control_of_type("gap").is_none(),
        "no gap should be reported"
    );

    // Resuming exactly at next_offset is valid and replays nothing.
    let mut mirror = Mirror::connect(&server, terminal_id, &alice.identity_token)
        .await
        .unwrap();
    let subscribed = mirror.subscribe(Some(2000)).await;
    assert_eq!(subscribed["replay_start_offset"], 2000);
    let stream = mirror.drain(Duration::from_millis(500)).await;
    assert!(stream.bytes.is_empty());

    // An offset beyond next_offset fails explicitly (spec §6.2).
    let mut mirror = Mirror::connect(&server, terminal_id, &alice.identity_token)
        .await
        .unwrap();
    let reply = mirror.subscribe(Some(9_999)).await;
    assert_eq!(reply["type"], "error");
    assert_eq!(reply["code"], "offset_ahead");
    assert_eq!(reply["next_offset"], 2000);
    assert!(reply["durable_offset"].is_u64());

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn criterion_8_an_evicted_offset_produces_an_explicit_gap() {
    let server = TestServer::start().await;
    server
        .patch_settings(vec![(keys::TERMINAL_REPLAY_CAPACITY_BYTES, json!(4096))])
        .expect("shrink replay window");

    let api = Api::new(&server);
    let alice = api.provision().await;
    let mut publisher = Publisher::connect(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    let terminal_id = publisher.open_terminal_id("pty0").await;

    // Write well past the window so the early bytes are evicted.
    let mut offset = 0u64;
    for index in 0u8..10 {
        let chunk = vec![index; 1000];
        publisher.send_output(terminal_id, offset, &chunk).await;
        offset += 1000;
    }
    publisher.wait_ack(terminal_id, offset).await;

    let mut mirror = Mirror::connect(&server, terminal_id, &alice.identity_token)
        .await
        .unwrap();
    let subscribed = mirror.subscribe(Some(0)).await;
    assert_eq!(subscribed["requested_from_offset"], 0);
    let replay_start = subscribed["replay_start_offset"].as_u64().unwrap();
    assert_eq!(
        replay_start,
        offset - 4096,
        "replay starts at the earliest retained byte"
    );

    let stream = mirror.collect(4096, Duration::from_secs(10)).await;
    let gap = stream
        .control_of_type("gap")
        .expect("a gap notice is required");
    assert_eq!(gap["requested_from_offset"], 0);
    assert_eq!(gap["available_from_offset"], replay_start);
    stream.assert_contiguous_from(replay_start);

    server.shutdown().await;
}

// ---------------------------------------------------------------- criterion 9

#[tokio::test(flavor = "multi_thread")]
async fn criterion_9_the_replay_window_never_exceeds_1_500_000_bytes() {
    let server = TestServer::start().await;
    let api = Api::new(&server);
    let alice = api.provision().await;

    let mut publisher = Publisher::connect(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    let terminal_id = publisher.open_terminal_id("pty0").await;

    // Publish 2 MB in 200 kB frames, comfortably past the 1.5 MB decimal window.
    let mut all: Vec<u8> = Vec::new();
    let mut offset = 0u64;
    for index in 0u8..10 {
        let chunk = vec![index; 200_000];
        publisher.send_output(terminal_id, offset, &chunk).await;
        offset += chunk.len() as u64;
        all.extend_from_slice(&chunk);
        publisher.wait_ack(terminal_id, offset).await;
    }
    assert_eq!(offset, 2_000_000);

    let response = api
        .get(
            &format!("/v1/terminals/{terminal_id}"),
            Some(&alice.identity_token),
        )
        .await;
    assert_eq!(response.body["next_offset"], 2_000_000);
    assert_eq!(
        response.body["retained_bytes"], 1_500_000,
        "the retained window is exactly the specification maximum"
    );
    assert_eq!(response.body["earliest_offset"], 500_000);

    // The retained data is exactly the newest contiguous suffix.
    let mut mirror = Mirror::connect(&server, terminal_id, &alice.identity_token)
        .await
        .unwrap();
    let subscribed = mirror.subscribe(None).await;
    assert_eq!(subscribed["replay_start_offset"], 500_000);
    let stream = mirror.collect(1_500_000, Duration::from_secs(30)).await;
    assert_eq!(stream.bytes.len(), 1_500_000);
    assert_eq!(
        stream.bytes,
        all[500_000..],
        "retained bytes must be the newest suffix"
    );
    stream.assert_contiguous_from(500_000);

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_frame_larger_than_the_window_advances_fully_and_retains_its_tail() {
    let server = TestServer::start().await;
    server
        .patch_settings(vec![
            (keys::TERMINAL_REPLAY_CAPACITY_BYTES, json!(4096)),
            (keys::LIMITS_MAX_OUTPUT_FRAME_BYTES, json!(65_536)),
        ])
        .expect("configure a window smaller than one frame");

    let api = Api::new(&server);
    let alice = api.provision().await;
    let mut publisher = Publisher::connect(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    let terminal_id = publisher.open_terminal_id("pty0").await;

    // One frame larger than the whole replay window (spec §7.1 requires acceptance).
    let big: Vec<u8> = (0..20_000u32).map(|i| (i % 251) as u8).collect();
    publisher.send_output(terminal_id, 0, &big).await;
    publisher.wait_ack(terminal_id, 20_000).await;

    let response = api
        .get(
            &format!("/v1/terminals/{terminal_id}"),
            Some(&alice.identity_token),
        )
        .await;
    assert_eq!(
        response.body["next_offset"], 20_000,
        "offsets advance by the full length"
    );
    assert_eq!(response.body["retained_bytes"], 4096);
    assert_eq!(response.body["earliest_offset"], 20_000 - 4096);

    let mut mirror = Mirror::connect(&server, terminal_id, &alice.identity_token)
        .await
        .unwrap();
    mirror.subscribe(None).await;
    let stream = mirror.collect(4096, Duration::from_secs(10)).await;
    assert_eq!(
        stream.bytes,
        big[20_000 - 4096..],
        "only the frame's tail is retained"
    );

    server.shutdown().await;
}

// --------------------------------------------------------------- criterion 10

#[tokio::test(flavor = "multi_thread")]
async fn criterion_10_many_frames_coalesce_into_far_fewer_transactions() {
    let server = TestServer::start().await;
    // Default batching: 5 s interval, 256 kB threshold.
    let api = Api::new(&server);
    let alice = api.provision().await;

    let mut publisher = Publisher::connect(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    let terminal_id = publisher.open_terminal_id("pty0").await;

    // 500 small frames under sustained load.
    const FRAMES: u64 = 500;
    const FRAME_BYTES: usize = 512;
    let mut offset = 0u64;
    for _ in 0..FRAMES {
        publisher
            .send_output(terminal_id, offset, &vec![b'q'; FRAME_BYTES])
            .await;
        offset += FRAME_BYTES as u64;
    }
    publisher.wait_ack(terminal_id, offset).await;

    let transactions = checkpoints_for(&server, terminal_id);

    assert!(transactions >= 1, "output must reach durable storage");
    assert!(
        transactions < FRAMES / 10,
        "expected many frames coalesced into far fewer transactions, got {transactions} for {FRAMES} frames"
    );

    // Every byte is still accounted for, and is served from memory.
    let response = api
        .get(
            &format!("/v1/terminals/{terminal_id}"),
            Some(&alice.identity_token),
        )
        .await;
    assert_eq!(response.body["next_offset"], offset);
    assert_eq!(response.body["durable_offset"], offset);

    server.shutdown().await;
}

// --------------------------------------------------------------- criterion 11

#[tokio::test(flavor = "multi_thread")]
async fn criterion_11_acknowledgements_follow_commits_and_survive_restart() {
    let server = TestServer::start().await;
    server
        .patch_settings(manual_flush_settings())
        .expect("manual flush");
    let api = Api::new(&server);
    let alice = api.provision().await;

    let mut publisher = Publisher::connect(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    let terminal_id = publisher.open_terminal_id("pty0").await;

    publisher
        .send_output(terminal_id, 0, b"durable bytes")
        .await;

    // With automatic flushing effectively disabled, no acknowledgement may appear.
    let quiet = tokio::time::timeout(Duration::from_secs(2), publisher.next_json()).await;
    assert!(
        quiet.is_err(),
        "an acknowledgement must not precede a commit, saw {quiet:?}"
    );
    let response = api
        .get(
            &format!("/v1/terminals/{terminal_id}"),
            Some(&alice.identity_token),
        )
        .await;
    assert_eq!(
        response.body["next_offset"], 13,
        "bytes are accepted into memory immediately"
    );
    assert_eq!(
        response.body["durable_offset"], 0,
        "but are not yet durable"
    );

    // An explicit operator flush is one of the required triggers (spec §7.2).
    let flush = api
        .post("/v1/admin/flush", Some(OPERATOR_TOKEN), &json!({}))
        .await;
    assert_eq!(flush.status, 200, "{:?}", flush.body);

    let ack = publisher.wait_ack(terminal_id, 13).await;
    assert_eq!(ack["durable_offset"], 13);
    assert_eq!(ack["next_offset"], 13);

    // Acknowledged output and monotonic offsets survive a restart.
    let restarted = server.restart(Some(manual_flush_settings())).await;
    let api = Api::new(&restarted);

    let response = api
        .get(
            &format!("/v1/terminals/{terminal_id}"),
            Some(&alice.identity_token),
        )
        .await;
    assert_eq!(response.status, 200, "{:?}", response.body);
    assert_eq!(response.body["durable_offset"], 13);
    assert_eq!(response.body["next_offset"], 13);
    assert_eq!(response.body["retained_bytes"], 13);

    // The retained bytes are still readable.
    let mut mirror = Mirror::connect(&restarted, terminal_id, &alice.identity_token)
        .await
        .unwrap();
    mirror.subscribe(Some(0)).await;
    let stream = mirror.collect(13, Duration::from_secs(10)).await;
    assert_eq!(stream.bytes, b"durable bytes");

    restarted.shutdown().await;
}

// --------------------------------------------------------------- criterion 12

#[tokio::test(flavor = "multi_thread")]
async fn criterion_12_a_crash_rolls_back_to_durable_offset_without_duplication() {
    let server = TestServer::start().await;
    server
        .patch_settings(manual_flush_settings())
        .expect("manual flush");
    let api = Api::new(&server);
    let alice = api.provision().await;

    let mut publisher = Publisher::connect(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    let terminal_id = publisher.open_terminal_id("pty0").await;

    // Committed prefix.
    publisher.send_output(terminal_id, 0, b"committed-").await;
    api.post("/v1/admin/flush", Some(OPERATOR_TOKEN), &json!({}))
        .await;
    publisher.wait_ack(terminal_id, 10).await;

    // Uncommitted suffix: accepted into memory and even relayed live, but never
    // acknowledged, so the publisher must still be holding it.
    let mut mirror = Mirror::connect(&server, terminal_id, &alice.identity_token)
        .await
        .unwrap();
    mirror.subscribe(Some(10)).await;
    publisher
        .send_output(terminal_id, 10, b"in-memory-only")
        .await;
    let live = mirror.collect(14, Duration::from_secs(20)).await;
    assert_eq!(
        live.bytes, b"in-memory-only",
        "memory-resident bytes are relayed live"
    );

    // Crash: the process dies with dirty bytes still in memory.
    let restarted = server
        .restart_after_crash(Some(manual_flush_settings()))
        .await;
    let api = Api::new(&restarted);

    // Offsets rolled back to durable_offset, and no further.
    let response = api
        .get(
            &format!("/v1/terminals/{terminal_id}"),
            Some(&alice.identity_token),
        )
        .await;
    assert_eq!(response.body["durable_offset"], 10);
    assert_eq!(response.body["next_offset"], 10);

    // The publisher reconnects, learns the authoritative offset, and retransmits.
    let device_token = api.device_token(&alice.device_key).await;
    let mut publisher = Publisher::connect(&restarted, alice.device_id, &device_token)
        .await
        .unwrap();
    let opened = publisher.open_terminal("pty0").await;
    assert_eq!(
        opened["terminal_id"].as_str().unwrap(),
        terminal_id.to_string()
    );
    assert_eq!(opened["deduplicated"], json!(true));
    assert_eq!(
        opened["next_offset"], 10,
        "resume exactly at the durable offset"
    );

    publisher
        .send_output(terminal_id, 10, b"in-memory-only")
        .await;
    api.post("/v1/admin/flush", Some(OPERATOR_TOKEN), &json!({}))
        .await;
    publisher.wait_ack(terminal_id, 24).await;

    // The stream contains the retransmitted bytes exactly once.
    let mut mirror = Mirror::connect(&restarted, terminal_id, &alice.identity_token)
        .await
        .unwrap();
    mirror.subscribe(Some(0)).await;
    let stream = mirror.collect(24, Duration::from_secs(10)).await;
    assert_eq!(stream.bytes, b"committed-in-memory-only");
    stream.assert_contiguous_from(0);

    restarted.shutdown().await;
}

// --------------------------------------------------------------- criterion 13

#[tokio::test(flavor = "multi_thread")]
async fn criterion_13_retries_do_not_duplicate_and_mismatches_do_not_mutate() {
    let server = TestServer::start().await;
    let api = Api::new(&server);
    let alice = api.provision().await;

    let mut publisher = Publisher::connect(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    let terminal_id = publisher.open_terminal_id("pty0").await;

    let mut mirror = Mirror::connect(&server, terminal_id, &alice.identity_token)
        .await
        .unwrap();
    mirror.subscribe(Some(0)).await;

    publisher.send_output(terminal_id, 0, b"hello").await;

    // A duplicate retry of an already-accepted frame is rejected, not appended.
    publisher.send_output(terminal_id, 0, b"hello").await;
    let error = publisher
        .expect_message("error", Duration::from_secs(20))
        .await;
    assert_eq!(error["code"], "offset_mismatch");
    assert_eq!(error["next_offset"], 5);
    assert_eq!(error["terminal_id"], json!(terminal_id));

    // A frame that skips ahead is also rejected, and changes nothing.
    publisher.send_output(terminal_id, 500, b"gap").await;
    let error = publisher
        .expect_message("error", Duration::from_secs(20))
        .await;
    assert_eq!(error["code"], "offset_mismatch");
    assert_eq!(error["next_offset"], 5);

    // A stale offset below next_offset is rejected too.
    publisher.send_output(terminal_id, 2, b"xx").await;
    let error = publisher
        .expect_message("error", Duration::from_secs(20))
        .await;
    assert_eq!(error["code"], "offset_mismatch");

    // After all rejections, the correct next frame is accepted.
    publisher.send_output(terminal_id, 5, b"-world").await;
    publisher.wait_ack(terminal_id, 11).await;

    let response = api
        .get(
            &format!("/v1/terminals/{terminal_id}"),
            Some(&alice.identity_token),
        )
        .await;
    assert_eq!(
        response.body["next_offset"], 11,
        "rejected frames must not advance offsets"
    );

    let stream = mirror.collect(11, Duration::from_secs(10)).await;
    assert_eq!(stream.bytes, b"hello-world", "no byte was duplicated");
    stream.assert_contiguous_from(0);

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_publisher_supersedes_the_previous_one() {
    let server = TestServer::start().await;
    let api = Api::new(&server);
    let alice = api.provision().await;

    let mut first = Publisher::connect(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    let terminal_id = first.open_terminal_id("pty0").await;
    first.send_output(terminal_id, 0, b"from first").await;
    first.wait_ack(terminal_id, 10).await;

    // A second authenticated connection takes over the device (spec §6.1).
    let mut second = Publisher::connect(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();

    let code = first.expect_close(Duration::from_secs(10)).await;
    assert_eq!(
        code,
        Some(4002),
        "the older connection closes with the superseded code"
    );

    // The new connection resumes the same terminal at the authoritative offset.
    let opened = second.open_terminal("pty0").await;
    assert_eq!(
        opened["terminal_id"].as_str().unwrap(),
        terminal_id.to_string()
    );
    assert_eq!(opened["next_offset"], 10);
    second.send_output(terminal_id, 10, b" and second").await;
    second.wait_ack(terminal_id, 21).await;

    let mut mirror = Mirror::connect(&server, terminal_id, &alice.identity_token)
        .await
        .unwrap();
    mirror.subscribe(Some(0)).await;
    let stream = mirror.collect(21, Duration::from_secs(10)).await;
    assert_eq!(stream.bytes, b"from first and second");

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_disconnected_publisher_closes_its_terminals_after_the_grace_period() {
    let server = TestServer::start().await;
    server
        .patch_settings(vec![(
            keys::TERMINAL_PUBLISHER_RECONNECT_GRACE_SECONDS,
            json!(0),
        )])
        .expect("zero grace period");

    let api = Api::new(&server);
    let alice = api.provision().await;

    let mut publisher = Publisher::connect(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    let terminal_id = publisher.open_terminal_id("pty0").await;
    publisher.send_output(terminal_id, 0, b"work").await;
    publisher.wait_ack(terminal_id, 4).await;

    // Drop the connection without closing the terminal.
    drop(publisher);

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
    assert!(closed, "terminals should close after the grace period");

    let response = api
        .get(
            &format!("/v1/terminals/{terminal_id}"),
            Some(&alice.identity_token),
        )
        .await;
    assert_eq!(response.body["close_reason"], "publisher_disconnected");
    assert_eq!(
        response.body["durable_offset"], 4,
        "committed output is unaffected"
    );

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn subscribers_see_resize_and_close_in_stream_order() {
    let server = TestServer::start().await;
    let api = Api::new(&server);
    let alice = api.provision().await;

    let mut publisher = Publisher::connect(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    let terminal_id = publisher.open_terminal_id("pty0").await;

    let mut mirror = Mirror::connect(&server, terminal_id, &alice.identity_token)
        .await
        .unwrap();
    mirror.subscribe(Some(0)).await;

    publisher.send_output(terminal_id, 0, b"before").await;
    publisher
        .send_json(&json!({
            "type": "terminal.resize",
            "terminal_id": terminal_id,
            "cols": 160,
            "rows": 50,
        }))
        .await;
    publisher.send_output(terminal_id, 6, b"after").await;
    publisher.wait_ack(terminal_id, 11).await;
    publisher
        .close_terminal(terminal_id, "process_exited")
        .await;

    let stream = mirror.drain(Duration::from_secs(20)).await;
    assert_eq!(stream.bytes, b"beforeafter");

    let resize = stream
        .control_of_type("terminal.resize")
        .expect("resize is forwarded");
    assert_eq!(resize["cols"], 160);
    assert_eq!(resize["rows"], 50);

    let closed = stream
        .control_of_type("terminal.closed")
        .expect("close is forwarded");
    assert_eq!(closed["reason"], "process_exited");
    // Every accepted byte was committed before the close was announced (spec §6.2).
    assert_eq!(closed["durable_offset"], 11);
    assert_eq!(closed["next_offset"], 11);
    assert_eq!(stream.close_code, Some(1000));

    // The persisted size reflects the resize.
    let response = api
        .get(
            &format!("/v1/terminals/{terminal_id}"),
            Some(&alice.identity_token),
        )
        .await;
    assert_eq!(response.body["cols"], 160);
    assert_eq!(response.body["rows"], 50);

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn several_subscribers_each_receive_the_whole_stream() {
    let server = TestServer::start().await;
    let api = Api::new(&server);
    let alice = api.provision().await;

    let mut publisher = Publisher::connect(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    let terminal_id = publisher.open_terminal_id("pty0").await;

    // One subscriber joins before any output, another after a prefix exists, so one
    // gets everything live and the other gets replay followed by live.
    let mut early = Mirror::connect(&server, terminal_id, &alice.identity_token)
        .await
        .unwrap();
    early.subscribe(Some(0)).await;

    let mut expected: Vec<u8> = Vec::new();
    let mut offset = 0u64;
    for index in 0u8..5 {
        let chunk = vec![index; 2_000];
        publisher.send_output(terminal_id, offset, &chunk).await;
        offset += chunk.len() as u64;
        expected.extend_from_slice(&chunk);
    }
    publisher.wait_ack(terminal_id, offset).await;

    let mut late = Mirror::connect(&server, terminal_id, &alice.identity_token)
        .await
        .unwrap();
    late.subscribe(Some(0)).await;

    for index in 5u8..10 {
        let chunk = vec![index; 2_000];
        publisher.send_output(terminal_id, offset, &chunk).await;
        offset += chunk.len() as u64;
        expected.extend_from_slice(&chunk);
    }

    let early_stream = early.collect(expected.len(), Duration::from_secs(20)).await;
    let late_stream = late.collect(expected.len(), Duration::from_secs(20)).await;

    assert_eq!(
        early_stream.bytes, expected,
        "the early subscriber saw the whole stream"
    );
    assert_eq!(
        late_stream.bytes, expected,
        "the late subscriber saw replay then live"
    );
    early_stream.assert_contiguous_from(0);
    late_stream.assert_contiguous_from(0);

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn byte_order_is_preserved_separately_per_terminal() {
    let server = TestServer::start().await;
    let api = Api::new(&server);
    let alice = api.provision().await;

    let mut publisher = Publisher::connect(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    let first = publisher.open_terminal_id("pty0").await;
    let second = publisher.open_terminal_id("pty1").await;

    let mut mirror_first = Mirror::connect(&server, first, &alice.identity_token)
        .await
        .unwrap();
    mirror_first.subscribe(Some(0)).await;
    let mut mirror_second = Mirror::connect(&server, second, &alice.identity_token)
        .await
        .unwrap();
    mirror_second.subscribe(Some(0)).await;

    // Interleave writes across the two terminals.
    let mut expected_first: Vec<u8> = Vec::new();
    let mut expected_second: Vec<u8> = Vec::new();
    let mut offset_first = 0u64;
    let mut offset_second = 0u64;
    for index in 0u8..10 {
        let a = vec![b'A' + index; 500];
        publisher.send_output(first, offset_first, &a).await;
        offset_first += a.len() as u64;
        expected_first.extend_from_slice(&a);

        let b = vec![b'a' + index; 700];
        publisher.send_output(second, offset_second, &b).await;
        offset_second += b.len() as u64;
        expected_second.extend_from_slice(&b);
    }

    let stream_first = mirror_first
        .collect(expected_first.len(), Duration::from_secs(20))
        .await;
    let stream_second = mirror_second
        .collect(expected_second.len(), Duration::from_secs(20))
        .await;

    // Each stream carries exactly its own terminal's bytes, in order.
    assert_eq!(stream_first.bytes, expected_first);
    assert_eq!(stream_second.bytes, expected_second);
    stream_first.assert_contiguous_from(0);
    stream_second.assert_contiguous_from(0);

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn terminal_listing_filters_cannot_reach_another_identity() {
    let server = TestServer::start().await;
    let api = Api::new(&server);
    let alice = api.provision().await;
    let bob = api.provision().await;

    let mut publisher = Publisher::connect(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    publisher.open_terminal_id("pty0").await;

    // Filtering by another identity's device yields nothing rather than leaking.
    let response = api
        .get(
            &format!("/v1/terminals?device_id={}", alice.device_id),
            Some(&bob.identity_token),
        )
        .await;
    assert_eq!(response.status, 200);
    assert_eq!(response.body["terminals"].as_array().unwrap().len(), 0);

    // Alice sees her own.
    let own = api
        .get(
            &format!("/v1/terminals?device_id={}", alice.device_id),
            Some(&alice.identity_token),
        )
        .await;
    assert_eq!(own.body["terminals"].as_array().unwrap().len(), 1);

    server.shutdown().await;
}

// --------------------------------------------------------------- criterion 18

#[tokio::test(flavor = "multi_thread")]
async fn criterion_18_graceful_shutdown_flushes_dirty_output() {
    let server = TestServer::start().await;
    server
        .patch_settings(manual_flush_settings())
        .expect("manual flush");
    let api = Api::new(&server);
    let alice = api.provision().await;

    let mut publisher = Publisher::connect(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    let terminal_id = publisher.open_terminal_id("pty0").await;
    publisher
        .send_output(terminal_id, 0, b"unflushed at shutdown")
        .await;

    // Nothing is durable yet.
    let response = api
        .get(
            &format!("/v1/terminals/{terminal_id}"),
            Some(&alice.identity_token),
        )
        .await;
    assert_eq!(response.body["durable_offset"], 0);
    assert_eq!(response.body["next_offset"], 21);

    // Shutdown must drain dirty output before exiting (spec §8, §10).
    let restarted = server.restart(Some(manual_flush_settings())).await;
    let api = Api::new(&restarted);
    let response = api
        .get(
            &format!("/v1/terminals/{terminal_id}"),
            Some(&alice.identity_token),
        )
        .await;
    assert_eq!(
        response.body["durable_offset"], 21,
        "graceful shutdown must commit accepted output"
    );

    let mut mirror = Mirror::connect(&restarted, terminal_id, &alice.identity_token)
        .await
        .unwrap();
    mirror.subscribe(Some(0)).await;
    let stream = mirror.collect(21, Duration::from_secs(10)).await;
    assert_eq!(stream.bytes, b"unflushed at shutdown");

    restarted.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn durable_state_lives_in_the_configured_data_directory() {
    let server = TestServer::start().await;
    let api = Api::new(&server);
    let alice = api.provision().await;

    let mut publisher = Publisher::connect(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    let terminal_id = publisher.open_terminal_id("pty0").await;
    publisher.send_output(terminal_id, 0, b"persisted").await;
    publisher.wait_ack(terminal_id, 9).await;

    // Identities, devices, terminals, offsets and settings all live in the database
    // file under the data directory, not in the container's writable layer.
    let db_path = server.data_dir.join("relay.db");
    assert!(
        db_path.exists(),
        "the database must live in the data directory"
    );

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    for (table, expected_minimum) in [
        ("identities", 1),
        ("devices", 1),
        ("terminals", 1),
        ("terminal_output", 1),
        ("settings", 10),
    ] {
        let count: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(
            count >= expected_minimum,
            "{table} should hold durable state, found {count}"
        );
    }

    let durable: i64 = conn
        .query_row(
            "SELECT durable_offset FROM terminals WHERE terminal_id = ?1",
            rusqlite::params![terminal_id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(durable, 9);

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_notifies_connected_peers_before_closing() {
    let server = TestServer::start().await;
    let api = Api::new(&server);
    let alice = api.provision().await;

    let mut publisher = Publisher::connect(&server, alice.device_id, &alice.device_token)
        .await
        .unwrap();
    let terminal_id = publisher.open_terminal_id("pty0").await;
    let mut mirror = Mirror::connect(&server, terminal_id, &alice.identity_token)
        .await
        .unwrap();
    mirror.subscribe(Some(0)).await;

    let state = std::sync::Arc::clone(&server.state);
    let shutdown = tokio::spawn(async move {
        terminal_relay::server::shutdown(&state).await;
    });

    // Both protocols receive a notice, then a shutdown close code (spec §10).
    let notice = publisher
        .expect_message("notice", Duration::from_secs(15))
        .await;
    assert_eq!(notice["code"], "server_shutdown");
    assert_eq!(
        publisher.expect_close(Duration::from_secs(15)).await,
        Some(4007)
    );

    let stream = mirror.drain(Duration::from_secs(15)).await;
    assert_eq!(stream.close_code, Some(4007));
    assert!(
        stream
            .control
            .iter()
            .any(|m| m["code"] == "server_shutdown")
    );

    shutdown.await.unwrap();
}
