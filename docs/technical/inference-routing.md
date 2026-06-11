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
