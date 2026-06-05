//! Shared HMAC-SHA256 request-signing helpers for the Vietnam
//! marketplace connectors (Tiki, Shopee, Lazada).
//!
//! All three providers authenticate Open-Platform calls by signing a
//! provider-specific base string with the partner/app secret and
//! sending the lowercase hex digest as a `sign` parameter. The exact
//! base-string layout differs per provider — Shopee concatenates
//! `partner_id + path + timestamp + access_token + shop_id`, Lazada
//! sorts the request parameters and concatenates `key+value` pairs —
//! so each connector builds its own base string and calls
//! [`hmac_sha256_hex`] here for the digest.
//!
//! This lives in the `connectors` crate rather than
//! `connector_framework` because request signing is connector-
//! implementation detail; the crate already depends on the RustCrypto
//! `hmac`/`sha2` pair (pinned in lockstep at the workspace root) that
//! the `crypto` crate uses, so no new dependency is introduced.

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Compute the lowercase hex HMAC-SHA256 digest of `message` keyed by
/// `secret`.
///
/// HMAC accepts a key of any length, so the construction never fails;
/// the only `Result` the RustCrypto API surfaces is the
/// infallible-by-contract `new_from_slice`, mirrored by the
/// `expect` in `crypto::provenance`.
pub(crate) fn hmac_sha256_hex(secret: &[u8], message: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC-SHA256 accepts any key length");
    mac.update(message);
    to_hex(&mac.finalize().into_bytes())
}

/// Encode bytes as a lowercase hex string.
fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[usize::from(b >> 4)] as char);
        out.push(HEX[usize::from(b & 0x0f)] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_sha256_matches_known_vector() {
        // RFC 4231 test case 1: key = 20 bytes of 0x0b, data = "Hi There".
        let digest = hmac_sha256_hex(&[0x0b; 20], b"Hi There");
        assert_eq!(
            digest,
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn to_hex_pads_each_byte_to_two_chars() {
        assert_eq!(to_hex(&[0x00, 0x0f, 0xff, 0xa5]), "000fffa5");
    }

    #[test]
    fn hmac_is_deterministic_and_key_sensitive() {
        let a = hmac_sha256_hex(b"secret", b"payload");
        let b = hmac_sha256_hex(b"secret", b"payload");
        let c = hmac_sha256_hex(b"other", b"payload");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 64);
    }
}
