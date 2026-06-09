# Executives on the Substrate — A Field Series

> **TL;DR:** Five executives, five countries, seven languages, one
> on-device knowledge substrate. This series drives the *real* running
> system — gateway, encrypted substrate, and the Bonsai-1.7B model via
> `llama-server` — through realistic business situations and reports
> what actually happened: the screens, the queries, and the model's
> verbatim input and output. Including where the output is weak.

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
   including cross-language recall and the FTS5 fix that makes
   `BR-2505`-style business identifiers searchable.
3. **[Synthesis Quality: An Honest Critique](03-synthesis-quality.md)** —
   verbatim model output, good and bad, and the one finding that matters
   most: the grammar-constrained path produces dramatically better
   briefings than the free-form trigger path.
4. **[The UI, and What It Honestly Reveals](04-design-and-product-gaps.md)** —
   the design pass that took the reference UI from monotone to
   professional, and the product gap the UI made impossible to hide.

## Reproducing this

```bash
# Stack: gateway :8080, llama-server :8081 (Bonsai-1.7B), UI :3002
export KNOWLEDGE_GATEWAY_URL=http://localhost:8080
export KNOWLEDGE_API_KEY=ci-demo-key
export LLAMA_SERVER_URL=http://localhost:8081
python3 demos/executive-personas/run_personas.py
# → results/<persona>.md + .json (with verbatim model I/O)
#   results/executive_summary.md
```

Everything below is drawn from those outputs and from the running UI.
