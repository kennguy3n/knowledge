# Performance at Device Scale

> **TL;DR:** A power user with years of chat history holds hundreds of
> thousands of messages on a phone. Knowledge's hybrid retriever keeps
> retrieval fast at that scale by using a fast FTS5 prefilter to trim
> the candidate set before any expensive reranking runs. The latency
> numbers below were measured on a **server-class reference VM**, not a
> phone — they characterise the *algorithm*, and we are now measuring
> the *device* numbers explicitly. See
> [the device benchmark matrix](../docs/technical/benchmarks-device.md)
> for what has and has not yet been measured on real hardware.

> **A note on honesty.** An earlier version of this post quoted
> server-VM benchmark numbers as if they were phone numbers. They are
> not. This post now distinguishes **server-measured** results (the
> AMD EPYC reference VM in [`benchmarks.md`](../docs/technical/benchmarks.md))
> from **device-measured** results (the portable `device_bench` tool
> and matrix in [`benchmarks-device.md`](../docs/technical/benchmarks-device.md)),
> and labels device rows we have not yet captured as pending.

## The Business Problem

The users who get the most value from AI memory are the ones who have
used the app the longest — and they are exactly the ones whose stores
are largest. A power user with three years of daily conversations might
have 500,000 messages on their device. If retrieval gets slow as the
store grows, the product gets *worse* for your best users over time.
That is the opposite of what you want.

And this all has to happen on a phone. There is no datacenter to throw
at the problem, no horizontal scaling, no GPU cluster. The retrieval
that powers "what did we decide last quarter?" has to complete fast
enough to feel instant, on a device that is also running the rest of
the user's apps, on battery.

That constraint is the whole point of the architecture below — and it
is also why we are careful about *where* our numbers come from. An
algorithm that is fast on a 64-core server is not automatically fast on
a phone CPU under thermal and memory pressure. The honest position is:
the design is sound and the server numbers are strong, and the
per-device numbers are something we measure rather than assume.

## The Technical Approach

The [`evidence_store` crate](../crates/evidence_store/) implements
**hybrid retrieval** with three lanes (see the
[design document](../docs/technical/design.md) §3.1):

1. **Lexical (FTS5).** SQLite's full-text search over the evidence
   bodies, scoped to the querying scope. This is the workhorse: it is
   fast, it scales well with index size, and it does the bulk
   candidate selection.

2. **Semantic (optional).** A pluggable embedding model scores
   candidates by vector similarity for meaning-based matches. This lane
   is the expensive one.

3. **Recency rerank.** A recency signal reorders candidates so newer,
   more relevant memory surfaces first.

The performance trick is *ordering*: the FTS5 lane runs **first** and
trims the corpus down to a small candidate set; the semantic lane only
ever reranks that small set. You never embed-and-compare the whole
store. This is why, counterintuitively, the full hybrid query is
*faster* than running the semantic lane alone — the prefilter does the
expensive lane a favor.

Other design choices that hold up at scale: content-hash deduplication
avoids storing the same body twice; size-threshold routing keeps the
hot evidence table lean (large bodies live in a separate table, low-
value chatter in a bounded ring buffer); and FTS5 query escaping keeps
user input from blowing up into pathological queries.

## Implementation Walk-through

Retrieval is one call, and the lanes are transparent to the caller:

```text
query(scope_id, query_text, limit) -> [QueryResult { score, fts_score, ... }]
```

If no embedding model is wired in, the semantic lane simply contributes
nothing and the substrate falls back to lexical + recency — so the
fast path works out of the box, and the semantic lane is an upgrade you
opt into. The [api cookbook](../docs/guides/api-cookbook.md) shows
query patterns, and the [platforms doc](../docs/technical/platforms.md)
covers per-platform tuning knobs.

For the power user, the practical upshot is that "search my three years
of history" runs against an FTS5 index that was built for exactly this,
with the semantic rerank applied only to the handful of candidates that
survive the prefilter.

## Performance & Cost Implications

### Server-measured (reference VM)

These numbers are from the [benchmarks](../docs/technical/benchmarks.md)
on a **100K-message corpus**, collected on the **AMD EPYC 7763 cloud
VM (8 vCPU, 31 GiB)** reference hardware. They are server numbers — use
them to reason about the *algorithm* and to track regressions across
commits, not as a phone latency budget.

| Operation | Latency (server VM) |
|---|---|
| Ingest (amortised) | ≈959 µs/msg (≈1,043 msgs/sec) |
| Single-message ingest | p50 624 µs, p99 661 µs |
| FTS exact query | p50 55.86 ms |
| FTS phrase query | p50 13.56 ms |
| FTS boolean AND | p50 14.75 ms |
| FTS-only retrieval | 188.8 µs |
| Semantic-only (mock) | 9.93 ms |
| **Hybrid (FTS + semantic + recency)** | **9.70 ms** |

The load-bearing result is that last row: hybrid retrieval lands
*under* the semantic-only lane, because the FTS prefilter shrinks the
rerank set — the ordering trick pays off. Exact and prefix-wildcard
queries are heavier (≈56 ms) because they match a large fraction of the
corpus; phrase and boolean queries, the common shapes, sit in the ~14
ms range.

### Device-measured (portable `device_bench`)

To measure the on-device story honestly we ship a portable, one-command
benchmark — `cargo run -p benchmarks --release --bin device_bench` —
that drives the same real ingest / FTS / hybrid / decay paths and emits
machine-readable JSON. It builds and runs unchanged on Linux, macOS
(Apple Silicon), Windows, and constrained devices, and records each
result against the hardware that produced it. The full methodology and
the device matrix live in
[`benchmarks-device.md`](../docs/technical/benchmarks-device.md).

What is **measured so far** (a Linux x86-64 reference run, 25K-message
single-scope corpus):

| Operation | Latency (Linux reference) |
|---|---|
| Ingest (amortised) | ≈593 µs/msg (≈1,685 msgs/sec) |
| FTS phrase query | p50 2.19 ms, p95 2.27 ms |
| Hybrid (FTS + semantic + recency) | 8.34 ms (FTS-only lane 181 µs) |
| Decay sweep | p50 1.28 ms over 25K rows |
| Peak RSS | ~23 MiB |

What is **not yet measured**: the rows that actually matter for the
on-device claim — **iPhone (A-series), mid-range Android, Apple-Silicon
Mac, Windows laptop, and a 2–4 GB constrained device** — are still
`[pending real-device measurement]` in the matrix. We are filling them
in by running the same binary on physical hardware rather than
extrapolating from the server VM.

### Cost

The cost angle is unchanged and real: retrieval runs on the device, so
there is no retrieval infrastructure to scale and no per-query cost.
Your best, highest-history users cost exactly as much to serve as your
newest ones — nothing.

## What's Next

On-device performance handles the B2C power user. The next post crosses
into B2B territory: serving thousands of organizations from one
deployment, where the challenge shifts from raw latency to isolation
and permissions.

---
*This is part 8 of the "Building Knowledge" series. [Previous: Zero to Production Deployment](07-zero-to-production-deployment.md) | [Next: Multi-Tenant at Scale](09-multi-tenant-at-scale.md)*
