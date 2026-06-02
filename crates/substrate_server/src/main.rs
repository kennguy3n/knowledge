//! Binary entrypoint for the substrate loopback server.
//!
//! See the crate-level docs in `lib.rs` for the architecture. This
//! module only wires up logging + the tokio runtime and delegates to
//! [`substrate_server::run`].

use std::process::ExitCode;

use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    // Structured logging. `KNOWLEDGE_LOG` (falling back to `RUST_LOG`)
    // controls the filter; default to `info`. No request bodies or
    // secrets are ever logged — only scope ids and operation names.
    let filter = EnvFilter::try_from_env("KNOWLEDGE_LOG")
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    match substrate_server::run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!(error = %err, "substrate_server: fatal error");
            ExitCode::FAILURE
        }
    }
}
