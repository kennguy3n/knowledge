//! Criterion benchmarks for the hot `crypto` crate primitives.
//!
//! Covers:
//!
//! * AEAD encrypt / decrypt at 1 KB, 64 KB, and 1 MB plaintext sizes
//!   — the size sweep that pins the substrate's per-byte
//!   XChaCha20-Poly1305 throughput.
//! * Hybrid X25519 + ML-KEM-768 encap / decap (one fixed shape, the
//!   wire shape is fixed by the spec).
//! * BLAKE3 content hashing at the same 1 KB / 64 KB / 1 MB sweep.
//! * HKDF-SHA256 key derivation through `derive_key`.
//! * ML-DSA-65 sign / verify and SPHINCS+-SHAKE-128f-simple sign /
//!   verify — the two halves of the substrate's hybrid signature
//!   path.
//!
//! Run with:
//!
//! ```bash
//! cargo bench -p crypto
//! cargo bench -p crypto -- aead          # filter by name
//! ```
//!
//! HTML reports land in `target/criterion/`.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use crypto::signer_backend::{MlDsa65Signer, SignerBackend};
use crypto::sphincs::SphincsPlusSigner;
use crypto::{
    content_hash, decrypt_aead, derive_key, encrypt_aead, hybrid_kem_decap, hybrid_kem_encap,
    hybrid_keypair, AeadKey, AeadNonce, AEAD_KEY_LEN, AEAD_NONCE_LEN, MASTER_KEY_LEN,
};

/// Plaintext sizes swept by the AEAD / hash benchmarks: 1 KB, 64 KB,
/// and 1 MB. These bracket the realistic body-size distribution the
/// `evidence_store` ingest path sees — short messages inline, large
/// document bodies in the body table, and the upper-bound an offline
/// archive segment can pull in a single AEAD frame.
const PAYLOAD_SIZES: &[usize] = &[1 << 10, 64 << 10, 1 << 20];

fn fixed_key() -> AeadKey {
    let mut k = [0u8; AEAD_KEY_LEN];
    for (i, byte) in k.iter_mut().enumerate() {
        // `i` is bounded by AEAD_KEY_LEN (= 32) so this `try_from`
        // never returns Err. The fallible conversion keeps us within
        // the crypto crate's `cast_possible_truncation = "deny"`
        // lint policy.
        *byte = u8::try_from(i).unwrap_or(0xAA);
    }
    k
}

fn fixed_nonce() -> AeadNonce {
    let mut n = [0u8; AEAD_NONCE_LEN];
    for (i, byte) in n.iter_mut().enumerate() {
        // `i` is bounded by AEAD_NONCE_LEN (= 24).
        *byte = u8::try_from(i).unwrap_or(0x5A).wrapping_mul(0x11);
    }
    n
}

fn payload(size: usize) -> Vec<u8> {
    // Deterministic, non-zero pattern — pure-zero buffers can hit
    // compiler / CPU short-circuits and skew bench numbers. The
    // modulus is a prime < 256 so every byte fits in `u8` without
    // truncation.
    let mut buf = vec![0u8; size];
    for (i, byte) in buf.iter_mut().enumerate() {
        *byte = u8::try_from(i % 251).unwrap_or(0);
    }
    buf
}

/// Convert a payload size to the `u64` Criterion's `Throughput::Bytes`
/// expects. The bench file lives in the `crypto` crate which sets
/// `cast_possible_truncation = "deny"`, so we use the fallible
/// conversion rather than a raw `as u64` cast.
fn bytes_for(size: usize) -> u64 {
    u64::try_from(size).expect("payload size fits in u64")
}

fn bench_encrypt_aead(c: &mut Criterion) {
    let key = fixed_key();
    let nonce = fixed_nonce();
    let aad = b"bench:aad";

    let mut group = c.benchmark_group("crypto/aead/encrypt");
    for &size in PAYLOAD_SIZES {
        let pt = payload(size);
        group.throughput(Throughput::Bytes(bytes_for(size)));
        group.bench_with_input(BenchmarkId::from_parameter(size), &pt, |b, pt| {
            b.iter(|| {
                let ct = encrypt_aead(black_box(&key), black_box(&nonce), black_box(pt), aad)
                    .expect("encrypt must not fail with a valid key/nonce");
                black_box(ct);
            });
        });
    }
    group.finish();
}

fn bench_decrypt_aead(c: &mut Criterion) {
    let key = fixed_key();
    let nonce = fixed_nonce();
    let aad = b"bench:aad";

    let mut group = c.benchmark_group("crypto/aead/decrypt");
    for &size in PAYLOAD_SIZES {
        let pt = payload(size);
        let ct = encrypt_aead(&key, &nonce, &pt, aad).expect("encrypt");
        group.throughput(Throughput::Bytes(bytes_for(size)));
        group.bench_with_input(BenchmarkId::from_parameter(size), &ct, |b, ct| {
            b.iter(|| {
                let pt = decrypt_aead(black_box(&key), black_box(&nonce), black_box(ct), aad)
                    .expect("decrypt must succeed on freshly-encrypted ciphertext");
                black_box(pt);
            });
        });
    }
    group.finish();
}

fn bench_hybrid_kem_encap(c: &mut Criterion) {
    let (pk, _sk) = hybrid_keypair().expect("hybrid keypair generation");
    c.bench_function("crypto/hybrid_kem/encap", |b| {
        b.iter(|| {
            let (ss, ct) = hybrid_kem_encap(black_box(&pk)).expect("encap");
            black_box((ss, ct));
        });
    });
}

fn bench_hybrid_kem_decap(c: &mut Criterion) {
    let (pk, sk) = hybrid_keypair().expect("hybrid keypair generation");
    let (_ss, ct) = hybrid_kem_encap(&pk).expect("encap");
    c.bench_function("crypto/hybrid_kem/decap", |b| {
        b.iter(|| {
            let ss = hybrid_kem_decap(black_box(&sk), black_box(&ct)).expect("decap");
            black_box(ss);
        });
    });
}

fn bench_content_hash(c: &mut Criterion) {
    let mut group = c.benchmark_group("crypto/content_hash");
    for &size in PAYLOAD_SIZES {
        let buf = payload(size);
        group.throughput(Throughput::Bytes(bytes_for(size)));
        group.bench_with_input(BenchmarkId::from_parameter(size), &buf, |b, buf| {
            b.iter(|| {
                let h = content_hash(black_box(buf));
                black_box(h);
            });
        });
    }
    group.finish();
}

fn bench_derive_key(c: &mut Criterion) {
    // 32-byte master key + a representative context string.
    let mut master_key = [0u8; MASTER_KEY_LEN];
    for (i, byte) in master_key.iter_mut().enumerate() {
        // `i` is bounded by MASTER_KEY_LEN (= 32).
        *byte = u8::try_from(i).unwrap_or(0xC3);
    }
    let context = b"scope:00000000-0000-0000-0000-000000000001:body:v1";
    c.bench_function("crypto/kdf/derive_key", |b| {
        b.iter(|| {
            let k = derive_key(black_box(&master_key), black_box(context.as_slice()))
                .expect("derive_key");
            black_box(k);
        });
    });
}

fn bench_ml_dsa_sign(c: &mut Criterion) {
    let signer = MlDsa65Signer::generate();
    let msg = b"ml-dsa-65 sign benchmark canonical bundle bytes";
    c.bench_function("crypto/ml_dsa_65/sign", |b| {
        b.iter(|| {
            let sig = signer.sign_bytes(black_box(msg)).expect("sign");
            black_box(sig);
        });
    });
}

fn bench_ml_dsa_verify(c: &mut Criterion) {
    let signer = MlDsa65Signer::generate();
    let verifier = signer.verifier();
    let msg = b"ml-dsa-65 verify benchmark canonical bundle bytes";
    let sig = signer.sign_bytes(msg).expect("sign");
    c.bench_function("crypto/ml_dsa_65/verify", |b| {
        b.iter(|| {
            let ok = verifier
                .verify_bytes(black_box(msg), black_box(&sig))
                .expect("verify_bytes");
            assert!(ok, "freshly-signed message must verify");
        });
    });
}

fn bench_sphincs_sign(c: &mut Criterion) {
    let signer = SphincsPlusSigner::generate();
    let msg = b"sphincs+ sign benchmark canonical bundle bytes";
    // SPHINCS+ sign is intentionally slow (~tens of ms). Capping
    // sample size keeps `cargo bench` runtime sane.
    let mut group = c.benchmark_group("crypto/sphincs_plus");
    group.sample_size(20);
    group.bench_function("sign", |b| {
        b.iter(|| {
            let sig = signer.sign_bytes(black_box(msg)).expect("sign");
            black_box(sig);
        });
    });
    group.finish();
}

fn bench_sphincs_verify(c: &mut Criterion) {
    let signer = SphincsPlusSigner::generate();
    let verifier = signer.verifier();
    let msg = b"sphincs+ verify benchmark canonical bundle bytes";
    let sig = signer.sign_bytes(msg).expect("sign");
    let mut group = c.benchmark_group("crypto/sphincs_plus");
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
    bench_encrypt_aead,
    bench_decrypt_aead,
    bench_hybrid_kem_encap,
    bench_hybrid_kem_decap,
    bench_content_hash,
    bench_derive_key,
    bench_ml_dsa_sign,
    bench_ml_dsa_verify,
    bench_sphincs_sign,
    bench_sphincs_verify,
);
criterion_main!(crypto_benches);
