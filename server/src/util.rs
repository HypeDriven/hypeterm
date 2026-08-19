//! Small shared helpers: base64url, length-prefixed encoding, time, IDs.

use base64::Engine;
use chrono::{DateTime, SecondsFormat, Utc};

pub const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::URL_SAFE_NO_PAD;

pub fn b64_encode(bytes: &[u8]) -> String {
    B64.encode(bytes)
}

pub fn b64_decode(s: &str) -> Option<Vec<u8>> {
    // Accept padded input as well, since some clients pad by default.
    if let Ok(v) = B64.decode(s.as_bytes()) {
        return Some(v);
    }
    base64::engine::general_purpose::URL_SAFE
        .decode(s.as_bytes())
        .ok()
}

/// Unambiguous length-prefixed concatenation.
///
/// Each field is prefixed with its length as an unsigned 32-bit network-byte-order
/// integer, per spec §3.1 and §4.2. This is what makes the identity fingerprint and
/// the challenge signing input impossible to confuse across field boundaries.
#[derive(Default)]
pub struct LengthPrefixed {
    buf: Vec<u8>,
}

impl LengthPrefixed {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
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

    pub fn field_u64(self, v: u64) -> Self {
        self.field(&v.to_be_bytes())
    }

    pub fn finish(self) -> Vec<u8> {
        self.buf
    }
}

pub fn now() -> DateTime<Utc> {
    Utc::now()
}

pub fn to_rfc3339(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Micros, true)
}

pub fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// Crockford base32 alphabet, excluding I, L, O and U.
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Lexicographically sortable identifier (ULID layout), used for request IDs,
/// challenge IDs and pagination cursors.
///
/// 48 bits of millisecond timestamp followed by 80 bits of randomness, rendered as
/// 26 Crockford base32 characters. Sorting by the string sorts by creation time,
/// which makes correlating log lines and audit entries straightforward.
pub fn new_ulid() -> String {
    let millis = Utc::now().timestamp_millis().max(0) as u128;
    let mut random = [0u8; 10];
    rand::fill(&mut random);

    let mut value: u128 = millis << 80;
    for (i, byte) in random.iter().enumerate() {
        value |= (*byte as u128) << (8 * (9 - i));
    }

    let mut out = [b'0'; 26];
    for slot in out.iter_mut().rev() {
        *slot = CROCKFORD[(value & 0x1f) as usize];
        value >>= 5;
    }
    String::from_utf8(out.to_vec()).expect("crockford alphabet is ASCII")
}

pub fn random_bytes(n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    rand::fill(buf.as_mut_slice());
    buf
}

/// Constant-time equality for secret material comparisons.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().into()
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&sha256(bytes))
}

pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}
