# Synthesis Quality: An Honest Critique

> **TL;DR:** A 1.7B model running on CPU can write a genuinely useful
> business briefing — and, on the next scope, can ramble for 512 tokens
> without saying anything. We show both, verbatim, and explain the
> mechanism: the GBNF grammar guarantees the *shape* of the output, not
> its *substance*. That distinction is the single most important thing
> to understand about on-device synthesis.

This is the post most write-ups would quietly skip. The personas run
synthesis against the real Bonsai-1.7B model via `llama-server`, and the
honest result is: **recall is uniformly strong; synthesis is good when
it's good and visibly weak when it isn't.** Here is the evidence.

## When it works, it really works

Élise's CartoNord briefing, written by the model from six evidence
records, is a usable negotiating position:

> *We will release payment of the 90,000 EUR invoice FA-2025-0411 only
> once a credit note of 12,600 EUR for the non-conforming BR-2505 lot is
> issued. Your 6,000 EUR offer does not cover our verified quarantine
> and re-purchase costs.*

Anand's renewal briefing is similarly tight:

> *The save plan hinges on shipping Okta SSO and getting the new VP Eng
> to sponsor. If we land both, renewal probability goes from 35% to
> ~70%.*

Both are faithful to the source evidence, correctly numeric, and short
enough to act on. This is the promise delivered.

## When it doesn't, the failure is loud

Kenji's AX-7 overheating scope produced this, written verbatim to
channel memory:

> *The session highlights the current state of quality control and the
> proposed mitigation strategies for the AX-7 and other critical
> components. The session also discusses the engineering note regarding
> the AX-7's potential for firmware-based overheating … The session is
> structured as follows: {*

The model spent its entire token budget describing *what it was about to
write* instead of writing it, then ran into the cap mid-sentence. Term
coverage: **3 of 11** expected business terms. Sofía's chargeback recap
shows the other failure mode — a fluent ES/PT summary that ends in a
stray brace where the cap cut it off:

> *Aumento de 6 contracargos en México marcados como posible fraude com
> tarjeta Nubank. Patrón: mesmo BIN, montos altos … no coincidem con a
> factura.* `{`

## The mechanism: shape vs substance

Every synthesis call goes through the **same** path —
`InferenceTask::SynthSummary` with a GBNF grammar that constrains the
model to emit a `SummaryBundle`:

```json
{ "recap": "...", "decisions": [...], "open_questions": [...], "active_tasks": [...] }
```

The grammar is a hard guarantee about **shape**: the output will always
parse into those four fields. It says nothing about **substance** — it
cannot force the `recap` string to be faithful, concise, or even
on-topic. At 1.7B parameters, the model sometimes fills a
grammar-valid `recap` with meta-commentary ("the session highlights…")
that is perfectly well-formed and nearly useless.

Two parameters shape the failure surface, both visible in the code:

```rust
// crates/inference_router/src/adapters/llama_cpp.rs
pub const DEFAULT_N_PREDICT: u32 = 512;     // a *latency* bound, not correctness
pub const DEFAULT_TEMPERATURE: f32 = 0.1;   // synthesis ≈ extraction, kept low
```

`n_predict = 512` is a deliberate latency ceiling: at ~10–15 tok/s on
CPU, 512 tokens is ~30–40 s, while 1024 would be 60–100 s and blow past
the gateway's substrate deadline (the root cause of an earlier class of
spurious `502`s). The cost of that ceiling is that a model which rambles
*runs out of budget before it gets to the point*.

### Same prompt, more room → better output

The harness also replays the **identical** prompt + grammar directly
against `llama-server` with `n_predict = 1024`. For the very same Kenji
scope that rambled under the 512 cap, the larger budget produced:

> *The AX-7 server overheating is firmware-driven, not a hardware fault.
> Sensor offset miscalibration delays fan spin-up. A firmware patch from
> Keyence is in test; interim mitigation is an 80% duty cap on the 2503
> lot.*

Same model, same prompt, same grammar — markedly better, purely from
headroom and an independent sampling draw. This is the honest tension:
the 512 cap that protects latency (and prevents `502`s) is also the cap
that strands a verbose generation. **Output stability is itself a
quality dimension** at this model size, and it trades directly against
response time.

### Truncation is salvaged, not crashed

Crucially, a cut-off generation never breaks the system. When the model
hits the cap mid-JSON, the parser closes the truncated prefix and
re-parses it:

```rust
// crates/inference_router/src/task.rs — SummaryBundle::from_slm_str
// A token-capped prefix (valid JSON head + dangling string/brackets) is
// closed and re-parsed, so a cut-off recap still yields a usable bundle.
```

Anand's replayed bundle is explicitly annotated by the harness: *"The
model hit the token cap mid-output; the bundle below was salvaged by
closing the truncated JSON prefix — exactly as the production
`SummaryBundle::from_slm_str` parser now does."* The stray `{` in
Sofía's recap is the same event, surfaced honestly rather than hidden.

## The scorecard, unhidden

The harness measures recap term coverage against a hand-written list of
business terms we'd want each briefing to contain. The scores are low,
and we report them as-is:

| Persona | Synthesis question | Recap term coverage |
| --- | --- | --- |
| Élise (France) | CartoNord dispute state | 3/10 |
| Kenji (Japan) | AX-7 overheating root cause | 3/11 |
| Sofía (LATAM) | Chargeback spike cause | 4/9 |
| Anand (India) | Acme renewal risk + save plan | 5/10 |
| Lena (Germany) | Quality-8D bearing charge | 2/10 |

A low term-coverage score does **not** mean the briefing is useless —
Élise's 3/10 recap is the strongest single sentence in her whole run.
It means the model paraphrases rather than parrots, so a keyword check
under-credits a faithful paraphrase. But it also honestly captures that
a 1.7B model is not a frontier summariser, and you should design the
product around that.

## The differentiated design is honesty, not magic

What makes this defensible as a *product* is not that the model is
great — it isn't, and we don't pretend otherwise. It is that the system
is built to be honest and robust about a small model's limits:

- **The grammar guarantees structure**, so downstream code never has to
  defend against malformed model output.
- **Truncation is salvaged**, so a slow generation degrades to a shorter
  briefing instead of a `500`.
- **The raw recap is shown to the user verbatim** (see the UI in
  [post 4](04-design-and-product-gaps.md)) — no silent post-editing that
  would hide a weak result.
- **Telemetry is exposed** (`escape_fts_query_total`, the synthesis
  counters) so quality is measurable, not anecdotal.

And the obvious next step is visible in the data: the same prompt at a
higher budget, or a slightly larger on-device model, lifts substance
without changing a line of the pipeline — because shape is already
guaranteed. The architecture is ready for a better model the moment one
fits the device.

[Post 4](04-design-and-product-gaps.md) turns to the UI — the design
pass that made these results presentable, and the product gap the UI
made impossible to hide.
