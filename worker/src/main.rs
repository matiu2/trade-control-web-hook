//! `trade-control-worker` — the native VM binary.
//!
//! Boots the axum HTTP receiver against a Postgres-backed [`PgStateStore`],
//! mirroring the Cloudflare Worker's `#[event(fetch)]` flow. It:
//!   1. installs a tracing subscriber (env-filter + error layer),
//!   2. loads non-secret [`Config`] from a TOML file,
//!   3. loads [`Secrets`] from the environment,
//!   4. connects + migrates Postgres,
//!   5. builds the app state, starts the cron scheduler, and serves `POST /`
//!      with graceful shutdown.
//!
//! TLS is terminated by a reverse proxy; this binds plain HTTP on loopback.
//!
//! The tokio scheduler currently runs the shared **engine tick** (the only cron
//! job ported so far) on a long-lived interval; the remaining cron jobs (upkeep
//! / daily / expiry-sweep) follow through the same `trade-control-cron` crate.

use std::sync::Arc;

use color_eyre::eyre::{Context, Result, eyre};
use tokio::net::TcpListener;
use trace_init::init_tracing;
use trade_control_worker::{
    Config, PgMetadataStore, PgStateStore, Secrets,
    http::{AppState, Dispatcher, router},
    run_scheduler,
};

/// Default config path when none is passed as the first CLI argument.
const DEFAULT_CONFIG_PATH: &str = "./trade-control.toml";

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    init_tracing();

    // Config path: first positional arg, else the default. If the file is
    // missing, `Config::load` surfaces a clear read error (operator must create
    // `./trade-control.toml` or pass a path).
    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| DEFAULT_CONFIG_PATH.to_string());
    let config = Config::load(&config_path)
        .wrap_err_with(|| format!("loading config from {config_path}"))?;
    tracing::info!(
        "loaded config from {config_path} (bind {}:{})",
        config.http.bind_addr,
        config.http.port
    );

    let secrets = Secrets::from_env().map_err(|e| eyre!("loading secrets from env: {e}"))?;

    // The signing key is hex on the wire; decode it once here (the wasm worker
    // does the same via `sig::parse_key_hex`) so the request path doesn't reparse.
    let signing_key = trade_control_core::sig::parse_key_hex(&secrets.signing_key)
        .map_err(|e| eyre!("SIGNING_KEY is not valid hex: {e:?}"))?
        .to_vec();

    let store = PgStateStore::connect(&config.database.url)
        .await
        .map_err(|e| eyre!("connecting to Postgres: {e}"))?;
    store
        .migrate()
        .await
        .map_err(|e| eyre!("running migrations: {e}"))?;
    let accounts = PgMetadataStore::from_state_store(&store);
    tracing::info!("Postgres connected + migrated");

    let state = Arc::new(AppState {
        store,
        accounts,
        secrets,
        signing_key,
    });
    // The broker dispatch returns `?Send` futures (single-threaded SDK clients),
    // so it runs on a dedicated current-thread + `LocalSet` thread owned by the
    // dispatcher; axum handlers stay `Send` and just ferry the body across.
    let dispatcher = Dispatcher::spawn(state.clone());
    let app = router(dispatcher);

    // Start the cron scheduler on its own dedicated current-thread + `LocalSet`
    // thread (the engine tick drives the `?Send` broker SDKs, same as the HTTP
    // dispatcher). It runs the shared engine tick on a long-lived re-arming
    // interval; on process shutdown the thread is torn down with the process
    // (the engine persists plan state before dispatching, so an abandoned tick
    // is safe — the next start re-evaluates from the persisted watermark). See
    // `scheduler` for the tokio#6504 timer-design guardrails.
    run_scheduler(state, config.scheduler.clone());

    let bind = format!("{}:{}", config.http.bind_addr, config.http.port);
    let listener = TcpListener::bind(&bind)
        .await
        .wrap_err_with(|| format!("binding {bind}"))?;
    tracing::info!("listening on http://{bind}");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .wrap_err("axum serve failed")?;

    tracing::info!("shut down cleanly");
    Ok(())
}

/// Resolve once either SIGINT (Ctrl-C) or SIGTERM (systemd stop) fires, so the
/// in-flight request finishes before the process exits.
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!("failed to install Ctrl-C handler: {e}");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => tracing::error!("failed to install SIGTERM handler: {e}"),
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("SIGINT received, shutting down"),
        _ = terminate => tracing::info!("SIGTERM received, shutting down"),
    }
}

/// Tracing initialisation, kept in its own module so `main` stays focused on
/// the boot sequence.
mod trace_init {
    use tracing_error::ErrorLayer;
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::{EnvFilter, fmt};

    /// Install the global tracing subscriber: an env-filtered fmt layer plus the
    /// `tracing_error` capture layer (per repo convention for apps). Honours
    /// `RUST_LOG`; defaults to `info` when unset.
    pub fn init_tracing() {
        let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
        tracing_subscriber::registry()
            .with(filter)
            .with(fmt::layer())
            .with(ErrorLayer::default())
            .init();
    }
}
