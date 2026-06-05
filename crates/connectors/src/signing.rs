//! Request-signing primitives shared by the GCC / Middle East
//! connectors whose auth schemes go beyond a plain bearer token:
//!
//! * [`hmac_sha256`] / [`hex_lower`] — Noon Seller Center signs every
//!   request with an HMAC-SHA256 of a canonical string under the
//!   seller's secret key (`api_key` + `secret_key`).
//! * [`sha256_hex`] — Amazon Payment Services (PayFort) computes a
//!   SHA-256 "signature" over the SHA-request-phrase-wrapped,
//!   lexicographically-sorted request parameters.
//! * [`sigv4_authorization`] — Amazon.ae sells through the Amazon
//!   Selling-Partner API, whose `execute-api` calls are signed with
//!   AWS Signature Version 4.
//!
//! These helpers are crate-internal (`pub(crate)`) — they are an
//! implementation detail of the connectors, not part of the public
//! connector API. They depend only on `hmac` + `sha2` (already in the
//! workspace via the `crypto` crate) and a small inlined lowercase-hex
//! encoder, mirroring the codebase convention (see `content::decode_base64`)
//! of inlining tiny encoders rather than pulling in a `hex` dependency.

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// Compute `HMAC-SHA256(key, msg)`.
///
/// `Hmac::new_from_slice` only fails for key types with a fixed length
/// constraint; HMAC accepts a key of any length, so the `expect` is
/// unreachable (mirrors `crypto::provenance`).
#[must_use]
pub(crate) fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC-SHA256 accepts any key length");
    mac.update(msg);
    mac.finalize().into_bytes().into()
}

/// Compute `SHA-256(msg)`.
#[must_use]
pub(crate) fn sha256(msg: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(msg);
    hasher.finalize().into()
}

/// Compute `SHA-256(msg)` and render it as a lowercase hex string.
#[must_use]
pub(crate) fn sha256_hex(msg: &[u8]) -> String {
    hex_lower(&sha256(msg))
}

/// Lowercase-hex-encode a byte slice.
#[must_use]
pub(crate) fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// Inputs to an AWS Signature Version 4 signing operation.
///
/// The connector builds the canonical request pieces (already
/// percent-encoded where required) and this helper produces the
/// `Authorization` header value. The empty-named `session_token` /
/// extra signed headers are handled by the caller — for the SP-API
/// the LWA access token rides in `x-amz-access-token`, which is
/// included as a signed header so a man-in-the-middle cannot swap it.
#[derive(Debug, Clone)]
pub(crate) struct SigV4Request<'a> {
    /// IAM access key id.
    pub access_key_id: &'a str,
    /// IAM secret access key.
    pub secret_access_key: &'a str,
    /// AWS region, e.g. `eu-west-1` (Amazon.ae's SP-API region).
    pub region: &'a str,
    /// AWS service name, e.g. `execute-api`.
    pub service: &'a str,
    /// HTTP method, upper-case (`GET`, `POST`).
    pub method: &'a str,
    /// Request host (the `Host` header value).
    pub host: &'a str,
    /// Canonical URI — the absolute path, already percent-encoded.
    pub canonical_uri: &'a str,
    /// Canonical query string — sorted `k=v` pairs joined by `&`,
    /// already percent-encoded. Empty string when there is no query.
    pub canonical_query: &'a str,
    /// `x-amz-date` value in ISO-8601 basic format `YYYYMMDDTHHMMSSZ`.
    pub amz_date: &'a str,
    /// Request payload (empty slice for a body-less GET).
    pub payload: &'a [u8],
    /// Extra headers to sign **in addition to** `host` and
    /// `x-amz-date`, as `(lowercase-name, value)`. Names must already
    /// be lowercase; the helper sorts the combined set.
    pub extra_signed_headers: &'a [(&'a str, &'a str)],
}

/// Produce the AWS SigV4 `Authorization` header value for `req`.
///
/// Implements the canonical-request → string-to-sign → signing-key →
/// signature pipeline from the AWS "Signature Version 4" spec. The
/// caller is responsible for actually attaching `x-amz-date` (and any
/// `extra_signed_headers`) to the outgoing request — this returns only
/// the `Authorization` value so it stays a pure function that is easy
/// to pin against the published `aws-sig-v4-test-suite` vectors.
#[must_use]
pub(crate) fn sigv4_authorization(req: &SigV4Request<'_>) -> String {
    // Canonical headers: host + x-amz-date + extras, lowercased names,
    // trimmed values, sorted by name, each rendered as `name:value\n`.
    let mut headers: Vec<(String, String)> = Vec::with_capacity(2 + req.extra_signed_headers.len());
    headers.push(("host".to_string(), req.host.trim().to_string()));
    headers.push(("x-amz-date".to_string(), req.amz_date.trim().to_string()));
    for (name, value) in req.extra_signed_headers {
        headers.push((name.to_ascii_lowercase(), value.trim().to_string()));
    }
    headers.sort_by(|a, b| a.0.cmp(&b.0));

    let mut canonical_headers = String::new();
    for (n, v) in &headers {
        canonical_headers.push_str(n);
        canonical_headers.push(':');
        canonical_headers.push_str(v);
        canonical_headers.push('\n');
    }
    let signed_headers: String = headers
        .iter()
        .map(|(n, _)| n.as_str())
        .collect::<Vec<_>>()
        .join(";");

    let payload_hash = sha256_hex(req.payload);
    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        req.method,
        req.canonical_uri,
        req.canonical_query,
        canonical_headers,
        signed_headers,
        payload_hash,
    );

    // `amz_date` is `YYYYMMDDTHHMMSSZ`; the credential scope date is
    // the leading `YYYYMMDD`.
    let datestamp = &req.amz_date[..8];
    let credential_scope = format!("{datestamp}/{}/{}/aws4_request", req.region, req.service);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{}\n{}",
        req.amz_date,
        credential_scope,
        sha256_hex(canonical_request.as_bytes()),
    );

    // Derive the signing key.
    let k_secret = format!("AWS4{}", req.secret_access_key);
    let k_date = hmac_sha256(k_secret.as_bytes(), datestamp.as_bytes());
    let k_region = hmac_sha256(&k_date, req.region.as_bytes());
    let k_service = hmac_sha256(&k_region, req.service.as_bytes());
    let k_signing = hmac_sha256(&k_service, b"aws4_request");
    let signature = hex_lower(&hmac_sha256(&k_signing, string_to_sign.as_bytes()));

    format!(
        "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
        req.access_key_id, credential_scope, signed_headers, signature,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_lower_matches_known_bytes() {
        assert_eq!(hex_lower(&[0x00, 0x0f, 0xa0, 0xff]), "000fa0ff");
        assert_eq!(hex_lower(b""), "");
    }

    #[test]
    fn sha256_hex_matches_nist_empty_vector() {
        // SHA-256("") — the canonical empty-input digest.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        // SHA-256("abc") — FIPS 180-2 example.
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn hmac_sha256_matches_rfc4231_test_case_2() {
        // RFC 4231 §4.3: key = "Jefe", data = "what do ya want for
        // nothing?".
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            hex_lower(&mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn sigv4_matches_aws_get_vanilla_vector() {
        // `aws-sig-v4-test-suite` `get-vanilla`: the canonical
        // published example with empty body and only host + x-amz-date
        // signed. Pinning the produced signature proves the canonical
        // request / string-to-sign / signing-key pipeline is correct.
        let req = SigV4Request {
            access_key_id: "AKIDEXAMPLE",
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            region: "us-east-1",
            service: "service",
            method: "GET",
            host: "example.amazonaws.com",
            canonical_uri: "/",
            canonical_query: "",
            amz_date: "20150830T123600Z",
            payload: b"",
            extra_signed_headers: &[],
        };
        let auth = sigv4_authorization(&req);
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 \
             Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
             SignedHeaders=host;x-amz-date, \
             Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    #[test]
    fn sigv4_includes_and_sorts_extra_signed_headers() {
        // With an extra signed header the SignedHeaders list must
        // include it in sorted order (host < x-amz-access-token <
        // x-amz-date).
        let req = SigV4Request {
            access_key_id: "AKIDEXAMPLE",
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            region: "eu-west-1",
            service: "execute-api",
            method: "GET",
            host: "sellingpartnerapi-eu.amazon.com",
            canonical_uri: "/orders/v0/orders",
            canonical_query: "MarketplaceIds=A2VIGQ35RCS4UG",
            amz_date: "20240101T000000Z",
            payload: b"",
            extra_signed_headers: &[("x-amz-access-token", "Atza|token")],
        };
        let auth = sigv4_authorization(&req);
        assert!(
            auth.contains("SignedHeaders=host;x-amz-access-token;x-amz-date"),
            "unexpected signed headers in {auth}"
        );
        assert!(auth.starts_with(
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20240101/eu-west-1/execute-api/aws4_request"
        ));
    }
}
