# 30 Connectors for Vietnam, SEA & the GCC

> **TL;DR:** Knowledge adds **30 region-focused connectors** — 10 for
> Vietnam, 10 for Singapore/Thailand/SEA, and 10 for the GCC/Middle East
> — taking the catalog to **70 stable**. They sit behind the same
> `Connector` contract as every other source, and they pair naturally
> with on-device mode (data never leaves the region) and the
> extraction engine's built-in Vietnamese, Thai, and Arabic lexicons.

## The Business Problem

The "connect your SaaS tools" story usually assumes a North American or
European stack: Salesforce, Slack, Google Drive. But a retailer in Ho
Chi Minh City runs on Zalo, MoMo, and KiotViet; a logistics startup in
Bangkok lives in LINE, PromptPay, and SCB Easy; a marketplace in Dubai
is built on Noon, Careem, Talabat, and Tabby. Their institutional
knowledge is just as scattered as anyone else's — it is simply spread
across a different set of platforms, most of which no Western knowledge
tool integrates with at all.

These markets also have the strongest reasons to care about where data
lives. Data-residency expectations, a preference for keeping customer
PII in-region, and intermittent connectivity all favor an architecture
that processes knowledge **on the device or on in-region infrastructure**
rather than shipping every message to a vendor cloud on another
continent. That is exactly what Knowledge is.

## The catalog

All 30 ship as **stable**, implementing the full `Connector` contract —
OAuth2 (or the provider's native auth) with refresh, the
`full → incremental → failure → recovery` sync state machine, content
fetch under per-provider rate limits, optional webhooks, and ACL
projection — with `MockHttpTransport` unit coverage against canned
provider responses.

- **Vietnam (10)** — Zalo, VNPay, MoMo, Tiki, Shopee VN, Lazada VN,
  Viettel Post, KiotViet, Sapo, Base.vn. Chat, payments, e-commerce,
  logistics, retail POS, and the Base.vn work platform.
- **Singapore / Thailand / SEA (10)** — LINE, Grab, Gojek, Talenox,
  Odoo (SEA), Fastwork, TrueMoney, SCB Easy, PromptPay, Tokopedia.
  Messaging, super-apps, HR/payroll, ERP, freelance marketplaces, and
  the region's dominant payment rails.
- **GCC / Middle East (10)** — Careem, Talabat, Noon, Amazon.ae, Tabby,
  Foodics, Zoho, Bayt, Fetchr, PayFort. Ride-hailing, food delivery,
  e-commerce, buy-now-pay-later, restaurant POS, recruitment, last-mile
  logistics, and payments.

The full list and per-provider status live in the
[connector maturity table](../docs/product/roadmap.md#connector-maturity).

## One contract, many auth schemes

The regional providers stretch the auth surface well beyond OAuth2. A
crate-internal `signing` module provides the primitives several of them
require: **HMAC-SHA256** request signing, raw **SHA-256** digests, and
**AWS Signature v4** (for Amazon.ae's SP-API). Centralizing these means
each connector declares *which* scheme it uses rather than re-deriving
the cryptography — and the substrate downstream still sees the same
uniform `DocumentCreated` / `DocumentUpdated` evidence regardless of how
the source authenticated.

## Data residency is an architecture, not a checkbox

For a connector pulling from a regional platform, the privacy win is not
a policy promise — it is where the bytes physically go. In on-device
mode the full pipeline (ingest → extract → remember → synthesize) runs
locally, so content fetched from MoMo or Careem is extracted and
synthesized on the user's device and never traverses a vendor cloud. In
the hybrid/enterprise modes you run the gateway and substrate on your own
in-region infrastructure. Either way the
[cryptographic forgetting](03-memory-that-forgets.md) guarantees apply:
forgetting a scope destroys its per-scope key, so the data is
unrecoverable rather than soft-deleted.

## Multilingual extraction, on day one

A connector is only useful if the substrate can actually understand what
it ingests. Knowledge's
[extraction engine](02-multilingual-extraction-engine.md) is validated
across 22 languages, and the three that matter most for these regions —
**Vietnamese (`vi`)**, **Thai (`th`)**, and **Arabic (`ar`)** — each
ship a full lexicon (decision/task keywords, imperative verbs,
stop-words) and an interrogative table, with script-aware detection
(Thai and Arabic are non-Latin scripts; Arabic is right-to-left). So a
Zalo thread in Vietnamese, a LINE chat in Thai, or a Talabat support
exchange in Arabic produces the same structured observations — tasks,
questions, decisions, entities — as an English one. The validated set
and its per-language scripts are listed in the
[design doc](../docs/technical/design.md).

## What it means for you

If you are building for these markets, the integration surface you
actually need now exists behind the same contract as the rest of the
catalog: OAuth a regional connector from the
[admin dashboard](20-admin-without-ops.md), let the substrate sync and
synthesize on-device or on your in-region infrastructure, and ask
questions across all of your sources — with each source's permissions
intact, in the local language, at $0 marginal cost per user.

## Further reading

- [70 Connectors](19-connector-ecosystem.md) — the full catalog and the
  one-contract design behind it.
- [Connector Architecture](06-connector-architecture.md) — the
  `Connector` trait these providers implement.
- [The Multilingual Extraction Engine](02-multilingual-extraction-engine.md)
  — how structured extraction works across 22 languages.
- [Knowledge Across APAC](16-knowledge-across-apac.md) — CJK/Thai
  extraction, data residency, and device constraints in the region.
