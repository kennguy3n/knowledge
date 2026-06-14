# Getting Started for Product Teams

You're evaluating whether Knowledge fits your product. This page frames
what it enables, where it fits, and how to decide.

## What Knowledge gives your product

- **Private memory that's actually private.** Each user's data lives on
  their device, encrypted. There is no server-side copy to breach,
  subpoena, or accidentally leak. For privacy-sensitive verticals this
  is a feature you can put on the box.
- **$0 marginal cost per user.** On-device synthesis means you don't
  pay per-token inference costs that scale with your user base. See
  [../operator/cost-model.md](../operator/cost-model.md).
- **Works offline and multilingually — and we measure it.** Retrieval and
  extraction run on the device across 22 languages — useful in emerging
  markets and for global products — and synthesis quality is graded by a
  reproducible offline eval harness we publish on a
  [per-language leaderboard](../technical/multilingual-leaderboard.md)
  rather than asserted.
- **Reasoning, not just retrieval.** Beyond similarity search, the product
  can answer *what contradicts X?*, *how has belief about X drifted?*, and
  *why was this retrieved?* — privately, on-device, scope-isolated — via
  the gateway's `/api/v1/reasoning/*` endpoints.
- **Future-proof confidentiality.** Post-quantum crypto protects data
  with a long confidentiality horizon against harvest-now/decrypt-later.

## What you can build

| Pattern | Example | Mode |
|---|---|---|
| B2C chat with memory | A private assistant like KChat | On-device |
| B2B knowledge tool | A team knowledge base over Notion/Slack/Drive | Hybrid / Enterprise |
| Agent memory | Structured long-term memory for an LLM agent | On-device / Hybrid |
| Vertical app | Healthcare, finance, legal, education tools | Any (compliance-driven) |

More detail in [../product/use-cases.md](../product/use-cases.md).

## How it deploys

Three modes — on-device, hybrid, enterprise — trade infrastructure for
reach. The [deployment scenarios](../product/deployment-scenarios.md)
page has a decision tree mapping business shape to mode.

## How it compares

For an honest comparison against hosted memory layers (Mem0, Zep,
Letta/MemGPT), vector DBs (Pinecone, Weaviate), enterprise assistants
(Copilot, Glean, Notion AI, NotebookLM, Dust), and managed ETL
(Fivetran, Airbyte), see
[../product/comparison.md](../product/comparison.md). Knowledge's wedge:
on-device privacy at $0 marginal cost, cryptographic forgetting, a
published multilingual eval board, and a reasoning plane. Pricing figures
there are publicly-reported, order-of-magnitude — not vendor quotes.

## Common questions

See the [FAQ](../product/faq.md) and the public
[roadmap](../product/roadmap.md).

## What's next

Ready to prototype? Hand your engineers
[for-developers.md](for-developers.md) and the
[Quickstart](../QUICKSTART.md).
