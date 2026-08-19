//! Keys, fingerprints and challenge signatures (relay spec §3.1, §4.2).
//!
//! These constructions are shared with the relay, and a mismatch of even one length
//! prefix produces a signature the relay rejects with no useful diagnosis. They are
//! therefore written to the specification's wording and checked against the relay's
//! own implementation in `tests/protocol.rs`, not merely against a second reading.

use base64::Engine as _;
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use sha2::{Digest, Sha256};

/// Domain separator for identity and device key fingerprints (spec §3.1).
pub const IDENTITY_FINGERPRINT_CONTEXT: &str = "terminal-relay-identity-v1";
/// Domain separator for proof-of-possession signatures (spec §4.2).
pub const CHALLENGE_SIGNATURE_CONTEXT: &str = "terminal-relay-challenge-v1";
pub const ALGORITHM_ED25519: &str = "ed25519";

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::URL_SAFE_NO_PAD;

pub fn b64_encode(bytes: &[u8]) -> String {
    B64.encode(bytes)
}

pub fn b64_decode(s: &str) -> Option<Vec<u8>> {
    // The relay emits unpadded base64url but tolerates padding; be equally tolerant.
    if let Ok(v) = B64.decode(s.as_bytes()) {
        return Some(v);
    }
    base64::engine::general_purpose::URL_SAFE
        .decode(s.as_bytes())
        .ok()
}

/// Unambiguous length-prefixed concatenation: each field carries its length as an
/// unsigned 32-bit network-byte-order integer, so no field boundary can be confused
/// with content (spec §3.1, §4.2).
#[derive(Default)]
pub struct LengthPrefixed {
    buf: Vec<u8>,
}

impl LengthPrefixed {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn field(mut self, bytes: &[u8]) -> Self {
        self.buf
            .extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        self.buf.extend_from_slice(bytes);
        self
    }

    pub fn field_str(self, s: &str) -> Self {
        self.field(s.as_bytes())
    }

    pub fn field_u64(self, value: u64) -> Self {
        self.field(&value.to_be_bytes())
    }

    pub fn finish(self) -> Vec<u8> {
        self.buf
    }
}

/// Splits a length-prefixed encoding back into its fields.
///
/// Used to inspect the `signing_input` the relay hands back before signing it. The
/// relay recomputes and verifies its own derivation, so this is not what makes the
/// exchange safe — but signing bytes chosen entirely by the peer, without looking at
/// them, is a habit worth not having.
pub fn parse_length_prefixed(bytes: &[u8]) -> Option<Vec<Vec<u8>>> {
    let mut fields = Vec::new();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        if cursor + 4 > bytes.len() {
            return None;
        }
        let length = u32::from_be_bytes(bytes[cursor..cursor + 4].try_into().ok()?) as usize;
        cursor += 4;
        if cursor + length > bytes.len() {
            return None;
        }
        fields.push(bytes[cursor..cursor + length].to_vec());
        cursor += length;
        // A malformed input could otherwise describe an unbounded number of fields.
        if fields.len() > 64 {
            return None;
        }
    }
    Some(fields)
}

pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

/// The fingerprint of an Ed25519 public key, which for an identity is also its ID:
/// `base64url(SHA-256(lp(context) || lp(algorithm) || lp(key)))` (spec §3.1).
pub fn fingerprint(public_key: &[u8]) -> String {
    let input = LengthPrefixed::new()
        .field_str(IDENTITY_FINGERPRINT_CONTEXT)
        .field_str(ALGORITHM_ED25519)
        .field(public_key)
        .finish();
    b64_encode(&sha256(&input))
}

/// An Ed25519 key pair held by this machine.
#[derive(Clone)]
pub struct KeyPair {
    signing: SigningKey,
}

impl KeyPair {
    pub fn generate() -> Self {
        Self {
            signing: SigningKey::generate(&mut rand::rng()),
        }
    }

    pub fn from_seed(seed: &[u8]) -> Option<Self> {
        let seed: [u8; 32] = seed.try_into().ok()?;
        Some(Self {
            signing: SigningKey::from_bytes(&seed),
        })
    }

    pub fn seed(&self) -> [u8; 32] {
        self.signing.to_bytes()
    }

    pub fn verifying_key(&self) -> VerifyingKey {
        self.signing.verifying_key()
    }

    pub fn public_key_bytes(&self) -> [u8; 32] {
        self.signing.verifying_key().to_bytes()
    }

    pub fn public_key_base64(&self) -> String {
        b64_encode(&self.public_key_bytes())
    }

    pub fn fingerprint(&self) -> String {
        fingerprint(&self.public_key_bytes())
    }

    pub fn sign(&self, message: &[u8]) -> String {
        b64_encode(&self.signing.sign(message).to_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn length_prefixed_round_trips() {
        let encoded = LengthPrefixed::new()
            .field_str("alpha")
            .field(&[])
            .field_u64(7)
            .finish();
        let fields = parse_length_prefixed(&encoded).expect("parses");
        assert_eq!(fields.len(), 3);
        assert_eq!(fields[0], b"alpha");
        assert!(fields[1].is_empty(), "an empty field is still a field");
        assert_eq!(fields[2], 7u64.to_be_bytes());
    }

    #[test]
    fn a_truncated_field_is_rejected_rather_than_guessed() {
        let mut encoded = LengthPrefixed::new().field_str("alpha").finish();
        encoded.pop();
        assert!(parse_length_prefixed(&encoded).is_none());
    }

    #[test]
    fn a_signature_verifies_under_the_matching_public_key() {
        let pair = KeyPair::generate();
        let signature = pair.sign(b"message");
        let bytes: [u8; 64] = b64_decode(&signature).unwrap().try_into().unwrap();
        assert!(
            pair.verifying_key()
                .verify_strict(b"message", &ed25519_dalek::Signature::from_bytes(&bytes))
                .is_ok()
        );
    }

    #[test]
    fn a_seed_round_trips_to_the_same_key() {
        let pair = KeyPair::generate();
        let restored = KeyPair::from_seed(&pair.seed()).expect("32 bytes");
        assert_eq!(restored.public_key_bytes(), pair.public_key_bytes());
        assert_eq!(restored.fingerprint(), pair.fingerprint());
    }
}
