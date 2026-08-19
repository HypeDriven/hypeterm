//! Publishes a terminal session to a Terminal Mirror Relay.
//!
//! The relay conveys an append-only stream of terminal bytes and hands it to
//! subscribers; this is the other end — the thing that owns a pseudo-terminal, sends
//! what comes out of it, and writes back what a subscriber types.
//!
//! `../server/spec.md` is normative for everything protocol-shaped. Where behaviour is
//! prescribed, the module docs cite its section.

pub mod api;
pub mod crypto;
/// The multiplexing daemon. Unix only: see its module documentation for why.
#[cfg(unix)]
pub mod daemon;
pub mod ipc;
pub mod pairing;
pub mod protocol;
pub mod pty;
pub mod publish;
pub mod session;
pub mod state;
pub mod stream;
pub mod tls;
