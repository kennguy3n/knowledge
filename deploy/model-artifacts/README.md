# Model artifacts

This directory documents the model artifacts the Knowledge stack uses for
on-device synthesis and embedding, where each one lives, and how to fetch
it for local or on-device development.

For the **Docker / Compose / Helm** deployment you do **not** need anything
here: the published `llama-server` image
([`deploy/Dockerfile.llama-server`](../Dockerfile.llama-server)) ships the
Bonsai-1.7B GGUF baked in, so synthesis works out of the box. These
artifacts are only needed for **local builds** (running the substrate or
demo natively) and for **on-device** (iOS/macOS/Android) packaging.

## Artifacts

| Artifact | Quant | Used by | Where it lives |
|----------|-------|---------|----------------|
| `bonsai-1.7b.gguf` | Q1_0 | `llama-server` (server-side synthesis) | Baked into the `llama-server` image; also on Hugging Face / a GitHub Release. |
| `bonsai-1.7b-mlx-2bit.tar.gz` | 2-bit MLX | iOS / macOS on-device synthesis (Apple Silicon) | Hugging Face. |
| `xlm-r-embed-int8.onnx` | INT8 | Embedding model (semantic-vector lane) — higher accuracy | Hugging Face. |
| `xlm-r-embed-int4.onnx` | INT4 | Embedding model (semantic-vector lane) — smaller / faster | Hugging Face. |

### GGUF (server-side, baked into the image)

The `llama-server` runtime image bakes the GGUF in at
`/models/bonsai-1.7b.gguf` (downloaded at build time by the
`model-fetcher` stage; override the source with the `MODEL_URL` build-arg).
Operators can still override the bundled model at runtime by bind-mounting
a different GGUF over that path — see
[`docs/operator/deployment-guide.md`](../../docs/operator/deployment-guide.md).

The same GGUF is published on Hugging Face
(`prism-ml/Bonsai-1.7B-gguf`, file `Bonsai-1.7B.gguf`) and attached to the
matching GitHub Release, so local builds can download it without a Docker
build.

### MLX 2-bit (Apple Silicon on-device)

The 2-bit MLX conversion targets iOS / macOS on-device inference where
the GGUF path is not used. Published on Hugging Face
(`kennguy3n/bonsai-1.7b-mlx`).

### XLM-R ONNX (embedding model)

The multilingual XLM-RoBERTa embedding model powers the semantic-vector
search lane. Two quantizations are published on Hugging Face
(`kennguy3n/xlm-r-embed-onnx`): INT8 (higher accuracy) and INT4 (smaller
footprint, for constrained / mobile builds).

## Downloading

Use [`scripts/download-models.sh`](../../scripts/download-models.sh) to
fetch the artifacts for local / on-device development with SHA-256
verification:

```bash
# Download all artifacts into deploy/models/ (the default).
./scripts/download-models.sh

# Custom destination directory.
./scripts/download-models.sh --dest /path/to/models
```

The script verifies each download against the pinned SHA-256 in
`deploy/model-artifacts/SHA256SUMS`. Until a checksum is pinned for a
given release the script prints the computed hash and continues; pass
`--require-checksums` (or set `REQUIRE_CHECKSUMS=1`) to fail instead, e.g.
in CI.
