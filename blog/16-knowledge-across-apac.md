# Knowledge Across APAC

> **TL;DR:** APAC bundles three hard problems at once — CJK and
> Southeast-Asian scripts, strict data-residency regimes, and budget
> devices in emerging markets. Knowledge's substring-aware multilingual
> extraction, on-device (no cross-border transfer) model, and
> device-tier inference routing address all three.

## The Business Problem

A company rolling out an AI knowledge product across Asia-Pacific hits
three constraints that Western-market designs usually ignore:

1. **Scripts.** Chinese, Japanese, and Korean don't separate words with
   spaces; Thai, Lao, Khmer, and Myanmar have their own segmentation
   rules. Extraction pipelines built around whitespace tokenization
   quietly fail on half the region's languages.

2. **Data residency.** Several APAC jurisdictions restrict
   cross-border transfer of personal data. Routing user content to a
   model or vector store in another country is a legal problem, not
   just a latency one.

3. **Devices.** Large parts of the market run on budget Android phones
   with 2–4 GB of RAM. A feature that needs a flagship device or a GPU
   excludes the majority of users.

A single product has to work across all three at once — and the usual
fixes for one tend to make the others worse (e.g. a big cloud model
fixes scripts but breaks residency and assumes good connectivity).

## The Technical Approach

Knowledge addresses the three constraints with three subsystems already
covered in Series 1:

- **Script-aware extraction** ([post 2](02-multilingual-extraction-engine.md)).
  The observation engine uses per-language matching strategies,
  including a **Substring** strategy for scripts without inter-word
  whitespace (CJK Han, Thai, Lao, Khmer, Myanmar, Tibetan) and
  per-sentence language detection so mixed-script messages are handled
  correctly. CJK extraction is a first-class case, not an afterthought
  — see the [extraction-quality doc](../docs/technical/extraction-quality.md).
- **No server = no cross-border transfer** ([post 1](01-why-on-device-memory.md)).
  Because the substrate is on-device by default, user content does not
  cross a border to be embedded, retrieved, or synthesized. The
  cleanest way to satisfy a data-residency rule is to not transfer the
  data at all — which is the default posture, not a special
  configuration. The [compliance doc](../docs/operator/compliance.md)
  discusses residency considerations.
- **Device-tier inference** ([post 5](05-on-device-inference-under-constraints.md)).
  The router's device-tier gating means a 2 GB handset runs a lighter
  task profile (or the deterministic fallback) while a capable device
  runs full synthesis — one app, the whole device range.

## Implementation Walk-through

The APAC-specific work is mostly *not* special-casing — it falls out of
the defaults:

```text
ingest_message(scope, "会議は金曜日に延期することを決定した", ...)
  // per-sentence detection -> Japanese; substring matcher extracts the decision
query(scope, "金曜日")                       // CJK FTS retrieval, on-device
```

Adding or tuning a language is a lexicon change, not a code change
([post 2](02-multilingual-extraction-engine.md)), so regional
vocabulary can be extended without forking the engine. For residency,
choosing on-device or in-region hybrid deployment
([post 7](07-zero-to-production-deployment.md)) keeps data within the
required boundary. For devices, the inference router is configured with
the device tier and degrades gracefully — no per-market app build
required.

## Performance & Cost Implications

Multilingual extraction throughput is consistent across languages
([post 2](02-multilingual-extraction-engine.md)) — there is no "slow
path" for non-Latin scripts — and retrieval over CJK content uses the
same FTS5-backed hybrid retriever at the ~9.7 ms latency from
[post 8](08-performance-at-device-scale.md).

The cost picture is especially favorable in price-sensitive markets:
on-device operation means $0 marginal infrastructure cost
([post 10](10-cost-engineering-zero-marginal.md)) even at very large
user counts, and no per-token model bill — important when serving the
next billion users on inexpensive hardware where a usage-metered cloud
model would be economically untenable.

## What's Next

APAC highlights language and device diversity. Education shares the
device-and-connectivity constraints but adds a sharp focus on minors'
privacy. The next post covers Knowledge for education.

---
*This is part 16 of the "Building Knowledge" series. [Previous: Knowledge for Legal](15-knowledge-for-legal.md) | [Next: Knowledge for Education](17-knowledge-for-education.md)*
