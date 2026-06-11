# Model artifacts

This directory documents the model artifacts the Knowledge stack uses for
on-device synthesis and embedding, where each one lives, and how to fetch
it for local or on-device development.

For the **Docker / Compose / Helm** deployment you do **not** need anything
here: the published `llama-server` image
([`deploy/Dockerfile.llama-server`](../Dockerfile.llama-server)) ships the
Bonsai-1.7B Q2_0 GGUF baked in, so synthesis works out of the box. These
artifacts are only needed for **local builds** (running the substrate or
demo natively) and for **on-device** (iOS/macOS/Android) packaging.

## Artifacts

| Artifact | Quant | Used by | Where it lives |
|----------|-------|---------|----------------|
| `bonsai-1.7b.gguf` | Q2_0 (2-bit ternary) | `llama-server` (server-side synthesis) | Baked into the `llama-server` image; also on Hugging Face / a GitHub Release. |
| `bonsai-1.7b-mlx/` (directory) | 2-bit MLX | iOS / macOS on-device synthesis (Apple Silicon) | Hugging Face. |
| `xlm-r-embed-int8.onnx` | INT8 | Embedding model (semantic-vector lane) — higher accuracy | Hugging Face. |
| `xlm-r-embed-int4.onnx` | INT4 | Embedding model (semantic-vector lane) — smaller / faster | Hugging Face. |
| `bonsai-4b.gguf` *(opt-in)* | Q2_0 (2-bit ternary) | **Optional** `llama-server` upgrade for server-side / High-tier synthesis | Hugging Face (prep-only; see below). |
| `bonsai-4b-mlx/` (directory, *opt-in*) | 2-bit MLX | **Optional** 4B on-device synthesis (Apple Silicon) | Hugging Face (prep-only; see below). |

### GGUF (server-side, baked into the image)

The `llama-server` runtime image bakes the GGUF in at
`/models/bonsai-1.7b.gguf` (downloaded at build time by the
`model-fetcher` stage; override the source with the `MODEL_URL` build-arg).
Operators can still override the bundled model at runtime by bind-mounting
a different GGUF over that path — see
[`docs/operator/deployment-guide.md`](../../docs/operator/deployment-guide.md).

The same GGUF is published on Hugging Face
(`prism-ml/Ternary-Bonsai-1.7B-gguf`, file `Ternary-Bonsai-1.7B-Q2_0.gguf`)
and attached to the matching GitHub Release, so local builds can download
it without a Docker build.

### MLX 2-bit (Apple Silicon on-device)

The 2-bit MLX conversion targets iOS / macOS on-device inference where
the GGUF path is not used. Published on Hugging Face
(`prism-ml/Ternary-Bonsai-1.7B-mlx-2bit`) as a directory of loose files
(`config.json`, `model.safetensors`, `model.safetensors.index.json`,
`tokenizer.json`, `tokenizer_config.json`, `chat_template.jinja`) rather
than a single archive, so `download-models.sh` fetches each file into a
`bonsai-1.7b-mlx/` directory.

### Bonsai-4B Q2_0 (optional, opt-in upgrade — NOT the default)

The 4B Q2_0 model is an **optional** synthesis-quality upgrade for
server-side / High-tier deployments that have the RAM/compute headroom.
It is **not** the default anywhere: on-device Low/Medium tiers stay on
Bonsai-1.7B, and the baked `llama-server` image still ships 1.7B. The
adapter contract is unchanged — output shape is GBNF-grammar-guaranteed —
so nothing in the inference router needs to change to serve 4B; it is
purely a heavier weights file selected by configuration.

Fetch it only by opting in:

```bash
# Fetch the default 1.7B set PLUS the optional 4B artifacts.
./scripts/download-models.sh --include-4b      # or: INCLUDE_4B=1 ./scripts/download-models.sh
```

The URLs follow the same `prism-ml/Ternary-Bonsai-*` naming as the 1.7B
repos (`prism-ml/Ternary-Bonsai-4B-gguf` for the GGUF,
`prism-ml/Ternary-Bonsai-4B-mlx-2bit` for the MLX directory). The
checksums in `SHA256SUMS` are left **unpinned** (commented out) because
the 4B artifact may not be published for a release yet — pin the real
digests when the artifact is cut. To select 4B at deploy time, see
[selecting the 4B model](#selecting-the-4b-model-server-side) below.

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

The optional 4B artifacts are excluded from a plain run; add `--include-4b`
(or `INCLUDE_4B=1`) to fetch them too.

## Selecting the 4B model (server-side)

The 4B model is opt-in and only intended for server-side / High-tier
synthesis hosts. There are three ways to point a deployment at it, all
leaving 1.7B as the default for everyone who does not opt in:

1. **Build a 4B image.** Override the `llama-server` image build-args to
   bake the 4B GGUF in instead of 1.7B (1.7B remains the default when the
   args are omitted):

   ```bash
   docker build -f deploy/Dockerfile.llama-server \
     --build-arg MODEL_URL=https://huggingface.co/prism-ml/Ternary-Bonsai-4B-gguf/resolve/main/Ternary-Bonsai-4B-Q2_0.gguf \
     --build-arg MODEL_SHA256=<pin-when-published> \
     -t knowledge/llama-server:4b .
   ```

2. **Bind-mount at runtime.** Mount a `bonsai-4b.gguf` over
   `/models/bonsai-1.7b.gguf` (a runtime bind-mount always wins over the
   baked weights — no rebuild needed).

3. **Native / on-device builds.** Point the inference router at the 4B
   weights with `KNOWLEDGE_SLM_MODEL_PATH=/path/to/bonsai-4b.gguf`
   (defaults to the 1.7B path when unset).

In all cases the inference-router adapter contract is unchanged. See
[`docs/technical/inference-routing.md`](../../docs/technical/inference-routing.md)
for the routing/tier policy.
