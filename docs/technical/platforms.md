# Device & Platform Notes

This document collects the on-device tuning behaviour of the
Knowledge substrate (storage, memory, battery, network) and the
per-platform integration notes for iOS, Android, macOS, and
Windows. For the system-level architecture see
[`architecture.md`](architecture.md).

## Device Optimization

The substrate's behaviour adapts to four signals: storage,
memory, battery, and network.

### Storage

- **Tiered storage** — hot SQLCipher database for recent /
  pinned objects; cold encrypted segments for the long tail.
- **Content-aware storage routing** — inline storage for small
  bodies (≤ 512 B, no dedup index lookup); separate body table
  with BLAKE3 content-hash dedup for large bodies (> 512 B);
  ring buffer for noise-class messages (FIFO overwrite, no
  persistence beyond synthesis window).
- **Semantic near-dedup** — XLM-R detects semantically equivalent
  observations at the observation plane, deduplicating meaning
  rather than bytes for text content.
- **Hard caps** — configurable per device, with sane defaults
  (250 MB substrate footprint on mobile without SLM resident,
  1 GB+ on desktop with SLM resident).

### Memory

- **mmap** for all weight files so the OS can evict cleanly
  under pressure.
- **60 s idle-unload** of the SLM after a quiet period; the
  next synthesis triggers a re-warm.
- **Hard caps** — at most one heavy model resident on mobile
  at a time; on desktop, the SLM and the embedding model can
  coexist.

### Battery

The substrate gates background work at two battery thresholds. The
floors are layered: the higher (50%) sheds the medium-importance tail
while the device keeps working; the lower (20%) drops it out of
heavy synthesis entirely.

- **< 50% battery** — medium-importance observations and
  non-foreground channel synthesis are deferred to AC / Wi-Fi.
  High-importance observations and lexicon importance tagging
  continue. An elected synthesizer in this band stays elected (it is
  still ≥ the 20% eligibility floor) but advertises
  `defers_medium_importance` so peers route the medium tail
  elsewhere — see `DEFAULT_BATTERY_DEFER_MEDIUM_FLOOR` and
  `ElectionCandidate::defers_medium_importance` in
  `crates/synthesis_pipeline/src/election.rs`. Connector sync
  intervals are also doubled in this band (see
  [`cost-model.md`](../operator/cost-model.md) "Mobile sync
  defaults").
- **< 20% battery** — heavy synthesis (channel / domain
  windows) is skipped; only sensory observations + lexicon
  importance tagging continue, and the device drops out of the
  elected-synthesizer pool entirely (`DEFAULT_BATTERY_FLOOR`).
- **Defer non-critical observations** — low-importance
  candidates are queued until AC / Wi-Fi.
- **Batch sync** — sync uplink waits for AC + Wi-Fi by default;
  override per-tenant policy is allowed.

### Network

- **Delta sync only** — full re-sync is reserved for first run
  and explicit recovery.
- **Compressed encrypted payloads** — `zstd` over the
  encrypted body before transmission.
- **Bloom prefilters** — for cross-device retrieval, a small
  per-scope bloom filter is consulted before the full delta
  pull, in line with the chat-storage-search "Bloom shard"
  pattern.

---

## Platform Notes

### iOS

- **UI**: Swift native (SwiftUI + UIKit).
- **Rust core** via **UniFFI** (`.xcframework`).
- **Embeddings**: Core ML (XLM-R converted with `coremltools`).
- **SLM**: MLX runtime — `MLXAdapter` is the preferred path on
  Apple Silicon; Bonsai-1.7B 2-bit MLX (~248 MB).
- **Background work** — synthesis windows scheduled via BGTask
  scheduler; respects Low Power Mode.

### Android

- **UI**: Kotlin native (Jetpack Compose).
- **Rust core** via **JNI**.
- **Embeddings**: ONNX Runtime with the **NNAPI EP** (DSP / NPU
  fallback to CPU).
- **SLM**: `llama.cpp` via the NDK + the PrismML fork's NDK
  build artifacts.
- **Background work** — WorkManager constraints (charging,
  unmetered, idle); synthesis windows are deferrable.

### macOS

- **UI**: Electron 31 + React renderer.
- **Native bridge**: Swift N-API addon for Rust core +
  Swift-side MLX glue.
- **SLM**: MLX preferred (`MLXAdapter`); `LlamaCppAdapter`
  fallback.
- **Embeddings**: Core ML for XLM-R via Swift bridge; ONNX
  Runtime fallback.

### Windows

- **UI**: Electron 31 + React renderer.
- **Native bridge**: C++ N-API addon for Rust core.
- **SLM**: `LlamaCppAdapter` against `llama-server` from the
  PrismML fork. **CPU-only** profile uses AVX2 minimum, AVX-VNNI
  / AVX-512 VNNI when available; **CPU+GPU** profile adds the
  Vulkan or CUDA backend.
- **Embeddings**: ONNX Runtime with **DirectML EP** for GPU
  acceleration; CPU EP fallback.
- **AVX2 minimum** — devices below AVX2 are tier-locked to
  Low and never enter the SLM path.

### Electron optimization (macOS + Windows)

Both desktop hosts run Electron 31 + a React renderer. Rather than
migrating to Tauri, the install size and idle-memory footprint are
tightened **in-place** by the host app. The host MUST apply the
checklist in
[`../guides/electron-optimization.md`](../guides/electron-optimization.md);
the high-impact items are:

- **Ship size** — strip unused Electron locales (~7 MB), `asar`-pack
  everything except the `.node` addon (which stays unpacked for
  `require()`), tree-shake the renderer in `production` mode, and drop
  the spellcheck dictionary when unused (~40 MB on some platforms).
- **Runtime memory** — cap the renderer V8 old-space with
  `--js-flags=--max-old-space-size=256` (vs the ~1.5 GB default), keep
  `backgroundThrottling` at its default `true` so backgrounded windows
  throttle timers / rAF, and reuse a **single** `BrowserWindow`
  (settings / preferences via in-app routing) since each window is a
  full renderer process.
- **Cold start** — V8-snapshot the renderer's bootstrap heap
  (`--snapshot-blob` / `electron-link`).

None of these may relax a control in
[`../security/electron-hardening.md`](../security/electron-hardening.md)
(§8 mirrors this list from the security reviewer's angle).

### SLM weights are lazy-downloaded, not bundled

On **every** platform the SLM weights (~248 MB MLX on iOS / macOS,
~237 MB GGUF on Android / Windows) are **not** shipped inside the app
installer. They are fetched on demand the first time synthesis is
triggered, verified by SHA-256, and cached on disk. The host gets a
byte-level progress callback so it can render a one-time download UX
instead of a generic "Unavailable":

- First synthesis on a fresh install reports
  `FfiError::ModelDownloading { progress_pct }` while the weights
  stream in; the host shows a progress bar and retries when it reaches
  100%.
- Subsequent launches find the cached weights and start synthesis
  immediately (subject to the device-tier and battery gates above).
- The download URL is configured per platform via
  `RouterConfig::model_download_url` (with the pinned
  `RouterConfig::model_sha256`); see
  [`../../crates/inference_router/src/config.rs`](../../crates/inference_router/src/config.rs).

This keeps the installer small (and App Store / Play limits
comfortable) and means devices that never reach the synthesis tier
(Low tier) never pay the download at all.

#### How it works (implementation)

The download flow is owned by
[`inference_router::model_download`](../../crates/inference_router/src/model_download.rs)
and driven from `InferenceRouter::bootstrap()` — the first
`trigger_synthesis` kicks the bootstrap probe off on a background
thread, and bootstrap calls `ensure_model_present()` **before** probing
adapters (an MLX / llama.cpp probe loads or pings the model, so probing
before the weights exist would spuriously mark the backend
unavailable).

- **Verification & atomicity.** Bytes stream into a `*.partial` sidecar,
  are hashed with SHA-256 as they arrive, and are only `rename`d into
  place once the hash matches the pinned `model_sha256`. A mismatch
  deletes the partial file and fails the download — a verified-wrong
  artifact is never left where a later run could consume it. For a
  5000-tenant fleet this is the line between lazy-loading a model and
  executing attacker-substituted weights, so a pinned hash is mandatory
  for any public-CDN URL.
- **Progress is observable two ways.** A push callback
  (`(bytes_downloaded, total_bytes)`) drives host UX, and a pull
  accessor — `model_download_status(handle)` (N-API
  `modelDownloadStatus`) — returns the internally-tagged state
  (`{"state":"idle"}`, `{"state":"in_progress","pct":42}`,
  `{"state":"complete"}`, `{"state":"failed","message":…}`) so a host
  can paint a progress bar by polling once per frame instead of
  absorbing a hot per-chunk callback across the language boundary or
  re-calling `trigger_synthesis` just to read the percentage. While a
  download is in flight, `trigger_synthesis` short-circuits to
  `FfiError::ModelDownloading { progress_pct }` instead of running
  dispatch.
- **Transport is platform-gated.** The byte transport sits behind the
  `ModelFetcher` trait. Desktop / server builds enable the
  `http-client` feature and wire the reqwest-backed `ReqwestFetcher`
  automatically. **Mobile builds deliberately omit the reqwest + TLS
  stack** (it would bloat the very artifact this optimisation shrinks),
  so on iOS / Android the in-process fetch reports `Unsupported` and the
  host is expected to provision the weights out-of-band (host-managed
  download / on-demand resources) and drop them at
  `RouterConfig::model_path`. In that configuration the status stays
  `idle` and synthesis simply waits for the file to appear — see the
  embed guides for the host-side contract.

---

