# Why On-Device Memory

> **TL;DR:** Server-side RAG forces a choice between useful AI memory
> and user privacy. Keeping the knowledge substrate on the user's own
> device removes that trade-off — and, as a side effect, drops the
> marginal cost of memory to zero.

## The Business Problem

Imagine a consumer chat app — call it KChat — serving 10 million users
across APAC. Every conversation a user has is, in principle, valuable
context: the project they mentioned last week, the people they work
with, the decisions they made. An AI assistant that remembers those
things is dramatically more useful than one that starts cold every
session.

The conventional way to build that memory is server-side retrieval-
augmented generation (RAG): ship every message to a backend, embed it,
store the vectors in a central database, and retrieve the relevant
slices at query time. It works, but it puts the most sensitive thing a
user owns — the full text of their private conversations — on
someone else's computer. For a privacy-conscious B2C audience, that is
a liability, not a feature. It is also a regulatory problem: cross-
border data transfer rules, data-residency requirements, and breach-
notification obligations all attach to that central store.

And it is expensive. Ten million users generating memory means ten
million users' worth of embeddings, vector-index storage, and
retrieval compute — a cost that scales linearly with your user base
and never goes away. The companies that win consumer AI on thin
margins cannot afford memory that costs money per user per month.

Existing solutions force a three-way trade-off between privacy, cost,
and usefulness. Knowledge's answer is to move the entire memory
substrate onto the device where the data already lives.

## The Technical Approach

The foundation is the **evidence plane** — an encrypted, append-only
store that lives on the user's device. Every message, document, or
event the app wants to remember is ingested as a piece of *evidence*:
content-hashed for deduplication, size-routed (small items inline,
larger bodies in a separate table, low-value chatter into a bounded
noise ring buffer), and indexed for retrieval. The store is backed by
SQLCipher, so the on-disk database is encrypted at rest under a key
that never leaves the device. See the
[architecture overview](../docs/technical/architecture.md) §2 and the
[design document](../docs/technical/design.md) §3.1 for the full model.

Because the store is on-device, "send everything to a server" is
simply not a step that exists. There is no central corpus to breach,
no cross-border transfer to document, and no per-user storage bill.
The trade-offs are real and worth naming:

- **Compute is bounded by the device.** A phone is not a datacenter.
  Retrieval and synthesis must be cheap enough to run on a handset.
  (Spoiler from [post 8](08-performance-at-device-scale.md): they are.)
- **There is no global view.** The substrate sees one user's data, not
  the aggregate. For a privacy product that is the entire point, but it
  rules out designs that depend on cross-user signals.
- **You own the key problem.** On-device encryption is only as good as
  the host's key handling — covered in
  [post 4](04-post-quantum-crypto-for-mortals.md) and the
  [key-management guide](../docs/security/key-management.md).

Retrieval itself is hybrid: a lexical lane (SQLite FTS5), an optional
semantic-vector lane (a pluggable embedding model), and a recency
rerank. The lexical lane does the heavy candidate filtering so the
expensive semantic step only ever reranks a small set. This is what
keeps retrieval fast enough to feel instant on a phone.

## Implementation Walk-through

At the API level, the on-device path is deliberately small. A host app
opens a store with a master key, ingests messages tagged with a
*scope* (e.g. one scope per conversation), and queries within a scope:

```text
open_store(db_path, master_key)
ingest_message(scope_id, text, source_kind, importance)
query(scope_id, query_text, limit) -> [QueryResult]
```

Each `QueryResult` carries the matched evidence plus ordering scores.
The scope is the unit of isolation: data ingested under one scope is
encrypted under that scope's derived key and is never visible to a
query in another scope. That property is what lets a single app hold
many users — or many conversations — in one database without leaking
between them.

The same surface is exposed to every platform through the FFI layer:
Swift/Kotlin via UniFFI for mobile, N-API for Electron desktop. A
host never touches raw cryptographic state; it hands in plaintext and a
scope, and the substrate handles encryption, routing, and indexing.
The [build-a-chat-app guide](../docs/guides/build-a-chat-app.md) walks
the full flow end to end.

## Performance & Cost Implications

The [benchmark suite](../docs/technical/benchmarks.md) measures the
hot paths on commodity hardware. Ingest runs at roughly **1,043
messages/second** into a fresh store (≈959 µs amortised per message,
including FTS index growth). Hybrid retrieval over a 100K-message
corpus completes in about **9.7 ms** at p50 — faster than the
semantic-only lane, because the FTS prefilter trims the candidate set
before the rerank ever runs.

The cost story is the headline. Because memory lives on the device,
the marginal infrastructure cost of one more user's memory is **zero**.
There is no vector database to provision, no embedding API to meter, no
per-user storage line item. You pay for the device you already shipped
software to. For a 10-million-user consumer app, that is the
difference between a memory feature that is a cost center and one that
is free. The full breakdown is in
[post 10](10-cost-engineering-zero-marginal.md) and the
[cost model](../docs/operator/cost-model.md).

## What's Next

On-device memory is only useful if it understands what the user is
actually saying — in whatever language they say it. The next post digs
into the multilingual extraction engine that turns raw messages into
structured, queryable observations across 22 languages.

---
*This is part 1 of the "Building Knowledge" series. [Next: The Multilingual Extraction Engine](02-multilingual-extraction-engine.md)*
