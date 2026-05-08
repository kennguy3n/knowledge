//! Integration tests for the `knowledge_ffi` crate.
//!
//! These tests pin the **observable contract** of the bridge surface
//! that platform hosts (Swift / Kotlin / N-API consumers) depend on.
//! They cover three concerns:
//!
//! 1. **Surface coverage** — every public function in `lib.rs` is
//!    invoked at least once and its Phase 1 contract is asserted.
//! 2. **Error mapping** — every `FfiError` variant is constructible,
//!    JSON-stable, and exposes a stable `kind()` tag.
//! 3. **Round-trip semantics** — wire types survive a serde
//!    encode/decode cycle, simulating the bridge serialization that
//!    happens when the host side rehydrates a value.
//!
//! The Phase 1 skeleton intentionally returns
//! [`FfiError::Unimplemented`] from every business function. These
//! tests are the safety-net that flags any change to that contract:
//! when a method is wired up, a test here will fail and the author
//! will be forced to update the contract surface (and the host
//! shims) deliberately rather than by accident.

use ffi::{
    decrypt, encrypt, forget, generate_keypair, get_channel_memory, get_evidence, get_user_memory,
    ingest_message, list_memories, pin, query, run_decay_sweep, trigger_synthesis, unpin,
    EvidenceRecord, FfiError, FfiKeypair, FfiSignature, MemoryFilter, MemoryRecord, MemoryState,
    QueryResult, SourceKind, SynthesisTrigger,
};

const SCOPE: &str = "00000000-0000-0000-0000-000000000001";

/// Convenience: assert the error is `Unimplemented` and that its
/// `method` field matches the call-site name. The Phase 1 contract
/// guarantees the method tag is the Rust function name verbatim.
fn assert_unimplemented(err: FfiError, expected_method: &str) {
    match err {
        FfiError::Unimplemented { method } => {
            assert_eq!(
                method, expected_method,
                "Unimplemented method tag drifted: got {method:?}, expected {expected_method:?}"
            );
        }
        other => panic!("expected FfiError::Unimplemented, got {other:?}"),
    }
}

// ─────────────────────────── Surface coverage ───────────────────────

#[test]
fn evidence_surface_returns_unimplemented_with_stable_method_tags() {
    assert_unimplemented(
        ingest_message(SCOPE.into(), "hello".into(), SourceKind::Manual).unwrap_err(),
        "ingest_message",
    );
    assert_unimplemented(
        query(SCOPE.into(), "hello".into(), 10).unwrap_err(),
        "query",
    );
    assert_unimplemented(
        get_evidence("00000000-0000-0000-0000-000000000002".into()).unwrap_err(),
        "get_evidence",
    );
}

#[test]
fn memory_surface_returns_unimplemented_with_stable_method_tags() {
    assert_unimplemented(
        get_user_memory(SCOPE.into()).unwrap_err(),
        "get_user_memory",
    );
    assert_unimplemented(pin("id".into()).unwrap_err(), "pin");
    assert_unimplemented(unpin("id".into()).unwrap_err(), "unpin");
    assert_unimplemented(forget("id".into()).unwrap_err(), "forget");
    assert_unimplemented(
        list_memories(SCOPE.into(), MemoryFilter::default()).unwrap_err(),
        "list_memories",
    );
    assert_unimplemented(
        run_decay_sweep(SCOPE.into()).unwrap_err(),
        "run_decay_sweep",
    );
}

#[test]
fn synthesis_surface_returns_unimplemented_with_stable_method_tags() {
    assert_unimplemented(
        get_channel_memory(SCOPE.into()).unwrap_err(),
        "get_channel_memory",
    );
    assert_unimplemented(
        trigger_synthesis(SCOPE.into(), SynthesisTrigger::ManualUserAction).unwrap_err(),
        "trigger_synthesis",
    );
}

#[test]
fn crypto_surface_returns_unimplemented_with_stable_method_tags() {
    assert_unimplemented(generate_keypair().unwrap_err(), "generate_keypair");
    assert_unimplemented(
        encrypt(SCOPE.into(), b"plaintext".to_vec()).unwrap_err(),
        "encrypt",
    );
    assert_unimplemented(
        decrypt(SCOPE.into(), b"ciphertext".to_vec()).unwrap_err(),
        "decrypt",
    );
}

// ──────────────────────────── Error mapping ─────────────────────────

/// Every variant of [`FfiError`] must (a) JSON-round-trip and (b)
/// expose a stable `kind()` discriminant tag. Hosts switch on these
/// to drive recovery, so a silent rename here is a wire-breaking
/// change.
#[test]
fn ffi_error_variants_are_wire_stable() {
    let cases = vec![
        (
            FfiError::Unimplemented {
                method: "ingest_message".into(),
            },
            "Unimplemented",
        ),
        (
            FfiError::InvalidId {
                message: "not a uuid".into(),
            },
            "InvalidId",
        ),
        (
            FfiError::NotFound {
                kind: "evidence".into(),
                id: "abc".into(),
            },
            "NotFound",
        ),
        (
            FfiError::Evidence {
                message: "fts boom".into(),
            },
            "Evidence",
        ),
        (
            FfiError::Memory {
                message: "decay boom".into(),
            },
            "Memory",
        ),
        (
            FfiError::Synthesis {
                message: "synth boom".into(),
            },
            "Synthesis",
        ),
        (
            FfiError::Crypto {
                message: "aead boom".into(),
            },
            "Crypto",
        ),
        (
            FfiError::Unavailable {
                subsystem: "onnx".into(),
            },
            "Unavailable",
        ),
    ];
    for (err, kind_tag) in cases {
        assert_eq!(err.kind(), kind_tag);
        let json = serde_json::to_string(&err).expect("serialize");
        let back: FfiError = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(err, back, "JSON round-trip dropped variant {kind_tag}");
    }
}

#[test]
fn ffi_error_display_includes_diagnostic_payload() {
    let err = FfiError::NotFound {
        kind: "memory".into(),
        id: "missing".into(),
    };
    let msg = err.to_string();
    assert!(msg.contains("memory"));
    assert!(msg.contains("missing"));
}

// ─────────────────────── Round-trip simulations ─────────────────────

/// Phase 1 contract: the substrate's evidence-store wire types must
/// survive a serde round-trip, since UniFFI / N-API rehydrate them
/// on the host side. This simulates ingest → query → get_evidence
/// at the wire-type layer (the actual call sites all return
/// `Unimplemented` today, asserted above).
#[test]
fn evidence_round_trip_via_wire_types() {
    let ingested = EvidenceRecord {
        id: "00000000-0000-0000-0000-000000000010".into(),
        scope_id: SCOPE.into(),
        body: "deadline reminder".into(),
        source: SourceKind::Slack,
        created_at: 1_700_000_000,
    };
    let json = serde_json::to_string(&ingested).unwrap();
    let after_ingest: EvidenceRecord = serde_json::from_str(&json).unwrap();
    assert_eq!(ingested, after_ingest);

    let hit = QueryResult {
        evidence_id: ingested.id.clone(),
        score: 0.9,
        fts_score: 0.6,
        recency_score: 0.4,
        vector_score: 0.5,
        snippet: "deadline".into(),
    };
    let json = serde_json::to_string(&hit).unwrap();
    let after_query: QueryResult = serde_json::from_str(&json).unwrap();
    assert_eq!(hit, after_query);
    assert_eq!(after_query.evidence_id, after_ingest.id);

    // get_evidence simulation: the host re-fetches by id and gets
    // back the same record.
    let json = serde_json::to_string(&ingested).unwrap();
    let after_get: EvidenceRecord = serde_json::from_str(&json).unwrap();
    assert_eq!(after_get.id, after_query.evidence_id);
    assert_eq!(after_get.body, "deadline reminder");
}

#[test]
fn memory_round_trip_via_wire_types() {
    let record = MemoryRecord {
        id: "00000000-0000-0000-0000-000000000020".into(),
        scope_id: SCOPE.into(),
        summary: "user prefers Lisbon time-zone".into(),
        state: MemoryState::Reinforced,
        retention_score: 0.87,
        created_at: 1_700_000_000,
        last_reinforced_at: 1_700_000_500,
    };
    let json = serde_json::to_string(&record).unwrap();
    let back: MemoryRecord = serde_json::from_str(&json).unwrap();
    assert_eq!(record, back);

    // MemoryFilter spans Option<MemoryState> + bool; both must
    // survive the bridge serializer.
    let filter = MemoryFilter {
        state: Some(MemoryState::Pinned),
        pinned_only: true,
    };
    let json = serde_json::to_string(&filter).unwrap();
    let back: MemoryFilter = serde_json::from_str(&json).unwrap();
    assert_eq!(filter, back);
}

/// Crypto round-trip: generate_keypair → encrypt → decrypt simulated
/// at the wire-type layer. The actual functions return
/// `Unimplemented` today; this test guards the *envelope* contract
/// (algorithm tag plus opaque key/signature bytes) so when the real
/// implementation lands, the wire shape doesn't drift.
#[test]
fn crypto_envelope_round_trip_via_wire_types() {
    let keypair = FfiKeypair {
        algorithm: "ml-dsa-65".into(),
        public_key: vec![0x01, 0x02, 0x03, 0x04],
        private_key: vec![0xff, 0xee, 0xdd, 0xcc],
    };
    let json = serde_json::to_string(&keypair).unwrap();
    let back: FfiKeypair = serde_json::from_str(&json).unwrap();
    assert_eq!(keypair, back);

    let signature = FfiSignature {
        algorithm: keypair.algorithm.clone(),
        bytes: vec![0xab, 0xcd, 0xef],
    };
    let json = serde_json::to_string(&signature).unwrap();
    let back: FfiSignature = serde_json::from_str(&json).unwrap();
    assert_eq!(signature, back);

    // The Phase 1 stubs of encrypt/decrypt still return
    // Unimplemented — but they accept and reject the right wire
    // shape (Vec<u8> in, Vec<u8> out), which is what hosts depend
    // on.
    let plaintext = b"hello, knowledge".to_vec();
    let enc_err = encrypt(SCOPE.into(), plaintext.clone()).unwrap_err();
    let dec_err = decrypt(SCOPE.into(), plaintext).unwrap_err();
    assert_eq!(enc_err.kind(), "Unimplemented");
    assert_eq!(dec_err.kind(), "Unimplemented");
}

/// Cover every `SourceKind` variant via serde round-trip — this is
/// the connector-tag contract the host side switches on.
#[test]
fn source_kind_variants_all_round_trip() {
    let kinds = [
        SourceKind::Manual,
        SourceKind::Slack,
        SourceKind::Email,
        SourceKind::MicrosoftGraph,
        SourceKind::Atlassian,
        SourceKind::HubSpot,
        SourceKind::GoogleWorkspace,
        SourceKind::Other,
    ];
    for kind in kinds {
        let json = serde_json::to_string(&kind).unwrap();
        let back: SourceKind = serde_json::from_str(&json).unwrap();
        assert_eq!(kind, back);
    }
}

#[test]
fn synthesis_trigger_variants_all_round_trip() {
    let triggers = [
        SynthesisTrigger::ManualUserAction,
        SynthesisTrigger::BackgroundIdle,
        SynthesisTrigger::EvidenceThreshold,
        SynthesisTrigger::ConnectorSyncCompleted,
    ];
    for t in triggers {
        let json = serde_json::to_string(&t).unwrap();
        let back: SynthesisTrigger = serde_json::from_str(&json).unwrap();
        assert_eq!(t, back);
    }
}
