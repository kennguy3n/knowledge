# Élise Moreau — Directrice Administrative et Financière (CFO)
_Atelier Verdoyant · Lyon, France · languages: French, English_

_Run at 2026-06-12T01:18:49.676114+00:00 against `http://127.0.0.1:8080`._

> Élise is the CFO of Atelier Verdoyant, a 60-person sustainable-packaging manufacturer in Lyon selling to retail and food brands across France, Switzerland and the Benelux. Her institutional knowledge is scattered across Qonto (business banking), Pennylane (accounting), PayFit (payroll), GoCardless (SEPA direct debit), email, Slack and shared docs.

**Situation.** It is month-end close. A key supplier, CartoNord, delivered defective board stock and is disputing a credit note while a 90k EUR invoice is overdue; the statutory auditor has begun fieldwork; and the board pack is due Friday. Élise needs to answer 'what do we actually know about the CartoNord dispute and where does the close stand?' without opening six tools.

## The private compartments (scopes)

| Scope | Tier | What it holds |
| --- | --- | --- |
| `finance-month-end` | channel | Month-end close activity: accruals, reconciliations, Pennylane journals and the close checklist. |
| `supplier-cartonord` | channel | Everything about the CartoNord supplier: the defective board stock, the disputed credit note, and the overdue 90k EUR invoice. |
| `audit-2025` | channel | Statutory audit fieldwork: auditor requests (PBC list), confirmations and findings. |
| `treasury-cashflow` | channel | Cash position, Qonto balances, GoCardless SEPA collections and the 13-week forecast. |
| `board-reporting` | domain | Board pack and investor reporting: KPIs, runway and the quarterly narrative. |
| `customer-bonjourbio` | user | A single customer, BonjourBio, who has asked for a copy and deletion of their account data (RGPD). |

- **[PASS]** Gateway is healthy — HTTP 200

## Step 1 — Pull every source into one private store

Ingested **27/27** records across **6** scopes, **9** source types, languages: {'en': 5, 'fr': 22}.

- **[PASS]** All business records ingested — 27/27

## Step 2 — Recall in the local language (and across languages)

**Q [French] (supplier-cartonord):** CartoNord humidité  
_Find the quality root-cause across email + shared docs._
> Doc partagé « Litige CartoNord — chronologie » : photos des palettes, rapport du laboratoire d'humidité, et le cahier des charges signé fixant le seuil à 9 %. Conclusion interne : notre position sur l'avoir est solide.

- **[PASS]** Recall [French] 'CartoNord humidité' — 2 hits, matched ['humidité', '12,4', 'BR-2505', 'quarantaine']
**Q [French] (supplier-cartonord):** facture CartoNord avoir  
_Tie the overdue invoice to the disputed credit note._
> Pennylane : la facture fournisseur CartoNord FA-2025-0411 d'un montant de 90 000 EUR est échue depuis 15 jours. Paiement bloqué en attendant la résolution du litige sur l'avoir de 12 600 EUR.

- **[PASS]** Recall [French] 'facture CartoNord avoir' — 1 hits, matched ['90 000', 'FA-2025-0411', '12 600', 'avoir']
**Q [French] (treasury-cashflow):** GoCardless prélèvements  
_Surface failed SEPA collections from the banking connector._
> GoCardless : 38 prélèvements SEPA programmés pour le 5 du mois, total 96 400 EUR. Deux mandats clients ont échoué le mois dernier (compte clôturé, provision insuffisante) — relance en cours.

- **[PASS]** Recall [French] 'GoCardless prélèvements' — 1 hits, matched ['GoCardless', 'mandats', 'échoué']
**Q [English] (audit-2025):** auditor materiality  
_Cross-language recall: English query over mixed FR/EN audit records._
> Auditor preliminary materiality set at 85,000 EUR for the statutory accounts. The overdue CartoNord invoice (90,000 EUR) sits just above it, so its treatment will be a focus area.

- **[PASS]** Recall [English] 'auditor materiality' — 1 hits, matched ['materiality', '85,000']

## Step 3 — Scope isolation (no cross-compartment leakage)

- **[PASS]** Control: 'BonjourBio' retrievable in home scope `customer-bonjourbio` — HTTP 200, 3 hit(s)
- **[PASS]** Isolation: 'BonjourBio' does NOT leak into `supplier-cartonord` — HTTP 200, 0 hit(s) (want 0)

## Step 4 — Synthesise a briefing with the on-device model

**Business question:** What is the state of the CartoNord supplier dispute, and what should Élise decide?

The model is given **6** evidence record(s) from `supplier-cartonord` and asked for a JSON briefing.

- **[PASS]** Synthesis ran against the live model for `supplier-cartonord` — HTTP 202, recap chars=264
**Actual model output — recap written to channel memory:**

> CartoNord has failed to issue a credit note for the non-conforming BR-2505 lot, leading to a payment block of 90,000 EUR. Pennylane is requesting a 6,000 EUR commercial gesture in lieu of the 12,600 EUR invoice. The decision will be made by the finance department.

_Business-term coverage: matched 4/10 expected terms (['cartonord', 'credit', '90', 'invoice'])._

**Actual model output — full structured bundle (replaying the production `SynthSummary` prompt + grammar under the deterministic sampling preset):**

_Sampling: fixed seed=0, temperature=0.0 (greedy), top_k=1. First-attempt budget n_predict=656 (adaptive to 6 rows)._

_Verify-and-retry: first attempt passed the quality gate ({'recap_chars': 292, 'meta_commentary': False, 'too_short': False}); no retry needed._

```json
{
  "recap": "CartoNord has rejected the invoice FA-2025-0411 of 90,000 EUR for BR-2505 non-conforming lot. The payment is blocked until a credit note of 12,600 EUR is issued. Pennylane has reported that 30% of the pallets had humidity levels exceeding the maximum allowed (12,4 %) and 18 were quarantined.",
  "decisions": [
    "Adopt Postgres for the billing store"
  ],
  "open_questions": [
    "What is the exact amount of the credit note?"
  ],
  "active_tasks": [
    "Migrate staging data by Friday",
    "Quarantine 18 pallets",
    "Issue a credit note of 12,600 EUR"
  ]
}
```

- **[PASS]** Synthesis is byte-reproducible across runs (fixed seed) — 2 runs, identical=True, 543 chars

## Step 5 — Cryptographic right to be forgotten

> BonjourBio exercised their RGPD right to erasure; destroying the scope's DEK makes the data unrecoverable.

Before erase: **3** record(s); after erase: **0** record(s).

- **[PASS]** Deletion request accepted — HTTP 204
- **[PASS]** Data is unrecoverable after key destruction — HTTP 200→200, 3→0 records

## Result — 12/12 checks passed
