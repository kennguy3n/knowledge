//! Attestation reports bound to synthesizer keys.
//!
//! Per `docs/technical/design.md` §10 every synthesis run in confidential-compute
//! mode produces an attestation report that binds the synthesizer's
//! public key to a TEE measurement. This module provides:
//!
//! * [`AttestationReport`] — the report itself (measurement, platform,
//!   timestamp, signature).
//! * [`TeePlatform`] enum — Intel TDX, AMD SEV-SNP, Nitro Enclaves,
//!   or a mock platform for tests.
//! * [`AttestationBinding`] — binds a synthesizer's public key hash
//!   to an attestation report so consumers can verify that a given
//!   synthesis output was produced inside the attested enclave.
//! * [`verify_attestation`] — checks a report against an expected
//!   measurement digest.
//! * [`bind_synthesizer_key`] — creates the binding.
//! * [`AttestationAuditEntry`] — links an attestation to the audit
//!   trail so every confidential synthesis run is auditable.
//!
//! The current implementation uses mock attestation for all platforms
//! (the real TEE quote-verification libraries are platform-specific
//! C FFI and will land behind feature flags in a future update).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::errors::CryptoError;
use crate::hash::{content_hash, ContentHash};

/// Supported TEE platforms.
///
/// Currently ships `Mock` for testing; production implementations will
/// land behind feature flags (`intel-tdx`, `amd-sev-snp`,
/// `nitro-enclaves`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeePlatform {
    /// Intel Trust Domain Extensions.
    IntelTdx,
    /// AMD Secure Encrypted Virtualisation — Secure Nested Paging.
    AmdSevSnp,
    /// AWS Nitro Enclaves.
    NitroEnclaves,
    /// Mock platform for tests. **Not for production.**
    Mock,
}

impl TeePlatform {
    /// Stable string tag.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::IntelTdx => "intel_tdx",
            Self::AmdSevSnp => "amd_sev_snp",
            Self::NitroEnclaves => "nitro_enclaves",
            Self::Mock => "mock",
        }
    }
}

/// An attestation report from a TEE platform.
///
/// The `measurement` is the hash of the enclave image (MRTD for TDX,
/// MEASUREMENT for SNP, PCR0 for Nitro). The `signature` is the
/// platform-specific quote signature; for the `Mock` platform this is
/// an HMAC-SHA256 over the canonical report bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttestationReport {
    /// Unique report id.
    pub report_id: Uuid,
    /// TEE platform that produced the report.
    pub platform: TeePlatform,
    /// Enclave measurement (image hash / MRTD / PCR0).
    pub measurement: ContentHash,
    /// Opaque platform-specific report data (nonce binding, user
    /// data field, etc.).
    pub report_data: Vec<u8>,
    /// Platform-specific quote signature over the report.
    pub signature: Vec<u8>,
    /// Wall-clock time the report was produced.
    pub created_at: DateTime<Utc>,
}

impl AttestationReport {
    /// Construct a new attestation report.
    pub fn new(
        platform: TeePlatform,
        measurement: ContentHash,
        report_data: Vec<u8>,
        signature: Vec<u8>,
    ) -> Self {
        Self {
            report_id: Uuid::new_v4(),
            platform,
            measurement,
            report_data,
            signature,
            created_at: Utc::now(),
        }
    }

    /// Canonical byte representation for audit / binding purposes.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, CryptoError> {
        serde_json::to_vec(self)
            .map_err(|_| CryptoError::ProvenanceSerialisation("AttestationReport::canonical_bytes"))
    }
}

/// Binding between a synthesizer's public key and an attestation
/// report. Consumers use this to verify that a synthesis output was
/// produced inside the attested enclave by the key holder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttestationBinding {
    /// Id of this binding.
    pub binding_id: Uuid,
    /// Id of the attestation report this binding refers to.
    pub report_id: Uuid,
    /// BLAKE3 hash of the synthesizer's public key bytes.
    pub synthesizer_key_hash: ContentHash,
    /// The raw synthesizer public key bytes (so consumers can re-hash
    /// and compare).
    pub synthesizer_pub_key: Vec<u8>,
    /// TEE platform from the report.
    pub platform: TeePlatform,
    /// Wall-clock time the binding was created.
    pub created_at: DateTime<Utc>,
}

/// Audit-trail entry for attestation events. Links back to both the
/// report and the binding so the full chain of custody is auditable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttestationAuditEntry {
    /// Unique audit entry id.
    pub entry_id: Uuid,
    /// Report id.
    pub report_id: Uuid,
    /// Binding id (if a binding was created in the same flow).
    pub binding_id: Option<Uuid>,
    /// Scope in which the synthesis was performed.
    pub scope_id: Uuid,
    /// TEE platform.
    pub platform: TeePlatform,
    /// Whether the attestation was verified successfully.
    pub verified: bool,
    /// Optional reason for failure (empty on success).
    pub failure_reason: Option<String>,
    /// Wall-clock time.
    pub created_at: DateTime<Utc>,
}

impl AttestationAuditEntry {
    /// Construct a successful attestation audit entry.
    pub fn success(
        report_id: Uuid,
        binding_id: Uuid,
        scope_id: Uuid,
        platform: TeePlatform,
    ) -> Self {
        Self {
            entry_id: Uuid::new_v4(),
            report_id,
            binding_id: Some(binding_id),
            scope_id,
            platform,
            verified: true,
            failure_reason: None,
            created_at: Utc::now(),
        }
    }

    /// Construct a failed attestation audit entry.
    pub fn failure(
        report_id: Uuid,
        scope_id: Uuid,
        platform: TeePlatform,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            entry_id: Uuid::new_v4(),
            report_id,
            binding_id: None,
            scope_id,
            platform,
            verified: false,
            failure_reason: Some(reason.into()),
            created_at: Utc::now(),
        }
    }
}

/// Verify an attestation report against an expected enclave
/// measurement.
///
/// For the `Mock` platform, verification succeeds when
/// `report.measurement == expected_measurement`. Real platform
/// implementations will additionally verify the platform signature
/// against the vendor's root of trust.
pub fn verify_attestation(
    report: &AttestationReport,
    expected_measurement: &ContentHash,
) -> Result<bool, CryptoError> {
    match report.platform {
        TeePlatform::Mock => Ok(report.measurement == *expected_measurement),
        // Real platform verification stubs — always return
        // measurement-match for now; production implementations will
        // verify the quote signature against the vendor CA.
        TeePlatform::IntelTdx | TeePlatform::AmdSevSnp | TeePlatform::NitroEnclaves => {
            Ok(report.measurement == *expected_measurement)
        }
    }
}

/// Bind a synthesizer's public key to a verified attestation report.
///
/// The caller must have already verified the report via
/// [`verify_attestation`]. This function hashes the public key with
/// BLAKE3 and stores the hash alongside the report reference so that
/// downstream consumers can cheaply verify that a given synthesis
/// output came from the attested enclave.
pub fn bind_synthesizer_key(
    report: &AttestationReport,
    synthesizer_pub_key: &[u8],
) -> AttestationBinding {
    let key_hash = content_hash(synthesizer_pub_key);
    AttestationBinding {
        binding_id: Uuid::new_v4(),
        report_id: report.report_id,
        synthesizer_key_hash: key_hash,
        synthesizer_pub_key: synthesizer_pub_key.to_vec(),
        platform: report.platform,
        created_at: Utc::now(),
    }
}

/// Create a mock attestation report for testing. Produces a report
/// whose `measurement` is the BLAKE3 hash of `enclave_image` and
/// whose `report_data` binds to the supplied nonce.
pub fn mock_attestation_report(enclave_image: &[u8], nonce: &[u8]) -> AttestationReport {
    let measurement = content_hash(enclave_image);
    // Mock signature: BLAKE3 over (measurement || report_data).
    let mut sig_input = Vec::new();
    sig_input.extend_from_slice(&measurement);
    sig_input.extend_from_slice(nonce);
    let sig = content_hash(&sig_input);
    AttestationReport::new(TeePlatform::Mock, measurement, nonce.to_vec(), sig.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENCLAVE_IMAGE: &[u8] = b"synthesizer-enclave-v1.0";
    const NONCE: &[u8] = b"session-nonce-42";
    const PUB_KEY: &[u8] = b"mock-synthesizer-pub-key-32bytes";

    #[test]
    fn mock_report_creation_and_measurement() {
        let report = mock_attestation_report(ENCLAVE_IMAGE, NONCE);
        assert_eq!(report.platform, TeePlatform::Mock);
        assert_eq!(report.measurement, content_hash(ENCLAVE_IMAGE));
        assert_eq!(report.report_data, NONCE);
        assert!(!report.signature.is_empty());
    }

    #[test]
    fn verify_correct_measurement_succeeds() {
        let report = mock_attestation_report(ENCLAVE_IMAGE, NONCE);
        let expected = content_hash(ENCLAVE_IMAGE);
        assert!(verify_attestation(&report, &expected).expect("verify"));
    }

    #[test]
    fn verify_wrong_measurement_fails() {
        let report = mock_attestation_report(ENCLAVE_IMAGE, NONCE);
        let wrong = content_hash(b"different-enclave-image");
        assert!(!verify_attestation(&report, &wrong).expect("verify"));
    }

    #[test]
    fn bind_synthesizer_key_creates_correct_binding() {
        let report = mock_attestation_report(ENCLAVE_IMAGE, NONCE);
        let binding = bind_synthesizer_key(&report, PUB_KEY);
        assert_eq!(binding.report_id, report.report_id);
        assert_eq!(binding.synthesizer_key_hash, content_hash(PUB_KEY));
        assert_eq!(binding.synthesizer_pub_key, PUB_KEY);
        assert_eq!(binding.platform, TeePlatform::Mock);
    }

    #[test]
    fn binding_key_hash_matches_independent_hash() {
        let report = mock_attestation_report(ENCLAVE_IMAGE, NONCE);
        let binding = bind_synthesizer_key(&report, PUB_KEY);
        let independent_hash = content_hash(PUB_KEY);
        assert_eq!(binding.synthesizer_key_hash, independent_hash);
    }

    #[test]
    fn different_keys_produce_different_bindings() {
        let report = mock_attestation_report(ENCLAVE_IMAGE, NONCE);
        let b1 = bind_synthesizer_key(&report, b"key-alpha");
        let b2 = bind_synthesizer_key(&report, b"key-beta");
        assert_ne!(b1.synthesizer_key_hash, b2.synthesizer_key_hash);
        assert_ne!(b1.binding_id, b2.binding_id);
    }

    #[test]
    fn audit_entry_success_records_all_fields() {
        let report = mock_attestation_report(ENCLAVE_IMAGE, NONCE);
        let binding = bind_synthesizer_key(&report, PUB_KEY);
        let scope_id = Uuid::new_v4();
        let entry = AttestationAuditEntry::success(
            report.report_id,
            binding.binding_id,
            scope_id,
            report.platform,
        );
        assert!(entry.verified);
        assert!(entry.failure_reason.is_none());
        assert_eq!(entry.report_id, report.report_id);
        assert_eq!(entry.binding_id, Some(binding.binding_id));
        assert_eq!(entry.scope_id, scope_id);
        assert_eq!(entry.platform, TeePlatform::Mock);
    }

    #[test]
    fn audit_entry_failure_records_reason() {
        let report = mock_attestation_report(ENCLAVE_IMAGE, NONCE);
        let scope_id = Uuid::new_v4();
        let entry = AttestationAuditEntry::failure(
            report.report_id,
            scope_id,
            report.platform,
            "measurement mismatch",
        );
        assert!(!entry.verified);
        assert_eq!(
            entry.failure_reason.as_deref(),
            Some("measurement mismatch")
        );
        assert!(entry.binding_id.is_none());
    }

    #[test]
    fn full_attestation_flow_mock_tee() {
        // 1. Enclave boots and produces a report.
        let report = mock_attestation_report(ENCLAVE_IMAGE, NONCE);
        // 2. Verifier checks the measurement.
        let expected = content_hash(ENCLAVE_IMAGE);
        assert!(verify_attestation(&report, &expected).expect("verify"));
        // 3. Bind the synthesizer's public key to the report.
        let binding = bind_synthesizer_key(&report, PUB_KEY);
        assert_eq!(binding.report_id, report.report_id);
        // 4. Emit an audit entry.
        let scope_id = Uuid::new_v4();
        let entry = AttestationAuditEntry::success(
            report.report_id,
            binding.binding_id,
            scope_id,
            report.platform,
        );
        assert!(entry.verified);
    }

    #[test]
    fn report_canonical_bytes_serialises() {
        let report = mock_attestation_report(ENCLAVE_IMAGE, NONCE);
        let bytes = report.canonical_bytes().expect("serialise");
        let deser: AttestationReport =
            serde_json::from_slice(&bytes).expect("deserialise round-trip");
        assert_eq!(deser.report_id, report.report_id);
        assert_eq!(deser.measurement, report.measurement);
    }

    #[test]
    fn tee_platform_string_tags() {
        assert_eq!(TeePlatform::IntelTdx.as_str(), "intel_tdx");
        assert_eq!(TeePlatform::AmdSevSnp.as_str(), "amd_sev_snp");
        assert_eq!(TeePlatform::NitroEnclaves.as_str(), "nitro_enclaves");
        assert_eq!(TeePlatform::Mock.as_str(), "mock");
    }

    #[test]
    fn tee_platform_serde_round_trip() {
        for platform in [
            TeePlatform::IntelTdx,
            TeePlatform::AmdSevSnp,
            TeePlatform::NitroEnclaves,
            TeePlatform::Mock,
        ] {
            let json = serde_json::to_string(&platform).expect("ser");
            let deser: TeePlatform = serde_json::from_str(&json).expect("deser");
            assert_eq!(deser, platform);
        }
    }
}
