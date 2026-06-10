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
| Toolchain | rustc 1.95.0 (workspace MSRV is 1.88.0) |
| Criterion | 0.7 |
| Profile | `bench` (release optimisation, debug-assertions **on** — see `[profile.bench]` in `Cargo.toml`) |

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

## Device profiles

The substrate ships to a wide RAM envelope, from a 2 GB-class budget
phone to an 8 GB+ laptop/desktop. The [`DeviceTier`](../../crates/inference_router/src/config.rs)
classification (auto-detected from system RAM, overridable via
`KNOWLEDGE_SLM_DEVICE_TIER`) drives two coordinated behaviours:

- the **inference router** gates which SLM tasks run on-device per tier
  (Low = encoder-only, Medium = classification, High = + synthesis), and
- the **evidence store** selects a per-connection memory profile from
  the tier (see
  [`MemoryProfile`](../../crates/evidence_store/src/store.rs)): Low =
  512 KiB SQLCipher page cache with `mmap` disabled; **Medium = 1 MiB
  page cache with `mmap` kept enabled**; High = SQLite defaults.

The `device_profile_*` harnesses exercise one representative workload
mix per tier.

### Device targets

| Tier | Representative device | RAM | On-device inference | Store mode |
|------|-----------------------|-----|---------------------|-----------|
| **Low** | Budget Android (e.g. Redmi Note 12, 4 GB nominal / ~2 GB usable under app limits) | < 2 GiB | **Encoder-only** — MLX + llama.cpp gated off; classification via the encoder `FallbackAdapter` | Low-memory (512 KiB cache, no mmap) |
| **Medium** | Budget Windows i5 laptop (8 GB) or 6 GB Android | 2–8 GiB | **Classification** on llama.cpp (`TagImportance`, `ExtractEntities`, `PromoteObservation`); synthesis gated off | Medium (1 MiB cache, mmap kept) |
| **High** | M2 MacBook Air (8 GB), desktop, or `high`-pinned server | ≥ 8 GiB | **Full synthesis** end-to-end (`SynthSummary`, `SynthConcept`, `AdjudicateContradiction`) | Default |

### Provenance — read this before quoting the numbers

> ⚠️ **The tables below are measured-in-CI on the reference VM
> ([Reference hardware](#reference-hardware)), NOT on the named physical
> devices.** They characterise the *substrate-side* cost of each tier's
> code path (store I/O, encoder classification, router dispatch +
> latency instrumentation, synthesis-pipeline machinery) with the same
> deterministic, network-free fixtures as the rest of the suite. The
> on-device SLM transport is replaced by the in-process
> `MockLlamaServerClient`, so **real model-inference latency is excluded
> here** and reported separately, with placeholders, under
> [SLM latency](#slm-latency). Numbers requiring the named hardware are
> explicitly marked **TBM-on-device** (to-be-measured-on-device).

Run them with:

```bash
cargo bench -p benchmarks --bench device_profile_low_tier
cargo bench -p benchmarks --bench device_profile_medium_tier
cargo bench -p benchmarks --bench device_profile_high_tier
```

### Low tier (`device_profile_low_tier`) — measured-in-CI

Encoder-only ingest / query / maintenance against a **low-memory**
store (512 KiB page cache, mmap off) plus encoder classification
through the tier-gated adapter ladder.

| Workload | Result |
|----------|--------|
| `low_tier/ingest` — 10K msgs, low-memory store | **6.04 s** → ~1.65K msgs/sec (~604 µs/msg) |
| `low_tier/fts` — `search_fts`, low-memory store | p50 **1.42 ms** |
| `low_tier/decay` — full `decay_sweep`, 10K objects | **525 µs** → ~19.0 M rows/sec |
| `low_tier/classify` — `TagImportance` via encoder fallback | **2.61 µs** |

The 512 KiB cache trades throughput for a bounded resident set: ingest
is slower per-row than the default-tier `bench_ingest_throughput`
(~959 µs/msg there, but at 100K with index growth) because the smaller
cache faults more pages back from the encrypted file. Decay scoring is
pure in-memory CPU work and is unaffected by the store profile.

### Medium tier (`device_profile_medium_tier`) — measured-in-CI (mock transport)

Classification dispatched through the router → llama.cpp adapter, with
the model replaced by a constant-time mock. **This is the router +
adapter + latency-recording overhead, not model latency.**

| Task | Dispatch overhead (median) |
|------|----------------------------|
| `TagImportance` | **375 ns** |
| `ExtractEntities` | **371 ns** |
| `PromoteObservation` | **345 ns** |

These sub-µs figures are the fixed cost the
`knowledge_slm_dispatch_duration_seconds` instrumentation and adapter
plumbing add on top of whatever the model itself takes — i.e. the floor
that real on-device latency ([SLM latency](#slm-latency)) is added to.

#### Medium-tier store profile (`MemoryProfile::Medium`) — measured-in-CI

Medium-tier hosts (4–6 GB Android, 8 GB i5 laptops) no longer fall
through to the SQLite default page cache. The store opens each keyed
connection with a **1 MiB** page cache (`MEDIUM_MEMORY_PAGE_CACHE_KIB`,
half the 2 MiB default) while **keeping `mmap` enabled** — the middle
profile between the Low tier's 512 KiB-cache-and-no-mmap clamp and the
High tier's defaults. The harness exercises ingest + FTS against a
20 000-message corpus (2× the Low tier's 10 000) on this profile:

| Workload | Result |
|----------|--------|
| `medium_tier/store/ingest_medium_memory` — 20K msgs, 1 MiB cache + mmap | **12.49 s** → ~1.60K msgs/sec (~625 µs/msg) |
| `medium_tier/fts/fts_medium_memory` — `search_fts`, 1 MiB cache + mmap | p50 **2.48 ms** |

Per-row ingest (~625 µs/msg) lands between the Low tier's 512 KiB cache
(~604 µs/msg at 10K) and the default profile, with `mmap` left enabled
so the larger working set stays page-cache-friendly rather than
faulting through the 512 KiB clamp. FTS p50 (2.48 ms) is higher than
the Low tier's 1.42 ms purely because the corpus is 2× larger, not
because of the cache profile. The 1 MiB cap keeps the resident set
bounded on 4 GB-class devices while avoiding the throughput cliff the
512 KiB Low profile trades away. See
[`MemoryProfile`](../../crates/evidence_store/src/store.rs) and the
`medium_memory_mode_applies_1mib_cache_and_keeps_mmap` integration test.

### High tier (`device_profile_high_tier`)

| Workload | Result | Provenance |
|----------|--------|-----------|
| `high_tier/synthesis` window→synthesize→publish (1K-msg window, `NoOpSynthesizer`) | **8.49 µs** | measured-in-CI |
| `high_tier/synthesis` `SynthSummary` router dispatch (mock transport) | **411 ns** | measured-in-CI (dispatch overhead only) |
| End-to-end synthesis with a real GGUF model on M2 / desktop | **TBM-on-device** | see [SLM latency](#slm-latency) |

The 8.49 µs e2e figure is the synthesis-pipeline machinery (window
management + recap assembly + AEAD publication) in isolation from
inference — it matches the `bench_synthesis_e2e` headline and is the
fixed overhead the model's generation time adds to.

## SLM latency

The router instruments every dispatch with a wall-clock timer from
prompt submission to response completion and records it into the
`knowledge_slm_dispatch_duration_seconds` histogram, labelled by `task`
and `adapter` (see
[`router.rs`](../../crates/inference_router/src/router.rs) `dispatch`).
The FFI health surface exposes the p50/p95 of this histogram when an
inference adapter is present (`SlmLatencyReport` in
[`health.rs`](../../crates/ffi/src/health.rs)), and `substrate_server`
exports the raw histogram at `/internal/metrics`.

### Instrumentation overhead — measured-in-CI

With the mock transport (no real model), a full dispatch through the
router — including the histogram record — measures:

| Path | Median |
|------|--------|
| classification dispatch (Medium tier) | ~345–375 ns |
| synthesis dispatch (High tier) | ~411 ns |

This is the **instrumentation + plumbing floor**; the histogram bucket
boundaries (`LATENCY_BUCKETS_SECONDS`, 1 ms … 60 s) are sized for real
model latencies that are orders of magnitude larger. The tail extends to
60 s because cold on-device synthesis (weight paging + prompt prefill on
a budget phone/laptop) routinely exceeds 10 s; quantiles falling beyond
the top finite bound are clamped to it (standard Prometheus
`histogram_quantile` behaviour), so the wide tail keeps cold-start p95
observable instead of pinned at the ceiling.

### Model latency by tier — TBM-on-device

Real SLM latency (prompt eval + token generation) depends on the model,
quantisation, and device silicon, none of which exist in CI. These
cells are **to-be-measured-on-device** and must not be fabricated:

| Tier / device | Cold start (first dispatch, model load incl.) | Warm (model resident) |
|---------------|-----------------------------------------------|-----------------------|
| Low — encoder-only (no SLM) | n/a (fallback classifier, ~2.6 µs — measured-in-CI) | n/a |
| Medium — llama.cpp classification, budget i5 8 GB | **TBM-on-device** | **TBM-on-device** |
| High — llama.cpp synthesis, M2 MacBook Air 8 GB | **TBM-on-device** | **TBM-on-device** |

**Measurement procedure (on-device).** Build the substrate with the
`http-client` feature, point `KNOWLEDGE_LLAMA_SERVER_URL` at a
`llama-server` serving the target GGUF, set
`KNOWLEDGE_SLM_DEVICE_TIER` to the tier under test, then drive a fixed
prompt set through `trigger_synthesis` (High) / the classification FFI
(Medium). Read p50/p95 from the `knowledge_slm_dispatch_duration_seconds`
histogram via `/internal/metrics` (server) or the `health_check` FFI
(`SlmLatencyReport`). Capture the **first** dispatch separately for the
cold-start row (it pays the model-load + warm-up-prompt cost), then the
steady-state for the warm row. Record the device, model file, quant
level, and thread count alongside the numbers.

## Startup

`open_store` is the substrate's boot-critical path (schema bootstrap,
SQLCipher key derivation, tombstone replay, synthesis-window
rehydration). WS8 **lazy-loads the inference router**: `open_store` no
longer probes the SLM adapters at boot — the llama.cpp probe is a
`GET /health` with a multi-second timeout, now deferred to the first
synthesis dispatch (`InferenceRouter::ensure_bootstrap_started`). Ingest
/ query-only hosts therefore never pay the probe cost, and boot no
longer blocks on a (possibly absent) model sidecar. The
`knowledge_open_store_duration_seconds` histogram records the completed
open latency (success path only).

> **Operator note — health-check availability is lazy too.** Because
> adapters are not probed until the first synthesis dispatch, the FFI
> `health_check` reports the `inference_router` subsystem as
> `Unavailable` (and SLM latency as absent) until `trigger_synthesis`
> is first called. Hosts that gate UI on adapter availability should
> treat this as "not yet probed" rather than "permanently
> unsupported", or trigger a synthesis to force the probe. See the
> doc comment on `inference_router_subsystem` in
> [`crates/ffi/src/health.rs`](../../crates/ffi/src/health.rs).

### `open_store` latency — measured-in-CI

| Scenario | Latency |
|----------|---------|
| Cold open (fresh DB, schema creation) | **~13 ms** |
| Warm open (existing DB, 5K rows → tombstone replay + rehydration) | **~3.4 ms** median |

Measured on the reference VM by timing `open_store` directly (cold:
empty path; warm: reopen after ingesting 5K rows and `close_store`).
Both are dominated by SQLCipher key derivation + page setup; the lazy
router load removes the former multi-second adapter probe from this
path entirely.

### Health-check `start_period`

Because boot no longer absorbs an eager adapter probe, the substrate
container's `start_period` in
[`deploy/docker-compose.yml`](../../deploy/docker-compose.yml) was
reduced **20s → 5s**. 5s is ~300× the measured cold open and still
leaves generous headroom for binary load + Axum bind; `retries: 5` at
`interval: 10s` adds a further ~50s of post-start grace regardless.

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
