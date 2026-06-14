# Synthesis Quality: From a Lottery to a Pipeline

> **TL;DR:** An earlier version of this post made an uncomfortable
> admission: a 1.7B model on CPU could write a genuinely useful briefing
> on one scope and ramble for 512 tokens on the next — and the same
> prompt could give a different answer on every run. That was a real
> defect, and it is now fixed at the root. Synthesis is **deterministic**
> (a fixed seed + greedy decoding make `(model, prompt) → recap`
> byte-reproducible), guarded by a **verify-and-retry** validator that
> catches the meta-commentary failure mode, and sized by an **adaptive
> token budget**. We also closed a prompt-design flaw that let a few-shot
> exemplar's words bleed into unrelated bundles. What remains is an honest
> *model-capability* limit — non-Latin synthesis at 2-bit (CJK and
> Arabic) — which we measure and address by upgrading the model, not by
> pretending it away. Coverage is now ten languages across four scripts.

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

One honest detail about *what* the validator scores: it gates the
`recap` — the headline the briefing and the Memory page surface — not the
structured `decisions`/`active_tasks` lists beneath it. The production
prompt carries a single format-only few-shot exemplar to steer the 2-bit
model away from prefacing. In an earlier edition that exemplar was a
*concrete* business sentence ("Adopt Postgres for the billing store"),
and the 2-bit model copied it verbatim into the `decisions`/`active_tasks`
of unrelated sessions — two of the five persona bundles showed the leak,
and replaying the same prompt against the 4B leaked it too. So it was a
**prompt-design flaw, not a capacity limit**, and we fixed it at the
root rather than papering over it with a bigger model.

The exemplar is now an **abstract placeholder** —
`EXAMPLE_DECISION` / `EXAMPLE_TASK` — in both the production template
(`crates/inference_router/src/task.rs`) and the demo harness:

```text
// crates/inference_router/src/task.rs — the exemplar uses placeholder
// tokens, not a plausible business sentence, so a verbatim copy is
// unmistakably a demo artefact rather than a real-looking false decision
// in someone else's recap.
Example session (format illustration only):
Observations:
- [decision] (important) EXAMPLE_DECISION
- [task] (important) EXAMPLE_TASK
Example output:
{"recap":"EXAMPLE_DECISION was agreed and EXAMPLE_TASK was scheduled.",
 "decisions":["EXAMPLE_DECISION"],
 "open_questions":[],"active_tasks":["EXAMPLE_TASK"]}
```

After regenerating all five personas against the live stack, **no bundle
contains the leaked exemplar** — neither the old "Adopt Postgres" string
nor the new placeholder tokens. The 2-bit model now either leaves the
lists empty or fills them from the session's own evidence (Kenji's bundle,
for instance, carries the AX-7 engineering note it was actually given).
The recap stayed faithful throughout; the fix removes the one place where
a grammar-valid bundle could surface borrowed content as if it were the
session's own knowledge.

### From smaller blast radius to a hard guarantee — and a signal to watch it

Abstracting the exemplar shrinks the *blast radius* of a leak (a copied
`EXAMPLE_DECISION` is obviously an artefact, not a plausible false
decision) but it does not, on its own, stop a 2-bit model from emitting
the token. The complete fix grounds the structured lists in the session's
own evidence: on **every** attempt, before the bundle is scored or
persisted, the quality gate runs `strip_exemplar_leak`, deleting any
`decisions`/`open_questions`/`active_tasks` entry that copied an exemplar
placeholder. A leak in the `recap` (which can't be excised mid-prose)
instead forces a fact-only retry. The exemplar tokens live in a single
`inference_router::SYNTH_EXEMPLAR_TOKENS` constant that a bidirectional
drift-guard test pins to the prompt, so the strip list can never silently
fall out of sync with what the prompt actually teaches the model.

Because the strip is silent, "how often is a prompt actually leaking?"
needs to be observable rather than guessed. Each stripped entry now
increments `knowledge_synthesis_exemplar_leaks_stripped_total`, exported on
the substrate's `/internal/metrics` Prometheus surface on **both**
synthesis transports (the on-device FFI path and the server-tier
`LlamaCppSynthesizer`). A healthy prompt holds the counter at `0`; a
rising value is the early warning that a future prompt edit has started
teaching the model to copy. Scraped live after driving real syntheses
through the stack:

```text
# GET /internal/metrics  (text/plain; version=0.0.4)
# TYPE knowledge_synthesis_triggered_total counter
knowledge_synthesis_triggered_total 5
# TYPE knowledge_synthesis_retry_total counter
knowledge_synthesis_retry_total 1          # a thin-evidence "…" recap forced one fact-only retry
# TYPE knowledge_synthesis_exemplar_leaks_stripped_total counter
knowledge_synthesis_exemplar_leaks_stripped_total 0   # the abstract exemplar held; no leak to strip
```

The counter sits in the same snapshot as the sibling synthesis counters
that *did* move during the run, so its `0` is a measured "no leak
occurred", not a dead series — and a render test pins it as a
`_total`-suffixed counter so a future rename can't silently re-type it as
a gauge and break the alert.

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
They cannot fix what the weights cannot do. The multilingual matrix now
spans **ten languages across four script families**, which makes the
boundary precise. The Latin-script languages — English, French, German,
Spanish, and the two we added, **Vietnamese** (heavy stacked diacritics)
and **Indonesian** — synthesise cleanly and **in-language** on the 1.7B:

> **French (1.7B):** *Le litige avec le fournisseur CartoNord sur l'avoir
> de 12 600 EUR est solide; le paiement de la facture FA-2025-0411 de
> 90 000 EUR reste bloqué jusqu'à résolution.*
>
> **Vietnamese (1.7B):** *Quyết định chuyển hệ thống thanh toán từ MoMo
> sang VNPay trong quý tới được thông qua.*

The genuinely interesting new case is **Thai**. Thai is *spaceless* —
like CJK it has no word boundaries — so we expected it to be a stress
case for synthesis the way it is for the FTS lane. It is not: the 1.7B
returns a clean, in-language Thai recap (the 4B does too).

> **Thai (1.7B):** *การตัดสินใจย้ายระบบชำระเงินจาก 2C2P เป็นไปยัง Omise ในไตรมาสหน้า
> โดยผู้รับผูกคือคุณสมชาย และความเสี่ยงคือบริการหยุดชะงักระหว่างการเปลี่ยนระบบ*

So "spaceless" is a *recall*-lane property, not a synthesis blocker. The
scripts where the 1.7B actually breaks are **CJK and Arabic**, and the
failure is specific rather than total. Two things matter here, and we
report both honestly.

First, on the **full production prompt** the 1.7B's non-Latin behaviour
is **unstable from language to language**. In this run it held Japanese
in-language but answered the **Chinese** session in **English**:

> **Japanese (1.7B, full prompt):** *AX-7サーボの過熱はハードウェア故障ではなく、センサーのファームウェアのオフセットが原因である。暫定対策は2503ロットに80%のデューティ上限を適用する。* — in-language.
>
> **Chinese (1.7B, full prompt):** *"PostgreSQL migration from MySQL to be
> scheduled for next iteration, with risk of downtime during transition…"*
> — faithful, but in the **wrong language**.

An earlier edition of this post recorded the **opposite** split (Chinese
in-language, Japanese in English) from a different run. That flip is
itself the finding: determinism guarantees that an *identical* prompt
reproduces byte-for-byte, but it cannot make a 2-bit model *reliable* on
CJK — which of the two CJK languages survives in-language shifts with the
content. An unstable behaviour is not one you ship.

Second, the **controlled** signal is the bare, exemplar-free prompt used
for the head-to-head (it omits the format exemplar so both models are
judged on equal footing). There the 1.7B's non-Latin weakness is
consistent: it collapses to the placeholder `…` on **both** CJK
languages, and answers the **Arabic** (right-to-left) session in
**English** — while the 4B returns a clean, coherent, in-language recap
for **every** one:

> **Japanese — 1.7B:** `…`  →  **4B:** *AX-7サーボの過熱はセンサーのファームウェアオフセットによるものであり、Keyenceがv2.4.1を来週OTAで配信する。*
>
> **Chinese — 1.7B:** `…`  →  **4B:** *会议决定将计费数据库从 MySQL 迁移到 Postgres，并指定 Priya 作为负责人，主要关注切换期间的停机风险。*
>
> **Arabic — 1.7B:** *"The session discussed the migration of the
> database from MySQL to PostgreSQL… responsibility was assigned to
> Bria…"* (wrong language)  →  **4B:** *تم ترحيل قاعدة بيانات الفوترة من
> MySQL إلى Postgres في الدورة القادمة، مع خطر توقف الخدمة أثناء التحويل.*

No prompt change makes the 1.7B reliable on CJK or Arabic; it is a
capacity limit of a 1.7B model quantised to 2 bits. This is exactly the
case the opt-in **Bonsai-4B Q2_0** upgrade exists for, and the
head-to-head is decisive: across all ten languages the 4B is **10/10
in-language**, including every script where the 1.7B drops the language
or the recap. The full per-language comparison (the `usable` quality
gate *and* a script-aware `in-language` check for both models) is in
[`rollup_report.md`](../../demos/multilingual-rollup/results/rollup_report.md),
with the raw recaps in `rollup_results.json` alongside it.

| Language | Script | 1.7B (full prompt) | 1.7B (bare probe) | 4B |
| --- | --- | --- | --- | --- |
| English / French / German / Spanish | Latin | in-language | in-language | in-language |
| Vietnamese | Latin (heavy diacritics) | in-language | in-language | in-language |
| Indonesian | Latin | in-language | in-language | in-language |
| Thai | Thai (spaceless) | in-language | in-language | in-language |
| Japanese | CJK (spaceless) | in-language \* | `…` | in-language |
| Chinese | CJK (spaceless) | wrong language (EN) \* | `…` | in-language |
| Arabic | Arabic (RTL) | in-language | wrong language (EN) | in-language |

\* *Full-prompt CJK is unstable run-to-run: this run held Japanese and
flipped Chinese to English; a prior run did the reverse. The bare-probe
column is the stable, controlled signal — and there the 4B is the fix.*

The 4B model is not free — it is larger and slower — so it is offered as
a **gated, opt-in** upgrade for deployments that need reliable non-Latin
synthesis rather than the default. The point is that the architecture
absorbs it without a pipeline change: shape is grammar-guaranteed,
sampling is deterministic on either model, and the validator runs the
same way. A better model drops in; nothing downstream moves. The full
per-language evidence is in
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
- **Names the one limit it cannot prompt its way out of** — non-Latin
  synthesis at 2-bit (CJK and Arabic) — and offers a measured model
  upgrade for it that is 10/10 in-language across the matrix.

None of this is measured by eyeballing a demo. Quality is graded by a
standing, **offline** eval harness ([`demos/synthesis-eval/`](../../demos/synthesis-eval/),
documented in [`synthesis-eval.md`](../../docs/technical/synthesis-eval.md))
that scores the already-recorded model output with three deterministic
scorers — **term coverage**, **faithfulness/grounding**, and a
script-aware **in-language** check — re-using the same code as the
shipped `crates/synthesis_pipeline/src/eval.rs`, so the demo, the CI
gate, and the library agree on what they measure. Those per-recap scores
roll up into a **public, reproducible
[multilingual leaderboard](../../docs/technical/multilingual-leaderboard.md)**,
per language, with the 1.7B-vs-4B tier comparison and an honest pending
list. That is the axis an on-device, privacy-first substrate competes on
against hosted memory layers like Mem0 or Zep: not a single English
benchmark, but published multilingual, in-language quality that anyone
can regenerate from one command.

That is the difference between a demo and a product: the demo shows a
good output once; the product makes a good output reproducible, gates
the bad ones, measures it in the open, and is honest about the boundary
where only a bigger model will do.

[Post 4](04-design-and-product-gaps.md) turns to the UI — and to the
product gap the earlier edition documented as an empty Memory page,
which is now closed: the user-memory write path is live, and the decay
machine and concept graph have real data to operate on.
