# Device-Scale Benchmarks

The substrate's product promise is **on-device** memory: ingest,
retrieval, and decay all run on the user's phone, laptop, or a
constrained edge device — no datacenter in the loop. This document is
the home for performance numbers measured **on real device-class
hardware**.

> **Why this file exists (and how it differs from
> [`benchmarks.md`](benchmarks.md)).** The published numbers in
> `benchmarks.md` were collected on an **AMD EPYC 7763 cloud VM (8
> vCPU, 31 GiB)** — server-class hardware. They are honest and useful
> as a *relative* regression baseline, but they are **not** phone
> numbers, and historically the on-device blog framing leaned on them
> as if they were. This file separates the two: it defines a portable
> benchmark that runs unchanged on a phone, an Apple-Silicon Mac, a
> Windows laptop, and a constrained 2–4 GB device, and records each
> result against the hardware that produced it. Rows we have not yet
> measured on physical hardware are explicitly marked
> **`[pending real-device measurement]`** rather than back-filled from
> the server VM.

## The portable benchmark (`device_bench`)

The Criterion suites under `crates/benchmarks/benches/` are excellent
for statistically rigorous, regression-tracked measurement, but they
assume a Criterion runner, write an HTML/JSON tree under
`target/criterion/`, and take ~30 min for the full sweep. That does not
travel onto a phone or into a one-shot CI lane.

`device_bench` is a self-contained binary in the `benchmarks` crate
that drives the **same real substrate code paths** and prints **one
machine-readable JSON document** with **one command**:

```bash
# Default profile (~30–60 s on a modern machine), JSON on stdout:
cargo run -p benchmarks --release --bin device_bench

# Fast smoke run (a few seconds) — verifies every path executes:
cargo run -p benchmarks --release --bin device_bench -- --quick

# Scale the corpora up/down (see --help for all flags):
cargo run -p benchmarks --release --bin device_bench -- \
    --corpus 50000 --decay-rows 50000

# Capture the machine-readable row only (drop the human summary):
cargo run -p benchmarks --release --bin device_bench 2>/dev/null > device-row.json
```

A human-readable digest is written to **stderr**; the **stdout** stream
is pure JSON so it can be redirected straight into a results file or a
CI artifact.

### What it measures, and how

Every measurement uses the deterministic workload generators in the
`benchmarks` library (`realistic_messages`, `importance_for`,
`MockEmbeddingModel`) — no `rand`, no wall-clock seeding — so the
inputs are byte-for-byte identical run-to-run and across platforms.
Nothing on the measured path is mocked: ingest writes real encrypted
SQLCipher rows, FTS runs real SQLite FTS5 queries, and hybrid retrieval
runs the real lexical + recency + semantic fan-in. The only stand-in is
the deterministic bag-of-words `MockEmbeddingModel` for the semantic
lane (identical to the Criterion `bench_hybrid_retrieval` harness), so
the run needs no ONNX model file on the device.

| Metric | Path exercised | Reported |
|---|---|---|
| **Ingest throughput** | Build a fresh encrypted store and ingest the full corpus (mixed language / importance) | msgs/sec + amortised µs/msg |
| **Single-message ingest** | One ingest into a brand-new store per sample (cold-start floor: SQLCipher key derivation + schema bootstrap) | p50 / p95 µs |
| **FTS query** | `search_fts` over the populated scope, four query shapes (exact, phrase, boolean-AND, prefix-wildcard) | p50 / p95 / p99 ms |
| **Hybrid retrieval** | `HybridRetriever` in three modes: FTS-only, semantic-only, full hybrid (FTS + semantic + recency rerank) | median µs |
| **Decay sweep** | `memory_manager::decay_sweep` over N `MemoryObject`s with a realistic age/recency/counter spread | per-sweep p50 ms + rows/sec |
| **Peak RSS** | Process high-water-mark resident memory after the full run | bytes (see capture note) |

**Percentiles** are computed with the nearest-rank method over the
per-iteration wall-clock samples (`p50`, `p95`, `p99`). Each timed loop
runs a warm-up iteration first to prime caches and lazy
initialisation.

**Peak RSS capture is platform-dependent.** On Linux the tool reads
`VmHWM` from `/proc/self/status` (the kernel's own peak-RSS accounting)
and reports it in the JSON `peak_rss_bytes` field. On macOS and Windows
that field is `null` and peak memory must be captured **out-of-band**:

- **macOS** — Instruments (*Allocations* / *Activity Monitor* "Memory"
  column), or `/usr/bin/time -l ./device_bench` (read *maximum resident
  set size*).
- **Windows** — Task Manager (*Peak working set*), or PowerShell:
  `(Get-Process device_bench).PeakWorkingSet64`.

## Device matrix

The reference Linux row below is **measured** by this session's run of
`device_bench` (default profile). Every other row is a **template** to
be filled in by running the identical command on the physical device
and pasting the JSON; until then it is marked
`[pending real-device measurement]`.

| Device class | Example hardware | RAM | Ingest (msgs/s) | FTS p50 / p95 (phrase) | Hybrid retrieval (median) | Decay sweep (p50) | Peak RSS | Status |
|---|---|---|---|---|---|---|---|---|
| **Linux cloud VM (reference)** | AMD EPYC 7763, 8 vCPU | 31 GiB | **1,685** | **2.19 / 2.27 ms** | **8.34 ms** (fts-only **181 µs**) | **1.28 ms** / 25K rows | **23.3 MiB** | ✅ measured (this run) |
| **iPhone (A-series)** | iPhone 14/15, A16/A17 | 6 GiB | — | — | — | — | (Instruments) | `[pending real-device measurement]` |
| **Mid-range Android** | Pixel 7a / Galaxy A54 | 6–8 GiB | — | — | — | — | — | `[pending real-device measurement]` |
| **Apple-Silicon Mac** | MacBook Air M2/M3 | 8–16 GiB | — | — | — | — | (`/usr/bin/time -l`) | `[pending real-device measurement]` |
| **Windows laptop** | x86-64 ultrabook | 16 GiB | — | — | — | — | (Peak working set) | `[pending real-device measurement]` |
| **Constrained device** | Budget Android / SBC | 2–4 GiB | — | — | — | — | — | `[pending real-device measurement]` |

> The constrained 2–4 GB tier should additionally be run against the
> low-memory store profile the substrate already ships
> (`EvidenceStoreConfig { memory_profile: MemoryProfile::Low, .. }` —
> 512 KiB SQLCipher page cache, mmap disabled), which the
> `device_profile_low_tier` Criterion bench exercises. A `device_bench`
> flag to select the memory profile is a natural follow-up once we have
> a real constrained device to measure on.

## Reference Linux row — full result

Captured on this run (`cargo run -p benchmarks --release --bin
device_bench`, default profile: 25,000-message shared corpus in a
single scope, 200 single-ingest samples, 300 FTS iters/shape, 150
retrieval iters/mode, 25,000 decay rows × 25 sweeps):

| Metric | Value |
|---|---|
| Ingest throughput | **1,685 msgs/sec** (593.5 µs/msg amortised) |
| Single-message ingest | p50 **498.5 µs**, p95 **592.6 µs** |
| FTS exact (`migration`) | p50 **3.18 ms**, p95 3.31 ms, p99 3.84 ms |
| FTS phrase (`"team decided"`) | p50 **2.19 ms**, p95 2.27 ms, p99 2.38 ms |
| FTS boolean-AND (`team AND migration`) | p50 **2.24 ms**, p95 2.38 ms, p99 3.55 ms |
| FTS prefix-wildcard (`migrat*`) | p50 **3.42 ms**, p95 3.47 ms, p99 3.86 ms |
| Hybrid: FTS-only | **180.9 µs** |
| Hybrid: semantic-only (mock embeddings) | 8,299 µs |
| Hybrid: full hybrid (FTS + semantic + recency) | 8,336 µs |
| Decay sweep | p50 **1.28 ms** over 25,000 rows (~19.6M rows/sec) |
| Peak RSS | **~23.3 MiB** (`VmHWM`) |

Notes on reading these numbers honestly:

- **Ingest** here (~1,685 msgs/s) is on a 25K single-scope corpus; the
  `benchmarks.md` server figure (~1,043 msgs/s) is a 100K corpus where
  FTS-index growth raises the amortised per-row cost. The two are not
  directly comparable — corpus size and scope layout differ — which is
  exactly why this doc records the full `device_bench` config alongside
  the numbers.
- **FTS** latencies scale with corpus size and token selectivity. At
  25K rows the queries sit in the 2–3.5 ms range; at the 100K/50-scope
  layout in `benchmarks.md` the single-token `exact`/`prefix` shapes
  are heavier (tens of ms) because they match a large fraction of the
  corpus.
- **Hybrid retrieval**: the FTS-only lane is ~46× faster than the
  semantic lane because the semantic lane does a full-scan cosine over
  the bag-of-words mock embedding. At this corpus size full hybrid
  (8.34 ms) is within noise of semantic-only (8.30 ms); the FTS
  prefilter's ordering advantage (hybrid *faster* than semantic-only)
  grows with corpus size, as the larger
  [`benchmarks.md`](benchmarks.md) run shows. Real on-device numbers
  will also depend on the production ONNX embedding model, which this
  mock deliberately stands in for.
- **Peak RSS** (~23 MiB) is the substrate process only, on Linux. The
  on-device figure that matters to a phone budget additionally includes
  the embedding/SLM model footprint, which is out of scope for this
  binary and tracked separately.

## Capturing a new device row

1. Build and run on the target device:
   ```bash
   cargo run -p benchmarks --release --bin device_bench > device-row.json
   ```
   (On a phone, build for the target via the platform FFI harness; the
   binary itself is plain Rust + SQLCipher with no desktop-only deps.)
2. On macOS/Windows, capture peak RSS out-of-band as described above
   and note it next to the row (the JSON `peak_rss_bytes` will be
   `null`).
3. Replace the corresponding matrix row's `[pending real-device
   measurement]` cells with the measured values, record the exact
   hardware and OS build, and attach the JSON.

## Links

- [`benchmarks.md`](benchmarks.md) — server-side reference numbers and
  the Criterion suite.
- [`platforms.md`](platforms.md) — per-platform build and tuning notes.
- `crates/benchmarks/src/bin/device_bench.rs` — the benchmark source.
