# Device & Platform Notes

On-device tuning (storage, memory, battery, network) and per-platform
integration notes (iOS, Android, macOS, Windows) for the Rust shared
core. Split out of [`ARCHITECTURE.md`](../ARCHITECTURE.md) §9 and §10.

## 9. Device optimization

The substrate's behaviour adapts to three signals: storage,
memory, and battery.

### 9.1 Storage

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

### 9.2 Memory

- **mmap** for all weight files so the OS can evict cleanly
  under pressure.
- **60 s idle-unload** of the SLM after a quiet period; the
  next synthesis triggers a re-warm.
- **Hard caps** — at most one heavy model resident on mobile
  at a time; on desktop, the SLM and the embedding model can
  coexist.

### 9.3 Battery

- **< 20% battery** — heavy synthesis (channel / domain
  windows) is skipped; only sensory observations + lexicon
  importance tagging continue.
- **Defer non-critical observations** — low-importance
  candidates are queued until AC / Wi-Fi.
- **Batch sync** — sync uplink waits for AC + Wi-Fi by default;
  override per-tenant policy is allowed.

### 9.4 Network

- **Delta sync only** — full re-sync is reserved for first run
  and explicit recovery.
- **Compressed encrypted payloads** — `zstd` over the
  encrypted body before transmission.
- **Bloom prefilters** — for cross-device retrieval, a small
  per-scope bloom filter is consulted before the full delta
  pull, in line with the chat-storage-search "Bloom shard"
  pattern.

---

## 10. Platform-specific notes

### 10.1 iOS

- **UI**: Swift native (SwiftUI + UIKit).
- **Rust core** via **UniFFI** (`.xcframework`).
- **Embeddings**: Core ML (XLM-R converted with `coremltools`).
- **SLM**: MLX runtime — `MLXAdapter` is the preferred path on
  Apple Silicon; Bonsai-1.7B 2-bit MLX (~248 MB).
- **Background work** — synthesis windows scheduled via BGTask
  scheduler; respects Low Power Mode.

### 10.2 Android

- **UI**: Kotlin native (Jetpack Compose).
- **Rust core** via **JNI**.
- **Embeddings**: ONNX Runtime with the **NNAPI EP** (DSP / NPU
  fallback to CPU).
- **SLM**: `llama.cpp` via the NDK + the PrismML fork's NDK
  build artifacts.
- **Background work** — WorkManager constraints (charging,
  unmetered, idle); synthesis windows are deferrable.

### 10.3 macOS

- **UI**: Electron 31 + React renderer.
- **Native bridge**: Swift N-API addon for Rust core +
  Swift-side MLX glue.
- **SLM**: MLX preferred (`MLXAdapter`); `LlamaCppAdapter`
  fallback.
- **Embeddings**: Core ML for XLM-R via Swift bridge; ONNX
  Runtime fallback.

### 10.4 Windows

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

---

