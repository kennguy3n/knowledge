//! `bench_crypto_operations` — substrate cryptographic primitives.
//!
//! Covers the operations on the encrypt-on-ingest / sign-on-publish
//! hot paths:
//!
//! * **AEAD encrypt / decrypt** (XChaCha20-Poly1305) at 512 B, 4 KB,
//!   64 KB, and 1 MB — the body-size sweep bracketing inline rows,
//!   body-table documents, and offline-archive segments.
//! * **Hybrid KEM** (X25519 + ML-KEM-768) encapsulation /
//!   decapsulation — the post-quantum key-agreement wire shape.
//! * **ML-DSA-65 sign / verify** — the lattice half of the hybrid
//!   signature.
//! * **SPHINCS+-SHAKE-128f sign / verify** — the hash-based half.
//!
//! Run with:
//!
//! ```bash
//! cargo bench -p benchmarks --bench bench_crypto_operations
//! ```

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::hint::black_box;
use std::time::Duration;

use crypto::signer_backend::{MlDsa65Signer, SignerBackend};
use crypto::sphincs::SphincsPlusSigner;
use crypto::{
    decrypt_aead, encrypt_aead, hybrid_kem_decap, hybrid_kem_encap, hybrid_keypair, AeadKey,
    AeadNonce, AEAD_KEY_LEN, AEAD_NONCE_LEN,
};

/// Plaintext sizes swept by the AEAD benches: 512 B, 4 KB, 64 KB,
/// 1 MB. These bracket the realistic body-size distribution the
/// `evidence_store` ingest path sees.
const PAYLOAD_SIZES: &[usize] = &[512, 4 << 10, 64 << 10, 1 << 20];

fn fixed_key() -> AeadKey {
    let mut k = [0u8; AEAD_KEY_LEN];
    for (i, byte) in k.iter_mut().enumerate() {
        *byte = u8::try_from(i).unwrap_or(0xAA);
    }
    k
}

fn fixed_nonce() -> AeadNonce {
    let mut n = [0u8; AEAD_NONCE_LEN];
    for (i, byte) in n.iter_mut().enumerate() {
        *byte = u8::try_from(i).unwrap_or(0x5A).wrapping_mul(0x11);
    }
    n
}

fn payload(size: usize) -> Vec<u8> {
    let mut buf = vec![0u8; size];
    for (i, byte) in buf.iter_mut().enumerate() {
        // Modulus is a prime < 256 so every byte fits without
        // truncation; non-zero pattern avoids CPU short-circuits.
        *byte = u8::try_from(i % 251).unwrap_or(0);
    }
    buf
}

/// Fallible `usize -> u64` for `Throughput::Bytes`, keeping the cast
/// within the workspace's `cast_possible_truncation = "deny"` policy.
fn bytes_for(size: usize) -> u64 {
    u64::try_from(size).expect("payload size fits in u64")
}

fn bench_aead(c: &mut Criterion) {
    let key = fixed_key();
    let nonce = fixed_nonce();
    let aad = b"bench:aad";

    let mut encrypt = c.benchmark_group("crypto/aead/encrypt");
    for &size in PAYLOAD_SIZES {
        let pt = payload(size);
        encrypt.throughput(Throughput::Bytes(bytes_for(size)));
        encrypt.bench_with_input(BenchmarkId::from_parameter(size), &pt, |b, pt| {
            b.iter(|| {
                let ct = encrypt_aead(black_box(&key), black_box(&nonce), black_box(pt), aad)
                    .expect("encrypt must not fail with a valid key/nonce");
                black_box(ct);
            });
        });
    }
    encrypt.finish();

    let mut decrypt = c.benchmark_group("crypto/aead/decrypt");
    for &size in PAYLOAD_SIZES {
        let pt = payload(size);
        let ct = encrypt_aead(&key, &nonce, &pt, aad).expect("encrypt");
        decrypt.throughput(Throughput::Bytes(bytes_for(size)));
        decrypt.bench_with_input(BenchmarkId::from_parameter(size), &ct, |b, ct| {
            b.iter(|| {
                let pt = decrypt_aead(black_box(&key), black_box(&nonce), black_box(ct), aad)
                    .expect("decrypt must succeed on freshly-encrypted ciphertext");
                black_box(pt);
            });
        });
    }
    decrypt.finish();
}

fn bench_hybrid_kem(c: &mut Criterion) {
    let (pk, sk) = hybrid_keypair().expect("hybrid keypair generation");
    let (_ss, ct) = hybrid_kem_encap(&pk).expect("encap");

    let mut group = c.benchmark_group("crypto/hybrid_kem");
    group.bench_function("encap", |b| {
        b.iter(|| {
            let (ss, ct) = hybrid_kem_encap(black_box(&pk)).expect("encap");
            black_box((ss, ct));
        });
    });
    group.bench_function("decap", |b| {
        b.iter(|| {
            let ss = hybrid_kem_decap(black_box(&sk), black_box(&ct)).expect("decap");
            black_box(ss);
        });
    });
    group.finish();
}

fn bench_ml_dsa(c: &mut Criterion) {
    let signer = MlDsa65Signer::generate();
    let verifier = signer.verifier();
    let msg = b"ml-dsa-65 canonical provenance bundle bytes";
    let sig = signer.sign_bytes(msg).expect("sign");

    let mut group = c.benchmark_group("crypto/ml_dsa_65");
    group.bench_function("sign", |b| {
        b.iter(|| {
            let sig = signer.sign_bytes(black_box(msg)).expect("sign");
            black_box(sig);
        });
    });
    group.bench_function("verify", |b| {
        b.iter(|| {
            let ok = verifier
                .verify_bytes(black_box(msg), black_box(&sig))
                .expect("verify_bytes");
            assert!(ok, "freshly-signed message must verify");
        });
    });
    group.finish();
}

fn bench_sphincs(c: &mut Criterion) {
    let signer = SphincsPlusSigner::generate();
    let verifier = signer.verifier();
    let msg = b"sphincs+ canonical provenance bundle bytes";
    let sig = signer.sign_bytes(msg).expect("sign");

    let mut group = c.benchmark_group("crypto/sphincs_plus");
    // SPHINCS+ sign is intentionally slow (~tens of ms); cap the
    // sample size so the suite finishes in a sane wall-clock window.
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(20));
    group.bench_function("sign", |b| {
        b.iter(|| {
            let sig = signer.sign_bytes(black_box(msg)).expect("sign");
            black_box(sig);
        });
    });
    group.bench_function("verify", |b| {
        b.iter(|| {
            let ok = verifier
                .verify_bytes(black_box(msg), black_box(&sig))
                .expect("verify_bytes");
            assert!(ok, "freshly-signed message must verify");
        });
    });
    group.finish();
}

criterion_group!(
    crypto_benches,
    bench_aead,
    bench_hybrid_kem,
    bench_ml_dsa,
    bench_sphincs,
);
criterion_main!(crypto_benches);
