//! Provenance bundle — the PROV data model from `docs/technical/design.md` §7.2.
//!
//! Every observation and every synthesis output carries a PROV bundle:
//! the **entity** (the row itself), the **activity** (the synthesis run
//! — agent identity, model version, prompt id, run id), the **agent**
//! (human or software), and **derivations** (the evidence rows the
//! output was derived from). The bundle is signed by the synthesizer
//! key so a consumer can verify authenticity even when the synthesizer
//! is untrusted.
//!
//! This module ships:
//!
//! * The PROV data model ([`ProvenanceBundle`], [`SynthesisActivity`],
//!   [`ProvenanceAgent`], [`AgentKind`]).
//! * A [`ProvenanceSigner`] trait with `sign` / `verify` so the
//!   ML-DSA-65 implementation can drop in without touching the rest
//!   of the substrate.
//! * A [`TestSigner`] HMAC-SHA256 implementation for tests and
//!   bring-up. **This signer is not post-quantum and must not be used
//!   in production** — it is only here to exercise the trait surface
//!   while ML-DSA-65 lands behind the same trait.
//!
//! The signed envelope ([`SignedBundle`]) is a JSON-canonicalised
//! representation of the bundle plus a detached signature. The
//! canonicalisation is intentionally simple (`serde_json` over a
//! struct with stable field ordering); the same shape is consumed by
//! the ML-DSA-65 signer.

use chrono::{DateTime, Utc};
#[cfg(any(test, feature = "test-support"))]
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
#[cfg(any(test, feature = "test-support"))]
use sha2::Sha256;
use uuid::Uuid;

use crate::errors::CryptoError;

#[cfg(any(test, feature = "test-support"))]
type HmacSha256 = Hmac<Sha256>;

/// Length of the [`TestSigner`] HMAC key (matches the underlying
/// SHA-256 block size; HMAC accepts arbitrary lengths but a 32-byte
/// key is the substrate's standard symmetric-key size).
///
/// Gated behind `#[cfg(any(test, feature = "test-support"))]`
/// alongside [`TestSigner`].
#[cfg(any(test, feature = "test-support"))]
pub const TEST_SIGNER_KEY_LEN: usize = 32;

/// Reference to one evidence row that a provenance bundle was derived
/// from. Kept as a bare [`Uuid`] so `crypto` does not depend on
/// `evidence_store` (the evidence-store crate already re-exports
/// `EvidenceId(Uuid)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EvidenceRef(pub Uuid);

impl EvidenceRef {
    /// Construct from a raw [`Uuid`].
    pub fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    /// Return the underlying [`Uuid`].
    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}

/// Whether the agent that performed the activity is a human or a
/// piece of software (per `docs/technical/design.md` §7.2: "the human or software
/// agent responsible").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    /// A human user (an admin promoting a proposal, a user pinning a
    /// concept, …).
    Human,
    /// A software agent (the synthesis pipeline, an integration
    /// connector, an AI employee, …).
    Software,
}

impl AgentKind {
    /// Stable string tag.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Software => "software",
        }
    }
}

/// The PROV-Activity describing the synthesis run that produced the
/// bundle's entity (per `docs/technical/design.md` §7.2: "agent identity, model
/// version, prompt id, run id").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynthesisActivity {
    /// Identity of the agent that ran the activity (free-form label,
    /// e.g. `"synth-pipeline:elected:device-42"`).
    pub agent_identity: String,
    /// Model name + version (e.g. `"bonsai-1.7b@q1_0_g128-2026-04-01"`).
    pub model_version: String,
    /// Stable prompt id (template id from the prompt catalog).
    pub prompt_id: String,
    /// Unique run id (UUID v4 per invocation).
    pub run_id: Uuid,
}

impl SynthesisActivity {
    /// Construct a fresh activity record.
    pub fn new(
        agent_identity: impl Into<String>,
        model_version: impl Into<String>,
        prompt_id: impl Into<String>,
        run_id: Uuid,
    ) -> Self {
        Self {
            agent_identity: agent_identity.into(),
            model_version: model_version.into(),
            prompt_id: prompt_id.into(),
            run_id,
        }
    }
}

/// The PROV-Agent — who or what is responsible for the activity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvenanceAgent {
    /// Whether this agent is a human or a piece of software.
    pub kind: AgentKind,
    /// Stable identifier (user id, device id, service name, …).
    pub identity: String,
}

impl ProvenanceAgent {
    /// Construct a fresh human agent.
    pub fn human(identity: impl Into<String>) -> Self {
        Self {
            kind: AgentKind::Human,
            identity: identity.into(),
        }
    }

    /// Construct a fresh software agent.
    pub fn software(identity: impl Into<String>) -> Self {
        Self {
            kind: AgentKind::Software,
            identity: identity.into(),
        }
    }
}

/// PROV bundle — the unsigned shape.
///
/// Per `docs/technical/design.md` §7.2 every observation / summary / concept carries
/// one of these. The ML-DSA-65 [`ProvenanceSigner`] is the production
/// implementation; [`TestSigner`] is used for tests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvenanceBundle {
    /// Identifier of the entity (observation / summary / concept) the
    /// bundle is attached to.
    pub entity_id: Uuid,
    /// What the activity was.
    pub activity: SynthesisActivity,
    /// Who or what is responsible for the activity.
    pub agent: ProvenanceAgent,
    /// Evidence rows the bundle was derived from.
    pub derivations: Vec<EvidenceRef>,
    /// Wall-clock creation time.
    pub created_at: DateTime<Utc>,
}

impl ProvenanceBundle {
    /// Construct a fresh bundle.
    pub fn new(
        entity_id: Uuid,
        activity: SynthesisActivity,
        agent: ProvenanceAgent,
        derivations: Vec<EvidenceRef>,
    ) -> Self {
        Self {
            entity_id,
            activity,
            agent,
            derivations,
            created_at: Utc::now(),
        }
    }

    /// Canonical byte representation used as the message under
    /// signature. Uses `serde_json` with a fixed field order (the
    /// struct field order). The ML-DSA-65 signer adopts the same
    /// canonicalisation so existing signatures remain verifiable.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, CryptoError> {
        serde_json::to_vec(self)
            .map_err(|_| CryptoError::ProvenanceSerialisation("serde_json::to_vec failed"))
    }
}

/// Detached signature emitted by [`ProvenanceSigner::sign`] and
/// consumed by [`ProvenanceSigner::verify`]. The byte layout is opaque
/// to consumers — only the matching signer can verify a signature it
/// produced. The test signer uses HMAC-SHA256 (32 bytes); the
/// production signer uses ML-DSA-65 (~3 KB).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvenanceSignature(pub Vec<u8>);

impl ProvenanceSignature {
    /// Borrow the underlying bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// A [`ProvenanceBundle`] together with its detached
/// [`ProvenanceSignature`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedBundle {
    /// The bundle being signed.
    pub bundle: ProvenanceBundle,
    /// Detached signature over [`ProvenanceBundle::canonical_bytes`].
    pub signature: ProvenanceSignature,
}

/// Produce / verify provenance signatures.
///
/// The [`TestSigner`] uses HMAC-SHA256. The ML-DSA-65 implementation
/// sits behind this same trait so the rest of the substrate does not
/// change.
pub trait ProvenanceSigner {
    /// Sign `bundle` and return the [`SignedBundle`] envelope.
    fn sign(&self, bundle: ProvenanceBundle) -> Result<SignedBundle, CryptoError>;

    /// Return `Ok(true)` when `signed.signature` is a valid signature
    /// over `signed.bundle` under this signer's key. Returns
    /// `Ok(false)` for a bona-fide signature mismatch and
    /// `Err(CryptoError::ProvenanceSerialisation)` only when the
    /// bundle could not be canonicalised at all.
    fn verify(&self, signed: &SignedBundle) -> Result<bool, CryptoError>;
}

/// HMAC-SHA256 test signer.
///
/// **Not post-quantum, not for production.** This exists so the
/// [`ProvenanceSigner`] trait can be exercised by tests and callers
/// can wire the integration surface before ML-DSA-65 is adopted.
///
/// Gated behind `#[cfg(any(test, feature = "test-support"))]` so it
/// does not ship in default `cargo build` artifacts. Real production
/// signers live behind [`crate::signer_backend`].
#[cfg(any(test, feature = "test-support"))]
#[derive(Debug, Clone)]
pub struct TestSigner {
    key: [u8; TEST_SIGNER_KEY_LEN],
}

#[cfg(any(test, feature = "test-support"))]
impl TestSigner {
    /// Construct a test signer from a fixed key.
    pub fn new(key: [u8; TEST_SIGNER_KEY_LEN]) -> Self {
        Self { key }
    }

    fn mac(&self, msg: &[u8]) -> Vec<u8> {
        let mut mac = HmacSha256::new_from_slice(&self.key).expect("HMAC-SHA256 accepts any key");
        mac.update(msg);
        mac.finalize().into_bytes().to_vec()
    }
}

#[cfg(any(test, feature = "test-support"))]
impl ProvenanceSigner for TestSigner {
    fn sign(&self, bundle: ProvenanceBundle) -> Result<SignedBundle, CryptoError> {
        let canonical = bundle.canonical_bytes()?;
        let signature = ProvenanceSignature(self.mac(&canonical));
        Ok(SignedBundle { bundle, signature })
    }

    fn verify(&self, signed: &SignedBundle) -> Result<bool, CryptoError> {
        let canonical = signed.bundle.canonical_bytes()?;
        let mut mac = HmacSha256::new_from_slice(&self.key).expect("HMAC-SHA256 accepts any key");
        mac.update(&canonical);
        Ok(mac.verify_slice(signed.signature.as_bytes()).is_ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_key(seed: u8) -> [u8; TEST_SIGNER_KEY_LEN] {
        let mut k = [0u8; TEST_SIGNER_KEY_LEN];
        for (i, byte) in k.iter_mut().enumerate() {
            *byte = u8::try_from(i)
                .expect("TEST_SIGNER_KEY_LEN fits in u8")
                .wrapping_add(seed);
        }
        k
    }

    fn fixture_bundle() -> ProvenanceBundle {
        ProvenanceBundle::new(
            Uuid::nil(),
            SynthesisActivity::new(
                "synth-pipeline:elected:device-42",
                "bonsai-1.7b@q1_0_g128",
                "synth.summary.v1",
                Uuid::nil(),
            ),
            ProvenanceAgent::software("synthesizer:test"),
            vec![EvidenceRef::from_uuid(Uuid::nil())],
        )
    }

    #[test]
    fn round_trip_signs_and_verifies() {
        let signer = TestSigner::new(fixture_key(0));
        let signed = signer.sign(fixture_bundle()).expect("sign");
        assert!(signer.verify(&signed).expect("verify"));
        assert_eq!(signed.signature.as_bytes().len(), 32);
    }

    #[test]
    fn wrong_key_fails_verification() {
        let signer = TestSigner::new(fixture_key(0));
        let signed = signer.sign(fixture_bundle()).expect("sign");
        let wrong = TestSigner::new(fixture_key(1));
        assert!(!wrong.verify(&signed).expect("verify"));
    }

    #[test]
    fn tampered_entity_id_fails_verification() {
        let signer = TestSigner::new(fixture_key(0));
        let mut signed = signer.sign(fixture_bundle()).expect("sign");
        signed.bundle.entity_id = Uuid::from_u128(0xdead_beef);
        assert!(!signer.verify(&signed).expect("verify"));
    }

    #[test]
    fn tampered_signature_fails_verification() {
        let signer = TestSigner::new(fixture_key(0));
        let mut signed = signer.sign(fixture_bundle()).expect("sign");
        signed.signature.0[0] ^= 0x01;
        assert!(!signer.verify(&signed).expect("verify"));
    }

    #[test]
    fn signature_is_deterministic_for_same_inputs() {
        // HMAC-SHA256 with the same key over the same canonical bytes
        // is deterministic; bundles whose `created_at` differs will
        // produce different signatures, so we pin `created_at`.
        let signer = TestSigner::new(fixture_key(7));
        let bundle = ProvenanceBundle {
            created_at: chrono::DateTime::<Utc>::from_timestamp_nanos(0),
            ..fixture_bundle()
        };
        let a = signer.sign(bundle.clone()).expect("sign a");
        let b = signer.sign(bundle).expect("sign b");
        assert_eq!(a.signature, b.signature);
    }

    #[test]
    fn agent_kind_round_trips_through_string_tag() {
        assert_eq!(AgentKind::Human.as_str(), "human");
        assert_eq!(AgentKind::Software.as_str(), "software");
    }
}
