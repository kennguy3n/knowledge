# Dependencies and MSRV policy

This document describes the workspace's minimum supported Rust
version (MSRV) and the rationale for any direct dependency that is
deliberately pinned behind its latest published line. Each pinned
dependency also has the same rationale inline next to its
`version = "…"` declaration in the relevant `Cargo.toml`; this
document is the cross-cutting summary, but the `Cargo.toml`
comments are authoritative if the two ever drift.

## MSRV

- **Workspace floor:** `rust-version = "1.85"` (declared at the
  root [`Cargo.toml`](../Cargo.toml) and enforced in CI by the
  `MSRV (1.85.0)` job in
  [`.github/workflows/ci.yml`](../.github/workflows/ci.yml),
  which pins the toolchain via `dtolnay/rust-toolchain@1.85.0`
  and runs `cargo check --workspace --all-targets --exclude
  napi_addon`).
- **Why `1.85`:** the `ml-dsa 0.1.0` post-quantum signature
  backend declares `edition = "2024"` internally, which requires
  Rust 1.85+.
- **`napi_addon` exception:** the Node N-API addon crate at
  [`crates/napi/Cargo.toml`](../crates/napi/Cargo.toml) carries
  its own `rust-version = "1.88"` because `napi-rs 3.x`
  independently requires `rustc ≥ 1.88`. The MSRV gate excludes
  this crate; the addon is built separately by the host shell's
  toolchain.

## Notable dependency pins

### `rusqlite = "0.36"` (workspace)

Pinned at the `0.36.x` line. `rusqlite 0.37`+ depends on
`libsqlite3-sys 0.36`+, which uses the `cfg_select!` macro that
did not stabilise until Rust 1.94. Bumping `rusqlite` past
`0.36.x` is gated on a workspace MSRV bump to `1.94`.

The bundled SQLite version (via the
`bundled-sqlcipher-vendored-openssl` feature) is `3.46.1`,
shipped inside the SQLCipher 4.6.1 vendored fork. A canary test
suite at
[`crates/evidence_store/tests/bundled_sqlite_canary.rs`](../crates/evidence_store/tests/bundled_sqlite_canary.rs)
asserts on the literal `sqlite_version()` and
`sqlite_source_id()` so any future `rusqlite` /
`libsqlite3-sys` bump that moves the bundled SQLite forward
forces a deliberate ack — the canary docs include the audit
procedure (re-run the FTS5 tokeniser tests, re-run the
cross-lingual recall benchmark, audit upstream changelog between
versions).

### `ort = "=2.0.0-rc.10"` (in `evidence_store`, feature-gated)

Exact-version pin gated behind the `onnx-runtime` feature. Two
independent constraints:

1. **Upstream build break on `rc.11` / `rc.12`.** `rc.11`+ added
   a Xilinx Vitis AI execution-provider field referencing a
   member that does not exist on the bundled `OrtApi` struct,
   so `cargo build --all-features` fails to compile against the
   current `onnxruntime` sources. Upstream's fix has not yet
   landed in a published `rc.13`.
2. **MSRV floor.** `rc.11` and `rc.12` both declare
   `rust-version = "1.88"`, which is above the workspace's
   `1.85` floor.

Bumping `ort` past `rc.10` is therefore double-gated: both the
upstream `vitis` field needs to be fixed and the workspace MSRV
needs to reach `1.88`.

### `criterion = "0.7"` (workspace)

Pinned at the `0.7.x` line. `criterion 0.8`+ declares
`rust-version = "1.86"`, above the workspace MSRV. Bumping to
`0.8` is gated on a workspace MSRV bump to `1.86`.

The `0.5` → `0.6` transition moved `criterion::black_box` to a
deprecation; the workspace bench files (`crypto`,
`concept_graph`, `evidence_store`, `integration_tests`) now
import `black_box` from `std::hint::` directly.

### `aws-nitro-enclaves-nsm-api = "0.4"` (workspace)

Pinned at the `0.4.x` line. `0.5`+ declares
`rust-version = "1.92"`, above the workspace MSRV. Used only by
the optional `nitro-tee` feature; production builds without
that feature do not pull this crate.

## Dependabot ignore rules

The pin rationales above are mirrored as `versions:` blocks in
[`.github/dependabot.yml`](../.github/dependabot.yml) so the
Dependabot queue does not cycle unmergeable PRs for the gated
ranges. The blocks are:

- `rusqlite`: `versions: [">=0.37"]`
- `ort`: `versions: [">=2.0.0-rc.11"]`
- `criterion`: `versions: [">=0.8"]`
- `aws-nitro-enclaves-nsm-api`: `versions: [">=0.5"]`

Patch and intermediate bumps within each pinned line still
surface as Dependabot PRs.

## Text-handling dependencies (no pins)

These crates handle human-language text in the substrate's
multilingual paths. All are tracked on their latest published
line; no MSRV gating applies.

- **`whatlang = "0.18"`** — pure-Rust trigram language detector.
  Used by `observation_engine::detect_language` (`is_reliable()`-
  filtered for confident BCP-47 stamping) and
  `evidence_store::embedding_routing::classify_for_embedding`
  (looser admit criterion — anything with linguistic content
  routes to embedding). `whatlang 0.18` does not ship trigram
  classifiers for Tibetan or Lao; the FTS5 CJK / bigram routing
  predicate keys on **script presence** (Unicode code-point
  ranges) rather than the language tag so detection refusal
  does not silently drop those scripts out of the recall lane.
- **`unicode-normalization = "0.1"`** — NFC normalisation, one-
  shot at extractor time. Used by `observation_engine::lexicon`
  before the interrogative first-token table lookup. `no_std`-
  compatible.
- **`tokenizers = "0.23"`** — HuggingFace tokenisation library
  for the optional ONNX embedding lane. Feature-gated behind
  `evidence_store/onnx-runtime`. Built with
  `default-features = false, features = ["fancy-regex"]` to
  swap the default C-library `onig` backend for the pure-Rust
  `fancy-regex` backend.

## When the workspace MSRV is bumped

Walk this checklist in order:

1. **`1.86`** — unlocks `criterion 0.8`. No multilingual
   surface changes.
2. **`1.88`** — unlocks `ort 2.0.0-rc.11`+ (still gated on the
   upstream `vitis` field fix), brings `napi-rs 3.x` into the
   workspace MSRV gate's surface, and would allow
   `idna_adapter 1.x` / `icu_normalizer 2.2+` if a `reqwest`
   dev-dep ever becomes useful.
3. **`1.91`** — drops the workspace's `async-trait` dep
   (regular `async fn` in traits via AFIT-on-dyn-trait).
4. **`1.94`** — unlocks `rusqlite 0.37`+ / `libsqlite3-sys
   0.36`+ (uses the `cfg_select!` macro that stabilised at
   1.94). The bundled SQLite version would advance, refreshing
   the `unicode61` / `trigram` tokenisers; verify the
   multilingual lexical-lane tests still pass and re-run the
   cross-lingual recall benchmark to detect tokenisation
   regressions.
