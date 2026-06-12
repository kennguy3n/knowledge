# Sofía Herrera — Founder & CEO
_Selva Botánica · Ciudad de México, Mexico / Brazil · languages: Spanish, Portuguese, English_

_Run at 2026-06-12T01:41:05.175643+00:00 against `http://127.0.0.1:8080`._

> Sofía founded Selva Botánica, a 30-person natural-cosmetics D2C brand selling across Mexico, Colombia and Brazil through MercadoLibre, Rappi, its own Shopify store, Nubank and PagSeguro. Knowledge lives in WhatsApp, Instagram DMs, email, Slack, a Notion-like tracker, marketplace APIs and payment dashboards — across Spanish and Portuguese.

**Situation.** Selva Botánica is launching on MercadoLibre Brazil while a creator campaign with a São Paulo influencer goes viral, straining fulfilment. At the same time a spike in PagSeguro chargebacks needs triage. Sofía wants one place to answer 'how is the Brazil launch going and what's driving the chargebacks?' across two languages.

## The private compartments (scopes)

| Scope | Tier | What it holds |
| --- | --- | --- |
| `marketplace-brazil-launch` | channel | The MercadoLibre + Shopify Brazil launch: catalog, logistics, pricing in BRL and first-week sales. |
| `creator-campaign-saopaulo` | channel | The influencer partnership in São Paulo: deliverables, the viral spike and fulfilment strain. |
| `payments-chargebacks` | channel | PagSeguro and Nubank chargebacks and fraud signals, and the dispute-response playbook. |
| `support-mexico-es` | channel | Spanish-language customer support for Mexico and Colombia. |
| `support-brazil-pt` | channel | Portuguese-language customer support for Brazil. |
| `customer-luiza-personal` | user | A single Brazilian customer, Luiza, who invoked LGPD deletion of her personal data. |

- **[PASS]** Gateway is healthy — HTTP 200

## Step 1 — Pull every source into one private store

Ingested **22/22** records across **6** scopes, **4** source types, languages: {'en': 2, 'es': 7, 'pt': 13}.

- **[PASS]** All business records ingested — 22/22

## Step 2 — Recall in the local language (and across languages)

**Q [Portuguese] (creator-campaign-saopaulo):** Mariana açaí  
_Connect the viral campaign to the stock-out._
> Slack #marketing: 'La campaña de Mariana funcionó demasiado bien — nos quedamos sin sérum de açaí. Necesitamos reposición urgente y avisar a los que compraron en preventa.'

- **[PASS]** Recall [Portuguese] 'Mariana açaí' — 1 hits, matched ['mariana', 'açaí']
**Q [Portuguese] (payments-chargebacks):** chargebacks fraude  
_Find the chargeback spike and its suspected cause._
> Slack #pagamentos: 'O pico de chargebacks parece fraude de teste de cartão aproveitando o tráfego viral. Sugiro ativar 3-D Secure e revisão manual acima de R$ 300.'

- **[PASS]** Recall [Portuguese] 'chargebacks fraude' — 1 hits, matched ['chargeback', 'fraude', '3-D']
**Q [Spanish] (payments-chargebacks):** Nubank fraude  
_Cross-language: Spanish query surfaces ES + PT payment records._
> Nubank: 6 contracargos en México marcados como posible fraude con tarjeta. Patrón — mismo BIN, montos altos, direcciones de envío que no coinciden con la facturación.

- **[PASS]** Recall [Spanish] 'Nubank fraude' — 1 hits, matched ['Nubank', 'fraude', 'BIN', 'contracargo']
**Q [English] (marketplace-brazil-launch):** MercadoLibre açaí  
_Cross-language: English query over PT/ES launch records._
> MercadoLibre Brasil: catálogo publicado com 24 SKUs. Primeira semana — 312 pedidos, ticket médio R$ 89. O sérum facial de açaí é o campeão de vendas, 38% do total.

- **[PASS]** Recall [English] 'MercadoLibre açaí' — 1 hits, matched ['312', 'açaí', 'R$', 'MercadoLibre']

## Step 3 — Scope isolation (no cross-compartment leakage)

- **[PASS]** Control: 'Luiza' retrievable in home scope `customer-luiza-personal` — HTTP 200, 2 hit(s)
- **[PASS]** Isolation: 'Luiza' does NOT leak into `support-brazil-pt` — HTTP 200, 0 hit(s) (want 0)

## Step 4 — Synthesise a briefing with the on-device model

**Business question:** What is driving the chargeback spike, and what is the recommended response?

The model is given **3** evidence record(s) from `payments-chargebacks` and asked for a JSON briefing.

- **[PASS]** Synthesis ran against the live model for `payments-chargebacks` — HTTP 202, recap chars=371
**Actual model output — recap written to channel memory:**

> Aumento de chargebacks na semana do pico de 27 disputas, com a taxa média histórica de 0,6%. Maioria classificada como 'não reconhece a compra', concentrada em pedidos com o cupom MARIANA10. Nubank identificou 6 contracargos en México marcados como possíveis fraude con tarjeta, patrón sendo mesmo BIN, montos altos e direções de envio que não coincidem com a facturacao.

_Business-term coverage: matched 7/9 expected terms (['chargeback', 'contracargo', 'fraude', 'fraud', 'MARIANA10', 'Nubank', 'disputa'])._

**Actual model output — full structured bundle (replaying the production `SynthSummary` prompt + grammar under the deterministic sampling preset):**

_Sampling: fixed seed=0, temperature=0.0 (greedy), top_k=1. First-attempt budget n_predict=584 (adaptive to 3 rows)._

_Verify-and-retry: first attempt passed the quality gate ({'recap_chars': 172, 'meta_commentary': False, 'too_short': False}); no retry needed._

```json
{
  "recap": "Resposta de disputa enviada ao PagSeguro com comprovantes de entrega e IP. Taxa de recuperação de chargebacks subiu de 22% para 41% após anexar o rastreamento dos Correios.",
  "decisions": [
    "Adopt Postgres for the billing store"
  ],
  "open_questions": [
    "Sugiro ativar 3-D Secure e revisão manual acima de R$ 300."
  ],
  "active_tasks": [
    "Migrate staging data by Friday"
  ]
}
```

- **[PASS]** Synthesis is byte-reproducible across runs (fixed seed) — 2 runs, identical=True, 375 chars

## Step 5 — Cryptographic right to be forgotten

> Luiza invoked her LGPD right to deletion; destroying the scope DEK erases her data irreversibly.

Before erase: **2** record(s); after erase: **0** record(s).

- **[PASS]** Deletion request accepted — HTTP 204
- **[PASS]** Data is unrecoverable after key destruction — HTTP 200→200, 2→0 records

## Result — 12/12 checks passed
