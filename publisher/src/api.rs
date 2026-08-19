//! The relay's HTTP API: challenges, registration and tokens (relay spec §5.1, §5.2).

use serde::Deserialize;
use serde_json::json;

use crate::crypto::{
    ALGORITHM_ED25519, CHALLENGE_SIGNATURE_CONTEXT, KeyPair, b64_decode, parse_length_prefixed,
};

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("{0}")]
    Transport(String),
    /// The relay answered, and said no. The message is the relay's own.
    #[error("{status}: {code}: {message}")]
    Relay {
        status: u16,
        code: String,
        message: String,
    },
    #[error("the relay's response was not what this client expects: {0}")]
    Malformed(String),
}

type Result<T> = std::result::Result<T, ApiError>;

pub struct Client {
    http: reqwest::Client,
    base_url: String,
}

#[derive(Debug, Deserialize)]
pub struct Challenge {
    pub challenge_id: String,
    /// The exact bytes to sign, base64url. The relay recomputes and verifies its own
    /// derivation, so this is a convenience rather than a trust anchor — which is why
    /// `verify_binding` looks at it before it is signed.
    pub signing_input: String,
    pub key_fingerprint: String,
}

#[derive(Debug, Deserialize)]
pub struct Token {
    pub access_token: String,
    pub expires_in: i64,
    #[serde(default)]
    pub scopes: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct Identity {
    pub identity_id: String,
}

#[derive(Debug, Deserialize)]
pub struct Terminal {
    pub terminal_id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub cols: Option<u32>,
    #[serde(default)]
    pub rows: Option<u32>,
    #[serde(default)]
    pub accepts_input: bool,
}

#[derive(Debug, Deserialize)]
struct TerminalList {
    #[serde(default)]
    terminals: Vec<Terminal>,
}

#[derive(Debug, Deserialize)]
pub struct Device {
    pub device_id: String,
    #[serde(default)]
    pub role: String,
}

impl Client {
    pub fn new(base_url: &str) -> Result<Self> {
        crate::tls::ensure_provider();
        let http = reqwest::Client::builder()
            .user_agent(concat!("hypeterm-publish/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    async fn post<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        body: serde_json::Value,
        bearer: Option<&str>,
    ) -> Result<T> {
        let mut request = self
            .http
            .post(format!("{}{path}", self.base_url))
            .json(&body);
        if let Some(token) = bearer {
            request = request.bearer_auth(token);
        }
        let response = request
            .send()
            .await
            .map_err(|e| ApiError::Transport(e.to_string()))?;

        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| ApiError::Transport(e.to_string()))?;

        if !status.is_success() {
            // The relay's error bodies are `{"error": {"code", "message"}}`; fall back
            // to the raw body so an unexpected shape is still legible.
            let (code, message) = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|value| {
                    let error = value.get("error")?;
                    Some((
                        error.get("code")?.as_str()?.to_string(),
                        error.get("message")?.as_str()?.to_string(),
                    ))
                })
                .unwrap_or_else(|| ("unknown".to_string(), text.clone()));
            return Err(ApiError::Relay {
                status: status.as_u16(),
                code,
                message,
            });
        }
        serde_json::from_str(&text).map_err(|e| ApiError::Malformed(e.to_string()))
    }

    /// Ask for a proof-of-possession challenge (spec §5.1).
    pub async fn challenge(
        &self,
        operation: &str,
        public_key_base64: &str,
        owner_identity_id: Option<&str>,
    ) -> Result<Challenge> {
        let mut body = json!({
            "operation": operation,
            "key": { "algorithm": ALGORITHM_ED25519, "public_key": public_key_base64 },
        });
        if let Some(owner) = owner_identity_id {
            body["owner_identity_id"] = json!(owner);
        }
        self.post("/v1/auth/challenges", body, None).await
    }

    pub async fn register_identity(&self, key: &KeyPair) -> Result<Identity> {
        let challenge = self
            .challenge("register_identity", &key.public_key_base64(), None)
            .await?;
        let signature = sign_challenge(&challenge, key, "register_identity", None)?;
        self.post(
            "/v1/identities",
            json!({ "challenge_id": challenge.challenge_id, "signature": signature }),
            None,
        )
        .await
    }

    pub async fn identity_token(&self, key: &KeyPair) -> Result<Token> {
        let challenge = self
            .challenge("authenticate_identity", &key.public_key_base64(), None)
            .await?;
        let signature = sign_challenge(&challenge, key, "authenticate_identity", None)?;
        self.post(
            "/v1/auth/tokens",
            json!({ "challenge_id": challenge.challenge_id, "signature": signature }),
            None,
        )
        .await
    }

    pub async fn device_token(&self, key: &KeyPair) -> Result<Token> {
        let challenge = self
            .challenge("authenticate_device", &key.public_key_base64(), None)
            .await?;
        let signature = sign_challenge(&challenge, key, "authenticate_device", None)?;
        self.post(
            "/v1/auth/tokens",
            json!({ "challenge_id": challenge.challenge_id, "signature": signature }),
            None,
        )
        .await
    }

    /// The identity's terminals, which is what a subscriber would see (spec §5.3).
    pub async fn terminals(&self, identity_token: &str) -> Result<Vec<Terminal>> {
        self.terminals_filtered(identity_token, None).await
    }

    /// The same, narrowed to one publishing device.
    pub async fn terminals_filtered(
        &self,
        identity_token: &str,
        device_id: Option<&str>,
    ) -> Result<Vec<Terminal>> {
        let mut url = format!("{}/v1/terminals?state=open&limit=100", self.base_url);
        if let Some(device) = device_id {
            url.push_str("&device_id=");
            url.push_str(device);
        }
        let response = self
            .http
            .get(url)
            .bearer_auth(identity_token)
            .send()
            .await
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        if !status.is_success() {
            return Err(ApiError::Relay {
                status: status.as_u16(),
                code: "request_failed".into(),
                message: text,
            });
        }
        let list: TerminalList =
            serde_json::from_str(&text).map_err(|e| ApiError::Malformed(e.to_string()))?;
        Ok(list.terminals)
    }

    /// Register a device under an identity (spec §5.2).
    ///
    /// Two parties act: the challenge is bound to the owning identity *and* to the
    /// proposed device key, the signature is made by the device key, and the request
    /// itself is authorised by an identity token. That is what makes registering
    /// someone else's key impossible.
    pub async fn register_device(
        &self,
        identity_token: &str,
        identity_id: &str,
        device_public_key_base64: &str,
        device_signer: Option<&KeyPair>,
        name: &str,
        role: &str,
    ) -> Result<Device> {
        let challenge = self
            .challenge(
                "register_device",
                device_public_key_base64,
                Some(identity_id),
            )
            .await?;

        // The device signature must come from the device's own key. When the device is
        // elsewhere — a phone being paired — the caller supplies the signature it
        // produced instead.
        let signature = match device_signer {
            Some(key) => sign_challenge(&challenge, key, "register_device", Some(identity_id))?,
            None => {
                return Err(ApiError::Malformed(
                    "registering a device needs its private key to sign the challenge".into(),
                ));
            }
        };

        self.post(
            "/v1/devices",
            json!({
                "name": name,
                "key": { "algorithm": ALGORITHM_ED25519, "public_key": device_public_key_base64 },
                "challenge_id": challenge.challenge_id,
                "device_signature": signature,
                "role": role,
            }),
            Some(identity_token),
        )
        .await
    }
}

/// Check the relay's `signing_input` says what we think we are signing, then sign it.
///
/// The relay verifies its own derivation, so a mismatch here cannot let a forged
/// signature through. It can, however, stop this machine from putting its signature
/// on a statement it did not intend to make — for instance a `register_device`
/// binding to an identity that is not ours.
pub fn sign_challenge(
    challenge: &Challenge,
    key: &KeyPair,
    expected_operation: &str,
    expected_owner_identity: Option<&str>,
) -> Result<String> {
    let bytes = b64_decode(&challenge.signing_input)
        .ok_or_else(|| ApiError::Malformed("signing_input is not base64url".into()))?;
    verify_binding(&bytes, key, expected_operation, expected_owner_identity)?;
    Ok(key.sign(&bytes))
}

fn verify_binding(
    signing_input: &[u8],
    key: &KeyPair,
    expected_operation: &str,
    expected_owner_identity: Option<&str>,
) -> Result<()> {
    // Field order is fixed by spec §4.2: context, origin, challenge id, challenge,
    // operation, key fingerprint, owner identity, device key fingerprint, expiry.
    let fields = parse_length_prefixed(signing_input)
        .ok_or_else(|| ApiError::Malformed("signing_input is not length-prefixed".into()))?;
    if fields.len() != 9 {
        return Err(ApiError::Malformed(format!(
            "signing_input has {} fields, expected 9",
            fields.len()
        )));
    }
    let field = |index: usize| String::from_utf8_lossy(&fields[index]).to_string();

    if field(0) != CHALLENGE_SIGNATURE_CONTEXT {
        return Err(ApiError::Malformed(format!(
            "signing_input is for {:?}, not this protocol",
            field(0)
        )));
    }
    if field(4) != expected_operation {
        return Err(ApiError::Malformed(format!(
            "the relay offered a {:?} challenge when {expected_operation:?} was asked for",
            field(4)
        )));
    }
    if field(5) != key.fingerprint() {
        return Err(ApiError::Malformed(
            "the challenge is bound to a different key than the one signing it".into(),
        ));
    }
    if let Some(expected) = expected_owner_identity {
        if field(6) != expected {
            return Err(ApiError::Malformed(
                "the challenge is bound to a different owning identity".into(),
            ));
        }
    }
    Ok(())
}
