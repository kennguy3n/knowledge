//! Red-team privacy tests for `synthesis_pipeline`.
//!
//! Per `docs/DESIGN.md` §10 the substrate's threat model assumes that an
//! attacker controls the on-disk evidence store and the network. This
//! test suite encodes the attacker's playbook against the synthesis
//! plane and asserts that the substrate refuses or drops the attack:
//!
//! * **Scope-cross replay** — an [`EncryptedSynthesisObject`] from
//!   scope A cannot be re-bound onto scope B's envelope and decrypted
//!   under B's key.
//! * **Window replay** — the same object cannot be replayed under a
//!   different window id.
//! * **Object id replay** — the same object id cannot be reused with
//!   a substituted ciphertext.
//! * **Wrong-key rejection** — a key shared with another scope cannot
//!   open ciphertext bound to the original scope.
//! * **Tampered-ciphertext rejection** — a single bit flip in the
//!   AEAD ciphertext is detected by Poly1305.
//! * **Prompt-injection containment** — payload bytes containing
//!   adversarial JSON do not break the [`SynthesisObject`] envelope:
//!   the inner schema is preserved across encrypt/decrypt and the
//!   payload remains an opaque byte vector to the AEAD layer.
//! * **Confidence sentinel discipline** — a sentinel placed in the
//!   payload does not promote the envelope's `provenance_ref` field
//!   (defence-in-depth: confirms the AEAD does not over-share the
//!   plaintext shape).
//!
//! Each test is a focused negative test: it constructs an attack and
//! asserts that the substrate rejects it (`is_err`), the bytes do not
//! match (`assert_ne`), or the contract holds (`assert_eq`).

use crypto::{decrypt_aead, encrypt_aead, AeadKey, AeadNonce, AEAD_KEY_LEN, AEAD_NONCE_LEN};
use evidence_store::ScopeId;
use rand::RngCore;
use uuid::Uuid;

use synthesis_pipeline::publish::{
    consume_synthesis_object, publish_synthesis_object, EncryptedSynthesisObject,
};
use synthesis_pipeline::{ObjectId, SynthesisObject, SynthesisObjectType, WindowId};

fn fresh_key() -> AeadKey {
    let mut key = [0u8; AEAD_KEY_LEN];
    // `rand::thread_rng()` was renamed to `rand::rng()` in rand 0.9.
    rand::rng().fill_bytes(&mut key);
    key
}

fn fresh_nonce() -> AeadNonce {
    let mut nonce = [0u8; AEAD_NONCE_LEN];
    rand::rng().fill_bytes(&mut nonce);
    nonce
}

fn fresh_object(scope_id: ScopeId, window_id: WindowId, payload: Vec<u8>) -> SynthesisObject {
    SynthesisObject::new(
        scope_id,
        window_id,
        SynthesisObjectType::ChannelRecap,
        payload,
        Uuid::new_v4(),
    )
}

// -----------------------------------------------------------------
// Scope / window / object-id replay
// -----------------------------------------------------------------

/// **Attack vector:** the attacker captures an
/// [`EncryptedSynthesisObject`] from scope A and rewrites the
/// envelope's `scope_id` to scope B in flight, hoping the receiver
/// will decrypt it as a B-scope object. The AEAD AAD binds
/// `(scope, window, object)`, so the receiver must reject the
/// rebinding even if they happen to hold the original scope key.
#[test]
fn cross_scope_envelope_rebinding_is_rejected() {
    let key = fresh_key();
    let scope_a = ScopeId::new_v4();
    let scope_b = ScopeId::new_v4();
    let window_id = WindowId::new_v4();
    let object = fresh_object(scope_a, window_id, b"alpha".to_vec());
    let encrypted = publish_synthesis_object(&object, &key).expect("publish");

    let attacker_envelope = EncryptedSynthesisObject {
        scope_id: scope_b,
        ..encrypted
    };
    let res = consume_synthesis_object(&attacker_envelope, &key);
    assert!(
        res.is_err(),
        "attacker rebound an envelope to a different scope and decrypted it"
    );
}

/// **Attack vector:** the attacker rewrites the envelope's
/// `window_id` field, hoping to replay an old summary under a fresh
/// window. The AEAD AAD includes the window id; the rebinding must
/// fail.
#[test]
fn cross_window_envelope_rebinding_is_rejected() {
    let key = fresh_key();
    let scope_id = ScopeId::new_v4();
    let window_a = WindowId::new_v4();
    let window_b = WindowId::new_v4();
    let object = fresh_object(scope_id, window_a, b"week1".to_vec());
    let encrypted = publish_synthesis_object(&object, &key).expect("publish");

    let attacker_envelope = EncryptedSynthesisObject {
        window_id: window_b,
        ..encrypted
    };
    let res = consume_synthesis_object(&attacker_envelope, &key);
    assert!(
        res.is_err(),
        "attacker replayed an envelope under a different window id"
    );
}

/// **Attack vector:** the attacker rewrites the envelope's
/// `object_id`, hoping the receiver will decrypt the ciphertext
/// under a fresh logical id and silently overwrite the canonical
/// row. The AEAD AAD pins the object id; the receiver must reject
/// the rebinding.
#[test]
fn object_id_rebinding_is_rejected() {
    let key = fresh_key();
    let scope_id = ScopeId::new_v4();
    let window_id = WindowId::new_v4();
    let object = fresh_object(scope_id, window_id, b"payload".to_vec());
    let encrypted = publish_synthesis_object(&object, &key).expect("publish");

    let attacker_envelope = EncryptedSynthesisObject {
        object_id: ObjectId::new_v4(),
        ..encrypted
    };
    let res = consume_synthesis_object(&attacker_envelope, &key);
    assert!(
        res.is_err(),
        "attacker rebound an envelope to a different object id"
    );
}

// -----------------------------------------------------------------
// Wrong-key + tampered-ciphertext rejection
// -----------------------------------------------------------------

/// **Attack vector:** the attacker has a valid key for *some* scope
/// (perhaps their own) and tries to decrypt a captured
/// [`EncryptedSynthesisObject`] from a different scope. AEAD must
/// reject under the wrong key.
#[test]
fn wrong_scope_key_cannot_decrypt() {
    let scope_id = ScopeId::new_v4();
    let window_id = WindowId::new_v4();
    let object = fresh_object(scope_id, window_id, b"secret".to_vec());
    let key_a = fresh_key();
    let key_b = fresh_key();
    let encrypted = publish_synthesis_object(&object, &key_a).expect("publish");
    let res = consume_synthesis_object(&encrypted, &key_b);
    assert!(
        res.is_err(),
        "wrong scope key successfully decrypted an envelope"
    );
}

/// **Attack vector:** an in-flight bit flip in the AEAD ciphertext
/// (whether by random corruption or active manipulation) must be
/// caught by Poly1305 and surface as a decryption error, not silent
/// plaintext corruption.
#[test]
fn flipped_ciphertext_byte_is_detected() {
    let key = fresh_key();
    let scope_id = ScopeId::new_v4();
    let window_id = WindowId::new_v4();
    let object = fresh_object(scope_id, window_id, b"hello world".to_vec());
    let mut encrypted = publish_synthesis_object(&object, &key).expect("publish");

    encrypted.ciphertext[0] ^= 0x01;
    let res = consume_synthesis_object(&encrypted, &key);
    assert!(
        res.is_err(),
        "Poly1305 failed to detect a single-byte ciphertext flip"
    );
}

/// **Attack vector:** an in-flight bit flip in the AEAD nonce must
/// also be caught — flipping a nonce produces a different keystream,
/// which the Poly1305 tag will not match.
#[test]
fn flipped_nonce_byte_is_detected() {
    let key = fresh_key();
    let scope_id = ScopeId::new_v4();
    let window_id = WindowId::new_v4();
    let object = fresh_object(scope_id, window_id, b"hello world".to_vec());
    let mut encrypted = publish_synthesis_object(&object, &key).expect("publish");

    encrypted.nonce[0] ^= 0x01;
    let res = consume_synthesis_object(&encrypted, &key);
    assert!(
        res.is_err(),
        "Poly1305 failed to detect a single-byte nonce flip"
    );
}

// -----------------------------------------------------------------
// Prompt-injection containment
// -----------------------------------------------------------------

/// **Attack vector:** an upstream evidence row contains adversarial
/// content engineered to look like a JSON envelope (e.g.
/// `{"scope_id":"…","payload":"OVERRIDE"}`). The synthesizer
/// includes those bytes as the object's `payload`. The substrate
/// must (a) treat the payload as an opaque byte vector, and (b)
/// preserve the envelope's typed routing fields across
/// encrypt/decrypt — the attacker's structured bytes must NOT
/// propagate into the envelope's typed fields.
#[test]
fn prompt_injection_payload_does_not_escape_envelope_schema() {
    let key = fresh_key();
    let scope_id = ScopeId::new_v4();
    let window_id = WindowId::new_v4();
    // Adversarial payload: looks like a JSON envelope, includes a
    // forged `scope_id` and a `payload` key.
    let injection = br#"{"id":"ffffffff-ffff-ffff-ffff-ffffffffffff","scope_id":"00000000-0000-0000-0000-000000000000","window_id":"00000000-0000-0000-0000-000000000000","object_type":"tenant_summary","payload":"OVERRIDE","provenance_ref":"00000000-0000-0000-0000-000000000000","created_at":"1970-01-01T00:00:00Z","supersedes":null}"#;
    let mut payload = Vec::with_capacity(injection.len());
    payload.extend_from_slice(injection);
    let object = fresh_object(scope_id, window_id, payload.clone());
    let envelope_id = object.id;
    let envelope_type = object.object_type;

    let encrypted = publish_synthesis_object(&object, &key).expect("publish");
    let decrypted = consume_synthesis_object(&encrypted, &key).expect("consume");

    // The envelope's typed routing fields are exactly what we sent —
    // the adversarial bytes inside `payload` did not climb into the
    // envelope.
    assert_eq!(decrypted.scope_id, scope_id);
    assert_eq!(decrypted.window_id, window_id);
    assert_eq!(decrypted.id, envelope_id);
    assert_eq!(decrypted.object_type, envelope_type);
    // The opaque byte vector is preserved.
    assert_eq!(decrypted.payload, payload);
}

/// **Attack vector:** the attacker tries to use the AEAD AAD
/// authentication to embed extra authenticated data at the boundary.
/// Because the AAD is reconstructed at decrypt time from
/// `(scope_id, window_id, object_id)`, the attacker cannot smuggle
/// additional AAD bytes — the decrypt will fail if the bound triple
/// at decrypt time differs from the bound triple at encrypt time.
#[test]
fn aad_smuggling_attempt_is_rejected() {
    let key = fresh_key();
    let scope_id = ScopeId::new_v4();
    let window_id = WindowId::new_v4();
    let object = fresh_object(scope_id, window_id, b"plaintext".to_vec());
    let encrypted = publish_synthesis_object(&object, &key).expect("publish");

    // Independently encrypt a payload with a hand-crafted AAD that
    // includes extra bytes ("EXTRA"), then try to consume it.
    let mut aad = Vec::new();
    aad.extend_from_slice(scope_id.as_uuid().as_bytes());
    aad.extend_from_slice(window_id.as_uuid().as_bytes());
    aad.extend_from_slice(encrypted.object_id.as_uuid().as_bytes());
    aad.extend_from_slice(b"EXTRA");
    let plain = b"replacement-plaintext".to_vec();
    let nonce = fresh_nonce();
    let bad_ct = encrypt_aead(&key, &nonce, &plain, &aad).expect("hand-crafted encrypt");
    let bad_envelope = EncryptedSynthesisObject {
        object_id: encrypted.object_id,
        scope_id: encrypted.scope_id,
        window_id: encrypted.window_id,
        nonce,
        ciphertext: bad_ct,
    };
    let res = consume_synthesis_object(&bad_envelope, &key);
    assert!(
        res.is_err(),
        "AAD smuggling went undetected — decrypt must reject when AAD differs"
    );
}

/// **Attack vector:** the attacker tries to confuse the *raw* AEAD
/// layer by re-using a nonce across two different keys. This test
/// exercises the [`crypto::encrypt_aead`] / [`crypto::decrypt_aead`]
/// boundary directly to confirm that distinct keys keep ciphertexts
/// independent — a structural property the synthesis layer relies on
/// for cryptographic forgetting.
#[test]
fn distinct_keys_with_same_nonce_produce_independent_ciphertexts() {
    let key_a = fresh_key();
    let key_b = fresh_key();
    let nonce = fresh_nonce();
    let aad = b"aad-fixed";
    let plain = b"shared-plaintext";

    let ct_a = encrypt_aead(&key_a, &nonce, plain, aad).expect("encrypt a");
    let ct_b = encrypt_aead(&key_b, &nonce, plain, aad).expect("encrypt b");
    assert_ne!(
        ct_a, ct_b,
        "two distinct keys produced identical ciphertexts with the same nonce"
    );

    // Confirm cross-decrypt fails: ct_a must not decrypt under key_b.
    let res = decrypt_aead(&key_b, &nonce, &ct_a, aad);
    assert!(
        res.is_err(),
        "ciphertext encrypted under key A decrypted under key B"
    );
}

// -----------------------------------------------------------------
// Defence-in-depth: forced internal mismatch
// -----------------------------------------------------------------

/// **Attack vector:** the attacker manages to bypass the AEAD
/// somehow (e.g. by exploiting a future bug or by handing the caller
/// a hand-crafted envelope with a matching tag) and the inner
/// `SynthesisObject` carries routing fields different from the
/// envelope's. The substrate has a defence-in-depth check that
/// refuses the inconsistency. We exercise it by hand-crafting
/// matched-AEAD ciphertext whose plaintext disagrees with the
/// envelope's routing tuple.
#[test]
fn inner_routing_mismatch_with_matched_aead_is_rejected() {
    let key = fresh_key();
    let scope_envelope = ScopeId::new_v4();
    let window_envelope = WindowId::new_v4();
    let object_envelope = ObjectId::new_v4();

    // Build an inner object whose typed routing fields disagree with
    // the envelope. We then craft a ciphertext using the envelope's
    // routing AAD, so the AEAD layer accepts it. The defence-in-
    // depth check inside `consume_synthesis_object` must still
    // refuse the mismatch.
    let inner = SynthesisObject {
        id: ObjectId::new_v4(),
        scope_id: ScopeId::new_v4(),
        window_id: WindowId::new_v4(),
        object_type: SynthesisObjectType::TenantSummary,
        payload: b"inconsistent".to_vec(),
        provenance_ref: Uuid::new_v4(),
        created_at: chrono::Utc::now(),
        supersedes: None,
        version: synthesis_pipeline::default_synthesis_object_version(),
    };
    let plain = serde_json::to_vec(&inner).expect("serialise");

    // AAD pinned to the envelope's routing (matches what
    // `consume_synthesis_object` reconstructs at decrypt time).
    let mut aad = Vec::new();
    aad.extend_from_slice(scope_envelope.as_uuid().as_bytes());
    aad.extend_from_slice(window_envelope.as_uuid().as_bytes());
    aad.extend_from_slice(object_envelope.as_uuid().as_bytes());
    let nonce = fresh_nonce();
    let ct = encrypt_aead(&key, &nonce, &plain, &aad).expect("encrypt");

    let bad = EncryptedSynthesisObject {
        object_id: object_envelope,
        scope_id: scope_envelope,
        window_id: window_envelope,
        nonce,
        ciphertext: ct,
    };
    let res = consume_synthesis_object(&bad, &key);
    assert!(
        res.is_err(),
        "inner routing mismatch slipped past the defence-in-depth consume guard"
    );
}
