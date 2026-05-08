//! FFI surface contract tests.
//!
//! The Phase 1 deliverable for `knowledge_ffi` is the *interface
//! skeleton* — every entrypoint returns
//! [`FfiError::Unimplemented`] until the per-call orchestration work
//! lands. The contract that platform hosts (Swift / Kotlin /
//! TypeScript) bind against is therefore:
//!
//! 1. Wire types round-trip cleanly through serde with stable
//!    PascalCase tags.
//! 2. Every entrypoint returns a typed [`FfiError`] (not a panic, not
//!    an opaque string), and every Phase 1 entrypoint returns
//!    [`FfiError::Unimplemented`] with a method tag matching the
//!    public function name.
//! 3. The `FfiError` JSON envelope uses an externally-tagged
//!    `kind` / `detail` shape that mirrors what platform decoders see.
//!
//! These integration tests pin those guarantees so a regression in
//! the bridge surface is caught at `cargo test --all` rather than at
//! the platform host.
//!
//! Test 10 — "FFI surface round-trip tests" from the PR-#12/#13
//! follow-up.

use ffi::{
    decrypt, encrypt, forget, generate_keypair, get_channel_memory, get_evidence, get_user_memory,
    ingest_message, list_memories, pin, query, run_decay_sweep, trigger_synthesis, unpin,
    EvidenceRecord, FfiError, FfiKeypair, FfiSignature, MemoryFilter, MemoryRecord, MemoryState,
    QueryResult, SourceKind, SynthesisTrigger,
};

// ─────────────────────── 1. Round-trip pinning ──────────────────────

/// Every Phase 1 entrypoint must return `FfiError::Unimplemented` and
/// the `method` tag must equal the function name. Hosts switch on
/// `kind()` first and then surface the method tag for diagnostics —
/// drift here breaks the platform shells silently.
#[test]
fn every_phase1_entrypoint_returns_unimplemented_with_method_tag() {
    let scope = "00000000-0000-0000-0000-000000000001".to_string();

    // Evidence plane.
    let cases: Vec<(&str, FfiError)> = vec![
        (
            "ingest_message",
            ingest_message(scope.clone(), "body".into(), SourceKind::Manual).unwrap_err(),
        ),
        ("query", query(scope.clone(), "q".into(), 5).unwrap_err()),
        ("get_evidence", get_evidence("id".into()).unwrap_err()),
        // Memory plane.
        (
            "get_user_memory",
            get_user_memory(scope.clone()).unwrap_err(),
        ),
        ("pin", pin("id".into()).unwrap_err()),
        ("unpin", unpin("id".into()).unwrap_err()),
        ("forget", forget("id".into()).unwrap_err()),
        (
            "list_memories",
            list_memories(scope.clone(), MemoryFilter::default()).unwrap_err(),
        ),
        (
            "run_decay_sweep",
            run_decay_sweep(scope.clone()).unwrap_err(),
        ),
        // Synthesis pipeline.
        (
            "get_channel_memory",
            get_channel_memory(scope.clone()).unwrap_err(),
        ),
        (
            "trigger_synthesis",
            trigger_synthesis(scope.clone(), SynthesisTrigger::ManualUserAction).unwrap_err(),
        ),
        // Crypto.
        ("generate_keypair", generate_keypair().unwrap_err()),
        (
            "encrypt",
            encrypt(scope.clone(), b"plaintext".to_vec()).unwrap_err(),
        ),
        (
            "decrypt",
            decrypt(scope, b"ciphertext".to_vec()).unwrap_err(),
        ),
    ];

    for (expected_method, err) in cases {
        assert_eq!(err.kind(), "Unimplemented", "for {expected_method}");
        match err {
            FfiError::Unimplemented { method } => {
                assert_eq!(method, expected_method);
            }
            other => panic!("expected Unimplemented for {expected_method}, got {other:?}"),
        }
    }
}

/// Once a Phase 1 entrypoint surfaces an error, the host can re-encode
/// it as JSON for telemetry and the shape must round-trip through
/// serde — no information is lost.
#[test]
fn unimplemented_error_round_trips_through_json() {
    let err = ingest_message("scope".into(), "body".into(), SourceKind::Manual).unwrap_err();
    let s = serde_json::to_string(&err).expect("error must serialize");
    let back: FfiError = serde_json::from_str(&s).expect("error must deserialize");
    assert_eq!(err, back);

    // Externally-tagged shape — `kind` + `detail` is the contract that
    // platform hosts decode against.
    let v: serde_json::Value = serde_json::from_str(&s).unwrap();
    assert_eq!(v["kind"], "Unimplemented");
    assert!(v["detail"].is_object());
}

/// `query`'s wire output and the `EvidenceRecord` that `get_evidence`
/// will eventually return are both stable wire types — pin a
/// representative round-trip so future shape changes are caught here.
#[test]
fn query_result_and_evidence_record_round_trip_via_serde() {
    let r = QueryResult {
        evidence_id: "00000000-0000-0000-0000-000000000aaa".into(),
        score: 0.81,
        fts_score: 0.5,
        recency_score: 0.3,
        vector_score: 0.4,
        snippet: "hit".into(),
    };
    let s = serde_json::to_string(&r).unwrap();
    let back: QueryResult = serde_json::from_str(&s).unwrap();
    assert_eq!(r, back);

    let e = EvidenceRecord {
        id: "00000000-0000-0000-0000-000000000aaa".into(),
        scope_id: "00000000-0000-0000-0000-000000000bbb".into(),
        body: "ingested body".into(),
        source: SourceKind::Slack,
        created_at: 1_700_000_000,
    };
    let s = serde_json::to_string(&e).unwrap();
    let back: EvidenceRecord = serde_json::from_str(&s).unwrap();
    assert_eq!(e, back);
}

/// `MemoryRecord` is the bundle that hosts render in the UI — assert
/// every field survives a JSON round-trip and that the state enum
/// uses the stable PascalCase tag platform decoders expect.
#[test]
fn memory_record_round_trip_pins_state_tag() {
    let r = MemoryRecord {
        id: "00000000-0000-0000-0000-000000000001".into(),
        scope_id: "00000000-0000-0000-0000-000000000002".into(),
        summary: "user prefers Lisbon time-zone".into(),
        state: MemoryState::Reinforced,
        retention_score: 0.92,
        created_at: 1_700_000_000,
        last_reinforced_at: 1_700_000_500,
    };
    let s = serde_json::to_string(&r).unwrap();
    let v: serde_json::Value = serde_json::from_str(&s).unwrap();
    assert_eq!(v["state"], "Reinforced", "platform decoders pin PascalCase");
    let back: MemoryRecord = serde_json::from_str(&s).unwrap();
    assert_eq!(r, back);
}

/// The `FfiKeypair` / `FfiSignature` pair is what `generate_keypair`
/// will eventually return / what `sign` / `verify` will exchange. The
/// shape must round-trip without re-encoding the binary blobs as
/// hex / base64 (they are JSON arrays of bytes by serde default).
#[test]
fn ffi_keypair_and_signature_round_trip_via_serde() {
    let k = FfiKeypair {
        algorithm: "ml-dsa-65".into(),
        public_key: vec![1, 2, 3, 4, 5],
        private_key: vec![9, 8, 7, 6, 5, 4, 3, 2, 1],
    };
    let s = serde_json::to_string(&k).unwrap();
    let back: FfiKeypair = serde_json::from_str(&s).unwrap();
    assert_eq!(k, back);

    let sig = FfiSignature {
        algorithm: "ml-dsa-65".into(),
        bytes: vec![0x10, 0x20, 0x30],
    };
    let s = serde_json::to_string(&sig).unwrap();
    let back: FfiSignature = serde_json::from_str(&s).unwrap();
    assert_eq!(sig, back);
}

/// `MemoryFilter` is wire-flat with an optional state tag — covers
/// the `None` / `Some(MemoryState::Pinned)` / `pinned_only=true`
/// branches.
#[test]
fn memory_filter_round_trip_covers_optional_state() {
    let cases = vec![
        MemoryFilter::default(),
        MemoryFilter {
            state: Some(MemoryState::Pinned),
            pinned_only: true,
        },
        MemoryFilter {
            state: Some(MemoryState::Decaying),
            pinned_only: false,
        },
    ];
    for f in cases {
        let s = serde_json::to_string(&f).unwrap();
        let back: MemoryFilter = serde_json::from_str(&s).unwrap();
        assert_eq!(f, back);
    }
}

// ─────────────────────── 2. Error mapping ──────────────────────────

/// Pin the JSON shape of `FfiError::InvalidId` — platform hosts decode
/// this when a UUID is malformed and surface it to the UI.
#[test]
fn invalid_id_error_serialises_to_external_tag_shape() {
    let err = FfiError::InvalidId {
        message: "scope_id must be a UUID".into(),
    };
    let s = serde_json::to_string(&err).unwrap();
    let v: serde_json::Value = serde_json::from_str(&s).unwrap();
    assert_eq!(v["kind"], "InvalidId");
    assert_eq!(v["detail"]["message"], "scope_id must be a UUID");
    let back: FfiError = serde_json::from_str(&s).unwrap();
    assert_eq!(err, back);
}

/// Pin the JSON shape of `FfiError::NotFound` — every variant carries
/// its own `detail` payload so hosts don't need to introspect the
/// message string.
#[test]
fn not_found_error_carries_kind_and_id() {
    let err = FfiError::NotFound {
        kind: "evidence".into(),
        id: "00000000-0000-0000-0000-000000000001".into(),
    };
    let s = serde_json::to_string(&err).unwrap();
    let v: serde_json::Value = serde_json::from_str(&s).unwrap();
    assert_eq!(v["kind"], "NotFound");
    assert_eq!(v["detail"]["kind"], "evidence");
    assert_eq!(v["detail"]["id"], "00000000-0000-0000-0000-000000000001");
}

/// Every variant's `kind()` discriminant must be stable — hosts
/// typically `switch (err.kind)` for routing. Drift here is a
/// breaking ABI change.
#[test]
fn ffi_error_kind_strings_are_stable_for_every_variant() {
    let cases: Vec<(FfiError, &str)> = vec![
        (
            FfiError::Unimplemented { method: "x".into() },
            "Unimplemented",
        ),
        (
            FfiError::InvalidId {
                message: "x".into(),
            },
            "InvalidId",
        ),
        (
            FfiError::NotFound {
                kind: "x".into(),
                id: "y".into(),
            },
            "NotFound",
        ),
        (
            FfiError::Evidence {
                message: "x".into(),
            },
            "Evidence",
        ),
        (
            FfiError::Memory {
                message: "x".into(),
            },
            "Memory",
        ),
        (
            FfiError::Synthesis {
                message: "x".into(),
            },
            "Synthesis",
        ),
        (
            FfiError::Crypto {
                message: "x".into(),
            },
            "Crypto",
        ),
        (
            FfiError::Unavailable {
                subsystem: "x".into(),
            },
            "Unavailable",
        ),
    ];
    for (err, expected) in cases {
        assert_eq!(err.kind(), expected);
    }
}

// ─────────────────────── 3. Type conversion ─────────────────────────

/// `SourceKind` and `SynthesisTrigger` are platform-visible enums.
/// Their PascalCase serde tags are part of the contract — pin them.
#[test]
fn source_kind_and_trigger_use_pascal_case_tags() {
    let pairs = [
        (SourceKind::Manual, "\"Manual\""),
        (SourceKind::Slack, "\"Slack\""),
        (SourceKind::MicrosoftGraph, "\"MicrosoftGraph\""),
        (SourceKind::HubSpot, "\"HubSpot\""),
    ];
    for (k, expected) in pairs {
        assert_eq!(serde_json::to_string(&k).unwrap(), expected);
    }

    let triggers = [
        (SynthesisTrigger::ManualUserAction, "\"ManualUserAction\""),
        (SynthesisTrigger::BackgroundIdle, "\"BackgroundIdle\""),
        (SynthesisTrigger::EvidenceThreshold, "\"EvidenceThreshold\""),
        (
            SynthesisTrigger::ConnectorSyncCompleted,
            "\"ConnectorSyncCompleted\"",
        ),
    ];
    for (t, expected) in triggers {
        assert_eq!(serde_json::to_string(&t).unwrap(), expected);
    }
}

/// Stale `MemoryState` from a host JSON blob deserialises back into
/// the canonical Rust enum — tests the inverse direction of the
/// PascalCase tag mapping.
#[test]
fn memory_state_decodes_from_pascal_case_tags() {
    let cases = [
        ("\"Candidate\"", MemoryState::Candidate),
        ("\"Reinforced\"", MemoryState::Reinforced),
        ("\"Decaying\"", MemoryState::Decaying),
        ("\"Archived\"", MemoryState::Archived),
        ("\"Pinned\"", MemoryState::Pinned),
    ];
    for (input, expected) in cases {
        let parsed: MemoryState = serde_json::from_str(input).unwrap();
        assert_eq!(parsed, expected);
    }
}

/// Lower-case / snake-case tags must NOT decode — that would silently
/// mask a host that drifts off the contract.
#[test]
fn memory_state_rejects_non_pascal_case_tags() {
    for bad in ["\"reinforced\"", "\"REINFORCED\"", "\"re_inforced\""] {
        assert!(
            serde_json::from_str::<MemoryState>(bad).is_err(),
            "expected reject for {bad}"
        );
    }
}
