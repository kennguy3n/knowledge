# Observation Extraction Quality Evaluation

The observation engine ships with a systematic evaluation framework for
measuring extraction quality and preventing regressions.

## Overview

| Component | Path |
|-----------|------|
| Framework library | `crates/observation_engine/src/eval.rs` |
| Golden dataset + regression tests | `crates/integration_tests/tests/observation_eval.rs` |
| Throughput benchmark | `crates/integration_tests/benches/observation_throughput.rs` |

## Concepts

### Golden Dataset

A `GoldenDataset` is a collection of `TestCase`s, each containing:

- **`input_text`** — raw text fed to the extractor.
- **`expected`** — a list of `ExpectedObservation`s, each with:
  - `observation_type` (`Entity`, `Task`, `Decision`, `Fact`, `Question`)
  - `content_substring` — case-insensitive substring that must appear in
    the produced observation's `content` field.
  - Optional `min_confidence` / `max_confidence` range.

### Metrics

`run_eval` computes per-type **precision**, **recall**, and **F1**:

- **True positive (TP):** an expected observation matched a produced one
  (same type, substring found, confidence in range).
- **False positive (FP):** a produced observation matched no expected one.
- **False negative (FN):** an expected observation matched no produced one.

Matching is greedy first-match — each produced observation can match at
most one expected observation.

### EvalReport

The report exposes `per_type: HashMap<ObservationType, TypeMetrics>`,
a `macro_f1()` helper, and a human-readable `Display` impl.

## Running the Evaluation

### Quality test (regression guard)

```bash
cargo test -p integration_tests --test observation_eval
```

Add `-- --nocapture` to print the full report:

```bash
cargo test -p integration_tests --test observation_eval -- --nocapture
```

### Throughput benchmark

```bash
cargo bench -p integration_tests --bench observation_throughput
```

Filter to a specific benchmark group:

```bash
cargo bench -p integration_tests --bench observation_throughput -- observation_extraction
```

## Current Baseline

Measured on the default `LexiconExtractor` with the 52-case golden dataset:

| Type | Precision | Recall | F1 | Threshold |
|------|-----------|--------|----|-----------|
| Decision | 1.000 | 0.889 | 0.941 | 0.85 |
| Entity | 0.163 | 0.824 | 0.272 | 0.20 |
| Fact | 0.457 | 0.941 | 0.615 | 0.50 |
| Question | 0.923 | 1.000 | 0.960 | 0.90 |
| Task | 0.917 | 0.733 | 0.815 | 0.70 |

**Macro-F1:** 0.721

Entity precision is low because the lexicon extractor aggressively
extracts capitalised words and dates as entities. This is by design —
downstream pipeline stages (XLM-R, SLM) refine entity extraction.

## Extending the Golden Dataset

1. Open `crates/integration_tests/tests/observation_eval.rs`.
2. Add a new `tc(...)` call in the `golden_dataset()` function.
3. Each case needs:
   - A unique label string (for diagnostics).
   - Input text.
   - A vec of `ExpectedObservation`s using the `exp()` / `exp_conf()` helpers.
4. Run the eval with `--nocapture` to verify the new case.

### Categories

The dataset is organised into four blocks:

| Block | Cases | Focus |
|-------|-------|-------|
| 1 | English business | Meeting notes, Slack, email, standups |
| 2 | Mixed-language | 16+ languages per the lexicon registry |
| 3 | Edge cases | Short messages, code, URLs, CJK, emoji |
| 4 | Failure modes | Known FP/FN patterns, regression guards |

When adding cases, pick the block that best fits. For new languages,
add to Block 2.

### Recalibrating Thresholds

After improving the extractor:

1. Run `cargo test -p integration_tests --test observation_eval -- --nocapture`.
2. Read the printed F1 values.
3. Update the thresholds in `f1_regression_thresholds()` to be ~5-10%
   below the new measured values.
4. Commit the threshold update alongside the extractor improvement.

## Architecture

```
observation_engine::eval        ← framework (GoldenDataset, run_eval, EvalReport)
  ↑
integration_tests::observation_eval  ← golden dataset + cargo test target
integration_tests::observation_throughput  ← criterion benchmark
```

The framework lives in the `observation_engine` crate so it can be
reused by other test harnesses or downstream crates. The golden dataset
and assertions live in `integration_tests` following the workspace's
convention for cross-crate tests.
