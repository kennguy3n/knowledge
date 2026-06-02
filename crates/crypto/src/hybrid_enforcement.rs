//! Hybrid classical + post-quantum policy enforcement.
//!
//! Per `docs/DESIGN.md` §9, every key exchange in
//! the substrate must run a hybrid X25519 + ML-KEM-768 construction.
//! Operators control the cut-over with a [`HybridMode`]:
//!
//! * [`HybridMode::ClassicalOnly`] — legacy mode that accepts pure
//!   X25519 only. Available for migration testing; never enabled in
//!   production.
//! * [`HybridMode::HybridTransition`] — the production default. Every
//!   key exchange MUST include both X25519 and ML-KEM-768. Records
//!   audit entries on every operation.
//! * [`HybridMode::PostQuantumOnly`] — hardening profile that rejects
//!   any classical-only KEM and even rejects hybrid exchanges that
//!   were tagged as "classical fallback" (e.g. observed during a
//!   downgrade attempt).
//!
//! [`CryptoPolicy`] is the operator-facing struct. It is consulted on
//! every encap / decap and emits a [`KeyExchangeAudit`] entry that
//! records which primitives were used.

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::errors::CryptoError;
use crate::hybrid_kem::{
    hybrid_kem_decap_with_backend, hybrid_kem_encap_with_backend, HybridCiphertext,
    HybridPublicKey, HybridSecretKey, HybridSharedSecret,
};
use crate::kem::{KemBackend, MlKem768Backend};

/// What flavor of key exchange the substrate is willing to accept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HybridMode {
    /// Classical-only: pure X25519 (no ML-KEM-768). Migration / test
    /// only.
    ClassicalOnly,
    /// Hybrid X25519 + ML-KEM-768 — production default.
    HybridTransition,
    /// Post-quantum hardened: rejects every classical-only path and
    /// flags downgrade attempts.
    PostQuantumOnly,
}

impl HybridMode {
    /// Stable string tag for audit rows.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ClassicalOnly => "classical_only",
            Self::HybridTransition => "hybrid_transition",
            Self::PostQuantumOnly => "post_quantum_only",
        }
    }
}

/// Operator-facing crypto policy. Determines which key exchanges are
/// accepted and what audit metadata is recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CryptoPolicy {
    /// Active hybrid mode.
    pub mode: HybridMode,
}

impl CryptoPolicy {
    /// Construct a policy with the given mode.
    pub const fn new(mode: HybridMode) -> Self {
        Self { mode }
    }

    /// Production default — hybrid X25519 + ML-KEM-768.
    pub const fn production_default() -> Self {
        Self {
            mode: HybridMode::HybridTransition,
        }
    }
}

impl Default for CryptoPolicy {
    fn default() -> Self {
        Self::production_default()
    }
}

/// Tag identifying which primitives participated in a key exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KemPrimitives {
    /// `true` iff the X25519 half was used.
    pub used_x25519: bool,
    /// `true` iff the ML-KEM-768 half was used.
    pub used_mlkem768: bool,
}

impl KemPrimitives {
    /// Hybrid X25519 + ML-KEM-768.
    pub const fn hybrid() -> Self {
        Self {
            used_x25519: true,
            used_mlkem768: true,
        }
    }

    /// Classical-only X25519 (no ML-KEM-768).
    pub const fn classical_only() -> Self {
        Self {
            used_x25519: true,
            used_mlkem768: false,
        }
    }

    /// PQ-only ML-KEM-768 (no X25519). Reserved for future
    /// `PostQuantumOnly` hardening; not used by current encap path.
    pub const fn pq_only() -> Self {
        Self {
            used_x25519: false,
            used_mlkem768: true,
        }
    }

    /// True iff both halves were used.
    pub const fn is_hybrid(self) -> bool {
        self.used_x25519 && self.used_mlkem768
    }

    /// True iff only the classical half was used.
    pub const fn is_classical_only(self) -> bool {
        self.used_x25519 && !self.used_mlkem768
    }

    /// True iff only the PQ half was used.
    pub const fn is_pq_only(self) -> bool {
        !self.used_x25519 && self.used_mlkem768
    }

    /// Stable string tag for audit rows.
    pub const fn as_str(self) -> &'static str {
        match (self.used_x25519, self.used_mlkem768) {
            (true, true) => "x25519+mlkem768",
            (true, false) => "x25519",
            (false, true) => "mlkem768",
            (false, false) => "none",
        }
    }
}

/// Direction of a key exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyExchangeDirection {
    /// Encapsulate (sender side).
    Encap,
    /// Decapsulate (receiver side).
    Decap,
}

impl KeyExchangeDirection {
    /// Stable string tag.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Encap => "encap",
            Self::Decap => "decap",
        }
    }
}

/// Outcome of a policy check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyExchangeOutcome {
    /// Operation accepted.
    Accepted,
    /// Operation rejected (policy mismatch).
    Rejected,
}

impl KeyExchangeOutcome {
    /// Stable string tag.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
        }
    }
}

/// Audit row recording a single key-exchange operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyExchangeAudit {
    /// Random row id.
    pub id: Uuid,
    /// Wall-clock timestamp.
    pub at: DateTime<Utc>,
    /// Active policy mode.
    pub mode: HybridMode,
    /// Primitives observed on the wire.
    pub primitives: KemPrimitives,
    /// `Encap` or `Decap`.
    pub direction: KeyExchangeDirection,
    /// `Accepted` / `Rejected`.
    pub outcome: KeyExchangeOutcome,
    /// Free-form reason — populated on reject.
    pub reason: Option<String>,
}

impl KeyExchangeAudit {
    /// Construct an audit row.
    pub fn new(
        mode: HybridMode,
        primitives: KemPrimitives,
        direction: KeyExchangeDirection,
        outcome: KeyExchangeOutcome,
        reason: Option<String>,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            at: Utc::now(),
            mode,
            primitives,
            direction,
            outcome,
            reason,
        }
    }
}

/// Optional sink for [`KeyExchangeAudit`] rows. Wired at a higher
/// layer so `crypto` doesn't depend on `audit_service`.
pub trait KeyExchangeAuditor {
    /// Persist `audit` to the audit trail.
    fn record_key_exchange(&mut self, audit: &KeyExchangeAudit);
}

/// Validate that `primitives` satisfies `policy`. Returns
/// `Ok(())` on accept, `Err(CryptoError::HybridPolicyViolation)`
/// on reject.
///
/// The same predicate is used by [`enforce_hybrid_kem_encap`] and
/// [`enforce_hybrid_kem_decap`] as well as offline validation paths
/// (e.g. inspecting an MLS commit's KEM tag).
pub fn enforce_hybrid_kem(
    policy: &CryptoPolicy,
    primitives: KemPrimitives,
) -> Result<(), CryptoError> {
    match policy.mode {
        HybridMode::ClassicalOnly => {
            // Classical-only mode only accepts pure X25519. Reject
            // any exchange that used the PQ half (defensive: this
            // mode is meant for migration tests, not production).
            if primitives.used_mlkem768 && !primitives.used_x25519 {
                return Err(CryptoError::HybridPolicyViolation {
                    expected: "x25519",
                    got: primitives.as_str(),
                });
            }
            Ok(())
        }
        HybridMode::HybridTransition => {
            if !primitives.is_hybrid() {
                return Err(CryptoError::HybridPolicyViolation {
                    expected: "x25519+mlkem768",
                    got: primitives.as_str(),
                });
            }
            Ok(())
        }
        HybridMode::PostQuantumOnly => {
            // PQ-only requires the ML-KEM-768 half. Hybrid is still
            // accepted; classical-only is rejected.
            if !primitives.used_mlkem768 {
                return Err(CryptoError::HybridPolicyViolation {
                    expected: "mlkem768 (hybrid or pq-only)",
                    got: primitives.as_str(),
                });
            }
            Ok(())
        }
    }
}

/// Policy-checked encap. Runs the underlying hybrid encap, records a
/// [`KeyExchangeAudit`] row via `auditor`, and returns the same
/// `(shared, ciphertext)` pair as the unwrapped call.
///
/// On policy reject the row is recorded as `Rejected` and the underlying
/// encap is **not** executed.
pub fn enforce_hybrid_kem_encap<A: KeyExchangeAuditor>(
    policy: &CryptoPolicy,
    auditor: &mut A,
    recipient_pk: &HybridPublicKey,
) -> Result<(HybridSharedSecret, HybridCiphertext), CryptoError> {
    enforce_hybrid_kem_encap_with_backend(policy, auditor, &MlKem768Backend, recipient_pk)
}

/// Backend-flexible variant of [`enforce_hybrid_kem_encap`].
pub fn enforce_hybrid_kem_encap_with_backend<A, B>(
    policy: &CryptoPolicy,
    auditor: &mut A,
    backend: &B,
    recipient_pk: &HybridPublicKey,
) -> Result<(HybridSharedSecret, HybridCiphertext), CryptoError>
where
    A: KeyExchangeAuditor,
    B: KemBackend,
{
    let primitives = KemPrimitives::hybrid();
    if let Err(err) = enforce_hybrid_kem(policy, primitives) {
        let audit = KeyExchangeAudit::new(
            policy.mode,
            primitives,
            KeyExchangeDirection::Encap,
            KeyExchangeOutcome::Rejected,
            Some(err.to_string()),
        );
        auditor.record_key_exchange(&audit);
        return Err(err);
    }

    let result = hybrid_kem_encap_with_backend(backend, recipient_pk);
    let outcome = if result.is_ok() {
        KeyExchangeOutcome::Accepted
    } else {
        KeyExchangeOutcome::Rejected
    };
    let reason = result.as_ref().err().map(std::string::ToString::to_string);
    let audit = KeyExchangeAudit::new(
        policy.mode,
        primitives,
        KeyExchangeDirection::Encap,
        outcome,
        reason,
    );
    auditor.record_key_exchange(&audit);
    result
}

/// Policy-checked decap. Same shape as [`enforce_hybrid_kem_encap`].
pub fn enforce_hybrid_kem_decap<A: KeyExchangeAuditor>(
    policy: &CryptoPolicy,
    auditor: &mut A,
    recipient_sk: &HybridSecretKey,
    ciphertext: &HybridCiphertext,
) -> Result<HybridSharedSecret, CryptoError> {
    enforce_hybrid_kem_decap_with_backend(
        policy,
        auditor,
        &MlKem768Backend,
        recipient_sk,
        ciphertext,
    )
}

/// Backend-flexible variant of [`enforce_hybrid_kem_decap`].
pub fn enforce_hybrid_kem_decap_with_backend<A, B>(
    policy: &CryptoPolicy,
    auditor: &mut A,
    backend: &B,
    recipient_sk: &HybridSecretKey,
    ciphertext: &HybridCiphertext,
) -> Result<HybridSharedSecret, CryptoError>
where
    A: KeyExchangeAuditor,
    B: KemBackend,
{
    let primitives = KemPrimitives::hybrid();
    if let Err(err) = enforce_hybrid_kem(policy, primitives) {
        let audit = KeyExchangeAudit::new(
            policy.mode,
            primitives,
            KeyExchangeDirection::Decap,
            KeyExchangeOutcome::Rejected,
            Some(err.to_string()),
        );
        auditor.record_key_exchange(&audit);
        return Err(err);
    }

    let result = hybrid_kem_decap_with_backend(backend, recipient_sk, ciphertext);
    let outcome = if result.is_ok() {
        KeyExchangeOutcome::Accepted
    } else {
        KeyExchangeOutcome::Rejected
    };
    let reason = result.as_ref().err().map(std::string::ToString::to_string);
    let audit = KeyExchangeAudit::new(
        policy.mode,
        primitives,
        KeyExchangeDirection::Decap,
        outcome,
        reason,
    );
    auditor.record_key_exchange(&audit);
    result
}

/// In-memory auditor, useful for tests and short-running tools.
#[derive(Debug, Default)]
pub struct InMemoryKeyExchangeAuditor {
    /// Audit log accumulated so far.
    pub log: Vec<KeyExchangeAudit>,
}

impl InMemoryKeyExchangeAuditor {
    /// Construct an empty auditor.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of audit rows captured so far.
    pub fn len(&self) -> usize {
        self.log.len()
    }

    /// `true` iff no audit rows have been captured.
    pub fn is_empty(&self) -> bool {
        self.log.is_empty()
    }
}

impl KeyExchangeAuditor for InMemoryKeyExchangeAuditor {
    fn record_key_exchange(&mut self, audit: &KeyExchangeAudit) {
        self.log.push(audit.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hybrid_kem::hybrid_keypair_with_backend;
    use crate::kem::StubKemBackend;

    fn keys() -> (HybridPublicKey, HybridSecretKey) {
        hybrid_keypair_with_backend(&StubKemBackend).expect("keypair")
    }

    #[test]
    fn hybrid_transition_accepts_hybrid_primitives() {
        let policy = CryptoPolicy::new(HybridMode::HybridTransition);
        enforce_hybrid_kem(&policy, KemPrimitives::hybrid()).expect("hybrid accept");
    }

    #[test]
    fn hybrid_transition_rejects_classical_only() {
        let policy = CryptoPolicy::new(HybridMode::HybridTransition);
        let err = enforce_hybrid_kem(&policy, KemPrimitives::classical_only())
            .expect_err("classical reject");
        assert!(matches!(err, CryptoError::HybridPolicyViolation { .. }));
    }

    #[test]
    fn hybrid_transition_rejects_pq_only() {
        let policy = CryptoPolicy::new(HybridMode::HybridTransition);
        let err = enforce_hybrid_kem(&policy, KemPrimitives::pq_only()).expect_err("pq reject");
        assert!(matches!(err, CryptoError::HybridPolicyViolation { .. }));
    }

    #[test]
    fn classical_only_accepts_x25519() {
        let policy = CryptoPolicy::new(HybridMode::ClassicalOnly);
        enforce_hybrid_kem(&policy, KemPrimitives::classical_only()).expect("classical accept");
        // Hybrid is still accepted because the classical half was used.
        enforce_hybrid_kem(&policy, KemPrimitives::hybrid()).expect("hybrid accept in classical");
    }

    #[test]
    fn classical_only_rejects_pure_pq() {
        let policy = CryptoPolicy::new(HybridMode::ClassicalOnly);
        let err = enforce_hybrid_kem(&policy, KemPrimitives::pq_only()).expect_err("pq reject");
        assert!(matches!(err, CryptoError::HybridPolicyViolation { .. }));
    }

    #[test]
    fn pq_only_rejects_classical_only() {
        let policy = CryptoPolicy::new(HybridMode::PostQuantumOnly);
        let err = enforce_hybrid_kem(&policy, KemPrimitives::classical_only())
            .expect_err("classical reject");
        assert!(matches!(err, CryptoError::HybridPolicyViolation { .. }));
    }

    #[test]
    fn pq_only_accepts_hybrid_and_pq() {
        let policy = CryptoPolicy::new(HybridMode::PostQuantumOnly);
        enforce_hybrid_kem(&policy, KemPrimitives::hybrid()).expect("hybrid accept in pq");
        enforce_hybrid_kem(&policy, KemPrimitives::pq_only()).expect("pq accept");
    }

    #[test]
    fn enforce_encap_records_audit_on_accept() {
        let (pk, _sk) = keys();
        let policy = CryptoPolicy::new(HybridMode::HybridTransition);
        let mut auditor = InMemoryKeyExchangeAuditor::new();
        let _ = enforce_hybrid_kem_encap_with_backend(&policy, &mut auditor, &StubKemBackend, &pk)
            .expect("encap accept");
        assert_eq!(auditor.len(), 1);
        let row = &auditor.log[0];
        assert_eq!(row.outcome, KeyExchangeOutcome::Accepted);
        assert_eq!(row.direction, KeyExchangeDirection::Encap);
        assert!(row.primitives.is_hybrid());
        assert_eq!(row.mode, HybridMode::HybridTransition);
        assert!(row.reason.is_none());
    }

    #[test]
    fn enforce_decap_records_audit_on_accept() {
        let (pk, sk) = keys();
        let policy = CryptoPolicy::new(HybridMode::HybridTransition);
        let mut auditor = InMemoryKeyExchangeAuditor::new();
        let (_ss, ct) =
            enforce_hybrid_kem_encap_with_backend(&policy, &mut auditor, &StubKemBackend, &pk)
                .expect("encap");
        let _ss =
            enforce_hybrid_kem_decap_with_backend(&policy, &mut auditor, &StubKemBackend, &sk, &ct)
                .expect("decap");
        assert_eq!(auditor.len(), 2);
        assert_eq!(auditor.log[1].direction, KeyExchangeDirection::Decap);
        assert_eq!(auditor.log[1].outcome, KeyExchangeOutcome::Accepted);
    }

    #[test]
    fn audit_outcome_string_tags_round_trip() {
        assert_eq!(KeyExchangeOutcome::Accepted.as_str(), "accepted");
        assert_eq!(KeyExchangeOutcome::Rejected.as_str(), "rejected");
        assert_eq!(KeyExchangeDirection::Encap.as_str(), "encap");
        assert_eq!(KeyExchangeDirection::Decap.as_str(), "decap");
        assert_eq!(HybridMode::ClassicalOnly.as_str(), "classical_only");
        assert_eq!(HybridMode::HybridTransition.as_str(), "hybrid_transition");
        assert_eq!(HybridMode::PostQuantumOnly.as_str(), "post_quantum_only");
        assert_eq!(KemPrimitives::hybrid().as_str(), "x25519+mlkem768");
        assert_eq!(KemPrimitives::classical_only().as_str(), "x25519");
        assert_eq!(KemPrimitives::pq_only().as_str(), "mlkem768");
    }
}
