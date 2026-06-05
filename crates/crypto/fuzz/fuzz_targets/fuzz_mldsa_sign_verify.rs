//! Fuzz target: ML-DSA-65 (FIPS 204) sign → verify round-trip.
//!
//! Generates a fresh ML-DSA-65 keypair once (via `thread_local!`),
//! then for each fuzzed input:
//! 1. Signs the input with `sign_bytes`.
//! 2. Verifies the signature with `verify_bytes` (must succeed).
//! 3. Tampers one byte of the message and verifies again (must fail).
//! 4. Tampers one byte of the signature and verifies (must fail or
//!    error, never wrongly accept).

#![no_main]

use libfuzzer_sys::fuzz_target;

use crypto::signer_backend::{MlDsa65Signer, SignerBackend};

use std::cell::RefCell;

thread_local! {
    static SIGNER: RefCell<MlDsa65Signer> = RefCell::new(MlDsa65Signer::generate());
}

fuzz_target!(|data: &[u8]| {
    SIGNER.with(|cell| {
        let signer = cell.borrow();
        let verifier = signer.verifier();

        // sign_bytes must not panic for any input.
        let signature = match signer.sign_bytes(data) {
            Ok(sig) => sig,
            Err(_) => return,
        };

        // Verification of a fresh signature must succeed.
        let ok = verifier
            .verify_bytes(data, &signature)
            .expect("verify_bytes must not panic");
        assert!(ok, "freshly-signed message must verify");

        // Tampered message must not verify.
        if !data.is_empty() {
            let mut tampered = data.to_vec();
            tampered[0] ^= 0x01;
            let bad = verifier
                .verify_bytes(&tampered, &signature)
                .expect("verify_bytes must not panic on tampered message");
            assert!(!bad, "tampered message must fail verification");
        }

        // Tampered signature must not wrongly verify (it may return
        // Ok(false) or Err, but never Ok(true)).
        if !signature.is_empty() {
            let mut bad_sig = signature.clone();
            bad_sig[0] ^= 0x01;
            if let Ok(accepted) = verifier.verify_bytes(data, &bad_sig) {
                assert!(!accepted, "tampered signature must not verify");
            }
        }
    });
});
