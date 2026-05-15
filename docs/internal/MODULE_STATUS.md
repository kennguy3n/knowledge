# Module Status Matrix

This file is the single source of truth for **what is actually
implemented vs. what is a contract, mock, or test harness** in the
Knowledge substrate workspace. It is intentionally honest: if you
need to ship something, read the **Status** and **Notes** columns
before quoting any line from [PROGRESS.md](./PROGRESS.md) or
the public-facing [README.md](../../README.md).

The three categories are:

| Category          | What it means in this repo                                                                                                                       |
| ----------------- | ------------------------------------------------------------------------------------------------------------------------------------------------ |
| **Runtime-ready** | The crate compiles and runs against real data, with real cryptography / real storage / real algorithms. Production-ready security review still pending. |
| **Contract/spec** | Types, traits, and tests exist and the public API is stable, but the runtime path delegates to a stub, a skeleton, or a fixture parser.         |
| **Mock/test/demo**| The crate exists only to exercise other crates — tests, the `demo` driver, fixture harnesses, etc.                                              |

The body of the table below uses plain text labels (`Runtime-ready` /
`Contract/spec` / `Mock/test/demo`) for grep-ability so reviewers can
`rg 'Contract/spec' docs/internal/MODULE_STATUS.md` without worrying
about Unicode glyphs in their terminal font.

---

## Per-crate matrix

| Crate                  | Category       | Status                          | Notes                                                                                                                                         |
| ---------------------- | -------------- | ------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------- |
| `agent_contract`       | Runtime-ready  | Real implementation              | Capability tokens, audit hooks, scope binding. Used by `export_plane` to gate `agent_query`.                                                  |
| `audit_service`        | Runtime-ready  | Real implementation              | Append-only audit log over SQLCipher; integrates with attestation audit entries.                                                              |
| `concept_graph`        | Runtime-ready  | Real implementation              | Concept extraction, edge typing, hierarchy enforcement. Indices backed by real SQL.                                                           |
| `connector_framework`  | Contract/spec  | OAuth2 / token types only        | OAuth2 + delta-token types and rate-limit shells are defined; no real HTTP transport, retry, or refresh-token plumbing yet.                   |
| `connectors`           | Contract/spec  | Fixture parsers, no live APIs    | Slack / Drive / GitHub / Jira / Confluence / OneDrive / Notion / Figma parsers consume JSON fixtures committed in `crates/connectors/fixtures/`. None hit a live API. |
| `crypto`               | Runtime-ready  | Real PQ + classical primitives   | BLAKE3, XChaCha20-Poly1305, HKDF, HMAC, X25519, ML-KEM-768, ML-DSA-65 are all real. **`StubKemBackend`, `TestSigner`, `TEST_SIGNER_KEY_LEN`** are gated behind `cfg(any(test, feature = "test-support"))`. **SPHINCS+ is a BLAKE3-based stub** (see `crypto::sphincs` module docs). See "Known security debt" below. |
| `demo`                 | Mock/test/demo | Scaffolding driver only          | The CLI demo binary opts into `crypto/test-support`, `synthesis_pipeline/test-support`, and `synthesis_engine/test-support` because it wires `TestSigner` + `NoOpSynthesizer` + `MockTeeRuntime` end-to-end. Not a production entrypoint. |
| `evidence_store`       | Runtime-ready  | Real SQLCipher, AEAD, FTS5       | SQLCipher (AES-256-CBC + HMAC-SHA512 per page), per-scope AEAD, content-hash dedup, ring buffer FIFO, FTS5 retrieval all run on real SQLite. Embedding pipeline is a skeleton (XLM-R adapter pending). **FTS5 index retains plaintext after scope DEK destruction** — see `evidence_store/tests/forgetting_fts.rs` and the "Known security debt" section below. |
| `export_plane`         | Runtime-ready  | Real implementation              | Agent-facing read APIs, scope enforcement, redaction; integrates with `agent_contract`.                                                       |
| `ffi`                  | Contract/spec  | Skeleton, every export `Unimplemented` | UniFFI / cbindgen build pipeline produces real artifacts but every exported function returns the `Unimplemented` error variant. Host UI wiring lands in a later phase. |
| `inference_router`     | Contract/spec  | Routing logic only               | Cost/latency/policy router compiles and the trait surface is real, but the backend adapters (Bonsai-1.7B on-device, managed cloud endpoints) are stubs returning placeholder payloads. |
| `memory_manager`       | Runtime-ready  | Real state machine               | Decay state machine, retention scoring, working-memory promotion/eviction, lexicon classifier. No mocks.                                      |
| `napi`                 | Contract/spec  | Forwards to FFI                  | N-API addon for macOS / Windows; forwards every call into the FFI skeleton, which returns `Unimplemented`.                                    |
| `observation_engine`   | Contract/spec  | SLM-assisted pipeline skeleton   | Window manager + observation router are real; the SLM observer path is wired to the inference-router stub adapter so it does not yet run a real Bonsai-1.7B. |
| `permission_service`   | Runtime-ready  | Real implementation              | Tenant / domain / scope permission rules; backed by SQLCipher tables.                                                                          |
| `reasoning_engine`     | Contract/spec  | Graph-of-thought skeleton        | Plan / step / graph types and the `ReasoningEngine` trait compile and have tests, but the planner relies on stub inference and does not yet run a real LLM call. |
| `sync_engine`          | Contract/spec  | Election + handshake types only  | Elected-device handshake / window-publish protocol types exist; live multi-device sync transport is not wired.                                |
| `synthesis_engine`     | Contract/spec  | TEE skeleton, mock runtime       | Lifecycle, attestation audit, scope binding all compile and have unit tests. **`MockTeeRuntime`** is feature-gated behind `cfg(any(test, feature = "test-support"))`. Server synthesizer (`ManagedEndpointSynthesizer`) is a stub. No real Intel TDX / AMD SEV-SNP / AWS Nitro Enclaves integration. |
| `synthesis_pipeline`   | Contract/spec  | Pipeline skeleton, no-op synth   | Window manager, publish/consume encryption, GBNF schema, hierarchy enforcement are real. **`NoOpSynthesizer`** is feature-gated behind `cfg(any(test, feature = "test-support"))`. Real SLM-backed synthesizer (Bonsai-1.7B via `kennguy3n/llama.cpp@prism`) is pending the inference-router adapter. |
| `tenant_service`       | Runtime-ready  | Real implementation              | Tenant lifecycle, scope CRUD, retention policy resolution.                                                                                    |

---

## Feature-gating contract

Three cargo features track the mock/stub split:

* `crypto/test-support` — exposes `StubKemBackend`, `TestSigner`,
  `TEST_SIGNER_KEY_LEN`.
* `synthesis_pipeline/test-support` — exposes `NoOpSynthesizer`.
* `synthesis_engine/test-support` — exposes `MockTeeRuntime`.

**Important Cargo build-model detail:** `cfg(test)` is only set on the
crate being compiled *as a test target* (unit tests inside `src/` or
the integration-test binary itself). It is **not** set on library
dependencies of the test binary. This means an integration test in
`crates/crypto/tests/provenance.rs` that imports `TestSigner` from the
`crypto` library will fail unless the library is compiled with
`feature = "test-support"`.

To handle this, each crate that has integration tests using its own
gated types carries a **self-referential dev-dependency** that enables
the feature during `cargo test`:

```toml
# crates/crypto/Cargo.toml
[dev-dependencies]
crypto = { path = ".", features = ["test-support"] }
```

This is the same pattern used by `tokio`, `serde`, and `clap`. It
ensures that `cargo test -p crypto` (without `--all-features`) compiles
correctly.

Downstream crates (`evidence_store/tests/`, `synthesis_engine/tests/`,
`demo`) that need gated types from *another* crate enable that crate's
`test-support` feature explicitly in their `[dev-dependencies]` or
`[dependencies]` block; this keeps the workspace `cargo build` (no
features) free of mock types in the lib output.

`cargo test --all --all-features` (the CI command) activates every
`test-support` feature in the workspace via cargo's feature
unification, so CI always exercises both the gated and the un-gated
code paths.

---

## Known security debt

The following items are honest gaps tracked here so they cannot be
silently re-marketed as "complete":

1. **FTS5 plaintext index survives scope DEK destruction.**
   `evidence_store` keeps a SQLite FTS5 virtual table for fast
   retrieval. FTS5 stores the *tokenized plaintext* of every ingested
   body — that is how the index works — so zeroizing the scope's
   body AEAD key does NOT remove searchable terms from the index.
   The gap is pinned by
   `crates/evidence_store/tests/forgetting_fts.rs::fts_index_retains_plaintext_after_scope_dek_destruction`.
   Three viable mitigations are listed at the bottom of that file
   (rebuild FTS on key destruction, encrypt FTS terms separately, or
   destroy the entire SQLCipher database key).

2. **`ml-dsa 0.0.4` has a timing side-channel
   (`RUSTSEC-2025-0144`).** The fix lands in `ml-dsa >= 0.1.0-rc.3`,
   which is a substantial API bump for a pre-1.0 RustCrypto crate.
   The upgrade has to be sequenced with the wider Phase 7
   provenance overhaul because `crates/crypto/src/signer_backend.rs`
   and the FFI surface (`crates/ffi/src/types.rs`) currently pin
   the 0.0.4 type names. The CI `cargo audit` step records the
   ignore explicitly on the command line (`--ignore
   RUSTSEC-2025-0144`) so the debt shows up in every CI log.

3. **SPHINCS+ provenance signer is a BLAKE3-derived stub.** The
   module docs in `crates/crypto/src/sphincs.rs` (lines 20-31) are
   already honest about this: the type signatures match SPHINCS+
   wire formats but the underlying construction is a BLAKE3
   keyed-hash, *not* a hash-based signature scheme. Production
   provenance signing uses ML-DSA-65 today; replacing the stub with
   a real SPHINCS+ backend (e.g. via `pqcrypto-sphincsplus`) is
   tracked under Phase 7.

4. **Platform bindings return `Unimplemented`.** `crates/ffi`,
   `crates/napi`, the iOS UniFFI bindings, and the macOS / Windows
   N-API addons all have real build pipelines that emit artifacts,
   but every exported function currently returns the
   `Unimplemented` error variant. Host UI integration is a
   later-phase deliverable.

5. **No live connector traffic.** All eight connectors in
   `crates/connectors/` parse JSON fixtures from
   `crates/connectors/fixtures/`. There is no OAuth2 transport,
   no retry / refresh-token plumbing, and no ACL sync; the
   `connector_framework` provides only the type surface for those.

---

## How to keep this file honest

* When a crate's status changes (e.g. a real SLM adapter lands in
  `inference_router`), update **both** the per-crate row above
  **and** the corresponding section in [PROGRESS.md](./PROGRESS.md).
  A PR that changes one without the other should be blocked in
  review.
* When adding a new mock / stub / fixture-only construct, gate it
  behind `cfg(any(test, feature = "test-support"))` and document
  the gate in the per-crate `Notes` column.
* When closing a "Known security debt" item, delete its entry
  rather than marking it green — empty lists are easier to scan
  than struck-through ones, and CI's `cargo audit` step plus
  `evidence_store::tests::forgetting_fts` will tell you the moment
  the gap actually closes.
