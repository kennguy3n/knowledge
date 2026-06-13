//! `device_bench` — a portable, self-contained device-scale benchmark.
//!
//! The Criterion suites under `benches/` produce statistically rigorous
//! reports but assume a Criterion runner, write HTML/JSON trees under
//! `target/criterion/`, and take ~30 min for the full sweep. None of
//! that travels well onto a phone, a constrained laptop, or a CI lane
//! on Windows where you just want **one command** that prints **one
//! machine-readable result**.
//!
//! This binary fills that gap. It drives the *same* real substrate code
//! paths the Criterion benches exercise — encrypted-SQLCipher ingest,
//! FTS5 query, the three-lane [`HybridRetriever`], and the retention
//! [`decay_sweep`] — using the *same* deterministic workload generators
//! from the `benchmarks` library (no `rand`, no wall-clock seeding), and
//! emits a single JSON document on stdout plus a short human summary on
//! stderr. There is no mocking of the measured path: ingest writes real
//! encrypted rows, FTS runs real SQLite FTS5 queries, hybrid retrieval
//! runs the real lexical+recency+semantic fan-in (the only stand-in is
//! the deterministic [`MockEmbeddingModel`], exactly as the Criterion
//! hybrid bench uses, so the run needs no model file).
//!
//! It builds and runs unchanged on Linux, macOS (Apple Silicon), and
//! Windows. Peak RSS is read from `/proc/self/status` on Linux; on other
//! platforms the field is reported as `null` and is captured out-of-band
//! (Instruments / Task Manager) — see `docs/technical/benchmarks-device.md`.
//!
//! # Usage
//!
//! ```bash
//! # One command, sensible defaults (~1 min), JSON to stdout:
//! cargo run -p benchmarks --release --bin device_bench
//!
//! # Fast smoke run (a few seconds):
//! cargo run -p benchmarks --release --bin device_bench -- --quick
//!
//! # Scale the corpora up for a heavier capture:
//! cargo run -p benchmarks --release --bin device_bench -- \
//!     --corpus 50000 --decay-rows 50000
//!
//! # Capture the machine-readable row only:
//! cargo run -p benchmarks --release --bin device_bench 2>/dev/null > device-row.json
//! ```

use std::time::{Duration, Instant};

use benchmarks::{importance_for, realistic_messages, MockEmbeddingModel};
use evidence_store::retrieval::{HybridRetriever, HybridWeights};
use evidence_store::{EvidenceStore, EvidenceStoreConfig, ScopeId};
use memory_manager::{decay_sweep, MemoryObject, SensitivityClass};
use serde::Serialize;
use tempfile::TempDir;

/// SQLCipher master key for the throwaway benchmark stores. Fixed so
/// the run is reproducible; the stores live in a `TempDir` and are
/// dropped on exit.
const MASTER_KEY: [u8; 32] = [0xA5; 32];

/// FTS / hybrid search result cap, matching the Criterion benches.
const SEARCH_LIMIT: usize = 20;

/// Hybrid-retrieval query, matching `bench_hybrid_retrieval`.
const RETRIEVAL_QUERY: &str = "migration deadline team launch";

/// The four FTS query shapes the substrate's retrieval surface issues,
/// pinned to tokens the deterministic corpus is guaranteed to contain
/// (matches `bench_fts_query_at_scale`).
const FTS_QUERIES: &[(&str, &str)] = &[
    ("exact", "migration"),
    ("phrase", "\"team decided\""),
    ("boolean_and", "team AND migration"),
    ("prefix_wildcard", "migrat*"),
];

/// Run-shaping knobs, with defaults tuned for a ~1 min single-command
/// run that still produces stable percentiles.
#[derive(Debug, Clone, Copy)]
struct Config {
    /// Number of messages ingested into the shared corpus that backs
    /// the ingest-throughput, FTS, and hybrid measurements.
    corpus: usize,
    /// Fresh-store single-message ingest samples (for p50/p95).
    single_ingest_samples: usize,
    /// Timed iterations per FTS query shape (for p50/p95/p99).
    fts_iters: usize,
    /// Timed iterations per hybrid-retrieval mode (median reported).
    retrieval_iters: usize,
    /// `MemoryObject` count for the decay sweep.
    decay_rows: usize,
    /// Timed full decay-sweep iterations (median reported).
    decay_iters: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            corpus: 25_000,
            single_ingest_samples: 200,
            fts_iters: 300,
            retrieval_iters: 150,
            decay_rows: 25_000,
            decay_iters: 25,
        }
    }
}

impl Config {
    /// A fast smoke configuration — verifies every path runs in a few
    /// seconds, not for publishable numbers.
    fn quick() -> Self {
        Self {
            corpus: 2_000,
            single_ingest_samples: 50,
            fts_iters: 50,
            retrieval_iters: 30,
            decay_rows: 2_000,
            decay_iters: 5,
        }
    }
}

/// Top-level machine-readable result document.
#[derive(Debug, Serialize)]
struct DeviceBenchReport {
    /// Schema version so downstream parsers can evolve safely.
    schema_version: u32,
    /// Emitting tool name.
    tool: &'static str,
    /// RFC 3339 capture timestamp (UTC).
    captured_at_utc: String,
    /// Host the numbers were measured on.
    host: HostInfo,
    /// The run-shaping knobs in effect.
    config: ConfigReport,
    /// The measured results.
    results: Results,
}

/// Identifying facts about the host, so a captured row is attributable
/// to the hardware that produced it.
#[derive(Debug, Serialize)]
struct HostInfo {
    /// `std::env::consts::OS` (e.g. `linux`, `macos`, `windows`).
    os: &'static str,
    /// `std::env::consts::ARCH` (e.g. `x86_64`, `aarch64`).
    arch: &'static str,
    /// Logical CPU count, when the platform reports it.
    logical_cores: Option<usize>,
    /// Total physical RAM in bytes, when the platform reports it.
    total_ram_bytes: Option<u64>,
}

/// Serializable mirror of [`Config`].
#[derive(Debug, Serialize)]
struct ConfigReport {
    corpus: usize,
    single_ingest_samples: usize,
    fts_iters: usize,
    retrieval_iters: usize,
    decay_rows: usize,
    decay_iters: usize,
}

/// The full measured result set.
#[derive(Debug, Serialize)]
struct Results {
    /// Ingest throughput + single-message latency.
    ingest: IngestResultRow,
    /// Per-shape FTS query latency.
    fts: Vec<FtsResultRow>,
    /// Three-lane hybrid-retrieval latency.
    hybrid_retrieval: HybridResultRow,
    /// Retention decay-sweep throughput.
    decay_sweep: DecayResultRow,
    /// Peak resident set size in bytes, when the platform reports it.
    peak_rss_bytes: Option<u64>,
}

/// Ingest throughput (amortised over the corpus) plus the cold-start
/// single-message latency distribution.
#[derive(Debug, Serialize)]
struct IngestResultRow {
    corpus_size: usize,
    total_secs: f64,
    msgs_per_sec: f64,
    amortized_us_per_msg: f64,
    single_msg_p50_us: f64,
    single_msg_p95_us: f64,
}

/// One FTS query shape's latency distribution.
#[derive(Debug, Serialize)]
struct FtsResultRow {
    query_label: &'static str,
    query: &'static str,
    iters: usize,
    p50_ms: f64,
    p95_ms: f64,
    p99_ms: f64,
}

/// The three retriever configurations' median latency. The `_us`
/// suffix on every field is a deliberate part of the machine-readable
/// JSON key (it encodes the microsecond unit for downstream parsers),
/// so the uniform postfix is intentional here.
#[derive(Debug, Serialize)]
#[allow(clippy::struct_field_names)]
struct HybridResultRow {
    fts_only_us: f64,
    semantic_only_us: f64,
    hybrid_us: f64,
}

/// Decay-sweep throughput and per-sweep latency.
#[derive(Debug, Serialize)]
struct DecayResultRow {
    rows: usize,
    iters: usize,
    per_sweep_p50_ms: f64,
    rows_per_sec: f64,
}

fn main() {
    let config = parse_args();

    eprintln!(
        "device_bench: os={} arch={} corpus={} (use --quick for a fast smoke run)",
        std::env::consts::OS,
        std::env::consts::ARCH,
        config.corpus,
    );

    let results = run(config);
    let report = DeviceBenchReport {
        schema_version: 1,
        tool: "device_bench",
        captured_at_utc: chrono::Utc::now().to_rfc3339(),
        host: host_info(),
        config: ConfigReport {
            corpus: config.corpus,
            single_ingest_samples: config.single_ingest_samples,
            fts_iters: config.fts_iters,
            retrieval_iters: config.retrieval_iters,
            decay_rows: config.decay_rows,
            decay_iters: config.decay_iters,
        },
        results,
    };

    print_human_summary(&report);

    // The single machine-readable artifact: pure JSON on stdout.
    match serde_json::to_string_pretty(&report) {
        Ok(json) => println!("{json}"),
        Err(err) => {
            eprintln!("device_bench: failed to serialize report: {err}");
            std::process::exit(1);
        }
    }
}

/// Drive every measured path and assemble the result set.
fn run(config: Config) -> Results {
    // One shared, populated, encrypted store backs ingest-throughput,
    // FTS, and hybrid retrieval — building the corpus once and timing
    // the build itself as the throughput measurement.
    let scope = ScopeId::new_v4();
    let dir = TempDir::new().expect("create benchmark tempdir");
    let path = dir.path().join("device_bench.db");
    let mut store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default())
        .expect("open evidence store");

    let messages = realistic_messages(config.corpus);

    eprintln!("device_bench: ingesting {} messages…", config.corpus);
    let ingest_start = Instant::now();
    for (i, msg) in messages.iter().enumerate() {
        store
            .ingest(
                scope,
                msg.as_bytes(),
                Some("bench:device"),
                importance_for(i),
            )
            .expect("ingest");
    }
    let ingest_elapsed = ingest_start.elapsed();

    let ingest = measure_ingest(config, &ingest_elapsed);

    eprintln!("device_bench: FTS query sweep…");
    let fts = measure_fts(&store, scope, config.fts_iters);

    eprintln!("device_bench: hybrid retrieval…");
    let hybrid_retrieval = measure_hybrid(&store, scope, config.retrieval_iters);

    eprintln!("device_bench: decay sweep…");
    let decay_sweep_row = measure_decay(config);

    let peak_rss_bytes = peak_rss_bytes();

    Results {
        ingest,
        fts,
        hybrid_retrieval,
        decay_sweep: decay_sweep_row,
        peak_rss_bytes,
    }
}

/// Assemble the ingest row: amortised throughput from the corpus build,
/// plus a fresh-store single-message latency distribution.
fn measure_ingest(config: Config, corpus_elapsed: &Duration) -> IngestResultRow {
    let total_secs = corpus_elapsed.as_secs_f64();
    let corpus_f = config.corpus as f64;
    let msgs_per_sec = if total_secs > 0.0 {
        corpus_f / total_secs
    } else {
        0.0
    };
    let amortized_us_per_msg = if config.corpus > 0 {
        corpus_elapsed.as_micros() as f64 / corpus_f
    } else {
        0.0
    };

    // Cold-start single-message latency: each sample ingests into a
    // freshly-opened, empty encrypted store, so the timed write path
    // (row encryption + insert + FTS index update) never sees FTS index
    // bloat from a populated corpus. The timer starts *after*
    // `EvidenceStore::open`, so schema bootstrap and key setup are
    // deliberately excluded; and because the store is opened with a raw
    // 32-byte key, SQLCipher skips PBKDF2 (see
    // `evidence_store::store::open_keyed_connection`), so there is no
    // key-derivation cost on that path to capture in the first place.
    //
    // Every sample uses `ImportanceClass::Important` (`importance_for(1)`),
    // so this deliberately characterises the FTS-indexed write path and
    // keeps code-path variance out of the latency distribution. It is not
    // a production-representative mix: lower-importance messages take the
    // cheaper noise ring-buffer path, which this metric does not exercise.
    let scope = ScopeId::new_v4();
    let body = realistic_messages(1).pop().expect("one message");
    let mut samples_ns = Vec::with_capacity(config.single_ingest_samples);
    for _ in 0..config.single_ingest_samples {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("single_ingest.db");
        let mut store = EvidenceStore::open(&path, &MASTER_KEY, EvidenceStoreConfig::default())
            .expect("open evidence store");
        let start = Instant::now();
        store
            .ingest(
                scope,
                body.as_bytes(),
                Some("bench:single"),
                importance_for(1),
            )
            .expect("ingest");
        samples_ns.push(start.elapsed().as_nanos());
    }
    samples_ns.sort_unstable();

    IngestResultRow {
        corpus_size: config.corpus,
        total_secs,
        msgs_per_sec,
        amortized_us_per_msg,
        single_msg_p50_us: ns_to_us(percentile(&samples_ns, 50)),
        single_msg_p95_us: ns_to_us(percentile(&samples_ns, 95)),
    }
}

/// Time each FTS query shape `iters` times and report p50/p95/p99.
fn measure_fts(store: &EvidenceStore, scope: ScopeId, iters: usize) -> Vec<FtsResultRow> {
    FTS_QUERIES
        .iter()
        .map(|&(query_label, query)| {
            // Warm the page cache / query plan before timing.
            let _ = store
                .search_fts(scope, query, SEARCH_LIMIT)
                .expect("search_fts");
            let mut samples_ns = Vec::with_capacity(iters);
            for _ in 0..iters {
                let start = Instant::now();
                let hits = store
                    .search_fts(scope, query, SEARCH_LIMIT)
                    .expect("search_fts");
                samples_ns.push(start.elapsed().as_nanos());
                std::hint::black_box(hits.len());
            }
            samples_ns.sort_unstable();
            FtsResultRow {
                query_label,
                query,
                iters,
                p50_ms: ns_to_ms(percentile(&samples_ns, 50)),
                p95_ms: ns_to_ms(percentile(&samples_ns, 95)),
                p99_ms: ns_to_ms(percentile(&samples_ns, 99)),
            }
        })
        .collect()
}

/// Time the three retriever configurations and report each median.
///
/// All three lanes are measured *through the `HybridRetriever` surface*
/// so they are directly comparable: the `fts_only` lane calls
/// [`HybridRetriever::search_fts`] (the retriever's lexical-only path),
/// not [`EvidenceStore::search_fts`]. That is intentional and means
/// `fts_only_us` here is not the same measurement as the raw
/// `EvidenceStore::search_fts` cost reported by [`measure_fts`] — this
/// one includes the retriever's result-wrapping/scoring overhead.
fn measure_hybrid(store: &EvidenceStore, scope: ScopeId, iters: usize) -> HybridResultRow {
    let fts_only = HybridRetriever::new(store);
    // `semantic_only` zeroes only the FTS/recency *score* weights. The
    // underlying `search_hybrid` still runs FTS to *identify* candidate
    // rows before scoring them by vector similarity, so `semantic_only_us`
    // includes FTS candidate retrieval, not just embedding scoring — the
    // label means "only the semantic component contributes to the score",
    // not "only the semantic lane executes".
    let semantic_only = HybridRetriever::new(store)
        .with_embedding_model(MockEmbeddingModel::default(), "mock-v1")
        .with_weights(HybridWeights {
            fts: 0.0,
            recency: 0.0,
            vector: 1.0,
        });
    let hybrid =
        HybridRetriever::new(store).with_embedding_model(MockEmbeddingModel::default(), "mock-v1");

    // Each closure returns its hit set; `median_ns` applies the
    // `black_box` anti-elision fence *outside* the timed region, so the
    // timing boundary matches `measure_fts` exactly (the fence is never
    // counted toward the measured latency).
    let fts_only_us = ns_to_us(median_ns(iters, || {
        fts_only
            .search_fts(scope, RETRIEVAL_QUERY, SEARCH_LIMIT)
            .expect("search_fts")
    }));
    let semantic_only_us = ns_to_us(median_ns(iters, || {
        semantic_only
            .search_hybrid(scope, RETRIEVAL_QUERY, SEARCH_LIMIT)
            .expect("search_hybrid")
    }));
    let hybrid_us = ns_to_us(median_ns(iters, || {
        hybrid
            .search_hybrid(scope, RETRIEVAL_QUERY, SEARCH_LIMIT)
            .expect("search_hybrid")
    }));

    HybridResultRow {
        fts_only_us,
        semantic_only_us,
        hybrid_us,
    }
}

/// Build a realistic spread of `MemoryObject`s and time the full
/// retention `decay_sweep` over them.
fn measure_decay(config: Config) -> DecayResultRow {
    let base = build_decay_objects(config.decay_rows);
    let now = chrono::Utc::now();

    let mut samples_ns = Vec::with_capacity(config.decay_iters);
    for _ in 0..config.decay_iters {
        // Clone outside the timed region: `decay_sweep` mutates the
        // slice, so each iteration needs a fresh copy, but the clone
        // is setup cost, not sweep cost.
        let mut objects = base.clone();
        let start = Instant::now();
        let report = decay_sweep(&mut objects, now);
        let elapsed = start.elapsed().as_nanos();
        std::hint::black_box(report.scored);
        samples_ns.push(elapsed);
    }
    samples_ns.sort_unstable();

    let p50_ns = percentile(&samples_ns, 50);
    let per_sweep_p50_ms = ns_to_ms(p50_ns);
    let rows_f = config.decay_rows as f64;
    let p50_secs = p50_ns as f64 / 1e9;
    let rows_per_sec = if p50_secs > 0.0 {
        rows_f / p50_secs
    } else {
        0.0
    };

    DecayResultRow {
        rows: config.decay_rows,
        iters: config.decay_iters,
        per_sweep_p50_ms,
        rows_per_sec,
    }
}

/// Build `n` `MemoryObject`s spread across ages / access recency /
/// counters so the sweep exercises both the "still warm" and the
/// "archive a cold candidate" branches (mirrors `bench_decay_sweep`).
fn build_decay_objects(n: usize) -> Vec<MemoryObject> {
    let now = chrono::Utc::now();
    let scope = ScopeId::new_v4();
    (0..n)
        .map(|i| {
            let mut obj = MemoryObject::new_candidate(scope, sensitivity(i));
            obj.created_at = now - chrono::Duration::days(i64::try_from(i % 365).unwrap_or(0));
            obj.last_accessed_at =
                now - chrono::Duration::hours(i64::try_from(i % 720).unwrap_or(0));
            obj.retrieval_count = u32::try_from(i % 50).unwrap_or(0);
            obj.pin_count = u32::try_from(i % 5).unwrap_or(0);
            obj.corroboration_count = u32::try_from(i % 8).unwrap_or(0);
            obj
        })
        .collect()
}

/// Deterministic sensitivity class for the object at `index`.
fn sensitivity(index: usize) -> SensitivityClass {
    match index % 4 {
        0 => SensitivityClass::Critical,
        1 => SensitivityClass::Important,
        2 => SensitivityClass::Useful,
        _ => SensitivityClass::Noise,
    }
}

/// Run `op` `iters` times, returning the median wall-clock nanoseconds.
///
/// Only `op` itself is inside the timed region; the value it produces is
/// passed to `black_box` *after* the timer is read, so the anti-elision
/// fence never counts toward the measurement. This matches the timing
/// convention in [`measure_fts`], keeping the two measurement paths
/// consistent about where the timing boundary falls.
fn median_ns<T>(iters: usize, mut op: impl FnMut() -> T) -> u128 {
    // One warm-up pass to prime caches / lazy initialisation.
    std::hint::black_box(op());
    let mut samples = Vec::with_capacity(iters);
    for _ in 0..iters {
        let start = Instant::now();
        let out = op();
        samples.push(start.elapsed().as_nanos());
        std::hint::black_box(out);
    }
    samples.sort_unstable();
    percentile(&samples, 50)
}

/// Nearest-rank percentile over an already-sorted slice. `pct` is an
/// integer percentile in `1..=100`. Computed with integer arithmetic
/// so there is no float→int cast. Returns 0 for an empty slice.
fn percentile(sorted_ns: &[u128], pct: usize) -> u128 {
    if sorted_ns.is_empty() {
        return 0;
    }
    // 1-based nearest rank = ceil(pct * n / 100), clamped to [1, n].
    let n = sorted_ns.len();
    let rank = (pct * n).div_ceil(100).clamp(1, n);
    sorted_ns[rank - 1]
}

/// Nanoseconds → microseconds.
fn ns_to_us(ns: u128) -> f64 {
    ns as f64 / 1_000.0
}

/// Nanoseconds → milliseconds.
fn ns_to_ms(ns: u128) -> f64 {
    ns as f64 / 1_000_000.0
}

/// Gather host facts for attribution.
fn host_info() -> HostInfo {
    HostInfo {
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        logical_cores: std::thread::available_parallelism().ok().map(usize::from),
        total_ram_bytes: total_ram_bytes(),
    }
}

/// Peak resident set size in bytes.
///
/// On Linux this reads `VmHWM` ("high-water mark") from
/// `/proc/self/status`, which is the kernel's own peak-RSS accounting
/// and needs no `unsafe` and no extra dependency. On every other
/// platform it returns `None`; those rows capture peak memory
/// out-of-band (Instruments on macOS, Task Manager / `Get-Process` on
/// Windows) — see `docs/technical/benchmarks-device.md`.
#[cfg(target_os = "linux")]
fn peak_rss_bytes() -> Option<u64> {
    read_proc_status_kib("VmHWM:").map(|kib| kib * 1024)
}

#[cfg(not(target_os = "linux"))]
fn peak_rss_bytes() -> Option<u64> {
    None
}

/// Total physical RAM in bytes (Linux `/proc/meminfo` `MemTotal`).
#[cfg(target_os = "linux")]
fn total_ram_bytes() -> Option<u64> {
    read_meminfo_kib("MemTotal:").map(|kib| kib * 1024)
}

#[cfg(not(target_os = "linux"))]
fn total_ram_bytes() -> Option<u64> {
    None
}

/// Parse a `<key> <value> kB` line from `/proc/self/status`.
#[cfg(target_os = "linux")]
fn read_proc_status_kib(key: &str) -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    parse_kib_line(&status, key)
}

/// Parse a `<key> <value> kB` line from `/proc/meminfo`.
#[cfg(target_os = "linux")]
fn read_meminfo_kib(key: &str) -> Option<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    parse_kib_line(&meminfo, key)
}

/// Extract the kibibyte value from the first line in `text` starting
/// with `key` (lines look like `VmHWM:\t  123456 kB`).
#[cfg(target_os = "linux")]
fn parse_kib_line(text: &str, key: &str) -> Option<u64> {
    text.lines()
        .find(|line| line.starts_with(key))
        .and_then(|line| line[key.len()..].split_whitespace().next())
        .and_then(|value| value.parse::<u64>().ok())
}

/// Emit a compact, human-readable digest on stderr so an interactive
/// run is legible while stdout stays pure JSON.
fn print_human_summary(report: &DeviceBenchReport) {
    let r = &report.results;
    eprintln!("──────────────────────────────────────────────");
    eprintln!(
        "device_bench results  ({} / {})",
        report.host.os, report.host.arch
    );
    eprintln!("──────────────────────────────────────────────");
    eprintln!(
        "ingest        : {:.0} msgs/sec  ({:.1} µs/msg amortised, single-msg p50 {:.1} µs / p95 {:.1} µs)",
        r.ingest.msgs_per_sec,
        r.ingest.amortized_us_per_msg,
        r.ingest.single_msg_p50_us,
        r.ingest.single_msg_p95_us,
    );
    for f in &r.fts {
        eprintln!(
            "fts {:<15}: p50 {:.2} ms  p95 {:.2} ms  p99 {:.2} ms",
            f.query_label, f.p50_ms, f.p95_ms, f.p99_ms
        );
    }
    eprintln!(
        "hybrid        : fts-only {:.1} µs  semantic {:.1} µs  hybrid {:.1} µs",
        r.hybrid_retrieval.fts_only_us,
        r.hybrid_retrieval.semantic_only_us,
        r.hybrid_retrieval.hybrid_us,
    );
    eprintln!(
        "decay sweep   : p50 {:.2} ms  ({:.0} rows/sec over {} rows)",
        r.decay_sweep.per_sweep_p50_ms, r.decay_sweep.rows_per_sec, r.decay_sweep.rows,
    );
    match r.peak_rss_bytes {
        Some(bytes) => {
            let mib = bytes as f64 / (1024.0 * 1024.0);
            eprintln!("peak RSS      : {mib:.1} MiB");
        }
        None => eprintln!("peak RSS      : (capture out-of-band on this platform)"),
    }
    eprintln!("──────────────────────────────────────────────");
}

/// Parse the small set of CLI flags. Unknown flags abort with usage so
/// a typo never silently runs the default profile.
fn parse_args() -> Config {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // `--quick` selects the smoke-run baseline no matter where it
    // appears on the command line; the individual flags below then
    // override fields on top of whichever baseline is in effect. Doing
    // the selection in this pre-pass makes flag order irrelevant, so
    // both `--quick --corpus N` and `--corpus N --quick` yield the quick
    // baseline with `corpus = N`. (Scanning for the literal token is
    // safe: `--quick` takes no value, and value-flags like `--corpus`
    // reject a non-integer such as `--quick` via `parse_value`.)
    let mut config = if args.iter().any(|a| a == "--quick") {
        Config::quick()
    } else {
        Config::default()
    };
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            // Handled in the pre-pass above; accepted here so it is not
            // rejected as an unknown argument.
            "--quick" => {}
            "--corpus" => config.corpus = parse_value(&arg, args.next()),
            "--single-ingest-samples" => {
                config.single_ingest_samples = parse_value(&arg, args.next());
            }
            "--fts-iters" => config.fts_iters = parse_value(&arg, args.next()),
            "--retrieval-iters" => config.retrieval_iters = parse_value(&arg, args.next()),
            "--decay-rows" => config.decay_rows = parse_value(&arg, args.next()),
            "--decay-iters" => config.decay_iters = parse_value(&arg, args.next()),
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            other => {
                eprintln!("device_bench: unknown argument `{other}`\n");
                print_usage();
                std::process::exit(2);
            }
        }
    }
    config
}

/// Parse a required positive-integer flag value, aborting with a clear
/// message. Every knob `device_bench` accepts is a count of work to do
/// (corpus size, sample/iteration counts, decay rows), so `0` is never
/// meaningful: it would emit a report full of zeroed latencies with no
/// signal that the run was degenerate. Reject it up front.
fn parse_value(flag: &str, value: Option<String>) -> usize {
    let Some(value) = value else {
        eprintln!("device_bench: `{flag}` requires a value");
        std::process::exit(2);
    };
    let Ok(parsed) = value.parse::<usize>() else {
        eprintln!("device_bench: `{flag}` expects a positive integer, got `{value}`");
        std::process::exit(2);
    };
    if parsed == 0 {
        eprintln!("device_bench: `{flag}` must be greater than zero");
        std::process::exit(2);
    }
    parsed
}

/// Print CLI usage.
fn print_usage() {
    eprintln!(
        "device_bench — portable device-scale benchmark for the Knowledge substrate\n\
         \n\
         USAGE:\n\
         \u{20}   cargo run -p benchmarks --release --bin device_bench [-- FLAGS]\n\
         \n\
         FLAGS:\n\
         \u{20}   --quick                       fast smoke run (small corpora)\n\
         \u{20}   --corpus N                    shared ingest/FTS/hybrid corpus size\n\
         \u{20}   --single-ingest-samples N     fresh-store single-message samples\n\
         \u{20}   --fts-iters N                 timed iterations per FTS query shape\n\
         \u{20}   --retrieval-iters N           timed iterations per hybrid mode\n\
         \u{20}   --decay-rows N                MemoryObjects in the decay sweep\n\
         \u{20}   --decay-iters N               timed full decay-sweep iterations\n\
         \u{20}   -h, --help                    print this help\n\
         \n\
         Prints a JSON report on stdout and a human summary on stderr."
    );
}
