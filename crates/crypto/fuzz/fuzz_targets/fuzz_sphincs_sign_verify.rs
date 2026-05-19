//! Fuzz target: SPHINCS+ sign → verify round-trip.
//!
//! Generates a fresh SPHINCS+ keypair once (via `thread_local!`),
//! then for each fuzzed input:
//! 1. Signs the input with `sign_bytes`.
//! 2. Verifies the signature with `verify_bytes` (must succeed).
//! 3. Tampers one byte and verifies again (must fail).

#![no_main]

use libfuzzer_sys::fuzz_target;

use crypto::signer_backend::SignerBackend;
use crypto::sphincs::SphincsPlusSigner;

use std::cell::RefCell;

thread_local! {
    static SIGNER: RefCell<SphincsPlusSigner> = RefCell::new(SphincsPlusSigner::generate());
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
                .expect("verify_bytes must not panic on tampered input");
            assert!(!bad, "tampered message must fail verification");
        }
    });
});
