//! Red-team privacy tests for the evidence plane.
//!
//! Per `docs/technical/design.md` §9, the substrate must withstand a battery
//! of adversarial scenarios that probe the encrypted store's
//! confidentiality, scope-isolation, and tamper-resistance
//! properties. Each test in this file documents the attack vector it
//! covers in a comment block above the function so reviewers can map
//! the suite to the threat model in `docs/technical/design.md` §10.
//!
//! These are **negative** tests: every assertion is "the substrate
//! refused, dropped, or zeroized something it should have refused,
//! dropped, or zeroized". A green run means the substrate held the
//! line on that attack.

use evidence_store::{
    EvidenceStore, EvidenceStoreConfig, ImportanceClass, ScopeId, DEFAULT_INLINE_THRESHOLD_BYTES,
};
use tempfile::tempdir;

/// Symbolic master key reused across the suite. Tests that need a
/// *different* key construct a fresh array inline.
const MASTER_KEY: [u8; 32] = [0x5A; 32];

fn fresh_store_with_key(key: &[u8; 32]) -> (tempfile::TempDir, EvidenceStore) {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("evidence.db");
    let store =
        EvidenceStore::open(&path, key, EvidenceStoreConfig::default()).expect("open store");
    (dir, store)
}

fn fresh_store() -> (tempfile::TempDir, EvidenceStore) {
    fresh_store_with_key(&MASTER_KEY)
}

// -----------------------------------------------------------------
// Scope isolation
// -----------------------------------------------------------------

/// **Attack vector:** an attacker has read access to the on-disk
/// SQLCipher database (e.g. backup leak) and tries to open it with a
/// wrong master key. The store must refuse to unlock — no plaintext,
/// no schema, no metadata.
#[test]
fn scope_isolation_wrong_master_key_refuses_to_unlock() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("evidence.db");
    {
        let mut store =
            EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default()).unwrap();
        store
            .ingest(
                ScopeId::new_v4(),
                b"victim payload",
                None,
                ImportanceClass::Useful,
            )
            .unwrap();
    }
    let evil_key = [0xEEu8; 32];
    let result = EvidenceStore::open(&path, &evil_key, EvidenceStoreConfig::default());
    assert!(
        result.is_err(),
        "store opened with the wrong master key — confidentiality broken"
    );
}

/// **Attack vector:** an attacker controls one scope and tries to use
/// its scope-derived AEAD key to decrypt evidence rows owned by a
/// different scope. This test fakes the attack at the API level by
/// confirming that ingest never re-uses a `(content_hash, scope)`
/// pair to leak across scopes — bodies in `body_store` are encrypted
/// with the scope-independent body-store key, but that key is HKDF
/// from the master key with a stable label, *not* with the
/// attacker's per-scope label, so possession of one scope's DEK is
/// not enough to read the body table either.
#[test]
fn scope_isolation_inline_rows_use_distinct_aead_aads() {
    // Two scopes ingest the same small plaintext (inline path).
    // The on-disk ciphertexts must differ, because AAD includes the
    // scope id and the per-scope key is HKDF-derived. Any test
    // observation of identical ciphertexts would mean the AEAD has
    // collapsed to a single key+nonce+plaintext output across scopes
    // (it has not).
    let (_dir, mut store) = fresh_store();
    let scope_a = ScopeId::new_v4();
    let scope_b = ScopeId::new_v4();
    let body = b"shared inline secret";

    let res_a = store
        .ingest(scope_a, body, None, ImportanceClass::Useful)
        .unwrap();
    let res_b = store
        .ingest(scope_b, body, None, ImportanceClass::Useful)
        .unwrap();
    let conn = store.raw_conn();
    let ct_a: Vec<u8> = conn
        .query_row(
            "SELECT body FROM evidence WHERE id = ?1",
            rusqlite::params![res_a.evidence_id.as_uuid().as_bytes().as_slice()],
            |r| r.get(0),
        )
        .unwrap();
    let ct_b: Vec<u8> = conn
        .query_row(
            "SELECT body FROM evidence WHERE id = ?1",
            rusqlite::params![res_b.evidence_id.as_uuid().as_bytes().as_slice()],
            |r| r.get(0),
        )
        .unwrap();
    assert_ne!(
        ct_a, ct_b,
        "inline ciphertexts collapsed across scopes — keys/AAD broken"
    );
}

// -----------------------------------------------------------------
// Forgotten scope (cryptographic forgetting)
// -----------------------------------------------------------------

/// **Attack vector:** after a scope's DEK is destroyed by the
/// `crypto::forgetting` registry, an attacker with on-disk evidence
/// rows and the registry tries to decrypt those rows. With the DEK
/// gone, the row's ciphertext is unintelligible. We model this at
/// the registry layer: registering a scope DEK, destroying it, and
/// verifying the registry reports the scope as forgotten and yields
/// no key material.
#[test]
fn forgotten_scope_yields_no_dek() {
    use crypto::forgetting::{
        destroy_scope_dek, DekRegistry, EpochId, ScopeDek, ScopeId as CryptoScopeId,
    };

    let mut registry = DekRegistry::new();
    let scope_id = CryptoScopeId::new_v4();
    let key = [0x77u8; crypto::AEAD_KEY_LEN];
    registry.insert_scope_dek(ScopeDek::new(scope_id, EpochId::zero(), key));
    assert!(registry.get_scope_dek(scope_id).is_some());
    let events = destroy_scope_dek(&mut registry, scope_id, None)
        .expect("destroy_scope_dek must succeed with no tombstone store");
    assert!(!events.is_empty(), "scope DEK destroy must emit an event");
    assert!(
        registry.is_scope_forgotten(scope_id),
        "registry must report forgotten scope as forgotten"
    );
    assert!(
        registry.get_scope_dek(scope_id).is_none(),
        "registry must not return DEK material for a forgotten scope"
    );
    // Idempotent destroy.
    let again = destroy_scope_dek(&mut registry, scope_id, None)
        .expect("destroy_scope_dek must succeed with no tombstone store");
    assert!(
        again.is_empty(),
        "double-destroy must be idempotent (no new events)"
    );
}

// -----------------------------------------------------------------
// Cross-scope leakage at ingest layer
// -----------------------------------------------------------------

/// **Attack vector:** two scopes share a deduplicated body row
/// (same plaintext, large body → body_store path). The substrate
/// must allow each scope's evidence row to read the plaintext, but
/// the on-disk body row is encrypted under the *scope-independent*
/// body-store key — neither scope's per-scope DEK can decrypt the
/// body table on its own. We assert the dedup invariant: one
/// body_store row, two evidence rows, both decrypt to the same
/// plaintext.
#[test]
fn cross_scope_dedup_does_not_leak_or_corrupt() {
    let (_dir, mut store) = fresh_store();
    let scope_a = ScopeId::new_v4();
    let scope_b = ScopeId::new_v4();
    let body = vec![0xCDu8; DEFAULT_INLINE_THRESHOLD_BYTES * 3];

    let res_a = store
        .ingest(scope_a, &body, None, ImportanceClass::Useful)
        .unwrap();
    let res_b = store
        .ingest(scope_b, &body, None, ImportanceClass::Useful)
        .unwrap();

    assert_eq!(res_a.content_hash, res_b.content_hash);
    assert_eq!(store.body_store_count().unwrap(), 1);
    let pt_a = store.read_body(res_a.evidence_id).unwrap();
    let pt_b = store.read_body(res_b.evidence_id).unwrap();
    assert_eq!(pt_a, body);
    assert_eq!(pt_b, body);
}

// -----------------------------------------------------------------
// Ring buffer privacy
// -----------------------------------------------------------------

/// **Attack vector:** an attacker dumps the SQLCipher database after
/// the user has cleared the ring buffer. Because the ring buffer is
/// physically deleted (`ring_buffer_clear`) and not just marked
/// hidden, the row count must drop to zero and a fresh
/// `read_window` must return nothing.
#[test]
fn ring_buffer_clear_physically_drops_rows() {
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    store.ring_buffer_insert(scope, b"transient noise").unwrap();
    assert_eq!(store.ring_buffer_len().unwrap(), 1);
    store.ring_buffer_clear().unwrap();
    assert_eq!(store.ring_buffer_len().unwrap(), 0);
    assert!(store.ring_buffer_read_window(scope).unwrap().is_empty());
}

/// **Attack vector:** an attacker overflows the ring buffer with
/// noise hoping to cause the store to spill noise into the canonical
/// `evidence` table. The substrate must FIFO-evict ring entries and
/// never promote a noise body into the evidence table.
#[test]
fn ring_buffer_overflow_does_not_spill_into_evidence_table() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("evidence.db");
    let cfg = EvidenceStoreConfig {
        ring_buffer_max_bytes: 256,
        ..Default::default()
    };
    let mut store = EvidenceStore::open(&path, &MASTER_KEY, cfg).unwrap();
    let scope = ScopeId::new_v4();
    for i in 0..50u8 {
        let body = vec![i; 32];
        store.ring_buffer_insert(scope, &body).unwrap();
    }
    let total = store.ring_buffer_current_size().unwrap();
    assert!(
        total <= 256,
        "ring buffer ignored its byte cap (total={total})"
    );
    assert_eq!(
        store.evidence_count().unwrap(),
        0,
        "noise spilled into evidence table"
    );
}

// -----------------------------------------------------------------
// Append-only — tamper resistance
// -----------------------------------------------------------------

/// **Attack vector:** an attacker tries to mutate the
/// `source_ref` column on an existing evidence row to bypass an
/// upstream provenance check, or `DELETE` a row to erase audit
/// evidence. The append-only triggers in the schema must reject both
/// statements.
#[test]
fn append_only_evidence_table_rejects_update_and_delete() {
    let (_dir, mut store) = fresh_store();
    let scope = ScopeId::new_v4();
    let res = store
        .ingest(scope, b"a non-noise message", None, ImportanceClass::Useful)
        .unwrap();
    let id_bytes = res.evidence_id.as_uuid().as_bytes().to_vec();
    assert!(store
        .raw_conn()
        .execute(
            "UPDATE evidence SET source_ref = 'tampered' WHERE id = ?1",
            rusqlite::params![id_bytes.as_slice()],
        )
        .is_err());
    assert!(store
        .raw_conn()
        .execute(
            "DELETE FROM evidence WHERE id = ?1",
            rusqlite::params![id_bytes.as_slice()],
        )
        .is_err());
}

// -----------------------------------------------------------------
// Permission boundary
// -----------------------------------------------------------------

/// **Attack vector:** a user with only `viewer` access on a scope
/// tries to exercise an `editor`-only relation. The Zanzibar-style
/// reachability walk must return `false` because the namespace
/// chain does not say `viewer ⇒ editor`.
#[test]
fn permission_viewer_cannot_promote_to_editor() {
    use permission_service::{
        check_permission, NamespaceRegistry, ObjectRef, ObjectType, Relation, RelationTuple,
        SubjectRef, SubjectType, TupleStore,
    };
    use uuid::Uuid;

    let mut store = TupleStore::new();
    let ns = NamespaceRegistry::with_defaults();
    let channel = ObjectRef::new(ObjectType::Channel, Uuid::new_v4());
    let user = SubjectRef::direct(SubjectType::User, Uuid::new_v4());
    store
        .insert(RelationTuple::new(channel, Relation::Viewer, user))
        .unwrap();
    assert!(check_permission(
        &store,
        &ns,
        channel,
        Relation::Viewer,
        user
    ));
    assert!(
        !check_permission(&store, &ns, channel, Relation::Editor, user),
        "viewer was wrongly promoted to editor"
    );
}

// -----------------------------------------------------------------
// Agent boundary
// -----------------------------------------------------------------

/// **Attack vector:** a software agent tries to write a canonical
/// concept directly. The proposal lifecycle must require an explicit
/// `promote_to_canonical` call, which itself only succeeds against a
/// `Promoted` proposal — not a fresh `Proposed` one. We therefore
/// expect `promote_to_canonical` to refuse a freshly-submitted
/// proposal.
#[test]
fn agent_boundary_canonical_promotion_requires_review() {
    use agent_contract::{
        AgentIdentity, AgentProposal, AutoPromotionPolicy, ObservationProposal, ProposalKind,
        ProposalStore,
    };
    use crypto::EvidenceRef;
    use evidence_store::ScopeId;
    use memory_manager::SensitivityClass;
    use uuid::Uuid;

    let mut store = ProposalStore::new();
    let identity = AgentIdentity::new(
        Uuid::new_v4(),
        "test-agent",
        "bonsai-1.7b",
        "q1_0_g128-2026-04-01",
    );
    let proposal = AgentProposal::new(
        ProposalKind::Observation,
        ScopeId::new_v4(),
        ObservationProposal::new("Friday is the deadline", "fact"),
        vec![EvidenceRef::from_uuid(Uuid::new_v4())],
        0.5,
        SensitivityClass::Useful,
        identity,
    );
    let proposal_id = store.submit_observation(proposal).unwrap();
    // No review yet → must NOT be in canonical state.
    let result = store.promote_to_canonical(proposal_id);
    assert!(
        result.is_err(),
        "agent slipped a canonical write past the review gate"
    );
    // Even an explicit `review` against the deny-by-default policy
    // must not auto-flip a fresh proposal to Promoted.
    let policy = AutoPromotionPolicy::default();
    let _ = store.review(proposal_id, &policy);
    // Still not canonical-promotable until manual `promote`.
    assert!(
        store.promote_to_canonical(proposal_id).is_err(),
        "default policy auto-promoted a proposal that should require review"
    );
}

// -----------------------------------------------------------------
// Provenance integrity
// -----------------------------------------------------------------

/// **Attack vector:** an attacker intercepts a provenance bundle in
/// flight and flips a byte. The verifier must reject the tampered
/// bundle with a deterministic verification error.
#[test]
fn provenance_signature_rejects_tampered_payload() {
    use crypto::provenance::{
        EvidenceRef, ProvenanceAgent, ProvenanceBundle, ProvenanceSigner, SynthesisActivity,
        TestSigner,
    };
    use uuid::Uuid;

    let signer = TestSigner::new([0x42u8; crypto::TEST_SIGNER_KEY_LEN]);
    let agent = ProvenanceAgent::software("synthesizer:test");
    let evidence = vec![EvidenceRef::from_uuid(Uuid::new_v4())];
    let activity = SynthesisActivity::new(
        "synth-pipeline:elected:device-42",
        "bonsai-1.7b@q1_0_g128",
        "synth.summary.v1",
        Uuid::new_v4(),
    );
    let bundle = ProvenanceBundle::new(Uuid::new_v4(), activity, agent, evidence);
    let signed = signer.sign(bundle).expect("sign clean bundle");
    assert!(signer.verify(&signed).expect("verify clean bundle"));
    // Tamper: flip a byte inside the bundle's entity_id and prove the
    // verifier rejects it without producing a serialisation error.
    let mut tampered = signed.clone();
    tampered.bundle.entity_id = Uuid::from_u128(tampered.bundle.entity_id.as_u128() ^ 0x1);
    assert!(
        !signer
            .verify(&tampered)
            .expect("verifier returns Ok(false)"),
        "tampered provenance bundle slipped past the verifier"
    );
    // Tamper: flip a byte of the detached signature itself.
    let mut tampered_sig = signed.clone();
    tampered_sig.signature.0[0] ^= 0x01;
    assert!(
        !signer
            .verify(&tampered_sig)
            .expect("verifier returns Ok(false)"),
        "tampered provenance signature slipped past the verifier"
    );
}

// -----------------------------------------------------------------
// Key material handling
// -----------------------------------------------------------------

/// **Attack vector:** a memory-disclosure attack reads cold pages
/// after a DEK has been "destroyed". The substrate must zeroize the
/// in-memory DEK before dropping it. We test the contract via the
/// public `destroy_scope_dek` semantics: after destroy, `scope_dek`
/// must return `None`. We additionally exercise the registry with
/// many DEKs to confirm it does not leak references between scopes.
#[test]
fn forgetting_zeroizes_and_isolates_dek_registry_entries() {
    use crypto::forgetting::{
        destroy_scope_dek, DekRegistry, EpochId, ScopeDek, ScopeId as CryptoScopeId,
    };

    let mut registry = DekRegistry::new();
    let scope_a = CryptoScopeId::new_v4();
    let scope_b = CryptoScopeId::new_v4();
    registry.insert_scope_dek(ScopeDek::new(
        scope_a,
        EpochId::zero(),
        [0x11u8; crypto::AEAD_KEY_LEN],
    ));
    registry.insert_scope_dek(ScopeDek::new(
        scope_b,
        EpochId::zero(),
        [0x22u8; crypto::AEAD_KEY_LEN],
    ));

    let key_a = registry.get_scope_dek(scope_a).expect("scope A DEK").key();
    let key_b = registry.get_scope_dek(scope_b).expect("scope B DEK").key();
    assert_ne!(key_a, key_b, "two distinct scopes returned the same DEK");

    let events_a = destroy_scope_dek(&mut registry, scope_a, None)
        .expect("destroy_scope_dek must succeed with no tombstone store");
    assert!(!events_a.is_empty(), "destroy_scope_dek must emit an event");
    assert!(registry.is_scope_forgotten(scope_a));
    assert!(!registry.is_scope_forgotten(scope_b));
    assert!(registry.get_scope_dek(scope_a).is_none());
    assert!(registry.get_scope_dek(scope_b).is_some());
}
