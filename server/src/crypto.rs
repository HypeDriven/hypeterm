//! Key handling, proof-of-possession signing inputs, secret encryption and tokens.

use crate::error::{ApiError, ApiResult, code};
use crate::util::{LengthPrefixed, b64_decode, b64_encode, ct_eq, random_bytes, sha256};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

/// Domain separation tag for the identity fingerprint (spec §3.1). Changing this
/// string changes every identity ID, so it is frozen for protocol version 1.
pub const IDENTITY_FINGERPRINT_CONTEXT: &str = "terminal-relay-identity-v1";

/// Versioned context for the challenge signing input (spec §4.2). Returned to
/// callers as `signature_context`.
pub const CHALLENGE_SIGNATURE_CONTEXT: &str = "terminal-relay-challenge-v1";

pub const ALGORITHM_ED25519: &str = "ed25519";

/// Granted scopes (spec §4.3). Which set a principal may hold depends on its kind and,
/// for devices, its role — enforced by settings validation so no configuration can
/// hand a publisher identity-level authority or a client the ability to publish.
pub mod scope {
    pub const DEVICES_READ: &str = "devices:read";
    pub const DEVICES_WRITE: &str = "devices:write";
    pub const TERMINALS_READ: &str = "terminals:read";
    pub const TERMINALS_MIRROR: &str = "terminals:mirror";
    /// Authority to send terminal input, distinct from reading it (spec §4.5).
    pub const TERMINALS_INPUT: &str = "terminals:input";
    /// Authority to ask a publishing device to open a terminal (spec §4.6).
    ///
    /// Equal in gravity to `TERMINALS_INPUT`, not a lesser read-like capability: a
    /// credential holding both is shell-equivalent on the publishing machine. It is
    /// deliberately absent from every default token scope list, so no principal
    /// acquires it by a server upgrade.
    pub const TERMINALS_CREATE: &str = "terminals:create";
    pub const TERMINALS_WRITE: &str = "terminals:write";
    pub const TERMINALS_PUBLISH: &str = "terminals:publish";

    pub const ALL: &[&str] = &[
        DEVICES_READ,
        DEVICES_WRITE,
        TERMINALS_READ,
        TERMINALS_MIRROR,
        TERMINALS_INPUT,
        TERMINALS_CREATE,
        TERMINALS_WRITE,
        TERMINALS_PUBLISH,
    ];

    /// An identity manages its devices and reads or writes its own terminals.
    pub const IDENTITY_ALLOWED: &[&str] = &[
        DEVICES_READ,
        DEVICES_WRITE,
        TERMINALS_READ,
        TERMINALS_MIRROR,
        TERMINALS_INPUT,
        TERMINALS_CREATE,
    ];

    /// A publisher device may only manage and publish its own terminals.
    pub const PUBLISHER_ALLOWED: &[&str] = &[TERMINALS_WRITE, TERMINALS_PUBLISH];

    /// A client device may only read and write terminals owned by its identity, and —
    /// when an operator grants it — list that identity's devices and ask one of them to
    /// open a terminal (spec §4.6). Neither of the last two is granted by default.
    pub const CLIENT_ALLOWED: &[&str] = &[
        DEVICES_READ,
        TERMINALS_READ,
        TERMINALS_MIRROR,
        TERMINALS_INPUT,
        TERMINALS_CREATE,
    ];
}

/// What a device is permitted to do (spec §3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DeviceRole {
    /// Publishes terminal output. The version 1 behaviour, and the default.
    #[default]
    Publisher,
    /// Mirrors and writes to its owner's terminals; cannot publish.
    Client,
    /// Both of the above.
    Both,
}

impl DeviceRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            DeviceRole::Publisher => "publisher",
            DeviceRole::Client => "client",
            DeviceRole::Both => "both",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "publisher" => Some(DeviceRole::Publisher),
            "client" => Some(DeviceRole::Client),
            "both" => Some(DeviceRole::Both),
            _ => None,
        }
    }

    pub fn may_publish(&self) -> bool {
        matches!(self, DeviceRole::Publisher | DeviceRole::Both)
    }

    pub fn may_mirror(&self) -> bool {
        matches!(self, DeviceRole::Client | DeviceRole::Both)
    }
}

// ---------------------------------------------------------------------------- keys

/// A canonicalised, validated public key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicKey {
    pub algorithm: String,
    /// Canonical key bytes. For Ed25519 this is the 32-byte compressed point,
    /// accepted only when it round-trips through key validation.
    pub bytes: Vec<u8>,
}

impl PublicKey {
    /// Parse and canonicalise a key, rejecting anything that is not a valid point
    /// for its algorithm. Validation happens *before* fingerprinting so that no
    /// unusable key can ever occupy an identity ID.
    pub fn parse(algorithm: &str, encoded: &str, supported: &[String]) -> ApiResult<Self> {
        let algorithm = algorithm.trim().to_ascii_lowercase();
        if !supported.iter().any(|a| a.eq_ignore_ascii_case(&algorithm)) {
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                code::UNSUPPORTED_ALGORITHM,
                format!("unsupported key algorithm: {algorithm}"),
            ));
        }
        let bytes = b64_decode(encoded)
            .ok_or_else(|| ApiError::invalid("public_key must be base64url-encoded"))?;

        match algorithm.as_str() {
            ALGORITHM_ED25519 => {
                let arr: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
                    ApiError::validation(format!(
                        "ed25519 public keys must be 32 bytes, got {}",
                        bytes.len()
                    ))
                })?;
                ed25519_dalek::VerifyingKey::from_bytes(&arr)
                    .map_err(|_| ApiError::validation("not a valid ed25519 public key"))?;
                Ok(Self {
                    algorithm,
                    bytes: arr.to_vec(),
                })
            }
            other => Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                code::UNSUPPORTED_ALGORITHM,
                format!("unsupported key algorithm: {other}"),
            )),
        }
    }

    /// Reconstruct from already-validated stored bytes.
    pub fn from_stored(algorithm: &str, bytes: Vec<u8>) -> Self {
        Self {
            algorithm: algorithm.to_string(),
            bytes,
        }
    }

    /// Stable fingerprint, and for identities the canonical identity ID (spec §3.1):
    ///
    /// ```text
    /// base64url(SHA-256(lp(context) || lp(algorithm_id) || lp(canonical_key_bytes)))
    /// ```
    ///
    /// Length prefixes are unsigned 32-bit network byte order; the Base64 encoding
    /// omits padding. Adding an algorithm cannot change existing fingerprints
    /// because the algorithm ID is a distinct length-prefixed field.
    pub fn fingerprint(&self) -> String {
        let input = LengthPrefixed::new()
            .field_str(IDENTITY_FINGERPRINT_CONTEXT)
            .field_str(&self.algorithm)
            .field(&self.bytes)
            .finish();
        b64_encode(&sha256(&input))
    }

    pub fn encoded(&self) -> String {
        b64_encode(&self.bytes)
    }

    /// Verify a detached signature over `message`.
    pub fn verify(&self, message: &[u8], signature: &[u8]) -> bool {
        match self.algorithm.as_str() {
            ALGORITHM_ED25519 => {
                let Ok(key_bytes): Result<[u8; 32], _> = self.bytes.as_slice().try_into() else {
                    return false;
                };
                let Ok(vk) = ed25519_dalek::VerifyingKey::from_bytes(&key_bytes) else {
                    return false;
                };
                let Ok(sig_bytes): Result<[u8; 64], _> = signature.try_into() else {
                    return false;
                };
                let sig = ed25519_dalek::Signature::from_bytes(&sig_bytes);
                // verify_strict rejects small-order public keys and non-canonical
                // encodings, closing signature-malleability corner cases.
                vk.verify_strict(message, &sig).is_ok()
            }
            _ => false,
        }
    }
}

// ------------------------------------------------------------------ signing input

/// Operations a challenge may authorise. A challenge is bound to exactly one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    RegisterIdentity,
    AuthenticateIdentity,
    RegisterDevice,
    AuthenticateDevice,
}

impl Operation {
    pub fn as_str(&self) -> &'static str {
        match self {
            Operation::RegisterIdentity => "register_identity",
            Operation::AuthenticateIdentity => "authenticate_identity",
            Operation::RegisterDevice => "register_device",
            Operation::AuthenticateDevice => "authenticate_device",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "register_identity" => Some(Operation::RegisterIdentity),
            "authenticate_identity" => Some(Operation::AuthenticateIdentity),
            "register_device" => Some(Operation::RegisterDevice),
            "authenticate_device" => Some(Operation::AuthenticateDevice),
            _ => None,
        }
    }
}

/// Everything bound into a challenge signature.
pub struct SigningInput<'a> {
    pub origin: &'a str,
    pub challenge_id: &'a str,
    pub challenge: &'a [u8],
    pub operation: Operation,
    pub key_fingerprint: &'a str,
    /// Owner identity for a device-registration challenge; empty otherwise.
    pub owner_identity_id: &'a str,
    /// Proposed device key fingerprint for a device-registration challenge; empty otherwise.
    pub device_key_fingerprint: &'a str,
    pub expires_at_unix_ms: u64,
}

impl SigningInput<'_> {
    /// Length-prefixed binary encoding (spec §4.2 prefers this over JSON).
    ///
    /// Every field is always present — empty when not applicable — so the encoding
    /// has a fixed shape and no field boundary is ambiguous.
    pub fn encode(&self) -> Vec<u8> {
        LengthPrefixed::new()
            .field_str(CHALLENGE_SIGNATURE_CONTEXT)
            .field_str(self.origin)
            .field_str(self.challenge_id)
            .field(self.challenge)
            .field_str(self.operation.as_str())
            .field_str(self.key_fingerprint)
            .field_str(self.owner_identity_id)
            .field_str(self.device_key_fingerprint)
            .field_u64(self.expires_at_unix_ms)
            .finish()
    }
}

// -------------------------------------------------------------------- secret box

/// Authenticated encryption of database-stored secrets under bootstrap key
/// material that is deliberately *not* stored in that database (spec §8.1).
pub struct SecretBox {
    key: [u8; 32],
}

impl SecretBox {
    pub fn new(key: [u8; 32]) -> Self {
        Self { key }
    }

    pub fn seal(&self, plaintext: &[u8]) -> (Vec<u8>, Vec<u8>) {
        use chacha20poly1305::{ChaCha20Poly1305, KeyInit, aead::Aead};
        let cipher = ChaCha20Poly1305::new((&self.key).into());
        let nonce = random_bytes(12);
        let nonce_arr: [u8; 12] = nonce.clone().try_into().expect("12 random bytes");
        let ciphertext = cipher
            .encrypt((&nonce_arr).into(), plaintext)
            .expect("chacha20poly1305 encryption cannot fail for in-memory input");
        (nonce, ciphertext)
    }

    pub fn open(&self, nonce: &[u8], ciphertext: &[u8]) -> Option<Vec<u8>> {
        use chacha20poly1305::{ChaCha20Poly1305, KeyInit, aead::Aead};
        let nonce_arr: [u8; 12] = nonce.try_into().ok()?;
        let cipher = ChaCha20Poly1305::new((&self.key).into());
        cipher.decrypt((&nonce_arr).into(), ciphertext).ok()
    }

    /// Compact single-string form used for inline secret settings.
    pub fn seal_to_string(&self, plaintext: &str) -> String {
        let (nonce, ct) = self.seal(plaintext.as_bytes());
        let mut joined = nonce;
        joined.extend_from_slice(&ct);
        format!("enc:v1:{}", b64_encode(&joined))
    }

    pub fn open_from_string(&self, s: &str) -> Option<String> {
        let rest = s.strip_prefix("enc:v1:")?;
        let joined = b64_decode(rest)?;
        if joined.len() < 12 {
            return None;
        }
        let (nonce, ct) = joined.split_at(12);
        let plain = self.open(nonce, ct)?;
        String::from_utf8(plain).ok()
    }
}

// ------------------------------------------------------------------------ tokens

pub const TOKEN_PREFIX: &str = "v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrincipalKind {
    Identity,
    Device,
}

impl PrincipalKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            PrincipalKind::Identity => "identity",
            PrincipalKind::Device => "device",
        }
    }
}

/// Access token claims (spec §4.3). Carried inside the token rather than
/// referenced, so ordinary authentication needs no database read; revocation is
/// checked separately against durable state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenClaims {
    /// Unique token ID.
    pub jti: String,
    /// Authenticated identity or device ID.
    pub sub: String,
    #[serde(rename = "typ")]
    pub principal: PrincipalKind,
    /// Owning identity. Equal to `sub` for identity tokens.
    pub identity_id: String,
    pub iss: String,
    pub aud: String,
    pub iat: i64,
    pub exp: i64,
    pub scopes: Vec<String>,
    /// Signing key ID, so a rotated key can still verify during its overlap window.
    pub kid: String,
}

impl TokenClaims {
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| s == scope)
    }
}

/// One HMAC signing key. Verification accepts any supplied key (rotation overlap);
/// minting always uses the active one.
#[derive(Debug, Clone)]
pub struct SigningKey {
    pub kid: String,
    pub secret: Vec<u8>,
}

fn mac(secret: &[u8], message: &[u8]) -> Vec<u8> {
    use hmac::{Hmac, KeyInit, Mac};
    let mut m = <Hmac<sha2::Sha256> as KeyInit>::new_from_slice(secret)
        .expect("hmac accepts keys of any length");
    m.update(message);
    m.finalize().into_bytes().to_vec()
}

pub fn mint_token(key: &SigningKey, claims: &TokenClaims) -> String {
    let payload = serde_json::to_vec(claims).expect("claims serialise");
    let signing_input = format!("{TOKEN_PREFIX}.{}", b64_encode(&payload));
    let tag = mac(&key.secret, signing_input.as_bytes());
    format!("{signing_input}.{}", b64_encode(&tag))
}

/// Verify structure, signature and expiry. Revocation and scope checks are the
/// caller's responsibility because they need durable state and request context.
pub fn verify_token(
    token: &str,
    keys: &[SigningKey],
    expected_issuer: &str,
    expected_audience: &str,
    now_unix: i64,
    max_skew_seconds: i64,
) -> ApiResult<TokenClaims> {
    let mut parts = token.split('.');
    let (Some(version), Some(payload_b64), Some(tag_b64), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(ApiError::unauthorized("malformed access token"));
    };
    if version != TOKEN_PREFIX {
        return Err(ApiError::unauthorized("unsupported access token version"));
    }
    let payload =
        b64_decode(payload_b64).ok_or_else(|| ApiError::unauthorized("malformed access token"))?;
    let tag =
        b64_decode(tag_b64).ok_or_else(|| ApiError::unauthorized("malformed access token"))?;
    let claims: TokenClaims = serde_json::from_slice(&payload)
        .map_err(|_| ApiError::unauthorized("malformed access token"))?;

    let signing_input = format!("{version}.{payload_b64}");
    let key = keys
        .iter()
        .find(|k| k.kid == claims.kid)
        .ok_or_else(|| ApiError::unauthorized("access token signed by an unknown key"))?;
    if !ct_eq(&mac(&key.secret, signing_input.as_bytes()), &tag) {
        return Err(ApiError::unauthorized("access token signature is invalid"));
    }

    if claims.iss != expected_issuer || claims.aud != expected_audience {
        return Err(ApiError::unauthorized(
            "access token issuer or audience mismatch",
        ));
    }
    if now_unix > claims.exp + max_skew_seconds {
        return Err(ApiError::unauthorized("access token has expired"));
    }
    if claims.iat > now_unix + max_skew_seconds {
        return Err(ApiError::unauthorized("access token is not yet valid"));
    }
    Ok(claims)
}

/// Generate a single-use WebSocket ticket. Only its hash is stored, so a database
/// reader cannot use a ticket it observes.
pub fn new_ticket() -> (String, String) {
    let raw = b64_encode(&random_bytes(32));
    let hash = b64_encode(&sha256(raw.as_bytes()));
    (raw, hash)
}

pub fn hash_ticket(raw: &str) -> String {
    b64_encode(&sha256(raw.as_bytes()))
}

pub fn hash_operator_token(raw: &str) -> String {
    b64_encode(&sha256(raw.as_bytes()))
}
