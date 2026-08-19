//! Terminal Mirror Relay entry point.

use terminal_relay::settings::defs::keys;
use terminal_relay::{app::AppState, bootstrap::Bootstrap, observability, server};

fn main() -> std::process::ExitCode {
    // A container health check that needs no shell, curl or database access, so it
    // works in a distroless image with a read-only root filesystem.
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("healthcheck") {
        let target = args
            .get(2)
            .cloned()
            .unwrap_or_else(|| "127.0.0.1:8080".to_string());
        let path = args
            .get(3)
            .cloned()
            .unwrap_or_else(|| "/healthz".to_string());
        return match probe_health(&target, &path) {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("health check failed: {e}");
                std::process::ExitCode::FAILURE
            }
        };
    }

    // Offline operator access to the settings table.
    //
    // Settings are only reachable over the admin API, which needs a listener the
    // caller can reach — and a secure-by-default deployment has none until TLS is
    // configured, which is itself a setting. This subcommand breaks that circle
    // without weakening it: it opens the database directly, applies the same
    // validation, revision and audit rules as `PATCH /v1/admin/settings`, and is
    // available only to whoever can already read the database file.
    if args.get(1).map(String::as_str) == Some("settings") {
        observability::init("warn", false);
        return match settings_command(&args[2..]) {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{e}");
                std::process::ExitCode::FAILURE
            }
        };
    }

    if matches!(
        args.get(1).map(String::as_str),
        Some("--help" | "-h" | "help")
    ) {
        print_usage();
        return std::process::ExitCode::SUCCESS;
    }

    // Logging starts at a safe default so bootstrap failures are visible; the
    // database settings take over as soon as they load.
    observability::init("info", true);

    let bootstrap = match Bootstrap::from_env() {
        Ok(bootstrap) => bootstrap,
        Err(e) => {
            tracing::error!(event = "bootstrap_failed", error = %e, "cannot start");
            return std::process::ExitCode::FAILURE;
        }
    };

    tracing::info!(
        event = "starting",
        instance_id = %bootstrap.instance_id,
        db_path = %bootstrap.db_path.display(),
        recovery_mode = bootstrap.recovery_mode,
        version = env!("CARGO_PKG_VERSION"),
        "terminal mirror relay starting"
    );

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            tracing::error!(event = "runtime_failed", error = %e, "cannot build the async runtime");
            return std::process::ExitCode::FAILURE;
        }
    };

    runtime.block_on(async move {
        let recovery_mode = bootstrap.recovery_mode;
        let recovery_listen = bootstrap.recovery_listen.clone();

        let state = match AppState::new(bootstrap) {
            Ok(state) => state,
            Err(e) => {
                tracing::error!(event = "startup_failed", error = %e, "cannot initialise state");
                return std::process::ExitCode::FAILURE;
            }
        };

        // Apply the stored logging settings now that they are available.
        observability::apply_log_settings(&state.snapshot());

        if recovery_mode {
            // Emergency mode: ignore the stored listen and TLS settings so an
            // operator can repair a revision that made the service unreachable.
            tracing::warn!(
                event = "recovery_mode",
                listen = %recovery_listen,
                "starting in recovery mode: stored listen and TLS settings are bypassed"
            );
            if let Err(e) = recovery_serve(state, &recovery_listen).await {
                tracing::error!(event = "recovery_failed", error = %e);
                return std::process::ExitCode::FAILURE;
            }
            return std::process::ExitCode::SUCCESS;
        }

        let snapshot = state.snapshot();
        tracing::info!(
            event = "settings_loaded",
            revision = snapshot.revision,
            listen = %snapshot.string(keys::SERVER_LISTEN_ADDRESS),
            tls = snapshot.bool(keys::SERVER_TLS_ENABLED),
            replay_capacity_bytes = snapshot.replay_capacity(),
            "loaded runtime settings from the database"
        );

        match server::run(state).await {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(e) => {
                tracing::error!(event = "server_failed", error = %e, "server stopped with an error");
                std::process::ExitCode::FAILURE
            }
        }
    })
}

fn print_usage() {
    println!(
        "terminal-relay {}

USAGE:
    terminal-relay                          Run the relay.
    terminal-relay settings get [NAME...]   Print settings; secrets are redacted.
    terminal-relay settings set NAME=VALUE  Apply settings atomically.
    terminal-relay healthcheck [ADDR PATH]  Probe a running instance.

Settings are stored in the database, not the environment. `settings set` applies the
same validation, revision and audit rules as PATCH /v1/admin/settings, and exists so a
deployment can be configured before its API is reachable.

Values are parsed as JSON when possible, otherwise as a string, so both
`features.input_enabled=false` and `server.public_origin=https://relay.example` work.",
        env!("CARGO_PKG_VERSION")
    );
}

/// `terminal-relay settings get|set` — see `print_usage`.
fn settings_command(args: &[String]) -> Result<(), String> {
    use terminal_relay::db::Db;
    use terminal_relay::settings::defs::DEFS;
    use terminal_relay::settings::store::SettingsStore;

    let action = args.first().map(String::as_str).unwrap_or("get");
    let bootstrap = Bootstrap::from_env().map_err(|e| format!("bootstrap failed: {e}"))?;
    let db = Db::open(&bootstrap.db_path).map_err(|e| format!("cannot open database: {e}"))?;
    let store = SettingsStore::initialize(&db, &bootstrap)
        .map_err(|e| format!("cannot load settings: {e}"))?;
    let snapshot = store.snapshot();

    match action {
        "get" => {
            let wanted: Vec<&str> = args[1..].iter().map(String::as_str).collect();
            for def in DEFS {
                if !wanted.is_empty() && !wanted.contains(&def.name) {
                    continue;
                }
                if def.secret {
                    // Report only whether a secret is configured, never its value.
                    println!("{} = <{}>", def.name, snapshot.secret_form(def.name));
                } else {
                    println!("{} = {}", def.name, snapshot.values[def.name].to_json());
                }
            }
            if !wanted.is_empty() {
                for name in wanted {
                    if !DEFS.iter().any(|d| d.name == name) {
                        return Err(format!("unknown setting: {name}"));
                    }
                }
            }
            Ok(())
        }
        "set" => {
            let mut desired = std::collections::BTreeMap::new();
            for pair in &args[1..] {
                let (name, raw) = pair
                    .split_once('=')
                    .ok_or_else(|| format!("expected NAME=VALUE, got: {pair}"))?;
                // Accept JSON so booleans, numbers and lists work, and fall back to a
                // bare string so paths and origins need no quoting.
                let value: serde_json::Value = serde_json::from_str(raw)
                    .unwrap_or_else(|_| serde_json::Value::String(raw.to_string()));
                desired.insert(name.to_string(), value);
            }
            if desired.is_empty() {
                return Err("no settings supplied".to_string());
            }

            // Skip a no-op so repeated deploys do not churn the revision or the audit
            // log. Secrets always apply, since the stored form is encrypted and cannot
            // be compared to the plaintext offered here.
            let unchanged = desired.iter().all(|(name, value)| {
                match terminal_relay::settings::find_def(name) {
                    Some(def) if !def.secret => snapshot
                        .values
                        .get(name)
                        .map(|current| current.to_json() == *value)
                        .unwrap_or(false),
                    _ => false,
                }
            });
            if unchanged {
                println!("no changes; already at revision {}", snapshot.revision);
                return Ok(());
            }

            let names: Vec<String> = desired.keys().cloned().collect();
            let outcome = store
                .patch("cli", snapshot.revision, desired)
                .map_err(|e| format!("settings update rejected: {}", e.message))?;
            println!(
                "revision {} applied ({} changed: {})",
                outcome.snapshot.revision,
                outcome.changed.len(),
                if outcome.changed.is_empty() {
                    names.join(", ")
                } else {
                    outcome.changed.join(", ")
                }
            );
            Ok(())
        }
        other => Err(format!(
            "unknown settings action: {other} (expected `get` or `set`)"
        )),
    }
}

/// Minimal HTTP/1.1 probe used by `terminal-relay healthcheck`.
fn probe_health(target: &str, path: &str) -> std::io::Result<()> {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;

    let mut stream = TcpStream::connect(target)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {target}\r\nConnection: close\r\n\r\n"
    )?;
    stream.flush()?;

    let mut response = Vec::new();
    // The status line is all that matters, but the body is small enough to read.
    let _ = stream.take(8192).read_to_end(&mut response)?;
    let text = String::from_utf8_lossy(&response);
    let status = text.lines().next().unwrap_or_default();
    if status.contains(" 200") {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "unexpected status: {status}"
        )))
    }
}

/// Recovery mode serves only the operator settings API and health endpoints, on a
/// bootstrap-supplied address.
async fn recovery_serve(
    state: std::sync::Arc<AppState>,
    listen: &str,
) -> Result<(), terminal_relay::error::ApiError> {
    use terminal_relay::error::ApiError;

    server::install_crypto_provider();
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .map_err(|e| ApiError::internal(format!("cannot bind the recovery listener: {e}")))?;

    tracing::warn!(
        event = "recovery_listener_bound",
        address = %listen,
        "recovery listener bound; repair settings then restart without RELAY_RECOVERY_MODE"
    );

    let router = terminal_relay::http::router(std::sync::Arc::clone(&state));
    let shutdown = async move {
        let _ = tokio::signal::ctrl_c().await;
    };
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await
        .map_err(|e| ApiError::internal(format!("recovery listener failed: {e}")))
}
