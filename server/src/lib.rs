//! Terminal Mirror Relay.
//!
//! A containerised service that accepts terminal output from registered devices and
//! streams it to authenticated clients over WebSockets, keeping a bounded replay
//! window per terminal so a reconnecting client can reconstruct recent output before
//! following the live stream.
//!
//! `spec.md` in the repository root is the normative specification; module docs cite
//! its section numbers where behaviour is prescribed.

pub mod bootstrap;
pub mod crypto;
pub mod db;
pub mod error;
pub mod http;
pub mod metrics;
pub mod observability;
pub mod relay;
pub mod settings;
pub mod util;

pub mod app;
pub mod server;
