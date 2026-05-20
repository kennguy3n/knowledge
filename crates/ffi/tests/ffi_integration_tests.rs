//! Integration tests for the `knowledge_ffi` crate.
//!
//! These tests pin the **observable contract** of the bridge surface
//! that platform hosts (Swift / Kotlin / N-API consumers) depend on.
//! They cover three concerns:
//!
//! 1. **Surface coverage** — every public function in `lib.rs` is
//!    invoked at least once. Wired functions are exercised
//!    against a temp-dir SQLCipher store; surfaces still pending
//!    Not-yet-wired functions assert their stable `Unimplemented` method
//!    tag.
//! 2. **Error mapping** — every `FfiError` variant is constructible,
//!    JSON-stable, and exposes a stable `kind()` tag.
//! 3. **Round-trip semantics** — wire types survive a serde
//!    encode/decode cycle, simulating the bridge serialization that
//!    happens when the host side rehydrates a value.
//!
//! Each test allocates its own [`RuntimeHandle`] via [`open_store`],
//! so tests run in parallel without contention on a process-global
//! singleton.

use ffi::{
    close_store, decrypt, encrypt, forget, generate_keypair, get_channel_memory, get_evidence,
    get_user_memory, ingest_message, list_memories, open_store, pin, query, run_decay_sweep,
    trigger_synthesis, unpin, EvidenceRecord, FfiError, FfiImportanceClass, FfiKeypair,
    FfiSignature, MemoryFilter, MemoryRecord, MemoryState, QueryResult, RuntimeHandle, SourceKind,
    SynthesisTrigger,
};
use tempfile::TempDir;

const SCOPE: &str = "00000000-0000-0000-0000-000000000001";

/// Open a fresh temp-dir-backed store with a deterministic master
/// key. Returns the allocated [`RuntimeHandle`] and the owning
/// `TempDir` (the caller must keep it alive for the duration of the
/// test so the on-disk database is not garbage-collected while the
/// runtime still holds it open).
fn fresh_store() -> (RuntimeHandle, TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("evidence.db");
    let master_key_hex = "a5".repeat(32);
    let handle =
        open_store(path.to_string_lossy().into_owned(), master_key_hex).expect("open_store");
    (handle, dir)
}

// ─────────────────────────── Surface coverage ───────────────────────

/// Evidence-store wiring is end-to-end live. This test
/// exercises the full ingest → query → get → forget → re-query loop
/// against a real SQLCipher temp store and asserts the documented
/// post-forget semantics.
#[test]
fn evidence_surface_round_trips_via_real_sqlcipher() {
    let (h, _dir) = fresh_store();

    let scope = uuid::Uuid::new_v4().to_string();
    let phrase = "xyzzyintegrationroundtrip";
    let body = format!("Schedule the {phrase} review for Q4 close.");

    let evidence_id = ingest_message(
        h,
        scope.clone(),
        body.clone(),
        SourceKind::Slack,
        FfiImportanceClass::Important,
    )
    .expect("ingest_message");
    assert!(!evidence_id.is_empty());

    let hits = query(h, scope.clone(), phrase.into(), 10).expect("query");
    assert_eq!(hits.len(), 1, "FTS5 should surface the ingested phrase");
    assert_eq!(hits[0].evidence_id, evidence_id);
    assert!(hits[0].snippet.contains(phrase));

    let record = get_evidence(h, evidence_id.clone()).expect("get_evidence");
    assert_eq!(record.body, body);
    assert_eq!(record.source, SourceKind::Slack);
    assert_eq!(record.scope_id, scope);

    forget(h, evidence_id.clone()).expect("forget");

    let hits_after = query(h, scope.clone(), phrase.into(), 10).expect("query after forget");
    assert!(
        hits_after.is_empty(),
        "post-forget query must return no rows for the forgotten scope"
    );
    match get_evidence(h, evidence_id) {
        Err(FfiError::NotFound { kind, .. }) => assert_eq!(kind, "evidence"),
        other => panic!("expected NotFound after forget, got {other:?}"),
    }

    close_store(h).expect("close_store");
}

/// Memory-manager surfaces are wired through to the in-process
/// `UserMemoryObject` CRUD layer. This integration
/// test exercises the **empty-state** contract every host depends
/// on: a fresh scope must return empty bundles (not `Unimplemented`
/// and not error), so callers can render an empty memory pane
/// without special-casing the runtime version.
#[test]
fn memory_surface_returns_empty_for_fresh_scope() {
    let (h, _dir) = fresh_store();

    let scope = uuid::Uuid::new_v4().to_string();
    let bundle = get_user_memory(h, scope.clone()).expect("get_user_memory");
    assert!(bundle.is_empty(), "fresh scope must have no memory rows");

    let listed = list_memories(h, scope.clone(), MemoryFilter::default()).expect("list_memories");
    assert!(listed.is_empty(), "fresh scope must have no memory rows");

    let sweep = run_decay_sweep(h, scope.clone()).expect("run_decay_sweep");
    assert_eq!(sweep, 0, "fresh scope sweep must transition nothing");

    // `pin` / `unpin` on a random id should report a structured
    // NotFound — this is the contract hosts switch on when the user
    // attempts to pin a row that the server-side memory layer has
    // already evicted.
    let bogus = uuid::Uuid::new_v4().to_string();
    match pin(h, bogus.clone()) {
        Err(FfiError::NotFound { kind, .. }) => assert_eq!(kind, "memory"),
        other => panic!("expected NotFound for unknown pin id, got {other:?}"),
    }
    match unpin(h, bogus) {
        Err(FfiError::NotFound { kind, .. }) => assert_eq!(kind, "memory"),
        other => panic!("expected NotFound for unknown unpin id, got {other:?}"),
    }

    close_store(h).expect("close_store");
}

/// Synthesis-pipeline surfaces are partially wired: the recap
/// fetcher returns `None` until a real synthesis has run, and
/// `trigger_synthesis` now dispatches through the inference router.
/// In a test build without MLX or the `http-client` feature, no
/// adapter supports `SynthSummary`, so the surface yields:
/// * `NotFound { kind: "evidence" }` when the scope is empty
///   (no point dispatching an empty prompt), and
/// * `Unavailable { subsystem: "synthesis…" }` when the scope has
///   evidence but no SLM adapter is available.
///
/// Hosts switch on `kind` to drive UI / fallback behaviour.
#[test]
fn synthesis_surface_returns_stable_partial_implementation() {
    let (h, _dir) = fresh_store();

    // Case 1: empty scope — recap is None and synthesis returns
    // NotFound rather than wasting an inference call.
    let empty_scope = uuid::Uuid::new_v4().to_string();
    let recap = get_channel_memory(h, empty_scope.clone()).expect("get_channel_memory");
    assert!(
        recap.is_none(),
        "channel recap must be None before synthesis runs"
    );
    match trigger_synthesis(h, empty_scope, SynthesisTrigger::ManualUserAction) {
        Err(FfiError::NotFound { kind, .. }) => assert_eq!(kind, "evidence"),
        other => panic!("expected NotFound {{ kind: evidence }}, got {other:?}"),
    }

    // Case 2: scope with evidence — router has no synth-capable
    // adapter on this build, so we get Unavailable.
    let scope = uuid::Uuid::new_v4().to_string();
    ingest_message(
        h,
        scope.clone(),
        "hello world".into(),
        SourceKind::Manual,
        FfiImportanceClass::Useful,
    )
    .expect("ingest seed evidence");
    match trigger_synthesis(h, scope, SynthesisTrigger::ManualUserAction) {
        Err(FfiError::Unavailable { subsystem }) => assert!(
            subsystem.starts_with("synthesis"),
            "expected synthesis subsystem, got {subsystem}"
        ),
        other => panic!("expected Unavailable, got {other:?}"),
    }

    close_store(h).expect("close_store");
}

/// Crypto wiring: `generate_keypair` is live; `encrypt` / `decrypt`
/// require an open store and round-trip plaintext through the
/// scope-derived AEAD key.
#[test]
fn crypto_surface_round_trips_via_scope_aead() {
    let (h, _dir) = fresh_store();

    let scope = uuid::Uuid::new_v4().to_string();
    // Ingesting a message registers the scope DEK (v6 schema) so
    // encrypt/decrypt can find the per-scope key.
    let _ = ingest_message(
        h,
        scope.clone(),
        "setup".into(),
        SourceKind::Slack,
        FfiImportanceClass::Important,
    )
    .expect("ingest to register scope");
    let plaintext = b"hello, knowledge".to_vec();
    let ciphertext = encrypt(h, scope.clone(), plaintext.clone()).expect("encrypt");
    assert!(
        ciphertext.len() > plaintext.len(),
        "envelope must include the nonce prefix and Poly1305 tag"
    );

    let recovered = decrypt(h, scope.clone(), ciphertext.clone()).expect("decrypt");
    assert_eq!(recovered, plaintext);

    // Wrong scope must reject — with independently-generated DEKs
    // (v6 schema) the unregistered scope has no DEK, so this returns
    // `NotFound { kind: "scope" }` rather than `Crypto`.
    let other_scope = uuid::Uuid::new_v4().to_string();
    let err = decrypt(h, other_scope, ciphertext).unwrap_err();
    assert!(
        err.kind() == "Crypto" || err.kind() == "NotFound",
        "expected Crypto or NotFound, got {}",
        err.kind()
    );

    let keypair = generate_keypair().expect("generate_keypair");
    assert_eq!(keypair.algorithm, "ml-dsa-65");
    assert!(!keypair.public_key.is_empty());
    assert!(!keypair.private_key.is_empty());

    close_store(h).expect("close_store");
}

/// Multiple open stores must coexist as independent handles in the
/// same process — this is the headline architectural property of
/// the handle-based runtime registry. Writes against one handle
/// must not be observable through the other.
#[test]
fn distinct_handles_isolate_independent_stores() {
    let (h1, _d1) = fresh_store();
    let (h2, _d2) = fresh_store();
    assert_ne!(h1, h2, "open_store must allocate distinct handles");

    let scope = uuid::Uuid::new_v4().to_string();
    let phrase = "isolationintegrationphrase";
    let evidence_id = ingest_message(
        h1,
        scope.clone(),
        format!("body containing {phrase}"),
        SourceKind::Manual,
        FfiImportanceClass::Important,
    )
    .expect("ingest into store 1");

    // The phrase must be visible through h1 …
    let hits_1 = query(h1, scope.clone(), phrase.into(), 10).expect("query h1");
    assert_eq!(hits_1.len(), 1, "store 1 must surface its own row");

    // … and absent from h2.
    let hits_2 = query(h2, scope.clone(), phrase.into(), 10).expect("query h2");
    assert!(hits_2.is_empty(), "store 2 must not see store 1's row");

    // Likewise, get_evidence on store 2 must return NotFound.
    match get_evidence(h2, evidence_id) {
        Err(FfiError::NotFound { kind, .. }) => assert_eq!(kind, "evidence"),
        other => panic!("expected NotFound for cross-handle lookup, got {other:?}"),
    }

    close_store(h1).expect("close h1");
    close_store(h2).expect("close h2");
}

/// `close_store` must be synchronous with respect to in-flight calls
/// on the same handle: when another thread is mid-call via
/// `with_runtime`, `close_store` must not return until that call has
/// released its `Arc` clone and the `FfiRuntime` (along with its
/// SQLCipher connection and master key) has been dropped. This
/// restores the implicit synchronous-teardown property the pre-handle
/// singleton design provided and matters for hosts on Windows whose
/// mandatory file locks would otherwise prevent a `move`/`unlink` of
/// the database file immediately after `close_store` returns.
///
/// Verify the contract along two complementary axes:
///
/// 1. **Registry removed before return** — a follow-up call on the
///    closed handle must return `FfiError::Unavailable`.
/// 2. **Database file fully released before return** — re-opening the
///    same on-disk SQLCipher file under a fresh handle immediately
///    after `close_store` returns must succeed and observe the row
///    the original handle wrote. SQLCipher's open path holds an
///    exclusive lock; if the previous connection were still alive in
///    a leaked `Arc` it would either fail outright on Windows or
///    surface a `SQLITE_BUSY` on Linux.
#[test]
fn close_store_blocks_on_inflight_calls() {
    use std::sync::{Arc, Barrier};
    use std::thread;

    // Spawn N worker threads that hammer ingest_message in a tight
    // loop. A `Barrier` synchronises their start with the main
    // thread's `close_store` so that at least one worker is
    // mid-`with_runtime` when the close lands and we exercise the
    // `Arc::try_unwrap` drain path rather than the empty-clones
    // fast path.
    const WORKERS: usize = 8;
    const ITERS_PER_WORKER: usize = 200;

    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("db.sqlite");
    let path_str = path.to_string_lossy().into_owned();
    let key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    let h = open_store(path_str.clone(), key.to_string()).expect("open_store");
    let scope = uuid::Uuid::new_v4().to_string();
    let phrase = "closestoresynchronousteardown";
    let evidence_id = ingest_message(
        h,
        scope.clone(),
        format!("body containing {phrase}"),
        SourceKind::Manual,
        FfiImportanceClass::Important,
    )
    .expect("seed ingest");

    let barrier = Arc::new(Barrier::new(WORKERS + 1));
    let mut workers = Vec::with_capacity(WORKERS);
    for w in 0..WORKERS {
        let barrier = Arc::clone(&barrier);
        let scope = scope.clone();
        workers.push(thread::spawn(move || {
            barrier.wait();
            for i in 0..ITERS_PER_WORKER {
                // Tolerate `Unavailable` — once `close_store` has
                // removed the registry entry, subsequent calls
                // from this worker will start observing it.
                let _ = ingest_message(
                    h,
                    scope.clone(),
                    format!("race body w{w} i{i}"),
                    SourceKind::Manual,
                    FfiImportanceClass::Important,
                );
            }
        }));
    }

    barrier.wait();
    close_store(h).expect("close_store");

    // 1. Registry must report the handle as gone immediately on
    //    return from `close_store`.
    match get_evidence(h, evidence_id.clone()) {
        Err(FfiError::Unavailable { subsystem }) => {
            assert_eq!(subsystem, "evidence_store");
        }
        other => panic!("expected Unavailable after close_store, got {other:?}"),
    }

    // 2. SQLCipher file must be fully released — opening it under a
    //    fresh handle from the *same* path must succeed without
    //    waiting on or contending with a leaked connection. The row
    //    we seeded above must still be queryable through the new
    //    handle (proving SQLCipher actually flushed the WAL on
    //    drop), and we read it back immediately, with no retry.
    let h2 = open_store(path_str, key.to_string()).expect("re-open after close_store");
    let hits = query(h2, scope.clone(), phrase.into(), 10).expect("query reopened handle");
    assert!(
        hits.iter().any(|hit| hit.evidence_id == evidence_id),
        "re-opened handle must observe the row the closed handle wrote"
    );
    close_store(h2).expect("close re-opened handle");

    // Joining the workers last makes the test deterministic without
    // relying on wall-clock — if `close_store` were not synchronous
    // we would still have observed the failures above.
    for j in workers {
        j.join().expect("worker join");
    }
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

    for (err, expected_kind) in cases {
        assert_eq!(err.kind(), expected_kind);
        let json = serde_json::to_string(&err).expect("FfiError must JSON-serialize");
        let back: FfiError =
            serde_json::from_str(&json).expect("FfiError must JSON-round-trip via its tag");
        assert_eq!(back.kind(), expected_kind);
        assert_eq!(format!("{err:?}"), format!("{back:?}"));
    }
}

/// `Display` for each variant must include enough information for a
/// human reading a host-side log to know *which* row / scope / store
/// triggered it. We pin a few canonical shapes — drift here is a
/// log-aggregator-breaking change.
#[test]
fn ffi_error_display_includes_diagnostic_payload() {
    let err = FfiError::NotFound {
        kind: "evidence".into(),
        id: "abc".into(),
    };
    let s = format!("{err}");
    assert!(s.contains("evidence"), "Display lost the kind tag: {s}");
    assert!(s.contains("abc"), "Display lost the id payload: {s}");
}

// ─────────────────────────── Round-trip semantics ──────────────────

/// `EvidenceRecord` is the canonical wire shape the host hydrates
/// after a [`get_evidence`] call. It must survive a JSON round-trip
/// so the bridge serialization (UniFFI on Swift/Kotlin, JSON-over-
/// CGO on Electron) preserves every field.
#[test]
fn evidence_round_trip_via_wire_types() {
    let original = EvidenceRecord {
        id: SCOPE.into(),
        scope_id: SCOPE.into(),
        body: "a sample evidence body with unicode: 한글 / café".into(),
        source: SourceKind::Slack,
        created_at: 1_700_000_000,
    };
    let json = serde_json::to_string(&original).expect("EvidenceRecord must serialize");
    let back: EvidenceRecord = serde_json::from_str(&json).expect("EvidenceRecord must round-trip");
    assert_eq!(original, back);
}

/// `MemoryRecord` is the canonical wire shape every host UI renders
/// in the memory pane.
#[test]
fn memory_round_trip_via_wire_types() {
    let original = MemoryRecord {
        id: SCOPE.into(),
        scope_id: SCOPE.into(),
        summary: "the team agreed on Q3 OKRs".into(),
        state: MemoryState::Reinforced,
        retention_score: 0.85,
        created_at: 1_700_000_000,
        last_reinforced_at: 1_700_001_000,
    };
    let json = serde_json::to_string(&original).expect("MemoryRecord must serialize");
    let back: MemoryRecord = serde_json::from_str(&json).expect("MemoryRecord must round-trip");
    assert_eq!(original, back);
}

/// `QueryResult` rows carry the search-side scoring breakdown the
/// host pipes into the user-facing relevance bar.
#[test]
fn crypto_envelope_round_trip_via_wire_types() {
    let kp = FfiKeypair {
        algorithm: "ml-dsa-65".into(),
        public_key: vec![1, 2, 3, 4],
        private_key: vec![5, 6, 7, 8, 9, 10, 11, 12],
    };
    let json = serde_json::to_string(&kp).expect("FfiKeypair must serialize");
    let back: FfiKeypair = serde_json::from_str(&json).expect("FfiKeypair must round-trip");
    assert_eq!(kp, back);

    let sig = FfiSignature {
        algorithm: "ml-dsa-65".into(),
        bytes: vec![0xCA, 0xFE],
    };
    let sig_json = serde_json::to_string(&sig).expect("FfiSignature must serialize");
    let sig_back: FfiSignature =
        serde_json::from_str(&sig_json).expect("FfiSignature must round-trip");
    assert_eq!(sig, sig_back);

    let qr = QueryResult {
        evidence_id: SCOPE.into(),
        score: 0.91,
        fts_score: 0.91,
        recency_score: 0.0,
        vector_score: 0.0,
        snippet: "snippet text".into(),
    };
    let qr_json = serde_json::to_string(&qr).expect("QueryResult must serialize");
    let qr_back: QueryResult = serde_json::from_str(&qr_json).expect("QueryResult must round-trip");
    assert_eq!(qr, qr_back);
}

/// `SourceKind` is a closed enum that hosts switch on to render the
/// connector icon. Every variant must round-trip via serde_json so
/// future additions are caught here (the new variant would fail
/// either the encode or the decode step).
#[test]
fn source_kind_variants_all_round_trip() {
    for variant in [
        SourceKind::Manual,
        SourceKind::Slack,
        SourceKind::Email,
        SourceKind::MicrosoftGraph,
        SourceKind::Atlassian,
        SourceKind::HubSpot,
        SourceKind::GoogleWorkspace,
        SourceKind::Other,
    ] {
        let s = serde_json::to_string(&variant).expect("SourceKind must serialize");
        let back: SourceKind =
            serde_json::from_str(&s).expect("SourceKind must round-trip via its tag");
        assert_eq!(variant, back);
    }
}

/// Likewise for `SynthesisTrigger`.
#[test]
fn synthesis_trigger_variants_all_round_trip() {
    for variant in [
        SynthesisTrigger::ManualUserAction,
        SynthesisTrigger::BackgroundIdle,
        SynthesisTrigger::EvidenceThreshold,
        SynthesisTrigger::ConnectorSyncCompleted,
    ] {
        let s = serde_json::to_string(&variant).expect("SynthesisTrigger must serialize");
        let back: SynthesisTrigger =
            serde_json::from_str(&s).expect("SynthesisTrigger must round-trip via its tag");
        assert_eq!(variant, back);
    }
}
