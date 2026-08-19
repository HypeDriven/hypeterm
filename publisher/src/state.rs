//! What this machine remembers between runs.
//!
//! Two private keys live here: the identity's, which owns everything, and this
//! machine's device key. The relay's design keeps them separate so a device can be
//! revoked without rotating the identity — that only holds if the file is protected,
//! so it is written 0600 and, on Unix, verified to still be 0600 when read.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::crypto::{KeyPair, b64_decode, b64_encode};

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("{0}")]
    Io(String),
    #[error("state file is malformed: {0}")]
    Malformed(String),
    #[error("state file {0} is readable by other users; fix with: chmod 600 {0}")]
    Permissions(String),
}

type Result<T> = std::result::Result<T, StateError>;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    /// Relay base URL, e.g. `https://hypeterm-relay.example.ts.net`.
    #[serde(default)]
    pub relay_url: String,
    /// Ed25519 seed for the identity key, base64url. Absent until `enroll` runs.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub identity_seed: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub identity_id: String,
    /// Ed25519 seed for this machine's publisher device key.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub device_seed: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub device_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub device_name: String,
    /// Whether, and how, this machine will open a terminal because a paired phone
    /// asked (relay spec §4.6). Off until somebody sitting at this machine turns it on.
    #[serde(default)]
    pub remote_open: RemoteOpenConfig,
}

fn default_max_remote_terminals() -> u32 {
    4
}

/// The machine owner's own policy for phone-initiated terminals.
///
/// Captured explicitly rather than inherited: the daemon's environment is whichever
/// `run` happened to start it, and its working directory is `/`. Neither is what
/// somebody means by "open me a shell".
#[derive(Debug, Serialize, Deserialize)]
pub struct RemoteOpenConfig {
    #[serde(default)]
    pub enabled: bool,
    /// The program and arguments to host, as argv. Never a shell string: nothing on
    /// this path is ever handed to `sh -c`.
    #[serde(default)]
    pub shell: Vec<String>,
    /// Working directory. Empty means `$HOME`.
    #[serde(default)]
    pub cwd: String,
    #[serde(default = "default_max_remote_terminals")]
    pub max_terminals: u32,
}

impl Default for RemoteOpenConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            shell: Vec::new(),
            cwd: String::new(),
            max_terminals: default_max_remote_terminals(),
        }
    }
}

impl State {
    pub fn identity_key(&self) -> Option<KeyPair> {
        KeyPair::from_seed(&b64_decode(&self.identity_seed)?)
    }

    pub fn device_key(&self) -> Option<KeyPair> {
        KeyPair::from_seed(&b64_decode(&self.device_seed)?)
    }

    pub fn set_identity_key(&mut self, key: &KeyPair) {
        self.identity_seed = b64_encode(&key.seed());
        self.identity_id = key.fingerprint();
    }

    pub fn set_device_key(&mut self, key: &KeyPair) {
        self.device_seed = b64_encode(&key.seed());
    }
}

/// Where the state file lives when `--state-file` is not given.
///
/// `HYPETERM_STATE` first, so a service can be pointed elsewhere without a flag.
pub fn default_path() -> PathBuf {
    if let Ok(explicit) = std::env::var("HYPETERM_STATE") {
        if !explicit.trim().is_empty() {
            return PathBuf::from(explicit);
        }
    }
    let base = if cfg!(windows) {
        std::env::var("APPDATA").ok().map(PathBuf::from)
    } else {
        std::env::var("XDG_CONFIG_HOME")
            .ok()
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var("HOME")
                    .ok()
                    .map(|h| PathBuf::from(h).join(".config"))
            })
    };
    base.unwrap_or_else(|| PathBuf::from("."))
        .join("hypeterm")
        .join("publisher.json")
}

pub fn load(path: &Path) -> Result<State> {
    if !path.exists() {
        return Ok(State::default());
    }
    check_permissions(path)?;
    let text = std::fs::read_to_string(path)
        .map_err(|e| StateError::Io(format!("reading {}: {e}", path.display())))?;
    serde_json::from_str(&text).map_err(|e| StateError::Malformed(e.to_string()))
}

pub fn save(path: &Path, state: &State) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| StateError::Io(format!("creating {}: {e}", parent.display())))?;
    }
    let text =
        serde_json::to_string_pretty(state).map_err(|e| StateError::Malformed(e.to_string()))?;

    // Write through a temporary file in the same directory and rename, so an
    // interrupted write cannot leave a half-written key behind.
    let temporary = path.with_extension("tmp");
    write_private(&temporary, text.as_bytes())?;
    std::fs::rename(&temporary, path)
        .map_err(|e| StateError::Io(format!("replacing {}: {e}", path.display())))?;
    Ok(())
}

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| StateError::Io(format!("creating {}: {e}", path.display())))?;
    file.write_all(bytes)
        .map_err(|e| StateError::Io(format!("writing {}: {e}", path.display())))?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    // Windows inherits the parent directory's ACL. Under %APPDATA% that is already
    // limited to the user; there is no mode bit to set.
    std::fs::write(path, bytes)
        .map_err(|e| StateError::Io(format!("writing {}: {e}", path.display())))
}

#[cfg(unix)]
fn check_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let mode = std::fs::metadata(path)
        .map_err(|e| StateError::Io(format!("reading {}: {e}", path.display())))?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        return Err(StateError::Permissions(path.display().to_string()));
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::KeyPair;

    #[test]
    fn keys_survive_a_save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("publisher.json");

        let identity = KeyPair::generate();
        let device = KeyPair::generate();
        let mut state = State::default();
        state.relay_url = "https://relay.example".into();
        state.set_identity_key(&identity);
        state.set_device_key(&device);
        save(&path, &state).unwrap();

        let loaded = load(&path).unwrap();
        assert_eq!(loaded.relay_url, "https://relay.example");
        assert_eq!(
            loaded.identity_key().unwrap().public_key_bytes(),
            identity.public_key_bytes()
        );
        assert_eq!(
            loaded.device_key().unwrap().public_key_bytes(),
            device.public_key_bytes()
        );
        assert_eq!(loaded.identity_id, identity.fingerprint());
    }

    #[test]
    fn a_missing_file_is_an_empty_state_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let state = load(&dir.path().join("absent.json")).unwrap();
        assert!(state.identity_seed.is_empty());
    }

    #[test]
    fn an_existing_state_file_does_not_acquire_remote_open() {
        // The field is new. A machine enrolled before it existed must read as "off":
        // upgrading the binary is not consent to let a phone start shells here.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("publisher.json");
        std::fs::write(
            &path,
            r#"{"relay_url":"https://relay.example","device_id":"d","device_name":"laptop"}"#,
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }

        let stored = load(&path).expect("loads");
        assert!(!stored.remote_open.enabled);
        assert!(stored.remote_open.shell.is_empty());
        assert_eq!(stored.remote_open.max_terminals, 4);
    }

    #[test]
    fn the_remote_open_policy_survives_a_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("publisher.json");
        let mut stored = State::default();
        stored.remote_open.enabled = true;
        stored.remote_open.shell = vec!["/bin/zsh".into(), "-l".into()];
        stored.remote_open.cwd = "/home/someone".into();
        stored.remote_open.max_terminals = 2;
        save(&path, &stored).unwrap();

        let read = load(&path).expect("loads");
        assert!(read.remote_open.enabled);
        assert_eq!(read.remote_open.shell, vec!["/bin/zsh", "-l"]);
        assert_eq!(read.remote_open.cwd, "/home/someone");
        assert_eq!(read.remote_open.max_terminals, 2);
    }

    #[cfg(unix)]
    #[test]
    fn a_world_readable_state_file_is_refused() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("publisher.json");
        save(&path, &State::default()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        // It holds two private keys; refusing is the only safe answer.
        assert!(matches!(load(&path), Err(StateError::Permissions(_))));
    }

    #[cfg(unix)]
    #[test]
    fn a_saved_file_is_private_by_default() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("publisher.json");
        save(&path, &State::default()).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
