# observation_engine

Lexicon-first extraction of structured observations from raw evidence.

## Purpose

Turns raw text into typed observations (entities, facts, tasks,
decisions, claims). The baseline is a lexicon extractor — regex /
keyword / capitalised-word heuristics that need no model — used as
the cheap first stage before XLM-R + SLM-assisted extraction.

## Public API summary

| Type / Function | Description |
|---|---|
| `LexiconExtractor` / `ObservationExtractor` | Extraction engines. |
| `ObservationPipeline` / `default_pipeline` | Configurable pipeline. |
| `Observation` / `ObservationType` | Extracted observation types. |
| `detect_language` / `LanguageTag` | Multilingual language detection. |
| `LexiconRegistry` / `default_registry` | Built-in lexicons. |
| `DocumentChunker` / `DocumentObservationPipeline` | Document processing. |
| `Citation` / `CitationRegistry` | Source citation tracking. |
| `should_promote` / `ChannelPromotionPolicy` | Observation promotion logic. |

## Links

- [ARCHITECTURE.md](../../docs/technical/architecture.md) §2.1 — Module map.
- [docs/technical/design.md](../../docs/technical/design.md) §3.2 — Observation plane.
- [docs/getting-started/for-developers.md](../../docs/getting-started/for-developers.md) — Consumer integration guide.
