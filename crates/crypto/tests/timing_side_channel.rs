//! Statistical timing side-channel check for AEAD encryption.
//!
//! **Important:** This is a *statistical* check, not a formal timing
//! analysis. It measures wall-clock variance of `encrypt_aead` across
//! 1 000 runs with different plaintexts of the same length and asserts
//! the coefficient of variation (CoV = σ / μ) stays below 15%.
//!
//! A passing result provides *evidence* of constant-time behaviour at
//! the Rust/OS level but does NOT constitute a formal guarantee.
//! Microarchitectural side channels (cache timing, branch prediction,
//! speculative execution) are outside the scope of this test.
//!
//! The test uses `std::time::Instant` for measurements.

use std::time::Instant;

use crypto::{encrypt_aead, AeadKey, AeadNonce, AEAD_KEY_LEN, AEAD_NONCE_LEN};

const ITERATIONS: usize = 1_000;
const PLAINTEXT_LEN: usize = 256;
const COV_THRESHOLD: f64 = 0.15; // 15% — generous for debug builds and CI VMs

fn fixed_key() -> AeadKey {
    [0x42; AEAD_KEY_LEN]
}

fn fixed_nonce() -> AeadNonce {
    [0x07; AEAD_NONCE_LEN]
}

#[test]
fn encrypt_timing_variance_below_threshold() {
    let key = fixed_key();
    let nonce = fixed_nonce();
    let aad = b"timing-test-aad";

    // Pre-generate distinct plaintexts of the same length.
    let plaintexts: Vec<Vec<u8>> = (0..ITERATIONS)
        .map(|i| {
            let mut pt = vec![0u8; PLAINTEXT_LEN];
            // Fill with a pattern derived from the iteration index so
            // each plaintext differs while keeping the length constant.
            for (j, byte) in pt.iter_mut().enumerate() {
                #[allow(clippy::cast_possible_truncation)]
                {
                    *byte = ((i + j) & 0xFF) as u8;
                }
            }
            pt
        })
        .collect();

    // Warm up: run 100 encryptions to stabilise caches / TLB.
    for pt in &plaintexts[..100.min(plaintexts.len())] {
        let _ = encrypt_aead(&key, &nonce, pt, aad);
    }

    // Measure.
    let mut durations_ns: Vec<f64> = Vec::with_capacity(ITERATIONS);
    for pt in &plaintexts {
        let start = Instant::now();
        let _ = encrypt_aead(&key, &nonce, pt, aad).expect("encrypt");
        let elapsed = start.elapsed();
        durations_ns.push(elapsed.as_nanos() as f64);
    }

    // Compute statistics.
    let n = durations_ns.len() as f64;
    let mean = durations_ns.iter().sum::<f64>() / n;
    let variance = durations_ns.iter().map(|d| (d - mean).powi(2)).sum::<f64>() / n;
    let stddev = variance.sqrt();
    let cov = if mean > 0.0 { stddev / mean } else { 0.0 };

    eprintln!(
        "timing_side_channel: mean={:.0}ns, stddev={:.0}ns, CoV={:.4} (threshold={:.2})",
        mean, stddev, cov, COV_THRESHOLD
    );

    assert!(
        cov < COV_THRESHOLD,
        "Coefficient of variation {cov:.4} exceeds threshold {COV_THRESHOLD}. \
         mean={mean:.0}ns stddev={stddev:.0}ns. \
         This may indicate data-dependent timing in the encrypt path."
    );
}
