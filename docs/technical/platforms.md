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

- **< 20% battery** — heavy synthesis (channel / domain
  windows) is skipped; only sensory observations + lexicon
  importance tagging continue.
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

---

