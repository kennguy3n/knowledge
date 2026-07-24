# Model artifacts

This directory documents the model artifacts the Knowledge stack uses for
on-device synthesis and embedding, where each one lives, and how to fetch
it for local or on-device development.

For the **Docker / Compose / Helm** deployment you do **not** need anything
here: the published `llama-server` image
([`deploy/Dockerfile.llama-server`](../Dockerfile.llama-server)) ships the
Qwen3.5-0.8B Q4_K_M GGUF baked in, so synthesis works out of the box. These
artifacts are only needed for **local builds** (running the substrate or
demo natively) and for **on-device** (iOS/macOS/Android) packaging.

## Device tier → model mapping

| Tier | RAM | Synthesis | NER | GGUF size | MLX size |
|------|-----|-----------|-----|-----------|----------|
| Low | < 2 GiB | Template-fill (no SLM) | XLM-V INT4 ONNX | — | — |
| Medium | 2–8 GiB | Qwen3.5-0.8B Q4_K_M | — | ~528 MB | ~622 MB |
| High | ≥ 8 GiB | Qwen3.5-0.8B Q4_K_M | — | ~528 MB | ~622 MB |

## Artifacts

| Artifact | Quant | Used by | Where it lives |
|----------|-------|---------|----------------|
| `qwen3.5-0.8b-q4_k_m.gguf` | Q4_K_M (4-bit) | `llama-server` (all SLM tiers — single-stage synthesis) | Baked into the `llama-server` image; also on Hugging Face. |
| `qwen3.5-0.8b-mlx/` (directory) | 4-bit MLX | iOS / macOS on-device synthesis (Apple Silicon, all tiers) | Hugging Face. |
| `xlm-r-embed-int8.onnx` | INT8 | Embedding model (semantic-vector lane) — higher accuracy | Hugging Face. |
| `xlm-r-embed-int4.onnx` | INT4 | Embedding model (semantic-vector lane) — smaller / faster | Hugging Face. |
| `xlm-v-ner-int4.onnx` | INT4 | NER model (low-tier synthesis — multilingual entity extraction) | Hugging Face. |
| `xlm-v-tokenizer.json` | — | XLM-V tokenizer (1M vocab, paired with NER model) | Hugging Face. |

### GGUF (server-side, baked into the image)

The `llama-server` runtime image bakes the GGUF in at
`/models/slm.gguf` (downloaded at build time by the
`model-fetcher` stage; override the source with the `MODEL_URL` build-arg).
Operators can still override the bundled model at runtime by bind-mounting
a different GGUF over that path — see
[`docs/operator/deployment-guide.md`](../../docs/operator/deployment-guide.md).

The default GGUF is published on Hugging Face
(`bartowski/Qwen_Qwen3.5-0.8B-GGUF`, file `Qwen_Qwen3.5-0.8B-Q4_K_M.gguf`),
so local builds can download it without a Docker build.

### MLX 4-bit (Apple Silicon on-device)

The 4-bit MLX conversion targets iOS / macOS on-device inference where
the GGUF path is not used. Published on Hugging Face
(`mlx-community/Qwen3.5-0.8B-4bit`) as a directory of loose files
(`config.json`, `model.safetensors`, `model.safetensors.index.json`,
`tokenizer.json`, `tokenizer_config.json`, `chat_template.jinja`,
`vocab.json`, `preprocessor_config.json`, `processor_config.json`,
`video_preprocessor_config.json`) rather than a single archive, so
`download-models.sh` fetches each file into a `qwen3.5-0.8b-mlx/`
directory.

### XLM-R ONNX (embedding model)

The multilingual XLM-RoBERTa embedding model powers the semantic-vector
search lane. Two quantizations are published on Hugging Face
(`kennguy3n/xlm-r-embed-onnx`): INT8 (higher accuracy) and INT4 (smaller
footprint, for constrained / mobile builds).

### XLM-V NER ONNX (low-tier synthesis)

The XLM-V NER model powers low-tier synthesis: deterministic multilingual
entity extraction followed by template-fill (no SLM text generation).
XLM-V uses a 1M token vocabulary (vs XLM-R's 250k), providing better
coverage for low-resource languages. Published on Hugging Face
(`kennguy3n/xlm-v-ner-onnx`) as an INT4 ONNX file (`xlm-v-ner-int4.onnx`)
paired with a tokenizer (`tokenizer.json`). When the `ner-onnx` feature
is enabled, the `ner_engine` crate loads this model via ONNX Runtime to
extract named entities (persons, organizations, locations, dates, etc.)
from evidence text. The extracted facts are template-filled into a
`SummaryBundle` without an SLM dispatch.

The model path defaults to `/var/lib/knowledge/xlm-v-ner-int4.onnx` and
the tokenizer to `/var/lib/knowledge/xlm-v-tokenizer.json`; override with
the `KNOWLEDGE_NER_MODEL_PATH` and `KNOWLEDGE_NER_TOKENIZER_PATH`
environment variables. When the model file is absent, the low-tier path
falls back to lexicon + regex extraction only.

## Downloading

Use [`scripts/download-models.sh`](../../scripts/download-models.sh) to
fetch the artifacts for local / on-device development with SHA-256
verification:

```bash
# Download all default artifacts (Qwen3.5 + embeddings) into deploy/models/.
./scripts/download-models.sh

# Custom destination directory.
./scripts/download-models.sh --dest /path/to/models

```

The script verifies each download against the pinned SHA-256 in
`deploy/model-artifacts/SHA256SUMS`. Until a checksum is pinned for a
given release the script prints the computed hash and continues; pass
`--require-checksums` (or set `REQUIRE_CHECKSUMS=1`) to fail instead, e.g.
in CI.

## Selecting an alternative model (server-side)

There are two ways to point a deployment at an alternative model:

1. **Build an alternative image.** Override the `llama-server` image
   build-args to bake a different GGUF in instead of Qwen3.5-0.8B:

   ```bash
   docker build -f deploy/Dockerfile.llama-server \
     --build-arg MODEL_URL=https://huggingface.co/<org>/<repo>/resolve/main/<file>.gguf \
     --build-arg MODEL_SHA256=<sha256-of-the-file> \
     -t knowledge/llama-server:custom .
   ```

2. **Bind-mount at runtime.** Mount a different GGUF over
   `/models/slm.gguf` (a runtime bind-mount always wins over the
   baked weights — no rebuild needed).

In all cases the inference-router adapter contract is unchanged. See
[`docs/technical/inference-routing.md`](../../docs/technical/inference-routing.md)
for the routing/tier policy.
