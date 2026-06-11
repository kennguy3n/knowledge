# Inference Routing

This document specifies how Knowledge dispatches Small Language Model
(SLM) work on-device. It is the reference companion to
[architecture.md §3](architecture.md) and [design.md §6](design.md) and
is implemented by the `inference_router` crate
(`crates/inference_router`).

## One router, ordered adapters

Every classification, extraction, and synthesis call into an SLM goes
through a single place: the `InferenceRouter`. The router holds an
ordered list of `InferenceAdapter`s — currently:

```
MLX  →  llama.cpp  →  Fallback
```

At boot it **probes** each adapter for availability, then dispatches
every `InferenceTask` to the highest-priority adapter that is both
available and supports that task. The router is deliberately small and
synchronous so it can be unit-tested against in-memory mock adapters;
the production runtime drives it from background tokio tasks.

| Adapter | Backend | When it's selected |
|---|---|---|
| **MLX** | Apple MLX | Apple silicon with the MLX runtime present. |
| **llama.cpp** | `llama-server` over loopback HTTP | Any platform with a GGUF model and the server reachable (requires the `http-client` feature). |
| **Fallback** | Deterministic no-op synthesizer | Always available; used when no real backend is present (e.g. CI, the quickstart demo). |

## Device-tier gating

On-device inference must run within real memory budgets (2–8 GB RAM on
target devices). The router pairs with a device-tier classification so
that a model is only selected on a device that can actually run it;
lower-tier devices degrade to a lighter task profile or the fallback
rather than thrashing. Per-platform tuning (ANR watchdogs, idle-window
processing, background-fetch policy) lives in
[platforms.md](platforms.md).

### Model size: 1.7B default, optional 4B upgrade

The default synthesis model everywhere is **Bonsai-1.7B Q2_0** (2-bit
ternary), sized to run within the on-device RAM budgets above. A larger
**Bonsai-4B Q2_0** model is available as an **opt-in** quality upgrade for
**server-side / High-tier** deployments that have the headroom — it is
**not** the default for anyone, and on-device Low/Medium tiers stay on
1.7B.

Selecting 4B is purely a deployment/configuration choice; the router's
adapter contract does **not** change, because the output shape is
GBNF-grammar-guaranteed regardless of model size. A 4B host opts in via
one of:

- a `llama-server` image built with the 4B `MODEL_URL` build-arg, or a
  runtime bind-mount of `bonsai-4b.gguf` over the baked model path; or
- `KNOWLEDGE_SLM_MODEL_PATH` pointing at the 4B weights for native /
  on-device builds (defaults to the 1.7B path when unset).

The artifacts and exact opt-in commands are documented in
[`deploy/model-artifacts/README.md`](../../deploy/model-artifacts/README.md)
(see "Selecting the 4B model"). Because the 4B artifact may not be
published for a release yet, its download checksums are left unpinned with
a TODO — fetching it requires the explicit `--include-4b` opt-in flag, so
the default 1.7B path is never affected.

## Task profile

`InferenceTask` describes the unit of work (classification vs.
extraction vs. synthesis) and its constraints. Adapters declare which
task kinds they support, so the router never dispatches a task to a
backend that cannot serve it. This keeps the dispatch decision a pure
function of `(available adapters, task kind, device tier)`.

## Wiring a real model

The quickstart runs against the deterministic fallback so it needs no
model. To run real inference, stand up `llama-server` with a GGUF model
(e.g. Bonsai-1.7B) and build with the `http-client` feature so the
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
