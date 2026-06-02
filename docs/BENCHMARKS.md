# Benchmarks

Performance benchmarks for the Knowledge substrate server-side
components. All benches use [Criterion.rs](https://bheisler.github.io/criterion.rs/book/)
for statistically rigorous measurement with HTML reports.

## Quick start

```bash
# Run ALL benchmarks (may take 15-30 minutes)
cargo bench -p integration_tests

# Run a specific benchmark suite
cargo bench -p integration_tests --bench load_bench
cargo bench -p integration_tests --bench graph_bench
cargo bench -p integration_tests --bench sync_bench
cargo bench -p integration_tests --bench permission_bench
cargo bench -p integration_tests --bench synthesis_bench

# Filter to a specific benchmark within a suite
cargo bench -p integration_tests --bench load_bench -- "ingest"
cargo bench -p integration_tests --bench sync_bench -- "compact_threshold"
```

HTML reports are generated at `target/criterion/report/index.html`.

## Benchmark suites

### Evidence store (`load_bench`)

| Benchmark | What it measures |
|-----------|-----------------|
| `evidence/ingest/throughput/{1K,10K,100K}` | Ingestion messages/sec across scope counts |
| `evidence/fts_retrieval/latency/{1K,10K,100K}` | FTS5 lexical query latency vs corpus size |
| `evidence/recent_retrieval` | `recent_evidence_ids_for_scope` at 10K rows |

Corpus sizes:
- 1K rows: 10 scopes × 100 evidence per scope
- 10K rows: 50 scopes × 200 evidence per scope
- 100K rows: 100 scopes × 1000 evidence per scope

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

The `compact_threshold` benchmarks validate the claims in
`docs/COST_MODEL.md` (lines 196–204): lowering the threshold from
10K to 5K ops reduces the steady-state delta payload without
affecting merge correctness.

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

## Methodology

- **Statistical rigor**: Criterion collects multiple samples per
  benchmark (10–100 depending on cost), applies outlier detection,
  and reports confidence intervals on the mean.
- **Warm-up**: Each benchmark group specifies `measurement_time` to
  allow Criterion's adaptive warm-up to stabilise variance.
- **Isolation**: Each iteration (where expensive) uses `iter_with_setup`
  to separate setup cost from the measured path.
- **Throughput annotation**: Benchmarks report `Throughput::Elements`
  or `Throughput::Bytes` so Criterion can compute ops/sec or MB/sec.
- **Determinism**: Where possible, UUIDs and data are generated
  during setup (`iter_with_setup`) so the measured path operates on
  pre-built data. Some throughput benchmarks (e.g. insert benchmarks)
  include UUID generation in the hot path since it is intrinsic to
  the operation being measured.

## CI integration

The `benchmarks.yml` workflow runs all Criterion benchmarks on every
tagged release (`v*` tags) and archives the HTML reports as CI
artifacts. This provides:

1. A historical record of performance across releases.
2. Downloadable Criterion HTML reports per release.

To compare performance across releases, download the artifact ZIPs
from two releases and use Criterion's `--baseline` flag locally:

```bash
cargo bench -p integration_tests -- --baseline v1.0.0
```

## Baseline results

> **Note**: These baselines are from the initial benchmark run on a
> standard CI runner. Absolute numbers depend on hardware; the value
> is in **relative** comparisons across releases.

Run `cargo bench -p integration_tests` to generate current baselines
for your hardware. Results are written to `target/criterion/`.
