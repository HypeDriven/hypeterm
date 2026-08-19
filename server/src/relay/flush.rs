//! The checkpoint task: memory-first buffering with infrequent, batched database
//! transactions (spec §7.2).
//!
//! Accepting an output frame never writes to the database. Dirty output from many
//! frames and many terminals is coalesced into one transaction, triggered by
//! whichever comes first: the flush interval elapsing since the oldest dirty byte,
//! the dirty-byte threshold, a terminal closing, graceful shutdown, memory pressure,
//! or an explicit operator request.

use super::registry::Registry;
use super::terminal::PendingCheckpoint;
use crate::db::{in_txn, repo};
use crate::error::ApiResult;
use crate::metrics;
use crate::settings::defs::keys;
use std::sync::Arc;
use std::time::Duration;

/// Why a checkpoint ran. Recorded in logs so operators can see which threshold is
/// actually driving their write rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlushTrigger {
    Interval,
    DirtyBytes,
    MemoryPressure,
    TerminalClosing,
    Requested,
    Shutdown,
}

impl FlushTrigger {
    fn as_str(&self) -> &'static str {
        match self {
            FlushTrigger::Interval => "interval",
            FlushTrigger::DirtyBytes => "dirty_bytes",
            FlushTrigger::MemoryPressure => "memory_pressure",
            FlushTrigger::TerminalClosing => "terminal_closing",
            FlushTrigger::Requested => "requested",
            FlushTrigger::Shutdown => "shutdown",
        }
    }
}

pub async fn run(registry: Arc<Registry>, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    let mut settings_rx = registry.settings().subscribe();
    loop {
        let snapshot = registry.settings().snapshot();
        let flush_interval = snapshot.duration_ms(keys::PERSISTENCE_FLUSH_INTERVAL_MS);
        let flush_bytes = snapshot.u64(keys::PERSISTENCE_FLUSH_BYTES);
        let memory_pressure = snapshot.u64(keys::PERSISTENCE_MEMORY_PRESSURE_DIRTY_BYTES);

        let stats = registry.dirty_stats();
        let trigger = if stats.has_pending_close {
            Some(FlushTrigger::TerminalClosing)
        } else if stats.total_dirty_bytes >= memory_pressure {
            Some(FlushTrigger::MemoryPressure)
        } else if stats.total_dirty_bytes >= flush_bytes {
            Some(FlushTrigger::DirtyBytes)
        } else if stats
            .oldest_dirty
            .map(|age| age >= flush_interval)
            .unwrap_or(false)
        {
            Some(FlushTrigger::Interval)
        } else {
            None
        };

        if let Some(trigger) = trigger {
            flush_once(&registry, trigger).await;
            continue;
        }

        // Sleep only until the oldest dirty byte would age out.
        let wait = match stats.oldest_dirty {
            Some(age) => flush_interval
                .saturating_sub(age)
                .max(Duration::from_millis(1)),
            None => flush_interval,
        };

        tokio::select! {
            _ = registry.wait_flush_request() => {
                flush_once(&registry, FlushTrigger::Requested).await;
            }
            _ = tokio::time::sleep(wait) => {}
            // A new flush interval or byte threshold must take effect promptly, not
            // after the sleep computed from the previous values.
            changed = settings_rx.changed() => {
                if changed.is_err() { return; }
            }
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    // Graceful shutdown: drain everything still dirty before exiting.
                    flush_once(&registry, FlushTrigger::Shutdown).await;
                    return;
                }
            }
        }
    }
}

/// Run one checkpoint. Serialised by the registry flush lock so chunk ranges for a
/// terminal can never overlap.
pub async fn flush_once(registry: &Arc<Registry>, trigger: FlushTrigger) -> u64 {
    let _guard = registry.flush_lock.lock().await;

    let pending: Vec<PendingCheckpoint> = registry
        .resident_handles()
        .into_iter()
        .filter_map(|handle| handle.take_checkpoint())
        .collect();

    if pending.is_empty() {
        return 0;
    }

    let batch: Vec<repo::TerminalCheckpoint> = pending
        .iter()
        .map(|p| repo::TerminalCheckpoint {
            terminal_id: p.terminal_id,
            chunk_start: p.chunk_start,
            chunk: p.chunk.clone(),
            earliest_offset: p.earliest_offset,
            durable_offset: p.durable_target,
            last_activity: p.last_activity,
            close_reason: p.close_reason.clone(),
        })
        .collect();

    let batch_bytes: u64 = batch.iter().map(|c| c.chunk.len() as u64).sum();
    let frames: u64 = pending.iter().map(|p| p.frames).sum();
    let oldest_age = pending
        .iter()
        .map(|p| p.dirty_age_seconds)
        .fold(0.0f64, f64::max);
    let terminals = batch.len();

    match commit_with_retry(registry, batch).await {
        Ok(rows) => {
            // Only after the transaction commits may durable_offset advance and
            // acknowledgements be sent (spec §7.2).
            for checkpoint in pending {
                if let Some(handle) = registry.resident(checkpoint.terminal_id) {
                    handle
                        .commit_durable(checkpoint.durable_target, checkpoint.close_reason.clone());
                    if checkpoint.close_reason.is_some() {
                        metrics::TERMINALS_OPEN.dec();
                    }
                }
            }

            registry.set_storage_failing(false);
            metrics::CHECKPOINT_TRANSACTIONS.inc();
            metrics::CHECKPOINT_ROWS_WRITTEN.add(rows);
            metrics::CHECKPOINT_FRAMES_COALESCED.add(frames);
            metrics::CHECKPOINT_BATCH_BYTES.observe(batch_bytes as f64);
            metrics::CHECKPOINT_BATCH_AGE_SECONDS.observe(oldest_age);
            metrics::CHECKPOINT_TERMINALS_PER_BATCH.observe(terminals as f64);
            metrics::STORAGE_BYTES.set(registry.db().storage_bytes() as i64);

            tracing::debug!(
                event = "checkpoint_committed",
                trigger = trigger.as_str(),
                terminals,
                bytes = batch_bytes,
                frames,
                rows,
                "committed a terminal output checkpoint"
            );
            batch_bytes
        }
        Err(e) => {
            // Dirty bytes stay in memory and are retried; no acknowledgement is sent,
            // so a false durable acknowledgement is impossible (spec §7.2).
            metrics::CHECKPOINT_FAILURES.inc();
            metrics::STORAGE_ERRORS.inc();
            registry.set_storage_failing(true);
            tracing::error!(
                event = "checkpoint_failed",
                trigger = trigger.as_str(),
                terminals,
                bytes = batch_bytes,
                error = %e,
                "checkpoint transaction failed; output stays in memory and will be retried"
            );
            0
        }
    }
}

async fn commit_with_retry(
    registry: &Arc<Registry>,
    batch: Vec<repo::TerminalCheckpoint>,
) -> ApiResult<u64> {
    let snapshot = registry.settings().snapshot();
    let max_attempts = snapshot
        .u32(keys::PERSISTENCE_COMMIT_RETRY_MAX_ATTEMPTS)
        .max(1);
    let initial = snapshot.duration_ms(keys::PERSISTENCE_COMMIT_RETRY_INITIAL_MS);
    let max_backoff = snapshot.duration_ms(keys::PERSISTENCE_COMMIT_RETRY_MAX_MS);

    let mut backoff = initial;
    let mut last_error = None;

    for attempt in 1..=max_attempts {
        let batch = clone_batch(&batch);
        let db = registry.db().clone();
        let result = db
            .call(move |conn| in_txn(conn, |txn| repo::commit_checkpoints(txn, &batch)))
            .await;

        match result {
            Ok(rows) => return Ok(rows),
            Err(e) => {
                tracing::warn!(
                    event = "checkpoint_attempt_failed",
                    attempt,
                    max_attempts,
                    error = %e,
                    "retrying checkpoint commit"
                );
                last_error = Some(e);
                if attempt < max_attempts {
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(max_backoff);
                }
            }
        }
    }

    Err(last_error
        .unwrap_or_else(|| crate::error::ApiError::storage_unavailable("checkpoint commit failed")))
}

fn clone_batch(batch: &[repo::TerminalCheckpoint]) -> Vec<repo::TerminalCheckpoint> {
    batch
        .iter()
        .map(|c| repo::TerminalCheckpoint {
            terminal_id: c.terminal_id,
            chunk_start: c.chunk_start,
            chunk: c.chunk.clone(),
            earliest_offset: c.earliest_offset,
            durable_offset: c.durable_offset,
            last_activity: c.last_activity,
            close_reason: c.close_reason.clone(),
        })
        .collect()
}
