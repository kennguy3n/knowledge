//! Skeletal MLS (Messaging Layer Security) group keying.
//!
//! Per `PROPOSAL.md` §9.3, the substrate uses MLS-style group keying
//! to share encrypted memory across the members of a channel: every
//! member's leaf in the MLS tree publishes a hybrid X25519 +
//! ML-KEM-768 [`LeafKeyPackage`] and the group derives a per-epoch
//! [`GroupKeySchedule`] off the tree root.
//!
//! This module is **deliberately simplified** — it implements the
//! data model and the key schedule necessary to drive shared-memory
//! encryption inside the substrate, but it is not a full RFC 9420
//! implementation. The production crate target is the upstream
//! `openmls` fork at `kennguy3n/openmls`. The local skeleton is
//! sufficient for:
//!
//! * Group lifecycle (`create`, `add_member`, `remove_member`).
//! * Per-epoch group secret derivation via HKDF-SHA256.
//! * Commit signing/verification through the existing
//!   [`crate::signer_backend::SignerBackend`] abstraction (so the
//!   ML-DSA-65 signer drops in directly with no wrapper code).
//! * Welcome envelope construction so a new member can be admitted to
//!   the existing group state.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use hkdf::Hkdf;
use sha2::Sha256;
use uuid::Uuid;
use zeroize::Zeroize;

use crate::aead::{AeadKey, AEAD_KEY_LEN};
use crate::errors::CryptoError;
use crate::hybrid_kem::HybridPublicKey;
use crate::kem::{KemPublicKey, KEM_PUBLIC_KEY_LEN};
use crate::signer_backend::SignerBackend;

/// Identifier of an MLS group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MlsGroupId(pub Uuid);

impl MlsGroupId {
    /// Generate a fresh random group id.
    pub fn new_v4() -> Self {
        Self(Uuid::new_v4())
    }
}

/// Identifier of an MLS member (mirror of `member_id` in
/// `permission_service`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MlsMemberId(pub Uuid);

impl MlsMemberId {
    /// Generate a fresh random member id.
    pub fn new_v4() -> Self {
        Self(Uuid::new_v4())
    }
}

/// Monotonic epoch counter — bumped on every commit that mutates the
/// group state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MlsEpoch(pub u64);

impl MlsEpoch {
    /// Genesis epoch.
    pub const fn zero() -> Self {
        Self(0)
    }

    /// Next epoch (saturating at `u64::MAX`).
    pub fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

/// Public material a member publishes to be admitted to a group.
/// Carries the hybrid X25519 + ML-KEM-768 KEM specified in
/// `PROPOSAL.md` §9.
#[derive(Debug, Clone)]
pub struct LeafKeyPackage {
    /// Member identifier.
    pub member_id: MlsMemberId,
    /// Hybrid public key (classical + PQ).
    pub init_key: HybridPublicKey,
    /// Wall-clock when this key package was generated.
    pub created_at: DateTime<Utc>,
    /// Cipher-suite tag (informational).
    pub cipher_suite: &'static str,
}

impl LeafKeyPackage {
    /// Construct a leaf key package.
    pub fn new(member_id: MlsMemberId, init_key: HybridPublicKey) -> Self {
        Self {
            member_id,
            init_key,
            created_at: Utc::now(),
            cipher_suite: "x25519+mlkem768/aes256-gcm/sha256/ed25519-or-mldsa65",
        }
    }
}

/// What kind of state change a commit encodes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitOperation {
    /// Group creation by `creator`. The commit body lists the
    /// initial roster.
    Create {
        /// Group creator.
        creator: MlsMemberId,
        /// Initial members.
        roster: Vec<MlsMemberId>,
    },
    /// Add a member.
    Add {
        /// Member being added.
        added: MlsMemberId,
    },
    /// Remove a member.
    Remove {
        /// Member being removed.
        removed: MlsMemberId,
    },
}

/// A signed MLS commit. The signature covers the canonical encoding
/// returned by [`MlsCommit::signing_payload`].
#[derive(Debug, Clone)]
pub struct MlsCommit {
    /// Group this commit belongs to.
    pub group_id: MlsGroupId,
    /// Epoch the group will be at *after* this commit is applied.
    pub epoch: MlsEpoch,
    /// State-change operation.
    pub operation: CommitOperation,
    /// Member that produced and signed the commit.
    pub committed_by: MlsMemberId,
    /// Wall-clock at signing time.
    pub committed_at: DateTime<Utc>,
    /// Signature over [`MlsCommit::signing_payload`].
    pub signature: Vec<u8>,
}

impl MlsCommit {
    /// Canonical bytes signed by [`MlsCommit::signature`]. Stable
    /// concatenation of the structural fields — the same bytes are
    /// produced by sender and receiver so verification is symmetric.
    pub fn signing_payload(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(self.group_id.0.as_bytes());
        out.extend_from_slice(&self.epoch.0.to_be_bytes());
        match &self.operation {
            CommitOperation::Create { creator, roster } => {
                out.push(0x01);
                out.extend_from_slice(creator.0.as_bytes());
                out.extend_from_slice(&(roster.len() as u32).to_be_bytes());
                for m in roster {
                    out.extend_from_slice(m.0.as_bytes());
                }
            }
            CommitOperation::Add { added } => {
                out.push(0x02);
                out.extend_from_slice(added.0.as_bytes());
            }
            CommitOperation::Remove { removed } => {
                out.push(0x03);
                out.extend_from_slice(removed.0.as_bytes());
            }
        }
        out.extend_from_slice(self.committed_by.0.as_bytes());
        out.extend_from_slice(
            &self
                .committed_at
                .timestamp_nanos_opt()
                .unwrap_or(0)
                .to_be_bytes(),
        );
        out
    }
}

/// Welcome envelope handed to a freshly-admitted member so they can
/// reconstruct the group state.
#[derive(Debug, Clone)]
pub struct MlsWelcome {
    /// Group the member is being welcomed into.
    pub group_id: MlsGroupId,
    /// Epoch the group is at when the welcome is issued.
    pub epoch: MlsEpoch,
    /// Roster the new member should see.
    pub roster: Vec<MlsMemberId>,
    /// Per-epoch secret to bootstrap the new member's view of the
    /// key schedule. In a full RFC 9420 implementation this would be
    /// encrypted to the member's leaf init key; here we encode the
    /// raw 32-byte secret since the registry is in-memory.
    pub epoch_secret: [u8; AEAD_KEY_LEN],
}

/// Per-epoch group key schedule. Holds the root secret and the keys
/// derived from it.
#[derive(Clone)]
pub struct GroupKeySchedule {
    /// Group identifier.
    pub group_id: MlsGroupId,
    /// Epoch.
    pub epoch: MlsEpoch,
    /// Root 32-byte epoch secret. Wiped on drop.
    epoch_secret: [u8; AEAD_KEY_LEN],
    /// Derived shared-memory AEAD key.
    pub shared_memory_key: AeadKey,
    /// Derived sender-data key (used for confidential channel headers).
    pub sender_data_key: AeadKey,
    /// Derived welcome-envelope key.
    pub welcome_key: AeadKey,
}

impl GroupKeySchedule {
    /// Borrow the raw epoch secret.
    pub fn epoch_secret(&self) -> &[u8; AEAD_KEY_LEN] {
        &self.epoch_secret
    }
}

impl std::fmt::Debug for GroupKeySchedule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GroupKeySchedule")
            .field("group_id", &self.group_id)
            .field("epoch", &self.epoch)
            .field("epoch_secret", &"<redacted>")
            .field("shared_memory_key", &"<redacted>")
            .field("sender_data_key", &"<redacted>")
            .field("welcome_key", &"<redacted>")
            .finish()
    }
}

impl Drop for GroupKeySchedule {
    fn drop(&mut self) {
        self.epoch_secret.zeroize();
        self.shared_memory_key.zeroize();
        self.sender_data_key.zeroize();
        self.welcome_key.zeroize();
    }
}

impl Drop for MlsGroup {
    fn drop(&mut self) {
        // The seed is the root from which every epoch secret is
        // HKDF-derived; wiping it on drop matches the behaviour of
        // [`GroupKeySchedule`] and the per-scope/per-epoch DEKs in
        // [`crate::forgetting`].
        self.seed.zeroize();
    }
}

/// Derive a [`GroupKeySchedule`] from a 32-byte epoch root secret.
fn derive_schedule(
    group_id: MlsGroupId,
    epoch: MlsEpoch,
    epoch_secret: [u8; AEAD_KEY_LEN],
) -> Result<GroupKeySchedule, CryptoError> {
    let salt = b"knowledge-mls-key-schedule-v1";
    let hk = Hkdf::<Sha256>::new(Some(salt), &epoch_secret);
    let mut shared = [0u8; AEAD_KEY_LEN];
    let mut sender = [0u8; AEAD_KEY_LEN];
    let mut welcome = [0u8; AEAD_KEY_LEN];
    hk.expand(b"shared-memory", &mut shared)
        .map_err(|_| CryptoError::KeyDerivation("HKDF expand shared-memory failed"))?;
    hk.expand(b"sender-data", &mut sender)
        .map_err(|_| CryptoError::KeyDerivation("HKDF expand sender-data failed"))?;
    hk.expand(b"welcome", &mut welcome)
        .map_err(|_| CryptoError::KeyDerivation("HKDF expand welcome failed"))?;

    Ok(GroupKeySchedule {
        group_id,
        epoch,
        epoch_secret,
        shared_memory_key: shared,
        sender_data_key: sender,
        welcome_key: welcome,
    })
}

/// Mutable group state. Tracks the current epoch, roster, and key
/// schedule. Mutations always go through a signed [`MlsCommit`].
#[derive(Debug)]
pub struct MlsGroup {
    /// Group identifier.
    pub group_id: MlsGroupId,
    /// Current epoch.
    pub epoch: MlsEpoch,
    /// Current member set, keyed by id.
    pub members: BTreeMap<MlsMemberId, LeafKeyPackage>,
    /// Active key schedule for `epoch`.
    pub schedule: GroupKeySchedule,
    /// Random secret seed used to derive successor-epoch secrets via
    /// HKDF. Wiped on drop with the schedule.
    seed: [u8; AEAD_KEY_LEN],
}

impl MlsGroup {
    /// Create a new MLS group with `creator` as the only member.
    /// Signs and returns the genesis [`MlsCommit`] alongside the
    /// initialised state.
    pub fn create<S: SignerBackend>(
        signer: &S,
        creator: MlsMemberId,
        creator_leaf: LeafKeyPackage,
        seed: [u8; AEAD_KEY_LEN],
    ) -> Result<(Self, MlsCommit), CryptoError> {
        let group_id = MlsGroupId::new_v4();
        let epoch = MlsEpoch::zero();
        let epoch_secret = derive_epoch_secret(&seed, group_id, epoch)?;
        let schedule = derive_schedule(group_id, epoch, epoch_secret)?;

        let mut members = BTreeMap::new();
        members.insert(creator, creator_leaf);

        let mut commit = MlsCommit {
            group_id,
            epoch,
            operation: CommitOperation::Create {
                creator,
                roster: vec![creator],
            },
            committed_by: creator,
            committed_at: Utc::now(),
            signature: Vec::new(),
        };
        commit.signature = signer.sign_bytes(&commit.signing_payload())?;

        Ok((
            Self {
                group_id,
                epoch,
                members,
                schedule,
                seed,
            },
            commit,
        ))
    }

    /// Build (sign) an [`MlsCommit::Add`] for `leaf`. The commit is
    /// not applied to local state until [`MlsGroup::process_commit`]
    /// runs against it — this matches RFC 9420's split between
    /// "build a commit" and "process it".
    pub fn add_member<S: SignerBackend>(
        &self,
        signer: &S,
        committed_by: MlsMemberId,
        leaf: LeafKeyPackage,
    ) -> Result<MlsCommit, CryptoError> {
        if self.members.contains_key(&leaf.member_id) {
            return Err(CryptoError::ProvenanceSerialisation(
                "member already in group",
            ));
        }
        let mut commit = MlsCommit {
            group_id: self.group_id,
            epoch: self.epoch.next(),
            operation: CommitOperation::Add {
                added: leaf.member_id,
            },
            committed_by,
            committed_at: Utc::now(),
            signature: Vec::new(),
        };
        commit.signature = signer.sign_bytes(&commit.signing_payload())?;
        Ok(commit)
    }

    /// Build (sign) an [`MlsCommit::Remove`].
    pub fn remove_member<S: SignerBackend>(
        &self,
        signer: &S,
        committed_by: MlsMemberId,
        removed: MlsMemberId,
    ) -> Result<MlsCommit, CryptoError> {
        if !self.members.contains_key(&removed) {
            return Err(CryptoError::ProvenanceSerialisation("member not in group"));
        }
        let mut commit = MlsCommit {
            group_id: self.group_id,
            epoch: self.epoch.next(),
            operation: CommitOperation::Remove { removed },
            committed_by,
            committed_at: Utc::now(),
            signature: Vec::new(),
        };
        commit.signature = signer.sign_bytes(&commit.signing_payload())?;
        Ok(commit)
    }

    /// Apply `commit` to the group state. Verifies the signature
    /// using `verifier`, updates the roster and bumps the epoch, then
    /// derives a fresh [`GroupKeySchedule`].
    pub fn process_commit<V: SignerBackend>(
        &mut self,
        commit: &MlsCommit,
        verifier: &V,
    ) -> Result<(), CryptoError> {
        if commit.group_id != self.group_id {
            return Err(CryptoError::ProvenanceSerialisation(
                "commit group_id mismatch",
            ));
        }
        let payload = commit.signing_payload();
        if !verifier.verify_bytes(&payload, &commit.signature)? {
            return Err(CryptoError::ProvenanceVerification);
        }
        // `Create` commits are consumed by [`MlsGroup::create`] and
        // must never be replayed against an existing group. Accepting
        // them here would silently advance the epoch to whatever the
        // (signed) commit claimed, desynchronising the key schedule
        // from every other member.
        if matches!(commit.operation, CommitOperation::Create { .. }) {
            return Err(CryptoError::ProvenanceSerialisation(
                "Create commits cannot be processed on an existing group",
            ));
        }
        if commit.epoch != self.epoch.next() {
            return Err(CryptoError::ProvenanceSerialisation(
                "commit epoch out of order",
            ));
        }
        match &commit.operation {
            CommitOperation::Create { .. } => unreachable!("Create rejected above"),
            CommitOperation::Add { added } => {
                if self.members.contains_key(added) {
                    return Err(CryptoError::ProvenanceSerialisation(
                        "member already present",
                    ));
                }
                // The new leaf key package is delivered via the
                // welcome envelope; we don't have it here. Insert a
                // placeholder leaf so the roster is consistent and
                // the caller can patch in the real one when they
                // apply the welcome.
                self.members.insert(
                    *added,
                    LeafKeyPackage {
                        member_id: *added,
                        init_key: placeholder_hybrid_pk(),
                        created_at: Utc::now(),
                        cipher_suite: "x25519+mlkem768/aes256-gcm/sha256/ed25519-or-mldsa65",
                    },
                );
            }
            CommitOperation::Remove { removed } => {
                self.members.remove(removed);
            }
        }
        self.epoch = commit.epoch;
        let secret = derive_epoch_secret(&self.seed, self.group_id, self.epoch)?;
        self.schedule = derive_schedule(self.group_id, self.epoch, secret)?;
        Ok(())
    }

    /// Build a [`MlsWelcome`] envelope for a freshly added member.
    pub fn build_welcome(&self, _added: MlsMemberId) -> MlsWelcome {
        MlsWelcome {
            group_id: self.group_id,
            epoch: self.epoch,
            roster: self.members.keys().copied().collect(),
            epoch_secret: *self.schedule.epoch_secret(),
        }
    }

    /// Patch a known leaf key package into the roster (used after
    /// the welcome envelope is delivered).
    pub fn install_leaf(&mut self, leaf: LeafKeyPackage) {
        self.members.insert(leaf.member_id, leaf);
    }
}

/// Derive the 32-byte epoch secret from `(seed, group_id, epoch)`.
fn derive_epoch_secret(
    seed: &[u8; AEAD_KEY_LEN],
    group_id: MlsGroupId,
    epoch: MlsEpoch,
) -> Result<[u8; AEAD_KEY_LEN], CryptoError> {
    let salt = b"knowledge-mls-epoch-secret-v1";
    let hk = Hkdf::<Sha256>::new(Some(salt), seed);
    let mut info = Vec::with_capacity(16 + 8);
    info.extend_from_slice(group_id.0.as_bytes());
    info.extend_from_slice(&epoch.0.to_be_bytes());
    let mut out = [0u8; AEAD_KEY_LEN];
    hk.expand(&info, &mut out)
        .map_err(|_| CryptoError::KeyDerivation("HKDF expand epoch-secret failed"))?;
    Ok(out)
}

/// Construct an all-zero placeholder hybrid public key. Used when a
/// commit observer doesn't have the real leaf key package yet.
fn placeholder_hybrid_pk() -> HybridPublicKey {
    let mlkem768: KemPublicKey = [0u8; KEM_PUBLIC_KEY_LEN];
    HybridPublicKey {
        x25519: [0u8; 32],
        mlkem768,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hybrid_kem::hybrid_keypair_with_backend;
    use crate::kem::StubKemBackend;
    use crate::signer_backend::MlDsa65Signer;

    fn fresh_leaf(member_id: MlsMemberId) -> LeafKeyPackage {
        let (pk, _sk) = hybrid_keypair_with_backend(&StubKemBackend).expect("keypair");
        LeafKeyPackage::new(member_id, pk)
    }

    fn signer() -> MlDsa65Signer {
        MlDsa65Signer::generate()
    }

    fn fixed_seed() -> [u8; AEAD_KEY_LEN] {
        let mut seed = [0u8; AEAD_KEY_LEN];
        for (i, b) in seed.iter_mut().enumerate() {
            *b = i as u8;
        }
        seed
    }

    #[test]
    fn create_group_yields_genesis_state() {
        let s = signer();
        let creator = MlsMemberId::new_v4();
        let leaf = fresh_leaf(creator);
        let (group, commit) = MlsGroup::create(&s, creator, leaf, fixed_seed()).expect("create");
        assert_eq!(group.epoch, MlsEpoch::zero());
        assert_eq!(group.members.len(), 1);
        assert_eq!(commit.epoch, MlsEpoch::zero());
        assert!(matches!(commit.operation, CommitOperation::Create { .. }));
        assert!(!commit.signature.is_empty());
    }

    #[test]
    fn add_member_advances_epoch_and_grows_roster() {
        let s = signer();
        let creator = MlsMemberId::new_v4();
        let (mut group, _genesis) =
            MlsGroup::create(&s, creator, fresh_leaf(creator), fixed_seed()).unwrap();
        let new_id = MlsMemberId::new_v4();
        let leaf = fresh_leaf(new_id);
        let commit = group.add_member(&s, creator, leaf.clone()).unwrap();
        group.process_commit(&commit, &s).unwrap();
        group.install_leaf(leaf);
        assert_eq!(group.epoch, MlsEpoch(1));
        assert_eq!(group.members.len(), 2);
        assert!(group.members.contains_key(&new_id));
    }

    #[test]
    fn remove_member_shrinks_roster() {
        let s = signer();
        let creator = MlsMemberId::new_v4();
        let (mut group, _) =
            MlsGroup::create(&s, creator, fresh_leaf(creator), fixed_seed()).unwrap();
        let new_id = MlsMemberId::new_v4();
        let leaf = fresh_leaf(new_id);
        let add = group.add_member(&s, creator, leaf.clone()).unwrap();
        group.process_commit(&add, &s).unwrap();
        group.install_leaf(leaf);
        let remove = group.remove_member(&s, creator, new_id).unwrap();
        group.process_commit(&remove, &s).unwrap();
        assert_eq!(group.epoch, MlsEpoch(2));
        assert!(!group.members.contains_key(&new_id));
        assert_eq!(group.members.len(), 1);
    }

    #[test]
    fn key_schedule_changes_every_epoch() {
        let s = signer();
        let creator = MlsMemberId::new_v4();
        let (mut group, _) =
            MlsGroup::create(&s, creator, fresh_leaf(creator), fixed_seed()).unwrap();
        let secret_genesis = *group.schedule.epoch_secret();
        let key_genesis = group.schedule.shared_memory_key;
        let new_id = MlsMemberId::new_v4();
        let leaf = fresh_leaf(new_id);
        let add = group.add_member(&s, creator, leaf.clone()).unwrap();
        group.process_commit(&add, &s).unwrap();
        let secret_next = *group.schedule.epoch_secret();
        let key_next = group.schedule.shared_memory_key;
        assert_ne!(secret_genesis, secret_next);
        assert_ne!(key_genesis, key_next);
    }

    #[test]
    fn process_commit_rejects_bad_signature() {
        let s = signer();
        let creator = MlsMemberId::new_v4();
        let (mut group, _) =
            MlsGroup::create(&s, creator, fresh_leaf(creator), fixed_seed()).unwrap();
        let new_id = MlsMemberId::new_v4();
        let leaf = fresh_leaf(new_id);
        let mut bad = group.add_member(&s, creator, leaf).unwrap();
        bad.signature[0] ^= 0xFF;
        let err = group.process_commit(&bad, &s).unwrap_err();
        assert!(matches!(err, CryptoError::ProvenanceVerification));
    }

    #[test]
    fn process_commit_rejects_wrong_group_id() {
        let s = signer();
        let creator = MlsMemberId::new_v4();
        let (mut group, _) =
            MlsGroup::create(&s, creator, fresh_leaf(creator), fixed_seed()).unwrap();
        let new_id = MlsMemberId::new_v4();
        let leaf = fresh_leaf(new_id);
        let mut commit = group.add_member(&s, creator, leaf).unwrap();
        commit.group_id = MlsGroupId::new_v4();
        let err = group.process_commit(&commit, &s).unwrap_err();
        assert!(matches!(
            err,
            CryptoError::ProvenanceSerialisation("commit group_id mismatch")
        ));
    }

    #[test]
    fn welcome_carries_current_state() {
        let s = signer();
        let creator = MlsMemberId::new_v4();
        let (group, _) = MlsGroup::create(&s, creator, fresh_leaf(creator), fixed_seed()).unwrap();
        let welcome = group.build_welcome(creator);
        assert_eq!(welcome.group_id, group.group_id);
        assert_eq!(welcome.epoch, group.epoch);
        assert_eq!(welcome.roster, vec![creator]);
        assert_eq!(welcome.epoch_secret, *group.schedule.epoch_secret());
    }

    #[test]
    fn signing_payload_is_deterministic() {
        let s = signer();
        let creator = MlsMemberId::new_v4();
        let (_group, commit) =
            MlsGroup::create(&s, creator, fresh_leaf(creator), fixed_seed()).unwrap();
        assert_eq!(commit.signing_payload(), commit.signing_payload());
    }

    #[test]
    fn epoch_zero_and_next_round_trip() {
        assert_eq!(MlsEpoch::zero().0, 0);
        assert_eq!(MlsEpoch::zero().next().0, 1);
    }

    #[test]
    fn process_commit_rejects_create_on_existing_group() {
        // Regression: a signed `Create` commit replayed against an
        // already-established group must be rejected, not silently
        // jump the epoch and re-derive the key schedule.
        let s = signer();
        let creator = MlsMemberId::new_v4();
        let (mut group, genesis) =
            MlsGroup::create(&s, creator, fresh_leaf(creator), fixed_seed()).unwrap();
        // Re-process the genesis Create commit on the live group.
        let err = group.process_commit(&genesis, &s).unwrap_err();
        assert!(matches!(
            err,
            CryptoError::ProvenanceSerialisation(
                "Create commits cannot be processed on an existing group"
            )
        ));
        // State is unchanged.
        assert_eq!(group.epoch, MlsEpoch::zero());
        assert_eq!(group.members.len(), 1);
    }

    #[test]
    fn member_already_in_group_rejects_add() {
        let s = signer();
        let creator = MlsMemberId::new_v4();
        let (group, _) = MlsGroup::create(&s, creator, fresh_leaf(creator), fixed_seed()).unwrap();
        let err = group
            .add_member(&s, creator, fresh_leaf(creator))
            .unwrap_err();
        assert!(matches!(err, CryptoError::ProvenanceSerialisation(_)));
    }
}
