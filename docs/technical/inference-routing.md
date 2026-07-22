# Inference Routing

This document specifies how Knowledge dispatches Small Language Model
(SLM) work on-device. It is the reference companion to
[architecture.md §3](architecture.md) and [design.md §6](design.md) and
is implemented by the `inference_router` crate
(`crates/inference_router`).

## One router, ordered adapters

Every classification, extraction, and synthesis call into an SLM goes
through a single place: the `InferenceRouter`. The router holds an
ordered list of `InferenceAdapter`s. With the optional on-device
accelerator adapters compiled in (and preferred — the default), the
full priority order is:

```
CoreML/ANE  →  ONNX-Runtime  →  MLX  →  llama.cpp  →  managed-cloud  →  Fallback
```

Without the accelerator features (the default workspace build) the
order is the classic `MLX → llama.cpp → managed-cloud → Fallback`.

At boot it **probes** each adapter for availability, then dispatches
every `InferenceTask` to the highest-priority adapter that is both
available and supports that task. The router is deliberately small and
synchronous so it can be unit-tested against in-memory mock adapters;
the production runtime drives it from background tokio tasks.

| Adapter | Backend | Feature | When it's selected |
|---|---|---|---|
| **CoreML/ANE** | Apple Core ML on the Neural Engine | `coreml` | Apple silicon / iOS, Medium+ tier, with a loaded Core ML model. Highest priority for lowest latency & battery cost. |
| **ONNX Runtime** | ONNX Runtime Mobile + NPU EP (NNAPI/QNN/Core ML) | `onnx-runtime` | Any platform, Medium+ tier, when an NPU execution provider initialises. |
| **MLX** | Apple MLX | — | Apple silicon with the MLX runtime present. |
| **llama.cpp** | `llama-server` over loopback HTTP | `http-client` | Any platform with a GGUF model and the server reachable. |
| **managed-cloud** | OpenAI-compatible remote endpoint | `http-client` | Synthesis only; opt-in via `KNOWLEDGE_MANAGED_INFERENCE_URL`. Tier-independent (compute is remote). |
| **Fallback** | Deterministic no-op synthesizer | — | Always available; used when no real backend is present (e.g. CI, the quickstart demo). |

The canonical priority order is computed by
`inference_router::ordered_adapter_kinds(&RouterConfig, AcceleratorAvailability)`,
which is the single unit-tested source of truth for adapter ranking and
the accelerator-absent fallback order. Adapter availability is then
applied on top of that order at runtime via each adapter's `probe()` /
`supports()`.

## On-device accelerators (NPU / ANE)

The two accelerator adapters target the on-device NPU paths where the
product must be competitive on latency and battery: **Apple Core ML on
the Neural Engine** (`CoreMlAdapter`) and **ONNX Runtime Mobile** with
an NPU execution provider — NNAPI or QNN/Hexagon on Android, the Core ML
EP on iOS (`OnnxRuntimeAdapter`).

**Where the runtime lives.** As with MLX and `llama-server`, the heavy
accelerator runtime lives in the platform shell (Swift on iOS/macOS, the
Android JNI layer), not linked into the Rust core. The shell constructs
an `AcceleratorBackend` and injects it into the adapter. This keeps the
`inference_router` crate free of platform-specific native build deps, so
the workspace — and the selection/fallback unit tests — build on every
host, including Linux CI with no NPU. The adapters are compiled only
behind their `coreml` / `onnx-runtime` features.

**Capability detection.** A backend reports, on every probe, an
`AcceleratorCapabilities { present, supports_synthesis, deterministic }`:

- `present` — the runtime is linked, the model graph is loaded, and the
  NPU is ready. When `false` the adapter probes `Unavailable` and the
  router skips it.
- `supports_synthesis` — the loaded graph can do generative synthesis,
  not just the fixed-shape classification head.
- `deterministic` — the backend guarantees a reproducible greedy-decode
  path (see the determinism contract below).

**Graceful fallback.** Selection is purely additive: if the accelerator
is off-platform, its runtime is absent, the device is Low-tier, or it
declines a task, the adapter simply reports unavailable / unsupported and
the router rolls to the next adapter — MLX, then llama.cpp, then
managed-cloud, then the deterministic fallback. No task ever fails just
because an accelerator is missing.

## Device-tier gating

On-device inference must run within real memory budgets (2–8 GB RAM on
target devices). The router pairs with a device-tier classification so
that a model is only selected on a device that can actually run it;
lower-tier devices degrade to a lighter task profile or the fallback
rather than thrashing. Per-platform tuning (ANR watchdogs, idle-window
processing, background-fetch policy) lives in
[platforms.md](platforms.md).

The accelerator adapters follow the same tier profile as the SLM
adapters: **Low** tier never admits an on-device accelerator (even when
the NPU exists, the model + context working set does not fit the budget,
so the router uses managed-cloud / fallback); **Medium** tier admits the
accelerator for cheap **classification/extraction** only; **High** tier
additionally admits **synthesis**, subject to the determinism contract
below. `RouterConfig::prefer_accelerator` (default `true`) controls
whether an available accelerator is ranked ahead of MLX / llama.cpp;
setting it `false` keeps the SLM path primary and uses the accelerator
only as a fallback (useful while validating a new accelerator backend in
production).

### Model size: Qwen3.5-0.8B and Qwen3.5-2B

The default synthesis models are **Qwen3.5-0.8B Q4_K_M** (Medium-tier)
and **Qwen3.5-2B Q4_K_M** (High-tier / server-side), sized to run within
the on-device RAM budgets above. The router selects the appropriate model
based on the device tier; the adapter contract does **not** change, because
the output shape is GBNF-grammar-guaranteed regardless of model size.

Operators can override the bundled model via one of:

- a `llama-server` image built with a different `MODEL_URL` build-arg, or a
  runtime bind-mount of an alternative GGUF over the baked model path; or
- `KNOWLEDGE_SLM_MODEL_PATH` pointing at alternative weights for native /
  on-device builds.

The artifacts and exact override commands are documented in
[`deploy/model-artifacts/README.md`](../../deploy/model-artifacts/README.md).

## Task profile

`InferenceTask` describes the unit of work (classification vs.
extraction vs. synthesis) and its constraints. Adapters declare which
task kinds they support, so the router never dispatches a task to a
backend that cannot serve it. This keeps the dispatch decision a pure
function of `(available adapters, task kind, device tier)`.

## Wiring a real model

The quickstart runs against the deterministic fallback so it needs no
model. To run real inference, stand up `llama-server` with a GGUF model
(e.g. Qwen3.5-2B) and build with the `http-client` feature so the
llama.cpp adapter becomes available — see the
[quickstart "Wiring a real SLM" section](../QUICKSTART.md#wiring-a-real-slm-optional).

## Deterministic sampling

Synthesis here is **extraction-like** — a faithful condensation of the
evidence, not creative generation — so the router samples
deterministically by default. Every `/completion` (llama.cpp) and
`/chat/completions` (managed-cloud) request carries an explicit
**`seed`** plus the full sampling parameter set. This is the fix for
the run-to-run inconsistency documented in
[the synthesis-quality blog post](../../blog/executive-personas/03-synthesis-quality.md):
with `llama-server`'s default seed (`-1`) an identical `(model,
prompt)` pair drew a fresh sample every call, so the same scope could
yield a clean briefing one run and rambling meta-commentary the next.
A fixed seed + greedy decode makes the mapping byte-reproducible.

The defaults live in `SamplingConfig::synthesis_default()` and every
field is overridable via a `KNOWLEDGE_SLM_*` environment variable
(same convention as `KNOWLEDGE_LLAMA_SERVER_URL`). Unset or malformed
values fall back to the default independently, so a typo in one knob
never silently disables determinism for the rest.

| Field | Env var | Default | Meaning |
|---|---|---|---|
| `seed` | `KNOWLEDGE_SLM_SEED` | `0` | RNG seed; `-1` restores non-deterministic sampling. |
| `temperature` | `KNOWLEDGE_SLM_TEMPERATURE` | `0.0` | `0.0` = greedy (most-likely token). |
| `top_k` | `KNOWLEDGE_SLM_TOP_K` | `1` | Keep only the `k` most-likely tokens. |
| `top_p` | `KNOWLEDGE_SLM_TOP_P` | `0.9` | Nucleus cutoff (inert under greedy). |
| `min_p` | `KNOWLEDGE_SLM_MIN_P` | `0.05` | Minimum-probability floor (inert under greedy). |
| `repeat_penalty` | `KNOWLEDGE_SLM_REPEAT_PENALTY` | `1.1` | Mild penalty against degenerate token loops. |
| `n_predict` | `KNOWLEDGE_SLM_N_PREDICT` | `512` | Token budget for classification/extraction tasks (see synthesis note below). |

The llama.cpp adapter sends all seven fields. The managed-cloud
adapter sends only the OpenAI-portable subset (`seed`, `temperature`,
`top_p`, `max_tokens` ← `n_predict`); the llama.cpp-only knobs
(`top_k` / `min_p` / `repeat_penalty`) are omitted because strict
OpenAI endpoints reject unknown sampling parameters.

> **`n_predict` and synthesis.** `KNOWLEDGE_SLM_N_PREDICT` governs the
> token budget for the plain `dispatch()` path (entity extraction,
> promotion, concept/contradiction tasks). It does **not** govern the
> `SynthSummary` budget: the synthesis pipeline computes an *adaptive*
> budget per window (`adaptive_budget`, clamped to `[512, 1024]`, with a
> verify-and-retry second attempt up to `1536`) so a large evidence
> window cannot run generation long enough to trip the substrate
> synthesis deadline (the prior cause of 502s) while a small window is
> not over-budgeted. The `512` floor matches the env default, so the
> change is only observable for hosts that raised `KNOWLEDGE_SLM_N_PREDICT`
> above `512` and expected synthesis to inherit it. This split is
> deliberate: deadline safety for synthesis is owned by the adaptive
> budget, not by an operator-tunable knob.

A non-finite float override (`KNOWLEDGE_SLM_TEMPERATURE=nan`, `inf`,
`-inf`) is rejected and the field keeps its deterministic default — a
non-finite float serialises as JSON `null`, which the endpoints reject,
so it is treated like any other malformed value rather than poisoning
the request body.

### MLX (Apple silicon)

The MLX adapter delegates to the native Swift engine, which owns its
own sampler. The default `MlxGenerateFn` callback is
`fn(task_tag, prompt, grammar) -> String` and carries no
`SamplingConfig`, so the `KNOWLEDGE_SLM_*` knobs above (including
`seed`) — which reach the llama.cpp and managed-cloud request bodies —
do **not** reach the MLX runtime through it. To honour per-call
sampling (the fixed seed, or this pipeline's adaptive `n_predict`
budget), an Apple-silicon shell registers the sampling-aware callback
`set_mlx_generate_with_sampling_fn`; `MlxAdapter::generate_with_sampling`
routes through it when present and otherwise falls back to the plain
callback. Absent a registered sampling-aware callback, MLX synthesis
uses the native engine's own sampling defaults, so reproducibility on
Apple silicon depends on the native engine's seeding rather than
`KNOWLEDGE_SLM_SEED`.

### Accelerators (CoreML/ANE, ONNX Runtime)

NPU/ANE kernels are frequently fused and/or fixed-point and need **not**
produce bit-identical logits across OS versions or driver revisions, so
the byte-reproducible synthesis contract cannot be assumed to hold on an
accelerator. The router preserves the contract explicitly rather than
silently weakening it:

- A backend advertises whether it guarantees a reproducible
  greedy-decode path via `AcceleratorCapabilities::deterministic`.
- When `RouterConfig::require_deterministic_synthesis` is `true` (the
  default), a **synthesis** task is admitted by an accelerator *only* if
  its backend reports `deterministic`. Otherwise the accelerator
  declines synthesis (via `supports()`) and the router falls through to
  the byte-reproducible llama.cpp / MLX / CPU path. Set the knob `false`
  (`KNOWLEDGE_SLM_REQUIRE_DETERMINISTIC=0`) to allow a non-deterministic
  accelerator to serve synthesis where the host accepts the trade-off.
- **Classification and extraction are always admitted** on the
  accelerator regardless of `deterministic`: these tasks are GBNF-
  constrained argmax decisions over a small label set, which are robust
  to the small numerical differences an accelerator introduces, so the
  selected token is stable in practice.

The sampling config (`seed`, `temperature`, `top_k`, …) is threaded to
the accelerator backend on every call exactly as it is for the other
adapters, so a backend that *can* honour a fixed seed receives it; the
`deterministic` flag is the backend's attestation that it does.

The two accelerator selection knobs follow the same `KNOWLEDGE_SLM_*`
environment convention (accepting `1`/`0`, `true`/`false`, `yes`/`no`,
`on`/`off`; an unset or malformed value keeps the safe default):

| Field | Env var | Default | Meaning |
|---|---|---|---|
| `require_deterministic_synthesis` | `KNOWLEDGE_SLM_REQUIRE_DETERMINISTIC` | `true` | Only admit accelerator synthesis when the backend attests determinism. |
| `prefer_accelerator` | `KNOWLEDGE_SLM_PREFER_ACCELERATOR` | `true` | Rank an available accelerator ahead of MLX / llama.cpp. |

## Bring your own model

The adapter list is the extension point. To route to a different
runtime or a custom endpoint, implement `InferenceAdapter` and insert it
at the desired priority. See
[../guides/custom-synthesis.md](../guides/custom-synthesis.md).

## Further reading

- [architecture.md §3](architecture.md) — where inference sits in the data flow.
- [design.md §6](design.md) — inference design rationale.
- [platforms.md](platforms.md) — per-device tuning.
- [../guides/custom-synthesis.md](../guides/custom-synthesis.md) — configuring inference.
