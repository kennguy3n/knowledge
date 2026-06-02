# Benchmarks

Performance benchmarks for the Knowledge substrate server-side
components. All benches use [Criterion.rs](https://bheisler.github.io/criterion.rs/book/)
for statistically rigorous measurement with HTML reports and
machine-readable JSON estimates.

There are two benchmark crates:

- **`benchmarks`** — the production benchmark suite (11 harnesses)
  documented below, exercising the hot substrate paths end to end with
  real numbers collected on the reference hardware.
- **`integration_tests`** — the original micro-benchmark suites
  (`load_bench`, `graph_bench`, `sync_bench`, `permission_bench`,
  `synthesis_bench`) used during development.

## Quick start

```bash
# Run the full production suite (≈30 min — the 100K/500K ingest and
# 500K storage harnesses dominate the wall-clock).
cargo bench -p benchmarks

# Run a single harness.
cargo bench -p benchmarks --bench bench_crypto_operations
cargo bench -p benchmarks --bench bench_fts_query_at_scale

# Filter to one benchmark within a harness.
cargo bench -p benchmarks --bench bench_crypto_operations -- aead/encrypt

# The legacy micro-benchmark suites still live in integration_tests.
cargo bench -p integration_tests
cargo bench -p integration_tests --bench graph_bench
```

HTML reports are generated at `target/criterion/report/index.html`.
Per-benchmark JSON estimates are written to
`target/criterion/<group>/<bench>/new/estimates.json` and the raw
per-sample timings to `.../new/sample.json` — these are what the CI
job archives for historical regression comparison.

## Reference hardware

All numbers in the results tables below were collected on this VM:

| Property | Value |
|----------|-------|
| CPU | AMD EPYC 7763 64-Core, 8 vCPU exposed (1 thread/core, 8 cores) |
| Memory | 31 GiB |
| OS | Ubuntu 22.04.5 LTS, Linux kernel 5.15.200 |
| Arch | x86_64 |
| Toolchain | rustc 1.95.0 (workspace MSRV is 1.85.0) |
| Criterion | 0.7 |
| Profile | `bench` (release optimisation, debug-assertions off) |

Absolute numbers are hardware-dependent; the durable value is in
**relative** comparison across commits/releases (see
[Regression comparison](#regression-comparison)). The store is
encrypted SQLCipher with `WAL` journaling; the `mlock()` warnings
printed during ingest harnesses are the SQLCipher key-page locking
falling back to unlocked pages inside the container and do not affect
the timings.

## Production benchmark suite (`benchmarks` crate)

### 1. Ingest throughput — `bench_ingest_throughput`

100K realistic mixed-language / mixed-importance messages into a fresh
encrypted store.

| Benchmark | Result |
|-----------|--------|
| `ingest/throughput_100k` (100K msgs) | **95.88 s** → **~1,043 msgs/sec** (≈959 µs/msg amortised, includes FTS index growth) |
| `ingest/single_message` into fresh store | p50 **624 µs**, p99 **661 µs** |

The single-message figure opens a brand-new store per iteration, so it
is dominated by SQLCipher key derivation + schema bootstrap; treat it
as the cold-start floor, and the 100K amortised rate as the
steady-state per-row cost (which is higher because the FTS5 index and
WAL grow as rows accumulate).

### 2. FTS query at scale — `bench_fts_query_at_scale`

100K rows ingested across 50 scopes, then FTS5 queries against the
scoped index. Each query type is its own Criterion group; p50/p99 are
computed over the per-sample timings.

| Query | p50 | p99 |
|-------|-----|-----|
| exact (`migration`) | **55.86 ms** | 58.83 ms |
| phrase (`"team decided"`) | **13.56 ms** | 14.37 ms |
| boolean AND (`team AND migration`) | **14.75 ms** | 19.82 ms |
| prefix wildcard (`migrat*`) | **56.19 ms** | 60.42 ms |

Phrase and boolean-AND queries are ~4× faster than the single-token
exact / prefix queries here because the high-selectivity token pair
prunes the postings list far more aggressively than the very common
`migration` token, which matches a large fraction of the 100K corpus.

### 3. Hybrid retrieval — `bench_hybrid_retrieval`

10K-row single-scope corpus, comparing the three retriever
configurations (mock deterministic embeddings for the semantic
component).

| Mode | Median latency |
|------|----------------|
| FTS-only | **188.8 µs** |
| semantic-only (mock embeddings) | 9.93 ms |
| hybrid (FTS + semantic + recency rerank) | **9.70 ms** |

The semantic component (full-scan cosine over the bag-of-words mock
embedding) dominates; hybrid is marginally faster than semantic-only
because the FTS prefilter trims the candidate set before reranking.

### 4. Synthesis end-to-end — `bench_synthesis_e2e`

Channel synthesis over a 1000-message scope using the `NoOpSynthesizer`
fallback adapter (isolates pipeline machinery from LLM inference).

| Stage | Median latency |
|-------|----------------|
| window creation → synthesis → publication | **8.14 µs** |
| synthesize only | 1.03 µs |
| publish only | 6.03 µs |

### 5. Storage footprint — `bench_storage_footprint`

Encrypted SQLCipher on-disk size (main DB + `-wal` + `-shm`) after
ingesting N rows. The headline is printed to stderr during setup
(`STORAGE_FOOTPRINT N=… file_bytes=… bytes_per_msg=…`); the Criterion
timing itself only measures the `metadata().len()` probe.

| Rows | On-disk bytes | Bytes / message |
|------|---------------|-----------------|
| 1K | 860,160 | 860 |
| 10K | 6,275,072 | 627 |
| 100K | 61,739,008 | 617 |
| 500K | 306,466,816 | **612** |

Per-message overhead converges to ~612 B as fixed page/index overhead
amortises; the 1K figure is inflated by the minimum WAL/page
allocation.

### 6. Decay sweep — `bench_decay_sweep`

Retention scoring + candidate-archive / TTL transitions over 100K
`MemoryObject`s.

| Benchmark | Result |
|-----------|--------|
| `decay/sweep_100k` (full slice) | **5.26 ms** → **~19.0 M rows/sec** |
| `decay/single_row` | p50 **83.7 ns**, p99 **90.7 ns** |

### 7. Concept-graph traversal — `bench_concept_graph_traversal`

100K-node balanced tree with typed `IsA` edges; bounded BFS at depths
1/3/5 and branching factors 10/100.

| Fan-out | depth 1 | depth 3 | depth 5 |
|---------|---------|---------|---------|
| 10 | 1.51 µs | 176.4 µs | 25.53 ms |
| 100 | 13.24 µs | 22.13 ms | 28.83 ms |

Depth-5 at fan-out 100 visits essentially the whole 100K-node graph, so
the two fan-outs converge there; the depth-3/fan-out-100 step is where
the frontier first explodes (10⁰→10²→10⁴ nodes).

### 8. Crypto operations — `bench_crypto_operations`

AEAD (ChaCha20-Poly1305) throughput across payload sizes, plus the
post-quantum hybrid primitives.

| Operation | 512 B | 4 KB | 64 KB | 1 MB |
|-----------|-------|------|-------|------|
| AEAD encrypt | 2.34 µs (208 MiB/s) | 6.77 µs (577 MiB/s) | 80.4 µs (778 MiB/s) | 1.223 ms (818 MiB/s) |
| AEAD decrypt | 2.36 µs (207 MiB/s) | 5.44 µs (718 MiB/s) | 59.6 µs (1.02 GiB/s) | 1.221 ms (819 MiB/s) |

| Operation | Median latency |
|-----------|----------------|
| hybrid KEM (X25519 + ML-KEM-768) encap | 159.9 µs |
| hybrid KEM decap | 156.8 µs |
| ML-DSA-65 sign | 320.3 µs |
| ML-DSA-65 verify | 77.4 µs |
| SPHINCS+ sign | **17.36 ms** |
| SPHINCS+ verify | 1.214 ms |

SPHINCS+ signing is ~54× slower than ML-DSA-65 and is why the SPHINCS+
group runs with a reduced sample count — it is intended for rare,
long-lived signatures (e.g. root-of-trust), not per-message use.

### 9. Connector sync throughput — `bench_connector_sync_throughput`

A bench-local `Connector` paginates 10K documents (100 pages × 100
docs) off a `MockHttpTransport` and emits one `DocumentCreated` event
per document.

| Benchmark | Result |
|-----------|--------|
| `connector/sync_10k_docs` initial_sync | **1.236 ms** → **~8.09 M events/sec** |

This isolates the framework's parse + event-emission cost from real
network latency (the mock transport returns canned page bodies).

### 10. Observation extraction — `bench_observation_extraction`

`observation_engine::default_pipeline` (lexicon extractor + classifier
+ language detection) over 10K mixed-language messages.

| Benchmark | Result |
|-----------|--------|
| `observation/pipeline_10k` (10K mixed) | 1.486 s → **~6,729 msgs/sec** |

Per-language per-message rate (`observation/by_language`):

| Language | Per-message rate |
|----------|------------------|
| English | ~5,191 msgs/sec |
| Spanish | ~4,947 msgs/sec |
| French | ~5,225 msgs/sec |
| German | ~5,210 msgs/sec |
| Japanese | ~83,065 msgs/sec |
| Arabic | ~24,747 msgs/sec |

Latin-script languages run the full lexicon-matching path on every
sentence; the CJK (Japanese) and Arabic buckets short-circuit large
parts of the English-centric lexicon, so they are markedly faster per
message.

### 11. Permission check — `bench_permission_check`

Zanzibar-style reachability over ~10K relation tuples, with a depth-5
group-indirection positive path and fan-out-100 at the target.

| Check | p50 | p99 | Rate |
|-------|-----|-----|------|
| allowed (depth-5 chain) | **6.51 µs** | 7.06 µs | ~152K checks/sec |
| denied (BFS exhausts reachable set) | 112.3 µs | 123.7 µs | ~8.8K checks/sec |

The allowed case short-circuits the moment the chain resolves; the
denied case is the worst case — it must walk the entire reachable
closure (including the 100-wide fan-out) before returning `false`.

## Methodology

- **Statistical rigor**: Criterion collects multiple samples per
  benchmark (10–100 depending on cost), applies outlier detection, and
  reports a 95% confidence interval around the mean. The tables above
  quote the point estimate (median for latency, mean for throughput).
- **p50 / p99**: where the spec calls for percentiles, they are
  computed over Criterion's per-sample mean timings
  (`time = sample_total / sample_iters`) read from `sample.json`. p50
  equals Criterion's reported median; p99 is the 99th percentile of the
  per-sample distribution. This is a conservative tail estimate —
  per-sample averaging smooths the very longest single-operation
  outliers — but is fully reproducible from the archived JSON.
- **Throughput annotation**: harnesses set `Throughput::Elements` or
  `Throughput::Bytes` so Criterion prints ops/sec, rows/sec, events/sec,
  or MB/sec directly.
- **Isolation**: expensive setup (corpus build, 100K-object allocation,
  fresh-store open, mock-response registration) runs in
  `iter_with_setup` / before the measured closure so the timed region
  only covers the operation under test.
- **Determinism**: all workloads come from the deterministic generators
  in `benchmarks::{realistic_messages, importance_for,
  messages_by_language, MockEmbeddingModel}` — no RNG in the corpus, so
  runs are reproducible and comparable across commits.

## Comparison vs typical SaaS ingest APIs

These numbers are single-node, synchronous, and include
encrypt-on-ingest — they are not directly comparable to a horizontally
scaled cloud API, but provide useful context:

- **Ingest (~1,043 msgs/sec single-writer, encrypted).** A typical
  hosted ingest API (e.g. a search/observability SaaS) advertises
  per-tenant write quotas in the low thousands of events/sec *per
  shard* and scales out horizontally. The substrate hits a comparable
  per-shard figure on one core while doing AEAD encryption and FTS
  indexing inline and durably (`WAL`), where most SaaS pipelines batch,
  defer indexing, and acknowledge before the write is durable. The
  ~959 µs/msg is dominated by per-row `fsync` + FTS index maintenance;
  batching ingests in a single transaction is the lever for higher
  throughput.
- **FTS query (13–56 ms p50 at 100K rows/scope).** In the same range as
  a managed search cluster's scoped query latency, but here it is a
  local encrypted SQLite FTS5 index with no network hop.
- **Permission checks (~152K allowed checks/sec).** Comparable to the
  cache-warm path of a hosted authorization service (e.g. SpiceDB /
  Zanzibar-style systems), again without the RPC round-trip — the
  denied/worst-case path is the one to watch as the relation graph
  grows.

## Regression comparison

Criterion supports baselines for cross-run comparison:

```bash
# Save the current run as a named baseline.
cargo bench -p benchmarks -- --save-baseline main

# … later, on a feature branch, compare against it.
cargo bench -p benchmarks -- --baseline main
```

Criterion prints a `change: [-x% +y%] (p = …)` line per benchmark and
flags statistically significant improvements/regressions. The CI job
(below) archives `target/criterion/` — including the `estimates.json`
and `sample.json` files — as an artifact per run, so two runs can be
diffed after the fact by restoring both trees and using
`--load-baseline`.

## CI integration

The `.github/workflows/benchmarks.yml` workflow runs the suite:

- on push of `v*` release tags (per-release baseline),
- on a **weekly schedule** (Mondays 06:00 UTC) so drift is caught
  between releases,
- and on manual `workflow_dispatch`.

It archives the full `target/criterion/` tree (HTML reports + JSON
estimates) and a dedicated `criterion-json` artifact containing just
the `estimates.json` / `sample.json` files for lightweight historical
comparison. Artifacts are retained for 90 days.

---

## Legacy micro-benchmark suites (`integration_tests`)

These predate the production suite and remain useful for targeted
micro-measurements.

### Evidence store (`load_bench`)

| Benchmark | What it measures |
|-----------|-----------------|
| `evidence/ingest/throughput/{1K,10K,100K}` | Ingestion messages/sec across scope counts |
| `evidence/fts_retrieval/latency/{1K,10K,100K}` | FTS5 lexical query latency vs corpus size |
| `evidence/recent_retrieval` | `recent_evidence_ids_for_scope` at 10K rows |

### Concept graph (`graph_bench`)

| Benchmark | What it measures |
|-----------|-----------------|
| `graph/traverse_typed_chain` | BFS over linear IsA chains at 10K/100K nodes |
| `graph/traverse_typed_bounded` | Depth-limited traversal (5/50/500) |
| `graph/tree_bfs` | BFS over wide trees (branching factor ~10) |
| `graph/neighbors` | Single-hop neighbor lookup throughput |
| `graph/mutation` | Node insertion throughput (10K nodes) |

### CRDT sync engine (`sync_bench`)

| Benchmark | What it measures |
|-----------|-----------------|
| `sync/merge` | Merge two engines at 1K/10K/50K ops |
| `sync/compact` | Compaction throughput after add+remove churn |
| `sync/compact_threshold` | Merge latency at different threshold values |
| `sync/delta_size` | Snapshot size at different compact thresholds |
| `sync/snapshot` | Serialize/deserialize throughput |

### Permission service (`permission_bench`)

| Benchmark | What it measures |
|-----------|-----------------|
| `permission/check_depth` | Zanzibar reachability at 0/1/3/5/10 hops |
| `permission/check_wide` | Check in a 1K-viewer set |
| `permission/tuple_insert` | TupleStore insert throughput |
| `permission/tuple_lookup` | TupleStore `contains()` lookup |

### Synthesis pipeline (`synthesis_bench`)

| Benchmark | What it measures |
|-----------|-----------------|
| `synthesis/channel` | Single channel recap (NoOp) latency |
| `synthesis/batch` | Batch synthesis at 10/100/1000 windows |
| `synthesis/window_manager` | Window open throughput |
| `synthesis/window_query` | Scope-filtered window lookup |

The synthesis benchmarks use `NoOpSynthesizer` (no actual LLM
inference) to isolate the pipeline machinery overhead from model
inference time.
