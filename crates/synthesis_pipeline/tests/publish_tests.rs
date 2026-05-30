//! Integration tests for the encrypted publish / consume round-trip.

use rand::RngCore;

use crypto::{AeadKey, AEAD_KEY_LEN};
use evidence_store::ScopeId;
use synthesis_pipeline::{
    consume_synthesis_object, publish_synthesis_object, PipelineError, SynthesisObjectType,
    SynthesisWindow,
};

fn fresh_key() -> AeadKey {
    let mut key = [0u8; AEAD_KEY_LEN];
    // `rand::thread_rng()` was renamed to `rand::rng()` in rand 0.9.
    rand::rng().fill_bytes(&mut key);
    key
}

fn fresh_object(scope: ScopeId) -> synthesis_pipeline::SynthesisObject {
    let window = SynthesisWindow::new(
        scope,
        chrono::Utc::now() - chrono::Duration::hours(1),
        chrono::Utc::now(),
    )
    .unwrap();
    synthesis_pipeline::SynthesisObject::new(
        scope,
        window.id,
        SynthesisObjectType::ChannelRecap,
        b"sensitive synthesis payload".to_vec(),
        uuid::Uuid::new_v4(),
    )
}

#[test]
fn round_trip_recovers_object() {
    let key = fresh_key();
    let scope = ScopeId::new_v4();
    let object = fresh_object(scope);
    let encrypted = publish_synthesis_object(&object, &key).unwrap();
    let decrypted = consume_synthesis_object(&encrypted, &key).unwrap();
    assert_eq!(decrypted, object);
}

#[test]
fn wrong_key_is_rejected() {
    let scope = ScopeId::new_v4();
    let object = fresh_object(scope);
    let encrypted = publish_synthesis_object(&object, &fresh_key()).unwrap();
    let err = consume_synthesis_object(&encrypted, &fresh_key()).unwrap_err();
    assert!(matches!(
        err,
        PipelineError::Crypto(crypto::CryptoError::AeadDecryption)
    ));
}

#[test]
fn aad_binds_scope_id() {
    // Tampering with the routing field invalidates the AAD and
    // therefore the AEAD authentication tag.
    let key = fresh_key();
    let scope = ScopeId::new_v4();
    let object = fresh_object(scope);
    let mut encrypted = publish_synthesis_object(&object, &key).unwrap();
    encrypted.scope_id = ScopeId::new_v4();
    let err = consume_synthesis_object(&encrypted, &key).unwrap_err();
    assert!(matches!(
        err,
        PipelineError::Crypto(crypto::CryptoError::AeadDecryption)
    ));
}

#[test]
fn aad_binds_window_id() {
    let key = fresh_key();
    let scope = ScopeId::new_v4();
    let object = fresh_object(scope);
    let mut encrypted = publish_synthesis_object(&object, &key).unwrap();
    encrypted.window_id = synthesis_pipeline::WindowId::new_v4();
    let err = consume_synthesis_object(&encrypted, &key).unwrap_err();
    assert!(matches!(
        err,
        PipelineError::Crypto(crypto::CryptoError::AeadDecryption)
    ));
}

#[test]
fn aad_binds_object_id() {
    let key = fresh_key();
    let scope = ScopeId::new_v4();
    let object = fresh_object(scope);
    let mut encrypted = publish_synthesis_object(&object, &key).unwrap();
    encrypted.object_id = synthesis_pipeline::ObjectId::new_v4();
    let err = consume_synthesis_object(&encrypted, &key).unwrap_err();
    assert!(matches!(
        err,
        PipelineError::Crypto(crypto::CryptoError::AeadDecryption)
    ));
}

#[test]
fn nonces_are_randomised() {
    // Two consecutive publications of the same object produce
    // distinct ciphertexts (because the nonce is random). This is
    // important for the substrate's ciphertext-indistinguishability
    // posture.
    let key = fresh_key();
    let scope = ScopeId::new_v4();
    let object = fresh_object(scope);
    let a = publish_synthesis_object(&object, &key).unwrap();
    let b = publish_synthesis_object(&object, &key).unwrap();
    assert_ne!(a.nonce, b.nonce);
    assert_ne!(a.ciphertext, b.ciphertext);
}

#[test]
fn tampered_ciphertext_is_rejected() {
    let key = fresh_key();
    let scope = ScopeId::new_v4();
    let object = fresh_object(scope);
    let mut encrypted = publish_synthesis_object(&object, &key).unwrap();
    encrypted.ciphertext[0] ^= 0x01;
    let err = consume_synthesis_object(&encrypted, &key).unwrap_err();
    assert!(matches!(
        err,
        PipelineError::Crypto(crypto::CryptoError::AeadDecryption)
    ));
}
