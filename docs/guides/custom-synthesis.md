# Custom Synthesis (bring your own model)

How to control which model performs synthesis and how inference is
routed. Read [inference-routing.md](../technical/inference-routing.md)
first for the model; this guide is the how-to.

## How routing works

All SLM work goes through the `InferenceRouter`, which holds an ordered
list of adapters and dispatches each task to the highest-priority
adapter that is available and supports it:

```
MLX  →  llama.cpp  →  Fallback
```

The **Fallback** is a deterministic no-op synthesizer that is always
available — it's what runs in the quickstart demo and CI, so nothing
requires a model to be present.

## Option 1: wire llama.cpp (a GGUF model)

The simplest real backend. Stand up `llama-server` with a GGUF model
(e.g. Bonsai-1.7B) and build the substrate with the `http-client`
feature so the llama.cpp adapter becomes available and reachable over
loopback. See the
[quickstart "Wiring a real SLM" section](../QUICKSTART.md#wiring-a-real-slm-optional).

## Option 2: use MLX on Apple silicon

On Apple silicon with the MLX runtime present, the MLX adapter is probed
at boot and selected ahead of llama.cpp. No code changes — it's a
priority-ordered fallthrough.

## Option 3: bring your own adapter

To route to a different runtime or a hosted endpoint, implement the
`InferenceAdapter` trait and insert it into the router at the priority
you want. The adapter declares which `InferenceTask` kinds it supports,
so the router will only dispatch compatible work to it. This is the
extension point — you don't fork the router, you add an adapter.

## Server-side synthesis

For server deployments, synthesis can run through managed endpoints
(and in a TEE via the `nitro-tee` feature on `synthesis_engine`). The
gateway exposes `/synthesis/trigger` and SSE status streaming — see
[api-cookbook.md](api-cookbook.md#trigger-synthesis-and-stream-status-sse).

## Device-tier considerations

On-device inference must fit real memory budgets. Pair model selection
with device-tier gating so a heavy model isn't selected on a device that
can't run it; per-platform tuning is in
[platforms.md](../technical/platforms.md).

## Further reading

- [inference-routing.md](../technical/inference-routing.md) — the router model.
- [platforms.md](../technical/platforms.md) — device tiers.
- [benchmarks.md](../technical/benchmarks.md) — performance.
