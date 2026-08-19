//! Bootstrap configuration: the *only* values that may come from the environment.
//!
//! Spec §8.1 restricts environment/CLI/file bootstrap to values required to locate,
//! decrypt and authenticate to the settings database, plus instance identity and an
//! emergency recovery mode. Everything that drives relay, API, security-policy,
//! retention, batching or limit behaviour lives in the database settings table.
//!
//! Adding a behaviour knob here is a spec violation; add a setting instead.

use crate::util::{b64_decode, b64_encode, random_bytes};
use std::path::{Path, PathBuf};

pub const ENV_DATA_DIR: &str = "RELAY_DATA_DIR";
pub const ENV_DB_PATH: &str = "RELAY_DB_PATH";
pub const ENV_SECRET_KEY: &str = "RELAY_SECRET_KEY";
pub const ENV_SECRET_KEY_FILE: &str = "RELAY_SECRET_KEY_FILE";
pub const ENV_OPERATOR_TOKEN: &str = "RELAY_OPERATOR_TOKEN";
pub const ENV_INSTANCE_ID: &str = "RELAY_INSTANCE_ID";
pub const ENV_RECOVERY_MODE: &str = "RELAY_RECOVERY_MODE";
pub const ENV_RECOVERY_LISTEN: &str = "RELAY_RECOVERY_LISTEN";

pub const DEFAULT_DATA_DIR: &str = "/var/lib/terminal-relay";

#[derive(Debug, Clone)]
pub struct Bootstrap {
    pub data_dir: PathBuf,
    pub db_path: PathBuf,
    /// 32 bytes of key material used to encrypt secret settings and token signing
    /// keys stored in the database. Never stored in that database.
    pub secret_key: [u8; 32],
    /// Seeds `auth.operator_token_hash` on first database initialisation only.
    pub operator_token_seed: Option<String>,
    pub instance_id: String,
    /// Emergency mode: ignore the stored listen/TLS settings and bind a plain-HTTP
    /// admin listener so an operator can repair a bad settings revision.
    pub recovery_mode: bool,
    pub recovery_listen: String,
}

#[derive(Debug, thiserror::Error)]
pub enum BootstrapError {
    #[error("{0}")]
    Invalid(String),
    #[error("io error for {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
}

fn env_opt(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => Some(v.trim().to_string()),
        _ => None,
    }
}

fn env_bool(key: &str) -> bool {
    matches!(
        env_opt(key).map(|v| v.to_ascii_lowercase()).as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

impl Bootstrap {
    pub fn from_env() -> Result<Self, BootstrapError> {
        let data_dir =
            PathBuf::from(env_opt(ENV_DATA_DIR).unwrap_or_else(|| DEFAULT_DATA_DIR.into()));
        let db_path = env_opt(ENV_DB_PATH)
            .map(PathBuf::from)
            .unwrap_or_else(|| data_dir.join("relay.db"));

        if let Some(parent) = db_path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|source| BootstrapError::Io {
                path: parent.display().to_string(),
                source,
            })?;
        }

        let secret_key = load_secret_key(&data_dir)?;

        Ok(Self {
            data_dir,
            db_path,
            secret_key,
            operator_token_seed: env_opt(ENV_OPERATOR_TOKEN),
            instance_id: env_opt(ENV_INSTANCE_ID).unwrap_or_else(|| {
                std::env::var("HOSTNAME").unwrap_or_else(|_| crate::util::new_ulid())
            }),
            recovery_mode: env_bool(ENV_RECOVERY_MODE),
            recovery_listen: env_opt(ENV_RECOVERY_LISTEN)
                .unwrap_or_else(|| "127.0.0.1:8081".into()),
        })
    }
}

/// Resolve bootstrap key material, in precedence order:
///
/// 1. `RELAY_SECRET_KEY` — base64url or base64 of 32 bytes.
/// 2. `RELAY_SECRET_KEY_FILE` — file containing the same.
/// 3. `<data_dir>/bootstrap.key` — generated on first run with 0600 permissions.
///
/// Option 3 keeps development frictionless but is weaker than a real secret
/// manager, because the key then lives on the same volume as the database it
/// protects. Production deployments should supply option 1 or 2.
fn load_secret_key(data_dir: &Path) -> Result<[u8; 32], BootstrapError> {
    if let Some(raw) = env_opt(ENV_SECRET_KEY) {
        return decode_key(&raw, ENV_SECRET_KEY);
    }

    if let Some(path) = env_opt(ENV_SECRET_KEY_FILE) {
        let raw = std::fs::read_to_string(&path).map_err(|source| BootstrapError::Io {
            path: path.clone(),
            source,
        })?;
        return decode_key(raw.trim(), ENV_SECRET_KEY_FILE);
    }

    let path = data_dir.join("bootstrap.key");
    if path.exists() {
        let raw = std::fs::read_to_string(&path).map_err(|source| BootstrapError::Io {
            path: path.display().to_string(),
            source,
        })?;
        return decode_key(raw.trim(), &path.display().to_string());
    }

    std::fs::create_dir_all(data_dir).map_err(|source| BootstrapError::Io {
        path: data_dir.display().to_string(),
        source,
    })?;
    let key = random_bytes(32);
    let encoded = b64_encode(&key);
    std::fs::write(&path, format!("{encoded}\n")).map_err(|source| BootstrapError::Io {
        path: path.display().to_string(),
        source,
    })?;
    restrict_permissions(&path);
    tracing::warn!(
        event = "bootstrap_key_generated",
        path = %path.display(),
        "generated bootstrap key material; supply RELAY_SECRET_KEY from a secret manager in production"
    );
    let mut out = [0u8; 32];
    out.copy_from_slice(&key);
    Ok(out)
}

fn decode_key(raw: &str, source: &str) -> Result<[u8; 32], BootstrapError> {
    let bytes = b64_decode(raw).ok_or_else(|| {
        BootstrapError::Invalid(format!("{source} must be base64url-encoded key material"))
    })?;
    if bytes.len() != 32 {
        return Err(BootstrapError::Invalid(format!(
            "{source} must decode to exactly 32 bytes, got {}",
            bytes.len()
        )));
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) {}
