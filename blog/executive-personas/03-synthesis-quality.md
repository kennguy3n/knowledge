# Synthesis Quality: From a Lottery to a Pipeline

> **TL;DR:** An earlier version of this post made an uncomfortable
> admission: a 1.7B model on CPU could write a genuinely useful briefing
> on one scope and ramble for 512 tokens on the next — and the same
> prompt could give a different answer on every run. That was a real
> defect, and it is now fixed at the root. Synthesis is **deterministic**
> (a fixed seed + greedy decoding make `(model, prompt) → recap`
> byte-reproducible), guarded by a **verify-and-retry** validator that
> catches the meta-commentary failure mode, and sized by an **adaptive
> token budget**. What remains is an honest *model-capability* limit —
> CJK synthesis at 2-bit — which we measure and address by upgrading the
> model, not by pretending it away.

This is the post most write-ups would quietly skip, and the earlier
edition kept that promise by showing the failures verbatim. The system
has since changed underneath it, so this edition reports the new
behaviour with the same candour — including the evidence that the old
"lottery" is gone and the one limit that a bigger model, not a better
prompt, has to solve.

## What was actually broken: non-determinism

The original symptom — "good one run, rambling the next" — was widely
read as "the model is small." It was partly that, but the larger cause
was a **determinism bug**. The `llama-server` completion call sent only
`n_predict`, `temperature`, and the grammar; it sent **no seed**. With
`llama-server`'s default seed of `-1`, every call reseeds from entropy,
so even at a near-zero temperature the same prompt could resolve ties
differently and wander down a different path. The earlier post even
credited a better Kenji briefing to *"an independent sampling draw"* —
which is to say, to luck.

The fix is a `SamplingConfig` with a **fixed seed and greedy decoding**,
threaded through one shared request builder so every transport (on-device
and managed-cloud) sends the identical knobs:

```rust
// crates/inference_router/src/config.rs — SamplingConfig::synthesis_default()
seed:           0,     // fixed — the heart of the reproducibility fix
temperature:    0.0,   // greedy: always take the most-likely token
top_k:          1,     // keep only that token (top_p/min_p inert under greedy)
top_p:          0.9,   // carried for hosts that opt into temperature > 0
min_p:          0.05,
repeat_penalty: 1.1,
```

The result is measurable, not aspirational. The roll-up harness fires
the **identical** synthesis prompt at the on-device model twice and
compares the bytes:

> *Determinism probe: fired the identical synthesis prompt 2× at the
> on-device model. Byte-identical output: **True** (351 chars).*

Same model, same prompt, same bytes — every time. The "lottery" is gone.
A briefing you can reproduce is a briefing you can review, diff, cache,
and trust; one you cannot reproduce is an anecdote.

## The grammar still guarantees shape — now a validator guards substance

Every synthesis call still goes through the **same** path —
`InferenceTask::SynthSummary` with a GBNF grammar that constrains the
model to emit a `SummaryBundle`:

```json
{ "recap": "...", "decisions": [...], "open_questions": [...], "active_tasks": [...] }
```

The grammar is a hard guarantee about **shape**: the output always
parses into those four fields. The earlier post's key insight stands —
the grammar says nothing about **substance**, so a grammar-valid `recap`
could still be filled with meta-commentary ("the session highlights…")
that is well-formed and useless. The Kenji AX-7 scope produced exactly
that:

> *The session highlights the current state of quality control and the
> proposed mitigation strategies for the AX-7 … The session is
> structured as follows: {*

What changed is that this is no longer shipped silently. A
**verify-and-retry validator** now runs after every synthesis and scores
the bundle before it is written:

```rust
// crates/synthesis_pipeline/src/quality.rs (paraphrased)
// A recap is low-quality if it:
//   - opens with meta-commentary ("the session", "this summary", "in summary", …)
//   - is shorter than MIN_RECAP_CHARS (12) — a placeholder like "…"
//   - parrots the prompt instead of summarising the evidence
// On a low-quality first attempt, synthesise ONCE more with a larger
// budget + a fact-only instruction suffix, and keep the better bundle.
```

So the "the session highlights…" opener is now *detected*, and the
pipeline gets a second, larger attempt with an instruction that
explicitly forbids the preface. Quality stopped being a coin-flip the
reader has to audit and became a gate the pipeline enforces, with
counters (`synthesis_retry_total`, `lowquality`, `truncated`,
recap-length) so it is measurable rather than anecdotal.

One honest caveat about *what* the validator scores: it gates the
`recap` — the headline the briefing and the Memory page surface — not the
structured lists beneath it. The production prompt carries a single
format-only few-shot exemplar ("Adopt Postgres for the billing store") to
steer the model away from prefacing, and that exemplar's words sometimes
bleed verbatim into the `decisions`/`active_tasks` of an unrelated
session (two of the five persona bundles show it). We checked whether
this was a small-model artifact: it is **not** — replaying the same
prompt against the 4B leaks the exemplar too, so it is a prompt-design
trade-off, not a capacity limit. The recap itself stays faithful on both
models. We call it out here rather than hide it — and it is a live
follow-up on the production prompt (make the exemplar abstract so there
is nothing real to copy), because that is the whole point of this series.

## The budget adapts instead of guessing

The earlier post described a hard tension: a `512`-token cap protects
latency (and prevents the substrate-deadline `502`s) but *strands a
verbose generation that runs out of room before it gets to the point*.
That cap is now **adaptive**, sized to the evidence window rather than
fixed:

```rust
// crates/synthesis_pipeline/src/quality.rs
pub const MIN_N_PREDICT: u32 = 512;     // floor — equals the env default
pub const MAX_N_PREDICT: u32 = 1024;    // ceiling — stays under the deadline
pub const TOKENS_PER_ROW: u32 = 24;     // budget = MIN + rows * 24, clamped
// verify-and-retry's second attempt is granted strictly more room,
// saturating at RETRY_N_PREDICT = 1536.
```

A three-line scope gets a tight budget; a twenty-line scope gets more,
up to a ceiling chosen so synthesis never blows the gateway's substrate
deadline. The retry always gets strictly more room than the first
attempt. The "ran out of budget mid-sentence" failure is now the
*trigger* for a larger retry, not an accident the user discovers.

### Truncation is still salvaged, not crashed

The robustness property the earlier post praised is unchanged: a
token-capped generation never breaks the system. When the model hits the
cap mid-JSON, `SummaryBundle::from_slm_str` closes the truncated prefix
and re-parses it, so a cut-off recap still yields a usable bundle. With
the adaptive budget and retry in front of it, truncation is now rarer —
but when it happens it still degrades to a shorter briefing instead of a
`500`.

## The roll-up: consolidation, measured

The new evidence harness (`demos/multilingual-rollup/`) tests the thing
the product is actually *for*: collapsing many overlapping messages into
one useful memory. Six messages were posted to a single `eng-billing`
channel — three of them restating the **same** decision in different
words, plus an open question, a task, and a budget sign-off:

> *"we will migrate the billing database to Postgres next sprint…"*
> *"the billing DB move to Postgres is locked in for next sprint…"*
> *"Postgres is the call for billing; Priya owns the cutover…"*

Synthesis consolidated them into a single recap naming the Postgres
billing migration, Priya as owner, the runbook task and the finance
sign-off — and, crucially, the substrate marked the resulting memory
**Reinforced** with a retention score of `1.0`, because the decision was
*repeated* across messages. That is the decay state machine doing its
job: knowledge that recurs is reinforced, not duplicated. (The 1.7B
recap is still a touch verbose — it echoes the standup phrasing — which
is the kind of honest residue we keep visible rather than edit out.)

## The remaining limit is the model, and we name it

Determinism, the validator and the adaptive budget fix the *pipeline*.
They cannot fix what the weights cannot do. The multilingual matrix
makes the boundary precise. Latin-script languages — English, French,
German, Spanish — synthesise cleanly and **in-language** on the 1.7B
model:

> **French:** *Le litige avec le fournisseur CartoNord sur l'avoir de
> 12 600 EUR est solide; le paiement de la facture FA-2025-0411 de
> 90 000 EUR reste bloqué jusqu'à résolution.*

CJK is the honest hard case. On the 1.7B model the same pipeline either
answers a Japanese session **in English** (a language-retention failure)
or drops characters from a Chinese recap:

> **Japanese (1.7B):** *"Keyence's firmware v2.4.1 will be released via
> OTA…"* — fluent, faithful, but in the **wrong language**.
> **Chinese (1.7B):** *"上仓报库差: SKU-8842 实数比统录 120 件…"* —
> on-topic but with characters dropped.

No prompt change rescues this; it is a capacity limit of a 1.7B model
quantised to 2 bits on CJK scripts. This is exactly the case the
opt-in **Bonsai-4B Q2_0** upgrade exists for, and the head-to-head is
decisive: where the 1.7B model returns the placeholder `…`, the 4B model
returns topical, in-language CJK.

| Language | Script | 1.7B usable | 4B usable | What the 4B recovers |
| --- | --- | --- | --- | --- |
| English / French / German / Spanish | Latin | yes | yes | tighter, less verbose recaps |
| Japanese | CJK | **no** (`…`) | **yes** | in-language topical recap |
| Chinese | CJK | **no** (`…`) | **yes** | in-language topical recap |

The 4B model is not free — it is larger and slower — and even at 2 bits
it still drops the occasional CJK character, so it is offered as a
**gated, opt-in** upgrade for deployments that need CJK synthesis rather
than the default. The point is that the architecture absorbs it without
a pipeline change: shape is grammar-guaranteed, sampling is deterministic
on either model, and the validator runs the same way. A better model
drops in; nothing downstream moves. The full per-language evidence is in
[`demos/multilingual-rollup/results/rollup_report.md`](../../demos/multilingual-rollup/results/rollup_report.md).

## The differentiated design is honesty *plus* a working pipeline

The earlier post argued the product was defensible because it was honest
about a small model's limits. That is still true — but honesty is no
longer the *only* answer to the failures. The system now:

- **Guarantees structure** via the grammar, so downstream code never
  defends against malformed output.
- **Guarantees reproducibility** via fixed-seed greedy decoding, so a
  briefing is byte-identical run to run.
- **Guards substance** via verify-and-retry, so the meta-commentary
  failure mode is caught and retried, not shipped.
- **Sizes the budget to the evidence**, so verbose generations get room
  instead of being stranded.
- **Salvages truncation**, so a slow generation degrades to a shorter
  briefing instead of a `500`.
- **Exposes telemetry** (the synthesis quality counters), so quality is
  measured, not asserted.
- **Names the one limit it cannot prompt its way out of** — CJK at 2-bit
  — and offers a measured model upgrade for it.

That is the difference between a demo and a product: the demo shows a
good output once; the product makes a good output reproducible, gates
the bad ones, and is honest about the boundary where only a bigger model
will do.

[Post 4](04-design-and-product-gaps.md) turns to the UI — and to the
product gap the earlier edition documented as an empty Memory page,
which is now closed: the user-memory write path is live, and the decay
machine and concept graph have real data to operate on.
