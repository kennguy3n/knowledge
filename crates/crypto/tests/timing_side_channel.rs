//! Statistical timing side-channel check for AEAD encryption.
//!
//! **Important:** This is a *statistical* check, not a formal timing
//! analysis. It asserts that the per-plaintext compute time of
//! `encrypt_aead` does not vary with plaintext *content* (all inputs
//! share a fixed length), by checking the coefficient of variation
//! (CoV = σ / μ) of those times stays below 15%.
//!
//! ## Robustness to environmental noise
//!
//! A naive approach — timing each plaintext once and taking the CoV of
//! those single-shot wall-clock samples — does **not** measure what we
//! want: single-shot wall-clock time is dominated by OS scheduling,
//! interrupts, CPU-frequency scaling, and contention from other tests
//! running in parallel (`cargo test --all`). That noise is unrelated to
//! the cipher and makes the metric flaky under load while *not* actually
//! tracking data-dependent behaviour.
//!
//! Instead, following the dudect methodology, we time each plaintext
//! `INNER_REPEATS` times and keep the **minimum**. Noise can only ever
//! *add* time, so the minimum is the cleanest estimate of the true
//! compute time for that input. CoV is then taken over the per-plaintext
//! minima: a genuine data-dependent branch would still show up (the min
//! time for some inputs would differ), but scheduler jitter is removed.
//!
//! A passing result provides *evidence* of constant-time behaviour at
//! the Rust/OS level but does NOT constitute a formal guarantee.
//! Microarchitectural side channels (cache timing, branch prediction,
//! speculative execution) are outside the scope of this test.
//!
//! The test uses `std::time::Instant` for measurements.

use std::hint::black_box;
use std::time::Instant;

use crypto::{encrypt_aead, AeadKey, AeadNonce, AEAD_KEY_LEN, AEAD_NONCE_LEN};

/// Number of distinct same-length plaintexts whose compute times are
/// compared.
const ITERATIONS: usize = 256;
/// Repeated timings per plaintext; the minimum is kept to reject the
/// upward-only environmental noise (see module docs).
const INNER_REPEATS: usize = 32;
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
        let _ = black_box(encrypt_aead(&key, &nonce, black_box(pt), aad));
    }

    // Measure: for each plaintext, time `INNER_REPEATS` encryptions and
    // keep the minimum. Environmental noise (scheduling, interrupts,
    // frequency scaling, contention) can only *add* time, so the minimum
    // is the cleanest estimate of the input's true compute time. The CoV
    // is then taken over the per-plaintext minima — a data-dependent
    // branch would still surface as content-correlated min times, but
    // scheduler jitter is rejected. `black_box` prevents the optimiser
    // from hoisting or eliding the call across repeats.
    let mut durations_ns: Vec<f64> = Vec::with_capacity(ITERATIONS);
    for pt in &plaintexts {
        let mut best = u128::MAX;
        for _ in 0..INNER_REPEATS {
            let start = Instant::now();
            let ct = encrypt_aead(&key, &nonce, black_box(pt), aad).expect("encrypt");
            let elapsed = start.elapsed().as_nanos();
            black_box(ct);
            best = best.min(elapsed);
        }
        durations_ns.push(best as f64);
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
