# Crypto Fuzz Targets

Fuzz harnesses for the `crypto` crate's core primitives using
[`cargo-fuzz`](https://github.com/rust-fuzz/cargo-fuzz) (libFuzzer).

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
| `fuzz_aead_roundtrip` | Random key/nonce/plaintext/AAD through `encrypt_aead` → `decrypt_aead`. Verifies round-trip correctness and tamper detection. |
| `fuzz_hkdf_derive` | Random master key + context label through `derive_key`. Verifies output length, determinism, and no panics. |
| `fuzz_sphincs_sign_verify` | Random message bytes through SPHINCS+ `sign_bytes` → `verify_bytes`. Verifies round-trip and tamper rejection. |

## Running

From the **repository root**:

```bash
# Run a specific target (Ctrl+C to stop):
cargo +nightly fuzz run fuzz_aead_roundtrip    -- -max_len=4096
cargo +nightly fuzz run fuzz_hkdf_derive       -- -max_len=1024
cargo +nightly fuzz run fuzz_sphincs_sign_verify -- -max_len=4096

# Run with a time limit (e.g. 60 seconds):
cargo +nightly fuzz run fuzz_aead_roundtrip -- -max_total_time=60

# List all available targets:
cargo +nightly fuzz list
```

All commands must be run from the `crates/crypto/fuzz/` directory, or
pass `--manifest-path crates/crypto/fuzz/Cargo.toml`.

## Corpus & Artifacts

- Corpora are stored in `fuzz/corpus/<target_name>/` (git-ignored).
- Crash-reproducing inputs land in `fuzz/artifacts/<target_name>/`.

To reproduce a crash:

```bash
cargo +nightly fuzz run fuzz_aead_roundtrip fuzz/artifacts/fuzz_aead_roundtrip/<crash_file>
```
