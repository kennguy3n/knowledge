//! Integration tests for the PROV provenance bundle and the
//! `ProvenanceSigner` trait surface.

use crypto::{
    AgentKind, EvidenceRef, ProvenanceAgent, ProvenanceBundle, ProvenanceSigner, SynthesisActivity,
    TestSigner, TEST_SIGNER_KEY_LEN,
};
use uuid::Uuid;

fn fixture_key(seed: u8) -> [u8; TEST_SIGNER_KEY_LEN] {
    let mut k = [0u8; TEST_SIGNER_KEY_LEN];
    for (i, byte) in k.iter_mut().enumerate() {
        *byte = (i as u8).wrapping_mul(11).wrapping_add(seed);
    }
    k
}

fn bundle() -> ProvenanceBundle {
    ProvenanceBundle::new(
        Uuid::new_v4(),
        SynthesisActivity::new(
            "synth-pipeline:elected:device-42",
            "bonsai-1.7b@q1_0_g128-2026-04-01",
            "synth.summary.v1",
            Uuid::new_v4(),
        ),
        ProvenanceAgent::software("synthesizer:test"),
        vec![
            EvidenceRef::from_uuid(Uuid::new_v4()),
            EvidenceRef::from_uuid(Uuid::new_v4()),
        ],
    )
}

#[test]
fn round_trip_signs_and_verifies() {
    let signer = TestSigner::new(fixture_key(1));
    let signed = signer.sign(bundle()).expect("sign");
    assert!(signer.verify(&signed).expect("verify"));
}

#[test]
fn wrong_key_does_not_verify() {
    let signer = TestSigner::new(fixture_key(1));
    let signed = signer.sign(bundle()).expect("sign");
    let other = TestSigner::new(fixture_key(2));
    assert!(!other.verify(&signed).expect("verify"));
}

#[test]
fn tampered_bundle_does_not_verify() {
    let signer = TestSigner::new(fixture_key(3));
    let mut signed = signer.sign(bundle()).expect("sign");
    // Tamper with the activity field after signing.
    signed.bundle.activity.model_version = "tampered@v0".to_string();
    assert!(!signer.verify(&signed).expect("verify"));
}

#[test]
fn tampered_signature_does_not_verify() {
    let signer = TestSigner::new(fixture_key(4));
    let mut signed = signer.sign(bundle()).expect("sign");
    // Flip a bit in the signature.
    signed.signature.0[0] ^= 0x80;
    assert!(!signer.verify(&signed).expect("verify"));
}

#[test]
fn human_agent_round_trips() {
    let signer = TestSigner::new(fixture_key(5));
    let mut b = bundle();
    b.agent = ProvenanceAgent::human("user:alice");
    let signed = signer.sign(b).expect("sign");
    assert!(signer.verify(&signed).expect("verify"));
    assert_eq!(signed.bundle.agent.kind, AgentKind::Human);
}

#[test]
fn empty_derivations_still_signs_and_verifies() {
    let signer = TestSigner::new(fixture_key(6));
    let mut b = bundle();
    b.derivations.clear();
    let signed = signer.sign(b).expect("sign");
    assert!(signer.verify(&signed).expect("verify"));
}

#[test]
fn signed_bundle_can_be_serialised_round_trip() {
    let signer = TestSigner::new(fixture_key(8));
    let signed = signer.sign(bundle()).expect("sign");
    let bytes = serde_json::to_vec(&signed).expect("encode");
    let decoded: crypto::SignedBundle = serde_json::from_slice(&bytes).expect("decode");
    assert!(signer.verify(&decoded).expect("verify"));
    assert_eq!(decoded, signed);
}
