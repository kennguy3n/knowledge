# On-Device Inference Under Constraints

> **TL;DR:** Running language models on real phones means living within
> 2–8 GB of RAM and wildly different runtimes. Knowledge routes
> inference across an ordered adapter ladder — on-device NPU backends
> (CoreML/ANE, ONNX-Runtime) ahead of the MLX and llama.cpp SLM
> adapters, with a managed-cloud endpoint and a deterministic fallback
> closing the list — gated by device tier, so the same app degrades
> gracefully from a high-end laptop to a budget Android handset.

## The Business Problem

KChat wants on-device synthesis — summarizing a conversation,
generating memory, answering from local context — without sending data
to a server. The catch: its users in emerging markets are on budget
Android phones with 2–4 GB of RAM, while its power users are on
M-series MacBooks. The same feature has to work on both.

This is the constraint that kills most "AI on the edge" plans. A model
that runs comfortably on a laptop with a GPU is a non-starter on a
handset. Teams either give up and go server-side (losing the privacy
and cost benefits from [post 1](01-why-on-device-memory.md)) or ship a
feature that only works on flagship hardware. Neither is acceptable for
a product whose whole pitch is privacy for *everyone*, including the
next billion users on inexpensive devices.

## The Technical Approach

The [`inference_router` crate](../crates/inference_router/) treats
on-device inference as a routing problem, not a single-model problem.
It maintains an **ordered list of inference adapters** and dispatches a
task to the highest-priority one that is available on the current device
and supports the task. The
[inference-routing spec](../docs/technical/inference-routing.md) is the
full reference; with the on-device accelerator features enabled the
canonical priority is:

```text
CoreML/ANE  →  ONNX-Runtime  →  MLX  →  llama.cpp  →  managed-cloud  →  Fallback
```

- **CoreML / ANE** — Apple's Neural Engine via Core ML, used on Apple
  hardware where the `coreml` feature is built and the backend probes
  ready.
- **ONNX-Runtime** — ONNX Runtime Mobile with an NPU execution
  provider, behind the `onnx-runtime` feature, for accelerators on
  other platforms.
- **MLX** — Apple's MLX runtime, used on Apple silicon where present.
- **llama.cpp** — a `llama-server` reached over loopback HTTP, usable
  on any platform with a GGUF model and the server reachable (requires
  the `http-client` feature).
- **managed-cloud** — an OpenAI-compatible endpoint, used by exception
  when no local model can run (e.g. a constrained device); the compute
  is off-device so it is tier-independent.
- **Fallback** — a deterministic, dependency-free adapter that is
  always available, so the substrate never hard-fails just because no
  real model is wired in.

The NPU adapters are feature-gated and only enter the ladder when the
platform shell reports the backend present; on hardware without one —
or a build without the features — the list degrades cleanly to the
classic `MLX → llama.cpp → managed-cloud → Fallback` order. Ranking
the accelerators ahead of the SLM adapters (the default,
`prefer_accelerator`) minimises on-device latency and battery cost.

**Device-tier gating.** Adapters alone are not enough — a device with
2 GB of RAM should not attempt the same workload as one with 8 GB. The
router pairs adapter selection with a device-tier classification. The
chosen execution is a function of `(available adapters, task kind,
device tier)`: a `Low`-tier device runs no local model at all — only
the managed-cloud path (if configured) and the encoder-only fallback
appear — while `Medium` and `High` tiers admit the accelerator and SLM
adapters. This is the mechanism that lets one binary span the
budget-phone-to-laptop range.

**Bring your own adapter.** The adapter list is an extension point.
Implementing the `InferenceAdapter` trait and inserting it at a chosen
priority lets a team wire a custom runtime — a vendor SDK, a
specialized quantized model — without touching the routing logic. The
[custom-synthesis guide](../docs/guides/custom-synthesis.md) walks
through wiring `llama.cpp` with a GGUF model (e.g. a small
~2B-parameter model quantized to fit a handset) and adding your own
adapter.

## Implementation Walk-through

From a host's perspective, synthesis is requested without naming a
runtime — the router decides:

```text
// configure once: ordered adapters + device tier + accelerator availability
// then request synthesis; the router picks the adapter
synthesize(scope_id, task) -> result   // ANE? ONNX? MLX? llama.cpp? cloud? fallback?
```

To run real inference rather than the deterministic fallback, you stand
up `llama-server` with a GGUF model and build with the `http-client`
feature so the llama.cpp adapter becomes available. On Apple silicon,
MLX is preferred automatically when present. The
[platforms doc](../docs/technical/platforms.md) covers per-platform
tuning, and the [custom-synthesis guide](../docs/guides/custom-synthesis.md)
covers server-side synthesis in a TEE for cases where a device tier
genuinely cannot run a model locally.

The design lets the product team make a single promise — "synthesis
works on-device" — and have it hold across the whole device matrix,
with the router quietly choosing the best available path per device.

## Performance & Cost Implications

The routing and gating layers are cheap; the cost is dominated by the
chosen model, which is exactly why device-tier gating matters. By
matching the task profile to the device tier, the substrate avoids the
two failure modes that wreck on-device inference: attempting too much
on a small device (jank, OOM, battery drain) or needlessly downgrading
a capable one.

The cost implication mirrors the rest of the substrate: inference runs
on the device the user already owns, so there is no per-token API bill
and no inference server to scale. For a 10-million-user app, moving
synthesis on-device turns a usage-metered cloud cost into a fixed
client-side one. When a device truly cannot run a model, server-side
synthesis in a TEE is the escape hatch — used by exception, not by
default.

## What's Next

So far the substrate remembers and reasons over what the app feeds it
directly. But most organizational knowledge already lives in Notion,
Slack, Drive, and a dozen other SaaS tools. The next post is about
connectors: pulling that knowledge in without standing up a data
pipeline team.

---
*This is part 5 of the "Building Knowledge" series. [Previous: Post-Quantum Crypto for Mortals](04-post-quantum-crypto-for-mortals.md) | [Next: Connector Architecture](06-connector-architecture.md)*
