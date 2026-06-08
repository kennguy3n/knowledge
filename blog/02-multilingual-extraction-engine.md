# The Multilingual Extraction Engine

> **TL;DR:** Knowledge extracts structured observations — decisions,
> tasks, questions, entities — from raw messages across 22 languages,
> using a lexicon-first pipeline that runs on-device with no model
> download and no per-language code branches.

## The Business Problem

A multinational rolls out a B2B knowledge tool to offices in twelve
countries. The sales team in São Paulo writes in Portuguese, the
engineers in Berlin in German, the support desk in Osaka in Japanese,
and the leadership thread switches between English and French
mid-sentence. The product promise is simple: surface the decisions,
action items, and open questions buried in all that chatter.

Most extraction pipelines are built English-first and bolt on other
languages later — if ever. The result is a tool that feels magical for
North American customers and useless everywhere else. Worse, the
common fix (route everything through a large multilingual model in the
cloud) reintroduces exactly the privacy and cost problems that
[on-device memory](01-why-on-device-memory.md) was meant to solve. A
knowledge tool that only works in English, or only works by shipping
every message to a server, is not a global product.

## The Technical Approach

The **observation engine** turns raw evidence into structured
observations: it classifies sentences as decisions, tasks, or
questions, and pulls out named entities. The design is *lexicon-first*
rather than model-first — instead of one giant neural model, it uses a
per-language `LexiconRegistry` of keyword sets and matching strategies.
See the [design document](../docs/technical/design.md) §3.2 and the
[extraction-quality doc](../docs/technical/extraction-quality.md) for
the evaluation methodology.

Three properties make it work across 22 languages:

1. **Per-sentence language detection.** A single message can mix
   languages; the engine detects language per sentence (not per
   message) so a French sentence in an otherwise-English thread is
   classified with the French lexicon.

2. **Script-aware matching strategies.** Languages form questions and
   mark tasks differently, and scripts segment words differently. The
   registry encodes a matching *strategy* per language:
   - *FirstToken* / *FirstBigram* for whitespace-segmented languages
     where leading particles carry the signal (e.g. Vietnamese `tại
     sao` "why", `khi nào` "when").
   - *Substring* for scripts without inter-word whitespace or with
     combining marks that fragment tokens — CJK Han, Thai, Lao, Khmer,
     Myanmar, Tibetan.
   - A *proclitic-peeling* strategy for Arabic-script morphology, where
     short particles agglutinate onto the front of a word with no
     separator, so the matcher iteratively peels recognised prefixes
     and re-checks.

3. **No model download.** Because the core is lexicon-driven, it runs
   immediately on-device with no multi-gigabyte model to ship, no GPU,
   and deterministic behavior. (A pluggable embedding model can be
   wired in later for the semantic retrieval lane, but extraction does
   not depend on it.)

This is a deliberate engineering trade. A lexicon-first design will
miss nuance a large model would catch, but it is fast, explainable,
runs on a budget phone, and — critically — works in 22 languages on day
one rather than English on day one and "coming soon" everywhere else.

## Implementation Walk-through

Extraction happens as part of ingest. A host ingests a message; the
observation pipeline runs language detection, then the per-language
matchers, and attaches the resulting observations to the evidence:

```text
extract(message_text, scope_id)
  -> detect language per sentence
  -> apply LexiconRegistry matchers (decision / task / question)
  -> extract capitalised-word entities
  -> [Observation { kind, text, language, ... }]
```

Adding a new language is a data change, not a code change: add a lexicon
entry with the language's decision/task/question keyword lists and pick
the matching strategy that fits its script. The
[`observation_engine` crate](../crates/observation_engine/) holds the
registry and the per-strategy matchers; the
[extraction-quality doc](../docs/technical/extraction-quality.md)
describes how to add evaluation cases for a new language so coverage is
measured, not assumed.

A worked detail: typographic apostrophes. French `Aujourd'hui`
(U+2019) and `Aujourd'hui` (ASCII U+0027) must produce the same entity
set, so the extractor folds typographic apostrophes before matching.
These are the kinds of script-specific edge cases that a lexicon-first
design lets you handle explicitly and test deterministically.

## Performance & Cost Implications

The [benchmarks](../docs/technical/benchmarks.md) put the full
observation pipeline (extraction + language detection) at roughly
**6,729 messages/second** over a 10K mixed-language corpus. Per-language
throughput is consistent — English ≈5,191/s, Spanish ≈4,947/s, French
≈5,225/s, German ≈5,210/s — which matters: there is no "fast path" for
English and a slow path for everyone else.

Because extraction runs on-device with no model inference required, the
cost is the same as the rest of the substrate: zero marginal
infrastructure spend. A multinational with twelve offices pays nothing
extra to extract knowledge in twelve languages, and no message ever
leaves the device to be classified.

## What's Next

Extraction fills the store with structured memory. But memory that
only ever grows is a liability — both for relevance and for privacy
law. The next post is about forgetting: why a knowledge substrate needs
decay, and how cryptographic forgetting makes "delete" actually mean
delete.

---
*This is part 2 of the "Building Knowledge" series. [Previous: Why On-Device Memory](01-why-on-device-memory.md) | [Next: Memory That Forgets](03-memory-that-forgets.md)*
