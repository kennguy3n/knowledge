# inference_router

On-device SLM inference routing for the Knowledge substrate.

## Purpose

Every classification / extraction / synthesis call into a Small
Language Model goes through the `InferenceRouter`. It holds an
ordered list of `InferenceAdapter`s (MLX -> llama.cpp -> Fallback),
probes them at boot, and dispatches every `InferenceTask` to the
highest-priority available adapter.

## Public API summary

| Type / Function | Description |
|---|---|
| `InferenceRouter` | Central dispatcher. |
| `InferenceAdapter` | Trait for pluggable backends. |
| `InferenceTask` | Task enum (`SynthSummary`, etc.). |
| `LlamaCppAdapter` / `MlxAdapter` / `FallbackAdapter` | Built-in adapters *(unstable, `#[doc(hidden)]` — prefer `InferenceRouter`)*. |
| `RouterConfig` / `DeviceTier` | Configuration. |
| `SummaryBundle` | Structured output from synthesis tasks. |

## Feature flags

| Feature | Description |
|---|---|
| `http-client` | Reqwest-backed `HttpLlamaServerClient` for llama.cpp loopback. |
| `async-http-client` | Async llama.cpp client under tokio. |
| `test-support` | Exposes test helpers outside `cfg(test)`. |

## Links

- [ARCHITECTURE.md](../../docs/technical/architecture.md) §3 — Inference layer.
- [docs/DESIGN.md](../../docs/DESIGN.md) §6 — On-device model strategy.
- [docs/INTEGRATION_GUIDE.md](../../docs/INTEGRATION_GUIDE.md) — Consumer integration guide.
