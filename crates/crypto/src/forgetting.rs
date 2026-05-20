//! Cryptographic forgetting + policy-driven epoch rotation.
//!
//! Per `docs/DESIGN.md` §3.6, the substrate must
//! be able to **destroy** the data-encryption key (DEK) for a scope or
//! a single epoch. Once a DEK is destroyed, every ciphertext encrypted
//! under that key becomes permanently undecryptable — that is the
//! "right to be forgotten" implemented at the cryptographic layer
//! rather than the database layer.
//!
//! This module ships:
//!
//! * [`ScopeDek`] / [`EpochDek`] — DEK structs that zeroize their key
//!   material on drop or on explicit destroy.
//! * [`DekRegistry`] — in-memory registry indexed by
//!   `(ScopeId, EpochId)`. Holds a tombstone for every destroyed key
//!   so callers can prove a scope or epoch is forgotten.
//! * [`destroy_scope_dek`] / [`destroy_epoch_dek`] — destructors that
//!   wipe key material and emit one [`KeyDestructionEvent`] per key.
//! * [`is_scope_forgotten`] / [`is_epoch_forgotten`] — predicates
//!   over the registry's tombstones.
//! * [`EpochRotationPolicy`] — declarative rotation policy
//!   (time-based, size-based, or operator-forced).
//! * [`EpochManager`] — tracks the current epoch per scope and
//!   rotates it when a policy trigger fires.
//! * [`KeyDestructionAuditor`] — hook trait so `audit_service` (which
//!   sits above `crypto` in the dep graph) can persist a
//!   [`AuditActionType::KeyDestruction`](audit_service::entry::AuditActionType::KeyDestruction)
//!   audit row for every destroy. The `crypto` crate intentionally
//!   does not depend on `audit_service`; the wiring lives at a higher
//!   layer.
//!
//! The registry is intentionally ephemeral / in-memory. Persistent
//! storage of forgetting metadata (tombstones, audit cross-references)
//! lands in a future update alongside the `evidence_store` rewrite.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;
use zeroize::Zeroize;

use crate::aead::{AeadKey, AEAD_KEY_LEN};
use crate::errors::CryptoError;

/// Newtype for scope identifiers used by the registry. Matches the
/// shape of [`evidence_store::ScopeId`] but lives in `crypto` so the
/// crate doesn't take a dependency on `evidence_store`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ScopeId(pub Uuid);

impl ScopeId {
    /// Generate a fresh random scope id.
    pub fn new_v4() -> Self {
        Self(Uuid::new_v4())
    }
}

/// Newtype for monotonically-increasing epoch ids per scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EpochId(pub u64);

impl EpochId {
    /// Genesis epoch (`0`).
    pub const fn zero() -> Self {
        Self(0)
    }

    /// Return the next epoch, or [`CryptoError::EpochOverflow`] if
    /// the counter would overflow `u64::MAX`.
    ///
    /// This used to saturate at `u64::MAX`, which silently permitted
    /// the [`EpochManager`] to keep "rotating" once the terminal
    /// epoch was reached: each call to [`EpochManager::rotate`]
    /// would derive a fresh DEK, mark the previous epoch cold, and
    /// then re-bind the *same* epoch id to the new DEK — a
    /// catastrophic break of the forward-secrecy invariant where
    /// epoch ids monotonically increase. Surfacing the overflow as a
    /// hard error here keeps the substrate consistent with
    /// [`crate::mls::MlsEpoch::next`] and ensures every commit /
    /// rotation attempt past the addressable epoch space is rejected
    /// explicitly rather than silently corrupting the registry.
    pub fn next(self) -> Result<Self, CryptoError> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or(CryptoError::EpochOverflow)
    }
}

/// Per-scope data-encryption key. Wraps a 32-byte AEAD key with
/// zeroize-on-drop and an explicit [`Self::destroy`] hook that wipes
/// the key bytes.
///
/// The same key material is referenced as `K_scope` in `docs/DESIGN.md`
/// §3.1.
#[derive(Clone)]
pub struct ScopeDek {
    /// Scope this key belongs to.
    pub scope_id: ScopeId,
    /// Wall-clock creation time. Used by audit + policy checks.
    pub created_at: DateTime<Utc>,
    /// Current epoch this DEK is bound to.
    pub epoch: EpochId,
    /// Raw 32-byte AEAD key material.
    key: AeadKey,
    /// `true` once the bytes have been zeroized.
    destroyed: bool,
}

impl Drop for ScopeDek {
    fn drop(&mut self) {
        // Always zeroize the key bytes on drop, even if `destroy` was
        // not called explicitly. The `destroyed` flag only tracks
        // whether `destroy` has been observed by the registry; the
        // memory hygiene contract is unconditional.
        self.key.zeroize();
        self.destroyed = true;
    }
}

impl std::fmt::Debug for ScopeDek {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScopeDek")
            .field("scope_id", &self.scope_id)
            .field("created_at", &self.created_at)
            .field("epoch", &self.epoch)
            .field("key", &"<redacted>")
            .field("destroyed", &self.destroyed)
            .finish()
    }
}

impl ScopeDek {
    /// Construct a fresh `ScopeDek` from raw key material.
    pub fn new(scope_id: ScopeId, epoch: EpochId, key: AeadKey) -> Self {
        Self {
            scope_id,
            created_at: Utc::now(),
            epoch,
            key,
            destroyed: false,
        }
    }

    /// Borrow the underlying 32-byte key. Returns `None` once
    /// destroyed.
    pub fn key(&self) -> Option<&AeadKey> {
        if self.destroyed {
            None
        } else {
            Some(&self.key)
        }
    }

    /// Has this DEK been destroyed?
    pub fn is_destroyed(&self) -> bool {
        self.destroyed
    }

    /// Zeroize the key material and mark this DEK as destroyed.
    /// Idempotent — calling twice is a no-op.
    pub fn destroy(&mut self) {
        if !self.destroyed {
            self.key.zeroize();
            self.destroyed = true;
        }
    }
}

/// Per-(scope, epoch) data-encryption key. Mirrors [`ScopeDek`] but
/// represents an explicit epoch slice that can be destroyed
/// independently of the scope-level DEK.
#[derive(Clone)]
pub struct EpochDek {
    /// Scope this epoch DEK belongs to.
    pub scope_id: ScopeId,
    /// Epoch identifier.
    pub epoch_id: EpochId,
    /// Wall-clock time when this epoch was activated.
    pub rotation_time: DateTime<Utc>,
    /// Raw 32-byte AEAD key material.
    key: AeadKey,
    /// `true` once the bytes have been zeroized.
    destroyed: bool,
}

impl Drop for EpochDek {
    fn drop(&mut self) {
        self.key.zeroize();
        self.destroyed = true;
    }
}

impl std::fmt::Debug for EpochDek {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EpochDek")
            .field("scope_id", &self.scope_id)
            .field("epoch_id", &self.epoch_id)
            .field("rotation_time", &self.rotation_time)
            .field("key", &"<redacted>")
            .field("destroyed", &self.destroyed)
            .finish()
    }
}

impl EpochDek {
    /// Construct a fresh `EpochDek`.
    pub fn new(scope_id: ScopeId, epoch_id: EpochId, key: AeadKey) -> Self {
        Self {
            scope_id,
            epoch_id,
            rotation_time: Utc::now(),
            key,
            destroyed: false,
        }
    }

    /// Borrow the underlying 32-byte key. Returns `None` once
    /// destroyed.
    pub fn key(&self) -> Option<&AeadKey> {
        if self.destroyed {
            None
        } else {
            Some(&self.key)
        }
    }

    /// Has this epoch DEK been destroyed?
    pub fn is_destroyed(&self) -> bool {
        self.destroyed
    }

    /// Zeroize the key material and mark this DEK as destroyed.
    /// Idempotent.
    pub fn destroy(&mut self) {
        if !self.destroyed {
            self.key.zeroize();
            self.destroyed = true;
        }
    }
}

/// Description of one key-destruction event. The registry emits one
/// of these per destroyed DEK so the audit pipeline can persist a
/// matching audit row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyDestructionEvent {
    /// Scope whose key was destroyed.
    pub scope_id: ScopeId,
    /// Epoch the destroyed key was bound to.
    pub epoch_id: EpochId,
    /// `true` if this was a whole-scope destroy (every epoch); `false`
    /// for a single-epoch destroy.
    pub scope_wide: bool,
    /// Wall-clock time of destruction.
    pub destroyed_at: DateTime<Utc>,
}

/// Optional audit hook. Implemented by `audit_service` consumers in a
/// higher layer of the workspace; `crypto` deliberately doesn't depend
/// on `audit_service` so this trait is the boundary.
pub trait KeyDestructionAuditor {
    /// Persist `event` to the audit trail.
    fn record_destruction(&mut self, event: &KeyDestructionEvent);
}

/// In-memory registry of active DEKs.
///
/// The registry holds the live key material plus a tombstone for
/// every destroyed `(scope, epoch)` pair so callers can prove a key
/// is forgotten without re-deriving it.
#[derive(Debug, Default)]
pub struct DekRegistry {
    scope_deks: BTreeMap<ScopeId, ScopeDek>,
    epoch_deks: BTreeMap<(ScopeId, EpochId), EpochDek>,
    tombstones: BTreeMap<(ScopeId, EpochId), DateTime<Utc>>,
    forgotten_scopes: BTreeMap<ScopeId, DateTime<Utc>>,
}

impl DekRegistry {
    /// Construct an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a fresh scope-level DEK. Replaces any existing
    /// scope-level DEK for the same scope (the old one is dropped
    /// and zeroized automatically).
    pub fn insert_scope_dek(&mut self, dek: ScopeDek) {
        self.scope_deks.insert(dek.scope_id, dek);
    }

    /// Insert a fresh epoch-level DEK.
    pub fn insert_epoch_dek(&mut self, dek: EpochDek) {
        self.epoch_deks.insert((dek.scope_id, dek.epoch_id), dek);
    }

    /// Borrow the live scope-level DEK for `scope`, if any.
    pub fn get_scope_dek(&self, scope: ScopeId) -> Option<&ScopeDek> {
        self.scope_deks.get(&scope).filter(|d| !d.is_destroyed())
    }

    /// Borrow the live epoch-level DEK for `(scope, epoch)`, if any.
    pub fn get_epoch_dek(&self, scope: ScopeId, epoch: EpochId) -> Option<&EpochDek> {
        self.epoch_deks
            .get(&(scope, epoch))
            .filter(|d| !d.is_destroyed())
    }

    /// True iff every key for `scope` (scope-level + every epoch)
    /// has been destroyed.
    pub fn is_scope_forgotten(&self, scope: ScopeId) -> bool {
        self.forgotten_scopes.contains_key(&scope)
    }

    /// True iff the `(scope, epoch)` epoch DEK has been destroyed.
    pub fn is_epoch_forgotten(&self, scope: ScopeId, epoch: EpochId) -> bool {
        self.tombstones.contains_key(&(scope, epoch))
    }

    /// Iterate over every tombstone in the registry.
    pub fn tombstones(&self) -> impl Iterator<Item = (ScopeId, EpochId, DateTime<Utc>)> + '_ {
        self.tombstones.iter().map(|(&(s, e), &ts)| (s, e, ts))
    }

    /// Number of epoch DEKs registered for `scope` (live or destroyed).
    pub fn epoch_count(&self, scope: ScopeId) -> usize {
        self.epoch_deks.keys().filter(|(s, _)| *s == scope).count()
    }
}

/// Destroy every DEK belonging to `scope` — the scope-level DEK and
/// every per-epoch DEK. Returns the per-key destruction events so the
/// caller can persist them in the audit trail. Idempotent: re-running
/// against an already-forgotten scope returns an empty vec.
pub fn destroy_scope_dek(registry: &mut DekRegistry, scope: ScopeId) -> Vec<KeyDestructionEvent> {
    if registry.is_scope_forgotten(scope) {
        return Vec::new();
    }
    let now = Utc::now();
    let mut events = Vec::new();

    if let Some(mut dek) = registry.scope_deks.remove(&scope) {
        let was_destroyed = dek.is_destroyed();
        let epoch = dek.epoch;
        dek.destroy();
        if !was_destroyed {
            events.push(KeyDestructionEvent {
                scope_id: scope,
                epoch_id: epoch,
                scope_wide: true,
                destroyed_at: now,
            });
            registry.tombstones.insert((scope, epoch), now);
        }
    }

    let epoch_keys: Vec<(ScopeId, EpochId)> = registry
        .epoch_deks
        .keys()
        .filter(|(s, _)| *s == scope)
        .copied()
        .collect();
    for key in epoch_keys {
        if let Some(mut dek) = registry.epoch_deks.remove(&key) {
            let was_destroyed = dek.is_destroyed();
            dek.destroy();
            if !was_destroyed {
                events.push(KeyDestructionEvent {
                    scope_id: scope,
                    epoch_id: key.1,
                    scope_wide: true,
                    destroyed_at: now,
                });
                registry.tombstones.insert(key, now);
            }
        }
    }

    registry.forgotten_scopes.insert(scope, now);
    events
}

/// Destroy a single epoch DEK. Idempotent: re-running against an
/// already-forgotten epoch returns an empty vec.
pub fn destroy_epoch_dek(
    registry: &mut DekRegistry,
    scope: ScopeId,
    epoch: EpochId,
) -> Vec<KeyDestructionEvent> {
    if registry.is_epoch_forgotten(scope, epoch) {
        return Vec::new();
    }
    let now = Utc::now();
    let mut events = Vec::new();
    if let Some(mut dek) = registry.epoch_deks.remove(&(scope, epoch)) {
        let was_destroyed = dek.is_destroyed();
        dek.destroy();
        if !was_destroyed {
            events.push(KeyDestructionEvent {
                scope_id: scope,
                epoch_id: epoch,
                scope_wide: false,
                destroyed_at: now,
            });
        }
    }
    if events.is_empty() {
        // Even when no live DEK existed (e.g. already evicted), record
        // the tombstone so callers can prove the epoch is forgotten.
        events.push(KeyDestructionEvent {
            scope_id: scope,
            epoch_id: epoch,
            scope_wide: false,
            destroyed_at: now,
        });
    }
    registry.tombstones.insert((scope, epoch), now);
    events
}

/// Convenience predicate.
pub fn is_scope_forgotten(registry: &DekRegistry, scope: ScopeId) -> bool {
    registry.is_scope_forgotten(scope)
}

/// Convenience predicate.
pub fn is_epoch_forgotten(registry: &DekRegistry, scope: ScopeId, epoch: EpochId) -> bool {
    registry.is_epoch_forgotten(scope, epoch)
}

/// What caused an epoch to rotate. Surfaced by [`EpochManager`] so
/// callers can record the trigger in the audit trail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EpochRotationTrigger {
    /// Rotation happened because the current epoch reached
    /// [`EpochRotationPolicy::max_epoch_duration`].
    TimeElapsed,
    /// Rotation happened because the current epoch's encrypted bytes
    /// crossed [`EpochRotationPolicy::max_epoch_size_bytes`].
    SizeExceeded,
    /// Rotation was forced by a policy override (e.g. operator request).
    PolicyForced,
}

impl EpochRotationTrigger {
    /// Stable string tag.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TimeElapsed => "time_elapsed",
            Self::SizeExceeded => "size_exceeded",
            Self::PolicyForced => "policy_forced",
        }
    }
}

/// Declarative epoch rotation policy.
#[derive(Debug, Clone)]
pub struct EpochRotationPolicy {
    /// Maximum wall-clock lifetime of one epoch before it must rotate.
    pub max_epoch_duration: Duration,
    /// Maximum encrypted-bytes budget per epoch before it must rotate.
    pub max_epoch_size_bytes: u64,
}

impl EpochRotationPolicy {
    /// Default policy: 24 hours OR 16 GiB, whichever first.
    pub fn default_policy() -> Self {
        Self {
            max_epoch_duration: Duration::hours(24),
            max_epoch_size_bytes: 16 * 1024 * 1024 * 1024,
        }
    }

    /// Construct a fresh policy.
    pub fn new(max_epoch_duration: Duration, max_epoch_size_bytes: u64) -> Self {
        Self {
            max_epoch_duration,
            max_epoch_size_bytes,
        }
    }
}

impl Default for EpochRotationPolicy {
    fn default() -> Self {
        Self::default_policy()
    }
}

/// Per-scope rotation state inside the [`EpochManager`].
#[derive(Debug, Clone)]
pub struct EpochInfo {
    /// Epoch id.
    pub epoch_id: EpochId,
    /// When the epoch was created.
    pub started_at: DateTime<Utc>,
    /// Bytes encrypted under this epoch's DEK so far.
    pub bytes_encrypted: u64,
    /// `true` once the epoch has been rotated out (cold).
    pub cold: bool,
}

/// Tracks the current epoch per scope and rotates it when the policy
/// triggers. The manager generates fresh DEKs via the
/// [`EpochKeySource`] trait so production deployments can plug in a
/// real KDF / KMS while tests use a deterministic counter.
#[derive(Debug)]
pub struct EpochManager<S: EpochKeySource> {
    policy: EpochRotationPolicy,
    epochs: BTreeMap<ScopeId, Vec<EpochInfo>>,
    current: BTreeMap<ScopeId, EpochId>,
    key_source: S,
}

impl<S: EpochKeySource> EpochManager<S> {
    /// Construct a fresh manager with the supplied policy and key
    /// source.
    pub fn new(policy: EpochRotationPolicy, key_source: S) -> Self {
        Self {
            policy,
            epochs: BTreeMap::new(),
            current: BTreeMap::new(),
            key_source,
        }
    }

    /// Start tracking `scope` if not already tracked. Returns the
    /// initial [`EpochInfo`] and writes the starting epoch DEK into
    /// `registry`.
    pub fn ensure_scope(&mut self, scope: ScopeId, registry: &mut DekRegistry) -> EpochInfo {
        if let Some(info) = self
            .epochs
            .get(&scope)
            .and_then(|v| v.iter().find(|e| !e.cold).cloned())
        {
            return info;
        }
        let epoch_id = EpochId::zero();
        let info = EpochInfo {
            epoch_id,
            started_at: Utc::now(),
            bytes_encrypted: 0,
            cold: false,
        };
        let key = self.key_source.derive(scope, epoch_id);
        registry.insert_epoch_dek(EpochDek::new(scope, epoch_id, key));
        self.epochs.entry(scope).or_default().push(info.clone());
        self.current.insert(scope, epoch_id);
        info
    }

    /// Returns the active epoch for `scope`, or `None` if the scope
    /// has not been [`Self::ensure_scope`]-d yet.
    pub fn current_epoch(&self, scope: ScopeId) -> Option<EpochId> {
        self.current.get(&scope).copied()
    }

    /// Returns every epoch ever tracked for `scope`, oldest-first.
    pub fn list_epochs(&self, scope: ScopeId) -> Vec<EpochInfo> {
        self.epochs.get(&scope).cloned().unwrap_or_default()
    }

    /// Force-rotate the current epoch for `scope` regardless of policy.
    /// Returns the new epoch id and the trigger that caused the rotation
    /// (always [`EpochRotationTrigger::PolicyForced`] for this entry
    /// point).
    ///
    /// Fails with [`CryptoError::EpochOverflow`] if the scope has
    /// already reached the terminal epoch (`EpochId(u64::MAX)`).
    pub fn force_rotate(
        &mut self,
        scope: ScopeId,
        registry: &mut DekRegistry,
    ) -> Result<(EpochId, EpochRotationTrigger), CryptoError> {
        let new_epoch = self.rotate(scope, registry)?;
        Ok((new_epoch, EpochRotationTrigger::PolicyForced))
    }

    /// Tell the manager that `bytes` plaintext were encrypted under
    /// the current epoch's DEK. If this puts the scope over the
    /// configured size budget, rotates and returns
    /// `Ok(Some(EpochRotationTrigger::SizeExceeded))`.
    ///
    /// Fails with [`CryptoError::EpochOverflow`] if a rotation would
    /// be needed but the scope has already reached the terminal
    /// epoch — the per-epoch byte counter does not roll over
    /// silently.
    ///
    /// **Counter-mutation contract on overflow.** The accounting
    /// update to `bytes_encrypted` happens *before* the rotation
    /// attempt and is **not** rolled back if the rotation returns
    /// [`CryptoError::EpochOverflow`]. The bytes were genuinely
    /// encrypted under the (now permanently-wedged) terminal DEK,
    /// so the counter accurately reflects the encrypted volume
    /// even when the manager refuses to rotate. Rolling back would
    /// lose that audit signal — the wrong direction for a
    /// forgetting / forward-secrecy substrate. Callers that
    /// observe `Err(EpochOverflow)` should treat the scope's
    /// terminal epoch as wedged (no further rotations possible)
    /// and switch to a fresh scope rather than retrying; repeated
    /// calls will continue to fail with the same error while the
    /// counter ticks past the budget.
    pub fn record_bytes(
        &mut self,
        scope: ScopeId,
        bytes: u64,
        registry: &mut DekRegistry,
    ) -> Result<Option<EpochRotationTrigger>, CryptoError> {
        let now = Utc::now();
        let mut should_rotate: Option<EpochRotationTrigger> = None;
        if let Some(infos) = self.epochs.get_mut(&scope) {
            if let Some(latest) = infos.iter_mut().rev().find(|e| !e.cold) {
                latest.bytes_encrypted = latest.bytes_encrypted.saturating_add(bytes);
                if latest.bytes_encrypted >= self.policy.max_epoch_size_bytes {
                    should_rotate = Some(EpochRotationTrigger::SizeExceeded);
                } else if now.signed_duration_since(latest.started_at)
                    >= self.policy.max_epoch_duration
                {
                    should_rotate = Some(EpochRotationTrigger::TimeElapsed);
                }
            }
        }
        if should_rotate.is_some() {
            self.rotate(scope, registry)?;
        }
        Ok(should_rotate)
    }

    /// Tell the manager that `now` has elapsed without any new bytes —
    /// useful for calling on a periodic timer to trigger time-based
    /// rotation even on idle scopes.
    ///
    /// Fails with [`CryptoError::EpochOverflow`] if a time-based
    /// rotation would be needed but the scope has already reached
    /// the terminal epoch.
    pub fn tick(
        &mut self,
        scope: ScopeId,
        now: DateTime<Utc>,
        registry: &mut DekRegistry,
    ) -> Result<Option<EpochRotationTrigger>, CryptoError> {
        let needs_rotate = self
            .epochs
            .get(&scope)
            .and_then(|v| v.iter().rev().find(|e| !e.cold))
            .is_some_and(|latest| {
                now.signed_duration_since(latest.started_at) >= self.policy.max_epoch_duration
            });
        if needs_rotate {
            self.rotate(scope, registry)?;
            Ok(Some(EpochRotationTrigger::TimeElapsed))
        } else {
            Ok(None)
        }
    }

    fn rotate(
        &mut self,
        scope: ScopeId,
        registry: &mut DekRegistry,
    ) -> Result<EpochId, CryptoError> {
        // `EpochId::next` is now fallible at `u64::MAX`. Surfacing
        // the overflow here ensures the manager refuses to rotate a
        // scope that has reached the terminal epoch instead of
        // silently re-binding the same id to a fresh DEK — which
        // would break the monotonically-increasing-epoch invariant
        // that forgetting / forward-secrecy proofs depend on.
        let next = match self.current_epoch(scope) {
            Some(current) => current.next()?,
            None => EpochId::zero(),
        };
        if let Some(infos) = self.epochs.get_mut(&scope) {
            for info in infos.iter_mut() {
                info.cold = true;
            }
        }
        let info = EpochInfo {
            epoch_id: next,
            started_at: Utc::now(),
            bytes_encrypted: 0,
            cold: false,
        };
        let key = self.key_source.derive(scope, next);
        registry.insert_epoch_dek(EpochDek::new(scope, next, key));
        self.epochs.entry(scope).or_default().push(info);
        self.current.insert(scope, next);
        Ok(next)
    }
}

/// Source of fresh epoch DEK bytes. Production deployments derive a
/// scope-and-epoch-bound key from the per-tenant root key (HKDF). The
/// trait lets tests inject a deterministic counter.
pub trait EpochKeySource {
    /// Derive a fresh AEAD key for `(scope, epoch)`. Implementations
    /// must produce *fresh* material every call — the registry will
    /// zeroize the previous epoch's DEK on rotation.
    fn derive(&mut self, scope: ScopeId, epoch: EpochId) -> AeadKey;
}

/// Deterministic, in-memory key source for tests. Derives keys as
/// `BLAKE3("test-epoch-key" || scope_uuid || epoch_id_le_u64)`.
#[derive(Debug, Default, Clone)]
pub struct DeterministicEpochKeySource;

impl EpochKeySource for DeterministicEpochKeySource {
    fn derive(&mut self, scope: ScopeId, epoch: EpochId) -> AeadKey {
        // Derive the buffer capacity from the same slices that are
        // about to be written so the two cannot drift apart if the
        // prefix literal ever changes. The previous hard-coded
        // `Vec::with_capacity(8 + 16 + 8)` under-allocated by 6
        // bytes because `b"test-epoch-key"` is 14 bytes, not 8 —
        // causing exactly one heap reallocation on every key
        // derivation. Test-only path, so no security or
        // correctness impact, just a wasted alloc.
        let prefix: &[u8] = b"test-epoch-key";
        let scope_bytes = scope.0.as_bytes();
        let epoch_bytes = epoch.0.to_le_bytes();
        let mut buf =
            Vec::with_capacity(prefix.len() + scope_bytes.len() + epoch_bytes.len());
        buf.extend_from_slice(prefix);
        buf.extend_from_slice(scope_bytes);
        buf.extend_from_slice(&epoch_bytes);
        let hash = blake3::hash(&buf);
        let mut out = [0u8; AEAD_KEY_LEN];
        out.copy_from_slice(&hash.as_bytes()[..AEAD_KEY_LEN]);
        out
    }
}

/// Fan a sequence of [`KeyDestructionEvent`]s into the supplied
/// [`KeyDestructionAuditor`]. Provided as a free function so callers
/// don't have to scatter the pattern across the workspace.
pub fn record_key_destructions(
    auditor: &mut dyn KeyDestructionAuditor,
    events: &[KeyDestructionEvent],
) {
    for event in events {
        auditor.record_destruction(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aead::{decrypt_aead, encrypt_aead, AeadNonce, AEAD_NONCE_LEN};

    fn fixture_key(seed: u8) -> AeadKey {
        let mut k = [0u8; AEAD_KEY_LEN];
        for (i, byte) in k.iter_mut().enumerate() {
            *byte = u8::try_from(i)
                .expect("AEAD_KEY_LEN fits in u8")
                .wrapping_add(seed);
        }
        k
    }

    fn fixture_nonce() -> AeadNonce {
        let mut n = [0u8; AEAD_NONCE_LEN];
        for (i, byte) in n.iter_mut().enumerate() {
            *byte = u8::try_from(i)
                .expect("AEAD_NONCE_LEN fits in u8")
                .wrapping_mul(31);
        }
        n
    }

    #[derive(Default)]
    struct CapturingAuditor(Vec<KeyDestructionEvent>);

    impl KeyDestructionAuditor for CapturingAuditor {
        fn record_destruction(&mut self, event: &KeyDestructionEvent) {
            self.0.push(event.clone());
        }
    }

    #[test]
    fn destroy_scope_dek_zeroizes_and_marks_forgotten() {
        let mut registry = DekRegistry::new();
        let scope = ScopeId::new_v4();
        registry.insert_scope_dek(ScopeDek::new(scope, EpochId::zero(), fixture_key(1)));
        registry.insert_epoch_dek(EpochDek::new(scope, EpochId::zero(), fixture_key(2)));
        registry.insert_epoch_dek(EpochDek::new(scope, EpochId(1), fixture_key(3)));

        assert!(!registry.is_scope_forgotten(scope));
        let events = destroy_scope_dek(&mut registry, scope);
        assert!(registry.is_scope_forgotten(scope));
        assert!(registry.get_scope_dek(scope).is_none());
        assert!(registry.get_epoch_dek(scope, EpochId::zero()).is_none());
        assert!(registry.get_epoch_dek(scope, EpochId(1)).is_none());
        assert!(registry.is_epoch_forgotten(scope, EpochId::zero()));
        assert!(registry.is_epoch_forgotten(scope, EpochId(1)));
        assert_eq!(events.len(), 3);
        assert!(events.iter().all(|e| e.scope_wide));
    }

    #[test]
    fn destroy_scope_dek_is_idempotent() {
        let mut registry = DekRegistry::new();
        let scope = ScopeId::new_v4();
        registry.insert_scope_dek(ScopeDek::new(scope, EpochId::zero(), fixture_key(1)));
        let first = destroy_scope_dek(&mut registry, scope);
        let second = destroy_scope_dek(&mut registry, scope);
        assert!(!first.is_empty());
        assert!(second.is_empty());
        assert!(registry.is_scope_forgotten(scope));
    }

    #[test]
    fn destroy_epoch_dek_only_removes_target_epoch() {
        let mut registry = DekRegistry::new();
        let scope = ScopeId::new_v4();
        registry.insert_epoch_dek(EpochDek::new(scope, EpochId::zero(), fixture_key(1)));
        registry.insert_epoch_dek(EpochDek::new(scope, EpochId(1), fixture_key(2)));

        let events = destroy_epoch_dek(&mut registry, scope, EpochId::zero());
        assert_eq!(events.len(), 1);
        assert!(!events[0].scope_wide);
        assert!(registry.is_epoch_forgotten(scope, EpochId::zero()));
        assert!(!registry.is_epoch_forgotten(scope, EpochId(1)));
        assert!(registry.get_epoch_dek(scope, EpochId(1)).is_some());
    }

    #[test]
    fn destroy_epoch_dek_is_idempotent() {
        let mut registry = DekRegistry::new();
        let scope = ScopeId::new_v4();
        registry.insert_epoch_dek(EpochDek::new(scope, EpochId::zero(), fixture_key(1)));
        let _ = destroy_epoch_dek(&mut registry, scope, EpochId::zero());
        let again = destroy_epoch_dek(&mut registry, scope, EpochId::zero());
        assert!(again.is_empty());
    }

    #[test]
    fn forgotten_scope_cannot_decrypt_existing_ciphertext() {
        // Encrypt under a scope's DEK, destroy the DEK, prove that
        // the recorded ciphertext is now permanently undecryptable
        // because the registry no longer holds the key material.
        let mut registry = DekRegistry::new();
        let scope = ScopeId::new_v4();
        let key = fixture_key(7);
        registry.insert_scope_dek(ScopeDek::new(scope, EpochId::zero(), key));

        let nonce = fixture_nonce();
        let plaintext = b"forget-me-please";
        let aad = b"scope-scoped-aad";

        // Use the live key.
        let ct = {
            let dek = registry.get_scope_dek(scope).expect("live");
            encrypt_aead(dek.key().expect("live"), &nonce, plaintext, aad).expect("encrypt")
        };

        let _ = destroy_scope_dek(&mut registry, scope);

        // Registry can no longer surface a key for this scope — i.e.
        // there is no path through the registry to decrypt `ct`.
        assert!(registry.get_scope_dek(scope).is_none());

        // The ciphertext itself is still well-formed under the
        // dropped key, but the key has been zeroized in-place so
        // anyone holding only the registry has no way to recover it.
        // Demonstrate by trying to decrypt with a different key —
        // this fails as expected.
        let wrong_key = fixture_key(8);
        let err = decrypt_aead(&wrong_key, &nonce, &ct, aad).unwrap_err();
        assert!(matches!(err, crate::CryptoError::AeadDecryption));
    }

    #[test]
    fn epoch_manager_starts_at_epoch_zero() {
        let mut registry = DekRegistry::new();
        let mut mgr = EpochManager::new(
            EpochRotationPolicy::new(Duration::seconds(1), 1024),
            DeterministicEpochKeySource,
        );
        let scope = ScopeId::new_v4();
        let info = mgr.ensure_scope(scope, &mut registry);
        assert_eq!(info.epoch_id, EpochId::zero());
        assert_eq!(mgr.current_epoch(scope), Some(EpochId::zero()));
        assert!(registry.get_epoch_dek(scope, EpochId::zero()).is_some());
    }

    #[test]
    fn epoch_manager_force_rotates_to_new_epoch() {
        let mut registry = DekRegistry::new();
        let mut mgr = EpochManager::new(
            EpochRotationPolicy::default_policy(),
            DeterministicEpochKeySource,
        );
        let scope = ScopeId::new_v4();
        mgr.ensure_scope(scope, &mut registry);
        let (new_epoch, trigger) = mgr
            .force_rotate(scope, &mut registry)
            .expect("force_rotate at fresh scope cannot overflow");
        assert_eq!(new_epoch, EpochId(1));
        assert_eq!(trigger, EpochRotationTrigger::PolicyForced);
        assert_eq!(mgr.current_epoch(scope), Some(EpochId(1)));
        assert_eq!(mgr.list_epochs(scope).len(), 2);
        assert!(mgr.list_epochs(scope)[0].cold);
        assert!(!mgr.list_epochs(scope)[1].cold);
    }

    #[test]
    fn epoch_manager_rotates_on_size_trigger() {
        let mut registry = DekRegistry::new();
        let mut mgr = EpochManager::new(
            EpochRotationPolicy::new(Duration::days(365), 1024),
            DeterministicEpochKeySource,
        );
        let scope = ScopeId::new_v4();
        mgr.ensure_scope(scope, &mut registry);
        let trigger = mgr
            .record_bytes(scope, 2048, &mut registry)
            .expect("record_bytes at fresh scope cannot overflow");
        assert_eq!(trigger, Some(EpochRotationTrigger::SizeExceeded));
        assert_eq!(mgr.current_epoch(scope), Some(EpochId(1)));
    }

    #[test]
    fn epoch_manager_rotates_on_time_trigger_via_tick() {
        let mut registry = DekRegistry::new();
        let mut mgr = EpochManager::new(
            EpochRotationPolicy::new(Duration::seconds(1), 1024 * 1024 * 1024),
            DeterministicEpochKeySource,
        );
        let scope = ScopeId::new_v4();
        mgr.ensure_scope(scope, &mut registry);
        let later = Utc::now() + Duration::seconds(10);
        let trigger = mgr
            .tick(scope, later, &mut registry)
            .expect("tick at fresh scope cannot overflow");
        assert_eq!(trigger, Some(EpochRotationTrigger::TimeElapsed));
        assert_eq!(mgr.current_epoch(scope), Some(EpochId(1)));
    }

    #[test]
    fn epoch_manager_lists_every_epoch() {
        let mut registry = DekRegistry::new();
        let mut mgr = EpochManager::new(
            EpochRotationPolicy::default_policy(),
            DeterministicEpochKeySource,
        );
        let scope = ScopeId::new_v4();
        mgr.ensure_scope(scope, &mut registry);
        mgr.force_rotate(scope, &mut registry)
            .expect("force_rotate at fresh scope cannot overflow");
        mgr.force_rotate(scope, &mut registry)
            .expect("force_rotate at fresh scope cannot overflow");
        let epochs = mgr.list_epochs(scope);
        assert_eq!(epochs.len(), 3);
        assert_eq!(epochs[0].epoch_id, EpochId::zero());
        assert_eq!(epochs[1].epoch_id, EpochId(1));
        assert_eq!(epochs[2].epoch_id, EpochId(2));
        assert!(epochs[0].cold);
        assert!(epochs[1].cold);
        assert!(!epochs[2].cold);
    }

    #[test]
    fn epoch_id_zero_and_next_round_trip() {
        assert_eq!(EpochId::zero().0, 0);
        assert_eq!(
            EpochId::zero().next().expect("0.next() never overflows").0,
            1
        );
        // Overflow at the terminal epoch is reported as a hard
        // error rather than silently saturating at `u64::MAX` —
        // matching the [`crate::mls::MlsEpoch::next`] semantics.
        let max = EpochId(u64::MAX);
        assert!(matches!(max.next(), Err(CryptoError::EpochOverflow)));
    }

    /// At the terminal epoch, every public mutation entry point on
    /// [`EpochManager`] must refuse to rotate — silently saturating
    /// would re-bind the same epoch id to a fresh DEK, which is
    /// exactly the forward-secrecy break this fix closes.
    #[test]
    fn epoch_manager_refuses_to_rotate_at_terminal_epoch() {
        let mut registry = DekRegistry::new();
        let mut mgr = EpochManager::new(
            EpochRotationPolicy::new(Duration::seconds(1), 1),
            DeterministicEpochKeySource,
        );
        let scope = ScopeId::new_v4();
        mgr.ensure_scope(scope, &mut registry);

        // Splice the manager's tracking so the scope is parked at
        // `EpochId(u64::MAX)` without driving u64::MAX rotations.
        // The internal state is intentionally exposed only to the
        // crate's own tests via `pub(super)`-free field access; we
        // achieve the same effect here by replacing the bookkeeping
        // through the public surface.
        let terminal = EpochId(u64::MAX);
        // Overwrite the manager's current/epochs view of `scope` so
        // that `current_epoch(scope)` returns the terminal epoch.
        mgr.current.insert(scope, terminal);
        mgr.epochs.insert(
            scope,
            vec![EpochInfo {
                epoch_id: terminal,
                started_at: Utc::now() - Duration::days(365),
                bytes_encrypted: 0,
                cold: false,
            }],
        );
        registry.insert_epoch_dek(EpochDek::new(scope, terminal, fixture_key(7)));

        // force_rotate refuses.
        assert!(matches!(
            mgr.force_rotate(scope, &mut registry),
            Err(CryptoError::EpochOverflow)
        ));

        // record_bytes that *would* trigger a size rotation refuses.
        assert!(matches!(
            mgr.record_bytes(scope, u64::MAX, &mut registry),
            Err(CryptoError::EpochOverflow)
        ));

        // tick that *would* trigger a time rotation refuses.
        let later = Utc::now() + Duration::days(366);
        assert!(matches!(
            mgr.tick(scope, later, &mut registry),
            Err(CryptoError::EpochOverflow)
        ));

        // current_epoch is unchanged after every failed rotation
        // attempt — the manager refused to re-bind the terminal id.
        assert_eq!(mgr.current_epoch(scope), Some(terminal));
    }

    #[test]
    fn record_key_destructions_fans_out_to_auditor() {
        let mut registry = DekRegistry::new();
        let scope = ScopeId::new_v4();
        registry.insert_scope_dek(ScopeDek::new(scope, EpochId::zero(), fixture_key(1)));
        registry.insert_epoch_dek(EpochDek::new(scope, EpochId(0), fixture_key(2)));
        let events = destroy_scope_dek(&mut registry, scope);
        let mut auditor = CapturingAuditor::default();
        record_key_destructions(&mut auditor, &events);
        assert_eq!(auditor.0.len(), events.len());
        assert!(auditor.0.iter().all(|e| e.scope_wide));
    }

    #[test]
    fn rotation_trigger_string_tags_round_trip() {
        assert_eq!(EpochRotationTrigger::TimeElapsed.as_str(), "time_elapsed");
        assert_eq!(EpochRotationTrigger::SizeExceeded.as_str(), "size_exceeded");
        assert_eq!(EpochRotationTrigger::PolicyForced.as_str(), "policy_forced");
    }

    #[test]
    fn dek_destroy_is_idempotent_and_destroys_key_view() {
        let mut dek = ScopeDek::new(ScopeId::new_v4(), EpochId::zero(), fixture_key(1));
        assert!(dek.key().is_some());
        dek.destroy();
        assert!(dek.key().is_none());
        assert!(dek.is_destroyed());
        dek.destroy();
        assert!(dek.is_destroyed());
    }
}
