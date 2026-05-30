//! Production [`TeeRuntime`] implementation for **AWS Nitro Enclaves**.
//!
//! Gated behind `feature = "nitro-tee"`. The default build keeps
//! `MockTeeRuntime` as the only `TeeRuntime` impl so substrate hosts
//! that do not run inside a Nitro enclave never pay the linker cost
//! of `aws-nitro-enclaves-nsm-api` (which `mmap`s `/dev/nsm`).
//!
//! ## What the implementation does
//!
//! 1. **Open the NSM device.** [`driver::nsm_init`] returns a file
//!    descriptor to `/dev/nsm`. On non-enclave hosts the descriptor
//!    is negative; we surface that as a panic because the trait
//!    signature is infallible and a missing NSM device for a
//!    runtime that was deliberately compiled with `nitro-tee` is a
//!    deployment bug, not a recoverable runtime condition.
//! 2. **Send an attestation request.** [`api::Request::Attestation`]
//!    carries three optional `ByteBuf`s — `user_data`, `nonce`,
//!    `public_key`. We pin:
//!      * `user_data`  → the caller-supplied `enclave_image` bytes
//!        (so consumers can verify the workload identity the call
//!        was made for).
//!      * `nonce`      → the caller-supplied `nonce` (so the report
//!        is freshness-bound).
//!      * `public_key` → empty; the synthesizer's signing key is
//!        bound separately by [`crypto::attestation::bind_synthesizer_key`].
//! 3. **Parse the COSE_Sign1 envelope.** NSM returns
//!    `Response::Attestation { document }` where `document` is the
//!    raw COSE_Sign1 array (CBOR-encoded). RFC 8152 §4.2 fixes the
//!    structure as `[protected, unprotected, payload, signature]`.
//!    We decode the array with [`ciborium`] and extract the
//!    `payload` bytes.
//! 4. **Decode the AttestationDocument.** The payload is itself
//!    CBOR-encoded and follows the schema documented in AWS's
//!    [Nitro Enclaves Application Programming Reference][nitro-ref]:
//!      * `module_id`  : `String`
//!      * `digest`     : `String` (the hashing algorithm name)
//!      * `timestamp`  : `u64`    (UTC millis since UNIX epoch)
//!      * `pcrs`       : `Map<u32, Bytes>` (PCR index → digest)
//!      * `certificate`: `Bytes`  (leaf certificate)
//!      * `cabundle`   : `Vec<Bytes>` (CA chain back to AWS root)
//!      * `public_key` : `Option<Bytes>` (echo of request input)
//!      * `user_data`  : `Option<Bytes>` (echo)
//!      * `nonce`      : `Option<Bytes>` (echo)
//!
//!    We pull **PCR0** (the enclave image measurement) and use its
//!    bytes as the report's `measurement`. PCR0 is exactly 32 bytes
//!    for the SHA-384-truncated form Nitro emits in current
//!    firmware, which is the
//!    [`ContentHash`](crypto::ContentHash) width.
//! 5. **Build the [`AttestationReport`].**
//!      * `platform`    → [`TeePlatform::NitroEnclaves`]
//!      * `measurement` → PCR0 bytes (panic if the device returns a
//!        shape we cannot parse — same reasoning as step 1)
//!      * `report_data` → the caller-supplied `nonce` (the freshness
//!        token consumers need to bind their session)
//!      * `signature`   → the **full COSE_Sign1 document** bytes.
//!        Verifiers that need to re-validate the chain re-parse
//!        this with their own COSE library and walk back to the AWS
//!        Nitro Enclaves root CA. We deliberately do **not** pull
//!        the COSE signature blob out on its own — the protected
//!        header carries the algorithm id the verifier needs.
//!
//! ## Why panic instead of `Result`
//!
//! [`TeeRuntime::quote`] is infallible by design (the substrate's
//! `TeeWorker` is the layer that turns missing/expired/wrong-scope
//! attestations into errors). For a `nitro-tee` build the only way
//! `quote` can fail is a deployment bug — wrong AMI, NSM device
//! unavailable, firmware shape we have not been recompiled for.
//! Failing loud at the point of compromise is preferable to
//! returning a fabricated report.
//!
//! [nitro-ref]: https://docs.aws.amazon.com/enclaves/latest/user/nitro-enclave-refs.html

use aws_nitro_enclaves_nsm_api::api::{Request, Response};
use aws_nitro_enclaves_nsm_api::driver;
use ciborium::value::Value as CborValue;

use crypto::attestation::{AttestationReport, TeePlatform};
use crypto::ContentHash;

use crate::tee_worker::TeeRuntime;

/// AWS Nitro Enclaves TEE runtime.
///
/// Holds no state — every [`quote`](Self::quote) call opens a fresh
/// NSM fd, makes one `ioctl`, and closes the fd. The NSM driver is
/// per-process global anyway, and an enclave's synthesis call rate
/// is well below the cost of one `nsm_init` + `nsm_exit` round trip
/// (which is a single `ioctl` each).
#[derive(Debug, Default, Clone, Copy)]
pub struct NitroTeeRuntime;

impl NitroTeeRuntime {
    /// Construct a runtime.
    ///
    /// Equivalent to `NitroTeeRuntime::default()`; provided for
    /// symmetry with future runtime impls that may carry config.
    pub const fn new() -> Self {
        Self
    }
}

/// RAII guard that owns a `/dev/nsm` file descriptor returned by
/// [`driver::nsm_init`] and calls [`driver::nsm_exit`] on drop.
///
/// Why a guard instead of an inline `driver::nsm_exit(fd)`: panics
/// can fire at *any* point between `nsm_init` and `nsm_exit` —
/// including from inside the nsm-api crate's
/// [`driver::nsm_process_request`] itself (e.g. an upstream
/// `expect` on a malformed CBOR response, or a `From<i32>`
/// implementation that asserts on an out-of-range error code).
/// Without the guard, any such panic skips `nsm_exit` and leaks
/// the host-side fd into the next attestation call (and, over
/// time, exhausts the enclave's fd table — even more painful
/// inside the very limited Nitro filesystem). With the guard,
/// stack unwinding (or `abort=panic`'s equivalent landing pads
/// for trait-object Drop impls) still runs the destructor.
struct NsmGuard {
    fd: i32,
}

impl NsmGuard {
    /// Open `/dev/nsm` and wrap the resulting fd. Panics if the
    /// driver reports the device is unavailable — see
    /// [`NitroTeeRuntime::quote`] for the "panic on deployment
    /// bug" rationale.
    fn open() -> Self {
        let fd = driver::nsm_init();
        assert!(
            fd >= 0,
            "nitro-tee: nsm_init() returned {fd}; \
             the synthesis binary was built with the `nitro-tee` feature \
             but /dev/nsm is unavailable — this build must only run inside \
             a Nitro Enclave"
        );
        Self { fd }
    }

    fn fd(&self) -> i32 {
        self.fd
    }
}

impl Drop for NsmGuard {
    fn drop(&mut self) {
        // `nsm_exit` is documented as infallible (it issues a
        // single `close` on the kernel-side fd); we cannot
        // propagate an error from a destructor anyway.
        driver::nsm_exit(self.fd);
    }
}

impl TeeRuntime for NitroTeeRuntime {
    fn quote(&self, enclave_image: &[u8], nonce: &[u8]) -> AttestationReport {
        // 1. Open NSM device through an RAII guard. The guard's
        //    Drop impl calls `nsm_exit` even if any of the
        //    subsequent steps panic — including a panic from
        //    inside `nsm_process_request` itself.
        let guard = NsmGuard::open();

        // 2. Send an Attestation request. We bind the caller's
        //    enclave_image bytes as user_data and the caller's
        //    nonce as the request nonce.
        //
        // `serde_bytes::ByteBuf` is the type the nsm-api crate's
        // `Request::Attestation` fields are typed as; constructing
        // it from a `Vec<u8>` is the documented idiomatic path.
        let request = Request::Attestation {
            user_data: Some(serde_bytes::ByteBuf::from(enclave_image.to_vec())),
            nonce: Some(serde_bytes::ByteBuf::from(nonce.to_vec())),
            public_key: None,
        };
        let response = driver::nsm_process_request(guard.fd(), request);
        // Drop the guard explicitly here — the request is over,
        // and we want the fd back in the table before we start
        // CBOR-parsing the (potentially large) response document.
        // If any of the parse / decode steps below panic, the
        // fd has already been returned to the kernel, so the
        // enclave does not leak a host-side handle on unwind.
        drop(guard);

        let document_bytes: Vec<u8> = match response {
            Response::Attestation { document } => document,
            Response::Error(err) => panic!(
                "nitro-tee: NSM returned ErrorCode {err:?} for Attestation request"
            ),
            other => panic!(
                "nitro-tee: NSM returned unexpected response variant {other:?} for Attestation request"
            ),
        };

        // 3. Parse COSE_Sign1. RFC 8152 §4.2 fixes the shape as a
        //    4-element CBOR array.
        let cose: CborValue = ciborium::de::from_reader(document_bytes.as_slice())
            .expect("nitro-tee: NSM Attestation response was not valid CBOR");
        let cose_array = match cose {
            CborValue::Array(arr) => arr,
            other => panic!("nitro-tee: COSE_Sign1 envelope was not a CBOR array; got {other:?}"),
        };
        assert!(
            cose_array.len() == 4,
            "nitro-tee: COSE_Sign1 envelope must have exactly 4 elements per RFC 8152 §4.2; got {}",
            cose_array.len()
        );
        // Index 2 is the payload (the AttestationDocument), wrapped
        // as a CBOR byte string.
        let payload_bytes: Vec<u8> = match &cose_array[2] {
            CborValue::Bytes(b) => b.clone(),
            other => {
                panic!("nitro-tee: COSE_Sign1 payload slot must be a byte string; got {other:?}")
            }
        };

        // 4. Decode the AttestationDocument and pull PCR0.
        let doc: CborValue = ciborium::de::from_reader(payload_bytes.as_slice())
            .expect("nitro-tee: COSE payload was not valid CBOR");
        let doc_map = match doc {
            CborValue::Map(m) => m,
            other => panic!("nitro-tee: AttestationDocument was not a CBOR map; got {other:?}"),
        };
        let pcrs_value = doc_map
            .iter()
            .find_map(|(k, v)| match k {
                CborValue::Text(name) if name == "pcrs" => Some(v),
                _ => None,
            })
            .expect("nitro-tee: AttestationDocument missing required `pcrs` field");
        let pcr_map = match pcrs_value {
            CborValue::Map(m) => m,
            other => panic!("nitro-tee: `pcrs` field was not a CBOR map; got {other:?}"),
        };
        let pcr0_bytes: Vec<u8> = pcr_map
            .iter()
            .find_map(|(k, v)| {
                // Nitro emits PCR indices as CBOR unsigned integers.
                let idx_i128: i128 = match k {
                    CborValue::Integer(i) => (*i).into(),
                    _ => return None,
                };
                if idx_i128 != 0 {
                    return None;
                }
                match v {
                    CborValue::Bytes(b) => Some(b.clone()),
                    _ => None,
                }
            })
            .expect("nitro-tee: AttestationDocument missing PCR0 entry");

        // 5. Build the AttestationReport.
        //
        // `ContentHash` is a fixed-width 32-byte array. Nitro PCRs
        // are 48 bytes by default (SHA-384) but firmware-configured
        // truncation modes also exist. We truncate / left-pad to 32
        // bytes deterministically so the conversion is total. A
        // verifier that needs the full PCR0 width re-parses the
        // attestation document from the `signature` slot below.
        let measurement: ContentHash = truncate_or_pad_to_content_hash(&pcr0_bytes);

        AttestationReport::new(
            TeePlatform::NitroEnclaves,
            measurement,
            nonce.to_vec(),
            // Full COSE_Sign1 envelope so a downstream verifier can
            // walk the AWS Nitro root-CA chain and re-check the
            // signature themselves.
            document_bytes,
        )
    }
}

/// Convert a PCR digest of arbitrary length to a [`ContentHash`].
///
/// Nitro firmware emits PCR0 as 48 bytes (SHA-384). The substrate's
/// [`ContentHash`] is a fixed 32-byte alias (BLAKE3 output width).
/// Truncating to the first 32 bytes is deterministic and reversible
/// for a verifier who has the original document in the report's
/// `signature` slot — which we always include — so no information
/// is lost from the chain-of-custody perspective.
///
/// If a future firmware emits a shorter PCR (e.g. 16 bytes from a
/// SHA-256-only quote), we right-pad with zeros so the function
/// stays total. The right-pad case is observable from the
/// `signature` slot and is therefore not silently destructive.
fn truncate_or_pad_to_content_hash(pcr: &[u8]) -> ContentHash {
    let mut out: ContentHash = [0u8; 32];
    let n = pcr.len().min(32);
    out[..n].copy_from_slice(&pcr[..n]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `NitroTeeRuntime` type is `Copy` and trivially
    /// constructible — this is mostly here to ensure the `#[cfg]`
    /// gating in `lib.rs` lines up so the type is reachable from
    /// the test binary.
    #[test]
    fn nitro_runtime_constructs() {
        let _runtime: NitroTeeRuntime = NitroTeeRuntime::new();
    }

    /// `NitroTeeRuntime` must implement `TeeRuntime`. The test
    /// only asserts the trait object can be constructed — actually
    /// calling `quote()` requires a real `/dev/nsm`, which is only
    /// present inside a running enclave (see the panic in
    /// `nsm_init() < 0` above).
    #[test]
    fn nitro_runtime_implements_tee_runtime_trait() {
        fn assert_tee_runtime<T: TeeRuntime>(_t: T) {}
        assert_tee_runtime(NitroTeeRuntime::new());
    }

    /// Verify the PCR → ContentHash adapter handles the three
    /// realistic shapes: 48-byte SHA-384, 32-byte exact, and a
    /// hypothetical short PCR.
    #[test]
    fn truncate_or_pad_handles_sha384_exact_and_short() {
        let sha384: Vec<u8> = (0..48u8).collect();
        let truncated = truncate_or_pad_to_content_hash(&sha384);
        assert_eq!(&truncated[..], &sha384[..32]);

        let exact: Vec<u8> = (100..132u8).collect();
        let kept = truncate_or_pad_to_content_hash(&exact);
        assert_eq!(&kept[..], &exact[..]);

        let short: Vec<u8> = (200..216u8).collect();
        let padded = truncate_or_pad_to_content_hash(&short);
        let mut expected = [0u8; 32];
        expected[..16].copy_from_slice(&short);
        assert_eq!(&padded[..], &expected[..]);
    }
}
