# Zero to Running in One Command

> **TL;DR:** A single installer (`scripts/install.sh` /
> `scripts/install.ps1`) takes a fresh host to a running Knowledge stack:
> it checks Docker, generates strong secrets, pulls the published images,
> and waits for health. The `llama-server` image now ships the
> **Bonsai-1.7B** model baked in, so on-device synthesis works with **no
> model download**. A first-run wizard in the admin dashboard walks you
> through your first connector, and an end-user **reference UI** on
> `:3002` lets people start chatting immediately.

## The Business Problem

A 30-person company without a platform team should not have to read a
deployment guide to try a knowledge substrate. The old path — clone the
repo, copy `.env.example`, hand-generate a master key, figure out which
GGUF to download and where to put it, run `docker compose up --build`,
wait twenty minutes for a from-source build, then guess which URL to
open — is a wall that stops an evaluation before it starts.

The fix is to collapse all of that into one command, and to ship enough
batteries that the defaults just work.

## One command

The installer comes in two flavors — `scripts/install.sh` for
bash/macOS/Linux and `scripts/install.ps1` for Windows PowerShell — and
both do the same six things:

1. Check Docker and the Compose v2 plugin are present.
2. Generate per-deployment secrets (master key, Postgres/MinIO/Grafana
   passwords) into `.env` at mode `600`, **never** overwriting an
   existing file — rotating the master key would orphan the encrypted
   store.
3. Ask whether to enable on-device synthesis (it needs ~4 GB RAM).
4. Pull the published images and start the stack.
5. Wait for the gateway to report healthy.
6. Print the URLs to open.

From a clone:

```bash
./scripts/install.sh
```

Or straight from the web (it downloads the compose files into
`./knowledge` first):

```bash
curl -fsSL https://raw.githubusercontent.com/kennguy3n/knowledge/main/scripts/install.sh | bash
```

On Windows, `./scripts/install.ps1` or
`irm https://raw.githubusercontent.com/kennguy3n/knowledge/main/scripts/install.ps1 | iex`.

It is scriptable, too: set `KNOWLEDGE_ASSUME_YES=1` for a non-interactive
run, `KNOWLEDGE_SLM_DEVICE_TIER=high|medium|low` to skip the synthesis
prompt, `KNOWLEDGE_IMAGE_TAG` to pin a release, or
`KNOWLEDGE_INSTALL_DRY_RUN=1` to do everything except the
`docker compose up` and the health wait.

## The model is already in the box

The biggest papercut in the old flow was the model. Server-side
synthesis runs on a `llama-server` sidecar, which needs a GGUF — and
"go download a 1 GB file and mount it at the right path" is exactly the
kind of step that derails a first run.

So the published `llama-server` image now **bakes the Bonsai-1.7B GGUF
in** at `/models/bonsai-1.7b.gguf` (see `deploy/Dockerfile.llama-server`).
`docker compose up` brings up synthesis with nothing to download and
nothing to mount. The image is large — it compiles llama.cpp from source
and embeds the weights — but it is published like the gateway and
substrate images, so operators pull it rather than build it.

You can still override the weights when you want a different model:
bind-mount your own GGUF over the baked-in path by uncommenting the
`volumes:` override on the `llama-server` service. And for native local
or on-device development (outside Docker), `scripts/download-models.sh`
fetches the GGUF, MLX, and ONNX artifacts with SHA-256 verification.

### Or skip the local model entirely

If you would rather not run a local model at all, point synthesis at any
OpenAI-compatible endpoint with `KNOWLEDGE_MANAGED_INFERENCE_URL` /
`_KEY` / `_MODEL` (OpenAI, Groq, Together, a local Ollama, …). The
managed-cloud adapter sits in the inference priority chain
(`MLX → llama.cpp → ManagedCloud → Fallback`) and serves synthesis on
any device tier, since the compute is remote.

## A guided first run

Once the stack is up, the admin dashboard (`:3001`) does not drop you
onto an empty screen. On a fresh deployment with no connectors it shows
a **first-run wizard** — welcome → pick a source → OAuth → first sync —
and keeps a Getting Started card on the Dashboard until you have wired up
a few connectors. The goal is to get from "it's running" to "it's
answering questions about my data" without leaving the browser.

## Somewhere for end users to land

The admin dashboard is for operators. For everyone else, the new
end-user **reference UI** (`apps/knowledge-ui/`, a Next.js 14 app on
`:3002`) is wired into the compose stack out of the box. It is a thin,
fully client-side client over the gateway's REST surface where users can
chat with a scope, run hybrid search, browse synthesized memory and its
decay state, watch synthesis stream in over SSE, and cryptographically
forget a conversation. It is a starting point you can ship as-is or fork
into your own product.

## What it means for you

The distance from "I heard about Knowledge" to "my team is asking
questions across our tools" is now one command plus a guided wizard.
The defaults — baked-in model, generated secrets, a running reference UI
— are chosen so the happy path needs no decisions, while every one of
them (model, inference backend, ports, image tags) stays overridable
when you outgrow the defaults.

## Further reading

- [deployment-guide.md](../docs/operator/deployment-guide.md#one-command-installer)
  — the installer, HA, and the full service topology.
- [Managing Knowledge Without a DevOps Team](20-admin-without-ops.md) —
  the admin dashboard the wizard lives in.
- [Zero to Production Deployment](07-zero-to-production-deployment.md) —
  the three deployment modes in depth.
- [QUICKSTART.md](../docs/QUICKSTART.md) — the manual walkthrough across
  all three modes.
