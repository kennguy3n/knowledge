//! `knowledge-rotate-key` — offline master-key rotation CLI.
//!
//! Re-keys the substrate's SQLCipher evidence store and permission
//! store from the current master key to a new one, keeping timestamped
//! backups of the originals. See `docs/security/key-rotation.md` for
//! the full procedure and `crates/substrate_server/src/key_rotation.rs`
//! for the implementation.
//!
//! ## Contract
//!
//! This tool is **offline**: the substrate server MUST be stopped so
//! nothing writes to either database while the rotated copy is taken.
//!
//! ## Configuration (environment)
//!
//! * `KNOWLEDGE_STORE_PATH`        — evidence store (default
//!   `/var/lib/knowledge/substrate.db`).
//! * `KNOWLEDGE_PERMISSIONS_PATH`  — permission store (default: a
//!   `permissions.db` sibling of the evidence store).
//! * `KNOWLEDGE_MASTER_KEY`        — **current** key, 64 hex chars.
//! * `KNOWLEDGE_NEW_MASTER_KEY`    — **new** key, 64 hex chars.
//!
//! On success it prints a summary (row/tuple counts and backup paths)
//! and exits `0`. On any error it leaves the live databases untouched
//! under the old key and exits non-zero.

use std::path::PathBuf;
use std::process::ExitCode;

use zeroize::Zeroizing;

use substrate_server::config::{ConfigError, ServerConfig};
use substrate_server::key_rotation::{rotate, RotationPaths, ENV_NEW_MASTER_KEY};

const HELP: &str = "\
knowledge-rotate-key — offline master-key rotation for the substrate

USAGE:
    knowledge-rotate-key

The substrate server MUST be stopped before running this tool.

ENVIRONMENT:
    KNOWLEDGE_STORE_PATH        Evidence store path
                                (default /var/lib/knowledge/substrate.db)
    KNOWLEDGE_PERMISSIONS_PATH  Permission store path
                                (default: permissions.db beside the store)
    KNOWLEDGE_MASTER_KEY        Current master key (64 hex chars)
    KNOWLEDGE_NEW_MASTER_KEY    New master key (64 hex chars)

The original databases are moved aside to timestamped `.bak.<unix>`
files (still readable under the OLD key). Retain them until the server
is confirmed healthy under the new key, then destroy them securely.
";

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    if let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{HELP}");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("error: unexpected argument `{other}`\n");
                print!("{HELP}");
                return ExitCode::FAILURE;
            }
        }
    }

    // Reuse the server's env-driven config to resolve the store paths
    // and the *old* master key (with the same validation + defaults the
    // server applies). The bind address / update-check fields are
    // parsed but unused here.
    let config = match ServerConfig::from_env() {
        Ok(c) => c,
        Err(ConfigError::Missing(var)) => {
            eprintln!("error: required environment variable `{var}` is unset or empty");
            return ExitCode::FAILURE;
        }
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Wrap the new key hex in `Zeroizing` so it is wiped on drop, matching
    // `config.master_key_hex` (the old key, a `Zeroizing<String>`).
    let new_key_hex: Zeroizing<String> = match std::env::var(ENV_NEW_MASTER_KEY) {
        Ok(v) if !v.is_empty() => Zeroizing::new(v),
        _ => {
            eprintln!(
                "error: required environment variable `{ENV_NEW_MASTER_KEY}` is unset or empty"
            );
            return ExitCode::FAILURE;
        }
    };

    let paths = RotationPaths {
        store_path: PathBuf::from(&config.store_path),
        permissions_path: PathBuf::from(&config.permissions_path),
    };

    eprintln!("knowledge-rotate-key: rotating master key (substrate must be stopped)");
    eprintln!("  evidence store:   {}", config.store_path);
    eprintln!("  permission store: {}", config.permissions_path);

    match rotate(&paths, &config.master_key_hex, &new_key_hex) {
        Ok(outcome) => {
            println!("rotation complete:");
            println!(
                "  evidence: {} rows, {} scope keys re-wrapped, {} bodies verified",
                outcome.evidence.evidence_rows,
                outcome.evidence.scopes_rewrapped,
                outcome.evidence.bodies_verified,
            );
            println!(
                "  permissions: {} tuples re-encrypted",
                outcome.permission_tuples
            );
            println!("  evidence backup:   {}", outcome.evidence_backup.display());
            println!(
                "  permission backup: {}",
                outcome.permissions_backup.display()
            );
            println!(
                "Retain the backups until the server is healthy under the new key, \
                 then destroy them securely (they open under the OLD key)."
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            eprintln!("the live databases were left untouched under the old key.");
            ExitCode::FAILURE
        }
    }
}
