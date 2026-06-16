# Inference Routing on Device

> **TL;DR:** "On-device AI" runs on everything from a 2 GB budget phone
> to a 16 GB laptop with a Neural Engine. You can't ship one model and
> one runtime. The `inference_router` solves this with **device-tier
> gating** (what's even allowed to run here) and a **capability-detected
> accelerator chain** (Core ML/ANE → ONNX Runtime → MLX → llama.cpp →
> managed-cloud → fallback). The business payoff is a single product that
> degrades gracefully instead of five forks.

## What you are building

`inference_router`: the component that, given a task (embed, classify,
synthesize) and the current device, picks a backend and runs it — or
gracefully declines a task the device can't afford. It is the seam that
lets every layer above it stay backend-agnostic.

## Build it: tiers first, then the chain

1. **Detect the device tier.** A `DeviceTier` is auto-detected from
   system RAM (overridable via `KNOWLEDGE_SLM_DEVICE_TIER`) and gates
   which SLM tasks run on-device:
   - **Low** (≈2 GB) — encoder-only (embeddings).
   - **Medium** — + classification.
   - **High** (8 GB+) — + on-device synthesis.

   The same tier also picks the store's `MemoryProfile` (post 2), so one
   detection drives both inference and storage budgets coherently.
2. **Build the accelerator chain with graceful fallback.** The routing
   order is **Core ML/ANE → ONNX Runtime → MLX → llama.cpp →
   managed-cloud → fallback**. Two feature-gated adapters share an
   `AcceleratorAdapter<C>` core with **zero native build dependencies**
   (the runtime is *injected*, so the crate still compiles where the
   accelerator SDK is absent):
   - `CoreMlAdapter` — Apple Neural Engine (`coreml` feature).
   - `OnnxRuntimeAdapter` — ONNX Runtime Mobile + NPU execution provider
     (NNAPI / QNN-Hexagon on Android, Core ML EP on iOS; `onnx-runtime`
     feature).

   Capability detection probes what's actually present and falls through
   the chain; a device with no accelerator and no model still gets the
   deterministic fallback so callers never hard-fail. See
   [`inference-routing.md`](../../docs/technical/inference-routing.md).

The principle worth copying: **the router decides *placement*, the
layers above decide *intent*.** `synthesis_pipeline` asks for a recap; it
doesn't know or care whether the ANE, ONNX, or a cloud endpoint produced
it.

## Build it: the model tiers

For generation the default on-device model is **Bonsai-1.7B (Q2_0)** —
small enough for the High tier's budget. An opt-in **4B** model exists
for quality-sensitive deployments. The decision of *which* to default to
is not a vibe; it's measured (next post and below).

## The business decision: 1.7B default vs. 4B for non-Latin

**Scenario.** You're deploying to APAC and the GCC. The default 1.7B
model is cheap and fits the device budget — but does it actually write a
usable Japanese or Arabic briefing?

The honest, measured answer from the
[multilingual leaderboard](../../docs/technical/multilingual-leaderboard.md)
(`--compare-4b` probe, recorded model output):

| Language | 1.7B in-language | 4B in-language |
|---|---|---|
| French / German / Spanish | yes | yes |
| Vietnamese / Thai / Indonesian | yes | yes |
| **Japanese** | **no** | yes |
| **Chinese** | **no** | yes |
| **Arabic** | **no** | yes |

So the product decision writes itself: **default to 1.7B for
Latin-script deployments, default the 4B for non-Latin (CJK/Arabic)
deployments**, and let the router/device tier gate which is even
feasible. That's a business rule grounded in a reproducible eval, not an
assumption — and it's published so a customer can verify it.

## How a competitor would build this

Cloud-AI products (Copilot, Glean, the cloud memory layers) call one
hosted frontier model for everything. No device tiers, no accelerator
chain, no fallback to write — and uniformly high quality. The cost is
that *every* inference leaves the device and bills per token, and there
is no offline mode at all. Apple Intelligence runs on-device but is
single-vendor, single-platform, and closed. The router is the choice you
make when you need cross-platform on-device inference that you can
measure and that still works on a plane — accepting that you, not a
vendor, own the placement logic.

## What's next

The router can run a model; `synthesis_pipeline` decides *what to ask it*
and — critically — how to know whether the answer was any good. Next: the
synthesis pipeline and the deterministic eval harness that keeps it
honest.

---
*Part 5 of "How to Build Knowledge." [Previous: Retrieval & the Memory Graph](04-retrieval-and-memory.md) | [Next: Synthesis & Honest Eval](06-synthesis-and-eval.md) | [Series index](README.md)*
