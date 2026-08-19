//! Typed, database-backed runtime settings (spec §5.5 and §8.1).

pub mod defs;
pub mod store;

use crate::crypto::SecretBox;
use defs::{DEFS, REPLAY_CAPACITY_HARD_MAX, keys};
use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Bool,
    Int,
    Str,
    Enum,
    List,
    Secret,
}

#[derive(Debug, Clone, Copy)]
pub enum DefaultValue {
    Bool(bool),
    Int(i64),
    Str(&'static str),
    List(&'static [&'static str]),
}

/// How a change takes effect. Reported by `GET /v1/admin/settings` so operators
/// know what a change will do before they make it (spec §8.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reload {
    /// Applies to every operation started after the new revision is observed.
    Immediate,
    /// Existing connections keep the value negotiated at connect time; new
    /// connections use the new value. A *reduction* still applies immediately,
    /// because the specification requires reductions needed to prevent resource
    /// exhaustion to affect existing connections promptly.
    ConnectionRenegotiate,
    /// Triggers an automated listener rebind with connection drain.
    ListenerRebind,
    /// Reinitialises the logging subscriber in place.
    LoggingReload,
    /// Re-applies storage pragmas to pooled connections as they are checked out.
    StorageReconfigure,
}

pub struct SettingDef {
    pub name: &'static str,
    pub kind: Kind,
    pub default: DefaultValue,
    pub min: Option<i64>,
    pub max: Option<i64>,
    pub allowed: Option<&'static [&'static str]>,
    pub secret: bool,
    pub reload: Reload,
    pub description: &'static str,
}

impl SettingDef {
    pub fn default_value(&self) -> Value {
        match self.default {
            DefaultValue::Bool(b) => Value::Bool(b),
            DefaultValue::Int(i) => Value::Int(i),
            DefaultValue::Str(s) => Value::Str(s.to_string()),
            DefaultValue::List(l) => Value::List(l.iter().map(|s| s.to_string()).collect()),
        }
    }
}

pub fn find_def(name: &str) -> Option<&'static SettingDef> {
    DEFS.iter().find(|d| d.name == name)
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Bool(bool),
    Int(i64),
    Str(String),
    List(Vec<String>),
}

impl Value {
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Value::Bool(b) => serde_json::Value::Bool(*b),
            Value::Int(i) => serde_json::Value::from(*i),
            Value::Str(s) => serde_json::Value::String(s.clone()),
            Value::List(l) => {
                serde_json::Value::Array(l.iter().cloned().map(serde_json::Value::String).collect())
            }
        }
    }

    /// Parse a JSON value against a setting's declared kind. Type coercion is
    /// deliberately strict so a typo cannot silently change behaviour.
    pub fn from_json(def: &SettingDef, json: &serde_json::Value) -> Result<Self, String> {
        match def.kind {
            Kind::Bool => json
                .as_bool()
                .map(Value::Bool)
                .ok_or_else(|| format!("{} expects a boolean", def.name)),
            Kind::Int => json
                .as_i64()
                .map(Value::Int)
                .ok_or_else(|| format!("{} expects an integer", def.name)),
            Kind::Str | Kind::Enum | Kind::Secret => json
                .as_str()
                .map(|s| Value::Str(s.to_string()))
                .ok_or_else(|| format!("{} expects a string", def.name)),
            Kind::List => {
                let arr = json
                    .as_array()
                    .ok_or_else(|| format!("{} expects an array of strings", def.name))?;
                let mut out = Vec::with_capacity(arr.len());
                for item in arr {
                    let s = item
                        .as_str()
                        .ok_or_else(|| format!("{} expects an array of strings", def.name))?;
                    out.push(s.to_string());
                }
                Ok(Value::List(out))
            }
        }
    }

    /// A hash of the value, for the audit log. Raw values, and therefore raw
    /// secrets, never reach the audit table (spec §5.5).
    pub fn audit_hash(&self) -> String {
        crate::util::sha256_hex(self.to_json().to_string().as_bytes())
    }
}

/// Per-value validation against the setting's declared constraints.
pub fn validate_value(def: &SettingDef, value: &Value) -> Result<(), String> {
    match (def.kind, value) {
        (Kind::Bool, Value::Bool(_)) => Ok(()),
        (Kind::Int, Value::Int(i)) => {
            if let Some(min) = def.min
                && *i < min
            {
                return Err(format!("{} must be at least {min}", def.name));
            }
            if let Some(max) = def.max
                && *i > max
            {
                return Err(format!("{} must be at most {max}", def.name));
            }
            Ok(())
        }
        (Kind::Str, Value::Str(_)) | (Kind::Secret, Value::Str(_)) => Ok(()),
        (Kind::Enum, Value::Str(s)) => {
            let allowed = def.allowed.unwrap_or(&[]);
            if allowed.contains(&s.as_str()) {
                Ok(())
            } else {
                Err(format!(
                    "{} must be one of {}",
                    def.name,
                    allowed.join(", ")
                ))
            }
        }
        (Kind::List, Value::List(items)) => {
            if items.iter().any(|s| s.trim().is_empty()) {
                return Err(format!("{} must not contain empty entries", def.name));
            }
            Ok(())
        }
        _ => Err(format!("{} has the wrong value type", def.name)),
    }
}

/// An immutable, internally consistent view of every setting.
///
/// Each request, connection and output batch captures one snapshot, so a
/// concurrent update can never produce a mix of old and new limits (spec §5.5).
pub struct Snapshot {
    pub revision: i64,
    pub values: BTreeMap<String, Value>,
    secret_box: Arc<SecretBox>,
}

impl std::fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Snapshot")
            .field("revision", &self.revision)
            .finish_non_exhaustive()
    }
}

impl Snapshot {
    pub fn new(revision: i64, values: BTreeMap<String, Value>, secret_box: Arc<SecretBox>) -> Self {
        Self {
            revision,
            values,
            secret_box,
        }
    }

    fn get(&self, name: &str) -> Value {
        if let Some(v) = self.values.get(name) {
            return v.clone();
        }
        // Unreachable in practice: loading fills every declared key. Falling back to
        // the declared default is still safer than panicking inside a request.
        match find_def(name) {
            Some(def) => {
                tracing::error!(
                    event = "setting_missing",
                    setting = name,
                    "falling back to default"
                );
                def.default_value()
            }
            None => {
                tracing::error!(
                    event = "setting_undeclared",
                    setting = name,
                    "returning zero"
                );
                Value::Int(0)
            }
        }
    }

    pub fn int(&self, name: &str) -> i64 {
        match self.get(name) {
            Value::Int(i) => i,
            _ => 0,
        }
    }

    pub fn u64(&self, name: &str) -> u64 {
        self.int(name).max(0) as u64
    }

    pub fn usize(&self, name: &str) -> usize {
        self.int(name).max(0) as usize
    }

    pub fn u32(&self, name: &str) -> u32 {
        self.int(name).clamp(0, u32::MAX as i64) as u32
    }

    pub fn bool(&self, name: &str) -> bool {
        matches!(self.get(name), Value::Bool(true))
    }

    pub fn string(&self, name: &str) -> String {
        match self.get(name) {
            Value::Str(s) => s,
            other => other.to_json().to_string(),
        }
    }

    pub fn list(&self, name: &str) -> Vec<String> {
        match self.get(name) {
            Value::List(l) => l,
            _ => Vec::new(),
        }
    }

    pub fn duration_ms(&self, name: &str) -> Duration {
        Duration::from_millis(self.u64(name))
    }

    pub fn duration_secs(&self, name: &str) -> Duration {
        Duration::from_secs(self.u64(name))
    }

    /// Resolve a secret setting to its plaintext.
    ///
    /// Supported forms: `env:NAME` and `file:/path` references to an external
    /// provider (preferred), or an inline value encrypted under bootstrap key
    /// material. Returns `None` when unset or unresolvable.
    pub fn secret(&self, name: &str) -> Option<String> {
        let raw = self.string(name);
        resolve_secret(&self.secret_box, &raw)
    }

    /// How a secret is configured, for the redacted admin view.
    pub fn secret_form(&self, name: &str) -> &'static str {
        let raw = self.string(name);
        if raw.is_empty() {
            "unset"
        } else if raw.starts_with("env:") {
            "environment_reference"
        } else if raw.starts_with("file:") {
            "file_reference"
        } else if raw.starts_with("enc:v1:") {
            "encrypted_inline"
        } else {
            "invalid"
        }
    }

    // ------------------------------------------------------------ derived helpers

    pub fn replay_capacity(&self) -> usize {
        // Belt and braces: the schema maximum already caps this, and the hard
        // specification ceiling is re-applied here so no code path can exceed it.
        self.usize(keys::TERMINAL_REPLAY_CAPACITY_BYTES)
            .min(REPLAY_CAPACITY_HARD_MAX as usize)
    }
}

pub fn resolve_secret(secret_box: &SecretBox, raw: &str) -> Option<String> {
    if raw.is_empty() {
        return None;
    }
    if let Some(var) = raw.strip_prefix("env:") {
        return std::env::var(var).ok().filter(|v| !v.is_empty());
    }
    if let Some(path) = raw.strip_prefix("file:") {
        return std::fs::read_to_string(path)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
    }
    secret_box.open_from_string(raw)
}

/// Cross-setting validation (spec §5.5: an invalid *combination* is rejected
/// atomically, without applying any part of the update).
pub fn validate_combination(values: &BTreeMap<String, Value>) -> Result<(), Vec<String>> {
    let mut errors = Vec::new();

    let int = |name: &str| -> i64 {
        match values.get(name) {
            Some(Value::Int(i)) => *i,
            _ => find_def(name)
                .map(|d| match d.default {
                    DefaultValue::Int(i) => i,
                    _ => 0,
                })
                .unwrap_or(0),
        }
    };
    let boolean = |name: &str| -> bool { matches!(values.get(name), Some(Value::Bool(true))) };
    let text = |name: &str| -> String {
        match values.get(name) {
            Some(Value::Str(s)) => s.clone(),
            _ => String::new(),
        }
    };
    let list = |name: &str| -> Vec<String> {
        match values.get(name) {
            Some(Value::List(l)) => l.clone(),
            _ => Vec::new(),
        }
    };

    // The specification ceiling, restated independently of the schema maximum.
    if int(keys::TERMINAL_REPLAY_CAPACITY_BYTES) > REPLAY_CAPACITY_HARD_MAX {
        errors.push(format!(
            "{} must not exceed the specification ceiling of {REPLAY_CAPACITY_HARD_MAX} bytes",
            keys::TERMINAL_REPLAY_CAPACITY_BYTES
        ));
    }

    // A checkpoint must be reachable inside the publisher's unacknowledged window,
    // otherwise a publisher would be closed for exceeding a limit it can never
    // clear (spec §7.2 forbids configuring the flush thresholds that way).
    if int(keys::PERSISTENCE_FLUSH_BYTES) > int(keys::LIMITS_MAX_UNACKED_OUTPUT_BYTES) {
        errors.push(format!(
            "{} must not exceed {}",
            keys::PERSISTENCE_FLUSH_BYTES,
            keys::LIMITS_MAX_UNACKED_OUTPUT_BYTES
        ));
    }
    if int(keys::LIMITS_MAX_OUTPUT_FRAME_BYTES) > int(keys::LIMITS_MAX_UNACKED_OUTPUT_BYTES) {
        errors.push(format!(
            "{} must not exceed {}",
            keys::LIMITS_MAX_OUTPUT_FRAME_BYTES,
            keys::LIMITS_MAX_UNACKED_OUTPUT_BYTES
        ));
    }
    if int(keys::PERSISTENCE_MEMORY_PRESSURE_DIRTY_BYTES) < int(keys::PERSISTENCE_FLUSH_BYTES) {
        errors.push(format!(
            "{} must be at least {}",
            keys::PERSISTENCE_MEMORY_PRESSURE_DIRTY_BYTES,
            keys::PERSISTENCE_FLUSH_BYTES
        ));
    }
    if int(keys::PERSISTENCE_COMMIT_RETRY_MAX_MS) < int(keys::PERSISTENCE_COMMIT_RETRY_INITIAL_MS) {
        errors.push(format!(
            "{} must be at least {}",
            keys::PERSISTENCE_COMMIT_RETRY_MAX_MS,
            keys::PERSISTENCE_COMMIT_RETRY_INITIAL_MS
        ));
    }
    if int(keys::LIMITS_DEFAULT_PAGE_SIZE) > int(keys::LIMITS_MAX_PAGE_SIZE) {
        errors.push(format!(
            "{} must not exceed {}",
            keys::LIMITS_DEFAULT_PAGE_SIZE,
            keys::LIMITS_MAX_PAGE_SIZE
        ));
    }
    if int(keys::WEBSOCKET_HEARTBEAT_TIMEOUT_SECONDS)
        <= int(keys::WEBSOCKET_HEARTBEAT_INTERVAL_SECONDS)
    {
        errors.push(format!(
            "{} must exceed {}",
            keys::WEBSOCKET_HEARTBEAT_TIMEOUT_SECONDS,
            keys::WEBSOCKET_HEARTBEAT_INTERVAL_SECONDS
        ));
    }

    // Network settings must be usable before they are committed, so a bad value
    // cannot take the listener down.
    let listen = text(keys::SERVER_LISTEN_ADDRESS);
    if listen.parse::<SocketAddr>().is_err() {
        errors.push(format!(
            "{} must be a socket address such as 0.0.0.0:8080",
            keys::SERVER_LISTEN_ADDRESS
        ));
    }
    let health_listen = text(keys::SERVER_HEALTH_LISTEN_ADDRESS);
    if !health_listen.is_empty() {
        if health_listen.parse::<SocketAddr>().is_err() {
            errors.push(format!(
                "{} must be empty or a socket address",
                keys::SERVER_HEALTH_LISTEN_ADDRESS
            ));
        } else if health_listen == listen {
            errors.push(format!(
                "{} must differ from {}",
                keys::SERVER_HEALTH_LISTEN_ADDRESS,
                keys::SERVER_LISTEN_ADDRESS
            ));
        }
    }

    let origin = text(keys::SERVER_PUBLIC_ORIGIN);
    if !(origin.starts_with("http://") || origin.starts_with("https://")) {
        errors.push(format!(
            "{} must start with http:// or https://",
            keys::SERVER_PUBLIC_ORIGIN
        ));
    }
    if origin.ends_with('/') {
        errors.push(format!(
            "{} must not end with a slash",
            keys::SERVER_PUBLIC_ORIGIN
        ));
    }

    if boolean(keys::SERVER_TLS_ENABLED) {
        for key in [
            keys::SERVER_TLS_CERTIFICATE_PATH,
            keys::SERVER_TLS_PRIVATE_KEY_PATH,
        ] {
            let path = text(key);
            if path.is_empty() {
                errors.push(format!(
                    "{key} is required when {} is true",
                    keys::SERVER_TLS_ENABLED
                ));
            } else if std::fs::metadata(&path).is_err() {
                errors.push(format!("{key} points at an unreadable path: {path}"));
            }
        }
    }

    for cidr in list(keys::SECURITY_TRUSTED_PROXY_NETWORKS) {
        if parse_cidr(&cidr).is_none() {
            errors.push(format!(
                "{} contains an invalid CIDR block: {cidr}",
                keys::SECURITY_TRUSTED_PROXY_NETWORKS
            ));
        }
    }

    let algorithms = list(keys::AUTH_SUPPORTED_KEY_ALGORITHMS);
    if algorithms.is_empty() {
        errors.push(format!(
            "{} must list at least one algorithm",
            keys::AUTH_SUPPORTED_KEY_ALGORITHMS
        ));
    }
    for algorithm in &algorithms {
        if algorithm != crate::crypto::ALGORITHM_ED25519 {
            errors.push(format!(
                "{} contains an algorithm this build cannot verify: {algorithm}",
                keys::AUTH_SUPPORTED_KEY_ALGORITHMS
            ));
        }
    }
    if !algorithms
        .iter()
        .any(|a| a == crate::crypto::ALGORITHM_ED25519)
    {
        errors.push(format!(
            "{} must include ed25519, which the specification requires",
            keys::AUTH_SUPPORTED_KEY_ALGORITHMS
        ));
    }

    // No configuration may grant a principal authority its kind and role must not
    // have (spec §4.3): a publisher can never gain identity-level or input authority,
    // and a client can never gain the ability to publish or manage devices.
    for (setting, allowed, description) in [
        (
            keys::AUTH_IDENTITY_TOKEN_SCOPES,
            crate::crypto::scope::IDENTITY_ALLOWED,
            "an identity",
        ),
        (
            keys::AUTH_DEVICE_TOKEN_SCOPES,
            crate::crypto::scope::PUBLISHER_ALLOWED,
            "a publisher-role device",
        ),
        (
            keys::AUTH_CLIENT_TOKEN_SCOPES,
            crate::crypto::scope::CLIENT_ALLOWED,
            "a client-role device",
        ),
    ] {
        for scope in list(setting) {
            if !crate::crypto::scope::ALL.contains(&scope.as_str()) {
                errors.push(format!("{setting} contains an unknown scope: {scope}"));
            } else if !allowed.contains(&scope.as_str()) {
                errors.push(format!(
                    "{setting} must not include {scope}, which {description} may never hold"
                ));
            }
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Minimal CIDR parsing, used for the trusted-proxy allowlist.
pub fn parse_cidr(s: &str) -> Option<(IpAddr, u8)> {
    let (addr, prefix) = s.split_once('/')?;
    let ip: IpAddr = addr.trim().parse().ok()?;
    let bits: u8 = prefix.trim().parse().ok()?;
    let max = if ip.is_ipv4() { 32 } else { 128 };
    if bits > max { None } else { Some((ip, bits)) }
}

pub fn cidr_contains(network: IpAddr, prefix: u8, candidate: IpAddr) -> bool {
    fn octets(ip: IpAddr) -> Vec<u8> {
        match ip {
            IpAddr::V4(v4) => v4.octets().to_vec(),
            IpAddr::V6(v6) => v6.octets().to_vec(),
        }
    }
    // Compare only same-family addresses, after unwrapping IPv4-mapped IPv6.
    let candidate = match (network, candidate) {
        (IpAddr::V4(_), IpAddr::V6(v6)) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => return false,
        },
        _ => candidate,
    };
    if network.is_ipv4() != candidate.is_ipv4() {
        return false;
    }
    let (net, cand) = (octets(network), octets(candidate));
    let full = (prefix / 8) as usize;
    if net[..full] != cand[..full] {
        return false;
    }
    let remainder = prefix % 8;
    if remainder == 0 {
        return true;
    }
    let mask = 0xffu8 << (8 - remainder);
    net[full] & mask == cand[full] & mask
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_declared_key_has_a_definition() {
        for key in defs::ALL_KEYS {
            assert!(find_def(key).is_some(), "{key} is missing a definition");
        }
        assert_eq!(defs::ALL_KEYS.len(), DEFS.len());
    }

    #[test]
    fn remote_terminal_creation_is_granted_by_nobody_by_default() {
        // Spec §4.6 condition 5. Starting a process on somebody's machine is the most
        // dangerous authority this service has, so upgrading the binary must never
        // confer it: an operator has to add the scope, and separately turn the feature
        // on. Both halves are asserted here because either one alone would be enough to
        // make an upgrade grant it silently.
        for key in [
            keys::AUTH_IDENTITY_TOKEN_SCOPES,
            keys::AUTH_DEVICE_TOKEN_SCOPES,
            keys::AUTH_CLIENT_TOKEN_SCOPES,
        ] {
            let def = find_def(key).expect("declared");
            let DefaultValue::List(scopes) = def.default else {
                panic!("{key} should be a list");
            };
            assert!(
                !scopes.contains(&crate::crypto::scope::TERMINALS_CREATE),
                "{key} must not grant terminals:create by default"
            );
        }

        let def = find_def(keys::FEATURES_TERMINAL_CREATE_ENABLED).expect("declared");
        assert!(
            matches!(def.default, DefaultValue::Bool(false)),
            "features.terminal_create_enabled must default to off"
        );
    }

    #[test]
    fn only_the_principals_that_may_ask_are_allowed_the_creation_scope() {
        use crate::crypto::scope;
        // A publishing device hosts terminals; it never asks anybody else to. Letting it
        // hold the scope would let a compromised publisher reach sideways into the
        // owner's other machines.
        assert!(!scope::PUBLISHER_ALLOWED.contains(&scope::TERMINALS_CREATE));
        assert!(scope::IDENTITY_ALLOWED.contains(&scope::TERMINALS_CREATE));
        assert!(scope::CLIENT_ALLOWED.contains(&scope::TERMINALS_CREATE));
        assert!(scope::ALL.contains(&scope::TERMINALS_CREATE));
        // A phone must be able to name the machine it is asking (spec §4.4).
        assert!(scope::CLIENT_ALLOWED.contains(&scope::DEVICES_READ));
    }

    #[test]
    fn setting_names_are_unique_and_dotted() {
        let mut seen = std::collections::HashSet::new();
        for def in DEFS {
            assert!(seen.insert(def.name), "duplicate setting {}", def.name);
            assert!(
                def.name.contains('.'),
                "{} must use a dotted name",
                def.name
            );
            assert!(
                !def.description.is_empty(),
                "{} needs a description",
                def.name
            );
        }
    }

    #[test]
    fn declared_defaults_are_individually_valid() {
        for def in DEFS {
            let value = def.default_value();
            validate_value(def, &value).unwrap_or_else(|e| panic!("default for {}: {e}", def.name));
        }
    }

    #[test]
    fn declared_defaults_are_valid_as_a_combination() {
        let values: BTreeMap<String, Value> = DEFS
            .iter()
            .map(|d| (d.name.to_string(), d.default_value()))
            .collect();
        validate_combination(&values).expect("shipped defaults must form a valid combination");
    }

    #[test]
    fn replay_capacity_default_and_ceiling_match_the_specification() {
        let def = find_def(keys::TERMINAL_REPLAY_CAPACITY_BYTES).unwrap();
        assert!(matches!(def.default, DefaultValue::Int(1_500_000)));
        assert_eq!(def.max, Some(1_500_000));
    }

    #[test]
    fn cidr_matching() {
        let (net, bits) = parse_cidr("10.0.0.0/8").unwrap();
        assert!(cidr_contains(net, bits, "10.4.5.6".parse().unwrap()));
        assert!(!cidr_contains(net, bits, "11.0.0.1".parse().unwrap()));
        let (net, bits) = parse_cidr("192.168.1.128/25").unwrap();
        assert!(cidr_contains(net, bits, "192.168.1.200".parse().unwrap()));
        assert!(!cidr_contains(net, bits, "192.168.1.100".parse().unwrap()));
        assert!(parse_cidr("10.0.0.0/33").is_none());
    }
}
