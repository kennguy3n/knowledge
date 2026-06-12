# Executives on the Substrate — A Field Series

> **TL;DR:** Five executives, five countries, seven languages, one
> on-device knowledge substrate. This series drives the *real* running
> system — gateway, encrypted substrate, and the Bonsai-1.7B model via
> `llama-server` — through realistic business situations and reports
> what actually happened: the screens, the queries, and the model's
> verbatim input and output. Including where the output is weak.
>
> **Updated.** Since the first edition, the synthesis pipeline was
> rebuilt: it is now **deterministic** (fixed-seed greedy decoding →
> byte-reproducible briefings), guarded by a **verify-and-retry**
> validator, and the user-memory write path + concept graph are **live**
> end-to-end (the empty Memory page is now populated). A companion
> harness, [`demos/multilingual-rollup/`](../../demos/multilingual-rollup/),
> adds a multilingual + code-switched + cross-channel roll-up
> demonstration and a Bonsai 1.7B-vs-4B comparison. Posts 2–4 reflect
> the current system.

Most product write-ups show the system at its best. This one does the
opposite: every artifact here is captured from a live run against the
actual stack ([`demos/executive-personas/run_personas.py`](../../demos/executive-personas/run_personas.py)),
and where the on-device model produced a rambling or truncated briefing,
we show that too — and explain why.

The series is built around five personas, each a different role,
country and language mix, each with a realistic mid-crisis situation:

| Persona | Role | Country | Languages |
| --- | --- | --- | --- |
| **Élise Moreau** | CFO, sustainable-packaging maker | France | French, English |
| **田中 健二 (Kenji Tanaka)** | COO, industrial-automation maker | Japan | Japanese, English |
| **Sofía Herrera** | Founder & CEO, DTC beauty brand | Mexico / Brazil | Spanish, Portuguese, English |
| **Anand Iyer** | VP of Customer Success, B2B SaaS | India | English, Hindi |
| **Lena Brandt** | Geschäftsführerin (MD), precision-bearing maker | Germany | German, English |

Across the five, a single run ingests **110 business records** into
**30 encrypted scopes**, asks **20 recall questions** in seven
languages, proves scope isolation, synthesises briefings on-device, and
cryptographically erases five people on request — **57/57 business
checks pass**.

## The posts

1. **[Five Executives, One Substrate](01-five-executives-one-substrate.md)** —
   how the system works, told through Élise's month-end close: scopes as
   encrypted compartments, unified ingest, and the reference UI.
2. **[Multilingual Recall, in Practice](02-multilingual-recall.md)** —
   real queries in French, Japanese, Portuguese, Spanish and Hindi,
   including cross-language recall, code-switched (mixed-language)
   messages, and the FTS5 fix that makes `BR-2505`-style business
   identifiers searchable.
3. **[Synthesis Quality: From a Lottery to a Pipeline](03-synthesis-quality.md)** —
   how the non-determinism bug was fixed at the root (fixed-seed greedy
   decoding → byte-reproducible briefings), the verify-and-retry
   validator that catches the meta-commentary failure mode, and the one
   honest limit a bigger model has to solve: CJK synthesis at 2-bit.
4. **[The UI, and What It Honestly Reveals](04-design-and-product-gaps.md)** —
   the design pass that took the reference UI from monotone to
   professional, and the product gap the UI made impossible to hide —
   the empty Memory page — now closed with a live user-memory write path
   and concept graph.

## Reproducing this

```bash
# Stack: gateway :8080, llama-server :8081 (Bonsai-1.7B), UI :3002
export KNOWLEDGE_GATEWAY_URL=http://localhost:8080
export KNOWLEDGE_API_KEY=ci-demo-key
export LLAMA_SERVER_URL=http://localhost:8081
python3 demos/executive-personas/run_personas.py
# → results/<persona>.md + .json (with verbatim model I/O)
#   results/executive_summary.md

# Multilingual + code-switched + cross-channel roll-up, plus the
# Bonsai 1.7B-vs-4B synthesis comparison (4B server optional on :8082):
export LLAMA_17B_URL=http://localhost:8081
export LLAMA_4B_URL=http://localhost:8082   # optional, for --compare-4b
python3 demos/multilingual-rollup/run_rollup.py --compare-4b
# → demos/multilingual-rollup/results/rollup_report.md + rollup_results.json
```

Everything below is drawn from those outputs and from the running UI.
