//! The local protocol between `hypeterm-publish run` and the multiplexing daemon.
//!
//! A device may hold only one publisher connection to the relay (spec §6.1), so on a
//! machine with several mirrored terminals exactly one process may own it. That
//! process is the daemon; every `run` hands it one terminal over a socket here.
//!
//! **One connection carries exactly one terminal, for its whole life.** The connection
//! is the terminal's identity, so nothing on this wire needs routing — no terminal id,
//! no per-terminal queues, no head-of-line blocking to design around. The daemon
//! already has all of that once, on the relay side, and once is enough.
//!
//! What does cross the wire is the terminal's *offset*, because the bytes that offset
//! refers to are retained in `run` (see `crate::stream`). The relay accepts a frame
//! only when its start offset matches, so whoever holds the unacknowledged bytes must
//! be the one that decides where the stream resumes. Keeping that in `run` is what
//! makes a daemon restart an interruption rather than a hole.

use serde::{Deserialize, Serialize};

/// Incremented only for a change that an older peer could misread. Unknown control
/// message types and unknown fields are ignored on both sides, so almost everything is
/// additive and this stays put (spec §12 applies the same rule to the relay protocol).
///
/// 2: `Open` carries `in_reply_to` (relay spec §4.6). Additive on the wire, but an older
/// daemon does not merely ignore it — it hosts the shell and never answers the
/// subscriber that asked, so the phone waits out the relay's timeout while a terminal
/// quietly appears on this machine. Failing the handshake with a message that names the
/// fix is much better than that, and is why this is not treated as additive.
pub const IPC_VERSION: u32 = 2;

/// `0x01 | length (u32 BE) | UTF-8 JSON`.
pub const FRAME_CONTROL: u8 = 0x01;
/// `0x02 | length (u32 BE) | start offset (u64 BE) | opaque pty bytes`. Client to daemon.
pub const FRAME_OUTPUT: u8 = 0x02;
/// `0x03 | length (u32 BE) | opaque keystrokes`. Daemon to client.
pub const FRAME_INPUT: u8 = 0x03;

/// Bigger than any pseudo-terminal read (`pty::READ_CHUNK` is 16 KiB) and any relay
/// frame limit, so a length beyond it is a corrupt or hostile peer rather than a large
/// one.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("the peer sent a frame of {0} bytes, beyond the {MAX_FRAME_BYTES} limit")]
    TooLarge(usize),
    #[error("the peer sent an unknown frame type 0x{0:02x}")]
    UnknownFrame(u8),
    #[error("the peer sent a frame in the wrong direction: 0x{0:02x}")]
    WrongDirection(u8),
    #[error("the peer sent a control message that is not a JSON object: {0}")]
    Malformed(String),
}

type Result<T> = std::result::Result<T, IpcError>;

/// What `run` says to the daemon.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FromClient {
    /// Always the first message on a connection.
    Hello {
        ipc_version: u32,
        build: String,
        pid: u32,
    },
    /// Exactly once, after `hello_ok`.
    Open {
        /// Minted fresh per hosted shell and never derived from anything reusable:
        /// the relay deduplicates opens by (device, local_ref), so two shells sharing
        /// one would be spliced onto a single offset stream and interleave.
        local_ref: String,
        label: String,
        cols: u16,
        rows: u16,
        term: String,
        /// The subscriber request this shell answers, if it answers one (relay spec
        /// §4.6). It has to travel over IPC because the daemon, not this process, owns
        /// the relay connection that will carry the answer.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        in_reply_to: Option<String>,
    },
    /// The authoritative size, which only the publisher may declare (spec §6.5).
    Resize {
        cols: u16,
        rows: u16,
    },
    Close {
        reason: String,
    },
    /// Asks a daemon of a different build to stand down so a matching one can start.
    Retire,
    #[serde(other)]
    Unrecognised,
}

/// What the daemon says to `run`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FromDaemon {
    HelloOk {
        ipc_version: u32,
        build: String,
        pid: u32,
        /// Attached clients *other than* this one, which is what decides whether a
        /// mismatched build may ask the daemon to stand down.
        clients: u32,
    },
    /// The relay has the terminal open and its stream continues at `next_offset`.
    /// Sent again after every relay reconnect, because the relay is the authority on
    /// where the stream is and its answer can move back to whatever it committed.
    Attached {
        terminal_id: String,
        next_offset: u64,
        max_output_frame_bytes: u64,
        max_unacked_output_bytes: u64,
    },
    /// Bytes the relay has committed; everything below this can be forgotten.
    Ack {
        durable_offset: u64,
    },
    /// The relay refused a frame and says the stream is here instead.
    Mismatch {
        next_offset: u64,
    },
    /// The relay connection is gone. Nothing may be sent until the next `attached`.
    Detached,
    /// A size a subscriber would like. The publisher decides (spec §6.5); the daemon
    /// never answers one itself, because only `run` knows whether a window is attached.
    ResizeRequest {
        cols: u16,
        rows: u16,
    },
    /// This terminal is no longer mirrored. The daemon closes the connection after.
    Ended {
        reason: String,
    },
    Error {
        code: String,
        message: String,
    },
    #[serde(other)]
    Unrecognised,
}

pub mod code {
    pub const IPC_VERSION_MISMATCH: &str = "ipc_version_mismatch";
    pub const PROTOCOL_ERROR: &str = "protocol_error";
    pub const DUPLICATE_LOCAL_REF: &str = "duplicate_local_ref";
    pub const RETIRE_REFUSED: &str = "retire_refused";
}

/// One decoded frame.
#[derive(Debug, PartialEq)]
pub enum Frame {
    Control(Vec<u8>),
    Output { start_offset: u64, bytes: Vec<u8> },
    Input(Vec<u8>),
}

// ------------------------------------------------------------------- framing

pub fn encode_control<T: Serialize>(message: &T) -> Vec<u8> {
    let json = serde_json::to_vec(message).unwrap_or_else(|_| b"{}".to_vec());
    let mut out = Vec::with_capacity(5 + json.len());
    out.push(FRAME_CONTROL);
    out.extend_from_slice(&(json.len() as u32).to_be_bytes());
    out.extend_from_slice(&json);
    out
}

pub fn encode_output(start_offset: u64, bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(13 + bytes.len());
    out.push(FRAME_OUTPUT);
    out.extend_from_slice(&((bytes.len() + 8) as u32).to_be_bytes());
    out.extend_from_slice(&start_offset.to_be_bytes());
    out.extend_from_slice(bytes);
    out
}

pub fn encode_input(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + bytes.len());
    out.push(FRAME_INPUT);
    out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(bytes);
    out
}

/// Reads one frame. `expect_output` is true on the daemon's side of a connection and
/// false on the client's: a frame travelling the wrong way is a confused peer, and
/// treating it as data would mean feeding keystrokes into an output stream.
pub async fn read_frame<R>(reader: &mut R, expect_output: bool) -> Result<Frame>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt as _;

    let kind = reader.read_u8().await?;
    let length = reader.read_u32().await? as usize;
    if length > MAX_FRAME_BYTES {
        return Err(IpcError::TooLarge(length));
    }
    let mut payload = vec![0u8; length];
    reader.read_exact(&mut payload).await?;

    match kind {
        FRAME_CONTROL => Ok(Frame::Control(payload)),
        FRAME_OUTPUT if expect_output => {
            if payload.len() < 8 {
                return Err(IpcError::Malformed("an output frame with no offset".into()));
            }
            let mut offset = [0u8; 8];
            offset.copy_from_slice(&payload[..8]);
            Ok(Frame::Output {
                start_offset: u64::from_be_bytes(offset),
                bytes: payload[8..].to_vec(),
            })
        }
        FRAME_INPUT if !expect_output => Ok(Frame::Input(payload)),
        FRAME_OUTPUT | FRAME_INPUT => Err(IpcError::WrongDirection(kind)),
        other => Err(IpcError::UnknownFrame(other)),
    }
}

pub fn parse_control<T: for<'de> Deserialize<'de>>(payload: &[u8]) -> Result<T> {
    serde_json::from_slice(payload).map_err(|e| IpcError::Malformed(e.to_string()))
}

// ------------------------------------------------------------------- where it lives

/// Names the socket, the lock and the log for one (relay, device) pair.
///
/// Per device, not per user: `--state-file` and `$HYPETERM_STATE` mean two `run`
/// processes on one machine can legitimately be two different enrolled devices, and a
/// shared socket would publish one device's terminal under the other's identity. This
/// tuple is exactly the scope of the relay's one-connection-per-device rule, so the
/// local boundary and the relay's coincide by construction.
pub fn key(relay_url: &str, device_id: &str) -> String {
    use sha2::{Digest as _, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(relay_url.as_bytes());
    hasher.update([0u8]);
    hasher.update(device_id.as_bytes());
    let digest = hasher.finalize();
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Debug, Clone)]
pub struct Paths {
    pub socket: std::path::PathBuf,
    pub lock: std::path::PathBuf,
    pub log: std::path::PathBuf,
}

/// `sun_path` is 108 bytes including its terminator; refusing early gives a legible
/// message instead of a bind that truncates or fails obscurely.
const MAX_SOCKET_PATH: usize = 100;

impl Paths {
    pub fn for_device(relay_url: &str, device_id: &str) -> std::result::Result<Self, String> {
        Self::in_dir(&runtime_dir()?, relay_url, device_id)
    }

    /// The same names, under a directory the caller has chosen. Lets a test give a
    /// daemon a runtime directory of its own rather than the user's.
    pub fn in_dir(
        dir: &std::path::Path,
        relay_url: &str,
        device_id: &str,
    ) -> std::result::Result<Self, String> {
        let key = key(relay_url, device_id);
        let socket = dir.join(format!("{key}.sock"));
        if socket.as_os_str().len() > MAX_SOCKET_PATH {
            return Err(format!(
                "the socket path {} is too long for a unix socket; set XDG_RUNTIME_DIR \
                 to somewhere shorter",
                socket.display()
            ));
        }
        Ok(Self {
            socket,
            lock: dir.join(format!("{key}.lock")),
            log: dir.join(format!("{key}.log")),
        })
    }
}

/// A private, user-owned directory for the socket.
///
/// Never `/tmp`: another local user can pre-create a path there. Never the config
/// directory: it gets synced and backed up, and sockets do not work over NFS. The last
/// candidate matters most in practice — WSL only sets `XDG_RUNTIME_DIR` when systemd
/// is enabled, which it is not by default.
pub fn runtime_dir() -> std::result::Result<std::path::PathBuf, String> {
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(explicit) = std::env::var("XDG_RUNTIME_DIR")
        && !explicit.trim().is_empty()
    {
        candidates.push(std::path::PathBuf::from(explicit));
    }
    #[cfg(unix)]
    candidates.push(std::path::PathBuf::from(format!("/run/user/{}", unsafe {
        libc::geteuid()
    })));
    if let Ok(home) = std::env::var("HOME") {
        candidates.push(
            std::path::PathBuf::from(home)
                .join(".local")
                .join("state")
                .join("hypeterm")
                .join("run"),
        );
    }

    let mut last = String::from("no candidate directory");
    for candidate in candidates {
        if !candidate.exists() {
            // Only the last candidate is ours to create; the others belong to the
            // system and their absence means this is not that kind of system.
            if candidate.ends_with("hypeterm/run") {
                if let Err(error) = std::fs::create_dir_all(&candidate) {
                    last = format!("creating {}: {error}", candidate.display());
                    continue;
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    let _ = std::fs::set_permissions(
                        &candidate,
                        std::fs::Permissions::from_mode(0o700),
                    );
                }
            } else {
                continue;
            }
        }
        match private_directory(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) => last = error,
        }
    }
    Err(format!("no private runtime directory is available: {last}"))
}

#[cfg(unix)]
fn private_directory(path: &std::path::Path) -> std::result::Result<(), String> {
    use std::os::unix::fs::MetadataExt as _;
    use std::os::unix::fs::PermissionsExt as _;
    // symlink_metadata, not metadata: a symlink planted here would otherwise point the
    // socket — and the lock that arbitrates who owns the device — somewhere else.
    let meta =
        std::fs::symlink_metadata(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    if !meta.is_dir() {
        return Err(format!("{} is not a directory", path.display()));
    }
    if meta.uid() != unsafe { libc::geteuid() } {
        return Err(format!("{} is not owned by this user", path.display()));
    }
    if meta.permissions().mode() & 0o077 != 0 {
        return Err(format!(
            "{} is accessible to other users; fix with: chmod 700 {}",
            path.display(),
            path.display()
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn private_directory(_path: &std::path::Path) -> std::result::Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_control_frame_round_trips() {
        let message = FromClient::Hello {
            ipc_version: IPC_VERSION,
            build: "0.1.0".into(),
            pid: 42,
        };
        let encoded = encode_control(&message);
        assert_eq!(encoded[0], FRAME_CONTROL);
        let length = u32::from_be_bytes(encoded[1..5].try_into().unwrap()) as usize;
        assert_eq!(length, encoded.len() - 5);
        let decoded: FromClient = parse_control(&encoded[5..]).unwrap();
        assert_eq!(decoded, message);
    }

    #[test]
    fn an_output_frame_carries_its_offset() {
        let encoded = encode_output(4096, b"hi");
        assert_eq!(encoded[0], FRAME_OUTPUT);
        assert_eq!(u64::from_be_bytes(encoded[5..13].try_into().unwrap()), 4096);
        assert_eq!(&encoded[13..], b"hi");
    }

    #[tokio::test]
    async fn frames_are_read_back_exactly_as_written() {
        let mut buffer: Vec<u8> = Vec::new();
        buffer.extend(encode_control(&FromClient::Resize { cols: 80, rows: 24 }));
        buffer.extend(encode_output(7, b"abc"));
        let mut cursor = std::io::Cursor::new(buffer);

        match read_frame(&mut cursor, true).await.unwrap() {
            Frame::Control(payload) => {
                let message: FromClient = parse_control(&payload).unwrap();
                assert_eq!(message, FromClient::Resize { cols: 80, rows: 24 });
            }
            other => panic!("expected control, got {other:?}"),
        }
        assert_eq!(
            read_frame(&mut cursor, true).await.unwrap(),
            Frame::Output {
                start_offset: 7,
                bytes: b"abc".to_vec()
            }
        );
    }

    #[tokio::test]
    async fn a_frame_travelling_the_wrong_way_is_refused() {
        // Feeding keystrokes into an output stream, or output into a keyboard, is
        // worse than closing the connection.
        let mut cursor = std::io::Cursor::new(encode_input(b"ls"));
        assert!(matches!(
            read_frame(&mut cursor, true).await,
            Err(IpcError::WrongDirection(FRAME_INPUT))
        ));

        let mut cursor = std::io::Cursor::new(encode_output(0, b"x"));
        assert!(matches!(
            read_frame(&mut cursor, false).await,
            Err(IpcError::WrongDirection(FRAME_OUTPUT))
        ));
    }

    #[tokio::test]
    async fn an_absurd_length_is_refused_before_it_is_allocated() {
        let mut frame = vec![FRAME_CONTROL];
        frame.extend_from_slice(&u32::MAX.to_be_bytes());
        let mut cursor = std::io::Cursor::new(frame);
        assert!(matches!(
            read_frame(&mut cursor, true).await,
            Err(IpcError::TooLarge(_))
        ));
    }

    #[test]
    fn an_unknown_control_message_is_ignored_rather_than_fatal() {
        // The relay protocol holds to this too (spec §12): a peer that fell over on an
        // unrecognised message would break on every upgrade.
        let decoded: FromDaemon = parse_control(br#"{"type":"teleport","x":1}"#).unwrap();
        assert_eq!(decoded, FromDaemon::Unrecognised);
    }

    #[test]
    fn an_unknown_field_does_not_stop_a_known_message_being_read() {
        let decoded: FromDaemon =
            parse_control(br#"{"type":"ack","durable_offset":9,"future":true}"#).unwrap();
        assert_eq!(decoded, FromDaemon::Ack { durable_offset: 9 });
    }

    #[test]
    fn the_key_separates_devices_and_relays() {
        let a = key("https://relay.example", "device-a");
        let b = key("https://relay.example", "device-b");
        let c = key("https://other.example", "device-a");
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_eq!(a, key("https://relay.example", "device-a"));
        assert_eq!(a.len(), 16);
        // The separator is what stops ("ab", "c") and ("a", "bc") colliding.
        assert_ne!(key("ab", "c"), key("a", "bc"));
    }
}
