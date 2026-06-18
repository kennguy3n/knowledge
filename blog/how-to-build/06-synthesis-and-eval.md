# Synthesis & Honest Eval

> **TL;DR:** A small model on a phone can write a great briefing on one
> scope and ramble for 512 tokens on the next — and, if you're not
> careful, a *different* answer every run. This post builds
> `synthesis_pipeline` to be **deterministic** (fixed seed + greedy
> decoding → byte-reproducible recaps), guarded by a **verify-and-retry**
> validator, and — the part most vendors skip — a **deterministic,
> GPU-free eval harness** rolled up into a public multilingual
> leaderboard with a byte-for-byte CI gate.

## What you are building

`synthesis_pipeline`: scope-window synthesis (channel / domain / tenant
recaps), grammar-constrained outputs, elected-device election, and
encrypted publication of the result. Plus the eval that gates it:
`crates/synthesis_pipeline/src/eval.rs`, the `demos/synthesis-eval/`
harness, and `leaderboard.py`.

## Build it: make synthesis deterministic first

Before quality, establish *reproducibility* — an irreproducible pipeline
can't be evaluated. The trap is sampling: a `llama-server` completion
call that sends `n_predict`, `temperature`, and the grammar but **no
seed** inherits the default seed `-1`, so every call reseeds from entropy
and the same prompt can wander down a different path. A "better" briefing
would just be luck.

The pin is a `SamplingConfig` with a **fixed seed and greedy decoding**,
threaded through one shared request builder so every transport
(on-device and managed-cloud) sends identical knobs. Result:
`(model, prompt) → recap` is byte-reproducible. Once that holds, you can
build everything else:

- **Verify-and-retry validator** — catches the meta-commentary failure
  mode (the model narrating *about* the task instead of doing it) and
  retries.
- **Adaptive token budget** — sizes the output to the input instead of a
  fixed 512.
- **Exemplar grounding** — a concrete few-shot exemplar's words can bleed
  into unrelated bundles, so the lists are grounded in session evidence
  and stripped before persistence, observable via the
  `knowledge_synthesis_exemplar_leaks_stripped_total` metric.

## Build it: three deterministic scorers

The eval harness scores recaps with three GPU-free, offline scorers that
match the production crate, so the library, the CI gate, and the
leaderboard all agree on what they measure:

| Scorer | Question | How |
|---|---|---|
| **Term coverage** | Does the recap surface the key facts? | Fraction of labelled expected-terms it mentions |
| **Faithfulness / grounding** | Does it invent entities? | Flags recap entities absent from the session evidence |
| **In-language** | Is it in the session's own language? | Unicode-script detector, tolerating embedded Latin product names |

`leaderboard.py --check` is a **byte-for-byte CI gate**: the published
[multilingual leaderboard](../../docs/technical/multilingual-leaderboard.md)
regenerates from one command, and any drift fails the build.

## The evidence — including where it's weak

The default Bonsai-1.7B Q2_0 board, aggregated over recorded recaps:

| Language | Script | In-language | Notes |
|---|---|---|---|
| Spanish | Latin | yes | 77% term coverage |
| German / French | Latin | yes | 50% / 40% |
| **Japanese / Chinese** | CJK | **no (1.7B)** | 4B recovers in-language |
| **Arabic** | Arabic | yes (1/1) on default; 4B for breadth |

The honest contract: **8/10 recorded languages are fully in-language on
the default model**, and the misses are the documented 2-bit non-Latin
limitation — fixed by the opt-in 4B, not by pretending. Languages with
no recorded run are listed as `pending`, never scored with a placeholder.

## The business decision: publish your quality bar

**Scenario.** A prospect asks, "How good is the summarization in
Japanese?" Most vendors answer with a marketing adjective.

- **Vendors who don't publish (the default).** Quality is a black box;
  you take their word for it; per-language behaviour is unknowable until
  you run your own eval.
- **Knowledge.** Hand them the leaderboard. It says, in numbers, that
  Japanese needs the 4B tier, that term coverage on English is only 29%
  on the 1.7B, and exactly which languages are `pending`. That candor
  *is* the differentiator — it lets a buyer make a calibrated decision
  instead of discovering the limit in production.

The agent memory layers (Mem0/Zep) publish recall leaderboards, but
English-centric ones. Knowledge competes on the axes a private,
on-device substrate can win outright: **multilingual breadth (SEA / GCC /
CJK / Arabic) and in-language correctness**, measured reproducibly.

## How a competitor would build this

A cloud product synthesizes with a frontier model and rarely publishes a
quality breakdown — it doesn't have to, because the model is good enough
that the risk is reputational, not technical. That works when you have a
70B-class model in a datacenter. When your model is a 1.7B on a phone,
*measuring and publishing* the limit is what makes the product
trustworthy — and the deterministic harness is what makes the
measurement repeatable.

## What's next

Synthesis answers "what happened." The reasoning plane answers the
harder questions — "what contradicts this," "how has our belief drifted,"
"why was this retrieved" — and it's the capability that separates a
memory *graph* from a search box. Next.

---
*Part 6 of "How to Build Knowledge." [Previous: Inference Routing on Device](05-inference-routing.md) | [Next: The Reasoning Plane](07-the-reasoning-plane.md) | [Series index](README.md)*
