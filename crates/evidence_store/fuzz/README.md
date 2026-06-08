# Evidence-store Fuzz Targets

Fuzz harnesses for the `evidence_store` crate's text-tokenisation path
using [`cargo-fuzz`](https://github.com/rust-fuzz/cargo-fuzz) (libFuzzer).

## Prerequisites

```bash
cargo install cargo-fuzz
```

Requires a **nightly** toolchain (libFuzzer is a nightly-only feature):

```bash
rustup toolchain install nightly
```

## Available Targets

| Target | What it tests |
|---|---|
| `fuzz_cjk_bigram_tokenizer` | Arbitrary UTF-8 (CJK / Thai / Arabic / combining-mark / U+FFFD edge cases) through `compute_cjk_bigrams` (index side) and `compute_cjk_bigram_query` (query side). Verifies neither panics and that both sides agree on the bigram count, consistent with the `script::contains_cjk_or_thai` router. |

## Running

From the `crates/evidence_store/fuzz/` directory (or with
`--manifest-path crates/evidence_store/fuzz/Cargo.toml`):

```bash
# Run for a fixed time (e.g. 60s):
cargo +nightly fuzz run fuzz_cjk_bigram_tokenizer -- -max_total_time=60

# Run until Ctrl+C:
cargo +nightly fuzz run fuzz_cjk_bigram_tokenizer

# List all available targets:
cargo +nightly fuzz list
```

## Corpus & Artifacts

- Corpora are stored in `fuzz/corpus/<target_name>/` (git-ignored).
- Crash-reproducing inputs land in `fuzz/artifacts/<target_name>/`.

To reproduce a crash:

```bash
cargo +nightly fuzz run fuzz_cjk_bigram_tokenizer fuzz/artifacts/fuzz_cjk_bigram_tokenizer/<crash_file>
```
