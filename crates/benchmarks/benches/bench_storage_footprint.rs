//! `bench_storage_footprint` — encrypted on-disk size vs row count.
//!
//! Ingests N = 1K / 10K / 100K / 500K realistic messages into fresh
//! SQLCipher stores and reports the resulting database file size and
//! the derived bytes-per-message at each scale.
//!
//! Unlike the latency suites, the headline deliverable here is a
//! **size**, not a wall-clock time. Each store is built exactly once
//! in setup (the WAL is checkpointed by dropping the connection
//! before measuring), the footprint is printed to stderr as
//! `STORAGE_FOOTPRINT N=… file_bytes=… bytes_per_msg=…`, and the
//! Criterion-timed operation is the cheap `metadata().len()` read so
//! the harness still produces a stable report. Read the printed
//! lines (or `docs/technical/benchmarks.md`) for the footprint table.
//!
//! Run with:
//!
//! ```bash
//! cargo bench -p benchmarks --bench bench_storage_footprint 2>&1 | grep STORAGE_FOOTPRINT
//! ```

use std::path::{Path, PathBuf};
use std::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use std::hint::black_box;
use tempfile::TempDir;

use benchmarks::{importance_for, realistic_messages};
use evidence_store::{EvidenceStore, EvidenceStoreConfig, ScopeId};

const MASTER_KEY: [u8; 32] = [0xA5; 32];

/// Row counts swept by the footprint measurement.
const SCALES: &[(&str, usize)] = &[
    ("1K", 1_000),
    ("10K", 10_000),
    ("100K", 100_000),
    ("500K", 500_000),
];

/// Sum the size of the SQLCipher database file plus any sibling WAL /
/// SHM files still on disk.
fn db_footprint_bytes(path: &Path) -> u64 {
    let mut total = 0u64;
    for suffix in ["", "-wal", "-shm"] {
        let p = if suffix.is_empty() {
            path.to_path_buf()
        } else {
            let mut s = path.to_path_buf().into_os_string();
            s.push(suffix);
            PathBuf::from(s)
        };
        if let Ok(meta) = std::fs::metadata(&p) {
            total += meta.len();
        }
    }
    total
}

/// Build one store, ingest `n` rows, checkpoint by dropping the
/// connection, and return `(tempdir, db_path, footprint_bytes)`.
fn build_and_measure(n: usize) -> (TempDir, PathBuf, u64) {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("footprint.db");
    {
        let mut store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default())
            .expect("open evidence store");
        let scope = ScopeId::new_v4();
        let messages = realistic_messages(n);
        for (i, msg) in messages.iter().enumerate() {
            store
                .ingest(
                    scope,
                    msg.as_bytes(),
                    Some("bench:footprint"),
                    importance_for(i),
                )
                .expect("ingest");
        }
        // `store` drops here, checkpointing the WAL into the main file.
    }
    let bytes = db_footprint_bytes(&path);
    let per_msg = bytes / (n as u64);
    eprintln!("STORAGE_FOOTPRINT N={n} file_bytes={bytes} bytes_per_msg={per_msg}");
    (dir, path, bytes)
}

fn bench_storage_footprint(c: &mut Criterion) {
    // Build every scale once, keeping the TempDirs alive so the
    // backing files survive for the metadata reads below.
    let measured: Vec<(&str, usize, TempDir, PathBuf, u64)> = SCALES
        .iter()
        .map(|&(label, n)| {
            let (dir, path, bytes) = build_and_measure(n);
            (label, n, dir, path, bytes)
        })
        .collect();

    let mut group = c.benchmark_group("storage/footprint");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(5));

    for (label, _n, _dir, path, _bytes) in &measured {
        group.bench_with_input(BenchmarkId::from_parameter(label), path, |b, path| {
            b.iter(|| {
                let bytes = db_footprint_bytes(black_box(path));
                black_box(bytes);
            });
        });
    }
    group.finish();
}

criterion_group!(storage_benches, bench_storage_footprint);
criterion_main!(storage_benches);
