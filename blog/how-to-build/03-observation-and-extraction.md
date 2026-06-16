# Observation & Extraction

> **TL;DR:** Raw messages are not memory. The `observation_engine` turns
> evidence into typed observations — entities, facts, tasks, decisions,
> questions — across 22 languages, on-device, with **no cloud NLP call**.
> This post builds that layer lexicon-first, publishes its per-type F1
> floors honestly (entity precision is *low*, by design), and explains
> why a deterministic, measurable extractor beats an opaque cloud API for
> a privacy product.

## What you are building

`observation_engine`: a pipeline that reads evidence and emits
`Observation`s typed as `Entity`, `Task`, `Decision`, `Fact`, or
`Question`, each with a confidence and a detected language. It feeds the
importance classifier (which drives the store's hot/noise routing from
the previous post) and the memory and concept layers downstream.

The design constraint is the same network-free invariant: extraction
must run on the device, so the default path is a **lexicon-first
extractor** — fast, deterministic, no model file required — with
heavier ML stages (XLM-R embeddings, an SLM) layered on top to *refine*,
not to gate, the base result.

## Build it: lexicon-first, then refine

1. **Language detection + a multilingual lexicon registry.** The
   extractor matches task/decision/question cues against a per-language
   lexicon (16+ languages in the registry, 22 supported end-to-end). The
   `bench_observation_extraction` harness shows CJK and Arabic
   short-circuit large parts of the English-centric lexicon, so they run
   *faster* per message than Latin scripts.
2. **A `GoldenDataset` eval from day one.** Before you tune anything,
   build the regression harness (`crates/observation_engine/src/eval.rs`).
   It scores per-type precision/recall/F1 against a labelled golden set,
   and the CI gate
   (`cargo test -p integration_tests --test observation_eval`) fails if
   any type drops below its floor. See
   [`extraction-quality.md`](../../docs/technical/extraction-quality.md).
3. **Refine with ML only where it pays.** XLM-R embeddings handle
   *semantic near-dedup at the observation plane* — catching the same
   fact stated in different words across channels — and an SLM refines
   entity extraction. Crucially this is a refinement stage; the lexicon
   result is the floor, so the system still works on a device with no
   model loaded.

## The evidence — published, including the weak number

The honest baseline on the default `LexiconExtractor` over a 52-case
golden dataset ([`extraction-quality.md`](../../docs/technical/extraction-quality.md)):

| Type | Precision | Recall | F1 |
|---|---|---|---|
| Decision | 1.000 | 0.889 | **0.941** |
| Question | 0.923 | 1.000 | **0.960** |
| Task | 0.917 | 0.733 | 0.815 |
| Fact | 0.457 | 0.941 | 0.615 |
| Entity | **0.163** | 0.824 | **0.272** |

**Macro-F1: 0.721.** Entity *precision* is deliberately low: the lexicon
extractor aggressively grabs capitalised words and dates as candidate
entities and lets the downstream XLM-R/SLM stages prune them. Publishing
that number — rather than quietly reporting only macro-F1 — is the point:
the floor is in CI, so the next contributor can't regress it without the
build going red.

## The business decision: own the extractor, or rent it

**Scenario.** You're building a multilingual assistant for teams in
France, Japan, Brazil, India, and Germany. Extraction quality varies by
language and you need to (a) keep content on-device and (b) be able to
tell a customer exactly how good extraction is in *their* language.

- **Rent a cloud NLP API (the default for Copilot/Glean/Notion AI-class
  products).** You get strong models with zero ML effort — but every
  message leaves the device, quality per language is a black box you
  can't audit, and you pay per call forever.
- **Own a lexicon-first on-device extractor (this).** More upfront work,
  and the base quality is lower than a frontier cloud model. In return:
  nothing leaves the device, the per-language F1 is *measured and
  published* (the [multilingual leaderboard](../../docs/technical/multilingual-leaderboard.md)
  rolls it up), and the marginal cost per message is zero. You can hand a
  customer the exact F1 for Japanese and the CI gate that protects it.

For a privacy-first product the calculus is clear: a *measurable,
private* extractor you can stand behind beats an *opaque, leaky* one you
can't — even when the opaque one scores higher on an English benchmark.

## How a competitor would build this

A cloud-native product sends text to a hosted extraction/embedding
service and stores the structured output centrally. It's less code and
higher raw quality, and it's the right call if privacy and offline use
aren't constraints. It cannot, however, tell you per-language quality
without its own eval, and it cannot run on a plane. The on-device
extractor is the choice you make when "it works offline and we can prove
how well" is a feature.

## What's next

Now we have typed observations. The next layer makes them *recallable*
and *forgettable*: the hybrid retriever that finds them fast, the decay
state machine that ages them out, and the concept graph that links them.

---
*Part 3 of "How to Build Knowledge." [Previous: The Encrypted Store](02-the-encrypted-store.md) | [Next: Retrieval & the Memory Graph](04-retrieval-and-memory.md) | [Series index](README.md)*
