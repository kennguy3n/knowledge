# Memory That Forgets

> **TL;DR:** A knowledge substrate that only accumulates becomes both
> less useful and legally radioactive. Knowledge treats decay as a
> first-class feature and implements the right to erasure by destroying
> encryption keys — making forgotten data cryptographically
> unrecoverable, not just flagged as deleted.

## The Business Problem

A healthcare provider deploys an AI assistant that remembers patient
interactions to help clinicians pick up where they left off. It is
genuinely useful — until a patient exercises their right under GDPR
Article 17 (and its healthcare-specific cousins) to have their data
erased. Now the provider has to prove that the patient's data is
*gone*: not hidden, not soft-deleted behind a flag, but actually
unrecoverable, including from backups and derived indexes.

This is where most "delete" implementations fall apart. A row marked
`deleted = true` is still on disk. A vector still sits in the index. A
nightly backup still holds the plaintext. Auditors and regulators do
not accept "we stopped showing it" as erasure. And the problem
generalizes well beyond healthcare: stale memory also degrades
quality, surfacing year-old context as if it were current.

A knowledge product needs two kinds of forgetting: *graceful* decay so
old, low-value memory fades, and *hard* erasure so a deletion request
is provably honored.

## The Technical Approach

**Decay as a feature.** The
[`memory_manager` crate](../crates/memory_manager/) implements a decay
state machine with retention scoring. Memory items carry an importance
class and age; retention scoring lets the substrate down-weight or
evict low-value items over time while preserving the things that
matter. Decay keeps the working set relevant and bounded rather than
letting the store grow without limit. The
[design document](../docs/technical/design.md) §3 covers the memory
model and decay planes.

**Cryptographic forgetting.** Hard erasure is the interesting part.
Every *scope* in the substrate (a conversation, a patient, a matter,
a tenant) has its own Data Encryption Key (DEK), derived from the
master key. All evidence in that scope is encrypted under its scope
DEK. To forget a scope, the substrate **destroys the DEK** — and
because the underlying ciphertext can no longer be decrypted by anyone,
the data is cryptographically unrecoverable even though the encrypted
bytes may still physically exist until they are overwritten.

This is the key insight: the evidence table is append-only and the
plaintext is never stored, so you cannot "go delete the rows" safely
across crashes and backups. But if the *only* way to read a scope's
data is its DEK, then destroying that one small key erases the entire
scope at once — atomically and durably. See the
[crypto spec](../docs/technical/crypto-spec.md) for the forgetting
protocol and the [threat model](../docs/security/threat-model.md) for
how this defends the right-to-erasure asset.

**Durable tombstones.** Destroying an in-memory key is not enough — the
process restarts. The substrate records a durable *forgetting
tombstone* for each forgotten scope (and each forgotten epoch within a
scope). On every `open_store`, it replays the tombstones into the
in-process key registry, so a call for a forgotten scope after a
restart still short-circuits with "not found" rather than resurrecting
the data. It also re-purges the FTS5 and embedding indexes for
forgotten scopes on open, closing the window where a crash between
"write tombstone" and "purge index" could otherwise leave
plaintext-derived search terms readable on disk.

## Implementation Walk-through

The forgetting API is one call:

```text
forget(scope_id)
  -> destroy the scope DEK (in-memory registry)
  -> persist a forgetting tombstone (durable)
  -> purge FTS5 + embedding rows for the scope
```

After `forget(scope_id)`, any `query(scope_id, ...)` returns nothing
and any attempt to decrypt that scope's evidence fails with a
not-found error. On the next process start, tombstone replay re-applies
the destruction before any query can run. The
[api cookbook](../docs/guides/api-cookbook.md) shows the forget flow,
and [build-a-chat-app](../docs/guides/build-a-chat-app.md) wires it to
a "delete conversation" action so deletion in the UI means erasure in
the substrate.

For the healthcare provider, this maps cleanly onto a patient erasure
request: one scope per patient, `forget(patient_scope)` on request, and
the data is cryptographically gone — with a durable record that it was
forgotten.

## Performance & Cost Implications

Decay is cheap. The [benchmarks](../docs/technical/benchmarks.md) clock
the decay sweep over a 100K-row slice at about **5.26 ms** (≈19 million
rows/second), and a single-row decay update at p50 **83.7 ns**. Decay
can run as routine maintenance without a noticeable cost.

Cryptographic forgetting is even cheaper at the moment of erasure:
destroying a key and writing a tombstone is constant work regardless of
how much data the scope holds. You do not pay to re-encrypt or
bulk-delete a large corpus to honor a deletion request — you destroy
one key. That property is what makes "right to erasure" operationally
viable at scale instead of a batch job that grinds for hours.

## What's Next

Cryptographic forgetting rests entirely on the strength and handling of
the keys involved. The next post tackles the cryptography directly: why
post-quantum matters *now*, and how the key hierarchy is structured so
that destroying one key cleanly erases exactly one scope.

---
*This is part 3 of the "Building Knowledge" series. [Previous: The Multilingual Extraction Engine](02-multilingual-extraction-engine.md) | [Next: Post-Quantum Crypto for Mortals](04-post-quantum-crypto-for-mortals.md)*
