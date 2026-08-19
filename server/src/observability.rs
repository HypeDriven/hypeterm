//! Structured logging (spec §9).
//!
//! Level and encoding are database settings, so both are switchable at runtime. Two
//! formatting layers are installed and a runtime flag decides which one emits, since
//! a formatter cannot be swapped in place once the subscriber is built.

use crate::settings::Snapshot;
use crate::settings::defs::keys;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, filter::FilterFn, reload};

static JSON_ENABLED: AtomicBool = AtomicBool::new(true);
static FILTER_HANDLE: OnceLock<reload::Handle<EnvFilter, ReloadTarget>> = OnceLock::new();

// The reload handle needs the concrete subscriber type; this alias keeps the
// signature readable.
type ReloadTarget = tracing_subscriber::Registry;

/// Install the logging subscriber. Safe to call once per process.
pub fn init(level: &str, json: bool) {
    JSON_ENABLED.store(json, Ordering::Release);

    let (filter, handle) = reload::Layer::new(env_filter(level));
    let _ = FILTER_HANDLE.set(handle);

    let json_layer = tracing_subscriber::fmt::layer()
        .json()
        .with_current_span(false)
        .with_span_list(false)
        .with_filter(FilterFn::new(|_| JSON_ENABLED.load(Ordering::Acquire)));

    let text_layer = tracing_subscriber::fmt::layer()
        .with_target(true)
        .with_filter(FilterFn::new(|_| !JSON_ENABLED.load(Ordering::Acquire)));

    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(json_layer)
        .with(text_layer)
        .try_init();
}

/// Apply a new settings revision to the logging subsystem.
pub fn apply_log_settings(snapshot: &Snapshot) {
    let level = snapshot.string(keys::LOGGING_LEVEL);
    let json = snapshot.string(keys::LOGGING_FORMAT) == "json";
    JSON_ENABLED.store(json, Ordering::Release);
    if let Some(handle) = FILTER_HANDLE.get() {
        let _ = handle.reload(env_filter(&level));
    }
}

fn env_filter(level: &str) -> EnvFilter {
    // Quiet the noisier dependencies by default; the service's own level is what the
    // operator setting controls.
    EnvFilter::new(format!("{level},hyper=warn,rustls=warn,h2=warn,tower=warn"))
}
