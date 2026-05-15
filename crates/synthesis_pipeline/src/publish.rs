//! Encrypted synthesis-object publish / consume.
//!
//! Per `docs/DESIGN.md` §6.4 and `docs/internal/PHASES.md` Phase 2, "the synthesis
//! output is published as an encrypted synthesis object back into the
//! scope; other members consume it instead of re-synthesizing".
//!
//! The Phase 2 implementation:
//!
//! * Serialises the [`crate::SynthesisObject`] with `serde_json` (the
//!   same canonicalisation used by [`crypto::ProvenanceBundle`]).
//! * Encrypts it with the scope DEK (XChaCha20-Poly1305) via the
//!   `crypto` crate's [`crypto::encrypt_aead`] / [`crypto::decrypt_aead`].
//! * Binds `(scope_id, window_id, object_id)` into the AEAD AAD so
//!   the ciphertext cannot be replayed in a different scope or
//!   window. The AAD is reconstructed at decrypt time and any
//!   mismatch surfaces as [`crypto::CryptoError::AeadDecryption`].

use rand::RngCore;
use serde::{Deserialize, Serialize};

use crypto::{decrypt_aead, encrypt_aead, AeadCiphertext, AeadKey, AeadNonce, AEAD_NONCE_LEN};
use evidence_store::ScopeId;

use crate::error::{PipelineError, Result};
use crate::object::{ObjectId, SynthesisObject};
use crate::window::WindowId;

/// Encrypted synthesis-object envelope.
///
/// The plaintext shape is intentionally kept tiny — only the routing
/// fields the consumer needs in order to authenticate the AAD are
/// public; everything else lives inside the AEAD ciphertext.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EncryptedSynthesisObject {
    /// Object id (also bound into AAD).
    pub object_id: ObjectId,
    /// Scope id (also bound into AAD).
    pub scope_id: ScopeId,
    /// Window id (also bound into AAD).
    pub window_id: WindowId,
    /// AEAD nonce (24 bytes XChaCha20-Poly1305).
    pub nonce: AeadNonce,
    /// AEAD ciphertext (Poly1305 tag appended).
    pub ciphertext: AeadCiphertext,
}

fn aad_for(scope_id: ScopeId, window_id: WindowId, object_id: ObjectId) -> Vec<u8> {
    // Compact, deterministic AAD: 16 bytes scope || 16 bytes window || 16 bytes object.
    let mut aad = Vec::with_capacity(48);
    aad.extend_from_slice(scope_id.as_uuid().as_bytes());
    aad.extend_from_slice(window_id.as_uuid().as_bytes());
    aad.extend_from_slice(object_id.as_uuid().as_bytes());
    aad
}

fn random_nonce() -> AeadNonce {
    let mut nonce = [0u8; AEAD_NONCE_LEN];
    rand::thread_rng().fill_bytes(&mut nonce);
    nonce
}

/// Encrypt `object` under `scope_key`, binding the routing fields
/// into the AAD.
pub fn publish_synthesis_object(
    object: &SynthesisObject,
    scope_key: &AeadKey,
) -> Result<EncryptedSynthesisObject> {
    let nonce = random_nonce();
    let plaintext = serde_json::to_vec(object)
        .map_err(|_| PipelineError::Serialisation("SynthesisObject::to_vec"))?;
    let aad = aad_for(object.scope_id, object.window_id, object.id);
    let ciphertext = encrypt_aead(scope_key, &nonce, &plaintext, &aad)?;
    Ok(EncryptedSynthesisObject {
        object_id: object.id,
        scope_id: object.scope_id,
        window_id: object.window_id,
        nonce,
        ciphertext,
    })
}

/// Decrypt and deserialise the inverse of [`publish_synthesis_object`].
pub fn consume_synthesis_object(
    encrypted: &EncryptedSynthesisObject,
    scope_key: &AeadKey,
) -> Result<SynthesisObject> {
    let aad = aad_for(encrypted.scope_id, encrypted.window_id, encrypted.object_id);
    let plaintext = decrypt_aead(scope_key, &encrypted.nonce, &encrypted.ciphertext, &aad)?;
    let object: SynthesisObject = serde_json::from_slice(&plaintext)
        .map_err(|_| PipelineError::Serialisation("SynthesisObject::from_slice"))?;
    // Defence-in-depth: refuse mismatched routing in case a caller
    // hand-crafted an envelope.
    if object.id != encrypted.object_id
        || object.scope_id != encrypted.scope_id
        || object.window_id != encrypted.window_id
    {
        return Err(PipelineError::Crypto(crypto::CryptoError::AeadDecryption));
    }
    Ok(object)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{SynthesisObject, SynthesisObjectType};
    use crate::window::WindowId;
    use uuid::Uuid;

    fn fresh_key() -> AeadKey {
        let mut key = [0u8; crypto::AEAD_KEY_LEN];
        rand::thread_rng().fill_bytes(&mut key);
        key
    }

    fn fresh_object() -> SynthesisObject {
        SynthesisObject::new(
            ScopeId::new_v4(),
            WindowId::new_v4(),
            SynthesisObjectType::ChannelRecap,
            b"hello".to_vec(),
            Uuid::nil(),
        )
    }

    #[test]
    fn publish_consume_round_trip() {
        let key = fresh_key();
        let object = fresh_object();
        let encrypted = publish_synthesis_object(&object, &key).unwrap();
        let decrypted = consume_synthesis_object(&encrypted, &key).unwrap();
        assert_eq!(decrypted, object);
    }

    #[test]
    fn wrong_key_fails_to_decrypt() {
        let key = fresh_key();
        let other = fresh_key();
        let object = fresh_object();
        let encrypted = publish_synthesis_object(&object, &key).unwrap();
        let err = consume_synthesis_object(&encrypted, &other).unwrap_err();
        assert!(matches!(
            err,
            PipelineError::Crypto(crypto::CryptoError::AeadDecryption)
        ));
    }
}
