# evidence_store

SQLCipher-backed encrypted evidence store for the Knowledge substrate.

## Purpose

Implements the evidence plane from `docs/DESIGN.md` §3.1: encrypted
append-only storage with content-hash deduplication, size-threshold
routing (inline ≤ 512 B / body table > 512 B / noise ring buffer),
FTS5 lexical indexing, and hybrid (lexical + semantic + recency)
retrieval.

## Public API summary

| Type / Function | Description |
|---|---|
| `EvidenceStore` | Main store: open, ingest, query, forget. |
| `ImportanceClass` | Storage routing tier (Critical, Important, Useful, Noise). |
| `HybridRetriever` | FTS5 + recency + semantic-vector retriever. |
| `EmbeddingModel` / `EmbeddingRuntime` | Pluggable embedding backends. |
| `escape_fts_query` | Safe FTS5 query escaping. |

## Feature flags

| Feature | Description |
|---|---|
| `onnx-runtime` | ONNX Runtime + HuggingFace tokenizer for real embeddings. |
| `test-support` | Exposes test hooks for injection and legacy migration. |

## Usage example

```rust
use evidence_store::EvidenceStore;
use crypto::MasterKey;

let key = MasterKey::generate();
let store = EvidenceStore::open("./store.db", &key)?;
let id = store.ingest("scope-1", "alice", "hello world",
    evidence_store::ImportanceClass::Important)?;
let results = store.query("hello", 10)?;
```

## Links

- [ARCHITECTURE.md](../../docs/technical/architecture.md) §2.1, §2.2 — Evidence plane.
- [docs/DESIGN.md](../../docs/DESIGN.md) §3.1 — Evidence plane design.
- [docs/INTEGRATION_GUIDE.md](../../docs/INTEGRATION_GUIDE.md) — Consumer integration guide.
