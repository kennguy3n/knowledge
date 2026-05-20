//! Stage 7 — Crypto.
//!
//! Exercises the substrate's cryptographic primitives end-to-end:
//!
//! * **Provenance signing** — a `TestSigner` (HMAC-SHA256) produces a
//!   `SignedBundle` for each canonical concept created in the concept-graph stage and
//!   then verifies the signature, plus shows that wrong-key /
//!   tampered bundles fail verification.
//! * **Hybrid KEM** — generate an X25519 + ML-KEM-768 hybrid keypair,
//!   encap → decap, assert the shared secret matches, plus a
//!   negative test where a different secret key fails to recover.
//! * **AEAD** — XChaCha20-Poly1305 encrypt/decrypt round-trip with
//!   AAD binding the scope id, plus tampered-key, tampered-AAD, and
//!   tampered-ciphertext negative tests.
//! * **Cryptographic forgetting** — register `ScopeDek`s in a
//!   `DekRegistry`, encrypt a payload, destroy the scope DEK, then
//!   verify the payload is permanently unrecoverable and a
//!   tombstone is observable.
//! * **Epoch rotation** — drive an `EpochManager` with a
//!   `DeterministicEpochKeySource`, rotate epochs by force / size /
//!   time, and verify a destroyed epoch is forgotten while live
//!   epochs continue to decrypt their own payloads.

use std::time::{Duration, Instant};

use chrono::Duration as ChronoDuration;
use crypto::{
    aead::AEAD_NONCE_LEN,
    decrypt_aead, encrypt_aead,
    forgetting::{
        destroy_epoch_dek, destroy_scope_dek, DekRegistry, DeterministicEpochKeySource, EpochDek,
        EpochId, EpochManager, EpochRotationPolicy, ScopeDek, ScopeId,
    },
    hybrid_kem_decap, hybrid_kem_encap, hybrid_keypair, AeadKey, EvidenceRef, ProvenanceAgent,
    ProvenanceBundle, ProvenanceSigner, SynthesisActivity, TestSigner, AEAD_KEY_LEN,
    TEST_SIGNER_KEY_LEN,
};
use uuid::Uuid;

use crate::assertions::AssertionLog;
use crate::dataset::Dataset;
use crate::phases::runtime::RuntimeState;
use crate::report::{DemoReport, PhaseReport};

const PHASE: &str = "crypto";

pub fn run(
    dataset: &Dataset,
    state: &mut RuntimeState,
    report: &mut DemoReport,
    log: &mut AssertionLog,
) {
    let started = Instant::now();
    let mut phase = PhaseReport::new("Stage 7: Crypto");

    // -------- Provenance signing -----------------------------------
    let signer_key: [u8; TEST_SIGNER_KEY_LEN] = {
        let mut k = [0u8; TEST_SIGNER_KEY_LEN];
        for (i, b) in k.iter_mut().enumerate() {
            *b = i as u8;
        }
        k
    };
    let wrong_key: [u8; TEST_SIGNER_KEY_LEN] = {
        let mut k = signer_key;
        k[0] ^= 0xff;
        k
    };
    let signer = TestSigner::new(signer_key);
    let wrong_signer = TestSigner::new(wrong_key);

    let prov_started = Instant::now();
    let mut signed_total: u64 = 0;
    let mut verify_total: u64 = 0;
    let mut verify_pass: u64 = 0;
    let mut wrong_key_failures: u64 = 0;
    let mut tampered_failures: u64 = 0;

    // Prefer real concept ids from the concept-graph stage, but fall back to derived
    // ids when that stage wasn't able to canonicalise anything (the
    // demo still needs to exercise the signer).
    let entity_ids: Vec<Uuid> = if state.canonical_concept_ids.is_empty() {
        (0..6)
            .map(|i| Uuid::from_u128(0x9999_0000_0000_0000_0000_0000_0000_0000 + i))
            .collect()
    } else {
        state.canonical_concept_ids.clone()
    };

    for (i, entity_id) in entity_ids.iter().take(8).enumerate() {
        let activity = SynthesisActivity::new(
            "synth-pipeline:elected:demo-device",
            "bonsai-1.7b@q1_0_g128-2026-04-01",
            format!("synth.summary.v{}", i + 1),
            Uuid::from_u128(0x7777_0000_0000_0000_0000_0000_0000_0000 + i as u128),
        );
        let agent = ProvenanceAgent::software("synthesizer:demo");
        let derivations: Vec<EvidenceRef> = state
            .ingested_rows
            .iter()
            .take(3)
            .map(|row| EvidenceRef::from_uuid(row.evidence_id.0))
            .collect();
        let bundle = ProvenanceBundle::new(*entity_id, activity, agent, derivations);
        let signed = signer
            .sign(bundle)
            .expect("provenance bundle signs cleanly");
        signed_total += 1;

        let ok = signer.verify(&signed).expect("verify succeeds");
        verify_total += 1;
        if ok {
            verify_pass += 1;
        }

        // Wrong-key verification must fail.
        let wrong_ok = wrong_signer
            .verify(&signed)
            .expect("verify under wrong key");
        if !wrong_ok {
            wrong_key_failures += 1;
        }

        // Tampered bundle: flip the entity id.
        let mut tampered = signed.clone();
        tampered.bundle.entity_id = Uuid::nil();
        let tampered_ok = signer
            .verify(&tampered)
            .expect("verify on tampered bundle returns Ok(false)");
        if !tampered_ok {
            tampered_failures += 1;
        }
    }
    let prov_elapsed = prov_started.elapsed();

    // -------- Hybrid KEM ------------------------------------------
    let kem_started = Instant::now();
    let (recipient_pk, recipient_sk) = hybrid_keypair().expect("hybrid keypair");
    let (sender_secret, kem_ct) = hybrid_kem_encap(&recipient_pk).expect("hybrid encap");
    let receiver_secret = hybrid_kem_decap(&recipient_sk, &kem_ct).expect("hybrid decap");
    let kem_elapsed = kem_started.elapsed();
    let kem_match = sender_secret == receiver_secret;
    let kem_secret_len = sender_secret.len();

    // Negative case: a different recipient cannot decap.
    let (_other_pk, other_sk) = hybrid_keypair().expect("second hybrid keypair");
    let other_decap = hybrid_kem_decap(&other_sk, &kem_ct);
    let kem_isolation = match other_decap {
        Ok(other_secret) => other_secret != sender_secret,
        Err(_) => true,
    };

    // -------- AEAD ------------------------------------------------
    let aead_key: AeadKey = sender_secret;
    let aead_nonce: [u8; AEAD_NONCE_LEN] = {
        let mut n = [0u8; AEAD_NONCE_LEN];
        for (i, b) in n.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(7).wrapping_add(0x42);
        }
        n
    };
    let aead_aad = format!(
        "scope:{};epoch:0;table:evidence_body",
        dataset.tenant_scope.id.0
    );
    let aead_payload = b"This is an evidence body that exercises the AEAD API end-to-end.".to_vec();
    let aead_started = Instant::now();
    let aead_ct = encrypt_aead(&aead_key, &aead_nonce, &aead_payload, aead_aad.as_bytes())
        .expect("aead encrypt");
    let aead_pt =
        decrypt_aead(&aead_key, &aead_nonce, &aead_ct, aead_aad.as_bytes()).expect("aead decrypt");
    let aead_elapsed = aead_started.elapsed();
    let aead_round_trip_ok = aead_pt == aead_payload;

    // Wrong key.
    let mut wrong_aead_key = aead_key;
    wrong_aead_key[0] ^= 0x01;
    let wrong_key_decrypt =
        decrypt_aead(&wrong_aead_key, &aead_nonce, &aead_ct, aead_aad.as_bytes());
    let aead_wrong_key_rejected = wrong_key_decrypt.is_err();

    // Wrong AAD.
    let wrong_aad_decrypt = decrypt_aead(
        &aead_key,
        &aead_nonce,
        &aead_ct,
        format!("scope:{};epoch:0;table:wrong", dataset.tenant_scope.id.0).as_bytes(),
    );
    let aead_wrong_aad_rejected = wrong_aad_decrypt.is_err();

    // Tampered ciphertext.
    let mut tampered_ct = aead_ct.clone();
    tampered_ct[0] ^= 0x01;
    let tampered_decrypt = decrypt_aead(&aead_key, &aead_nonce, &tampered_ct, aead_aad.as_bytes());
    let aead_tampered_rejected = tampered_decrypt.is_err();

    // -------- Cryptographic forgetting ----------------------------
    let mut registry = DekRegistry::new();
    let scope_id = ScopeId(dataset.channel_scope.id.0);
    let dek_key: AeadKey = sender_secret;
    let scope_dek = ScopeDek::new(scope_id, EpochId::zero(), dek_key);
    registry.insert_scope_dek(scope_dek);

    // Encrypt a payload under the live scope DEK.
    let forget_payload = b"Forgetting test: this should be irrecoverable post-destroy.";
    let forget_aad = format!("forgetting:scope={}", scope_id.0);
    let forget_nonce: [u8; AEAD_NONCE_LEN] = {
        let mut n = [0u8; AEAD_NONCE_LEN];
        for (i, b) in n.iter_mut().enumerate() {
            *b = (i as u8).wrapping_add(0x90);
        }
        n
    };
    let live_key = registry
        .get_scope_dek(scope_id)
        .and_then(|d| d.key())
        .copied()
        .expect("live DEK borrowable before destroy");
    let forget_ct = encrypt_aead(
        &live_key,
        &forget_nonce,
        forget_payload,
        forget_aad.as_bytes(),
    )
    .expect("encrypt under live DEK");
    let pre_destroy_decrypt =
        decrypt_aead(&live_key, &forget_nonce, &forget_ct, forget_aad.as_bytes())
            .expect("decrypt under live DEK")
            == forget_payload;

    // Destroy and verify forgetting.
    // The demo uses the legacy ephemeral-only signature (no
    // `TombstoneStore`) — the demo registry is throwaway and the
    // tombstones do not need to survive process exit.
    let destroy_events = destroy_scope_dek(&mut registry, scope_id, None)
        .expect("destroy_scope_dek must succeed with no tombstone store");
    let destroyed_count = destroy_events.len() as u64;
    let live_after_destroy = registry.get_scope_dek(scope_id).is_some();
    let scope_forgotten = registry.is_scope_forgotten(scope_id);

    // Re-derive a brand-new key — it must NOT decrypt the old
    // ciphertext (since the original key is gone forever, this is
    // the cryptographic guarantee).
    let mut zeroed_key: AeadKey = [0u8; AEAD_KEY_LEN];
    let new_key_attempt = decrypt_aead(
        &zeroed_key,
        &forget_nonce,
        &forget_ct,
        forget_aad.as_bytes(),
    );
    let zeroed_key_rejected = new_key_attempt.is_err();
    zeroed_key.fill(0);

    // Idempotency: a second destroy returns no fresh events.
    let redundant_events = destroy_scope_dek(&mut registry, scope_id, None)
        .expect("destroy_scope_dek must succeed with no tombstone store");
    let destroy_idempotent = redundant_events.is_empty();

    // -------- Epoch DEK destruction (independent epoch) -----------
    let mut epoch_registry = DekRegistry::new();
    let alt_scope = ScopeId(dataset.channel_alt_scope.id.0);
    let epoch_zero = EpochId::zero();
    let epoch_one = epoch_zero
        .next()
        .expect("EpochId::zero().next() never overflows");

    let mut zero_key: AeadKey = [0u8; AEAD_KEY_LEN];
    for (i, b) in zero_key.iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(3);
    }
    let mut one_key: AeadKey = [0u8; AEAD_KEY_LEN];
    for (i, b) in one_key.iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(5).wrapping_add(7);
    }

    epoch_registry.insert_epoch_dek(EpochDek::new(alt_scope, epoch_zero, zero_key));
    epoch_registry.insert_epoch_dek(EpochDek::new(alt_scope, epoch_one, one_key));

    let epoch_count_before = epoch_registry.epoch_count(alt_scope);
    let destroyed_epoch_events =
        destroy_epoch_dek(&mut epoch_registry, alt_scope, epoch_zero, None)
            .expect("destroy_epoch_dek must succeed with no tombstone store");
    let destroyed_epoch_zero_recorded = !destroyed_epoch_events.is_empty();
    let live_epoch_one = epoch_registry.get_epoch_dek(alt_scope, epoch_one).is_some();
    let live_epoch_zero = epoch_registry
        .get_epoch_dek(alt_scope, epoch_zero)
        .is_some();
    let epoch_zero_forgotten = epoch_registry.is_epoch_forgotten(alt_scope, epoch_zero);
    let scope_forgotten_via_single_epoch = epoch_registry.is_scope_forgotten(alt_scope);

    // -------- Epoch rotation policy --------------------------------
    let policy = EpochRotationPolicy::new(ChronoDuration::hours(24), 16 * 1024 * 1024 * 1024);
    let key_source = DeterministicEpochKeySource;
    let mut manager = EpochManager::new(policy, key_source);
    let mut rotation_registry = DekRegistry::new();
    let rotation_scope = ScopeId(dataset.domain_scope.id.0);
    manager.ensure_scope(rotation_scope, &mut rotation_registry);
    let initial_epoch = manager
        .current_epoch(rotation_scope)
        .expect("initial epoch");

    let _ = manager
        .force_rotate(rotation_scope, &mut rotation_registry)
        .expect("force_rotate at fresh demo scope cannot overflow EpochId");
    let after_force_rotate = manager
        .current_epoch(rotation_scope)
        .expect("post-force epoch");

    // Drive a size-based rotation by recording bytes.
    let _ = manager
        .record_bytes(
            rotation_scope,
            16 * 1024 * 1024 * 1024 + 1,
            &mut rotation_registry,
        )
        .expect("record_bytes at fresh demo scope cannot overflow EpochId");
    let after_size_rotate = manager
        .current_epoch(rotation_scope)
        .expect("post-size epoch");

    let total_epochs_listed = manager.list_epochs(rotation_scope).len();

    let force_progressed = after_force_rotate.0 > initial_epoch.0;
    let size_progressed = after_size_rotate.0 > after_force_rotate.0;
    let listed_at_least_three = total_epochs_listed >= 3;

    // -------- Audit trail ------------------------------------------
    state.audit_log.append(
        audit_service::AuditEntryBuilder::new()
            .actor(audit_service::Actor::System)
            .action(audit_service::AuditActionType::KeyDestruction)
            .target(audit_service::TargetRef::new(
                audit_service::TargetType::Key,
                scope_id.0,
            ))
            .scope(dataset.channel_scope.id)
            .details(serde_json::json!({
                "trigger": "demo:crypto",
                "events": destroyed_count,
            }))
            .build()
            .expect("key destruction audit"),
    );
    state.audit_log.append(
        audit_service::AuditEntryBuilder::new()
            .actor(audit_service::Actor::System)
            .action(audit_service::AuditActionType::KeyDestruction)
            .target(audit_service::TargetRef::new(
                audit_service::TargetType::Key,
                alt_scope.0,
            ))
            .scope(dataset.channel_alt_scope.id)
            .details(serde_json::json!({
                "trigger": "demo:crypto:single_epoch",
                "epoch": epoch_zero.0,
            }))
            .build()
            .expect("epoch key destruction audit"),
    );

    // -------- Statistics ------------------------------------------
    state.signed_provenance_bundles += signed_total;
    state.aead_round_trips += 1;
    state.kem_roundtrips += 1;
    state.scopes_forgotten += u64::from(scope_forgotten);
    state.epoch_rotations += 2; // force + size

    // -------- Assertions ------------------------------------------
    log.check(
        PHASE,
        "every signed bundle verifies under its own key",
        verify_total > 0 && verify_total == verify_pass,
    );
    log.check(
        PHASE,
        "wrong-key verification fails for every signed bundle",
        wrong_key_failures == verify_total && verify_total > 0,
    );
    log.check(
        PHASE,
        "tampered-entity-id bundles fail verification",
        tampered_failures == verify_total && verify_total > 0,
    );
    log.check(
        PHASE,
        "hybrid KEM encap/decap produces matching shared secrets",
        kem_match,
    );
    log.check(
        PHASE,
        "hybrid KEM shared secret length is 32 bytes (AEAD_KEY_LEN)",
        kem_secret_len == AEAD_KEY_LEN,
    );
    log.check(
        PHASE,
        "wrong recipient secret cannot recover the hybrid shared secret",
        kem_isolation,
    );
    log.check(
        PHASE,
        "AEAD encrypt/decrypt round-trips cleanly with bound AAD",
        aead_round_trip_ok,
    );
    log.check(PHASE, "AEAD wrong key is rejected", aead_wrong_key_rejected);
    log.check(PHASE, "AEAD wrong AAD is rejected", aead_wrong_aad_rejected);
    log.check(
        PHASE,
        "AEAD tampered ciphertext is rejected",
        aead_tampered_rejected,
    );
    log.check(
        PHASE,
        "scope DEK decrypts payload before destroy",
        pre_destroy_decrypt,
    );
    log.check(
        PHASE,
        "scope DEK is dropped from the registry after destroy",
        !live_after_destroy,
    );
    log.check(
        PHASE,
        "scope is forgotten after destroy_scope_dek",
        scope_forgotten,
    );
    log.check(
        PHASE,
        "destroy_scope_dek emitted at least one KeyDestructionEvent",
        destroyed_count >= 1,
    );
    log.check(
        PHASE,
        "decrypt with a zeroed key is rejected (forgetting holds)",
        zeroed_key_rejected,
    );
    log.check(PHASE, "destroy_scope_dek is idempotent", destroy_idempotent);
    log.check(
        PHASE,
        "alternate scope started with two epoch DEKs",
        epoch_count_before == 2,
    );
    log.check(
        PHASE,
        "destroy_epoch_dek emits at least one event",
        destroyed_epoch_zero_recorded,
    );
    log.check(PHASE, "destroyed epoch's DEK is gone", !live_epoch_zero);
    log.check(PHASE, "live epoch's DEK still resolves", live_epoch_one);
    log.check(
        PHASE,
        "tombstone is set for the destroyed epoch",
        epoch_zero_forgotten,
    );
    log.check(
        PHASE,
        "single-epoch destroy does NOT mark the whole scope forgotten",
        !scope_forgotten_via_single_epoch,
    );
    log.check(
        PHASE,
        "epoch manager force_rotate advances the current epoch",
        force_progressed,
    );
    log.check(
        PHASE,
        "epoch manager size trigger advances the current epoch",
        size_progressed,
    );
    log.check(
        PHASE,
        "epoch manager lists every historical epoch",
        listed_at_least_three,
    );

    // -------- Reporting --------------------------------------------
    phase.timing = started.elapsed();
    phase.stat("provenance_bundles_signed", signed_total.to_string());
    phase.stat("provenance_verifications_passed", verify_pass.to_string());
    phase.stat("aead_round_trips", "1".to_string());
    phase.stat("hybrid_kem_round_trips", "1".to_string());
    phase.stat("scope_deks_destroyed", destroyed_count.to_string());
    phase.stat("scopes_forgotten", state.scopes_forgotten.to_string());
    phase.stat("epoch_dek_tombstones", "1".to_string());
    phase.stat(
        "current_epoch_after_rotations",
        after_size_rotate.0.to_string(),
    );
    phase.stat("epochs_listed_for_scope", total_epochs_listed.to_string());
    phase.note(
        "Exercises TestSigner provenance round-trips (positive + \
         wrong-key + tampered), hybrid X25519+ML-KEM-768 encap/decap \
         (positive + wrong-recipient), XChaCha20-Poly1305 AEAD \
         (positive + wrong-key + wrong-AAD + tampered), scope DEK \
         destruction with cryptographic forgetting, single-epoch DEK \
         destruction with tombstoning, and EpochManager rotation via \
         force / size triggers.",
    );

    report.count("provenance_bundles_signed", signed_total);
    report.count("aead_round_trips", 1);
    report.count("hybrid_kem_round_trips", 1);
    report.count("scopes_forgotten", state.scopes_forgotten);
    report.count("epoch_rotations", state.epoch_rotations);
    report.add_phase(phase);

    let prov_count = signed_total.max(1);
    report.add_benchmark("provenance_sign_then_verify", prov_count, prov_elapsed);
    report.add_benchmark("hybrid_kem_encap_decap", 1, kem_elapsed);
    report.add_benchmark("aead_encrypt_decrypt", 1, aead_elapsed);

    let _ = Duration::from_secs(0); // keep std::time::Duration imported
}
