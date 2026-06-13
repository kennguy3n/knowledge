//! Binary entrypoint for the sync relay.
//!
//! Configuration is read from the environment so the relay drops into
//! a container / systemd unit without flags:
//!
//! * `RELAY_BIND` — bind address (default `127.0.0.1:8787`).
//! * `RELAY_TOKENS` — comma-separated `token:tenant` pairs. **Required**:
//!   a relay with no tokens fails closed (rejects every request), so
//!   startup aborts if this is missing/empty to surface the
//!   misconfiguration loudly.
//! * `RELAY_MAX_BLOBS_PER_TOPIC` / `RELAY_MAX_BLOB_BYTES` — optional
//!   storage caps (see [`StoreLimits`]).
//!
//! Secrets are never logged: only tenant ids, topic prefixes, and
//! counts reach the tracing output.

use std::process::ExitCode;
use std::sync::Arc;

use tracing_subscriber::EnvFilter;

use sync_relay::store::StoreLimits;
use sync_relay::{InMemoryBlobStore, RelayConfig, RelayServer, RelayState, TokenRegistry};

const DEFAULT_BIND: &str = "127.0.0.1:8787";

#[tokio::main]
async fn main() -> ExitCode {
    let filter = EnvFilter::try_from_env("KNOWLEDGE_LOG")
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!(error = %err, "sync_relay: fatal error");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), String> {
    let bind = std::env::var("RELAY_BIND").unwrap_or_else(|_| DEFAULT_BIND.to_string());
    let bind_addr = bind
        .parse()
        .map_err(|e| format!("invalid RELAY_BIND '{bind}': {e}"))?;

    let tokens_spec = std::env::var("RELAY_TOKENS").unwrap_or_default();
    let registry = TokenRegistry::from_pairs(&tokens_spec)
        .ok_or_else(|| "RELAY_TOKENS is malformed (expected 'token:tenant,...')".to_string())?;
    if registry.is_empty() {
        return Err("RELAY_TOKENS is empty; a relay with no tokens accepts nobody".to_string());
    }

    let limits = store_limits_from_env()?;
    let store = Arc::new(InMemoryBlobStore::new(limits));
    let state = RelayState::new(store, Arc::new(registry));
    let server = RelayServer::new(RelayConfig::new(bind_addr), state);

    let listener = server.bind().await.map_err(|e| e.to_string())?;
    let local_addr = listener
        .local_addr()
        .map_err(|e| format!("could not read local addr: {e}"))?;
    tracing::info!(%local_addr, "sync_relay listening");

    server
        .serve_on(listener, shutdown_signal())
        .await
        .map_err(|e| e.to_string())
}

fn store_limits_from_env() -> Result<StoreLimits, String> {
    let mut limits = StoreLimits::default();
    if let Ok(v) = std::env::var("RELAY_MAX_BLOBS_PER_TOPIC") {
        limits.max_blobs_per_topic = v
            .parse()
            .map_err(|e| format!("invalid RELAY_MAX_BLOBS_PER_TOPIC '{v}': {e}"))?;
    }
    if let Ok(v) = std::env::var("RELAY_MAX_BLOB_BYTES") {
        limits.max_blob_bytes = v
            .parse()
            .map_err(|e| format!("invalid RELAY_MAX_BLOB_BYTES '{v}': {e}"))?;
    }
    Ok(limits)
}

/// Resolve on SIGINT (Ctrl-C) so axum can drain in-flight requests.
async fn shutdown_signal() {
    if let Err(e) = tokio::signal::ctrl_c().await {
        tracing::error!(error = %e, "failed to install Ctrl-C handler");
    }
    tracing::info!("sync_relay shutting down");
}
