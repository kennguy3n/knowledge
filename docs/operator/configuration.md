# Tunables Reference

Every configurable threshold in the Knowledge substrate library, its default value, what it controls, and when a consuming product should override it.

> **Audience:** This document targets product engineers ("SMEs") integrating the substrate. Each tunable is safe to expose as an admin setting in a consuming product's configuration surface.

---

## Sync Engine (`crates/sync_engine`)

| Tunable | Constant / Field | Default | What it controls | When to override |
|---------|-----------------|---------|------------------|-----------------|
| Compaction policy | `CompactionPolicy` (enum) | `Adaptive { tombstone_ratio_threshold: 0.3, max_delta_bytes: 4_194_304 }` | Governs when the CRDT op-log is automatically compacted to bound memory and delta-sync payload size. | Switch to `Fixed(n)` for deterministic compaction cadence, or `Disabled` when compaction is managed externally (e.g. by a background job). |
| Tombstone ratio threshold | `DEFAULT_TOMBSTONE_RATIO_THRESHOLD` | `0.3` | In `Adaptive` mode, compaction triggers when the ratio of `Remove`/`Supersede` ops in the growth window exceeds this value. | Lower (e.g. 0.2) for write-heavy workloads with frequent deletes; raise (e.g. 0.5) to reduce compaction frequency on read-heavy deployments. |
| Max delta bytes | `DEFAULT_MAX_DELTA_BYTES` | `4_194_304` (4 MiB) | In `Adaptive` mode, compaction triggers when the estimated serialized size of accumulated ops exceeds this byte count. | Increase on high-bandwidth links (e.g. desktop-to-desktop LAN sync); decrease on constrained mobile connections. |
| Fixed compaction threshold | `DEFAULT_COMPACT_THRESHOLD` | `10_000` ops | Legacy fixed threshold — compact after N ops accumulate since last compaction. Used by `CompactionPolicy::Fixed`. | Lower for memory-constrained devices; raise for servers with ample RAM that benefit from longer op history. |
| Estimated bytes per op | `ESTIMATED_BYTES_PER_OP` (internal) | `256` | Heuristic multiplier for estimating delta payload size without serialization. | Not directly configurable — an implementation detail. |

---

## Synthesis Engine (`crates/synthesis_engine`)

| Tunable | Constant / Field | Default | What it controls | When to override |
|---------|-----------------|---------|------------------|-----------------|
| Max requests per minute | `DEFAULT_MAX_RPM` | `60` | Conservative per-endpoint rate cap applied when `EndpointConfig::max_requests_per_minute` is `None`. Provides cost protection against the upstream inference provider. | Increase for high-throughput enterprise deployments with negotiated rate limits; decrease for shared-tier API keys. |
| Per-request timeout | `DEFAULT_TIMEOUT` | `30s` | Hard timeout for each synthesis HTTP request. | Increase for large-context models or slow endpoints; decrease for latency-sensitive UIs. |
| Max response tokens | `DEFAULT_MAX_TOKENS` | `1024` | Hard cap on the number of tokens the model may generate per request. | Increase for long-form synthesis (summaries, reports); decrease to save cost on short-form tasks (classifications, tags). |
| Rate limiter window | `WINDOW` (in `rate_limiter.rs`) | `60s` | Fixed-window duration for the token-bucket rate limiter. | Not typically overridden — the per-minute cap (`DEFAULT_MAX_RPM`) is the user-facing knob. |

---

## Inference Router (`crates/inference_router`)

| Tunable | Constant / Field | Default | What it controls | When to override |
|---------|-----------------|---------|------------------|-----------------|
| Device tier | `DeviceTier` (enum) | Auto-detected from system RAM | Controls which inference adapters are activated: `Low` = encoder-only, `Medium` = llama.cpp classification, `High` = full SLM synthesis. | Override with `RouterConfig::with_device_tier()` when the auto-detection heuristic doesn't match the deployment (e.g. a high-RAM CI machine that shouldn't run SLM, or a device with fast GPU but low RAM). |
| Low-tier RAM threshold | `LOW_TIER_RAM_THRESHOLD` | `2 GiB` | Devices with less than this RAM are classified as `Low`. | Lower if your SLM model fits in less than 2 GiB; raise if the workload requires more headroom. |
| High-tier RAM threshold | `HIGH_TIER_RAM_THRESHOLD` | `8 GiB` | Devices with at least this RAM are classified as `High`. | Lower if your SLM is small enough to run on 4 GiB devices; raise for very large models. |
| Idle unload timeout | `IDLE_UNLOAD_TIMEOUT_SECS` | `60s` | Time after which an idle adapter is unloaded from memory. | Increase for "always-warm" deployments where cold-start latency matters; decrease on memory-constrained devices. |
| Warm-up prompt | `WARM_UP_PROMPT` | `"knowledge substrate boot probe"` | Prompt sent to newly-loaded adapters to prime the KV cache. | Override with a domain-specific prompt for better first-request latency on production workloads. |

### Managed-Cloud Adapter (`crates/inference_router/src/adapters/managed_cloud.rs`)

The **managed-cloud adapter** lets an operator run synthesis against an external
OpenAI-compatible `/v1/chat/completions` endpoint instead of a self-hosted
`llama-server` sidecar. This is the zero-self-hosting path for SMEs: point it at
OpenAI, Groq, Together, Anthropic (via an OpenAI-compatible proxy), a local
Ollama, etc.

It slots into the router's priority chain between llama.cpp and the fallback:

```text
MLX → llama.cpp → ManagedCloud → Fallback
```

The adapter is **only wired when `KNOWLEDGE_MANAGED_INFERENCE_URL` is set** (and
only on builds that compile the HTTP transport — every server / desktop build;
mobile builds omit it). Because the compute is remote it is **independent of the
device tier** — it serves synthesis even on a `Low`-tier host that could never
run an SLM locally. Classification tasks (`tag_importance`, `extract_entities`,
`promote_observation`) deliberately fall through to the free encoder-only
fallback, so you are never billed per-message for work the local classifier
already handles.

| Setting | Environment variable | Default | What it controls |
|---------|---------------------|---------|------------------|
| Endpoint URL | `KNOWLEDGE_MANAGED_INFERENCE_URL` | _(unset → adapter disabled)_ | OpenAI-compatible base URL, e.g. `https://api.openai.com/v1`, `https://api.groq.com/openai/v1`, or `http://localhost:11434/v1` for Ollama. `/chat/completions` and `/models` are appended automatically. Trailing slashes are tolerated. |
| API key | `KNOWLEDGE_MANAGED_INFERENCE_KEY` | _(empty)_ | Bearer token sent as `Authorization: Bearer …`. Optional — leave empty for keyless local endpoints (e.g. Ollama). |
| Model | `KNOWLEDGE_MANAGED_INFERENCE_MODEL` | `gpt-4o-mini` | Model name passed in the request body. Override per provider, e.g. `llama-3.1-8b-instant` (Groq), `qwen2.5:3b` (Ollama). |
| Completion timeout | `DEFAULT_MANAGED_TIMEOUT_SECS` (const) | `60s` | Hard timeout for each `/chat/completions` request. |
| Probe timeout | `DEFAULT_MANAGED_PROBE_TIMEOUT_SECS` (const) | `5s` | Timeout for the `GET /models` liveness probe run at boot. |
| Max response tokens | `DEFAULT_MANAGED_MAX_TOKENS` (const) | `512` | `max_tokens` cap per request — sized for one grammar-shaped synthesis payload. |
| Sampling temperature | `DEFAULT_MANAGED_TEMPERATURE` (const) | `0.1` | Low temperature keeps synthesis close to extraction rather than creative generation. |

**Structured output.** The same GBNF grammar used for the on-device SLM is
applied to the managed call so the JSON shape is enforced regardless of backend:

- For llama.cpp / Ollama OpenAI-compatible servers the request sets a top-level
  `grammar` field (true GBNF enforcement).
- For the OpenAI family (OpenAI / Groq / Together) the request also sets
  `response_format: {"type": "json_object"}`, forcing a valid JSON object even
  when the `grammar` extension is ignored.

**Rate limits & cost.** The adapter itself does not rate-limit; it relies on the
provider's own limits and on the synthesis engine's per-endpoint cap
(`DEFAULT_MAX_RPM`, see [Synthesis Engine](#synthesis-engine-cratessynthesis_engine)).
Restricting the adapter to synthesis (not classification) bounds spend to the
comparatively rare summary/concept/adjudication calls.

> **Example (Groq):**
> ```bash
> export KNOWLEDGE_MANAGED_INFERENCE_URL="https://api.groq.com/openai/v1"
> export KNOWLEDGE_MANAGED_INFERENCE_KEY="gsk_…"
> export KNOWLEDGE_MANAGED_INFERENCE_MODEL="llama-3.1-8b-instant"
> ```
> With these set, a `Low`/`Medium`-tier deployment with no reachable
> `llama-server` will route `synth_summary` / `synth_concept` /
> `adjudicate_contradiction` to Groq while classification stays local.

---

## Memory Manager (`crates/memory_manager`)

### Decay (`crates/memory_manager/src/decay.rs`)

| Tunable | Constant / Field | Default | What it controls | When to override |
|---------|-----------------|---------|------------------|-----------------|
| Candidate archive threshold | `DEFAULT_CANDIDATE_ARCHIVE_THRESHOLD` | `0.15` | Retention score below which a memory object becomes a candidate for archival during decay sweeps. | Lower to be more aggressive about archiving (frees memory faster); raise to keep more marginal objects live. |
| Superseded TTL | `DEFAULT_SUPERSEDED_TTL_DAYS` | `90` days | How long a superseded (replaced) memory object is retained before being eligible for permanent deletion. | Shorten for privacy-sensitive deployments; lengthen for audit/compliance scenarios that require longer history. |

### Retention Scoring (`crates/memory_manager/src/retention.rs`)

| Weight | Default | What it controls |
|--------|---------|------------------|
| `pinning` | `0.50` | Weight given to explicit user-pinned status. |
| `retrieval_frequency` | `0.15` | Weight given to how often the object is retrieved. |
| `corroboration` | `0.10` | Weight given to corroboration by other memory objects. |
| `contradiction` | `0.05` | Weight given to contradiction signals (negative — contradicted objects score lower). |
| `age` | `0.10` | Weight given to recency (newer = higher). |
| `non_use` | `0.10` | Weight given to non-use decay (less-used = lower). |

> **Note:** Weights must sum to `1.0`. Override via `RetentionWeights` struct when the product's retention policy differs — e.g. a research-focused product may increase `retrieval_frequency` at the expense of `pinning`.

---

## How to override

All tunables are set via Rust structs and builder methods:

```rust
use sync_engine::{SyncEngine, CompactionPolicy};
use synthesis_engine::managed_endpoint::EndpointConfig;
use inference_router::config::{RouterConfig, DeviceTier};

// Sync engine: use adaptive (default) or switch to fixed
let engine = SyncEngine::<String>::new()
    .with_compaction_policy(CompactionPolicy::Fixed(5_000));

// Synthesis engine: explicit RPM override
let cfg = EndpointConfig::new(url, key_ref, model)
    .with_max_requests_per_minute(120);

// Inference router: force a specific tier
let router_cfg = RouterConfig::new(server_url, model_path)
    .with_device_tier(DeviceTier::High);
```

Products exposing these as admin settings should map them 1:1 onto their configuration surface (JSON, TOML, environment variables, etc.) and pass the parsed values into the builder methods shown above.
