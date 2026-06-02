# integration_tests

Cross-crate integration tests for the Knowledge substrate.

## Purpose

This crate intentionally exports nothing — it only exists so the
files under `tests/` can pull in `crypto`, `evidence_store`,
`concept_graph`, `permission_service`, and other crates together and
exercise the full pipeline.

## Test suites

| Test file | Coverage |
|---|---|
| `ingest_to_query.rs` | Evidence-store ingest, query, and cryptographic forgetting. |
| `concept_graph_pipeline.rs` | Evidence -> observation -> concept graph supersession. |
| `permission_check.rs` | Zanzibar-style permission checks with inheritance. |
| `crypto_round_trip.rs` | Hybrid KEM, ML-DSA-65, SPHINCS+, co-sign, AEAD post-forgetting. |

## Running

```bash
cargo test -p integration_tests --all-features
```

## Links

- [CONTRIBUTING.md](../../CONTRIBUTING.md) — Build, test, lint.
