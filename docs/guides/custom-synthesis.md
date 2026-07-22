# Custom Synthesis (bring your own model)

How to control which model performs synthesis and how inference is
routed. Read [inference-routing.md](../technical/inference-routing.md)
first for the model; this guide is the how-to.

## How routing works

All SLM work goes through the `InferenceRouter`, which holds an ordered
list of adapters and dispatches each task to the highest-priority
adapter that is available and supports it:

```
Core ML / ANE  →  ONNX Runtime (NPU)  →  MLX  →  llama.cpp  →  managed cloud  →  Fallback
```

Each adapter is probed at boot via capability detection; the router
dispatches to the highest-priority one that is available and supports the
task, falling through gracefully when an accelerator is absent. The
**Fallback** is a deterministic no-op synthesizer that is always
available — it's what runs in the quickstart demo and CI, so nothing
requires a model to be present.

## Option 1: wire llama.cpp (a GGUF model)

The simplest real backend. Stand up `llama-server` with a GGUF model
(e.g. Qwen3.5-2B) and build the substrate with the `http-client`
feature so the llama.cpp adapter becomes available and reachable over
loopback. See the
[quickstart "Wiring a real SLM" section](../QUICKSTART.md#wiring-a-real-slm-optional).

## Option 2: use MLX on Apple silicon

On Apple silicon with the MLX runtime present, the MLX adapter is probed
at boot and selected ahead of llama.cpp. No code changes — it's a
priority-ordered fallthrough.

## Option 3: on-device NPU / ANE accelerators

Two feature-gated accelerator adapters sit at the top of the ladder and
are selected ahead of MLX/llama.cpp when their runtime is present:

- **`CoreMlAdapter`** (`coreml` feature) — runs the model on the Apple
  Neural Engine via Core ML when the graph is ANE-resident.
- **`OnnxRuntimeAdapter`** (`onnx-runtime` feature) — ONNX Runtime Mobile
  with an NPU execution provider: NNAPI / QNN-Hexagon on Android, the
  Core ML EP on iOS.

Both build on a shared `AcceleratorAdapter<C>` core, carry **zero native
build dependencies** (the accelerator runtime is injected at load time),
and do **capability detection with graceful fallback** to MLX, llama.cpp,
or CPU when the accelerator or its model artifact is unavailable. See
[inference-routing.md](../technical/inference-routing.md).

## Option 4: bring your own adapter

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

## Synthesis quality: validator, retry, and adaptive budget

The `LlamaCppSynthesizer` does more than a single SLM call. Because a
small on-device model occasionally prefaces the bundle with
meta-commentary (`"The session highlights…"`) instead of emitting
facts, the synthesizer runs a **deterministic** quality check on the
parsed `SummaryBundle` and retries once when it is poor:

1. **Adaptive budget.** The first attempt's `n_predict` scales with the
   number of observation rows (`quality::adaptive_budget`), floored at
   512 and clamped well under the synthesis deadline so a large window
   cannot run long enough to trip the gateway timeout.
2. **Score + flag.** `quality::score_bundle` returns a signed score
   (higher is better) and flags the bundle low-quality when the recap
   opens with a known meta-commentary phrase, is shorter than
   `MIN_RECAP_CHARS`, or — when there are enough salient evidence terms
   — covers fewer than `MIN_TERM_COVERAGE` of them. The check is pure
   (no clock/RNG), so the retry decision is as reproducible as the
   sampling preset.
3. **Ground the structured lists.** The grammar can't stop a small
   model from copying the prompt's one-shot exemplar into the
   `decisions` / `open_questions` / `active_tasks` lists, so before
   scoring, `quality::strip_exemplar_leak` deterministically drops any
   list entry that contains a `inference_router::SYNTH_EXEMPLAR_TOKENS`
   placeholder (`EXAMPLE_DECISION` / `EXAMPLE_TASK`) — guaranteeing a
   leaked example can never reach persistence even if both attempts
   leak. `score_bundle` then adds two evidence-grounding signals on top
   of the recap checks: an **`exemplar_leak`** hard fail (a leak left in
   the recap free text, which can't be surgically stripped, forces a
   retry) and **`ungrounded_entries`**, a deliberately weak per-entry
   penalty for list items that share no salient term with the evidence
   (it only nudges the score so a better-grounded retry wins a tie —
   it never deletes an entry or triggers a retry on its own).
4. **Verify-and-retry.** On a flagged bundle the synthesizer retries
   **once** with a larger budget (`quality::retry_budget`) and a
   fact-only suffix, then keeps whichever attempt scores better. The
   retry is capped at one to protect latency; a failed retry keeps the
   first (usable-but-mediocre) bundle rather than failing the synthesis.
   `retry_budget` adds a fixed bonus to the first attempt's budget and
   then **caps** at `RETRY_N_PREDICT` (a hard ceiling), so a retry can
   never run the generation past the deadline-safe window for *any*
   input.

None of this changes the GBNF shape contract or the
`SummaryBundle::from_slm_str` salvage parser — a token-truncated but
otherwise-good recap is still recovered (and counted).

The decision logic above lives in `synthesis_pipeline::quality` as a
pure, evidence-agnostic orchestration — `salient_terms_from_texts`,
`score_bundle_with_terms`, and `verify_and_retry` — so the **same**
scoring and retry contract is shared by both synthesis paths and they
can never drift apart:

* the **server-tier** `LlamaCppSynthesizer` (this crate), and
* the **on-device** path (`ffi::trigger_synthesis` →
  `synthesize_scope`), which dispatches via
  `InferenceRouter::dispatch_with_sampling` (carrying the deterministic
  seed + sampling knobs onto the wire) and runs the identical
  `verify_and_retry` policy before persisting the recap.

### Metrics

`SynthesisMetrics` accumulates process-global counters exposed via
`LlamaCppSynthesizer::metrics_snapshot()` for the host's metrics
surface, alongside the router's `knowledge_slm_dispatch_duration_seconds`
histogram:

| Counter | Meaning |
| --- | --- |
| `synthesis_retry_total` | verify-and-retry second attempts made |
| `synthesis_retry_failed_total` | retries that errored (first bundle kept) |
| `synthesis_lowquality_total` | first attempts flagged low-quality |
| `synthesis_truncated_total` | outputs recovered by the salvage parser |
| `synthesis_exemplar_leaks_stripped_total` | structured-list entries scrubbed because they copied a prompt exemplar placeholder |
| recap length (sum + count) | mean recap length signal |

`synthesis_retry_failed_total` makes the graceful-degradation path
observable: a retry that *errors* keeps the first (mediocre) bundle
rather than failing the synthesis, so without this counter a flaky
retry-only adapter would leave no trace. The on-device path additionally
emits a `tracing::warn!` on the same event.

`synthesis_exemplar_leaks_stripped_total` counts list entries that
`quality::strip_exemplar_leak` removed before persistence (entries, not
runs). The on-device FFI path emits a `tracing::warn!` with the same
`stripped` count; this counter is the logging-free server path's
equivalent, so a rising value flags that the prompt's one-shot exemplar
is leaking into real bundles and warrants a prompt review.

Share one `SynthesisMetrics` across several synthesizers with
`LlamaCppSynthesizer::with_metrics(Arc::clone(&metrics))` so their
counters fold into the same totals.

The **on-device** path surfaces the equivalent signals on the FFI
`MetricsSnapshot` (`synthesis_lowquality_total`, `synthesis_retry_total`,
`synthesis_retry_failed_total`, `synthesis_truncated_total`,
`synthesis_exemplar_leaks_stripped_total`, and
`synthesis_recap_chars_total` / `synthesis_recap_samples_total` for the
mean recap length). All are `#[serde(default)]` additive fields, so an
older host reading a newer snapshot — or vice versa — never breaks on
the wire. `substrate_server::metrics::render` walks the snapshot JSON and
emits each `_total` leaf as a `counter`, so
`knowledge_synthesis_exemplar_leaks_stripped_total` reaches the
`/internal/metrics` Prometheus surface automatically. This is the
on-device path's scrapeable equivalent of its `tracing::warn!` — both
fire on the same scrubbed-leak event, so a leaking prompt is observable
across the tenant fleet without log aggregation.

## Device-tier considerations

On-device inference must fit real memory budgets. Pair model selection
with device-tier gating so a heavy model isn't selected on a device that
can't run it; per-platform tuning is in
[platforms.md](../technical/platforms.md).

## Further reading

- [inference-routing.md](../technical/inference-routing.md) — the router model.
- [platforms.md](../technical/platforms.md) — device tiers.
- [benchmarks.md](../technical/benchmarks.md) — performance.
