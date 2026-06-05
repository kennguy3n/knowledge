//! Offline master-key rotation for a deployed substrate.
//!
//! The substrate seals two SQLCipher databases under the same 32-byte
//! master key (see [`crate::config`]):
//!
//! * the **evidence store** (`KNOWLEDGE_STORE_PATH`) — evidence bodies
//!   are encrypted under per-scope DEKs that are *wrapped* with a
//!   master-derived key, so rotating the master re-wraps the DEKs and
//!   re-keys the SQLCipher pages without ever re-encrypting a body;
//! * the **permission store** (`KNOWLEDGE_PERMISSIONS_PATH`) — every
//!   relation-tuple payload is encrypted *directly* under a
//!   master-derived AEAD key, so rotation re-encrypts every tuple.
//!
//! Both crates expose a `rotate_master_key` primitive that writes a
//! fully verified copy to a fresh path
//! ([`evidence_store::EvidenceStore::rotate_master_key`],
//! [`permission_service::PersistentTupleStore::rotate_master_key`]).
//! This module owns the *deployment* choreography around those
//! primitives: validating the key material, producing the rotated
//! copies into sibling temp files, and atomically swapping them over
//! the live files while keeping timestamped backups of the originals.
//!
//! The tool is **offline** by contract: the substrate server must be
//! stopped first so nothing writes to either database while the copy
//! is taken. The `knowledge-rotate-key` binary (`src/bin/rotate_key.rs`)
//! is the CLI front-end; `scripts/rotate-master-key.sh` wraps it for
//! Docker / Kubernetes deployments.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use zeroize::Zeroizing;

use crypto::MasterKey;
use evidence_store::{EvidenceStore, EvidenceStoreConfig, MasterKeyRotationReport};
use permission_service::PersistentTupleStore;

use crate::config::decode_master_key;

/// Environment variable carrying the *new* 64-hex-char master key the
/// stores are rotated to. The *old* key is read from the same
/// [`crate::config::ENV_MASTER_KEY`] the server uses, so the rotation
/// tool inherits the deployment's existing key configuration and only
/// needs this one extra variable.
pub const ENV_NEW_MASTER_KEY: &str = "KNOWLEDGE_NEW_MASTER_KEY";

/// On-disk locations of the two stores to rotate.
#[derive(Debug, Clone)]
pub struct RotationPaths {
    /// SQLCipher evidence store (`KNOWLEDGE_STORE_PATH`).
    pub store_path: PathBuf,
    /// SQLCipher permission-tuple store (`KNOWLEDGE_PERMISSIONS_PATH`).
    pub permissions_path: PathBuf,
}

/// Outcome of a successful [`rotate`] run.
///
/// The backup paths are where the *pre-rotation* databases were moved
/// before the rotated copies were swapped in. Operators should retain
/// them until the substrate has been confirmed healthy under the new
/// key, then destroy them (they are still readable under the **old**
/// master key).
#[derive(Debug, Clone)]
pub struct RotationOutcome {
    /// Integrity report from the evidence-store rotation.
    pub evidence: MasterKeyRotationReport,
    /// Number of permission tuples re-encrypted under the new key.
    pub permission_tuples: usize,
    /// Backup of the original evidence store (old key).
    pub evidence_backup: PathBuf,
    /// Backup of the original permission store (old key).
    pub permissions_backup: PathBuf,
}

/// Errors surfaced by [`rotate`].
#[derive(Debug, thiserror::Error)]
pub enum RotationError {
    /// One of the master keys was not exactly 64 ASCII-hex characters.
    #[error("{which} master key must be exactly 64 hex characters")]
    BadMasterKey {
        /// Which key was malformed (`"old"` or `"new"`).
        which: &'static str,
    },
    /// The old and new master keys are identical — rotation would be a
    /// no-op and is almost certainly an operator mistake.
    #[error("old and new master keys are identical — nothing to rotate")]
    KeysIdentical,
    /// The evidence store does not exist at the configured path.
    #[error("evidence store not found at {0}")]
    EvidenceStoreMissing(PathBuf),
    /// A temp/backup path the tool wants to use is already occupied,
    /// so the tool refuses to clobber it.
    #[error("refusing to overwrite pre-existing path {0}")]
    PathOccupied(PathBuf),
    /// The evidence-store rotation primitive failed.
    #[error("evidence-store rotation failed: {0}")]
    Evidence(#[from] evidence_store::EvidenceError),
    /// The permission-store rotation primitive failed.
    #[error("permission-store rotation failed: {0}")]
    Permission(#[from] permission_service::PermissionError),
    /// A filesystem operation (rename, fsync, …) failed.
    #[error("filesystem operation failed during {context}: {source}")]
    Io {
        /// Human-readable description of the step that failed.
        context: String,
        /// Underlying I/O error.
        source: std::io::Error,
    },
}

impl RotationError {
    fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }
}

/// Build a sibling path by appending `suffix` to `base`'s file name,
/// e.g. `/data/substrate.db` + `.rotating` →
/// `/data/substrate.db.rotating`.
fn sibling(base: &Path, suffix: &str) -> PathBuf {
    let mut name = base.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// `fsync` a directory so a rename it contains is durable across a
/// crash. Best-effort on platforms where opening a directory for sync
/// is not supported.
fn fsync_dir(path: &Path) -> Result<(), RotationError> {
    let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) else {
        return Ok(());
    };
    match fs::File::open(parent) {
        Ok(dir) => dir
            .sync_all()
            .map_err(|e| RotationError::io(format!("fsync dir {}", parent.display()), e)),
        // Some filesystems refuse to open a dir as a file; treat the
        // durability fsync as best-effort there.
        Err(_) => Ok(()),
    }
}

/// Rotate both stores from `old_master_hex` to `new_master_hex`.
///
/// On success the live files at [`RotationPaths::store_path`] and
/// [`RotationPaths::permissions_path`] are the rotated copies (openable
/// only under the new key) and the originals have been moved to the
/// returned backup paths. On any error the live files are left
/// untouched and any partial temp files are cleaned up.
///
/// # Errors
///
/// See [`RotationError`]. The function fails fast and atomically: if
/// either rotation or the swap cannot complete, the original databases
/// remain in place under the old key.
pub fn rotate(
    paths: &RotationPaths,
    old_master_hex: &str,
    new_master_hex: &str,
) -> Result<RotationOutcome, RotationError> {
    let old_key: Zeroizing<MasterKey> =
        decode_master_key(old_master_hex).ok_or(RotationError::BadMasterKey { which: "old" })?;
    let new_key: Zeroizing<MasterKey> =
        decode_master_key(new_master_hex).ok_or(RotationError::BadMasterKey { which: "new" })?;

    if *old_key == *new_key {
        return Err(RotationError::KeysIdentical);
    }

    if !paths.store_path.exists() {
        return Err(RotationError::EvidenceStoreMissing(
            paths.store_path.clone(),
        ));
    }

    let evidence_tmp = sibling(&paths.store_path, ".rotating");
    let permissions_tmp = sibling(&paths.permissions_path, ".rotating");
    // Refuse to start if a stale temp file from a previous aborted run
    // is in the way — the rotation primitives also refuse, but failing
    // here gives a clearer error and avoids opening the source stores.
    for tmp in [&evidence_tmp, &permissions_tmp] {
        if tmp.exists() {
            return Err(RotationError::PathOccupied(tmp.clone()));
        }
    }

    // --- Phase 1: produce verified rotated copies into temp files. ---
    let evidence_report = {
        let store =
            EvidenceStore::open(&paths.store_path, &old_key, EvidenceStoreConfig::default())?;
        // `rotate_master_key` may fail *after* its `VACUUM INTO` has
        // already created `evidence_tmp` (e.g. during rekey, DEK re-wrap,
        // or integrity verification). Remove the partial copy so a retry
        // is not blocked by the stale-temp-file guard above — mirrors the
        // permission-store cleanup below.
        match store.rotate_master_key(&new_key, &evidence_tmp) {
            Ok(report) => report,
            Err(e) => {
                let _ = fs::remove_file(&evidence_tmp);
                return Err(RotationError::Evidence(e));
            }
        }
    };

    let permission_tuples = {
        // The permission store may not exist yet on a fresh deployment
        // with no grants; opening creates an empty database, which
        // rotates to an empty copy. That is correct and harmless.
        let store = match PersistentTupleStore::open(&paths.permissions_path, &old_key) {
            Ok(store) => store,
            Err(e) => {
                // Clean up the evidence temp copy so a retry is clean.
                let _ = fs::remove_file(&evidence_tmp);
                return Err(RotationError::Permission(e));
            }
        };
        match store.rotate_master_key(&new_key, &permissions_tmp) {
            Ok(n) => n,
            Err(e) => {
                let _ = fs::remove_file(&evidence_tmp);
                let _ = fs::remove_file(&permissions_tmp);
                return Err(RotationError::Permission(e));
            }
        }
    };

    // --- Phase 2: atomically swap the rotated copies in, keeping
    // timestamped backups of the originals. ---
    let stamp = unix_stamp();
    let evidence_backup = sibling(&paths.store_path, &format!(".bak.{stamp}"));
    let permissions_backup = sibling(&paths.permissions_path, &format!(".bak.{stamp}"));
    for backup in [&evidence_backup, &permissions_backup] {
        if backup.exists() {
            let _ = fs::remove_file(&evidence_tmp);
            let _ = fs::remove_file(&permissions_tmp);
            return Err(RotationError::PathOccupied(backup.clone()));
        }
    }

    // Evidence store: move original aside, then move rotated copy in.
    // A failure here leaves the live files untouched but would orphan
    // both `.rotating` temp copies, so clean them up to satisfy the
    // documented "partial temp files are cleaned up" contract and keep a
    // retry clear of the stale-temp guard.
    if let Err(e) = rename(
        &paths.store_path,
        &evidence_backup,
        "back up evidence store",
    ) {
        let _ = fs::remove_file(&evidence_tmp);
        let _ = fs::remove_file(&permissions_tmp);
        return Err(e);
    }
    if let Err(e) = rename(
        &evidence_tmp,
        &paths.store_path,
        "install rotated evidence store",
    ) {
        // Roll back: restore the original evidence store. The rename
        // *from* `evidence_tmp` failed, so the rotated copy is still at
        // `evidence_tmp` and must be removed alongside `permissions_tmp`
        // to honour the cleanup contract.
        rollback_rename(
            &evidence_backup,
            &paths.store_path,
            "restore evidence store",
        );
        let _ = fs::remove_file(&evidence_tmp);
        let _ = fs::remove_file(&permissions_tmp);
        return Err(e);
    }

    // Permission store: same dance. Phase 1 opened (and therefore
    // created, if absent) the permission store, so the live file always
    // exists here and is unconditionally backed up. If either step
    // fails, roll back BOTH stores so the deployment stays consistent
    // under the old key.
    if let Err(e) = rename(
        &paths.permissions_path,
        &permissions_backup,
        "back up permission store",
    ) {
        // `rollback_evidence` leaves the rotated evidence copy back at
        // `evidence_tmp`; remove it (and `permissions_tmp`) so no
        // `.rotating` files survive to block a retry.
        rollback_evidence(paths, &evidence_backup, &evidence_tmp);
        let _ = fs::remove_file(&evidence_tmp);
        let _ = fs::remove_file(&permissions_tmp);
        return Err(e);
    }
    if let Err(e) = rename(
        &permissions_tmp,
        &paths.permissions_path,
        "install rotated permission store",
    ) {
        rollback_rename(
            &permissions_backup,
            &paths.permissions_path,
            "restore permission store",
        );
        // The rename *from* `permissions_tmp` failed so that copy still
        // exists, and `rollback_evidence` restores the rotated evidence
        // copy to `evidence_tmp`; remove both to honour the cleanup
        // contract and keep a retry clear of the stale-temp guard.
        rollback_evidence(paths, &evidence_backup, &evidence_tmp);
        let _ = fs::remove_file(&evidence_tmp);
        let _ = fs::remove_file(&permissions_tmp);
        return Err(e);
    }

    // Make the renames durable.
    fsync_dir(&paths.store_path)?;
    fsync_dir(&paths.permissions_path)?;

    Ok(RotationOutcome {
        evidence: evidence_report,
        permission_tuples,
        evidence_backup,
        permissions_backup,
    })
}

/// Roll the evidence store back to its pre-swap state after a later
/// step failed: move the rotated copy back out of the way and restore
/// the original from its backup.
fn rollback_evidence(paths: &RotationPaths, evidence_backup: &Path, evidence_tmp: &Path) {
    // Best-effort: move the just-installed rotated copy back to the
    // temp name, then restore the original.
    rollback_rename(
        &paths.store_path,
        evidence_tmp,
        "move rotated evidence copy aside",
    );
    rollback_rename(evidence_backup, &paths.store_path, "restore evidence store");
}

/// Best-effort rename used only on rollback/cleanup paths. Unlike
/// [`rename`] it never returns an error — the caller is already unwinding
/// a prior failure and will surface that original error — but it emits a
/// `warn!` so an operator can recover manually if the rollback itself
/// could not complete (leaving `.rotating` / `.bak.<unix>` files behind).
fn rollback_rename(from: &Path, to: &Path, what: &str) {
    if let Err(e) = fs::rename(from, to) {
        tracing::warn!(
            error = %e,
            from = %from.display(),
            to = %to.display(),
            "master-key rotation rollback step failed ({what}); manual recovery may be required"
        );
    }
}

/// Seconds since the Unix epoch, used to disambiguate backup files.
/// Falls back to `0` if the clock is before the epoch (it never is).
fn unix_stamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn rename(from: &Path, to: &Path, context: &str) -> Result<(), RotationError> {
    fs::rename(from, to).map_err(|e| {
        RotationError::io(
            format!("{context} ({} -> {})", from.display(), to.display()),
            e,
        )
    })
}
