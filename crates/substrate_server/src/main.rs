//! Binary entrypoint for the substrate loopback server.
//!
//! See the crate-level docs in `lib.rs` for the architecture. This
//! module wires up logging + the tokio runtime, parses the
//! `--role primary|standby|auto` replication flag, and delegates to
//! [`substrate_server::run`].

use std::process::ExitCode;

use tracing_subscriber::EnvFilter;

/// Parse the optional `--role <value>` / `--role=<value>` flag.
///
/// Returns `Some(value)` for the last occurrence, or `None` if absent.
/// The value is validated downstream by
/// `replication::ReplicationMode::parse`, so an invalid role surfaces
/// as a clean startup error rather than a panic here.
fn parse_role_flag<I: IntoIterator<Item = String>>(args: I) -> Option<String> {
    let mut role = None;
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        if let Some(value) = arg.strip_prefix("--role=") {
            role = Some(value.to_string());
        } else if arg == "--role" {
            role = iter.next();
        }
    }
    role
}

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

    let role = parse_role_flag(std::env::args().skip(1));

    match substrate_server::run(role).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!(error = %err, "substrate_server: fatal error");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_role_flag;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn parses_space_separated_role() {
        assert_eq!(
            parse_role_flag(args(&["--role", "standby"])),
            Some("standby".to_string())
        );
    }

    #[test]
    fn parses_equals_role() {
        assert_eq!(
            parse_role_flag(args(&["--role=primary"])),
            Some("primary".to_string())
        );
    }

    #[test]
    fn absent_role_is_none() {
        assert_eq!(parse_role_flag(args(&["--other", "x"])), None);
        assert_eq!(parse_role_flag(args(&[])), None);
    }

    #[test]
    fn last_occurrence_wins() {
        assert_eq!(
            parse_role_flag(args(&["--role", "auto", "--role=standby"])),
            Some("standby".to_string())
        );
    }
}
