//! Integration tests for the `knowledge_ffi` crate.
//!
//! These tests pin the **observable contract** of the bridge surface
//! that platform hosts (Swift / Kotlin / N-API consumers) depend on.
//! They cover three concerns:
//!
//! 1. **Surface coverage** — every public function in `lib.rs` is
//!    invoked at least once. Wired functions (Phase A) are exercised
//!    against a temp-dir SQLCipher store; surfaces still pending
//!    Phase B / C wiring assert their stable `Unimplemented` method
//!    tag.
//! 2. **Error mapping** — every `FfiError` variant is constructible,
//!    JSON-stable, and exposes a stable `kind()` tag.
//! 3. **Round-trip semantics** — wire types survive a serde
//!    encode/decode cycle, simulating the bridge serialization that
//!    happens when the host side rehydrates a value.

use std::sync::{Mutex, MutexGuard, OnceLock};

use ffi::{
    close_store, decrypt, encrypt, forget, generate_keypair, get_channel_memory, get_evidence,
    get_user_memory, ingest_message, list_memories, open_store, pin, query, run_decay_sweep,
    trigger_synthesis, unpin, EvidenceRecord, FfiError, FfiKeypair, FfiSignature, MemoryFilter,
    MemoryRecord, MemoryState, QueryResult, SourceKind, SynthesisTrigger,
};
use tempfile::TempDir;

const SCOPE: &str = "00000000-0000-0000-0000-000000000001";

/// Serialize tests that touch the process-global FFI singleton. The
/// runtime is one-per-process so parallel tests that call
/// [`open_store`] / [`close_store`] would otherwise clobber each
/// other.
fn singleton_lock() -> MutexGuard<'static, ()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Open a fresh temp-dir-backed store with a deterministic master
/// key. Returns the `TempDir` so the caller can keep it alive for the
/// duration of the test.
fn fresh_store() -> TempDir {
    let _ = close_store();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("evidence.db");
    let master_key_hex = "a5".repeat(32);
    open_store(path.to_string_lossy().into_owned(), master_key_hex).expect("open_store");
    dir
}

/// Convenience: assert the error is `Unimplemented` and that its
/// `method` field matches the call-site name. The wire contract
/// guarantees the method tag is the Rust function name verbatim — a
/// host watchdog can switch on it to surface "this surface is not
/// available in your runtime build" cleanly.
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

/// Evidence-store wiring is end-to-end live as of Phase A. This test
/// exercises the full ingest → query → get → forget → re-query loop
/// against a real SQLCipher temp store and asserts the documented
/// post-forget semantics.
#[test]
fn evidence_surface_round_trips_via_real_sqlcipher() {
    let _g = singleton_lock();
    let _dir = fresh_store();

    let scope = uuid::Uuid::new_v4().to_string();
    let phrase = "xyzzyintegrationroundtrip";
    let body = format!("Schedule the {phrase} review for Q4 close.");

    let evidence_id =
        ingest_message(scope.clone(), body.clone(), SourceKind::Slack).expect("ingest_message");
    assert!(!evidence_id.is_empty());

    let hits = query(scope.clone(), phrase.into(), 10).expect("query");
    assert_eq!(hits.len(), 1, "FTS5 should surface the ingested phrase");
    assert_eq!(hits[0].evidence_id, evidence_id);
    assert!(hits[0].snippet.contains(phrase));

    let record = get_evidence(evidence_id.clone()).expect("get_evidence");
    assert_eq!(record.body, body);
    assert_eq!(record.source, SourceKind::Slack);
    assert_eq!(record.scope_id, scope);

    forget(evidence_id.clone()).expect("forget");

    let hits_after = query(scope.clone(), phrase.into(), 10).expect("query after forget");
    assert!(
        hits_after.is_empty(),
        "post-forget query must return no rows for the forgotten scope"
    );
    match get_evidence(evidence_id) {
        Err(FfiError::NotFound { kind, .. }) => assert_eq!(kind, "evidence"),
        other => panic!("expected NotFound after forget, got {other:?}"),
    }

    close_store().expect("close_store");
}

/// Memory-manager surfaces are still pending Phase B wiring. This
/// test pins the `Unimplemented` contract so hosts that already
/// switch on `method` continue to work, and so a future PR that
/// wires up the surface has to update the test deliberately.
#[test]
fn memory_surface_returns_unimplemented_with_stable_method_tags() {
    let _g = singleton_lock();
    let _dir = fresh_store();

    assert_unimplemented(
        get_user_memory(SCOPE.into()).unwrap_err(),
        "get_user_memory",
    );
    assert_unimplemented(pin(SCOPE.into()).unwrap_err(), "pin");
    assert_unimplemented(unpin(SCOPE.into()).unwrap_err(), "unpin");
    assert_unimplemented(
        list_memories(SCOPE.into(), MemoryFilter::default()).unwrap_err(),
        "list_memories",
    );
    assert_unimplemented(
        run_decay_sweep(SCOPE.into()).unwrap_err(),
        "run_decay_sweep",
    );

    close_store().expect("close_store");
}

/// Synthesis-pipeline surfaces are still pending Phase C wiring.
#[test]
fn synthesis_surface_returns_unimplemented_with_stable_method_tags() {
    let _g = singleton_lock();
    let _dir = fresh_store();

    assert_unimplemented(
        get_channel_memory(SCOPE.into()).unwrap_err(),
        "get_channel_memory",
    );
    assert_unimplemented(
        trigger_synthesis(SCOPE.into(), SynthesisTrigger::ManualUserAction).unwrap_err(),
        "trigger_synthesis",
    );

    close_store().expect("close_store");
}

/// Crypto wiring: `generate_keypair` is live; `encrypt` / `decrypt`
/// require an open store and round-trip plaintext through the
/// scope-derived AEAD key.
#[test]
fn crypto_surface_round_trips_via_scope_aead() {
    let _g = singleton_lock();
    let _dir = fresh_store();

    let scope = uuid::Uuid::new_v4().to_string();
    let plaintext = b"hello, knowledge".to_vec();
    let ciphertext = encrypt(scope.clone(), plaintext.clone()).expect("encrypt");
    assert!(
        ciphertext.len() > plaintext.len(),
        "envelope must include the nonce prefix and Poly1305 tag"
    );

    let recovered = decrypt(scope.clone(), ciphertext.clone()).expect("decrypt");
    assert_eq!(recovered, plaintext);

    // Wrong scope must reject (AEAD AAD binds the scope id).
    let other_scope = uuid::Uuid::new_v4().to_string();
    let err = decrypt(other_scope, ciphertext).unwrap_err();
    assert_eq!(err.kind(), "Crypto");

    let keypair = generate_keypair().expect("generate_keypair");
    assert_eq!(keypair.algorithm, "ml-dsa-65");
    assert!(!keypair.public_key.is_empty());
    assert!(!keypair.private_key.is_empty());

    close_store().expect("close_store");
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
                message: "decay sweep failed".into(),
            },
            "Memory",
        ),
        (
            FfiError::Synthesis {
                message: "router timeout".into(),
            },
            "Synthesis",
        ),
        (
            FfiError::Crypto {
                message: "aead tampered".into(),
            },
            "Crypto",
        ),
        (
            FfiError::Unavailable {
                subsystem: "tee_worker".into(),
            },
            "Unavailable",
        ),
    ];
    for (variant, kind) in cases {
        assert_eq!(variant.kind(), kind, "kind() drifted for {kind}");
        let json = serde_json::to_string(&variant).expect("encode");
        let back: FfiError = serde_json::from_str(&json).expect("decode");
        assert_eq!(
            variant.kind(),
            back.kind(),
            "round-trip kind() mismatch for {kind}"
        );
    }
}

#[test]
fn ffi_error_display_includes_diagnostic_payload() {
    let err = FfiError::Evidence {
        message: "no such row".into(),
    };
    let s = format!("{err}");
    assert!(s.contains("no such row"), "Display drift: got {s}");

    let err = FfiError::NotFound {
        kind: "evidence".into(),
        id: "abc".into(),
    };
    let s = format!("{err}");
    assert!(s.contains("evidence"), "Display drift: got {s}");
    assert!(s.contains("abc"), "Display drift: got {s}");
}

// ──────────────────────── Round-trip wire types ─────────────────────

/// Evidence flow at the wire layer: a host marshals an ingest →
/// query → get_evidence call chain by serializing each
/// intermediate value across the FFI boundary.
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

    let filter = MemoryFilter {
        state: Some(MemoryState::Pinned),
        pinned_only: true,
    };
    let json = serde_json::to_string(&filter).unwrap();
    let back: MemoryFilter = serde_json::from_str(&json).unwrap();
    assert_eq!(filter, back);
}

/// Crypto envelope wire shape (algorithm tag plus opaque key /
/// signature bytes) is what hosts marshal across the bridge. The
/// real `generate_keypair` produces ML-DSA-65 keys; this test
/// guards the *shape* of the envelope so a future swap-in of a
/// different post-quantum signer doesn't break the host's
/// deserializer.
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
