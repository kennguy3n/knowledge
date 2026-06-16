# The Encrypted Store

> **TL;DR:** "On-device" is only meaningful if the on-disk data is
> encrypted under a key that never leaves the device, and if "delete"
> destroys the key rather than flips a flag. This post builds the bottom
> two layers — the `crypto` primitives and the `evidence_store` — and
> shows how the DEK/CEK key hierarchy makes **cryptographic forgetting**
> a one-line erase that competitors structurally cannot match.

## What you are building

Two crates:

- **`crypto`** — every primitive the substrate consumes: a hybrid
  X25519 + ML-KEM-768 KEM (post-quantum), ML-DSA-65 and SPHINCS+
  signatures, XChaCha20-Poly1305 AEAD, BLAKE3 hashing, and the
  provenance bundle. (Benchmarked in
  [`benchmarks.md`](../../docs/technical/benchmarks.md) §8.)
- **`evidence_store`** — the encrypted, append-only evidence plane:
  SQLCipher at rest, FTS5 for lexical retrieval, content-aware storage
  routing, and the per-scope key wrapping that makes erasure real.

## Build it: the key hierarchy first

Do not start with the database. Start with the key tree, because the
store's erase semantics are a *property of the key tree*, not of the
SQL.

1. **Master key** — derived/unwrapped on the device via the hybrid
   X25519 + ML-KEM-768 KEM (§8 of the architecture doc). The
   post-quantum half matters today because of *harvest-now-decrypt-later*:
   an adversary who records ciphertext now can't wait for a quantum
   computer to read it later. This is why the KEM is hybrid, not
   classical-only.
2. **Per-scope DEK (Data Encryption Key)** — a *scope* is the privacy
   boundary (one conversation, one user, one matter). The SQLCipher
   database key for a scope is derived under the master key. Destroy the
   scope's key material and the scope's rows become unrecoverable
   ciphertext.
3. **Per-body CEK (Content Encryption Key)** — large bodies are
   encrypted under a random CEK; per-scope CEK *wraps* live in a
   `body_store_key_wraps` table so several scopes can share one
   deduplicated body. Delete a scope's wraps and, once no wraps remain,
   the body is gone.

This three-level tree (master → DEK → CEK) is what makes "forget this
person" a key-destruction operation instead of a row scan.

## Build it: the store

The public surface is deliberately tiny (see
[post 1 of the main series](../01-why-on-device-memory.md)):

```text
open_store(db_path, master_key)
ingest_message(scope_id, text, source_kind, importance)
query(scope_id, query_text, limit) -> [QueryResult]
```

Two store-design decisions earn their keep:

- **Content-aware storage routing** (architecture doc §2.2). Bodies are
  routed by size and importance:
  - **≤ 512 B inline** — short chat messages stored in the evidence row;
    BLAKE3-framed but no dedup-index JOIN. Optimises the common case.
  - **> 512 B body table** — files, transcripts, document chunks stored
    once with BLAKE3 content-hash dedup, each under a random CEK.
  - **Noise ring buffer** — messages the importance tagger marks as
    noise go into a fixed-size circular buffer (default 5 MB) that never
    persists past the current synthesis window.
- **A `MemoryProfile` per device tier** — Low = 512 KiB SQLCipher page
  cache with `mmap` disabled (the 2–4 GB phone), Medium/High scale up.
  The store reads its budget from the auto-detected device tier so it
  doesn't blow the memory budget on a constrained handset.

## The evidence it works

These are server-VM reference numbers from the benchmark suite
([`benchmarks.md`](../../docs/technical/benchmarks.md)) — use them for
the *algorithm* and regression tracking, not as a phone budget (the
device matrix in [`benchmarks-device.md`](../../docs/technical/benchmarks-device.md)
has the on-device row):

| Operation | Result (reference VM) |
|---|---|
| AEAD encrypt (4 KB) | 6.77 µs (577 MiB/s) |
| Hybrid KEM encap (X25519 + ML-KEM-768) | 159.9 µs |
| ML-DSA-65 sign / verify | 320 µs / 77 µs |
| Storage footprint (500K rows) | ~612 B/message amortised |
| Ingest (100K corpus, amortised) | ≈959 µs/msg (~1,043 msgs/s) |

A note on honesty you should copy: SPHINCS+ signing is ~17 ms — ~54×
slower than ML-DSA-65 — so it is reserved for rare, long-lived
signatures (root-of-trust), never per-message. Publishing *why* a
primitive is slow is more credible than hiding it.

## The business decision: erasure as a feature

**Scenario.** A healthcare or HR product gets a verified "delete my
data" request, or an employee leaves and their device-resident memory
must be provably destroyed. The compliance team needs an answer to "can
you prove it's gone?"

- **Soft-delete vendors (Copilot, Glean, Notion AI, Mem0, Zep, Letta).**
  "Delete" sets a flag or removes a row; backups, replicas, and vector
  indices may retain copies; proving destruction across a distributed
  cloud is hard. See the [comparison](../../docs/product/comparison.md)
  — every cloud row in it is "soft delete."
- **Knowledge.** Destroy the per-scope DEK (and the CEK wraps for shared
  bodies). The data is mathematically unrecoverable, on every replica,
  immediately, without a backup sweep. This is the same
  *crypto-shredding* primitive the sibling security platform uses for
  per-tenant/day key destruction — it's a proven pattern, not a novelty.

That capability maps directly onto HIPAA, SOX, and FERPA obligations;
the [PQC threat-model whitepaper](../../docs/security/pqc-threat-model.md)
documents the mapping and the residual side-channel risks honestly.

## How a competitor would build this

A cloud product builds the store as a central multi-tenant database with
row-level security and envelope encryption *in the vendor's KMS*. That's
operationally simpler and enables cross-user features — but the keys
live with the vendor, "offline" isn't a mode, and erasure is a
distributed-systems problem rather than a key-destruction one. The
on-device tree trades that convenience for a guarantee you can put in a
contract.

## What's next

An encrypted store full of raw messages isn't *memory* yet — it's an
encrypted log. The next layer turns evidence into structured
observations: who, what, which task, which decision, in 22 languages, on
the device.

---
*Part 2 of "How to Build Knowledge." [Previous: Architecture & Build Order](01-architecture-and-build-order.md) | [Next: Observation & Extraction](03-observation-and-extraction.md) | [Series index](README.md)*
