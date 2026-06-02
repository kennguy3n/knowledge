# Multilingual dependency audit — Phase 2.4

**Audit date:** 2026-06-01
**Audit scope:** every direct workspace dependency whose role is to
classify, normalise, segment, or otherwise inspect human-language
text contents. Transitive deps are excluded — they are pinned by
their direct consumers, and the workspace's `cargo audit` /
`cargo deny` gates already cover advisory drift on the full graph.

This document is the single-place audit log for the multilingual
dependency surface. The corresponding pin rationales live next to
each `version = "…"` line in the relevant `Cargo.toml`, with a
cross-reference back to this file. When you bump or add a
multilingual dep, update both this file and the inline comment so
the audit log stays current.

---

## TL;DR

Every multilingual direct dep is on the latest published version
that fits the workspace's `MSRV (1.85.0)` CI gate. One feature-gated
dep (`ort`) is intentionally pinned behind the head of its
pre-release line for two cumulative reasons (an upstream build
break on `rc.12` and an MSRV floor of `1.88` on `rc.11`+). No
action items.

## Workspace MSRV gate

The workspace declares `rust-version = "1.85"` at the root
[`Cargo.toml`](../Cargo.toml) and enforces it in CI via the
`MSRV (1.85.0)` job in
[`.github/workflows/ci.yml`](../.github/workflows/ci.yml), which
pins the toolchain via `dtolnay/rust-toolchain@1.85.0` and runs
`cargo check --workspace --all-targets --exclude napi_addon`. The
`napi_addon` exclusion exists because `napi-rs 3.x` independently
requires `rustc ≥ 1.88` — that crate carries its own
`rust-version = "1.88"` in
[`crates/napi/Cargo.toml`](../crates/napi/Cargo.toml) and is not
part of the workspace MSRV gate's surface.

The `1.85` floor is currently set by `ml-dsa 0.1.0`'s internal
`edition = "2024"` requirement (FIPS 204 ML-DSA-65 post-quantum
signature backend); see the comment on the workspace's
`edition = "2021"` declaration in [`Cargo.toml`](../Cargo.toml)
for the per-crate edition rationale.

Every dep listed below has its current latest-version status
verified against `https://index.crates.io/` on the audit date.

## Direct multilingual dependencies

### `whatlang = "0.18"` (workspace-pinned)

| Field | Value |
|---|---|
| Current pin | `whatlang = "0.18"` |
| Resolves to | `whatlang 0.18.0` |
| Latest published | `0.18.0` |
| Upstream MSRV | undeclared (works fine on `1.85`) |
| Workspace MSRV status | OK — pure Rust, no `cfg`-gated MSRV features |
| Used by | `observation_engine::language::detect_language` (Phase 1.3), `observation_engine::extractor::sentence_split` (Phase 1.4), `evidence_store::embedding_routing::classify_for_embedding` (Phase 1.12) |
| Action | **None — on latest** |

`whatlang` is a pure-Rust trigram-statistics language detector
returning `Option<Info>` where `Info` carries an ISO 639-3 enum,
a confidence score, and an `is_reliable` flag. The substrate
calls `whatlang::detect` directly at three sites:

1. `observation_engine::detect_language` — `is_reliable()`-filtered
   so only confident detections stamp BCP-47 primary subtags onto
   `Observation`s.
2. `observation_engine::sentence_split` per-sentence detection
   inside multilingual paragraphs.
3. `evidence_store::embedding_routing::classify_for_embedding` —
   uses `whatlang::detect(text).is_none()` as a looser admit
   criterion than `detect_language`'s `is_reliable()` filter (the
   embedding-lane gate wants to admit anything with linguistic
   content, not just the high-confidence subset).

#### Known limitation: Tibetan / Lao classifiers

`whatlang 0.18` does **not** ship trigram classifiers for the
Tibetan or Lao scripts. This is called out inline in the
module-level doc comment of
[`crates/evidence_store/src/script.rs`](../crates/evidence_store/src/script.rs)
and worked around correctly: the FTS5 CJK / bigram routing
predicate keys on **script presence** (Tibetan / Khmer / Myanmar /
Lao codepoints) rather than on the language tag, so detection
refusal does not silently drop those scripts out of the recall
lane. The audit confirms no regression vector — the script-based
routing is the right architecture even with a perfect detector,
because mixed-language bodies whose dominant language is Latin
must still be tokenisable in the CJK / bigram lane for any
non-Latin codepoints they carry.

If a future Phase ever wants per-script language detection beyond
what `whatlang 0.18` offers (e.g. to gate the embedding lane on a
narrower set of scripts than the FTS lane), the candidate
replacements published on crates.io today are:

- `whichlang` `0.1.x` — successor-style API, pure Rust, MIT,
  currently `0.1.1` (pre-1.0, surface still in motion).
- `lingua` `1.x` — heavier (≥40 MB of model data), Apache-2.0,
  much higher recall on short inputs but a substantial size hit
  for the substrate's mobile-deployment surface.

Neither is in scope for Phase 2.4 — this audit is freshness-only
and not a redesign brief.

### `unicode-normalization = "0.1"` (workspace-pinned)

| Field | Value |
|---|---|
| Current pin | `unicode-normalization = "0.1"` |
| Resolves to | `unicode-normalization 0.1.25` |
| Latest published | `0.1.25` |
| Upstream MSRV | `1.36` (well below workspace floor) |
| Workspace MSRV status | OK |
| Used by | `observation_engine::lexicon` (Phase 1.4) — applies NFC to the input before the interrogative first-token table lookup |
| Action | **None — on latest** |

NFC normalisation is one-shot at extractor time (not on every
field write) and only applied to lexicon-bound input, so the dep
footprint is small and stable. The crate is `no_std`-compatible
which makes it usable even from contexts that disable allocator
features upstream (currently irrelevant for us, but it keeps
options open for future embedded surfaces).

### `tokenizers = "0.23"` (feature-gated in `evidence_store`)

| Field | Value |
|---|---|
| Current pin | `tokenizers = "0.23"` (gated behind `onnx-runtime` feature) |
| Resolves to | `tokenizers 0.23.1` |
| Latest published | `0.23.1` |
| Upstream MSRV | undeclared on `0.23.1` (later lines none either; lockfile shows no `rust_version`) |
| Workspace MSRV status | OK |
| Used by | `evidence_store::embeddings::OrtOnnxRuntime` — XLM-RoBERTa byte-level BPE tokenisation before ONNX inference |
| Action | **None — on latest** |

`tokenizers` is the HuggingFace tokenisation library used by the
optional ONNX embedding lane in `evidence_store::embeddings`.
The dep is gated behind the `onnx-runtime` feature so workspace
builds without `--features onnx-runtime` skip the dep entirely.
The `default-features = false, features = ["fancy-regex"]` mix
swaps the default `onig` (C library) backend for the pure-Rust
`fancy-regex` backend, which is what the CI matrix expects.

### `ort = "=2.0.0-rc.10"` (feature-gated in `evidence_store`)

| Field | Value |
|---|---|
| Current pin | `ort = "=2.0.0-rc.10"` (gated behind `onnx-runtime` feature) |
| Resolves to | `ort 2.0.0-rc.10` |
| Latest published | `2.0.0-rc.12` |
| Upstream MSRV (rc.10) | `1.81` (fits workspace floor) |
| Upstream MSRV (rc.11 / rc.12) | `1.88` — **above workspace MSRV `1.85`** |
| Workspace MSRV status | OK at rc.10; rc.11+ would break the MSRV CI gate |
| Used by | `evidence_store::embeddings::OrtOnnxRuntime` — XLM-RoBERTa ONNX inference for the vector lane |
| Action | **Pin retained — bump gated on MSRV `≥1.88`** |

The `=` (exact-version) pin is intentional and reflects two
independent constraints:

1. **`rc.12` `vitis` execution provider build break.** `rc.12`
   added a Xilinx Vitis AI execution-provider field that
   references a member that does not exist on the bundled
   `OrtApi` struct, so a `cargo build --all-features` on the
   workspace fails to compile against the current `onnxruntime`
   sources. The same break sits on `rc.11`. Upstream's fix has
   not yet landed in a published `rc.13`.
2. **MSRV floor.** Even if `rc.12`'s `vitis` were patched,
   `rc.11` and `rc.12` both declare `rust-version = "1.88"` in
   their published metadata, which is above the workspace's
   `1.85` floor. Bumping `ort` past `rc.10` is therefore double-
   gated: it needs both the upstream build break resolved *and*
   a workspace MSRV bump to `≥1.88` (the same threshold the
   `napi_addon` crate already requires).

The pin policy for this dep is: track `rc.10` until a published
`rc.13+` lands with the `vitis` field corrected AND the workspace
MSRV reaches `1.88`. The matching `Cargo.toml` comment lives at
[`crates/evidence_store/Cargo.toml`](../crates/evidence_store/Cargo.toml)
on the `ort` line.

## Indirectly-multilingual deps (out of audit scope, listed for completeness)

These are not in the multilingual surface but interact with it in
ways worth flagging for future maintainers:

- **`rusqlite = "0.36"`** — pinned at `0.36.x` (workspace MSRV
  ceiling, would otherwise be on `0.37`+). The bundled SQLite
  carries the `unicode61` and `trigram` FTS5 tokenisers used by
  the multilingual lexical lane (Phases 1.2 / 1.2.1 / 1.8 / 1.9).
  See the inline pin-rationale comment block above the
  `rusqlite = { version = "0.36", … }` line in
  [`Cargo.toml`](../Cargo.toml).
- **`unicode-segmentation`** — `1.13.x`, **transitive only** via
  `convert_case` ← `napi-derive`. Not a direct workspace concern;
  not used by any substrate code path. If a future Phase ever
  wants grapheme-cluster segmentation (currently the substrate
  uses code-point iteration via `char::is_alphabetic` /
  `unicode_script`-style predicates implemented inline in
  [`crates/evidence_store/src/script.rs`](../crates/evidence_store/src/script.rs)),
  this is the natural candidate.
- **`unicode-bidi`** — not in the workspace graph today.
  Bidi-control marks are preserved verbatim through identity
  (pinned by the Phase 2.3 `sync_engine` multilingual contract
  test suite at
  [`crates/sync_engine/tests/multilingual_contract.rs`](../crates/sync_engine/tests/multilingual_contract.rs)),
  but no substrate code path needs structural bidi analysis. If
  that ever changes, this is the canonical crate.

## Future-bump policy

When the workspace MSRV is bumped, walk this list in order:

1. **`1.86`** — unlocks `criterion 0.8`. No multilingual surface
   changes, just the bench harness.
2. **`1.88`** — unlocks `ort 2.0.0-rc.11`+ (still gated on the
   upstream `vitis` field fix), `napi-rs 3.x` on the workspace
   MSRV gate (today excluded), and `idna_adapter 1.x` /
   `icu_normalizer 2.2+` if/when the substrate ever wants a
   reqwest dev-dep. No multilingual *direct* dep moves at this
   threshold beyond `ort`.
3. **`1.91`** — drops the workspace's `async-trait` dep (regular
   `async fn` in traits via AFIT-on-dyn-trait). No multilingual
   surface changes.
4. **`1.94`** — unlocks `rusqlite 0.37`+ / `libsqlite3-sys 0.36`+
   (the `cfg_select!` macro that didn't stabilise until 1.94).
   The bundled SQLite version would advance too, which would
   refresh the `unicode61` / `trigram` tokenisers — verify the
   multilingual lexical-lane tests still pass and re-run the
   Phase 2.1 cross-lingual recall benchmark to detect tokenisation
   regressions.

Subsequent freshness audits should refresh this document and the
inline comments together; the pin-rationale comments stay
authoritative for the per-dep "why" and this document stays
authoritative for the "when last verified".
