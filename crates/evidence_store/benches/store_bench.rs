//! Criterion benchmarks for the `evidence_store` ingest / read /
//! search / ring-buffer hot paths.
//!
//! Each bench opens a fresh SQLCipher database in a `tempfile::TempDir`
//! so the SQLCipher page-encryption cost (the dominant non-AEAD cost)
//! is included in the measurement.
//!
//! Run with:
//!
//! ```bash
//! cargo bench -p evidence_store
//! cargo bench -p evidence_store -- ingest_inline
//! ```

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use tempfile::TempDir;

use evidence_store::{
    EvidenceStore, EvidenceStoreConfig, ImportanceClass, ScopeId, DEFAULT_INLINE_THRESHOLD_BYTES,
};

const MASTER_KEY: [u8; 32] = [0xA5; 32];

/// `Important` body that fits inline (≤ 512 bytes).
const INLINE_BODY_SIZE: usize = 100;
/// `Important` body that overflows to the body table.
const BODY_TABLE_BODY_SIZE: usize = 10 * 1024;
/// `Noise` body that lands in the ring buffer.
const RING_BUFFER_BODY_SIZE: usize = 256;
/// How many FTS docs to ingest before benching `search_fts`.
const FTS_CORPUS_SIZE: usize = 1_000;

fn fresh_store() -> (TempDir, EvidenceStore) {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("bench.db");
    let store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default())
        .expect("open evidence store");
    (dir, store)
}

fn payload(size: usize, seed: u8) -> Vec<u8> {
    let mut buf = vec![0u8; size];
    for (i, byte) in buf.iter_mut().enumerate() {
        // Deterministic non-zero pattern; modulo 251 (< 256) so the
        // value always fits in u8 without truncation.
        *byte = u8::try_from(i % 251).unwrap_or(0).wrapping_add(seed);
    }
    buf
}

fn bench_ingest_inline(c: &mut Criterion) {
    const _: () = assert!(INLINE_BODY_SIZE <= DEFAULT_INLINE_THRESHOLD_BYTES);
    let scope = ScopeId::new_v4();
    let body = payload(INLINE_BODY_SIZE, 0x11);

    let mut group = c.benchmark_group("evidence_store/ingest_inline");
    group.throughput(Throughput::Bytes(INLINE_BODY_SIZE as u64));
    group.bench_function("100B_Important", |b| {
        // One fresh store per iter so each ingest sees an empty
        // table — keeps the measurement free of monotonic FTS-bloat.
        b.iter_with_setup(fresh_store, |(_dir, mut store)| {
            let res = store
                .ingest(
                    black_box(scope),
                    black_box(&body),
                    Some("bench:inline"),
                    ImportanceClass::Important,
                )
                .expect("ingest_inline");
            black_box(res);
        });
    });
    group.finish();
}

fn bench_ingest_body_table(c: &mut Criterion) {
    const _: () = assert!(BODY_TABLE_BODY_SIZE > DEFAULT_INLINE_THRESHOLD_BYTES);
    let scope = ScopeId::new_v4();
    let body = payload(BODY_TABLE_BODY_SIZE, 0x22);

    let mut group = c.benchmark_group("evidence_store/ingest_body_table");
    group.throughput(Throughput::Bytes(BODY_TABLE_BODY_SIZE as u64));
    group.bench_function("10KB_Important", |b| {
        b.iter_with_setup(fresh_store, |(_dir, mut store)| {
            let res = store
                .ingest(
                    black_box(scope),
                    black_box(&body),
                    Some("bench:body-table"),
                    ImportanceClass::Important,
                )
                .expect("ingest_body_table");
            black_box(res);
        });
    });
    group.finish();
}

fn bench_read_body(c: &mut Criterion) {
    let scope = ScopeId::new_v4();
    let body = payload(BODY_TABLE_BODY_SIZE, 0x33);
    let (_dir, mut store) = fresh_store();
    let res = store
        .ingest(scope, &body, Some("bench:read"), ImportanceClass::Important)
        .expect("ingest");
    let evidence_id = res.evidence_id;

    let mut group = c.benchmark_group("evidence_store/read_body");
    group.throughput(Throughput::Bytes(BODY_TABLE_BODY_SIZE as u64));
    group.bench_function("10KB_Important", |b| {
        b.iter(|| {
            let pt = store.read_body(black_box(evidence_id)).expect("read_body");
            black_box(pt);
        });
    });
    group.finish();
}

fn bench_search_fts(c: &mut Criterion) {
    let scope = ScopeId::new_v4();
    let (_dir, mut store) = fresh_store();
    for i in 0..FTS_CORPUS_SIZE {
        // Mix a unique token + a shared token so the bench measures
        // both selective and selective-but-multi-hit FTS queries.
        let body = format!("alpha bravo charlie unique-token-{i} migration deadline channel-recap");
        store
            .ingest(
                scope,
                body.as_bytes(),
                Some("bench:fts"),
                ImportanceClass::Important,
            )
            .expect("ingest");
    }

    let mut group = c.benchmark_group("evidence_store/search_fts");
    group.bench_function("selective_unique_token", |b| {
        b.iter(|| {
            let hits = store
                .search_fts(black_box(scope), black_box("unique-token-42"), 10)
                .expect("search_fts");
            black_box(hits);
        });
    });
    group.bench_function("common_token_top_10", |b| {
        b.iter(|| {
            let hits = store
                .search_fts(black_box(scope), black_box("migration"), 10)
                .expect("search_fts");
            black_box(hits);
        });
    });
    group.finish();
}

fn bench_ring_buffer_insert(c: &mut Criterion) {
    let scope = ScopeId::new_v4();
    let body = payload(RING_BUFFER_BODY_SIZE, 0x44);

    let mut group = c.benchmark_group("evidence_store/ring_buffer_insert");
    group.throughput(Throughput::Bytes(RING_BUFFER_BODY_SIZE as u64));
    group.bench_function("256B_Noise", |b| {
        b.iter_with_setup(fresh_store, |(_dir, mut store)| {
            store
                .ring_buffer_insert(black_box(scope), black_box(&body))
                .expect("ring_buffer_insert");
        });
    });
    group.finish();
}

criterion_group!(
    store_benches,
    bench_ingest_inline,
    bench_ingest_body_table,
    bench_read_body,
    bench_search_fts,
    bench_ring_buffer_insert,
);
criterion_main!(store_benches);
