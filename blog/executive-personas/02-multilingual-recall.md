# Multilingual Recall, in Practice

> **TL;DR:** Across five personas we ran 22 recall questions in French,
> Japanese, Portuguese, Spanish, Hindi and English — including
> *cross-language* recall (ask in English over Japanese records). Every
> one returned the right evidence. The roll-up harness then extends the
> matrix to ten languages across four script families — adding
> Vietnamese (heavy Latin diacritics), Thai (spaceless, like CJK),
> Indonesian, and Arabic (right-to-left) — and recall holds in every one,
> on native script. This post shows the real queries and hits, and how
> business identifiers like `BR-2505` stay searchable despite FTS5's
> punctuation grammar.

## Recall is hybrid, and language-aware

A query runs through `POST /api/v1/query`, which fuses three signals:
full-text (FTS5), recency, and vector similarity. The UI surfaces all
three so you can see *why* something ranked where it did:

![Searching the literal business identifier BR-2505 returns three ranked hits across French and English evidence, each with fts/recency/vector breakdown.](assets/03-search-br2505.png)

The interesting part is that the corpus is genuinely multilingual. The
extraction engine ([post 2 of the foundation series](../02-multilingual-extraction-engine.md))
handles 22 languages; here are real hits, verbatim, in six of them —
then four more from the roll-up harness, for ten in total.

## French — tie the invoice to the dispute

> **Q (`treasury-cashflow`):** `GoCardless prélèvements`
>
> → *GoCardless : 38 prélèvements SEPA programmés pour le 5 du mois,
> total 96 400 EUR. Deux mandats clients ont échoué le mois dernier
> (compte clôturé, provision insuffisante) — relance en cours.*
>
> ✓ matched `['GoCardless', 'mandats', 'échoué']`

## Japanese — CJK recall, native script

> **Q (`supply-disruption-kobe`):** `神戸港`
>
> → *台風の影響で神戸港のコンテナレーンが封鎖され、ハーモニックドライブ
> HD-320 の入荷が最低でも9日遅延します。AX-7セルの最終組立3ラインが部品
> 待ちで停止する見込みです。*
>
> ✓ matched `['神戸港', 'HD-320']`

CJK has no spaces between words, so it goes through a dedicated
trigram/bigram FTS lane. The substrate's health endpoint exposes the
telemetry that proves the right lane fired — `cjk_trigram_lane_queries`,
`bigram_lane_rows` — which is how we know recall is doing real CJK
segmentation, not accidental substring luck.

## Cross-language — ask in English, hit Japanese + English

> **Q [English] (`customer-marubeni`):** `Marubeni firmware`
>
> → *Reply to Marubeni (English): 'Root cause is a sensor-firmware
> offset, not hardware. Interim: an 80% duty cap avoids shutdowns.
> Permanent: Keyence firmware v2.4.1 ships next week via OTA. We will
> prioritise your 40 units and waive the service fee.'*
>
> ✓ matched `['v2.4.1', 'firmware', 'OTA', 'duty']`

## Portuguese & Spanish — one brand, two markets

Sofía runs a DTC beauty brand selling into both Brazil (Portuguese) and
Mexico (Spanish). A Spanish query surfaces mixed ES/PT payment records:

> **Q [Spanish] (`payments-chargebacks`):** `Nubank fraude`
>
> → *Nubank: 6 contracargos en México marcados como posible fraude con
> tarjeta. Patrón — mismo BIN, montos altos, direcciones de envío que no
> coinciden con la facturación.*
>
> ✓ matched `['Nubank', 'fraude', 'BIN', 'contracargo']`

## Hindi — Devanagari support thread

> **Q [Hindi] (`support-escalations`):** `लोड`
>
> → *Zendesk टिकट: 'डैशबोर्ड लोड होने में बहुत समय लग रहा है जब हम 90 दिन
> का डेटा देखते हैं।' समाधान: कवरेज को ऑप्टिमाइज़ किया और कैशिंग चालू की;
> अब लोड समय 8 सेकंड से घटकर 2 सेकंड हो गया।*
>
> ✓ matched `['लोड', 'कैशिंग', '2', '8']`

## Four more scripts — Vietnamese, Thai, Indonesian, Arabic

The personas above cover six languages; the roll-up harness
([`demos/multilingual-rollup/`](../../demos/multilingual-rollup/)) pushes
the same business situation through four more, chosen because each
stresses a *different* part of the index. These are live hits against the
harness's per-language scopes — query token on the left, the record it
retrieved on the right.

**Vietnamese** is Latin script but carries dense stacked diacritics
(`ồ`, `ệ`, `ả`), which a naive tokenizer mangles. A native query keeps
every mark and still matches:

> **Q [Vietnamese]:** `tồn kho`  ("inventory") → **1 hit**
>
> → *Kho Hải Phòng báo cáo thiếu hụt tồn kho: SKU-7720 ít hơn 150 đơn vị
> so với hệ thống ghi nhận; đang điều tra lỗi quét mã.*

**Thai** has no spaces between words, so — like CJK — it routes through
the spaceless trigram/bigram FTS lane rather than word tokenisation. A
bare Thai-script token retrieves its message with no word boundaries to
lean on:

> **Q [Thai]:** `สมชาย`  (the owner, "Somchai") → **1 hit**
>
> → *การตัดสินใจ: ย้ายระบบชำระเงินจาก 2C2P ไปยัง Omise ในไตรมาสหน้า
> ผู้รับผิดชอบคือคุณสมชาย ความเสี่ยงคือบริการหยุดชะงักระหว่างการเปลี่ยนระบบ*

**Indonesian** is plain Latin; recall is unremarkable in the best way —
the billing-migration decision comes straight back:

> **Q [Indonesian]:** `penagihan`  ("billing") → **1 hit**
>
> → *Keputusan: migrasikan basis data penagihan dari MySQL ke Postgres
> pada sprint berikutnya; penanggung jawab Budi, risiko gangguan layanan
> saat peralihan.*

**Arabic** is right-to-left and non-Latin. A native RTL query surfaces
the inventory discrepancy, and the person's name (`بريا`, "Priya") and
the Latin product token `Postgres` both retrieve too — the index does not
care about writing direction:

> **Q [Arabic]:** `المخزون`  ("the inventory") → **1 hit**
>
> → *يبلغ مستودع دبي عن فرق في المخزون: الصنف SKU-9920 أقل بمقدار 130 وحدة
> مقارنة بالنظام؛ يجري التحقيق في خطأ مسح.*

Across these four, every native-script query returned its record
(`tồn kho`/`Hải Phòng`/`VNPay`, `สมชาย`/`ชำระเงิน`/`Omise`,
`penagihan`/`Surabaya`, `المخزون`/`بريا`/`Postgres`). Recall is the
strong half of the system in every script we tried — the *spaceless* Thai
and *RTL* Arabic lanes behave exactly like the well-trodden Latin ones.
Where the scripts genuinely diverge is **synthesis**, which is
[post 3](03-synthesis-quality.md).

## Searching identifiers like `BR-2505`

Business data is full of identifiers with punctuation: `BR-2505`,
`FA-2025-0411`, `HD-320`, decimals like `12,4 %`. A naive FTS5 query
would choke on them.

SQLite's FTS5 `MATCH` grammar treats a hyphen as a column-filter
operator, so a raw query of `BR-2505` parses as *"column BR minus token
2505"* and would be rejected as **HTTP 400 `InvalidQuery`** — for a
string a user could reasonably type. Stripping punctuation is the wrong
answer (you'd lose the ability to search the identifier at all). The
substrate instead preserves the full FTS5 grammar for power users while
making naive input robust:

```rust
// crates/ffi/src/lib.rs — on InvalidQuery, retry once with each
// whitespace-separated token wrapped as an FTS5 string literal.
fn fts_literal_token_fallback(raw: &str) -> String {
    raw.split_whitespace()
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}
```

So the substrate first tries the query as written (advanced FTS5
operators still work); only if that is rejected as invalid does it
retry with `"BR-2505"` quoted as a literal. Both branches are covered by
unit tests, and a dedicated `query_fts_fallback_total` counter on the
metrics snapshot records how often the recovery path fires in practice —
so operators can see whether raw user text is tripping the FTS5 parser
often enough to matter.

The screenshot above is that rescue working: `BR-2505` returns three ranked
hits spanning a French quality report, a French quarantine note, and an
English payment email — the full thread of the dispute, retrieved by its
lot number.

## Code-switched messages — two languages in one breath

Real SME chat does not switch languages politely at sentence
boundaries. A support thread mixes an English technical term into a
French sentence, or pivots mid-message from Japanese to English. The
roll-up harness ([`demos/multilingual-rollup/`](../../demos/multilingual-rollup/))
ingests a `support-emea-apac` channel of exactly these messages:

> *"Le client BonjourBio demande un rollback du dernier deploy — the
> checkout API returns 500 on SEPA payments since 14h."*
>
> *"@priya 確認お願いします: the Postgres read-replica lag is 8 seconds,
> だから reports are stale right now."*

Recall is **script-agnostic**: a token from each language lane retrieves
its message regardless of the surrounding script — `checkout` (English
in a French sentence) → 1 hit, `Postgres` (Latin in a Japanese sentence)
→ 1 hit, `hotfix` (English in a Spanish sentence) → 2 hits. The hybrid
FTS + vector index does not care which script a token sits inside, so a
mixed-language message is as retrievable as a monolingual one. (What a
code-switched thread does to *synthesis* — where the model has to pick a
language to write the recap in — is the more interesting story, and it
is in [post 3](03-synthesis-quality.md).)

## Scope isolation holds across languages

Recall never crosses a compartment boundary, regardless of language:

```
✓ Control:  '山本' retrievable in customer-sakura-personal — 2 hits
✓ Isolation:'山本' does NOT leak into customer-marubeni     — 0 hits (want 0)
```

```
✓ Control:  'Luiza' retrievable in customer-luiza-personal — 2 hits
✓ Isolation:'Luiza' does NOT leak into support-brazil-pt    — 0 hits (want 0)
```

**Across all five personas: 20/20 recall checks and every isolation
check passed.** Recall is the part of the system that is unambiguously
strong — across native scripts, code-switched messages, and
cross-language queries alike. Synthesis is the harder problem, because
there the model must not only *find* the knowledge but *rewrite* it in
the session's own language — and that is where a 1.7B model's limits, and
the deterministic pipeline that now contains them, show up. That is
[post 3](03-synthesis-quality.md).
