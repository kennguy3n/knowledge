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
- **Works offline and multilingually.** Retrieval and extraction run on
  the device across 22 languages — useful in emerging markets and for
  global products.
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

For an honest comparison against Copilot, Glean, Notion AI, and
Pinecone, see [../product/comparison.md](../product/comparison.md).

## Common questions

See the [FAQ](../product/faq.md) and the public
[roadmap](../product/roadmap.md).

## What's next

Ready to prototype? Hand your engineers
[for-developers.md](for-developers.md) and the
[Quickstart](../QUICKSTART.md).
