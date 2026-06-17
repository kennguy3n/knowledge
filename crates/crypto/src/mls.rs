//! Skeletal MLS (Messaging Layer Security) group keying.
//!
//! Per `docs/technical/design.md` §9.3, the substrate uses MLS-style group keying
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
//!
//! # Trust model and known limitation
//!
//! [`MlsGroup::process_commit`] verifies a commit's signature against
//! a single caller-supplied [`crate::signer_backend::SignerBackend`]
//! — one group-wide verifying key — rather than a per-member signing
//! key keyed by `committed_by`. A full RFC 9420 implementation binds
//! each leaf to its own signature credential, so a verified commit
//! authenticates *which* member produced it. In this skeleton the
//! signature only proves the commit was produced by *some* holder of
//! the recognised group key; committer attribution is therefore
//! enforced solely by current-roster membership, not bound to an
//! individual member's key. The practical consequence: a current
//! member holding the group key could attribute a commit to another
//! current member. Per-leaf credential authentication is part of the
//! work deferred to the `openmls` production target — until then,
//! callers must not treat `commit.committed_by` as a cryptographically
//! authenticated identity beyond "is a current group member".

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use hkdf::Hkdf;
use sha2::Sha256;
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};

use crate::aead::{decrypt_aead, encrypt_aead, AeadKey, AeadNonce, AEAD_KEY_LEN, AEAD_NONCE_LEN};
use crate::errors::CryptoError;
use crate::hybrid_kem::{
    hybrid_kem_decap, hybrid_kem_encap, HybridCiphertext, HybridPublicKey, HybridSecretKey,
    HybridSharedSecret,
};
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

    /// Next epoch, or [`CryptoError::EpochOverflow`] if the counter
    /// would overflow `u64::MAX`.
    ///
    /// Previously this saturated at `u64::MAX`, which silently
    /// permitted an unbounded sequence of commits at the terminal
    /// epoch: `commit.epoch == self.epoch.next()` would keep
    /// returning `true` and `process_commit` would keep consuming
    /// the ratchet for commits that did not actually advance the
    /// counter. Surfacing the overflow as a hard error makes the
    /// terminal condition observable and prevents that footgun. In
    /// practice no group ever reaches `u64::MAX`, but the semantic
    /// difference is worth getting right.
    pub fn next(self) -> Result<Self, CryptoError> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or(CryptoError::EpochOverflow)
    }
}

/// Public material a member publishes to be admitted to a group.
/// Carries the hybrid X25519 + ML-KEM-768 KEM specified in
/// `docs/technical/design.md` §9.
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
                let roster_len: u32 = roster
                    .len()
                    .try_into()
                    .expect("roster length exceeds u32::MAX");
                out.extend_from_slice(&roster_len.to_be_bytes());
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
///
/// Per `docs/technical/design.md` §9 the welcome must transport the per-epoch
/// secret confidentially to a single recipient. The secret (along
/// with the next ratchet value, so the new member can advance with
/// future commits) is wrapped under an XChaCha20-Poly1305 AEAD whose
/// key and nonce are derived from a hybrid X25519 + ML-KEM-768
/// shared secret that is itself KEM-encapsulated to the new member's
/// published [`LeafKeyPackage::init_key`]. The wrapped plaintext is
/// 64 bytes: `epoch_secret (32) || next_ratchet (32)`.
#[derive(Debug, Clone)]
pub struct MlsWelcome {
    /// Group the member is being welcomed into.
    pub group_id: MlsGroupId,
    /// Epoch the group is at when the welcome is issued.
    pub epoch: MlsEpoch,
    /// Roster the new member should see.
    pub roster: Vec<MlsMemberId>,
    /// Hybrid KEM ciphertext: ephemeral X25519 public key plus
    /// ML-KEM-768 ciphertext, produced by [`hybrid_kem_encap`] under
    /// the new member's [`HybridPublicKey`]. Decapsulating with the
    /// matching [`HybridSecretKey`] recovers the shared secret used
    /// to derive the welcome AEAD key/nonce.
    pub kem_ciphertext: HybridCiphertext,
    /// XChaCha20-Poly1305 ciphertext of `epoch_secret || next_ratchet`
    /// (64 bytes plaintext) under the welcome AEAD key derived from
    /// the hybrid KEM shared secret. The 16-byte Poly1305 tag is
    /// appended by [`encrypt_aead`].
    pub encrypted_epoch_secret: Vec<u8>,
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
            .finish()
    }
}

impl Drop for GroupKeySchedule {
    fn drop(&mut self) {
        self.epoch_secret.zeroize();
        self.shared_memory_key.zeroize();
        self.sender_data_key.zeroize();
    }
}

impl Drop for MlsGroup {
    fn drop(&mut self) {
        // The ratchet is the symmetric secret that, together with the
        // current epoch, derives the next epoch secret. Wiping it on
        // drop matches the behaviour of [`GroupKeySchedule`] and the
        // per-scope/per-epoch DEKs in [`crate::forgetting`], and is
        // what gives the rest of the design forward secrecy: once a
        // group is dropped, no past or future epoch secret can be
        // reconstructed from its in-memory state.
        self.ratchet.zeroize();
    }
}

/// Expand `epoch_secret` into the per-epoch [`GroupKeySchedule`].
///
/// The HKDF `info` for each output binds `group_id` and `epoch`
/// alongside a fixed domain-separation label. This is defence-in-
/// depth: `epoch_secret` is itself derived per `(group_id, epoch)`
/// inside [`ratchet_epoch`], so re-binding here is strictly
/// redundant for any caller that goes through `ratchet_epoch`.
/// Including it anyway makes [`derive_schedule`] safe to call with
/// any caller-supplied epoch secret — the derived keys are
/// unambiguously tied to a specific group and epoch even if the
/// input were ever recycled across contexts.
///
/// `epoch_secret` is taken as [`Zeroizing<[u8; AEAD_KEY_LEN]>`] so
/// the stack-frame copy this function receives is wiped on every
/// exit path (including the error branches). Callers that pass
/// their own [`Zeroizing`] binding by move get end-to-end
/// defence-in-depth: their stack frame is wiped on scope exit and
/// this function's frame is wiped on return.
fn derive_schedule(
    group_id: MlsGroupId,
    epoch: MlsEpoch,
    epoch_secret: Zeroizing<[u8; AEAD_KEY_LEN]>,
) -> Result<GroupKeySchedule, CryptoError> {
    let salt = b"knowledge-mls-key-schedule-v1";
    let hk = Hkdf::<Sha256>::new(Some(salt), epoch_secret.as_slice());
    // Bind `(group_id, epoch)` plus a per-output domain label into
    // each `info`. Reusing the same `prefix` keeps the two info
    // strings cheap to construct and ensures both derivations
    // share the same group/epoch tag.
    let mut prefix = Vec::with_capacity(16 + 8);
    prefix.extend_from_slice(group_id.0.as_bytes());
    prefix.extend_from_slice(&epoch.0.to_be_bytes());
    let info_shared = {
        let mut v = prefix.clone();
        v.extend_from_slice(b"|shared-memory");
        v
    };
    let info_sender = {
        let mut v = prefix;
        v.extend_from_slice(b"|sender-data");
        v
    };
    // The welcome envelope's AEAD key is **not** derived from the
    // group key schedule — it is derived per-welcome from the
    // hybrid KEM shared secret via [`derive_welcome_aead_material`].
    // We therefore intentionally do not expand a `welcome_key` here.
    let mut shared = [0u8; AEAD_KEY_LEN];
    let mut sender = [0u8; AEAD_KEY_LEN];
    hk.expand(&info_shared, &mut shared)
        .map_err(|_| CryptoError::KeyDerivation("HKDF expand shared-memory failed"))?;
    hk.expand(&info_sender, &mut sender)
        .map_err(|_| CryptoError::KeyDerivation("HKDF expand sender-data failed"))?;

    // Move the inner array into the schedule. `*epoch_secret`
    // dereferences and copies (`[u8; 32]: Copy`); the `Zeroizing`
    // wrapper is then dropped, wiping our stack-frame copy. The
    // schedule has its own `Drop` impl that wipes `epoch_secret`
    // when the schedule is dropped, so long-term storage is also
    // covered.
    let inner: [u8; AEAD_KEY_LEN] = *epoch_secret;
    drop(epoch_secret);
    Ok(GroupKeySchedule {
        group_id,
        epoch,
        epoch_secret: inner,
        shared_memory_key: shared,
        sender_data_key: sender,
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
    /// Current ratchet state — the HKDF output produced by the
    /// previous epoch's [`ratchet_epoch`] call (or the genesis
    /// initialiser for epoch 0). On every commit the ratchet is
    /// consumed to derive `(epoch_secret, next_ratchet)`; the old
    /// value is then zeroised and replaced with `next_ratchet`. This
    /// is what gives the group **forward secrecy**: compromising the
    /// current ratchet does not allow an attacker to recover any
    /// epoch secret from a previous epoch.
    ratchet: [u8; AEAD_KEY_LEN],
}

impl MlsGroup {
    /// Create a new MLS group with `creator` as the only member.
    /// Signs and returns the genesis [`MlsCommit`] alongside the
    /// initialised state.
    ///
    /// `initial_ratchet` is the high-entropy seed for the per-epoch
    /// HKDF ratchet. It is taken as [`Zeroizing<[u8; AEAD_KEY_LEN]>`]
    /// for two reasons:
    ///
    /// 1. [`Zeroizing`] is **not** `Copy` even though `[u8; 32]` is,
    ///    so the caller is forced to express ownership transfer
    ///    explicitly — accidental duplication of the genesis seed
    ///    via implicit copy is rejected at compile time.
    /// 2. The local copy this function receives is wrapped in
    ///    [`Zeroizing`], so its [`Drop`] impl wipes the bytes from
    ///    this stack frame on every exit path, including on error
    ///    returns. The genesis derivation consumes the value to
    ///    produce epoch 0's secret and the ratchet that will feed
    ///    epoch 1; no group member needs to retain the genesis seed
    ///    after creation.
    ///
    /// Note that Rust does not guarantee zeroisation of the **caller's**
    /// stack frame copy of the array — the bytes are `memcpy`'d into
    /// this function's frame at call-time and only this frame is
    /// wiped on return. Callers that need defence-in-depth at their
    /// own frame should also store the seed in a [`Zeroizing`] binding
    /// from the start, which is naturally enforced by this signature.
    pub fn create<S: SignerBackend>(
        signer: &S,
        creator: MlsMemberId,
        creator_leaf: LeafKeyPackage,
        initial_ratchet: Zeroizing<[u8; AEAD_KEY_LEN]>,
    ) -> Result<(Self, MlsCommit), CryptoError> {
        let group_id = MlsGroupId::new_v4();
        let epoch = MlsEpoch::zero();
        // `epoch_secret` and `next_ratchet` are both wrapped in
        // `Zeroizing` by `ratchet_epoch`, so the local bindings
        // here get wiped on scope exit — the bytes don't linger on
        // this stack frame after `create` returns.
        let (epoch_secret, next_ratchet) = ratchet_epoch(&initial_ratchet, group_id, epoch)?;
        // `initial_ratchet` is dropped here; its `Zeroizing` wrapper
        // wipes the bytes from our stack frame.
        drop(initial_ratchet);
        // `epoch_secret` moves into `derive_schedule`, which wipes
        // its own parameter copy on drop.
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

        // Copy the inner bytes out of `next_ratchet`. `*next_ratchet`
        // dereferences and copies (`[u8; 32]: Copy`); the original
        // `Zeroizing` wrapper is dropped before we return, wiping our
        // local stack-frame copy. Long-term storage lives in
        // `MlsGroup::ratchet` and is wiped by `Drop for MlsGroup`.
        let ratchet: [u8; AEAD_KEY_LEN] = *next_ratchet;
        drop(next_ratchet);

        Ok((
            Self {
                group_id,
                epoch,
                members,
                schedule,
                ratchet,
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
            epoch: self.epoch.next()?,
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
            epoch: self.epoch.next()?,
            operation: CommitOperation::Remove { removed },
            committed_by,
            committed_at: Utc::now(),
            signature: Vec::new(),
        };
        commit.signature = signer.sign_bytes(&commit.signing_payload())?;
        Ok(commit)
    }

    /// Apply `commit` to the group state. Verifies the signature
    /// using `verifier`, checks roster authorisation, validates the
    /// commit operation against the current roster, then advances
    /// the ratchet and derives a fresh [`GroupKeySchedule`] before
    /// finally mutating `self`.
    ///
    /// `verifier` is the group's shared commit-verification key, not
    /// a per-member signing key keyed by `commit.committed_by`: the
    /// signature proves the commit came from a holder of the group
    /// key, and the roster check below is what restricts commits to
    /// current members. See the module-level "Trust model and known
    /// limitation" note for why `committed_by` is not a
    /// cryptographically authenticated identity in this skeleton.
    ///
    /// State mutation is **atomic**: every fallible step (signature
    /// verification, roster authorisation, operation validation,
    /// ratchet derivation, schedule derivation) runs against the
    /// pre-commit state without touching `self`. Only once every
    /// fallible step has succeeded do we commit the new roster,
    /// epoch, ratchet, and schedule together. A `?` early-return
    /// midway through can therefore never leave the group in a
    /// half-applied state where, e.g., the roster has been mutated
    /// but the ratchet did not advance.
    ///
    /// The ratchet advance is the per-epoch forward-secrecy step: it
    /// consumes the current ratchet to derive `(epoch_secret,
    /// next_ratchet)`, zeroises the previous ratchet, and stores
    /// `next_ratchet`. After a successful `process_commit`, the only
    /// way to recompute the new epoch secret is to know the previous
    /// ratchet value, which has been wiped.
    pub fn process_commit<V: SignerBackend>(
        &mut self,
        commit: &MlsCommit,
        verifier: &V,
    ) -> Result<(), CryptoError> {
        // ---- Step 1: validate (read-only against `self`) ----
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
        // Roster-based authorisation: a valid signature alone is not
        // sufficient — the committer must currently be a member of
        // the group. Without this check a former member (or any
        // party holding a signing key recognised by `verifier`)
        // could forge state transitions on a group they no longer
        // belong to.
        if !self.members.contains_key(&commit.committed_by) {
            return Err(CryptoError::ProvenanceSerialisation(
                "committer is not a group member",
            ));
        }
        // `self.epoch.next()` is fallible at `u64::MAX`. Surfacing
        // the overflow here ensures that a terminal group cannot
        // silently keep "advancing" — every commit attempt past
        // the addressable epoch space is rejected explicitly.
        let expected_epoch = self.epoch.next()?;
        if commit.epoch != expected_epoch {
            return Err(CryptoError::ProvenanceSerialisation(
                "commit epoch out of order",
            ));
        }
        // Validate the operation against the current roster. Both
        // Add and Remove must point at a meaningful target — a
        // Remove of a non-member would silently advance the epoch
        // and consume the ratchet for a no-op, which is
        // indistinguishable from a successful removal to a passive
        // observer and lets a malicious committer wastefully
        // rotate the schedule.
        match &commit.operation {
            CommitOperation::Create { .. } => unreachable!("Create rejected above"),
            CommitOperation::Add { added } => {
                if self.members.contains_key(added) {
                    return Err(CryptoError::ProvenanceSerialisation(
                        "member already present",
                    ));
                }
            }
            CommitOperation::Remove { removed } => {
                if !self.members.contains_key(removed) {
                    return Err(CryptoError::ProvenanceSerialisation(
                        "removed member is not in the roster",
                    ));
                }
            }
        }

        // ---- Step 2: derive new state (fallible, but still no
        // mutations to `self`) ----
        //
        // `ratchet_epoch` returns both outputs wrapped in `Zeroizing`,
        // so `epoch_secret` and `next_ratchet` here are bound to
        // wrappers that wipe their inner bytes on scope exit. Any
        // `?` early-return below leaves no plaintext on the stack.
        let new_epoch = commit.epoch;
        let (epoch_secret, next_ratchet) = ratchet_epoch(&self.ratchet, self.group_id, new_epoch)?;
        // `epoch_secret` moves into `derive_schedule`, which wipes
        // its own parameter copy on drop.
        let new_schedule = derive_schedule(self.group_id, new_epoch, epoch_secret)?;

        // ---- Step 3: commit. From this point on no operation may
        // fail — every mutation below is infallible. ----
        match &commit.operation {
            CommitOperation::Create { .. } => unreachable!("Create rejected above"),
            CommitOperation::Add { added } => {
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
        self.epoch = new_epoch;
        self.ratchet.zeroize();
        // Copy the new ratchet into `self.ratchet`, then drop the
        // `Zeroizing` wrapper so its stack-frame copy is wiped.
        // `self.ratchet` is the long-term owner; `Drop for MlsGroup`
        // wipes it when the group itself is dropped.
        self.ratchet = *next_ratchet;
        drop(next_ratchet);
        self.schedule = new_schedule;
        Ok(())
    }

    /// Build a [`MlsWelcome`] envelope for `added`, encrypting the
    /// epoch secret and the current ratchet under a hybrid X25519 +
    /// ML-KEM-768 KEM to the added member's published init key.
    ///
    /// The caller MUST have installed `added`'s real
    /// [`LeafKeyPackage`] via [`install_leaf`] before calling this —
    /// otherwise we would be encapsulating to an all-zero placeholder
    /// hybrid public key, silently breaking the welcome's
    /// confidentiality. Such calls are rejected with
    /// [`CryptoError::ProvenanceSerialisation`].
    pub fn build_welcome(&self, added: MlsMemberId) -> Result<MlsWelcome, CryptoError> {
        let leaf = self
            .members
            .get(&added)
            .ok_or(CryptoError::ProvenanceSerialisation(
                "added member is not in the current roster",
            ))?;
        if is_placeholder_hybrid_pk(&leaf.init_key) {
            return Err(CryptoError::ProvenanceSerialisation("added member's init_key is a placeholder; install_leaf with the real key package first",
            ));
        }

        // Hybrid KEM-encapsulate to the new member's init key. The
        // returned shared secret is used as IKM for the welcome AEAD
        // key + nonce derivation; only the holder of the matching
        // hybrid secret key can recover it.
        //
        // `shared`, `aead_key`, and `aead_nonce` are all derived from
        // the KEM shared secret and are sensitive: an attacker who
        // recovers any of them can decrypt the welcome and obtain the
        // epoch secret and ratchet. They are kept `mut` and zeroised
        // on every exit path below, so they live on the stack only as
        // long as `encrypt_aead` needs them.
        let (mut shared, kem_ciphertext) = hybrid_kem_encap(&leaf.init_key)?;

        let (mut aead_key, mut aead_nonce) =
            match derive_welcome_aead_material(&shared, self.group_id, self.epoch) {
                Ok(material) => material,
                Err(e) => {
                    shared.zeroize();
                    return Err(e);
                }
            };
        // The roster carried in the welcome envelope is bound into
        // the AEAD AAD so any tampering with `welcome.roster` in
        // transit causes `process_welcome` to fail closed. The
        // BTreeMap iterates keys in sorted order, which the
        // serialised `roster: Vec<MlsMemberId>` field preserves.
        let roster: Vec<MlsMemberId> = self.members.keys().copied().collect();
        let aad = welcome_aad(self.group_id, self.epoch, &roster);

        // Plaintext = epoch_secret || ratchet, so the new member can
        // both decrypt the current epoch AND advance the ratchet on
        // the next commit (otherwise they would be stuck at the join
        // epoch). Both halves are 32 bytes; the concatenation never
        // leaves this function as plaintext.
        let mut plaintext = [0u8; AEAD_KEY_LEN * 2];
        plaintext[..AEAD_KEY_LEN].copy_from_slice(self.schedule.epoch_secret());
        plaintext[AEAD_KEY_LEN..].copy_from_slice(&self.ratchet);
        let ciphertext = encrypt_aead(&aead_key, &aead_nonce, &plaintext, &aad);
        plaintext.zeroize();
        shared.zeroize();
        aead_key.zeroize();
        aead_nonce.zeroize();
        let encrypted_epoch_secret = ciphertext?;

        Ok(MlsWelcome {
            group_id: self.group_id,
            epoch: self.epoch,
            roster,
            kem_ciphertext,
            encrypted_epoch_secret,
        })
    }

    /// Bootstrap a fresh [`MlsGroup`] from `welcome` using the new
    /// member's hybrid secret key.
    ///
    /// The function decapsulates the hybrid KEM ciphertext to recover
    /// the shared secret, derives the welcome AEAD key/nonce from it,
    /// decrypts the wrapped `epoch_secret || ratchet` plaintext, and
    /// reconstructs the [`GroupKeySchedule`] and ratchet state.
    /// Roster members other than the new member receive placeholder
    /// leaves — the caller is expected to install real
    /// [`LeafKeyPackage`]s via [`install_leaf`] as it learns them,
    /// matching the existing `process_commit` Add behaviour.
    pub fn process_welcome(
        welcome: &MlsWelcome,
        init_sk: &HybridSecretKey,
    ) -> Result<Self, CryptoError> {
        // Every sensitive local in this function is either an array
        // that we explicitly `zeroize()` before every return path
        // (`shared`, `aead_key`, `aead_nonce`, `plaintext`) or is
        // wrapped in `Zeroizing` so its stack-frame bytes are wiped
        // on `Drop` (`epoch_secret`, `ratchet`, the schedule's own
        // [`Drop`] impl). After `process_welcome` returns, no
        // plaintext key material lingers on this frame.
        let mut shared = hybrid_kem_decap(init_sk, &welcome.kem_ciphertext)?;
        let (mut aead_key, mut aead_nonce) =
            match derive_welcome_aead_material(&shared, welcome.group_id, welcome.epoch) {
                Ok(material) => material,
                Err(e) => {
                    shared.zeroize();
                    return Err(e);
                }
            };
        let aad = welcome_aad(welcome.group_id, welcome.epoch, &welcome.roster);

        let plaintext_result = decrypt_aead(
            &aead_key,
            &aead_nonce,
            &welcome.encrypted_epoch_secret,
            &aad,
        );
        // The AEAD key/nonce are no longer needed beyond this point;
        // wipe them regardless of decryption outcome.
        shared.zeroize();
        aead_key.zeroize();
        aead_nonce.zeroize();
        let mut plaintext = plaintext_result?;
        if plaintext.len() != AEAD_KEY_LEN * 2 {
            // Wipe before erroring so we never leak partial state.
            plaintext.zeroize();
            return Err(CryptoError::ProvenanceSerialisation(
                "welcome plaintext has unexpected length",
            ));
        }
        // Split the decrypted plaintext into the two secret halves.
        // Both are wrapped in `Zeroizing` so their stack-frame copies
        // here are wiped on scope exit — the bytes don't linger on
        // `process_welcome`'s frame after we return.
        let mut epoch_secret = Zeroizing::new([0u8; AEAD_KEY_LEN]);
        let mut ratchet = Zeroizing::new([0u8; AEAD_KEY_LEN]);
        epoch_secret.copy_from_slice(&plaintext[..AEAD_KEY_LEN]);
        ratchet.copy_from_slice(&plaintext[AEAD_KEY_LEN..]);
        plaintext.zeroize();

        // `epoch_secret` moves into `derive_schedule`, which wipes
        // its own parameter copy on drop. On the error branch,
        // `epoch_secret` and `ratchet` are still owned by us; both
        // are dropped (and wiped) when this function returns.
        let schedule = derive_schedule(welcome.group_id, welcome.epoch, epoch_secret)?;

        // Reconstruct the roster with placeholder leaves; real
        // [`LeafKeyPackage`]s are installed via [`install_leaf`].
        let mut members = BTreeMap::new();
        for id in &welcome.roster {
            members.insert(
                *id,
                LeafKeyPackage {
                    member_id: *id,
                    init_key: placeholder_hybrid_pk(),
                    created_at: Utc::now(),
                    cipher_suite: "x25519+mlkem768/aes256-gcm/sha256/ed25519-or-mldsa65",
                },
            );
        }

        // Copy the inner bytes out of `ratchet`. `*ratchet`
        // dereferences and copies (`[u8; 32]: Copy`); the `Zeroizing`
        // wrapper is dropped before we return, wiping our local
        // stack-frame copy. Long-term storage lives in
        // `MlsGroup::ratchet` and is wiped by `Drop for MlsGroup`.
        let ratchet_inner: [u8; AEAD_KEY_LEN] = *ratchet;
        drop(ratchet);

        Ok(Self {
            group_id: welcome.group_id,
            epoch: welcome.epoch,
            members,
            schedule,
            ratchet: ratchet_inner,
        })
    }

    /// Patch a known leaf key package into the roster (used after
    /// the welcome envelope is delivered).
    pub fn install_leaf(&mut self, leaf: LeafKeyPackage) {
        self.members.insert(leaf.member_id, leaf);
    }
}

/// Per-epoch HKDF ratchet step.
///
/// Given the current `ratchet` (the HKDF output produced by the
/// previous epoch's call to this function, or the genesis initialiser
/// for epoch 0), this derives **two** 32-byte outputs:
///
/// * `epoch_secret` — the root of the [`GroupKeySchedule`] for the
///   epoch being entered.
/// * `next_ratchet` — the value the group must store and consume on
///   the next commit. The current `ratchet` is logically destroyed
///   after this call: the caller MUST zeroise it before retaining
///   `next_ratchet`.
///
/// `(group_id, epoch)` is bound into the HKDF `info` so that key
/// material is keyed to a specific group and epoch — no two epochs
/// (within the same or across groups) can ever produce the same
/// outputs even if they accidentally shared a ratchet value.
fn ratchet_epoch(
    ratchet: &[u8; AEAD_KEY_LEN],
    group_id: MlsGroupId,
    epoch: MlsEpoch,
) -> Result<(Zeroizing<[u8; AEAD_KEY_LEN]>, Zeroizing<[u8; AEAD_KEY_LEN]>), CryptoError> {
    let hk = Hkdf::<Sha256>::new(Some(b"knowledge-mls-ratchet-v2"), ratchet);
    let mut info = Vec::with_capacity(16 + 8);
    info.extend_from_slice(group_id.0.as_bytes());
    info.extend_from_slice(&epoch.0.to_be_bytes());

    // Both outputs are wrapped in `Zeroizing` so the caller's local
    // bindings are wiped on scope exit. `[u8; 32]` is `Copy`, so a
    // bare return-by-value would leave both the callee's stack
    // slot and any caller-side intermediate copies live until their
    // frames are reused for other data — wrapping in `Zeroizing`
    // explicitly bounds that lifetime to the wrapper's drop.
    let mut epoch_secret = Zeroizing::new([0u8; AEAD_KEY_LEN]);
    let mut next_ratchet = Zeroizing::new([0u8; AEAD_KEY_LEN]);
    // The labels `info || "…"` keep the two outputs domain-separated
    // — recovering one of them tells an attacker nothing about the
    // other, even though both are expanded from the same PRK.
    let mut info_epoch = info.clone();
    info_epoch.extend_from_slice(b"|epoch-secret");
    let mut info_next = info;
    info_next.extend_from_slice(b"|next-ratchet");
    hk.expand(&info_epoch, epoch_secret.as_mut_slice())
        .map_err(|_| CryptoError::KeyDerivation("HKDF expand epoch-secret failed"))?;
    hk.expand(&info_next, next_ratchet.as_mut_slice())
        .map_err(|_| CryptoError::KeyDerivation("HKDF expand next-ratchet failed"))?;
    Ok((epoch_secret, next_ratchet))
}

/// Derive the welcome AEAD key and nonce from the hybrid KEM shared
/// secret, binding `(group_id, epoch)` into the derivation so that a
/// captured welcome cannot be replayed against a different epoch.
fn derive_welcome_aead_material(
    shared: &HybridSharedSecret,
    group_id: MlsGroupId,
    epoch: MlsEpoch,
) -> Result<(AeadKey, AeadNonce), CryptoError> {
    let hk = Hkdf::<Sha256>::new(Some(b"knowledge-mls-welcome-v2"), shared);
    let mut info = Vec::with_capacity(16 + 8);
    info.extend_from_slice(group_id.0.as_bytes());
    info.extend_from_slice(&epoch.0.to_be_bytes());
    let mut key_info = info.clone();
    key_info.extend_from_slice(b"|aead-key");
    let mut nonce_info = info;
    nonce_info.extend_from_slice(b"|aead-nonce");

    let mut key = [0u8; AEAD_KEY_LEN];
    let mut nonce = [0u8; AEAD_NONCE_LEN];
    hk.expand(&key_info, &mut key)
        .map_err(|_| CryptoError::KeyDerivation("HKDF expand welcome-aead-key failed"))?;
    hk.expand(&nonce_info, &mut nonce)
        .map_err(|_| CryptoError::KeyDerivation("HKDF expand welcome-aead-nonce failed"))?;
    Ok((key, nonce))
}

/// AAD bound into the welcome AEAD: a versioned tag, the group id
/// and epoch the welcome is for, plus a length-prefixed serialisation
/// of the roster the welcome carries.
///
/// Binding the roster into the AAD is what closes the
/// "tampered-roster" gap: [`MlsWelcome::roster`] is transmitted
/// alongside the AEAD ciphertext and is otherwise unauthenticated.
/// Without this binding, a man-in-the-middle could rewrite the roster
/// (add or remove member ids) without invalidating the AEAD tag,
/// causing the new member to either reject legitimate commits from
/// real members or accept commits from a member it shouldn't trust.
/// With this binding, any modification of `welcome.roster` between
/// [`MlsGroup::build_welcome`] and [`MlsGroup::process_welcome`]
/// makes the AEAD tag invalid, so [`process_welcome`] fails closed.
///
/// Distinct from the AEAD key/nonce info labels so cross-protocol
/// confusion is impossible. The roster ids are emitted in the order
/// they appear in the slice — [`build_welcome`] feeds the BTreeMap's
/// sorted-key iteration, and [`process_welcome`] feeds the exact
/// bytes carried in `welcome.roster`, so the two sides only agree on
/// the AAD when the roster has not been tampered with in transit.
fn welcome_aad(group_id: MlsGroupId, epoch: MlsEpoch, roster: &[MlsMemberId]) -> Vec<u8> {
    let prefix = b"knowledge-mls-welcome-v2|";
    let roster_len: u32 = roster.len().try_into().expect("welcome roster fits in u32");
    let mut aad = Vec::with_capacity(prefix.len() + 16 + 8 + 4 + roster.len() * 16);
    aad.extend_from_slice(prefix);
    aad.extend_from_slice(group_id.0.as_bytes());
    aad.extend_from_slice(&epoch.0.to_be_bytes());
    aad.extend_from_slice(&roster_len.to_be_bytes());
    for id in roster {
        aad.extend_from_slice(id.0.as_bytes());
    }
    aad
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

/// Detect the all-zero placeholder hybrid public key inserted by
/// `process_commit` for an Add before the real leaf key package is
/// delivered. Used by [`MlsGroup::build_welcome`] to refuse to issue
/// a welcome encapsulated to zeros.
fn is_placeholder_hybrid_pk(pk: &HybridPublicKey) -> bool {
    pk.x25519.iter().all(|b| *b == 0) && pk.mlkem768.iter().all(|b| *b == 0)
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

    /// Like [`fresh_leaf`] but generated with the **real**
    /// `MlKem768Backend` (the default backend used by
    /// [`hybrid_kem_encap`]) and retains the hybrid secret key. The
    /// welcome-decryption tests need a real keypair because the stub
    /// backend's public keys are not interoperable with real
    /// encapsulation; only a real keypair will round-trip through
    /// [`build_welcome`] -> [`process_welcome`].
    fn fresh_leaf_with_sk(member_id: MlsMemberId) -> (LeafKeyPackage, HybridSecretKey) {
        let (pk, sk) = crate::hybrid_kem::hybrid_keypair().expect("keypair");
        (LeafKeyPackage::new(member_id, pk), sk)
    }

    fn signer() -> MlDsa65Signer {
        MlDsa65Signer::generate()
    }

    /// Deterministic 32-byte initial ratchet for tests, wrapped in
    /// [`Zeroizing`] to match the production [`MlsGroup::create`]
    /// signature. The genesis derivation consumes this and produces
    /// epoch 0's secret plus the ratchet that feeds epoch 1 — the
    /// value here is never retained on the group after
    /// `MlsGroup::create` returns, and is wiped from this stack frame
    /// when the [`Zeroizing`] wrapper is dropped.
    fn fixed_initial_ratchet() -> Zeroizing<[u8; AEAD_KEY_LEN]> {
        let mut seed = [0u8; AEAD_KEY_LEN];
        for (i, b) in seed.iter_mut().enumerate() {
            *b = u8::try_from(i).expect("AEAD_KEY_LEN fits in u8");
        }
        Zeroizing::new(seed)
    }

    #[test]
    fn create_group_yields_genesis_state() {
        let s = signer();
        let creator = MlsMemberId::new_v4();
        let leaf = fresh_leaf(creator);
        let (group, commit) =
            MlsGroup::create(&s, creator, leaf, fixed_initial_ratchet()).expect("create");
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
            MlsGroup::create(&s, creator, fresh_leaf(creator), fixed_initial_ratchet()).unwrap();
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
            MlsGroup::create(&s, creator, fresh_leaf(creator), fixed_initial_ratchet()).unwrap();
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
            MlsGroup::create(&s, creator, fresh_leaf(creator), fixed_initial_ratchet()).unwrap();
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
            MlsGroup::create(&s, creator, fresh_leaf(creator), fixed_initial_ratchet()).unwrap();
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
            MlsGroup::create(&s, creator, fresh_leaf(creator), fixed_initial_ratchet()).unwrap();
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
        // Use the *real* backend keypair here — `build_welcome`
        // calls `hybrid_kem_encap` which uses the real ML-KEM-768
        // backend, and a stub-format public key is not a meaningful
        // input for that path. Exercising the real keypair makes
        // the structural assertions below reflect production
        // behaviour rather than a coincidence of the stub backend.
        let s = signer();
        let creator = MlsMemberId::new_v4();
        let (leaf, _sk) = fresh_leaf_with_sk(creator);
        let (group, _) = MlsGroup::create(&s, creator, leaf, fixed_initial_ratchet()).unwrap();
        let welcome = group.build_welcome(creator).expect("welcome");
        assert_eq!(welcome.group_id, group.group_id);
        assert_eq!(welcome.epoch, group.epoch);
        assert_eq!(welcome.roster, vec![creator]);
        // Wrapped material is non-empty and the ciphertext is at
        // least the plaintext length (64 bytes) plus a 16-byte
        // Poly1305 tag.
        assert_eq!(welcome.encrypted_epoch_secret.len(), AEAD_KEY_LEN * 2 + 16);
        // KEM ciphertext carries a non-zero ephemeral X25519 public
        // key and a non-zero ML-KEM-768 ciphertext.
        assert!(welcome
            .kem_ciphertext
            .x25519_eph_pub
            .iter()
            .any(|b| *b != 0));
        assert!(welcome.kem_ciphertext.mlkem768_ct.iter().any(|b| *b != 0));
    }

    #[test]
    fn build_welcome_rejects_placeholder_init_key() {
        // A welcome cannot be issued before the added member's real
        // leaf is installed — otherwise we would KEM-encap to the
        // all-zero placeholder hybrid public key, breaking welcome
        // confidentiality.
        let s = signer();
        let creator = MlsMemberId::new_v4();
        let (mut group, _) =
            MlsGroup::create(&s, creator, fresh_leaf(creator), fixed_initial_ratchet()).unwrap();
        let new_id = MlsMemberId::new_v4();
        let leaf = fresh_leaf(new_id);
        let add = group.add_member(&s, creator, leaf.clone()).unwrap();
        group.process_commit(&add, &s).unwrap();
        // NOTE: we deliberately skip `install_leaf` so the new
        // member's stored leaf is still the placeholder.
        let err = group.build_welcome(new_id).unwrap_err();
        assert!(matches!(err,
            CryptoError::ProvenanceSerialisation("added member's init_key is a placeholder; install_leaf with the real key package first"
            )
        ));
    }

    #[test]
    fn signing_payload_is_deterministic() {
        let s = signer();
        let creator = MlsMemberId::new_v4();
        let (_group, commit) =
            MlsGroup::create(&s, creator, fresh_leaf(creator), fixed_initial_ratchet()).unwrap();
        assert_eq!(commit.signing_payload(), commit.signing_payload());
    }

    #[test]
    fn epoch_zero_and_next_round_trip() {
        assert_eq!(MlsEpoch::zero().0, 0);
        assert_eq!(MlsEpoch::zero().next().unwrap().0, 1);
        // Overflow at the terminal epoch is reported as a hard
        // error rather than silently saturating at `u64::MAX`.
        let max = MlsEpoch(u64::MAX);
        assert!(matches!(max.next(), Err(CryptoError::EpochOverflow)));
    }

    #[test]
    fn process_commit_rejects_create_on_existing_group() {
        // Regression: a signed `Create` commit replayed against an
        // already-established group must be rejected, not silently
        // jump the epoch and re-derive the key schedule.
        let s = signer();
        let creator = MlsMemberId::new_v4();
        let (mut group, genesis) =
            MlsGroup::create(&s, creator, fresh_leaf(creator), fixed_initial_ratchet()).unwrap();
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
        let (group, _) =
            MlsGroup::create(&s, creator, fresh_leaf(creator), fixed_initial_ratchet()).unwrap();
        let err = group
            .add_member(&s, creator, fresh_leaf(creator))
            .unwrap_err();
        assert!(matches!(err, CryptoError::ProvenanceSerialisation(_)));
    }

    /// Forward secrecy: once the ratchet has advanced, the
    /// epoch_secret for any previous epoch cannot be reconstructed
    /// from the current ratchet state. We advance the group through
    /// three commits and verify that the resulting ratchet, fed
    /// through `ratchet_epoch` at *any* prior epoch number, produces
    /// secrets that do NOT match the captured historical secrets.
    #[test]
    fn epoch_secret_cannot_be_recovered_after_ratchet_advance() {
        let s = signer();
        let creator = MlsMemberId::new_v4();
        let (mut group, _) =
            MlsGroup::create(&s, creator, fresh_leaf(creator), fixed_initial_ratchet()).unwrap();
        let group_id = group.group_id;

        // Snapshot epoch 0's secret.
        let secret_epoch_0 = *group.schedule.epoch_secret();

        // Add a member → epoch 1.
        let m1 = MlsMemberId::new_v4();
        let l1 = fresh_leaf(m1);
        let c1 = group.add_member(&s, creator, l1.clone()).unwrap();
        group.process_commit(&c1, &s).unwrap();
        group.install_leaf(l1);
        let secret_epoch_1 = *group.schedule.epoch_secret();

        // Add another member → epoch 2.
        let m2 = MlsMemberId::new_v4();
        let l2 = fresh_leaf(m2);
        let c2 = group.add_member(&s, creator, l2.clone()).unwrap();
        group.process_commit(&c2, &s).unwrap();
        group.install_leaf(l2);
        let secret_epoch_2 = *group.schedule.epoch_secret();

        // Add another member → epoch 3.
        let m3 = MlsMemberId::new_v4();
        let l3 = fresh_leaf(m3);
        let c3 = group.add_member(&s, creator, l3.clone()).unwrap();
        group.process_commit(&c3, &s).unwrap();
        group.install_leaf(l3);
        let secret_epoch_3 = *group.schedule.epoch_secret();

        // All four epoch secrets are distinct.
        assert_ne!(secret_epoch_0, secret_epoch_1);
        assert_ne!(secret_epoch_1, secret_epoch_2);
        assert_ne!(secret_epoch_2, secret_epoch_3);
        assert_ne!(secret_epoch_0, secret_epoch_2);
        assert_ne!(secret_epoch_0, secret_epoch_3);
        assert_ne!(secret_epoch_1, secret_epoch_3);

        // The post-epoch-3 ratchet is what the group currently
        // holds. Even if an attacker exfiltrates it, they cannot
        // re-derive any prior epoch's secret — not by running
        // `ratchet_epoch` at the prior epoch number, not by running
        // it at the current epoch number, not at any epoch number.
        let compromised = group.ratchet;
        for epoch in 0u64..=4 {
            let (candidate, _) = ratchet_epoch(&compromised, group_id, MlsEpoch(epoch)).unwrap();
            // `candidate` is `Zeroizing<[u8; 32]>`; deref to compare
            // against the bare arrays captured above.
            let candidate: [u8; AEAD_KEY_LEN] = *candidate;
            assert_ne!(
                candidate, secret_epoch_0,
                "epoch 0 secret recovered from post-epoch-3 ratchet at epoch {epoch}",
            );
            assert_ne!(
                candidate, secret_epoch_1,
                "epoch 1 secret recovered from post-epoch-3 ratchet at epoch {epoch}",
            );
            assert_ne!(
                candidate, secret_epoch_2,
                "epoch 2 secret recovered from post-epoch-3 ratchet at epoch {epoch}",
            );
            assert_ne!(
                candidate, secret_epoch_3,
                "epoch 3 secret recovered from post-epoch-3 ratchet at epoch {epoch}",
            );
        }
    }

    /// Confidentiality: the wrapped welcome plaintext can be
    /// recovered only by the holder of the hybrid secret key matching
    /// the added member's [`LeafKeyPackage::init_key`]. An attacker
    /// holding any *other* hybrid secret key cannot read the
    /// `epoch_secret || ratchet` payload.
    #[test]
    fn welcome_epoch_secret_requires_member_secret_key() {
        let s = signer();
        let creator = MlsMemberId::new_v4();
        let (mut group, _) =
            MlsGroup::create(&s, creator, fresh_leaf(creator), fixed_initial_ratchet()).unwrap();
        let new_id = MlsMemberId::new_v4();
        let (new_leaf, new_sk) = fresh_leaf_with_sk(new_id);
        let add = group.add_member(&s, creator, new_leaf.clone()).unwrap();
        group.process_commit(&add, &s).unwrap();
        group.install_leaf(new_leaf);
        let welcome = group.build_welcome(new_id).expect("welcome");

        // The intended member CAN decrypt and the bootstrap recovers
        // exactly the current epoch secret + ratchet held by the
        // originating group.
        let bootstrapped = MlsGroup::process_welcome(&welcome, &new_sk).expect("process_welcome");
        assert_eq!(bootstrapped.group_id, group.group_id);
        assert_eq!(bootstrapped.epoch, group.epoch);
        assert_eq!(
            bootstrapped.schedule.epoch_secret(),
            group.schedule.epoch_secret(),
        );
        // The bootstrapped derived keys match too, because they are
        // a deterministic HKDF of the epoch secret.
        assert_eq!(
            bootstrapped.schedule.shared_memory_key,
            group.schedule.shared_memory_key,
        );
        assert_eq!(bootstrapped.ratchet, group.ratchet);

        // A different (real-backend) hybrid secret key cannot
        // recover the payload. We use the real backend here too so
        // the decap call itself succeeds structurally — what must
        // fail is the AEAD authentication step after the wrong
        // shared secret is recovered.
        let (_pk_other, sk_other) = crate::hybrid_kem::hybrid_keypair().expect("keypair");
        let err = MlsGroup::process_welcome(&welcome, &sk_other).expect_err("foreign sk must fail");
        assert!(
            matches!(err, CryptoError::AeadDecryption),
            "unexpected error variant: {err:?}",
        );
    }

    /// Integrity: [`MlsWelcome::roster`] is bound into the AEAD AAD,
    /// so any tampering of the roster between sender and receiver
    /// causes [`MlsGroup::process_welcome`] to fail with an AEAD
    /// authentication error instead of silently accepting the welcome
    /// under an attacker-chosen roster. Without this binding a MITM
    /// could rewrite the roster (e.g. inject a phantom member id, or
    /// drop a legitimate one) and the new member would have no way to
    /// notice.
    #[test]
    fn welcome_with_tampered_roster_is_rejected() {
        let s = signer();
        let creator = MlsMemberId::new_v4();
        let (mut group, _) =
            MlsGroup::create(&s, creator, fresh_leaf(creator), fixed_initial_ratchet()).unwrap();
        let new_id = MlsMemberId::new_v4();
        let (new_leaf, new_sk) = fresh_leaf_with_sk(new_id);
        let add = group.add_member(&s, creator, new_leaf.clone()).unwrap();
        group.process_commit(&add, &s).unwrap();
        group.install_leaf(new_leaf);
        let welcome = group.build_welcome(new_id).expect("welcome");

        // Tamper with the roster in transit: prepend a phantom id.
        // The KEM ciphertext and AEAD ciphertext are unchanged, so
        // decapsulation still recovers the correct shared secret and
        // derives the correct key/nonce — what must fail is the AEAD
        // authentication step, because the receiver computes the AAD
        // from the tampered roster and gets a different byte string
        // than the sender bound at encryption time.
        let mut tampered = welcome.clone();
        let phantom = MlsMemberId::new_v4();
        tampered.roster.insert(0, phantom);
        let err = MlsGroup::process_welcome(&tampered, &new_sk)
            .expect_err("tampered roster must be rejected");
        assert!(
            matches!(err, CryptoError::AeadDecryption),
            "unexpected error variant: {err:?}",
        );

        // Removing a legitimate roster entry is also detected.
        let mut tampered = welcome.clone();
        tampered.roster.pop();
        let err = MlsGroup::process_welcome(&tampered, &new_sk)
            .expect_err("roster-shortened welcome must be rejected");
        assert!(
            matches!(err, CryptoError::AeadDecryption),
            "unexpected error variant: {err:?}",
        );

        // Reordering the roster is also detected — the AAD encodes
        // members in their exact transmitted order, not as a set.
        if welcome.roster.len() >= 2 {
            let mut tampered = welcome.clone();
            tampered.roster.swap(0, 1);
            let err = MlsGroup::process_welcome(&tampered, &new_sk)
                .expect_err("reordered roster must be rejected");
            assert!(
                matches!(err, CryptoError::AeadDecryption),
                "unexpected error variant: {err:?}",
            );
        }

        // Sanity: the untampered welcome still round-trips.
        MlsGroup::process_welcome(&welcome, &new_sk).expect("untampered welcome must succeed");
    }

    /// Authorisation: a commit signed by a party who is not a
    /// current roster member must be rejected, even when the
    /// signature itself verifies under the supplied verifier.
    #[test]
    fn non_member_commit_is_rejected() {
        let s = signer();
        let creator = MlsMemberId::new_v4();
        let (mut group, _) =
            MlsGroup::create(&s, creator, fresh_leaf(creator), fixed_initial_ratchet()).unwrap();

        // Forge a well-formed Add commit whose `committed_by` is a
        // brand-new, never-admitted member id. The commit is signed
        // with `s` (whose public half `verifier` recognises), so the
        // signature DOES verify — the only reason to reject is the
        // roster check.
        let outsider = MlsMemberId::new_v4();
        let added = MlsMemberId::new_v4();
        let leaf = fresh_leaf(added);
        // Reuse `add_member`'s signing path by manually building
        // the commit with `committed_by = outsider`.
        let mut commit = MlsCommit {
            group_id: group.group_id,
            epoch: group.epoch.next().unwrap(),
            operation: CommitOperation::Add {
                added: leaf.member_id,
            },
            committed_by: outsider,
            committed_at: Utc::now(),
            signature: Vec::new(),
        };
        commit.signature = s.sign_bytes(&commit.signing_payload()).unwrap();

        // Sanity: the signature alone DOES verify against `s`.
        assert!(s
            .verify_bytes(&commit.signing_payload(), &commit.signature)
            .unwrap());

        // But `process_commit` rejects because `outsider` is not in
        // the current roster.
        let err = group.process_commit(&commit, &s).unwrap_err();
        assert!(matches!(
            err,
            CryptoError::ProvenanceSerialisation("committer is not a group member")
        ));

        // Group state is unchanged.
        assert_eq!(group.epoch, MlsEpoch::zero());
        assert_eq!(group.members.len(), 1);
        assert!(!group.members.contains_key(&added));
        assert!(!group.members.contains_key(&outsider));
    }

    /// Authorisation: a Remove commit whose target is not currently
    /// in the roster must be rejected, even when the signature is
    /// valid and the committer is a real member. Without this check
    /// a malicious committer could silently force the epoch to
    /// advance and the ratchet to be consumed for a no-op Remove,
    /// indistinguishable on the wire from a genuine removal and
    /// wastefully rotating the schedule.
    #[test]
    fn remove_of_nonexistent_member_is_rejected() {
        let s = signer();
        let creator = MlsMemberId::new_v4();
        let (mut group, _) =
            MlsGroup::create(&s, creator, fresh_leaf(creator), fixed_initial_ratchet()).unwrap();

        // Snapshot pre-state so we can confirm atomicity below.
        let epoch_before = group.epoch;
        let ratchet_before = group.ratchet;
        let schedule_epoch_secret_before = *group.schedule.epoch_secret();
        let members_before: Vec<MlsMemberId> = group.members.keys().copied().collect();

        // Sign a well-formed Remove commit targeting an outsider id.
        let ghost = MlsMemberId::new_v4();
        let mut commit = MlsCommit {
            group_id: group.group_id,
            epoch: group.epoch.next().unwrap(),
            operation: CommitOperation::Remove { removed: ghost },
            committed_by: creator,
            committed_at: Utc::now(),
            signature: Vec::new(),
        };
        commit.signature = s.sign_bytes(&commit.signing_payload()).unwrap();

        let err = group.process_commit(&commit, &s).unwrap_err();
        assert!(matches!(
            err,
            CryptoError::ProvenanceSerialisation("removed member is not in the roster")
        ));

        // Atomicity: every part of `MlsGroup` is exactly as it was
        // before the rejected commit — epoch did not advance, the
        // ratchet was not consumed, the schedule was not rotated,
        // and the roster is unchanged.
        assert_eq!(group.epoch, epoch_before);
        assert_eq!(group.ratchet, ratchet_before);
        assert_eq!(group.schedule.epoch_secret(), &schedule_epoch_secret_before);
        let members_after: Vec<MlsMemberId> = group.members.keys().copied().collect();
        assert_eq!(members_after, members_before);
    }

    /// Atomicity (defence-in-depth): when `process_commit` rejects
    /// a commit for any reason — here, an out-of-order epoch — the
    /// group's roster, epoch, ratchet, and schedule are all left
    /// untouched. The validate / derive / commit phasing in
    /// `process_commit` is what guarantees this; this test pins it.
    #[test]
    fn process_commit_rejection_leaves_state_unchanged() {
        let s = signer();
        let creator = MlsMemberId::new_v4();
        let (mut group, _) =
            MlsGroup::create(&s, creator, fresh_leaf(creator), fixed_initial_ratchet()).unwrap();

        let epoch_before = group.epoch;
        let ratchet_before = group.ratchet;
        let schedule_epoch_secret_before = *group.schedule.epoch_secret();
        let members_before: Vec<MlsMemberId> = group.members.keys().copied().collect();

        // Build an Add commit whose epoch is several steps ahead of
        // `self.epoch.next()` — this triggers the "epoch out of
        // order" rejection path *after* signature verification and
        // roster authorisation have passed.
        let new_id = MlsMemberId::new_v4();
        let leaf = fresh_leaf(new_id);
        let mut commit = group.add_member(&s, creator, leaf).unwrap();
        commit.epoch = MlsEpoch(commit.epoch.0 + 5);
        commit.signature = s.sign_bytes(&commit.signing_payload()).unwrap();

        let err = group.process_commit(&commit, &s).unwrap_err();
        assert!(matches!(
            err,
            CryptoError::ProvenanceSerialisation("commit epoch out of order")
        ));

        // No mutation occurred.
        assert_eq!(group.epoch, epoch_before);
        assert_eq!(group.ratchet, ratchet_before);
        assert_eq!(group.schedule.epoch_secret(), &schedule_epoch_secret_before);
        let members_after: Vec<MlsMemberId> = group.members.keys().copied().collect();
        assert_eq!(members_after, members_before);
        assert!(!group.members.contains_key(&new_id));
    }
}
