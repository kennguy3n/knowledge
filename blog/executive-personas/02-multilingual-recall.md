# Multilingual Recall, in Practice

> **TL;DR:** Across five personas we ran 22 recall questions in French,
> Japanese, Portuguese, Spanish, Hindi and English — including
> *cross-language* recall (ask in English over Japanese records). Every
> one returned the right evidence. This post shows the real queries and
> hits, and the one-line-class of bug that used to make business
> identifiers like `BR-2505` un-searchable.

## Recall is hybrid, and language-aware

A query runs through `POST /api/v1/query`, which fuses three signals:
full-text (FTS5), recency, and vector similarity. The UI surfaces all
three so you can see *why* something ranked where it did:

![Searching the literal business identifier BR-2505 returns three ranked hits across French and English evidence, each with fts/recency/vector breakdown.](assets/03-search-br2505.png)

The interesting part is that the corpus is genuinely multilingual. The
extraction engine ([post 2 of the foundation series](../02-multilingual-extraction-engine.md))
handles 22 languages; here are real hits, verbatim, in five of them.

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

## The bug that made `BR-2505` un-searchable

Business data is full of identifiers with punctuation: `BR-2505`,
`FA-2025-0411`, `HD-320`, decimals like `12,4 %`. These broke recall.

SQLite's FTS5 `MATCH` grammar treats a hyphen as a column-filter
operator, so a raw query of `BR-2505` parsed as *"column BR minus token
2505"* and the substrate returned **HTTP 400 `InvalidQuery`** — for a
string a user could reasonably type. The wrong fix is to strip
punctuation (you'd lose the ability to search the identifier at all).
The right fix preserves the full FTS5 grammar for power users while
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
unit tests, and the `escape_fts_query_total` counter on the health
endpoint records how often the fallback fires in practice.

The screenshot above is that fix working: `BR-2505` returns three ranked
hits spanning a French quality report, a French quarantine note, and an
English payment email — the full thread of the dispute, retrieved by its
lot number.

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
strong. Synthesis is more interesting — and that is [post 3](03-synthesis-quality.md).
