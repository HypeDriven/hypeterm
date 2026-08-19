//! The pairing code a phone uses to enrol itself (relay spec §5.2).
//!
//! Registering a device takes both parties: the owner authorises the request, and the
//! device proves it holds the key by signing a challenge bound to that owner. A phone
//! cannot hold the identity key, so what crosses is a short-lived identity token — the
//! owner's half, delegated for a few minutes.
//!
//! The encoding matches `core/src/api/pairing.cpp` in the client, and
//! `tests/pairing.rs` checks the two agree on a fixed vector rather than by inspection.

use crate::crypto::b64_encode;
use serde::Serialize;

#[derive(Debug, Clone)]
pub struct PairingCode {
    pub server_url: String,
    pub identity_id: String,
    /// A `devices:write` identity token. Minutes, not days.
    pub identity_token: String,
}

#[derive(Serialize)]
struct Wire<'a> {
    // Short keys: a person types or pastes this, so every character counts.
    u: &'a str,
    i: &'a str,
    t: &'a str,
}

/// `HT1.<base64url(json)>`. The prefix means a truncated paste fails immediately and
/// legibly rather than as a signature rejection several requests later.
pub fn encode(code: &PairingCode) -> String {
    let wire = Wire {
        u: &code.server_url,
        i: &code.identity_id,
        t: &code.identity_token,
    };
    let json = serde_json::to_string(&wire).expect("a pairing code always serialises");
    format!("HT1.{}", b64_encode(json.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::b64_decode;

    #[test]
    fn the_code_is_a_prefixed_base64url_document() {
        let code = encode(&PairingCode {
            server_url: "https://relay.example".into(),
            identity_id: "identity".into(),
            identity_token: "v1.a.b".into(),
        });
        let body = code.strip_prefix("HT1.").expect("carries the prefix");
        let decoded = b64_decode(body).expect("base64url");
        let text = String::from_utf8(decoded).expect("utf-8");
        // The client parses these three keys and nothing else.
        assert!(text.contains("\"u\":\"https://relay.example\""));
        assert!(text.contains("\"i\":\"identity\""));
        assert!(text.contains("\"t\":\"v1.a.b\""));
    }

    #[test]
    fn the_code_carries_no_padding_that_a_paste_could_lose() {
        let code = encode(&PairingCode {
            server_url: "https://relay.example/".into(),
            identity_id: "x".repeat(43),
            identity_token: "y".repeat(200),
        });
        assert!(!code.contains('='), "padding invites truncation on paste");
        assert!(
            !code.contains('+') && !code.contains('/'),
            "must be url-safe"
        );
    }
}

#[cfg(test)]
mod interoperability {
    use super::*;

    /// The exact string the client must accept. `tests/integration/test_pairing.cpp`
    /// in the Android project asserts the same literal decodes to the same fields, so
    /// neither side can change the encoding without the other's test failing.
    pub const VECTOR: &str = "HT1.eyJ1IjoiaHR0cHM6Ly9yZWxheS5leGFtcGxlIiwiaSI6ImlkZW50aXR5LWZpbmdlcnByaW50IiwidCI6InYxLnBheWxvYWQudGFnIn0";

    #[test]
    fn the_encoding_matches_the_client() {
        let encoded = encode(&PairingCode {
            server_url: "https://relay.example".into(),
            identity_id: "identity-fingerprint".into(),
            identity_token: "v1.payload.tag".into(),
        });
        assert_eq!(encoded, VECTOR);
    }
}
