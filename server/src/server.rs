//! Listener supervision, TLS, live rebind, and graceful shutdown (spec §8).
//!
//! The listener is supervised rather than simply bound once, because
//! `server.listen_address` and the TLS settings are runtime tunable: a change
//! rebinds the socket and drains the old listener instead of requiring a restart
//! (spec §8.1).

use crate::app::AppState;
use crate::error::{ApiError, ApiResult};
use crate::http::context::ConnMeta;
use crate::metrics;
use crate::relay::flush;
use crate::relay::terminal::Termination;
use crate::settings::Snapshot;
use crate::settings::defs::keys;
use axum::Router;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as ConnBuilder;
use hyper_util::service::TowerToHyperService;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_util::task::TaskTracker;

/// The listener configuration derived from a settings snapshot. A change to any of
/// these fields triggers a rebind.
#[derive(Clone, PartialEq, Eq)]
struct ListenerConfig {
    address: String,
    tls_enabled: bool,
    certificate_path: String,
    private_key_path: String,
}

impl ListenerConfig {
    fn main(snapshot: &Snapshot) -> Self {
        Self {
            address: snapshot.string(keys::SERVER_LISTEN_ADDRESS),
            tls_enabled: snapshot.bool(keys::SERVER_TLS_ENABLED),
            certificate_path: snapshot.string(keys::SERVER_TLS_CERTIFICATE_PATH),
            private_key_path: snapshot.string(keys::SERVER_TLS_PRIVATE_KEY_PATH),
        }
    }

    fn health(snapshot: &Snapshot) -> Self {
        Self {
            address: snapshot.string(keys::SERVER_HEALTH_LISTEN_ADDRESS),
            tls_enabled: false,
            certificate_path: String::new(),
            private_key_path: String::new(),
        }
    }
}

/// Install the process-wide rustls crypto provider once.
pub fn install_crypto_provider() {
    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
}

fn load_tls(config: &ListenerConfig) -> ApiResult<Arc<tokio_rustls::rustls::ServerConfig>> {
    use tokio_rustls::rustls::ServerConfig;

    let cert_file = std::fs::File::open(&config.certificate_path)
        .map_err(|e| ApiError::internal(format!("cannot open {}: {e}", config.certificate_path)))?;
    let mut cert_reader = std::io::BufReader::new(cert_file);
    let certs = rustls_pemfile::certs(&mut cert_reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| ApiError::internal(format!("cannot parse TLS certificate chain: {e}")))?;
    if certs.is_empty() {
        return Err(ApiError::internal(
            "TLS certificate file contains no certificates",
        ));
    }

    let key_file = std::fs::File::open(&config.private_key_path)
        .map_err(|e| ApiError::internal(format!("cannot open {}: {e}", config.private_key_path)))?;
    let mut key_reader = std::io::BufReader::new(key_file);
    let key = rustls_pemfile::private_key(&mut key_reader)
        .map_err(|e| ApiError::internal(format!("cannot parse TLS private key: {e}")))?
        .ok_or_else(|| ApiError::internal("TLS private key file contains no key"))?;

    let mut server_config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| ApiError::internal(format!("invalid TLS key pair: {e}")))?;
    server_config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(server_config))
}

/// Serve one listener generation until told to stop.
async fn accept_loop(
    listener: TcpListener,
    tls: Option<Arc<tokio_rustls::rustls::ServerConfig>>,
    router: Router,
    tracker: TaskTracker,
    mut stop: tokio::sync::watch::Receiver<bool>,
    max_connections: usize,
    handshake_timeout: Duration,
) {
    let acceptor = tls.map(tokio_rustls::TlsAcceptor::from);
    let in_flight = Arc::new(tokio::sync::Semaphore::new(max_connections.max(1)));

    loop {
        tokio::select! {
            _ = stop.changed() => {
                if *stop.borrow() { return; }
            }
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(pair) => pair,
                    Err(e) => {
                        tracing::warn!(event = "accept_failed", error = %e);
                        // Back off briefly so a persistent accept error cannot spin.
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                };

                let Ok(permit) = Arc::clone(&in_flight).try_acquire_owned() else {
                    tracing::warn!(
                        event = "connection_rejected",
                        peer = %peer,
                        "refusing a connection: server.max_concurrent_connections reached"
                    );
                    continue;
                };

                let router = router.clone();
                let acceptor = acceptor.clone();
                tracker.spawn(async move {
                    let _permit = permit;
                    serve_connection(stream, peer, acceptor, router, handshake_timeout).await;
                });
            }
        }
    }
}

async fn serve_connection(
    stream: tokio::net::TcpStream,
    peer: SocketAddr,
    acceptor: Option<tokio_rustls::TlsAcceptor>,
    router: Router,
    handshake_timeout: Duration,
) {
    let _ = stream.set_nodelay(true);
    let tls = acceptor.is_some();

    // Connection facts the request middleware needs for client identification and
    // transport-security enforcement.
    let service = TowerToHyperService::new(tower::service_fn(
        move |mut request: axum::http::Request<hyper::body::Incoming>| {
            request.extensions_mut().insert(ConnMeta { peer, tls });
            let mut router = router.clone();
            async move { tower::Service::call(&mut router, request).await }
        },
    ));

    let builder = ConnBuilder::new(TokioExecutor::new());

    match acceptor {
        Some(acceptor) => {
            let accepted =
                match tokio::time::timeout(handshake_timeout, acceptor.accept(stream)).await {
                    Ok(Ok(stream)) => stream,
                    Ok(Err(e)) => {
                        tracing::debug!(event = "tls_handshake_failed", peer = %peer, error = %e);
                        return;
                    }
                    Err(_) => {
                        tracing::debug!(event = "tls_handshake_timeout", peer = %peer);
                        return;
                    }
                };
            if let Err(e) = builder
                .serve_connection_with_upgrades(TokioIo::new(accepted), service)
                .await
            {
                tracing::debug!(event = "connection_error", peer = %peer, error = %e);
            }
        }
        None => {
            if let Err(e) = builder
                .serve_connection_with_upgrades(TokioIo::new(stream), service)
                .await
            {
                tracing::debug!(event = "connection_error", peer = %peer, error = %e);
            }
        }
    }
}

/// Supervise one logical listener across settings revisions.
async fn supervise(
    state: Arc<AppState>,
    label: &'static str,
    build_config: fn(&Snapshot) -> ListenerConfig,
    build_router: fn(Arc<AppState>) -> Router,
    bound: Option<tokio::sync::oneshot::Sender<SocketAddr>>,
) {
    let mut settings_rx = state.settings.subscribe();
    let mut shutdown_rx = state.shutdown_rx.clone();
    let mut bound = bound;

    loop {
        let snapshot = state.snapshot();
        let config = build_config(&snapshot);

        if config.address.trim().is_empty() {
            // Disabled; wait for a revision that enables it.
            tokio::select! {
                _ = settings_rx.changed() => continue,
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() { return; }
                    continue;
                }
            }
        }

        let tls = if config.tls_enabled {
            match load_tls(&config) {
                Ok(tls) => Some(tls),
                Err(e) => {
                    tracing::error!(
                        event = "tls_load_failed",
                        listener = label,
                        error = %e,
                        "keeping the previous listener; fix the TLS settings and update again"
                    );
                    tokio::select! {
                        _ = settings_rx.changed() => continue,
                        _ = shutdown_rx.changed() => {
                            if *shutdown_rx.borrow() { return; }
                            continue;
                        }
                    }
                }
            }
        } else {
            None
        };

        let listener = match TcpListener::bind(&config.address).await {
            Ok(listener) => listener,
            Err(e) => {
                tracing::error!(
                    event = "bind_failed",
                    listener = label,
                    address = %config.address,
                    error = %e,
                    "cannot bind; retrying on the next settings change"
                );
                tokio::select! {
                    _ = settings_rx.changed() => continue,
                    _ = tokio::time::sleep(Duration::from_secs(5)) => continue,
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() { return; }
                        continue;
                    }
                }
            }
        };

        let local_addr = listener.local_addr().ok();
        tracing::info!(
            event = "listener_bound",
            listener = label,
            address = ?local_addr,
            tls = config.tls_enabled,
            settings_revision = snapshot.revision,
            "listener bound"
        );
        if let (Some(sender), Some(addr)) = (bound.take(), local_addr) {
            let _ = sender.send(addr);
        }

        let tracker = TaskTracker::new();
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let accept = tokio::spawn(accept_loop(
            listener,
            tls,
            build_router(Arc::clone(&state)),
            tracker.clone(),
            stop_rx,
            snapshot.usize(keys::SERVER_MAX_CONCURRENT_CONNECTIONS),
            snapshot.duration_secs(keys::WEBSOCKET_HANDSHAKE_TIMEOUT_SECONDS),
        ));

        // Wait for a configuration change that affects this listener, or shutdown.
        let rebind = loop {
            tokio::select! {
                changed = settings_rx.changed() => {
                    if changed.is_err() { break false }
                    let fresh = build_config(&state.snapshot());
                    if fresh != config {
                        break true;
                    }
                }
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() { break false }
                }
            }
        };

        let drain = state
            .snapshot()
            .duration_secs(keys::SERVER_CONNECTION_DRAIN_SECONDS);
        let _ = stop_tx.send(true);
        accept.abort();
        tracker.close();

        if tokio::time::timeout(drain, tracker.wait()).await.is_err() {
            tracing::warn!(
                event = "listener_drain_timeout",
                listener = label,
                drain_seconds = drain.as_secs(),
                "connections were still open when the drain deadline passed"
            );
        }

        if !rebind {
            tracing::info!(
                event = "listener_stopped",
                listener = label,
                "listener stopped"
            );
            return;
        }

        metrics::LISTENER_REBINDS.inc();
        tracing::info!(
            event = "listener_rebinding",
            listener = label,
            "listener configuration changed; rebinding"
        );
    }
}

/// Run the service until a termination signal, then shut down gracefully.
pub async fn run(state: Arc<AppState>) -> ApiResult<()> {
    install_crypto_provider();
    state.start_background().await?;

    let main = tokio::spawn(supervise(
        Arc::clone(&state),
        "main",
        ListenerConfig::main,
        crate::http::router,
        None,
    ));
    let health = tokio::spawn(supervise(
        Arc::clone(&state),
        "health",
        ListenerConfig::health,
        crate::http::health_router,
        None,
    ));

    wait_for_signal().await;
    shutdown(&state).await;

    let _ = main.await;
    let _ = health.await;
    Ok(())
}

/// Start listeners without taking over signal handling, and report the bound main
/// address. Used by integration tests.
pub async fn spawn_for_test(state: Arc<AppState>) -> ApiResult<SocketAddr> {
    install_crypto_provider();
    state.start_background().await?;

    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(supervise(
        Arc::clone(&state),
        "main",
        ListenerConfig::main,
        crate::http::router,
        Some(tx),
    ));
    tokio::spawn(supervise(
        Arc::clone(&state),
        "health",
        ListenerConfig::health,
        crate::http::health_router,
        None,
    ));

    rx.await
        .map_err(|_| ApiError::internal("listener failed to bind"))
}

/// Stop accepting, tell peers, finish committed writes, and close inside the
/// deadline (spec §8, §10).
pub async fn shutdown(state: &Arc<AppState>) {
    let deadline = state
        .snapshot()
        .duration_secs(keys::SERVER_SHUTDOWN_DEADLINE_SECONDS);
    tracing::info!(
        event = "shutdown_started",
        deadline_seconds = deadline.as_secs(),
        "graceful shutdown started"
    );

    state.registry.begin_shutdown();
    let _ = state.shutdown_tx.send(true);

    let result = tokio::time::timeout(deadline, async {
        // The checkpoint task performs a final flush when it sees the shutdown
        // signal; this second pass covers anything that arrived while it ran.
        flush::flush_once(&state.registry, flush::FlushTrigger::Shutdown).await;

        // Give connections a moment to observe the notice before they are dropped.
        tokio::time::sleep(Duration::from_millis(250)).await;
        state.registry.terminate_all(Termination::server_shutdown());

        flush::flush_once(&state.registry, flush::FlushTrigger::Shutdown).await;
    })
    .await;

    if result.is_err() {
        tracing::error!(
            event = "shutdown_deadline_exceeded",
            "shutdown deadline passed before draining finished"
        );
    }

    let dirty = state.registry.dirty_stats().total_dirty_bytes;
    if dirty > 0 {
        tracing::error!(
            event = "shutdown_dirty_bytes",
            dirty,
            "output bytes were still uncommitted at shutdown; publishers will retransmit them"
        );
    }

    tracing::info!(event = "shutdown_complete", "graceful shutdown complete");
}

#[cfg(unix)]
async fn wait_for_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(event = "signal_setup_failed", error = %e);
            return;
        }
    };
    let mut sigint = match signal(SignalKind::interrupt()) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(event = "signal_setup_failed", error = %e);
            return;
        }
    };
    tokio::select! {
        _ = sigterm.recv() => tracing::info!(event = "signal_received", signal = "SIGTERM"),
        _ = sigint.recv() => tracing::info!(event = "signal_received", signal = "SIGINT"),
    }
}

#[cfg(not(unix))]
async fn wait_for_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!(event = "signal_received", signal = "ctrl-c");
}
