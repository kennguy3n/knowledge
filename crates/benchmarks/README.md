# benchmarks

Criterion benchmark suite and shared workload generators for the Knowledge substrate.

## Purpose

Provides reproducible performance measurement for the substrate's hot
paths. The library crate exposes deterministic workload generators (no
`rand`, no wall-clock seeding) so every harness draws byte-for-byte
identical synthetic corpora run-to-run; each file under `benches/` is a
standalone Criterion harness exercising one path.

## Key types

- `MESSAGE_TEMPLATES` — multilingual business-chatter templates used to build corpora.
- `messages_by_language` — deterministic corpus generator over the templates.
- `MockEmbeddingModel` — `EmbeddingModel` implementation feeding the hybrid-retrieval semantic lane.

## Benchmarks

| Harness | Path exercised |
|---|---|
| `bench_ingest_throughput` | Evidence ingest. |
| `bench_fts_query_at_scale` | FTS5 query at 100K messages. |
| `bench_hybrid_retrieval` | Lexical + semantic + recency retrieval. |
| `bench_synthesis_e2e` | End-to-end synthesis. |
| `bench_storage_footprint` | On-disk storage efficiency. |
| `bench_decay_sweep` | Decay state-machine sweep. |
| `bench_concept_graph_traversal` | Concept-graph traversal. |
| `bench_crypto_operations` | AEAD, KEM, signatures. |
| `bench_connector_sync_throughput` | Connector delta sync. |
| `bench_observation_extraction` | Observation extraction. |
| `bench_permission_check` | Zanzibar reachability checks. |

## Usage

```bash
cargo bench -p benchmarks
```

## Links

- [docs/technical/benchmarks.md](../../docs/technical/benchmarks.md) — Published results and methodology.
