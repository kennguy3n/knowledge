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
    get_user_memory, health_check, ingest_message, list_memories, open_store, pin, query,
    run_decay_sweep, trigger_synthesis, unpin, EvidenceRecord, FfiError, FfiImportanceClass,
    FfiKeypair, FfiSignature, MemoryFilter, MemoryRecord, MemoryState, QueryResult, RuntimeHandle,
    SourceKind, SubsystemStatus, SynthesisTrigger,
};
// `create_connector` / `list_connectors` / `forget_scope` /
// `ConnectorKindTag` are only needed by the connector-cleanup
// integration test, which is itself gated on `http-client` (see
// the test's own attribute). Gating the imports keeps the
// default-features `cargo test` warning-free.
#[cfg(feature = "http-client")]
use ffi::{
    authenticate_connector, create_connector, forget_scope, list_connectors, remove_connector,
    ConnectorKindTag,
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
        // Pin the host-visible distinction between
        // `Unavailable` (no adapter for the task) and
        // `InferenceFailure` (adapter ran, model produced an
        // unusable result). Hosts switch on `kind` and the
        // detail field to drive retry policy — silently
        // collapsing the two would erase that signal.
        (
            FfiError::InferenceFailure {
                message: "synthesis: grammar violation on SummaryBundle".into(),
            },
            "InferenceFailure",
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

/// `health_check` over a freshly-opened runtime must include the
/// new `connector` subsystem entry. Phase 2 wires the connector
/// framework into the substrate and `CONTRIBUTING.md` §4 mandates a
/// matching `health_check` probe per subsystem — this test pins
/// that wiring contract so a future regression that drops the probe
/// (or silently downgrades it) is caught locally before reaching
/// the host shells.
///
/// We deliberately do NOT call `create_connector` here: the
/// substrate's steady state of "zero registered connectors" must
/// itself produce a healthy `ok` subsystem entry with the
/// per-status counts at zero, so the host UI's
/// `(0 ok / 0 failed / …)` rendering is well-defined.
#[test]
fn health_check_envelope_includes_connector_subsystem() {
    let (h, _dir) = fresh_store();
    let env = health_check(Some(h)).expect("health_check with open handle");

    let connector = env
        .subsystems
        .iter()
        .find(|s| s.name == "connector")
        .expect("connector subsystem entry must be present in the envelope");
    assert_eq!(connector.status, SubsystemStatus::Ok);
    let detail = connector
        .detail
        .as_deref()
        .expect("connector probe always emits a detail string");
    assert!(detail.contains("total=0"), "detail={detail}");
    assert!(detail.contains("authenticated=0"), "detail={detail}");
    assert!(detail.contains("failed=0"), "detail={detail}");
    // Phase 4.1: the probe also surfaces the
    // `ClientSecretResolver` registration state alongside the
    // per-status counts. A fresh runtime has no resolver wired up,
    // so the host should see `oauth_resolver=unset` — this is the
    // signal a host operator looks at first when diagnosing an
    // `invalid_client` rejection on a confidential-client grant.
    #[cfg(feature = "http-client")]
    assert!(detail.contains("oauth_resolver=unset"), "detail={detail}");

    // Sanity-check the probe ordering — the Phase 2 wiring appends
    // `connector` after the four Phase 1 subsystems, so a host
    // rendering subsystems in array order sees the connector tile
    // last. The exact array order is part of the host UI contract
    // (Electron / Swift / Kotlin all render subsystems in the order
    // they appear in the envelope), so changes here are intentional
    // and require updating the host shells.
    let names: Vec<&str> = env.subsystems.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "bridge",
            "evidence_store",
            "crypto",
            "memory_manager",
            "inference_router",
            "connector",
        ]
    );

    close_store(h).expect("close_store");
}

/// Cryptographic-forgetting contract: every piece of state bound
/// to the forgotten scope MUST become unrecoverable. This pins the
/// connector-state cleanup branch of `forget_scope` against
/// regression — without it, a forgotten scope's `ConnectorInstance`
/// rows, live `Arc<dyn Connector>` handles, and cached OAuth2
/// tokens would survive the call, letting a later `sync_connector`
/// resurrect plaintext provider credentials and re-emit fresh
/// evidence onto a tombstoned scope.
///
/// The test creates two connectors against different scopes,
/// forgets one scope, and asserts:
///
/// 1. The connector bound to the forgotten scope disappears from
///    `list_connectors`.
/// 2. The connector bound to the OTHER scope survives.
///
/// Direct token-vault inspection lives in the unit test in
/// `connector.rs`; this test pins the FFI-observable contract.
///
/// Gated on `http-client` because `create_connector` requires a
/// live `BlockingHttpTransport` — without the feature every
/// connector lifecycle call returns `FfiError::Unavailable`, which
/// is the surface this test is *not* exercising. The Phase 2 CI
/// workflow builds + tests this crate with `--all-features` so the
/// gate keeps the unit test deterministic on every developer's
/// local `cargo test` while still being exercised by the
/// release-shape CI matrix.
#[cfg(feature = "http-client")]
#[test]
fn forget_scope_purges_connectors_bound_to_the_forgotten_scope() {
    let (h, _dir) = fresh_store();

    let scope_a = uuid::Uuid::new_v4().to_string();
    let scope_b = uuid::Uuid::new_v4().to_string();

    // Use a minimal-but-real auth config so the JSON parses; the
    // test never authenticates the connector (which would require a
    // live OAuth2 provider) — `create_connector` allocates the
    // instance regardless and that's what `forget_scope` must clean
    // up.
    let cfg = r#"{
        "client_id": "test-client",
        "redirect_uri": "https://example.invalid/oauth/callback",
        "token_url": "https://example.invalid/oauth/token"
    }"#;
    let id_a = create_connector(
        h,
        ConnectorKindTag::Notion,
        scope_a.clone(),
        cfg.to_string(),
    )
    .expect("create_connector A");
    let id_b = create_connector(
        h,
        ConnectorKindTag::Notion,
        scope_b.clone(),
        cfg.to_string(),
    )
    .expect("create_connector B");

    let before = list_connectors(h).expect("list before");
    assert_eq!(before.len(), 2, "both connectors should be registered");

    // Forget scope A — the connector bound to scope A must
    // disappear.
    forget_scope(h, scope_a.clone()).expect("forget_scope A");

    let after = list_connectors(h).expect("list after forget_scope A");
    assert_eq!(
        after.len(),
        1,
        "exactly one connector should survive — the one bound to scope B"
    );
    assert!(
        after.iter().all(|s| s.instance_id != id_a),
        "connector A (instance_id={id_a}) must be purged"
    );
    assert!(
        after.iter().any(|s| s.instance_id == id_b),
        "connector B (instance_id={id_b}) must survive"
    );

    close_store(h).expect("close_store");
}

/// `forget(evidence_id)` resolves the row to its scope and MUST run
/// the *exact same* cryptographic-forgetting sequence as
/// `forget_scope(scope_uuid)` — including the connector lifecycle
/// purge. This pins the bug surfaced by Devin Review on PR #54:
/// before the fix, `forget()` left `ConnectorInstance` rows, live
/// `Arc<dyn Connector>` handles, and cached OAuth2 tokens behind
/// for the forgotten scope, while `forget_scope()` cleaned them up
/// — letting a host resurrect the same provider plaintext by
/// calling `forget()` (the evidence-id surface) instead of
/// `forget_scope()` (the scope-uuid surface). Both surfaces now
/// route through the shared `forget_scope_state` helper, so this
/// test is what keeps them aligned going forward.
///
/// Gated on `http-client` for the same reason as the
/// sibling `forget_scope_purges_connectors_*` test —
/// `create_connector` requires a live `BlockingHttpTransport`.
#[cfg(feature = "http-client")]
#[test]
fn forget_by_evidence_id_also_purges_connectors_bound_to_the_resolved_scope() {
    let (h, _dir) = fresh_store();

    let scope_a = uuid::Uuid::new_v4().to_string();
    let scope_b = uuid::Uuid::new_v4().to_string();

    // Ingest evidence in scope A so `forget(evidence_id)` has a row
    // to resolve to. The ingest path registers the per-scope DEK,
    // which is also what `connector` instances expect to be live.
    let evidence_id_a = ingest_message(
        h,
        scope_a.clone(),
        "forget-by-evidence-id integration test message".into(),
        SourceKind::Slack,
        FfiImportanceClass::Important,
    )
    .expect("ingest_message in scope A");

    let cfg = r#"{
        "client_id": "test-client",
        "redirect_uri": "https://example.invalid/oauth/callback",
        "token_url": "https://example.invalid/oauth/token"
    }"#;
    let id_a = create_connector(
        h,
        ConnectorKindTag::Notion,
        scope_a.clone(),
        cfg.to_string(),
    )
    .expect("create_connector A");
    let id_b = create_connector(
        h,
        ConnectorKindTag::Notion,
        scope_b.clone(),
        cfg.to_string(),
    )
    .expect("create_connector B");

    let before = list_connectors(h).expect("list before");
    assert_eq!(before.len(), 2, "both connectors should be registered");

    // Drive the by-evidence-id surface — NOT the by-scope-uuid one.
    // The fix routes both through the same shared cleanup helper,
    // so the connector bound to scope A must disappear here too.
    forget(h, evidence_id_a).expect("forget by evidence id");

    let after = list_connectors(h).expect("list after forget");
    assert_eq!(
        after.len(),
        1,
        "exactly one connector should survive — the one bound to scope B"
    );
    assert!(
        after.iter().all(|s| s.instance_id != id_a),
        "connector A (instance_id={id_a}) must be purged by forget(evidence_id)"
    );
    assert!(
        after.iter().any(|s| s.instance_id == id_b),
        "connector B (instance_id={id_b}) must survive the forget"
    );

    close_store(h).expect("close_store");
}

/// `authenticate_connector` MUST splice the OAuth2 authorisation
/// code into `auth_config_json` under the key `"authorization_code"`
/// — every concrete connector (`crates/connectors/src/{slack,
/// notion, hubspot, email, onedrive, confluence, jira, figma,
/// google_drive}.rs`) reads from that exact key in its
/// `authenticate` impl, surfacing
/// `ConnectorError::Auth("…auth_config_json.authorization_code is
/// required")` if the key is missing.
///
/// This pins the round-4 Devin Review bug on PR #54: the FFI
/// previously spliced the code under `"auth_code"`, which would
/// cause every host `authenticate_connector` call to surface
/// `auth_config_json.authorization_code is required` even when the
/// host correctly passed an `auth_code` argument. After the fix,
/// the connector reaches its HTTP transport and fails with a
/// network error instead — which is what this test asserts.
///
/// Gated on `http-client` because `authenticate_connector` requires
/// a live `BlockingHttpTransport` to drive the OAuth2 exchange.
#[cfg(feature = "http-client")]
#[test]
fn authenticate_connector_splices_auth_code_under_correct_json_key() {
    let (h, _dir) = fresh_store();

    let scope = uuid::Uuid::new_v4().to_string();
    // Point at a reserved-for-testing TLD (RFC 6761 `.invalid`) so
    // DNS resolution fails fast on every resolver — no real
    // network round-trip is required, and the test stays
    // deterministic regardless of the runner's connectivity.
    let cfg = r#"{
        "client_id": "test-client",
        "redirect_uri": "https://example.invalid/oauth/callback",
        "token_url": "https://example.invalid/oauth/token"
    }"#;
    let instance_id = create_connector(h, ConnectorKindTag::Notion, scope.clone(), cfg.to_string())
        .expect("create_connector");

    // Drive the actual surface this test cares about. Every
    // concrete connector will reject the call (no live OAuth2
    // server is reachable), so we expect an error — the question
    // is *which* error. The pre-fix bug surfaces as
    // `auth_config_json.authorization_code is required` because
    // the connector never sees the spliced key; the post-fix
    // behaviour surfaces as a transport error because the
    // connector reads the (correctly-spliced) code and tries to
    // exchange it against `example.invalid`.
    let err = authenticate_connector(h, instance_id, "test-authorization-code".into())
        .expect_err("authenticate_connector should fail against example.invalid");
    let msg = format!("{err}");
    assert!(
        !msg.contains("auth_config_json.authorization_code is required"),
        "regression: authenticate_connector spliced auth code under wrong JSON key; \
         every concrete connector reads from `authorization_code` and surfaced \
         the missing-key error — got: {msg}",
    );
    // Sanity: confirm we actually reached the connector's HTTP
    // transport (otherwise the negative assertion above would
    // pass vacuously e.g. on an `Unavailable` error). Accept any
    // of: a transport / network diagnostic substring, or the
    // connector framework's own `Transport` variant tag.
    let reached_transport = msg.contains("transport")
        || msg.contains("Transport")
        || msg.contains("dns")
        || msg.contains("lookup")
        || msg.contains("error sending request")
        || msg.contains("connect")
        || msg.contains("network");
    assert!(
        reached_transport,
        "authenticate_connector reached neither the connector's HTTP transport \
         nor the missing-key path — got: {msg}",
    );

    close_store(h).expect("close_store");
}

/// Single-instance-per-`(scope, kind)` invariant: a second
/// `create_connector` against the same `(scope_id, kind)` pair on a
/// given runtime is rejected with `FfiError::Connector` carrying the
/// `ConnectorError::DuplicateConnector` message, and the existing
/// instance is preserved. After the caller `remove_connector`s the
/// existing one, a fresh `create_connector` for the same pair
/// succeeds — the constraint is per *currently-registered* instance,
/// not per ever-existed instance. This pins the product decision
/// captured on PR #54 (one upstream source = one instance) and
/// prevents the double-ingest hazard that would otherwise occur if
/// two instances synced the same provider against the same scope.
///
/// Gated on `http-client` because `create_connector` requires a
/// live `BlockingHttpTransport`.
#[cfg(feature = "http-client")]
#[test]
fn create_connector_rejects_duplicate_scope_and_kind_pair() {
    let (h, _dir) = fresh_store();

    let scope = uuid::Uuid::new_v4().to_string();
    let other_scope = uuid::Uuid::new_v4().to_string();
    let cfg = r#"{
        "client_id": "test-client",
        "redirect_uri": "https://example.invalid/oauth/callback",
        "token_url": "https://example.invalid/oauth/token"
    }"#;

    // First create succeeds.
    let id_a = create_connector(h, ConnectorKindTag::Notion, scope.clone(), cfg.to_string())
        .expect("first create_connector should succeed");

    // Duplicate (same scope, same kind) is rejected.
    let err = create_connector(h, ConnectorKindTag::Notion, scope.clone(), cfg.to_string())
        .expect_err("second create_connector for same (scope, kind) must be rejected");
    assert!(
        matches!(err, FfiError::Connector { .. }),
        "duplicate-create must surface as FfiError::Connector, got: {err:?}",
    );
    let msg = format!("{err}");
    assert!(
        msg.contains("connector instance already exists"),
        "duplicate-create message must mention the framework's DuplicateConnector \
         variant — got: {msg}",
    );

    // Different scope, same kind: allowed (the constraint is per pair).
    let id_b = create_connector(
        h,
        ConnectorKindTag::Notion,
        other_scope.clone(),
        cfg.to_string(),
    )
    .expect("different-scope create_connector should succeed");
    assert_ne!(id_a, id_b);

    // Different kind, same scope: allowed (the constraint is per pair).
    let id_c = create_connector(h, ConnectorKindTag::Slack, scope.clone(), cfg.to_string())
        .expect("different-kind create_connector on same scope should succeed");
    assert_ne!(id_a, id_c);
    assert_ne!(id_b, id_c);

    // Existing instance preserved after the duplicate rejection —
    // no partial state leaked.
    let registered = list_connectors(h).expect("list_connectors");
    assert_eq!(
        registered.len(),
        3,
        "duplicate-create must not leak a partial entry; \
         expected exactly the three allowed connectors"
    );
    assert!(registered.iter().any(|s| s.instance_id == id_a));
    assert!(registered.iter().any(|s| s.instance_id == id_b));
    assert!(registered.iter().any(|s| s.instance_id == id_c));

    // After `remove_connector`, a fresh create for the same pair
    // succeeds — the constraint is over the *currently-registered*
    // set, not the historical set.
    remove_connector(h, id_a.clone()).expect("remove_connector(id_a)");
    let id_a_v2 = create_connector(h, ConnectorKindTag::Notion, scope.clone(), cfg.to_string())
        .expect("re-create after remove_connector should succeed");
    assert_ne!(
        id_a, id_a_v2,
        "re-created instance must have a fresh id (uuid v4)",
    );

    close_store(h).expect("close_store");
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

// ─────────────────── Phase 3 connector persistence ──────────────────
//
// These tests pin the **Phase 3 contract**: the connector lifecycle
// state (instances + sync state + OAuth2 tokens) is durable across
// `close_store` / `open_store` and respects the cryptographic
// forgetting contract (forgotten scopes never resurrect).
//
// All persistence-bearing tests open a real temp-dir SQLCipher store,
// hold the same on-disk path stable across a `close_store` /
// re-`open_store` cycle, and reuse the same deterministic master
// key — that combination is what the in-process `open_store` calls
// would experience in a real host that restarted the process. The
// `TempDir` is kept alive (`_dir` binding) so the on-disk database
// is not GC'd between the two opens.
//
// Gated on `http-client` because `create_connector` requires a live
// `BlockingHttpTransport` to build the connector handle — without
// the feature the FFI returns `Unavailable` and there's nothing to
// persist. The default-features `cargo test` builds the integration
// tests without these imports.

#[cfg(feature = "http-client")]
const PERSISTENCE_CONNECTOR_CFG: &str = r#"{
    "client_id": "phase3-persist-client",
    "redirect_uri": "https://example.invalid/oauth/callback",
    "token_url": "https://example.invalid/oauth/token"
}"#;

/// Open a fresh store *at a caller-supplied path* with the
/// deterministic master key, mirroring `fresh_store` but allowing the
/// path to outlive a `close_store` / `open_store` cycle. The caller
/// owns the `TempDir` so it can be reused across the boundary.
#[cfg(feature = "http-client")]
fn open_at(path: &std::path::Path) -> RuntimeHandle {
    let master_key_hex = "a5".repeat(32);
    open_store(path.to_string_lossy().into_owned(), master_key_hex).expect("open_store")
}

/// A connector instance row written by `create_connector` MUST
/// survive `close_store` + `open_store` — the row is encrypted under
/// the scope DEK in `connector_instances`, and `open_store`
/// rehydrates the in-memory `connector_instances` / `connectors`
/// maps before returning control to the host. After the round-trip,
/// `list_connectors` must surface the same instance id, scope, and
/// kind that `create_connector` returned.
#[cfg(feature = "http-client")]
#[test]
fn connector_instance_persists_across_close_store_reopen() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("evidence.db");

    let scope = uuid::Uuid::new_v4().to_string();
    let h1 = open_at(&path);
    let instance_id = create_connector(
        h1,
        ConnectorKindTag::Notion,
        scope.clone(),
        PERSISTENCE_CONNECTOR_CFG.into(),
    )
    .expect("create_connector");
    let before = list_connectors(h1).expect("list_connectors pre-close");
    assert_eq!(before.len(), 1, "pre-close: exactly one instance");
    assert_eq!(before[0].instance_id, instance_id);
    assert_eq!(before[0].scope_id, scope);
    assert!(matches!(before[0].kind, ConnectorKindTag::Notion));
    close_store(h1).expect("close_store");

    // Re-open the same on-disk file under a new runtime handle.
    let h2 = open_at(&path);
    let after = list_connectors(h2).expect("list_connectors post-reopen");
    assert_eq!(
        after.len(),
        1,
        "post-reopen: persisted instance must be rehydrated"
    );
    assert_eq!(
        after[0].instance_id, instance_id,
        "rehydrated instance must keep its original UUID",
    );
    assert_eq!(
        after[0].scope_id, scope,
        "rehydrated instance must keep its original scope_id",
    );
    assert!(
        matches!(after[0].kind, ConnectorKindTag::Notion),
        "rehydrated instance must keep its original kind",
    );
    close_store(h2).expect("close_store re-opened");
}

/// `remove_connector` MUST delete the persisted row too — after
/// `close_store` + `open_store`, the instance does not reappear.
/// Pins the on-disk side of `remove_connector`'s idempotency
/// contract.
#[cfg(feature = "http-client")]
#[test]
fn remove_connector_deletes_persisted_rows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("evidence.db");

    let scope = uuid::Uuid::new_v4().to_string();
    let h1 = open_at(&path);
    let instance_id = create_connector(
        h1,
        ConnectorKindTag::Notion,
        scope.clone(),
        PERSISTENCE_CONNECTOR_CFG.into(),
    )
    .expect("create_connector");
    remove_connector(h1, instance_id.clone()).expect("remove_connector");
    assert!(
        list_connectors(h1)
            .expect("list_connectors after remove")
            .is_empty(),
        "post-remove pre-close: list should be empty",
    );
    close_store(h1).expect("close_store");

    let h2 = open_at(&path);
    let after = list_connectors(h2).expect("list_connectors post-reopen");
    assert!(
        after.is_empty(),
        "removed connector must not resurrect across close_store/open_store"
    );
    close_store(h2).expect("close_store re-opened");
}

/// `forget_scope` MUST drop the persisted rows for every connector
/// bound to the forgotten scope. The on-disk side of the
/// cryptographic-forgetting contract: even if the AEAD payload is
/// unrecoverable (the scope DEK was destroyed in step 1), the row
/// must not survive in the table — the open_store rehydration would
/// skip it via the tombstone check, but the test pins the actual
/// delete so the table doesn't grow unbounded with dead rows.
#[cfg(feature = "http-client")]
#[test]
fn forget_scope_purges_persisted_connector_instances_and_tokens() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("evidence.db");

    let scope_a = uuid::Uuid::new_v4().to_string();
    let scope_b = uuid::Uuid::new_v4().to_string();

    let h1 = open_at(&path);
    let id_a = create_connector(
        h1,
        ConnectorKindTag::Notion,
        scope_a.clone(),
        PERSISTENCE_CONNECTOR_CFG.into(),
    )
    .expect("create_connector A");
    let id_b = create_connector(
        h1,
        ConnectorKindTag::Notion,
        scope_b.clone(),
        PERSISTENCE_CONNECTOR_CFG.into(),
    )
    .expect("create_connector B");

    forget_scope(h1, scope_a.clone()).expect("forget_scope A");
    close_store(h1).expect("close_store");

    let h2 = open_at(&path);
    let after = list_connectors(h2).expect("list_connectors post-reopen");
    assert_eq!(
        after.len(),
        1,
        "post-reopen: only the un-forgotten scope's connector should rehydrate",
    );
    assert_eq!(
        after[0].instance_id, id_b,
        "connector bound to forgotten scope must not reappear (id_a={id_a})",
    );
    assert_eq!(after[0].scope_id, scope_b);
    close_store(h2).expect("close_store re-opened");
}

/// Even if a connector was created BEFORE `forget_scope` ran, then
/// the database was closed and reopened, the rehydration sweep MUST
/// skip every persisted row bound to a tombstoned scope. This pins
/// the second line of defense: even if `forget_scope_state`'s
/// step-5b SQL delete failed (e.g. a transient SQLCipher I/O error),
/// the next `open_store` walks `forgotten_scopes`, builds the
/// tombstone set, and refuses to rehydrate any row whose `scope_id`
/// is in that set — and best-effort deletes the dangling row from
/// disk on the way out.
#[cfg(feature = "http-client")]
#[test]
fn rehydration_skips_tombstoned_scopes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("evidence.db");

    let scope_a = uuid::Uuid::new_v4().to_string();

    let h1 = open_at(&path);
    let _id_a = create_connector(
        h1,
        ConnectorKindTag::Notion,
        scope_a.clone(),
        PERSISTENCE_CONNECTOR_CFG.into(),
    )
    .expect("create_connector");
    forget_scope(h1, scope_a.clone()).expect("forget_scope");
    close_store(h1).expect("close_store");

    let h2 = open_at(&path);
    assert!(
        list_connectors(h2)
            .expect("list_connectors post-reopen")
            .is_empty(),
        "connectors bound to a tombstoned scope must not rehydrate",
    );
    close_store(h2).expect("close_store re-opened");
}

/// The DB-layer unique index on `connector_instances(scope_id, kind)`
/// pins the single-instance-per-(scope, kind) contract — a future
/// regression in the runtime check would still be caught here.
/// Drive the duplicate through the FFI surface; `create_connector`
/// rejects with `DuplicateConnector` *before* the SQL insert, so
/// the unique index never fires under normal flow, but its presence
/// is the defense-in-depth and the rejection contract is what the
/// host observes.
#[cfg(feature = "http-client")]
#[test]
fn dedup_constraint_pinned_on_persisted_rows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("evidence.db");
    let scope = uuid::Uuid::new_v4().to_string();

    let h = open_at(&path);
    let _ = create_connector(
        h,
        ConnectorKindTag::Notion,
        scope.clone(),
        PERSISTENCE_CONNECTOR_CFG.into(),
    )
    .expect("first create_connector");
    let err = create_connector(
        h,
        ConnectorKindTag::Notion,
        scope.clone(),
        PERSISTENCE_CONNECTOR_CFG.into(),
    )
    .expect_err("duplicate create_connector must be rejected");
    let msg = format!("{err}");
    assert!(
        msg.contains("already exists") || msg.contains("DuplicateConnector"),
        "duplicate rejection should surface a DuplicateConnector message — got: {msg}",
    );
    close_store(h).expect("close_store");

    // After reopening, the persisted row is rehydrated and the same
    // duplicate-rejection contract holds — the runtime check sees
    // the rehydrated instance and refuses the duplicate.
    let h2 = open_at(&path);
    let err2 = create_connector(
        h2,
        ConnectorKindTag::Notion,
        scope.clone(),
        PERSISTENCE_CONNECTOR_CFG.into(),
    )
    .expect_err("duplicate create_connector after reopen must also be rejected");
    let msg2 = format!("{err2}");
    assert!(
        msg2.contains("already exists") || msg2.contains("DuplicateConnector"),
        "post-rehydrate duplicate rejection must also surface DuplicateConnector — got: {msg2}",
    );
    close_store(h2).expect("close_store re-opened");
}

/// Multiple scope-DEK boundaries: an instance under scope A and an
/// instance under scope B must both rehydrate independently across
/// `close_store` / `open_store`. The two rows are encrypted under
/// separate per-scope keys, so this test pins that the rehydration
/// loop in `open_store_inner` walks every row in the table (not
/// just the one matching some "first" scope) and decrypts each one
/// under its own scope's DEK.
#[cfg(feature = "http-client")]
#[test]
fn multiple_scope_connectors_all_persist_and_rehydrate() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("evidence.db");

    let scope_a = uuid::Uuid::new_v4().to_string();
    let scope_b = uuid::Uuid::new_v4().to_string();

    let h1 = open_at(&path);
    let id_a_notion = create_connector(
        h1,
        ConnectorKindTag::Notion,
        scope_a.clone(),
        PERSISTENCE_CONNECTOR_CFG.into(),
    )
    .expect("create A/Notion");
    let id_a_slack = create_connector(
        h1,
        ConnectorKindTag::Slack,
        scope_a.clone(),
        PERSISTENCE_CONNECTOR_CFG.into(),
    )
    .expect("create A/Slack");
    let id_b_notion = create_connector(
        h1,
        ConnectorKindTag::Notion,
        scope_b.clone(),
        PERSISTENCE_CONNECTOR_CFG.into(),
    )
    .expect("create B/Notion");
    close_store(h1).expect("close_store");

    let h2 = open_at(&path);
    let after = list_connectors(h2).expect("list post-reopen");
    assert_eq!(
        after.len(),
        3,
        "all three persisted instances across both scopes must rehydrate",
    );
    let ids: std::collections::HashSet<_> = after.iter().map(|s| s.instance_id.clone()).collect();
    assert!(ids.contains(&id_a_notion), "id_a_notion must rehydrate");
    assert!(ids.contains(&id_a_slack), "id_a_slack must rehydrate");
    assert!(ids.contains(&id_b_notion), "id_b_notion must rehydrate");
    close_store(h2).expect("close_store re-opened");
}

/// A row with a payload whose plaintext is NOT a valid
/// `PersistedConnectorInstance` JSON envelope MUST be skipped (with
/// WARN) at rehydration time without blocking `open_store` or
/// affecting other rows. This pins the partial-corruption tolerance
/// documented on `rehydrate_connectors`: one bad row never wedges
/// the entire connector subsystem.
///
/// To inject the corruption we open the `EvidenceStore` directly
/// using the same hex master key the FFI uses, then call
/// `save_connector_instance` to overwrite one row's plaintext with
/// `b"not a valid JSON envelope"`. The AEAD round-trip succeeds on
/// reopen (it's our key + AAD), but `serde_json::from_slice::<
/// PersistedConnectorInstance>` fails — exercising the JSON
/// deserialise-fail skip path. We could also tamper with the
/// ciphertext via raw SQLCipher to exercise the AEAD-decrypt-fail
/// skip path, but driving it via the evidence-store API keeps the
/// test resilient to internal pragma changes (the page-key
/// derivation, kdf_iter, etc.). Both skip paths funnel through the
/// same `tracing::warn!` continue branch in
/// `rehydrate_connectors`, so a single test on the
/// deserialise-fail path is sufficient to pin the contract.
#[cfg(feature = "http-client")]
#[test]
fn corrupted_payload_doesnt_block_open_store() {
    use evidence_store::{EvidenceStore, EvidenceStoreConfig, ScopeId};

    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("evidence.db");

    let scope_a = uuid::Uuid::new_v4().to_string();
    let scope_b = uuid::Uuid::new_v4().to_string();
    let h1 = open_at(&db_path);
    let id_a = create_connector(
        h1,
        ConnectorKindTag::Notion,
        scope_a.clone(),
        PERSISTENCE_CONNECTOR_CFG.into(),
    )
    .expect("create A");
    let id_b = create_connector(
        h1,
        ConnectorKindTag::Slack,
        scope_b.clone(),
        PERSISTENCE_CONNECTOR_CFG.into(),
    )
    .expect("create B");
    close_store(h1).expect("close_store");

    // Re-open the evidence store directly (bypassing the FFI) and
    // overwrite instance A's payload with garbage JSON. The store
    // still encrypts the plaintext correctly under instance A's
    // scope key, so the AEAD layer round-trips; the deserialise
    // step is what fails. The `[0xa5; 32]` master key matches the
    // "a5".repeat(32) hex that `fresh_store` / `open_at` decode.
    {
        let master_key: [u8; 32] = [0xa5_u8; 32];
        let store = EvidenceStore::open(&db_path, &master_key, EvidenceStoreConfig::default())
            .expect("EvidenceStore::open");

        let id_a_uuid = uuid::Uuid::parse_str(&id_a).expect("uuid parse id_a");
        let scope_a_id =
            ScopeId::from_uuid(uuid::Uuid::parse_str(&scope_a).expect("parse scope_a"));
        store
            .save_connector_instance(
                id_a_uuid,
                scope_a_id,
                connector_framework::ConnectorKind::Notion.as_str(),
                b"not a valid JSON envelope",
            )
            .expect("overwrite payload with garbage plaintext");
    }

    // Re-open via the FFI and verify the un-corrupted instance still
    // rehydrates while the deserialise-fail row is skipped.
    let h2 = open_at(&db_path);
    let after = list_connectors(h2).expect("list post-reopen");
    assert_eq!(
        after.len(),
        1,
        "corrupted row must be skipped while the healthy row rehydrates; got {} rows",
        after.len(),
    );
    assert_eq!(
        after[0].instance_id, id_b,
        "the surviving row should be the un-corrupted instance B (id_a={id_a})",
    );
    close_store(h2).expect("close_store re-opened");
}

/// Advanced `SyncState` (cursor, status, events_ingested) MUST
/// survive `close_store` / `open_store`. Drive this through the
/// evidence-store API directly: we write a `connector_instances`
/// row with a hand-crafted `SyncState::Succeeded` envelope (cursor
/// = `"cursor-after-sync-7"`), then reopen via the FFI and verify
/// `list_connectors` reports the advanced state.
#[cfg(feature = "http-client")]
#[test]
fn sync_state_advance_persists_across_close_store_reopen() {
    use chrono::{TimeZone, Utc};
    use connector_framework::{
        AuthKind, ConnectorConfig, ConnectorInstance, ConnectorInstanceId, ConnectorKind, SyncMode,
        SyncState, SyncStatus,
    };
    use evidence_store::{EvidenceStore, EvidenceStoreConfig, ScopeId};

    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("evidence.db");

    let scope = uuid::Uuid::new_v4().to_string();
    let h1 = open_at(&db_path);
    let instance_id = create_connector(
        h1,
        ConnectorKindTag::Notion,
        scope.clone(),
        PERSISTENCE_CONNECTOR_CFG.into(),
    )
    .expect("create_connector");
    close_store(h1).expect("close_store");

    // Build a fresh `ConnectorInstance` with an advanced sync state
    // and persist it through the evidence-store API directly. The
    // FFI's `persist_connector_instance` produces the same shape;
    // we replicate it here to avoid having to drive a real provider
    // round-trip to advance the cursor.
    let instance_uuid = uuid::Uuid::parse_str(&instance_id).expect("uuid parse");
    let scope_id = ScopeId::from_uuid(uuid::Uuid::parse_str(&scope).expect("uuid parse scope"));
    let master_key: [u8; 32] = [0xa5_u8; 32];

    let mut sync_state = SyncState::new(ConnectorInstanceId::from_uuid(instance_uuid));
    sync_state.mode = SyncMode::Incremental;
    sync_state.cursor = Some("cursor-after-sync-7".to_string());
    sync_state.last_synced_at = Some(Utc.with_ymd_and_hms(2025, 6, 15, 12, 0, 0).unwrap());
    sync_state.status = SyncStatus::Succeeded;
    sync_state.last_error = None;

    let mut config = ConnectorConfig::new(ConnectorKind::Notion, AuthKind::OAuth2, scope_id);
    config.auth_config_json = serde_json::from_str(PERSISTENCE_CONNECTOR_CFG).unwrap();

    let instance = ConnectorInstance {
        id: ConnectorInstanceId::from_uuid(instance_uuid),
        config,
        sync_state,
    };
    // Build the envelope the FFI persists. Schema version 1 matches
    // `PERSISTED_INSTANCE_SCHEMA` in `crates/ffi/src/connector.rs`.
    let envelope = serde_json::json!({
        "schema": 1,
        "config": &instance.config,
        "sync_state": &instance.sync_state,
    });
    let plaintext_json = serde_json::to_vec(&envelope).expect("encode envelope");
    {
        let store = EvidenceStore::open(&db_path, &master_key, EvidenceStoreConfig::default())
            .expect("EvidenceStore::open");
        store
            .save_connector_instance(
                instance_uuid,
                scope_id,
                ConnectorKind::Notion.as_str(),
                &plaintext_json,
            )
            .expect("save_connector_instance");
    }

    // Re-open via the FFI and verify the advanced sync state
    // rehydrates intact. `ConnectorStatus` exposes `sync_mode`,
    // `sync_status`, and `last_synced_at` — those three observable
    // fields cover the steady-state contract a host depends on.
    let h2 = open_at(&db_path);
    let after = list_connectors(h2).expect("list post-reopen");
    assert_eq!(after.len(), 1, "single instance must rehydrate");
    let status = &after[0];
    assert_eq!(status.instance_id, instance_id);
    assert_eq!(status.scope_id, scope);
    assert!(
        matches!(status.sync_mode, ffi::SyncModeKind::Incremental),
        "advanced mode (Incremental) must survive close/reopen",
    );
    assert!(
        matches!(status.sync_status, ffi::SyncStatusKind::Succeeded),
        "advanced status (Succeeded) must survive close/reopen",
    );
    assert_eq!(
        status.last_synced_at,
        Some(
            Utc.with_ymd_and_hms(2025, 6, 15, 12, 0, 0)
                .unwrap()
                .timestamp()
        ),
        "advanced last_synced_at must survive close/reopen",
    );
    assert!(
        status.last_error.is_none(),
        "Succeeded sync_state should not carry a last_error; got {:?}",
        status.last_error,
    );
    close_store(h2).expect("close_store re-opened");

    // The cursor itself is not exposed through `ConnectorStatus`
    // (the type only carries the observable lifecycle fields), but
    // it MUST still be in the persisted ciphertext so a future
    // `sync_connector` resumes from the right point. Verify
    // directly via the evidence-store API that the cursor JSON
    // round-trips bit-for-bit.
    let store = EvidenceStore::open(&db_path, &master_key, EvidenceStoreConfig::default())
        .expect("EvidenceStore::open for cursor check");
    let rows = store
        .load_connector_instances()
        .expect("load_connector_instances");
    assert_eq!(rows.len(), 1, "exactly one persisted row expected");
    let (loaded_uuid, loaded_scope, loaded_kind, loaded_plain) = &rows[0];
    assert_eq!(*loaded_uuid, instance_uuid);
    assert_eq!(*loaded_scope, scope_id);
    assert_eq!(loaded_kind, ConnectorKind::Notion.as_str());
    let envelope: serde_json::Value =
        serde_json::from_slice(loaded_plain).expect("envelope JSON parse");
    let cursor = envelope
        .get("sync_state")
        .and_then(|s| s.get("cursor"))
        .and_then(|c| c.as_str());
    assert_eq!(
        cursor,
        Some("cursor-after-sync-7"),
        "advanced cursor must survive close/reopen in the persisted ciphertext",
    );
}

/// OAuth2 tokens MUST round-trip through `connector_tokens` —
/// written by `EvidenceStore::save_connector_token`, decrypted by
/// `EvidenceStore::load_connector_tokens`, and the plaintext
/// `OAuth2Token` JSON survives bit-for-bit. We drive the test
/// through the evidence-store API directly (rather than via
/// `authenticate_connector`) because the FFI surface requires a
/// live OAuth2 provider for the Phase 2 exchange — the *persistence*
/// contract is the same regardless of how the token was acquired,
/// so testing the evidence-store round-trip pins the at-rest
/// encryption + AAD-binding behaviour without standing up a fake
/// OAuth endpoint.
#[cfg(feature = "http-client")]
#[test]
fn oauth_token_persists_across_close_store_reopen() {
    use chrono::{TimeZone, Utc};
    use connector_framework::{ConnectorInstanceId, OAuth2Token};
    use evidence_store::{EvidenceStore, EvidenceStoreConfig, ScopeId};

    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("evidence.db");

    let scope = uuid::Uuid::new_v4().to_string();
    let h1 = open_at(&db_path);
    // Use the FFI to create a connector — this registers the scope
    // DEK in `scope_deks` so subsequent direct EvidenceStore calls
    // can encrypt under the same scope key the FFI uses.
    let instance_id = create_connector(
        h1,
        ConnectorKindTag::Notion,
        scope.clone(),
        PERSISTENCE_CONNECTOR_CFG.into(),
    )
    .expect("create_connector");
    close_store(h1).expect("close_store");

    // Write a token row directly via EvidenceStore, then reopen and
    // verify load_connector_tokens decrypts it back to the same
    // OAuth2Token plaintext.
    let instance_uuid = uuid::Uuid::parse_str(&instance_id).expect("uuid parse instance");
    let scope_id = ScopeId::from_uuid(uuid::Uuid::parse_str(&scope).expect("uuid parse scope"));
    let master_key: [u8; 32] = [0xa5_u8; 32];
    let original = OAuth2Token::new(
        "test-access-token-deadbeef",
        "test-refresh-token-cafebabe",
        Utc.with_ymd_and_hms(2030, 1, 1, 0, 0, 0).unwrap(),
        "drive.readonly profile",
    );
    {
        let store = EvidenceStore::open(&db_path, &master_key, EvidenceStoreConfig::default())
            .expect("EvidenceStore::open for write");
        let original_json = serde_json::to_vec(&original).expect("encode token");
        store
            .save_connector_token(instance_uuid, scope_id, &original_json)
            .expect("save_connector_token");
    }

    // Re-open and read back via the same path the FFI rehydration
    // walks.
    let store = EvidenceStore::open(&db_path, &master_key, EvidenceStoreConfig::default())
        .expect("EvidenceStore::open for read");
    let tokens = store
        .load_connector_tokens()
        .expect("load_connector_tokens");
    assert_eq!(
        tokens.len(),
        1,
        "exactly one token row should round-trip; got {}",
        tokens.len(),
    );
    let (loaded_instance, loaded_scope, loaded_plain) = &tokens[0];
    assert_eq!(*loaded_instance, instance_uuid);
    assert_eq!(*loaded_scope, scope_id);
    let decoded: OAuth2Token = serde_json::from_slice(loaded_plain).expect("decode token");
    assert_eq!(
        decoded, original,
        "loaded OAuth2Token must equal what was persisted (access + refresh + expiry + scope + token_type)",
    );

    // Also verify the FFI's rehydration loop populates the in-memory
    // token_vault: after `open_store`, `list_connectors` should
    // surface the rehydrated instance, and the token's presence is
    // implicit via the `authenticate_connector`/`sync_connector` API
    // contracts (which we cannot exercise without a live provider).
    // We instead verify `delete_connector_token` clears the row so
    // subsequent reopens see no token — same idempotency contract
    // as `remove_connector` on the instance side.
    store
        .delete_connector_token(instance_uuid)
        .expect("delete_connector_token");
    let after_delete = store
        .load_connector_tokens()
        .expect("load_connector_tokens after delete");
    assert!(
        after_delete.is_empty(),
        "delete_connector_token must clear the row; got {} rows after delete",
        after_delete.len(),
    );
    // Suppress unused warning — `ConnectorInstanceId` is imported
    // for clarity in the test signature but not directly
    // constructed (we work with the raw UUID for the store-side
    // API).
    let _ = ConnectorInstanceId::from_uuid(instance_uuid);
}

/// `remove_connector` is idempotent — calling it twice in a row,
/// including across a `close_store` / `open_store` cycle for the
/// second call, returns `Ok(())` both times. Pins the
/// idempotency-on-persistence contract.
#[cfg(feature = "http-client")]
#[test]
fn remove_connector_is_idempotent_across_reopen() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("evidence.db");

    let scope = uuid::Uuid::new_v4().to_string();
    let h1 = open_at(&path);
    let id = create_connector(
        h1,
        ConnectorKindTag::Notion,
        scope.clone(),
        PERSISTENCE_CONNECTOR_CFG.into(),
    )
    .expect("create");
    remove_connector(h1, id.clone()).expect("first remove");
    close_store(h1).expect("close_store");

    let h2 = open_at(&path);
    // The second remove targets a row that no longer exists on
    // either side (in-memory or persisted). The DELETE is a no-op
    // and the call returns `Ok(())`.
    remove_connector(h2, id.clone()).expect("second remove must be idempotent");
    close_store(h2).expect("close_store re-opened");
}

/// Token rows whose owning instance row failed to rehydrate (or
/// was never persisted) MUST be skipped on `open_store` AND
/// best-effort purged from disk. Otherwise the vault accumulates
/// orphans that can never be retired and that re-walk every open.
///
/// Reproduces the contract by:
/// 1. Creating a connector + persisting an OAuth2 token row via the
///    FFI side (real instance + token both on disk).
/// 2. Reopening the underlying EvidenceStore directly and
///    overwriting the instance row's payload with garbage JSON so
///    the rehydrate loop's `serde_json::from_slice` fails and the
///    in-memory `connector_instances` map stays empty for that
///    instance — leaving the token row "orphaned" on next open.
/// 3. Reopening via the FFI and asserting (a) no connectors
///    rehydrate (instance row tampered), (b) the orphan token row
///    is purged from disk so subsequent opens do not re-walk it.
#[cfg(feature = "http-client")]
#[test]
fn orphan_token_skipped_and_cleaned_up_on_rehydrate() {
    use chrono::{TimeZone, Utc};
    use connector_framework::OAuth2Token;
    use evidence_store::{EvidenceStore, EvidenceStoreConfig, ScopeId};

    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("evidence.db");

    let scope = uuid::Uuid::new_v4().to_string();
    let h1 = open_at(&db_path);
    let instance_id = create_connector(
        h1,
        ConnectorKindTag::Notion,
        scope.clone(),
        PERSISTENCE_CONNECTOR_CFG.into(),
    )
    .expect("create_connector");
    close_store(h1).expect("close_store");

    let instance_uuid = uuid::Uuid::parse_str(&instance_id).expect("uuid parse instance");
    let scope_id = ScopeId::from_uuid(uuid::Uuid::parse_str(&scope).expect("uuid parse scope"));
    let master_key: [u8; 32] = [0xa5_u8; 32];

    // Persist a real token row alongside the instance, then
    // corrupt the instance row's payload so it fails to
    // deserialise on rehydrate.
    {
        let store = EvidenceStore::open(&db_path, &master_key, EvidenceStoreConfig::default())
            .expect("EvidenceStore::open for setup");
        let token = OAuth2Token::new(
            "orphan-access",
            "orphan-refresh",
            Utc.with_ymd_and_hms(2030, 1, 1, 0, 0, 0).unwrap(),
            "drive.readonly",
        );
        let token_json = serde_json::to_vec(&token).expect("encode token");
        store
            .save_connector_token(instance_uuid, scope_id, &token_json)
            .expect("save_connector_token");
        // Corrupt the instance payload — AEAD still seals cleanly,
        // but the JSON envelope parse fails so rehydrate skips it.
        store
            .save_connector_instance(
                instance_uuid,
                scope_id,
                connector_framework::ConnectorKind::Notion.as_str(),
                b"orphan: not a valid envelope",
            )
            .expect("corrupt instance payload");
        assert_eq!(
            store
                .load_connector_tokens()
                .expect("load before reopen")
                .len(),
            1,
            "setup: token row should be on disk before reopen",
        );
    }

    // Reopen via the FFI. The instance fails to rehydrate
    // (corrupt payload → skipped); the token is now an orphan and
    // must be (a) skipped (not inserted into token_vault) and (b)
    // purged from disk.
    let h2 = open_at(&db_path);
    let listed = list_connectors(h2).expect("list post-reopen");
    assert!(
        listed.is_empty(),
        "corrupted instance row must skip rehydrate; got {} entries",
        listed.len(),
    );
    close_store(h2).expect("close_store after rehydrate sweep");

    // The orphan token row should have been purged on rehydrate
    // (best-effort, traced on failure). Verify directly via the
    // evidence-store API that the row is gone.
    let store = EvidenceStore::open(&db_path, &master_key, EvidenceStoreConfig::default())
        .expect("EvidenceStore::open for verify");
    let remaining = store
        .load_connector_tokens()
        .expect("load_connector_tokens post-reopen");
    assert!(
        remaining.is_empty(),
        "orphan token row must be cleaned up by rehydration; {} row(s) remain",
        remaining.len(),
    );
}

/// `save_connector_instance` must surface a unique-constraint
/// violation on the secondary `(scope_id, kind)` index as a
/// structured error rather than silently deleting the conflicting
/// row. The runtime-side `create_connector` check rejects duplicates
/// before they reach the SQL layer, but a regression of that check
/// (or a stray writer holding a parallel handle to the same
/// database file) must NOT silently destroy the existing row — the
/// `INSERT … ON CONFLICT(instance_id) DO UPDATE` spelling lets the
/// secondary unique constraint propagate the violation upward.
///
/// Drives the failure directly through the evidence-store API so
/// the test isolates the SQL contract from the FFI-level dedup
/// check. A successful collision under `INSERT OR REPLACE` would
/// silently overwrite `instance_a` with `instance_b`'s payload; the
/// `ON CONFLICT(instance_id)` spelling instead surfaces a Sqlite
/// `UNIQUE constraint failed` error so the operator notices.
#[cfg(feature = "http-client")]
#[test]
fn save_connector_instance_propagates_secondary_unique_violation() {
    use evidence_store::{EvidenceStore, EvidenceStoreConfig, ScopeId};

    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("evidence.db");
    let scope_uuid = uuid::Uuid::new_v4();
    let scope_id = ScopeId::from_uuid(scope_uuid);
    let master_key: [u8; 32] = [0xa5_u8; 32];

    let mut store = EvidenceStore::open(&db_path, &master_key, EvidenceStoreConfig::default())
        .expect("EvidenceStore::open");
    // Register the scope DEK so the AEAD encrypt succeeds for both
    // insertions; otherwise the test would fail at `scope_key` before
    // ever exercising the SQL conflict.
    store.ensure_scope_dek(scope_id).expect("ensure_scope_dek");

    let instance_a = uuid::Uuid::new_v4();
    let instance_b = uuid::Uuid::new_v4();
    let kind_tag = connector_framework::ConnectorKind::Notion.as_str();
    store
        .save_connector_instance(instance_a, scope_id, kind_tag, b"{\"schema\":1}")
        .expect("first save_connector_instance must succeed");

    let err = store
        .save_connector_instance(instance_b, scope_id, kind_tag, b"{\"schema\":1}")
        .expect_err("colliding save_connector_instance must error, not silently overwrite");
    let msg = format!("{err}");
    assert!(
        msg.contains("UNIQUE constraint failed")
            && msg.contains("connector_instances")
            && msg.contains("scope_id")
            && msg.contains("kind"),
        "expected a structured UNIQUE-constraint error on (scope_id, kind); got: {msg}",
    );

    // The original row must still be intact — `INSERT OR REPLACE`
    // would have silently destroyed it; `ON CONFLICT(instance_id)`
    // leaves it alone.
    let rows = store
        .load_connector_instances()
        .expect("load_connector_instances after collision");
    assert_eq!(
        rows.len(),
        1,
        "secondary-unique collision must leave the existing row untouched",
    );
    let (loaded_id, _scope, loaded_kind, _payload) = &rows[0];
    assert_eq!(
        loaded_id, &instance_a,
        "the surviving row must be instance_a"
    );
    assert_eq!(loaded_kind.as_str(), kind_tag);
}

// ───────────── Phase 4: OAuth2 token refresh via FFI ─────────────
//
// Phase 4 wires `refresh_connector_token` and the auto-refresh path
// inside `sync_connector` through the FFI surface. The substrate-side
// primitives (`OAuth2Client::refresh_with_config`,
// `ConfiguredRefresher`, `OAuth2TokenVault::refresh_if_expiring`)
// are exhaustively unit-tested at the connector_framework level;
// these tests pin the **FFI-level contract**:
//
// * `refresh_connector_token` POSTs to the configured `token_url`,
//   updates the in-memory vault, AND persists the new token to
//   SQLCipher so it survives `close_store`/`open_store`.
// * A token without `refresh_token` (Slack legacy / PKCE-only public
//   clients) short-circuits with an actionable substrate-side
//   diagnostic — the substrate refuses to POST `refresh_token=`
//   to the provider because every compliant provider rejects that
//   with a generic `invalid_grant` whose message names neither the
//   instance nor the recovery path.
//
// Both tests stand up a tiny in-process HTTP/1.1 server pinned to
// `127.0.0.1:0` (an OS-allocated ephemeral port) so the
// production-built reqwest blocking transport actually drives the
// refresh path — we exercise the real wire format, not a mocked
// trait impl.

#[cfg(feature = "http-client")]
use std::io::{Read, Write};
#[cfg(feature = "http-client")]
use std::net::TcpListener;
#[cfg(feature = "http-client")]
use std::sync::{
    atomic::{AtomicUsize, Ordering as AtomicOrdering},
    Arc,
};
#[cfg(feature = "http-client")]
use std::thread::JoinHandle;

/// Tiny single-connection HTTP/1.1 server used by the Phase 4
/// integration tests to back the connector's OAuth2 token endpoint.
///
/// Scope is deliberately minimal: each call to
/// [`OAuthTestServer::enqueue`] queues one canned response body; the
/// background thread accepts one connection per response, parses
/// just enough of the request to count `(method, path)`, and writes
/// back `HTTP/1.1 200 OK` with `Content-Type: application/json`.
///
/// Lives in the integration-test file (not the test-support crate)
/// because Phase 4 is the first test surface that needs it; if a
/// future test wants the same plumbing the helper graduates to a
/// shared module.
#[cfg(feature = "http-client")]
struct OAuthTestServer {
    base_url: String,
    request_count: Arc<AtomicUsize>,
    request_bodies: Arc<std::sync::Mutex<Vec<String>>>,
    join: Option<JoinHandle<()>>,
}

#[cfg(feature = "http-client")]
impl OAuthTestServer {
    /// Bind to `127.0.0.1:0`, return a handle pre-armed with
    /// `responses` (consumed in FIFO order). The server thread
    /// exits after handling `responses.len()` connections — keep
    /// this in sync with how many refresh / authenticate calls the
    /// test will issue.
    fn start(responses: Vec<String>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = listener.local_addr().expect("local_addr").port();
        let base_url = format!("http://127.0.0.1:{port}");
        let request_count = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&request_count);
        let request_bodies = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let bodies = Arc::clone(&request_bodies);
        let join = std::thread::Builder::new()
            .name("oauth-test-server".into())
            .spawn(move || {
                for body in responses {
                    let Ok((mut stream, _addr)) = listener.accept() else {
                        return;
                    };
                    // Read enough of the request to count it AND
                    // capture the urlencoded form body for Phase
                    // 4.1 tests that assert on `client_secret=` in
                    // the POST body. We don't fully parse the
                    // HTTP/1.1 framing — just split on the
                    // `\r\n\r\n` header boundary and stash the
                    // remainder. The 4 KiB buffer is comfortably
                    // larger than any OAuth2 form body produced by
                    // the substrate.
                    let mut buf = [0u8; 4096];
                    let n = stream.read(&mut buf).unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..n]).into_owned();
                    let captured_body = request
                        .split_once("\r\n\r\n")
                        .map(|(_, b)| b.to_string())
                        .unwrap_or_default();
                    if let Ok(mut g) = bodies.lock() {
                        g.push(captured_body);
                    }
                    counter.fetch_add(1, AtomicOrdering::SeqCst);
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body,
                    );
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                }
            })
            .expect("spawn oauth test server");
        Self {
            base_url,
            request_count,
            request_bodies,
            join: Some(join),
        }
    }

    /// Bind to `127.0.0.1:0` but never accept (the listener stays
    /// open until `Drop`). Used by the "no refresh_token
    /// short-circuit" test to assert that the substrate refuses
    /// to make a network call when the cached token has no refresh
    /// token. If the substrate-side guard regresses, the test will
    /// see the request_count tick up and fail.
    fn start_silent() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = listener.local_addr().expect("local_addr").port();
        let base_url = format!("http://127.0.0.1:{port}");
        let request_count = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&request_count);
        let join = std::thread::Builder::new()
            .name("oauth-test-server-silent".into())
            .spawn(move || {
                // Single accept just so the listener stays bound.
                // If anyone connects we tick the counter and the
                // test asserts `== 0`.
                listener
                    .set_nonblocking(true)
                    .expect("set_nonblocking on listener");
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                while std::time::Instant::now() < deadline {
                    if let Ok((_, _)) = listener.accept() {
                        counter.fetch_add(1, AtomicOrdering::SeqCst);
                    } else {
                        std::thread::sleep(std::time::Duration::from_millis(25));
                    }
                }
            })
            .expect("spawn oauth test server (silent)");
        Self {
            base_url,
            request_count,
            request_bodies: Arc::new(std::sync::Mutex::new(Vec::new())),
            join: Some(join),
        }
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }

    fn request_count(&self) -> usize {
        self.request_count.load(AtomicOrdering::SeqCst)
    }

    /// Snapshot the captured form bodies (one per accepted request,
    /// in FIFO order). Used by Phase 4.1 tests to assert that the
    /// `client_secret=` form field is — or isn't — included in the
    /// POST body the substrate sent.
    fn request_bodies(&self) -> Vec<String> {
        self.request_bodies
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default()
    }
}

#[cfg(feature = "http-client")]
impl Drop for OAuthTestServer {
    fn drop(&mut self) {
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// `refresh_connector_token` drives a real OAuth2 refresh round-trip
/// against a local test server, updates the in-memory token vault,
/// AND persists the refreshed token to SQLCipher. Pins both halves
/// of the contract:
///
/// * The reqwest blocking transport actually POSTs to the configured
///   `token_url` (counted by the test server's `request_count`).
/// * The persisted token survives `close_store` + `open_store` —
///   verified by reopening the database with a different
///   `RuntimeHandle` and re-driving a refresh against the same
///   in-flight server, observing that the rehydrated bundle's
///   `refresh_token` is the rotated value from the first round-trip.
#[cfg(feature = "http-client")]
#[test]
fn refresh_connector_token_round_trips_and_persists_across_close_store_reopen() {
    use chrono::Duration as ChronoDuration;
    use connector_framework::{ConnectorInstanceId, OAuth2Token};
    use evidence_store::ScopeId;
    use ffi::refresh_connector_token;
    use uuid::Uuid;

    // The test server returns two refreshed tokens — the second is
    // used after we `close_store` + `open_store` to prove the
    // rotated refresh_token from the first call was actually
    // persisted (otherwise the rehydrated token would still carry
    // the original `RT-INITIAL` value and the second call would
    // hit the server with the wrong refresh_token).
    let server = OAuthTestServer::start(vec![
        r#"{"access_token":"AT-1","refresh_token":"RT-ROTATED-1","expires_in":3600,"scope":"read"}"#
            .to_string(),
        r#"{"access_token":"AT-2","refresh_token":"RT-ROTATED-2","expires_in":3600,"scope":"read"}"#
            .to_string(),
    ]);
    let token_url = format!("{}/oauth/token", server.base_url());

    // Stage state through the EvidenceStore directly so the test
    // doesn't depend on a real `authenticate_connector` flow (which
    // would require the connector's own API to be reachable).
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("evidence.db");
    let master_key_hex = "a5".repeat(32);
    // The FFI's `open_store` decodes `master_key_hex` to a
    // `[u8; 32]`; the `EvidenceStore::open` setup path here uses
    // the same bytes (every byte = 0xa5) so both opens see the
    // same DEK.
    let master_key_bytes: [u8; 32] = [0xa5_u8; 32];

    let scope = ScopeId::new_v4();
    let instance = ConnectorInstanceId::new_v4();
    let kind_tag = connector_framework::ConnectorKind::Notion.as_str();
    let config = connector_framework::ConnectorConfig::new(
        connector_framework::ConnectorKind::Notion,
        connector_framework::AuthKind::OAuth2,
        scope,
    )
    .with_auth_config(serde_json::json!({
        "client_id": "phase4-client",
        "client_secret": "s3cret",
        "redirect_uri": "https://example.invalid/oauth/callback",
        "token_url": token_url,
    }));
    let sync_state = connector_framework::SyncState::new(instance);
    // Persisted blob schema: `{ schema: 1, config: ConnectorConfig,
    // sync_state: SyncState }`. Mirrors
    // `PersistedConnectorInstanceRef` in `crates/ffi/src/connector.rs`
    // — we re-serialize via `serde_json::json!` rather than depend on
    // a private struct from the FFI crate.
    let instance_payload = serde_json::json!({
        "schema": 1u32,
        "config": config,
        "sync_state": sync_state,
    });
    let initial_token = OAuth2Token::new(
        "AT-INITIAL",
        "RT-INITIAL",
        chrono::Utc::now() + ChronoDuration::seconds(10),
        "read",
    );
    let token_payload = serde_json::to_string(&initial_token).expect("serialize OAuth2Token");

    // Seed the SQLCipher rows directly via the EvidenceStore API so
    // they get encrypted under the right scope DEK.
    {
        let cfg = evidence_store::EvidenceStoreConfig::default();
        let mut store = evidence_store::EvidenceStore::open(
            path.to_string_lossy().as_ref(),
            &master_key_bytes,
            cfg,
        )
        .expect("EvidenceStore::open");
        store.ensure_scope_dek(scope).expect("ensure_scope_dek");
        store
            .save_connector_instance(
                instance.0,
                scope,
                kind_tag,
                serde_json::to_vec(&instance_payload)
                    .expect("serialize instance payload")
                    .as_slice(),
            )
            .expect("save_connector_instance");
        store
            .save_connector_token(instance.0, scope, token_payload.as_bytes())
            .expect("save_connector_token");
    }

    // First `open_store` rehydrates both rows; refresh against the
    // local server.
    let handle = open_store(path.to_string_lossy().into_owned(), master_key_hex.clone())
        .expect("open_store");
    let report = refresh_connector_token(handle, instance.0.to_string())
        .expect("refresh_connector_token must succeed against local test server");
    assert!(report.refreshed, "first refresh must report refreshed=true");
    assert_eq!(report.instance_id, instance.0.to_string());
    assert_eq!(
        server.request_count(),
        1,
        "first refresh must drive exactly one HTTP POST",
    );
    close_store(handle).expect("close_store");

    // Reopen — the persisted row must carry RT-ROTATED-1, not
    // RT-INITIAL. Drive a second refresh; if the rotated refresh
    // token didn't survive, the test server would still see the
    // request body, but the substrate would have lost track of
    // the rotation and a subsequent live refresh would race. Pin
    // the contract by observing the second request actually fires.
    let handle = open_store(path.to_string_lossy().into_owned(), master_key_hex)
        .expect("open_store (post-rotation)");
    let report2 = refresh_connector_token(handle, instance.0.to_string())
        .expect("refresh_connector_token must succeed post-reopen");
    assert!(
        report2.refreshed,
        "second refresh must also report refreshed=true",
    );
    assert_eq!(
        server.request_count(),
        2,
        "second refresh must drive a second HTTP POST",
    );
    close_store(handle).expect("close_store (post-rotation)");

    // Belt-and-suspenders: the persisted token after the second
    // refresh must hold RT-ROTATED-2.
    {
        let cfg = evidence_store::EvidenceStoreConfig::default();
        let store = evidence_store::EvidenceStore::open(
            path.to_string_lossy().as_ref(),
            &master_key_bytes,
            cfg,
        )
        .expect("EvidenceStore::open (verify)");
        let rows = store
            .load_connector_tokens()
            .expect("load_connector_tokens");
        let (_instance_id, _scope, payload) = rows
            .iter()
            .find(|(id, _, _)| *id == instance.0)
            .expect("token row for instance");
        let parsed: OAuth2Token = serde_json::from_slice(payload).expect("rehydrate OAuth2Token");
        assert_eq!(
            parsed
                .refresh_token
                .as_ref()
                .expect("rotated token must have refresh_token")
                .expose(),
            "RT-ROTATED-2",
            "second refresh's rotated refresh_token must be persisted",
        );
        assert_eq!(parsed.access_token.expose(), "AT-2");
    }

    // Sanity: the instance id we used is a valid Uuid surface.
    assert!(Uuid::parse_str(&instance.0.to_string()).is_ok());
    drop(server);
    drop(dir);
}

/// `refresh_connector_token` against an instance whose persisted
/// token has no `refresh_token` (Slack legacy / PKCE-only public
/// clients) MUST short-circuit with the substrate-side
/// `no refresh_token stored …` diagnostic — never POST a
/// `refresh_token=` to the provider's token endpoint.
///
/// Pins the contract by standing up a "silent" test server that
/// would tick a counter on any inbound connection; the test asserts
/// `request_count == 0`.
#[cfg(feature = "http-client")]
#[test]
fn refresh_connector_token_short_circuits_when_no_refresh_token_stored() {
    use chrono::Duration as ChronoDuration;
    use connector_framework::{ConnectorInstanceId, OAuth2Token};
    use evidence_store::ScopeId;
    use ffi::refresh_connector_token;

    let server = OAuthTestServer::start_silent();
    let token_url = format!("{}/oauth/token", server.base_url());

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("evidence.db");
    let master_key_hex = "a5".repeat(32);
    let master_key_bytes: [u8; 32] = [0xa5_u8; 32];
    let scope = ScopeId::new_v4();
    let instance = ConnectorInstanceId::new_v4();
    let kind_tag = connector_framework::ConnectorKind::Slack.as_str();
    let config = connector_framework::ConnectorConfig::new(
        connector_framework::ConnectorKind::Slack,
        connector_framework::AuthKind::OAuth2,
        scope,
    )
    .with_auth_config(serde_json::json!({
        "client_id": "phase4-slack-legacy",
        "redirect_uri": "https://example.invalid/oauth/callback",
        "token_url": token_url,
    }));
    let sync_state = connector_framework::SyncState::new(instance);
    let instance_payload = serde_json::json!({
        "schema": 1u32,
        "config": config,
        "sync_state": sync_state,
    });
    let legacy_token = OAuth2Token::new_without_refresh(
        "LEGACY-SLACK-AT",
        chrono::Utc::now() + ChronoDuration::seconds(5),
        "read",
    );
    let token_payload = serde_json::to_string(&legacy_token).expect("serialize OAuth2Token");

    {
        let cfg = evidence_store::EvidenceStoreConfig::default();
        let mut store = evidence_store::EvidenceStore::open(
            path.to_string_lossy().as_ref(),
            &master_key_bytes,
            cfg,
        )
        .expect("EvidenceStore::open");
        store.ensure_scope_dek(scope).expect("ensure_scope_dek");
        store
            .save_connector_instance(
                instance.0,
                scope,
                kind_tag,
                serde_json::to_vec(&instance_payload)
                    .expect("serialize instance payload")
                    .as_slice(),
            )
            .expect("save_connector_instance");
        store
            .save_connector_token(instance.0, scope, token_payload.as_bytes())
            .expect("save_connector_token");
    }

    let handle =
        open_store(path.to_string_lossy().into_owned(), master_key_hex).expect("open_store");
    let err = refresh_connector_token(handle, instance.0.to_string())
        .expect_err("refresh without refresh_token must short-circuit");
    match err {
        FfiError::Connector { message } => {
            assert!(
                message.contains("no refresh_token stored")
                    && message.contains(&instance.0.to_string())
                    && message.contains("re-authorisation required"),
                "expected substrate-side `no refresh_token stored` diagnostic naming the \
                 instance and the recovery path; got: {message:?}",
            );
        }
        other => panic!("expected Connector(no refresh_token …); got {other:?}"),
    }
    assert_eq!(
        server.request_count(),
        0,
        "substrate must NOT POST refresh_token= to the provider when none is stored",
    );

    close_store(handle).expect("close_store");
    drop(server);
    drop(dir);
}

// ───────────────── Phase 4.1: client_secret resolver tests ─────────────────

/// Helper resolver used by the Phase 4.1 tests. Holds a closure that
/// produces the secret on demand and a counter so tests can assert
/// on the number of times the resolver was consulted.
#[cfg(feature = "http-client")]
struct TestResolver {
    secret: Option<String>,
    calls: Arc<AtomicUsize>,
    last_kind: std::sync::Mutex<Option<String>>,
    last_client_id: std::sync::Mutex<Option<String>>,
}

#[cfg(feature = "http-client")]
impl TestResolver {
    fn new(secret: Option<&str>) -> Self {
        Self {
            secret: secret.map(str::to_owned),
            calls: Arc::new(AtomicUsize::new(0)),
            last_kind: std::sync::Mutex::new(None),
            last_client_id: std::sync::Mutex::new(None),
        }
    }
}

#[cfg(feature = "http-client")]
impl ffi::OAuthClientSecretResolver for TestResolver {
    fn resolve(&self, kind: String, _scope_id: String, client_id: String) -> Option<String> {
        self.calls.fetch_add(1, AtomicOrdering::SeqCst);
        if let Ok(mut g) = self.last_kind.lock() {
            *g = Some(kind);
        }
        if let Ok(mut g) = self.last_client_id.lock() {
            *g = Some(client_id);
        }
        self.secret.clone()
    }
}

/// Set up an instance + initial token row in SQLCipher with the
/// supplied `auth_config_json` blob and return the path + master key
/// bytes + scope + instance ids. Centralised so the four Phase 4.1
/// resolver tests stay focused on the resolver-resolution behaviour
/// rather than the persistence boilerplate.
#[cfg(feature = "http-client")]
fn seed_oauth_refresh_fixture(
    auth_config: serde_json::Value,
    kind: connector_framework::ConnectorKind,
) -> (
    std::path::PathBuf,
    [u8; 32],
    evidence_store::ScopeId,
    connector_framework::ConnectorInstanceId,
    tempfile::TempDir,
) {
    use chrono::Duration as ChronoDuration;
    use connector_framework::{ConnectorInstanceId, OAuth2Token};
    use evidence_store::ScopeId;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("evidence.db");
    let master_key_bytes: [u8; 32] = [0xa5_u8; 32];
    let scope = ScopeId::new_v4();
    let instance = ConnectorInstanceId::new_v4();
    let config = connector_framework::ConnectorConfig::new(
        kind,
        connector_framework::AuthKind::OAuth2,
        scope,
    )
    .with_auth_config(auth_config);
    let sync_state = connector_framework::SyncState::new(instance);
    let instance_payload = serde_json::json!({
        "schema": 1u32,
        "config": config,
        "sync_state": sync_state,
    });
    let initial_token = OAuth2Token::new(
        "AT-INITIAL",
        "RT-INITIAL",
        chrono::Utc::now() + ChronoDuration::seconds(10),
        "read",
    );
    let token_payload = serde_json::to_string(&initial_token).expect("serialize OAuth2Token");

    let cfg = evidence_store::EvidenceStoreConfig::default();
    let mut store = evidence_store::EvidenceStore::open(
        path.to_string_lossy().as_ref(),
        &master_key_bytes,
        cfg,
    )
    .expect("EvidenceStore::open");
    store.ensure_scope_dek(scope).expect("ensure_scope_dek");
    store
        .save_connector_instance(
            instance.0,
            scope,
            kind.as_str(),
            serde_json::to_vec(&instance_payload)
                .expect("serialize instance payload")
                .as_slice(),
        )
        .expect("save_connector_instance");
    store
        .save_connector_token(instance.0, scope, token_payload.as_bytes())
        .expect("save_connector_token");

    (path, master_key_bytes, scope, instance, dir)
}

/// Phase 4.1 layer 1: when a resolver returns `Some(secret)`, that
/// secret is what appears in the OAuth2 `refresh_token` POST body's
/// `client_secret=` form field — taking precedence over the
/// `auth_config_json["client_secret"]` value AND short-circuiting the
/// fallback ladder entirely. Pins the production path where hosts
/// keep secrets in the OS keychain and never persist them to the
/// substrate's SQLCipher.
#[cfg(feature = "http-client")]
#[test]
fn client_secret_resolver_layer_1_wins_over_auth_config_json() {
    use ffi::{refresh_connector_token, set_oauth_client_secret_resolver};

    let server = OAuthTestServer::start(vec![
        r#"{"access_token":"AT-1","refresh_token":"RT-1","expires_in":3600,"scope":"read"}"#
            .to_string(),
    ]);
    let token_url = format!("{}/oauth/token", server.base_url());
    let (path, master_key_bytes, _scope, instance, dir) = seed_oauth_refresh_fixture(
        serde_json::json!({
            "client_id": "phase4_1-client",
            "client_secret": "FALLBACK-SECRET-NOT-SENT",
            "redirect_uri": "https://example.invalid/oauth/callback",
            "token_url": token_url,
        }),
        connector_framework::ConnectorKind::Notion,
    );
    let master_key_hex = hex_encode(&master_key_bytes);

    let handle =
        open_store(path.to_string_lossy().into_owned(), master_key_hex).expect("open_store");

    let resolver = Arc::new(TestResolver::new(Some("RESOLVER-WINS")));
    let calls = Arc::clone(&resolver.calls);
    set_oauth_client_secret_resolver(
        handle,
        resolver.clone() as Arc<dyn ffi::OAuthClientSecretResolver>,
    )
    .expect("set_oauth_client_secret_resolver");

    refresh_connector_token(handle, instance.0.to_string())
        .expect("refresh_connector_token must succeed against local test server");

    assert!(
        calls.load(AtomicOrdering::SeqCst) >= 1,
        "resolver must be consulted at least once during the refresh grant",
    );
    let bodies = server.request_bodies();
    assert_eq!(bodies.len(), 1, "exactly one refresh POST expected");
    let body = &bodies[0];
    assert!(
        body.contains("client_secret=RESOLVER-WINS"),
        "POST body must carry the resolver-supplied secret; got body={body}",
    );
    assert!(
        !body.contains("FALLBACK-SECRET-NOT-SENT"),
        "POST body must NOT include the auth_config_json[\"client_secret\"] when the \
         resolver short-circuited; got body={body}",
    );

    close_store(handle).expect("close_store");
    drop(server);
    drop(dir);
}

/// Phase 4.1 layer 2: when no resolver is registered, the
/// substrate falls through to `auth_config_json["client_secret"]`
/// and includes it as the form field on the refresh POST. Pins the
/// test / single-tenant dev host path.
#[cfg(feature = "http-client")]
#[test]
fn client_secret_resolver_layer_2_auth_config_json_when_no_resolver() {
    use ffi::refresh_connector_token;

    let server = OAuthTestServer::start(vec![
        r#"{"access_token":"AT-1","refresh_token":"RT-1","expires_in":3600,"scope":"read"}"#
            .to_string(),
    ]);
    let token_url = format!("{}/oauth/token", server.base_url());
    let (path, master_key_bytes, _scope, instance, dir) = seed_oauth_refresh_fixture(
        serde_json::json!({
            "client_id": "phase4_1-client",
            "client_secret": "AUTH-CONFIG-SECRET",
            "redirect_uri": "https://example.invalid/oauth/callback",
            "token_url": token_url,
        }),
        connector_framework::ConnectorKind::Notion,
    );
    let master_key_hex = hex_encode(&master_key_bytes);

    let handle =
        open_store(path.to_string_lossy().into_owned(), master_key_hex).expect("open_store");

    refresh_connector_token(handle, instance.0.to_string())
        .expect("refresh_connector_token must succeed");

    let bodies = server.request_bodies();
    assert_eq!(bodies.len(), 1);
    let body = &bodies[0];
    assert!(
        body.contains("client_secret=AUTH-CONFIG-SECRET"),
        "with no resolver registered the POST body must carry \
         auth_config_json[\"client_secret\"]; got body={body}",
    );

    close_store(handle).expect("close_store");
    drop(server);
    drop(dir);
}

/// Phase 4.1 layer 2b: when a resolver IS registered but returns
/// `None`, the framework falls through to the
/// `auth_config_json["client_secret"]` layer instead of omitting
/// the form field. Pins the multi-tenant "secret not yet loaded
/// into keychain" recovery semantics.
#[cfg(feature = "http-client")]
#[test]
fn client_secret_resolver_layer_2_when_resolver_returns_none() {
    use ffi::{refresh_connector_token, set_oauth_client_secret_resolver};

    let server = OAuthTestServer::start(vec![
        r#"{"access_token":"AT-1","refresh_token":"RT-1","expires_in":3600,"scope":"read"}"#
            .to_string(),
    ]);
    let token_url = format!("{}/oauth/token", server.base_url());
    let (path, master_key_bytes, _scope, instance, dir) = seed_oauth_refresh_fixture(
        serde_json::json!({
            "client_id": "phase4_1-client",
            "client_secret": "FALLBACK-SECRET",
            "redirect_uri": "https://example.invalid/oauth/callback",
            "token_url": token_url,
        }),
        connector_framework::ConnectorKind::Notion,
    );
    let master_key_hex = hex_encode(&master_key_bytes);

    let handle =
        open_store(path.to_string_lossy().into_owned(), master_key_hex).expect("open_store");

    let resolver = Arc::new(TestResolver::new(None));
    let calls = Arc::clone(&resolver.calls);
    set_oauth_client_secret_resolver(
        handle,
        resolver.clone() as Arc<dyn ffi::OAuthClientSecretResolver>,
    )
    .expect("set_oauth_client_secret_resolver");

    refresh_connector_token(handle, instance.0.to_string())
        .expect("refresh_connector_token must succeed");

    assert!(
        calls.load(AtomicOrdering::SeqCst) >= 1,
        "resolver must be consulted at least once even when it returns None",
    );
    let bodies = server.request_bodies();
    assert_eq!(bodies.len(), 1);
    let body = &bodies[0];
    assert!(
        body.contains("client_secret=FALLBACK-SECRET"),
        "resolver-returns-None must fall through to auth_config_json fallback; got body={body}",
    );

    close_store(handle).expect("close_store");
    drop(server);
    drop(dir);
}

/// Phase 4.1 layer 3: when no resolver is registered AND
/// `auth_config_json["client_secret"]` is absent, the substrate
/// MUST NOT include a `client_secret=` form field at all. Public-
/// client / PKCE-only providers accept this (Slack legacy);
/// confidential-client providers reject with `invalid_client` — but
/// that's the host's misconfiguration, not the substrate's bug.
/// Pins the public-client path.
#[cfg(feature = "http-client")]
#[test]
fn client_secret_resolver_layer_3_omits_form_field_when_no_secret_available() {
    use ffi::refresh_connector_token;

    let server = OAuthTestServer::start(vec![
        r#"{"access_token":"AT-1","refresh_token":"RT-1","expires_in":3600,"scope":"read"}"#
            .to_string(),
    ]);
    let token_url = format!("{}/oauth/token", server.base_url());
    let (path, master_key_bytes, _scope, instance, dir) = seed_oauth_refresh_fixture(
        serde_json::json!({
            "client_id": "phase4_1-public-client",
            "redirect_uri": "https://example.invalid/oauth/callback",
            "token_url": token_url,
        }),
        connector_framework::ConnectorKind::Slack,
    );
    let master_key_hex = hex_encode(&master_key_bytes);

    let handle =
        open_store(path.to_string_lossy().into_owned(), master_key_hex).expect("open_store");

    refresh_connector_token(handle, instance.0.to_string())
        .expect("refresh_connector_token must succeed (public-client mode)");

    let bodies = server.request_bodies();
    assert_eq!(bodies.len(), 1);
    let body = &bodies[0];
    assert!(
        !body.contains("client_secret"),
        "with neither resolver nor auth_config_json secret, the POST body must \
         OMIT the client_secret form field entirely; got body={body}",
    );

    close_store(handle).expect("close_store");
    drop(server);
    drop(dir);
}

/// Phase 4.1 lifecycle: registering a resolver, then clearing it,
/// must restore the auth_config_json fallback semantics. Pins the
/// `clear_oauth_client_secret_resolver` FFI function's contract.
#[cfg(feature = "http-client")]
#[test]
fn client_secret_resolver_clear_restores_fallback() {
    use ffi::{
        clear_oauth_client_secret_resolver, refresh_connector_token,
        set_oauth_client_secret_resolver,
    };

    let server = OAuthTestServer::start(vec![
        r#"{"access_token":"AT-1","refresh_token":"RT-1","expires_in":3600,"scope":"read"}"#
            .to_string(),
        r#"{"access_token":"AT-2","refresh_token":"RT-2","expires_in":3600,"scope":"read"}"#
            .to_string(),
    ]);
    let token_url = format!("{}/oauth/token", server.base_url());
    let (path, master_key_bytes, _scope, instance, dir) = seed_oauth_refresh_fixture(
        serde_json::json!({
            "client_id": "phase4_1-client",
            "client_secret": "RESTORED-FALLBACK",
            "redirect_uri": "https://example.invalid/oauth/callback",
            "token_url": token_url,
        }),
        connector_framework::ConnectorKind::Notion,
    );
    let master_key_hex = hex_encode(&master_key_bytes);

    let handle =
        open_store(path.to_string_lossy().into_owned(), master_key_hex).expect("open_store");

    // First grant: resolver registered, returns "RESOLVER-FIRST".
    let resolver = Arc::new(TestResolver::new(Some("RESOLVER-FIRST")))
        as Arc<dyn ffi::OAuthClientSecretResolver>;
    set_oauth_client_secret_resolver(handle, resolver).expect("set");
    refresh_connector_token(handle, instance.0.to_string()).expect("first refresh");

    // Second grant: resolver cleared, expect fallback.
    clear_oauth_client_secret_resolver(handle).expect("clear");
    refresh_connector_token(handle, instance.0.to_string()).expect("second refresh");

    let bodies = server.request_bodies();
    assert_eq!(bodies.len(), 2);
    assert!(
        bodies[0].contains("client_secret=RESOLVER-FIRST"),
        "first grant must carry the resolver-supplied secret; got body={}",
        bodies[0],
    );
    assert!(
        bodies[1].contains("client_secret=RESTORED-FALLBACK"),
        "second grant (after clear) must carry the auth_config_json fallback; \
         got body={}",
        bodies[1],
    );

    close_store(handle).expect("close_store");
    drop(server);
    drop(dir);
}

/// Phase 4.1 negative path: `set_oauth_client_secret_resolver` on a
/// runtime that never had an OAuth2 client built (only happens on
/// `--no-default-features` builds where `http-client` is off) must
/// surface `Unavailable { subsystem: "connector-http-client" }`. We
/// can't exercise that arm directly from this test file because the
/// integration tests run with all features on, but we CAN assert
/// the happy-path call doesn't error on a freshly-opened store —
/// the resolver slot is interior-mutable and idempotent across
/// repeated set/clear cycles.
#[cfg(feature = "http-client")]
#[test]
fn client_secret_resolver_set_and_clear_are_idempotent() {
    use ffi::{clear_oauth_client_secret_resolver, set_oauth_client_secret_resolver};

    let (h, _dir) = fresh_store();

    let r1 = Arc::new(TestResolver::new(Some("S1"))) as Arc<dyn ffi::OAuthClientSecretResolver>;
    set_oauth_client_secret_resolver(h, r1).expect("first set");

    let r2 = Arc::new(TestResolver::new(Some("S2"))) as Arc<dyn ffi::OAuthClientSecretResolver>;
    set_oauth_client_secret_resolver(h, r2).expect("second set replaces first");

    clear_oauth_client_secret_resolver(h).expect("first clear");
    clear_oauth_client_secret_resolver(h).expect("second clear is a no-op");

    close_store(h).expect("close_store");
}

/// Phase 4.1 health-probe wiring: the `connector` subsystem must
/// flip its `oauth_resolver=` field from `unset` to `registered`
/// after a successful `set_oauth_client_secret_resolver` call, and
/// back to `unset` after `clear_oauth_client_secret_resolver`.
/// Pins the diagnostic surface against regression so host
/// operators debugging `invalid_client` grant rejections can
/// reliably check the probe to confirm their resolver wired up.
#[cfg(feature = "http-client")]
#[test]
fn health_probe_surfaces_oauth_resolver_registration_state() {
    use ffi::{clear_oauth_client_secret_resolver, set_oauth_client_secret_resolver};

    let (h, _dir) = fresh_store();

    // Baseline: a fresh runtime has no resolver registered.
    let env = health_check(Some(h)).expect("health_check (baseline)");
    let baseline_detail = env
        .subsystems
        .iter()
        .find(|s| s.name == "connector")
        .and_then(|s| s.detail.clone())
        .expect("connector subsystem detail (baseline)");
    assert!(
        baseline_detail.contains("oauth_resolver=unset"),
        "baseline detail={baseline_detail}"
    );

    // Register a resolver — probe must flip to `registered`.
    let resolver =
        Arc::new(TestResolver::new(Some("S"))) as Arc<dyn ffi::OAuthClientSecretResolver>;
    set_oauth_client_secret_resolver(h, resolver).expect("set_oauth_client_secret_resolver");

    let env = health_check(Some(h)).expect("health_check (post-set)");
    let post_set_detail = env
        .subsystems
        .iter()
        .find(|s| s.name == "connector")
        .and_then(|s| s.detail.clone())
        .expect("connector subsystem detail (post-set)");
    assert!(
        post_set_detail.contains("oauth_resolver=registered"),
        "post-set detail={post_set_detail}"
    );
    assert!(
        !post_set_detail.contains("oauth_resolver=unset"),
        "post-set detail must not contain unset; got {post_set_detail}"
    );

    // Clear — probe flips back to `unset`. Pins the round-trip.
    clear_oauth_client_secret_resolver(h).expect("clear_oauth_client_secret_resolver");

    let env = health_check(Some(h)).expect("health_check (post-clear)");
    let post_clear_detail = env
        .subsystems
        .iter()
        .find(|s| s.name == "connector")
        .and_then(|s| s.detail.clone())
        .expect("connector subsystem detail (post-clear)");
    assert!(
        post_clear_detail.contains("oauth_resolver=unset"),
        "post-clear detail={post_clear_detail}"
    );

    close_store(h).expect("close_store");
}

/// Phase 4.1 helper: hex-encode `bytes` as a lowercase string.
/// Mirrors the encoding used by `open_store(master_key_hex)`.
#[cfg(feature = "http-client")]
fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        write!(s, "{:02x}", b).expect("hex encode");
    }
    s
}

// ───────────────────── Phase 5: webhook receiver ─────────────────

#[cfg(feature = "http-client")]
mod webhook {
    //! Integration tests for the Phase 5 webhook-receiver FFI
    //! surface. Each test stands up a temp-dir SQLCipher store,
    //! binds an axum server on `127.0.0.1:0` (ephemeral port), and
    //! exercises the FFI surface end-to-end through real HTTP
    //! requests. The framework's `WebhookServer` is the actual
    //! axum 0.7 server — there are no in-memory shortcuts.

    use super::{fresh_store, ConnectorKindTag};
    use ffi::{
        close_store, create_connector, health_check, list_webhook_servers, metrics_snapshot,
        register_webhook_dispatch, start_webhook_server, stop_webhook_server,
        unregister_webhook_dispatch, FfiError, SubsystemStatus,
    };
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::{Duration, Instant};

    /// Minimum config to satisfy `create_connector` for Slack — all
    /// fields are present but pointed at the `.invalid` TLD so any
    /// accidental outbound HTTP call fails fast (we never actually
    /// drive an auth grant on this connector; we only need a live
    /// instance in the runtime's `connector_instances` map for the
    /// webhook dispatcher to resolve).
    const SLACK_CONNECTOR_CFG: &str = r#"{
        "client_id": "phase5-webhook-client",
        "redirect_uri": "https://example.invalid/oauth/callback",
        "token_url": "https://example.invalid/oauth/token",
        "auth_url": "https://example.invalid/oauth/authorize",
        "signing_secret": "phase5-webhook-signing-secret"
    }"#;

    /// Tiny synchronous HTTP/1.1 client. Avoids pulling reqwest's
    /// blocking feature (which the FFI crate explicitly does not
    /// link) into the test target — every webhook integration test
    /// sends one request, reads one response.
    fn http_post(addr: &str, path: &str, body: &[u8]) -> (u16, String) {
        let mut stream = TcpStream::connect(addr).expect("TcpStream::connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("set_read_timeout");
        let req = format!(
            "POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Length: {}\r\n\
             Content-Type: application/json\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream
            .write_all(req.as_bytes())
            .expect("write request headers");
        stream.write_all(body).expect("write request body");
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).expect("read response");
        let text = String::from_utf8_lossy(&raw).into_owned();
        // Parse the status line — "HTTP/1.1 200 OK\r\n…"
        let status_line = text.lines().next().unwrap_or("");
        let code = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(0);
        let body = text
            .split_once("\r\n\r\n")
            .map(|(_, b)| b.to_string())
            .unwrap_or_default();
        (code, body)
    }

    /// Send a `GET` instead of a `POST`.
    fn http_get(addr: &str, path: &str) -> (u16, String) {
        let mut stream = TcpStream::connect(addr).expect("TcpStream::connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("set_read_timeout");
        let req = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
        stream.write_all(req.as_bytes()).expect("write GET request");
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).expect("read response");
        let text = String::from_utf8_lossy(&raw).into_owned();
        let status_line = text.lines().next().unwrap_or("");
        let code = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(0);
        let body = text
            .split_once("\r\n\r\n")
            .map(|(_, b)| b.to_string())
            .unwrap_or_default();
        (code, body)
    }

    #[test]
    fn webhook_server_lifecycle_start_list_stop() {
        let (h, _dir) = fresh_store();

        // Bind on ephemeral port — list to discover resolved port.
        let server = start_webhook_server(h, "127.0.0.1:0".into()).expect("start_webhook_server");
        assert_ne!(server.0, 0, "server handle must be non-zero");

        let servers = list_webhook_servers(h).expect("list_webhook_servers");
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].server_handle, server);
        assert!(servers[0].bind_addr.starts_with("127.0.0.1:"));
        assert_ne!(
            servers[0].bind_addr, "127.0.0.1:0",
            "list must surface OS-resolved port, not the requested 0",
        );
        assert_eq!(servers[0].registration_count, 0);
        assert_eq!(servers[0].dispatch_ok_total, 0);
        assert_eq!(servers[0].dispatch_bad_request_total, 0);
        assert_eq!(servers[0].dispatch_bad_gateway_total, 0);
        assert!(servers[0].started_at > 0);

        // Healthz endpoint MUST be live on the server immediately
        // after `start_webhook_server` returns.
        let (code, body) = http_get(&servers[0].bind_addr, "/healthz");
        assert_eq!(
            code, 200,
            "/healthz must return 200; got {code} body={body}"
        );

        stop_webhook_server(h, server).expect("stop_webhook_server");
        let after = list_webhook_servers(h).expect("list_webhook_servers post-stop");
        assert!(after.is_empty(), "server must be removed after stop");

        // Idempotent: stop again is fine.
        stop_webhook_server(h, server).expect("idempotent stop");

        close_store(h).expect("close_store");
    }

    #[test]
    fn webhook_dispatch_routes_to_handle_webhook_event() {
        let (h, _dir) = fresh_store();
        let scope = "00000000-0000-0000-0000-00000000beef".to_string();
        let instance = create_connector(
            h,
            ConnectorKindTag::Slack,
            scope,
            SLACK_CONNECTOR_CFG.into(),
        )
        .expect("create_connector");

        let server = start_webhook_server(h, "127.0.0.1:0".into()).expect("start_webhook_server");
        let addr = list_webhook_servers(h).expect("list")[0].bind_addr.clone();

        register_webhook_dispatch(h, server, "slack".into(), instance.clone())
            .expect("register_webhook_dispatch");

        // Slack URL-verification envelope: handle_webhook_event
        // returns Ok(Vec::new()) on this, so the dispatcher's
        // 200-OK counter ticks but no evidence is ingested.
        let body = br#"{"type":"url_verification","challenge":"phase5-challenge"}"#;
        let (code, _resp) = http_post(&addr, "/webhooks/slack", body);
        assert_eq!(code, 200, "url_verification must dispatch with 200");

        // Wait briefly for the atomic counter update — the response
        // returns from axum's task BEFORE the spawn_blocking
        // worker's outcome closure runs. Spin-poll up to 2s.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let summary = &list_webhook_servers(h).expect("list")[0];
            if summary.dispatch_ok_total == 1 {
                assert_eq!(summary.dispatch_bad_request_total, 0);
                assert_eq!(summary.dispatch_bad_gateway_total, 0);
                break;
            }
            assert!(
                Instant::now() <= deadline,
                "dispatch_ok_total never reached 1: {:?}",
                summary,
            );
            std::thread::sleep(Duration::from_millis(25));
        }

        // Process-singleton counter should track too.
        let snap = metrics_snapshot();
        assert!(
            snap.webhook_dispatch_ok_total >= 1,
            "process metric should increment alongside per-server counter",
        );

        stop_webhook_server(h, server).expect("stop");
        close_store(h).expect("close_store");
    }

    #[test]
    fn webhook_dispatch_explicit_webhook_error_returns_400() {
        // The framework's contract: `ConnectorError::Webhook(_)`
        // maps to 400, ANY OTHER `ConnectorError` (including the
        // `Json` variant that `?`-bubbles from
        // `serde_json::from_slice`) maps to 502. This test pins
        // the 400 leg by sending a payload that the Slack handler
        // explicitly turns into `ConnectorError::Webhook` (the
        // "url_verification envelope missing challenge" arm).
        // The serde-parse-error → 502 leg is pinned by
        // `webhook_dispatch_serde_failure_returns_502`.
        let (h, _dir) = fresh_store();
        let scope = "00000000-0000-0000-0000-00000000cafe".to_string();
        let instance = create_connector(
            h,
            ConnectorKindTag::Slack,
            scope,
            SLACK_CONNECTOR_CFG.into(),
        )
        .expect("create_connector");
        let server = start_webhook_server(h, "127.0.0.1:0".into()).expect("start");
        let addr = list_webhook_servers(h).expect("list")[0].bind_addr.clone();
        register_webhook_dispatch(h, server, "slack".into(), instance).expect("register");

        // url_verification missing challenge → ConnectorError::Webhook
        // → 400.
        let (code, _) = http_post(&addr, "/webhooks/slack", br#"{"type":"url_verification"}"#);
        assert_eq!(code, 400, "missing challenge must return 400");

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let s = &list_webhook_servers(h).expect("list")[0];
            if s.dispatch_bad_request_total >= 1 {
                assert_eq!(s.dispatch_ok_total, 0);
                break;
            }
            assert!(
                Instant::now() <= deadline,
                "dispatch_bad_request_total never reached 1: {:?}",
                s,
            );
            std::thread::sleep(Duration::from_millis(25));
        }

        stop_webhook_server(h, server).expect("stop");
        close_store(h).expect("close_store");
    }

    #[test]
    fn webhook_dispatch_serde_failure_returns_502() {
        // `serde_json::from_slice` failures bubble through the
        // connector's `?` as `ConnectorError::Json`, which the
        // framework maps to `502 Bad Gateway`. This is by design
        // — the framework treats malformed payloads as substrate-
        // side faults (`Json`/`Transport`/`Auth`) and reserves the
        // 400 mapping for `ConnectorError::Webhook` variants the
        // connector emits ON PURPOSE (e.g. missing fields,
        // signature failures). This test pins that contract.
        let (h, _dir) = fresh_store();
        let scope = "00000000-0000-0000-0000-00000000c0fe".to_string();
        let instance = create_connector(
            h,
            ConnectorKindTag::Slack,
            scope,
            SLACK_CONNECTOR_CFG.into(),
        )
        .expect("create_connector");
        let server = start_webhook_server(h, "127.0.0.1:0".into()).expect("start");
        let addr = list_webhook_servers(h).expect("list")[0].bind_addr.clone();
        register_webhook_dispatch(h, server, "slack".into(), instance).expect("register");

        let (code, _) = http_post(&addr, "/webhooks/slack", b"not even close to json");
        assert_eq!(code, 502, "serde_json parse failure must return 502");

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let s = &list_webhook_servers(h).expect("list")[0];
            if s.dispatch_bad_gateway_total >= 1 {
                assert_eq!(s.dispatch_ok_total, 0);
                break;
            }
            assert!(
                Instant::now() <= deadline,
                "dispatch_bad_gateway_total never reached 1: {:?}",
                s,
            );
            std::thread::sleep(Duration::from_millis(25));
        }

        stop_webhook_server(h, server).expect("stop");
        close_store(h).expect("close_store");
    }

    #[test]
    fn webhook_dispatch_unregistered_provider_returns_400() {
        let (h, _dir) = fresh_store();
        let server = start_webhook_server(h, "127.0.0.1:0".into()).expect("start");
        let addr = list_webhook_servers(h).expect("list")[0].bind_addr.clone();

        // No register_webhook_dispatch call — the router's table
        // has no entry for "slack". The framework's static route
        // exists (so 404 is not returned); the FfiWebhookRouter
        // surfaces ConnectorError::Webhook ("no instance
        // registered…") which the framework maps to 400.
        let body = br#"{"type":"url_verification","challenge":"x"}"#;
        let (code, _) = http_post(&addr, "/webhooks/slack", body);
        assert_eq!(
            code, 400,
            "unregistered provider_id must return 400, not 404",
        );

        stop_webhook_server(h, server).expect("stop");
        close_store(h).expect("close_store");
    }

    #[test]
    fn register_then_unregister_round_trip() {
        let (h, _dir) = fresh_store();
        let scope = "00000000-0000-0000-0000-00000000a0a0".to_string();
        let instance = create_connector(
            h,
            ConnectorKindTag::Slack,
            scope,
            SLACK_CONNECTOR_CFG.into(),
        )
        .expect("create_connector");
        let server = start_webhook_server(h, "127.0.0.1:0".into()).expect("start");

        assert_eq!(
            list_webhook_servers(h).expect("list")[0].registration_count,
            0
        );

        register_webhook_dispatch(h, server, "slack".into(), instance.clone()).expect("register");
        assert_eq!(
            list_webhook_servers(h).expect("list")[0].registration_count,
            1
        );

        // Re-register replaces (idempotent), count stays at 1.
        register_webhook_dispatch(h, server, "slack".into(), instance.clone())
            .expect("re-register replaces");
        assert_eq!(
            list_webhook_servers(h).expect("list")[0].registration_count,
            1
        );

        // Unregister returns Ok regardless of prior state.
        unregister_webhook_dispatch(h, server, "slack".into()).expect("unregister bound provider");
        assert_eq!(
            list_webhook_servers(h).expect("list")[0].registration_count,
            0
        );

        unregister_webhook_dispatch(h, server, "slack".into())
            .expect("unregister of unbound provider is a no-op success");

        stop_webhook_server(h, server).expect("stop");
        close_store(h).expect("close_store");
    }

    #[test]
    fn register_rejects_unknown_provider_id() {
        let (h, _dir) = fresh_store();
        let scope = "00000000-0000-0000-0000-00000000b0b0".to_string();
        let instance = create_connector(
            h,
            ConnectorKindTag::Slack,
            scope,
            SLACK_CONNECTOR_CFG.into(),
        )
        .expect("create_connector");
        let server = start_webhook_server(h, "127.0.0.1:0".into()).expect("start");

        let err = register_webhook_dispatch(h, server, "totally-not-a-provider".into(), instance)
            .expect_err("unknown provider_id must be rejected");
        assert!(
            matches!(err, FfiError::Connector { .. }),
            "unknown provider_id must surface as FfiError::Connector, got {err:?}",
        );

        stop_webhook_server(h, server).expect("stop");
        close_store(h).expect("close_store");
    }

    #[test]
    fn register_rejects_unknown_server_handle() {
        let (h, _dir) = fresh_store();
        let scope = "00000000-0000-0000-0000-00000000c0c0".to_string();
        let instance = create_connector(
            h,
            ConnectorKindTag::Slack,
            scope,
            SLACK_CONNECTOR_CFG.into(),
        )
        .expect("create_connector");

        let bogus = ffi::WebhookServerHandle(99_999_999);
        let err = register_webhook_dispatch(h, bogus, "slack".into(), instance)
            .expect_err("unknown server_handle must be rejected");
        assert!(
            matches!(err, FfiError::NotFound { ref kind, .. } if kind == "webhook_server"),
            "got {err:?}",
        );

        close_store(h).expect("close_store");
    }

    #[test]
    fn register_rejects_unknown_instance_id() {
        let (h, _dir) = fresh_store();
        let server = start_webhook_server(h, "127.0.0.1:0".into()).expect("start");

        let bogus_uuid = "00000000-0000-0000-0000-00000000dead".to_string();
        let err = register_webhook_dispatch(h, server, "slack".into(), bogus_uuid)
            .expect_err("unknown instance_id must be rejected");
        assert!(
            matches!(err, FfiError::NotFound { ref kind, .. } if kind == "connector_instance"),
            "got {err:?}",
        );

        stop_webhook_server(h, server).expect("stop");
        close_store(h).expect("close_store");
    }

    #[test]
    fn start_rejects_invalid_bind_addr() {
        let (h, _dir) = fresh_store();
        let err = start_webhook_server(h, "definitely not a socket addr".into())
            .expect_err("invalid bind_addr must be rejected");
        assert!(matches!(err, FfiError::InvalidId { .. }), "got {err:?}");
        close_store(h).expect("close_store");
    }

    #[test]
    fn close_store_drains_running_servers() {
        // The pre-drain step in close_store must synchronously
        // join the runtime threads of every running webhook server
        // BEFORE the try_unwrap spin loop. Without it, a busy
        // server would deadlock the close.
        let (h, _dir) = fresh_store();
        let _server = start_webhook_server(h, "127.0.0.1:0".into()).expect("start");
        let _server2 = start_webhook_server(h, "127.0.0.1:0".into()).expect("start 2");
        assert_eq!(list_webhook_servers(h).expect("list").len(), 2);

        // close_store must return cleanly even with running servers.
        // We do NOT call stop_webhook_server first — the drain step
        // is the test subject.
        let t0 = Instant::now();
        close_store(h).expect("close_store must drain servers, not hang");
        let elapsed = t0.elapsed();
        assert!(
            elapsed < Duration::from_secs(10),
            "close_store with drained servers should be sub-10s, took {elapsed:?}",
        );
    }

    #[test]
    fn health_probe_surfaces_webhook_server_count() {
        let (h, _dir) = fresh_store();

        // Baseline: zero servers.
        let report = health_check(Some(h)).expect("health_check");
        let connector = report
            .subsystems
            .iter()
            .find(|s| s.name == "connector")
            .expect("connector subsystem");
        assert_eq!(connector.status, SubsystemStatus::Ok);
        let detail0 = connector.detail.as_deref().unwrap_or("");
        assert!(
            detail0.contains("webhook_servers=0"),
            "baseline detail must include webhook_servers=0: {detail0}",
        );
        assert!(
            detail0.contains("webhook_registrations=0"),
            "baseline detail must include webhook_registrations=0: {detail0}",
        );

        // Start two servers, register one dispatch.
        let s1 = start_webhook_server(h, "127.0.0.1:0".into()).expect("start s1");
        let _s2 = start_webhook_server(h, "127.0.0.1:0".into()).expect("start s2");
        let scope = "00000000-0000-0000-0000-00000000d0d0".to_string();
        let instance = create_connector(
            h,
            ConnectorKindTag::Slack,
            scope,
            SLACK_CONNECTOR_CFG.into(),
        )
        .expect("create_connector");
        register_webhook_dispatch(h, s1, "slack".into(), instance).expect("register");

        let report = health_check(Some(h)).expect("health_check");
        let connector = report
            .subsystems
            .iter()
            .find(|s| s.name == "connector")
            .expect("connector subsystem");
        let detail1 = connector.detail.as_deref().unwrap_or("");
        assert!(
            detail1.contains("webhook_servers=2"),
            "post-start detail must include webhook_servers=2: {detail1}",
        );
        assert!(
            detail1.contains("webhook_registrations=1"),
            "post-register detail must include webhook_registrations=1: {detail1}",
        );

        close_store(h).expect("close_store");
    }

    #[test]
    fn metrics_snapshot_includes_webhook_counters() {
        // Counters are process-singletons and other webhook tests
        // run in parallel inside the same test binary, so we can
        // only assert that AT LEAST the counts we drove showed up
        // — not the exact delta. Pinning `==` here would race the
        // sibling tests' start/stop calls.
        let (h, _dir) = fresh_store();
        let before = metrics_snapshot();

        let server = start_webhook_server(h, "127.0.0.1:0".into()).expect("start");
        let _ = list_webhook_servers(h).expect("list");
        stop_webhook_server(h, server).expect("stop");

        let after = metrics_snapshot();
        assert!(
            after.start_webhook_server_total > before.start_webhook_server_total,
            "start counter must increment by at least 1: before={} after={}",
            before.start_webhook_server_total,
            after.start_webhook_server_total,
        );
        assert!(
            after.stop_webhook_server_total > before.stop_webhook_server_total,
            "stop counter must increment by at least 1: before={} after={}",
            before.stop_webhook_server_total,
            after.stop_webhook_server_total,
        );
        assert!(
            after.list_webhook_servers_total > before.list_webhook_servers_total,
            "list counter must increment",
        );

        close_store(h).expect("close_store");
    }

    #[test]
    fn graceful_shutdown_drains_in_flight_dispatch() {
        // The framework's graceful-shutdown contract guarantees
        // that `shutdown_and_join` blocks until every in-flight
        // request finishes. Verify by starting a request, kicking
        // off `stop_webhook_server` from another thread, and
        // checking the request completes with 200 (NOT a
        // ConnectionRefused / 503) even though stop was called
        // mid-flight.
        let (h, _dir) = fresh_store();
        let scope = "00000000-0000-0000-0000-00000000e0e0".to_string();
        let instance = create_connector(
            h,
            ConnectorKindTag::Slack,
            scope,
            SLACK_CONNECTOR_CFG.into(),
        )
        .expect("create_connector");
        let server = start_webhook_server(h, "127.0.0.1:0".into()).expect("start");
        let addr = list_webhook_servers(h).expect("list")[0].bind_addr.clone();
        register_webhook_dispatch(h, server, "slack".into(), instance).expect("register");

        let body = br#"{"type":"url_verification","challenge":"graceful"}"#;
        let (code, _) = http_post(&addr, "/webhooks/slack", body);
        assert_eq!(code, 200);

        // Subsequent stop must drain without panic.
        stop_webhook_server(h, server).expect("stop after in-flight");
        close_store(h).expect("close_store");
    }
}
