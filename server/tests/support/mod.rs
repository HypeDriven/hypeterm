//! Test harness: a real server on an ephemeral port with its own database, plus
//! client helpers for the full proof-of-possession, HTTP and WebSocket flows.

#![allow(dead_code)]

use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use terminal_relay::app::AppState;
use terminal_relay::bootstrap::Bootstrap;
use terminal_relay::server;
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite};
use tungstenite::client::IntoClientRequest;
use uuid::Uuid;

pub const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::URL_SAFE_NO_PAD;

pub const OPERATOR_TOKEN: &str = "test-operator-token";

pub fn b64(bytes: &[u8]) -> String {
    B64.encode(bytes)
}

/// Enable server logging when `RELAY_TEST_LOG` is set, for diagnosing failures with
/// `cargo test -- --nocapture`.
fn init_test_logging() {
    static ONCE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    ONCE.get_or_init(|| {
        if let Ok(level) = std::env::var("RELAY_TEST_LOG") {
            terminal_relay::observability::init(&level, false);
        }
    });
}

pub fn unb64(text: &str) -> Vec<u8> {
    B64.decode(text).expect("valid base64url")
}

/// A running relay instance backed by a temporary directory.
pub struct TestServer {
    pub addr: SocketAddr,
    pub state: Arc<AppState>,
    pub data_dir: PathBuf,
    runtime: Option<tokio::runtime::Runtime>,
    _temp: Option<tempfile::TempDir>,
}

impl TestServer {
    pub async fn start() -> Self {
        let temp = tempfile::tempdir().expect("temp dir");
        let dir = temp.path().to_path_buf();
        let mut server = Self::start_in(dir, None).await;
        server._temp = Some(temp);
        server
    }

    /// Start against an existing data directory, which is how restart behaviour is
    /// exercised without touching the previous instance's in-memory state.
    pub async fn start_in(data_dir: PathBuf, settings: Option<Vec<(&str, Value)>>) -> Self {
        init_test_logging();
        std::fs::create_dir_all(&data_dir).expect("data dir");

        // Bootstrap values are constructed directly rather than through the
        // environment, so tests stay independent of process-global state.
        let bootstrap = Bootstrap {
            data_dir: data_dir.clone(),
            db_path: data_dir.join("relay.db"),
            secret_key: [7u8; 32],
            operator_token_seed: Some(OPERATOR_TOKEN.to_string()),
            instance_id: "test".to_string(),
            recovery_mode: false,
            recovery_listen: "127.0.0.1:0".to_string(),
        };

        let state = AppState::new(bootstrap).expect("state");

        // Bind an ephemeral loopback port so tests can run concurrently.
        let mut updates: BTreeMap<String, Value> = BTreeMap::new();
        updates.insert("server.listen_address".into(), json!("127.0.0.1:0"));
        // Keep the stored logging settings aligned with RELAY_TEST_LOG, otherwise the
        // first settings revision resets the level back to the stored default.
        if let Ok(level) = std::env::var("RELAY_TEST_LOG") {
            updates.insert("logging.level".into(), json!(level));
            updates.insert("logging.format".into(), json!("text"));
        }
        for (key, value) in settings.unwrap_or_default() {
            updates.insert(key.to_string(), value);
        }
        let revision = state.snapshot().revision;
        state
            .settings
            .patch("test", revision, updates)
            .expect("apply test settings");

        // The server gets its own multi-threaded runtime so it can be killed
        // abruptly to simulate a crash.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("server runtime");

        let addr = {
            let state = Arc::clone(&state);
            runtime
                .spawn(async move { server::spawn_for_test(state).await })
                .await
                .expect("spawn task")
                .expect("bind")
        };

        Self {
            addr,
            state,
            data_dir,
            runtime: Some(runtime),
            _temp: None,
        }
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn ws_url(&self, path: &str) -> String {
        format!("ws://{}{path}", self.addr)
    }

    /// Kill the instance without flushing, simulating a process crash.
    pub fn crash(mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }

    /// Shut down gracefully, which must drain dirty output first.
    pub async fn shutdown(mut self) {
        server::shutdown(&self.state).await;
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }

    /// Shut down but keep the temporary directory alive, so a test can inspect or
    /// tamper with the durable state afterwards.
    pub async fn shutdown_keeping_data(mut self) -> (PathBuf, Option<tempfile::TempDir>) {
        let temp = self._temp.take();
        let data_dir = self.data_dir.clone();
        self.shutdown().await;
        (data_dir, temp)
    }

    /// Stop gracefully and start a fresh instance on the same durable state.
    ///
    /// The temporary directory is handed to the new instance, so the database
    /// genuinely survives; letting it drop here would delete the very state under test.
    pub async fn restart(mut self, settings: Option<Vec<(&str, Value)>>) -> Self {
        let temp = self._temp.take();
        let data_dir = self.data_dir.clone();
        self.shutdown().await;
        let mut restarted = Self::start_in(data_dir, settings).await;
        restarted._temp = temp;
        restarted
    }

    /// Kill abruptly and start a fresh instance on the same durable state, so
    /// recovery from an uncommitted suffix can be observed.
    pub async fn restart_after_crash(mut self, settings: Option<Vec<(&str, Value)>>) -> Self {
        let temp = self._temp.take();
        let data_dir = self.data_dir.clone();
        self.crash();
        let mut restarted = Self::start_in(data_dir, settings).await;
        restarted._temp = temp;
        restarted
    }

    /// The revision committed in the database, which can be ahead of this
    /// instance's applied snapshot when another instance shares the same state.
    pub fn committed_revision(&self) -> i64 {
        let conn = rusqlite::Connection::open(self.data_dir.join("relay.db")).expect("open db");
        conn.query_row(
            "SELECT revision FROM settings_state WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .expect("read revision")
    }

    pub fn patch_settings(&self, updates: Vec<(&str, Value)>) -> Result<i64, String> {
        let mut map: BTreeMap<String, Value> = BTreeMap::new();
        for (key, value) in updates {
            map.insert(key.to_string(), value);
        }
        // Read the committed revision, as a real operator client would, so a
        // concurrent instance's update does not make this a stale write.
        let revision = self.committed_revision();
        self.state
            .settings
            .patch("test", revision, map)
            .map(|outcome| outcome.snapshot.revision)
            .map_err(|e| e.message)
    }

    pub fn snapshot_revision(&self) -> i64 {
        self.state.snapshot().revision
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

/// An Ed25519 key pair standing in for a client's root or device credential.
pub struct Key {
    pub signing: SigningKey,
}

impl Key {
    pub fn new(seed: u8) -> Self {
        let mut bytes = [0u8; 32];
        bytes[0] = seed;
        bytes[31] = seed.wrapping_add(1);
        Self {
            signing: SigningKey::from_bytes(&bytes),
        }
    }

    pub fn random() -> Self {
        let mut bytes = [0u8; 32];
        rand::fill(&mut bytes);
        Self {
            signing: SigningKey::from_bytes(&bytes),
        }
    }

    pub fn public_b64(&self) -> String {
        b64(self.signing.verifying_key().as_bytes())
    }

    pub fn sign_b64(&self, message: &[u8]) -> String {
        b64(&self.signing.sign(message).to_bytes())
    }

    pub fn key_json(&self) -> Value {
        json!({ "algorithm": "ed25519", "public_key": self.public_b64() })
    }
}

pub struct Api {
    pub base: String,
    pub http: reqwest::Client,
}

pub struct Response {
    pub status: u16,
    pub body: Value,
    pub headers: reqwest::header::HeaderMap,
}

impl Response {
    pub fn error_code(&self) -> Option<&str> {
        self.body.get("error")?.get("code")?.as_str()
    }

    pub fn request_id(&self) -> Option<&str> {
        self.body.get("error")?.get("request_id")?.as_str()
    }
}

impl Api {
    pub fn new(server: &TestServer) -> Self {
        Self {
            base: server.base_url(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .build()
                .expect("http client"),
        }
    }

    pub async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        token: Option<&str>,
        body: Option<&Value>,
        extra_headers: &[(&str, &str)],
    ) -> Response {
        let mut request = self.http.request(method, format!("{}{path}", self.base));
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        for (name, value) in extra_headers {
            request = request.header(*name, *value);
        }
        let response = request.send().await.expect("request");
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let text = response.text().await.unwrap_or_default();
        let body = serde_json::from_str(&text).unwrap_or(Value::String(text));
        Response {
            status,
            body,
            headers,
        }
    }

    /// Ask a device to open a terminal. `Idempotency-Key` is required by the endpoint,
    /// so it is not optional here either.
    pub async fn create_terminal(
        &self,
        device_id: Uuid,
        token: &str,
        body: &Value,
        key: &str,
    ) -> Response {
        self.request(
            reqwest::Method::POST,
            &format!("/v1/devices/{device_id}/terminals"),
            Some(token),
            Some(body),
            &[("idempotency-key", key)],
        )
        .await
    }

    pub async fn post(&self, path: &str, token: Option<&str>, body: &Value) -> Response {
        self.request(reqwest::Method::POST, path, token, Some(body), &[])
            .await
    }

    pub async fn get(&self, path: &str, token: Option<&str>) -> Response {
        self.request(reqwest::Method::GET, path, token, None, &[])
            .await
    }

    pub async fn delete(&self, path: &str, token: Option<&str>) -> Response {
        self.request(reqwest::Method::DELETE, path, token, None, &[])
            .await
    }

    pub async fn patch(&self, path: &str, token: Option<&str>, body: &Value) -> Response {
        self.request(reqwest::Method::PATCH, path, token, Some(body), &[])
            .await
    }

    // ------------------------------------------------------------ auth helpers

    /// Request a challenge and return `(challenge_id, signing_input_bytes)`.
    pub async fn challenge(
        &self,
        operation: &str,
        key: &Key,
        owner_identity_id: Option<&str>,
    ) -> (String, Vec<u8>) {
        let mut body = json!({ "operation": operation, "key": key.key_json() });
        if let Some(owner) = owner_identity_id {
            body["owner_identity_id"] = json!(owner);
        }
        let response = self.post("/v1/auth/challenges", None, &body).await;
        assert_eq!(
            response.status, 201,
            "challenge failed: {:?}",
            response.body
        );
        let challenge_id = response.body["challenge_id"]
            .as_str()
            .expect("challenge_id")
            .to_string();
        let signing_input = unb64(
            response.body["signing_input"]
                .as_str()
                .expect("signing_input"),
        );
        (challenge_id, signing_input)
    }

    pub async fn register_identity(&self, key: &Key) -> Response {
        let (challenge_id, signing_input) = self.challenge("register_identity", key, None).await;
        self.post(
            "/v1/identities",
            None,
            &json!({
                "challenge_id": challenge_id,
                "signature": key.sign_b64(&signing_input),
            }),
        )
        .await
    }

    pub async fn identity_id(&self, key: &Key) -> String {
        let response = self.register_identity(key).await;
        assert!(
            response.status == 201 || response.status == 200,
            "identity registration failed: {:?}",
            response.body
        );
        response.body["identity_id"]
            .as_str()
            .expect("identity_id")
            .to_string()
    }

    pub async fn identity_token(&self, key: &Key) -> String {
        let (challenge_id, signing_input) =
            self.challenge("authenticate_identity", key, None).await;
        let response = self
            .post(
                "/v1/auth/tokens",
                None,
                &json!({
                    "challenge_id": challenge_id,
                    "signature": key.sign_b64(&signing_input),
                }),
            )
            .await;
        assert_eq!(
            response.status, 200,
            "identity token failed: {:?}",
            response.body
        );
        response.body["access_token"]
            .as_str()
            .expect("access_token")
            .to_string()
    }

    pub async fn register_device(
        &self,
        identity_token: &str,
        identity_id: &str,
        device_key: &Key,
        name: &str,
    ) -> Response {
        self.register_device_with_role(identity_token, identity_id, device_key, name, None)
            .await
    }

    pub async fn register_device_with_role(
        &self,
        identity_token: &str,
        identity_id: &str,
        device_key: &Key,
        name: &str,
        role: Option<&str>,
    ) -> Response {
        let (challenge_id, signing_input) = self
            .challenge("register_device", device_key, Some(identity_id))
            .await;
        let mut body = json!({
            "name": name,
            "key": device_key.key_json(),
            "challenge_id": challenge_id,
            "device_signature": device_key.sign_b64(&signing_input),
        });
        if let Some(role) = role {
            body["role"] = json!(role);
        }
        self.post("/v1/devices", Some(identity_token), &body).await
    }

    /// Register a client-role device: the credential a mobile client holds instead of
    /// the identity's root key.
    pub async fn client_device(
        &self,
        identity_token: &str,
        identity_id: &str,
    ) -> (Key, Uuid, String) {
        let key = Key::random();
        let response = self
            .register_device_with_role(
                identity_token,
                identity_id,
                &key,
                "mobile client",
                Some("client"),
            )
            .await;
        assert_eq!(
            response.status, 201,
            "client registration failed: {:?}",
            response.body
        );
        let device_id =
            Uuid::parse_str(response.body["device_id"].as_str().expect("device_id")).expect("uuid");
        let token = self.device_token(&key).await;
        (key, device_id, token)
    }

    pub async fn device_id(
        &self,
        identity_token: &str,
        identity_id: &str,
        device_key: &Key,
        name: &str,
    ) -> Uuid {
        let response = self
            .register_device(identity_token, identity_id, device_key, name)
            .await;
        assert_eq!(
            response.status, 201,
            "device registration failed: {:?}",
            response.body
        );
        Uuid::parse_str(response.body["device_id"].as_str().expect("device_id")).expect("uuid")
    }

    pub async fn device_token(&self, device_key: &Key) -> String {
        let (challenge_id, signing_input) = self
            .challenge("authenticate_device", device_key, None)
            .await;
        let response = self
            .post(
                "/v1/auth/tokens",
                None,
                &json!({
                    "challenge_id": challenge_id,
                    "signature": device_key.sign_b64(&signing_input),
                }),
            )
            .await;
        assert_eq!(
            response.status, 200,
            "device token failed: {:?}",
            response.body
        );
        response.body["access_token"]
            .as_str()
            .expect("access_token")
            .to_string()
    }

    /// Complete identity plus device setup in one call.
    pub async fn provision(&self) -> Provisioned {
        let identity_key = Key::random();
        let identity_id = self.identity_id(&identity_key).await;
        let identity_token = self.identity_token(&identity_key).await;
        let device_key = Key::random();
        let device_id = self
            .device_id(&identity_token, &identity_id, &device_key, "test device")
            .await;
        let device_token = self.device_token(&device_key).await;
        Provisioned {
            identity_key,
            identity_id,
            identity_token,
            device_key,
            device_id,
            device_token,
        }
    }
}

pub struct Provisioned {
    pub identity_key: Key,
    pub identity_id: String,
    pub identity_token: String,
    pub device_key: Key,
    pub device_id: Uuid,
    pub device_token: String,
}

// ------------------------------------------------------------------ websockets

pub type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn connect(
    url: &str,
    subprotocol: &str,
    headers: &[(&str, String)],
) -> Result<Socket, tungstenite::Error> {
    let mut request = url.into_client_request()?;
    request.headers_mut().insert(
        "sec-websocket-protocol",
        subprotocol.parse().expect("header"),
    );
    for (name, value) in headers {
        let name: tungstenite::http::HeaderName = name.parse().expect("header name");
        request
            .headers_mut()
            .insert(name, value.parse().expect("header value"));
    }
    let (socket, _response) = tokio_tungstenite::connect_async(request).await?;
    Ok(socket)
}

pub struct Publisher {
    pub socket: Socket,
}

impl Publisher {
    pub async fn connect(
        server: &TestServer,
        device_id: Uuid,
        token: &str,
    ) -> Result<Self, tungstenite::Error> {
        let url = server.ws_url(&format!("/v1/devices/{device_id}/relay"));
        let socket = connect(
            &url,
            "terminal-relay.publisher.v1",
            &[("authorization", format!("Bearer {token}"))],
        )
        .await?;
        let mut publisher = Self { socket };
        let ready = publisher.next_json().await.expect("ready");
        assert_eq!(ready["type"], "ready", "expected ready, got {ready:?}");
        Ok(publisher)
    }

    /// Connect on subprotocol version 2, which can receive terminal input.
    pub async fn connect_v2(
        server: &TestServer,
        device_id: Uuid,
        token: &str,
    ) -> Result<Self, tungstenite::Error> {
        let url = server.ws_url(&format!("/v1/devices/{device_id}/relay"));
        let socket = connect(
            &url,
            "terminal-relay.publisher.v2",
            &[("authorization", format!("Bearer {token}"))],
        )
        .await?;
        let mut publisher = Self { socket };
        let ready = publisher.next_json().await.expect("ready");
        assert_eq!(ready["type"], "ready", "expected ready, got {ready:?}");
        assert_eq!(ready["protocol"], "terminal-relay.publisher.v2");
        Ok(publisher)
    }

    pub async fn connect_with_ticket(
        server: &TestServer,
        device_id: Uuid,
        ticket: &str,
    ) -> Result<Self, tungstenite::Error> {
        let url = server.ws_url(&format!("/v1/devices/{device_id}/relay"));
        let socket = connect(
            &url,
            "terminal-relay.publisher.v1",
            &[("x-relay-ticket", ticket.to_string())],
        )
        .await?;
        let mut publisher = Self { socket };
        let ready = publisher.next_json().await.expect("ready");
        assert_eq!(ready["type"], "ready");
        Ok(publisher)
    }

    pub async fn send_json(&mut self, value: &Value) {
        self.socket
            .send(tungstenite::Message::Text(value.to_string().into()))
            .await
            .expect("send json");
    }

    /// Assert that this machine allows subscribers to ask it to open a terminal.
    pub async fn send_capabilities(&mut self, terminal_open_requests: bool) {
        self.send_json(&serde_json::json!({
            "type": "publisher.capabilities",
            "terminal_open_requests": terminal_open_requests,
        }))
        .await;
    }

    /// Wait for the relay to forward a terminal-open request and return its id.
    pub async fn expect_open_request(&mut self, timeout: Duration) -> Value {
        self.expect_message("terminal.open_request", timeout).await
    }

    /// Answer a forwarded request by opening a terminal that echoes it.
    pub async fn answer_open_request(&mut self, request_id: &str, local_ref: &str) -> Uuid {
        self.send_json(&serde_json::json!({
            "type": "terminal.open",
            "request_id": local_ref,
            "local_ref": local_ref,
            "label": "phone",
            "cols": 80,
            "rows": 24,
            "accepts_input": true,
            "in_reply_to": request_id,
        }))
        .await;
        let opened = self
            .expect_message("terminal.opened", Duration::from_secs(5))
            .await;
        Uuid::parse_str(opened["terminal_id"].as_str().expect("terminal_id")).expect("uuid")
    }

    pub async fn decline_open_request(&mut self, request_id: &str, reason: &str, detail: &str) {
        self.send_json(&serde_json::json!({
            "type": "terminal.open_declined",
            "in_reply_to": request_id,
            "reason": reason,
            "detail": detail,
        }))
        .await;
    }

    /// Send an output frame with the publisher's 25-byte header.
    pub async fn send_output(&mut self, terminal_id: Uuid, offset: u64, payload: &[u8]) {
        let mut frame = Vec::with_capacity(25 + payload.len());
        frame.push(0x01);
        frame.extend_from_slice(terminal_id.as_bytes());
        frame.extend_from_slice(&offset.to_be_bytes());
        frame.extend_from_slice(payload);
        self.socket
            .send(tungstenite::Message::Binary(frame.into()))
            .await
            .expect("send output");
    }

    pub async fn send_raw(&mut self, bytes: Vec<u8>) {
        self.socket
            .send(tungstenite::Message::Binary(bytes.into()))
            .await
            .expect("send raw");
    }

    /// Next JSON control message, skipping transport pings.
    pub async fn next_json(&mut self) -> Option<Value> {
        next_json(&mut self.socket).await
    }

    pub async fn open_terminal(&mut self, local_ref: &str) -> Value {
        self.send_json(&json!({
            "type": "terminal.open",
            "request_id": format!("req-{local_ref}"),
            "local_ref": local_ref,
            "label": "test terminal",
            "cols": 80,
            "rows": 24,
            "term": "xterm-256color",
        }))
        .await;
        loop {
            let message = self.next_json().await.expect("terminal.opened");
            if message["type"] == "terminal.opened" {
                return message;
            }
            if message["type"] == "error" {
                panic!("terminal.open failed: {message:?}");
            }
        }
    }

    pub async fn open_terminal_id(&mut self, local_ref: &str) -> Uuid {
        let opened = self.open_terminal(local_ref).await;
        Uuid::parse_str(opened["terminal_id"].as_str().expect("terminal_id")).expect("uuid")
    }

    /// Open a terminal that opts in to receiving input (spec §4.5).
    pub async fn open_terminal_accepting_input(&mut self, local_ref: &str) -> Value {
        self.send_json(&json!({
            "type": "terminal.open",
            "request_id": format!("req-{local_ref}"),
            "local_ref": local_ref,
            "label": "interactive terminal",
            "cols": 80,
            "rows": 24,
            "term": "xterm-256color",
            "accepts_input": true,
        }))
        .await;
        loop {
            let message = self.next_json().await.expect("terminal.opened");
            if message["type"] == "terminal.opened" || message["type"] == "error" {
                return message;
            }
        }
    }

    pub async fn open_input_terminal_id(&mut self, local_ref: &str) -> Uuid {
        let opened = self.open_terminal_accepting_input(local_ref).await;
        assert_eq!(opened["type"], "terminal.opened", "open failed: {opened:?}");
        assert_eq!(opened["accepts_input"], json!(true));
        Uuid::parse_str(opened["terminal_id"].as_str().expect("terminal_id")).expect("uuid")
    }

    /// Read the next input frame the relay delivers, decoding the 25-byte header.
    pub async fn next_input(&mut self, timeout: Duration) -> Option<(Uuid, u64, Vec<u8>)> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let next = tokio::time::timeout_at(deadline, self.socket.next())
                .await
                .ok()?;
            match next {
                Some(Ok(tungstenite::Message::Binary(frame))) => {
                    assert_eq!(frame[0], 0x02, "expected an input frame");
                    let mut uuid_bytes = [0u8; 16];
                    uuid_bytes.copy_from_slice(&frame[1..17]);
                    let mut sequence = [0u8; 8];
                    sequence.copy_from_slice(&frame[17..25]);
                    return Some((
                        Uuid::from_bytes(uuid_bytes),
                        u64::from_be_bytes(sequence),
                        frame[25..].to_vec(),
                    ));
                }
                Some(Ok(_)) => continue,
                Some(Err(_)) | None => return None,
            }
        }
    }

    pub async fn close_terminal(&mut self, terminal_id: Uuid, reason: &str) {
        self.send_json(&json!({
            "type": "terminal.close",
            "terminal_id": terminal_id,
            "reason": reason,
        }))
        .await;
    }

    /// Wait for a cumulative acknowledgement reaching at least `offset`.
    pub async fn wait_ack(&mut self, terminal_id: Uuid, offset: u64) -> Value {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let message = tokio::time::timeout_at(deadline, self.next_json())
                .await
                .expect("ack timeout")
                .expect("stream ended before ack");
            if message["type"] == "output.ack"
                && message["terminal_id"] == json!(terminal_id)
                && message["durable_offset"].as_u64().unwrap_or(0) >= offset
            {
                return message;
            }
        }
    }

    /// Collect control messages until one matches, with a bounded wait.
    pub async fn expect_message(&mut self, kind: &str, timeout: Duration) -> Value {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let message = tokio::time::timeout_at(deadline, self.next_json())
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for {kind}"))
                .unwrap_or_else(|| panic!("stream ended while waiting for {kind}"));
            if message["type"] == kind {
                return message;
            }
        }
    }

    /// Wait for the connection to close, returning the close code if present.
    pub async fn expect_close(&mut self, timeout: Duration) -> Option<u16> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let next = tokio::time::timeout_at(deadline, self.socket.next()).await;
            match next {
                Ok(Some(Ok(tungstenite::Message::Close(frame)))) => {
                    return frame.map(|f| f.code.into());
                }
                Ok(Some(Ok(_))) => continue,
                Ok(Some(Err(_))) | Ok(None) => return None,
                Err(_) => panic!("timed out waiting for close"),
            }
        }
    }
}

pub struct Mirror {
    pub socket: Socket,
}

/// Everything a subscriber observed, in arrival order.
#[derive(Debug, Default)]
pub struct MirrorStream {
    pub bytes: Vec<u8>,
    /// `(start_offset, length)` for each output frame, to verify ordering.
    pub frames: Vec<(u64, usize)>,
    pub durable_offsets: Vec<u64>,
    pub control: Vec<Value>,
    pub close_code: Option<u16>,
}

impl MirrorStream {
    pub fn control_of_type(&self, kind: &str) -> Option<&Value> {
        self.control.iter().find(|m| m["type"] == kind)
    }

    /// Offsets must be contiguous and strictly increasing across frames.
    pub fn assert_contiguous_from(&self, start: u64) {
        let mut expected = start;
        for (offset, length) in &self.frames {
            assert_eq!(
                *offset, expected,
                "frame arrived out of order: {:?}",
                self.frames
            );
            assert!(*length > 0, "zero-length output frame was delivered");
            expected += *length as u64;
        }
    }
}

impl Mirror {
    pub async fn connect(
        server: &TestServer,
        terminal_id: Uuid,
        token: &str,
    ) -> Result<Self, tungstenite::Error> {
        let url = server.ws_url(&format!("/v1/terminals/{terminal_id}/mirror"));
        let socket = connect(
            &url,
            "terminal-relay.mirror.v1",
            &[("authorization", format!("Bearer {token}"))],
        )
        .await?;
        let mut mirror = Self { socket };
        let ready = mirror.next_json().await.expect("ready");
        assert_eq!(ready["type"], "ready", "expected ready, got {ready:?}");
        Ok(mirror)
    }

    /// Connect on subprotocol version 2, which may send terminal input.
    pub async fn connect_v2(
        server: &TestServer,
        terminal_id: Uuid,
        token: &str,
    ) -> Result<Self, tungstenite::Error> {
        let url = server.ws_url(&format!("/v1/terminals/{terminal_id}/mirror"));
        let socket = connect(
            &url,
            "terminal-relay.mirror.v2",
            &[("authorization", format!("Bearer {token}"))],
        )
        .await?;
        let mut mirror = Self { socket };
        let ready = mirror.next_json().await.expect("ready");
        assert_eq!(ready["type"], "ready", "expected ready, got {ready:?}");
        assert_eq!(ready["protocol"], "terminal-relay.mirror.v2");
        Ok(mirror)
    }

    /// Send an input frame with the 9-byte mirror header.
    pub async fn send_input(&mut self, client_sequence: u64, payload: &[u8]) {
        let mut frame = Vec::with_capacity(9 + payload.len());
        frame.push(0x02);
        frame.extend_from_slice(&client_sequence.to_be_bytes());
        frame.extend_from_slice(payload);
        self.socket
            .send(tungstenite::Message::Binary(frame.into()))
            .await
            .expect("send input");
    }

    pub async fn send_json(&mut self, value: &Value) {
        self.socket
            .send(tungstenite::Message::Text(value.to_string().into()))
            .await
            .expect("send json");
    }

    /// Wait for a control message of a given type.
    pub async fn expect_message(&mut self, kind: &str, timeout: Duration) -> Value {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let message = tokio::time::timeout_at(deadline, self.next_json())
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for {kind}"))
                .unwrap_or_else(|| panic!("stream ended while waiting for {kind}"));
            if message["type"] == kind {
                return message;
            }
        }
    }

    pub async fn subscribe(&mut self, from_offset: Option<u64>) -> Value {
        let mut message = json!({ "type": "subscribe" });
        if let Some(offset) = from_offset {
            message["from_offset"] = json!(offset);
        }
        self.socket
            .send(tungstenite::Message::Text(message.to_string().into()))
            .await
            .expect("subscribe");
        loop {
            let reply = self.next_json().await.expect("subscribed");
            if reply["type"] == "subscribed" || reply["type"] == "error" {
                return reply;
            }
        }
    }

    pub async fn next_json(&mut self) -> Option<Value> {
        next_json(&mut self.socket).await
    }

    /// Read until `until` bytes of payload have arrived, or the socket closes.
    pub async fn collect(&mut self, until_bytes: usize, timeout: Duration) -> MirrorStream {
        let mut stream = MirrorStream::default();
        let deadline = tokio::time::Instant::now() + timeout;

        while stream.bytes.len() < until_bytes {
            let next = match tokio::time::timeout_at(deadline, self.socket.next()).await {
                Ok(next) => next,
                Err(_) => break,
            };
            match next {
                Some(Ok(tungstenite::Message::Binary(frame))) => {
                    assert_eq!(frame[0], 0x01, "unexpected mirror frame type");
                    let mut offset_bytes = [0u8; 8];
                    offset_bytes.copy_from_slice(&frame[1..9]);
                    let offset = u64::from_be_bytes(offset_bytes);
                    let payload = &frame[9..];
                    stream.frames.push((offset, payload.len()));
                    stream.bytes.extend_from_slice(payload);
                }
                Some(Ok(tungstenite::Message::Text(text))) => {
                    let value: Value = serde_json::from_str(&text).expect("json control message");
                    if value["type"] == "durable" {
                        stream
                            .durable_offsets
                            .push(value["durable_offset"].as_u64().unwrap_or_default());
                    }
                    stream.control.push(value);
                }
                Some(Ok(tungstenite::Message::Close(frame))) => {
                    stream.close_code = frame.map(|f| f.code.into());
                    break;
                }
                Some(Ok(_)) => continue,
                Some(Err(_)) | None => break,
            }
        }
        stream
    }

    /// Drain whatever is already available, then stop.
    pub async fn drain(&mut self, quiet_for: Duration) -> MirrorStream {
        let mut stream = MirrorStream::default();
        loop {
            match tokio::time::timeout(quiet_for, self.socket.next()).await {
                Err(_) => break,
                Ok(Some(Ok(tungstenite::Message::Binary(frame)))) => {
                    let mut offset_bytes = [0u8; 8];
                    offset_bytes.copy_from_slice(&frame[1..9]);
                    stream
                        .frames
                        .push((u64::from_be_bytes(offset_bytes), frame.len() - 9));
                    stream.bytes.extend_from_slice(&frame[9..]);
                }
                Ok(Some(Ok(tungstenite::Message::Text(text)))) => {
                    let value: Value = serde_json::from_str(&text).expect("json");
                    if value["type"] == "durable" {
                        stream
                            .durable_offsets
                            .push(value["durable_offset"].as_u64().unwrap_or_default());
                    }
                    stream.control.push(value);
                }
                Ok(Some(Ok(tungstenite::Message::Close(frame)))) => {
                    stream.close_code = frame.map(|f| f.code.into());
                    break;
                }
                Ok(Some(Ok(_))) => continue,
                Ok(Some(Err(_))) | Ok(None) => break,
            }
        }
        stream
    }
}

async fn next_json(socket: &mut Socket) -> Option<Value> {
    loop {
        match socket.next().await? {
            Ok(tungstenite::Message::Text(text)) => {
                return serde_json::from_str(&text).ok();
            }
            Ok(tungstenite::Message::Ping(_)) | Ok(tungstenite::Message::Pong(_)) => continue,
            Ok(tungstenite::Message::Binary(_)) => continue,
            Ok(tungstenite::Message::Close(_)) => return None,
            Ok(_) => continue,
            Err(_) => return None,
        }
    }
}

/// Poll a condition until it holds, to avoid fixed sleeps in tests.
pub async fn eventually<F, Fut>(timeout: Duration, mut check: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if check().await {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
