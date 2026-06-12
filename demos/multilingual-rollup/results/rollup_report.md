# Multilingual & cross-scope roll-up — evidence run

_Generated 2026-06-12T00:49:26.616540+00:00 against `http://localhost:8080` (gateway) with the on-device Bonsai-1.7B Q2_0 model._

## Determinism (PR #223)

- Fired the identical synthesis prompt **2×** at the on-device model.
- Byte-identical output: **True** (351 chars).
- Sampling preset: `{"seed": 0, "temperature": 0.0, "top_k": 1, "top_p": 0.9, "min_p": 0.05, "repeat_penalty": 1.1}`.

## 1. Multilingual synthesis matrix

Same situation, expressed natively per language; recap must return in-language.

| Language | Script | Recap chars | Usable | Recap |
|----------|--------|-------------|--------|-------|
| English | Latin | 144 | yes | The Shanghai warehouse reports an inventory discrepancy: SKU-8842 is s… |
| French | Latin | 163 | yes | Le litige avec le fournisseur CartoNord sur l'avoir de 12 600 EUR est … |
| German | Latin | 138 | yes | Die Produktionslinie 3 wird auf das neue Etikettiersystem umgestellt; … |
| Spanish | Latin | 135 | yes | Migramos la pasarela de pagos de Stripe a Adyen el próximo trimestre; … |
| Japanese | CJK | 229 | yes | Keyence's firmware v2.4.1 will be released via OTA in the coming weeks… |
| Chinese | CJK | 50 | yes | 上海仓库报告库存差异: SKU-8842 实际数量比系统记录少 120 件，正在调查是否为扫描错误。 |

## 2. Code-switched (mixed-language) messages

- All 4 mixed-language messages ingested: **True**.
- Recall `checkout` (english-lane token): **1** hit(s).
- Recall `Postgres` (japanese-lane token): **1** hit(s).
- Recall `hotfix` (spanish-lane token): **2** hit(s).
- Synthesised recap: _Hotfix deployed for SEPA payments with 500 error, and bug fixed in 3DS payment validation. The rollback requested by client BonjourBio is pending as the Postgres read-replica lag is 8 seconds, causing stale reports. Follow-up: add regression test for 3DS._ (usable: True).

## 3. Cross-message roll-up (one channel)

- Ingested **6** messages (517 chars), three restating the same decision.
- Consolidated recap (**379** chars, ~1.4× compression): _Decision to migrate billing database to Postgres is locked in for next sprint, with Priya leading the cutover. Task of drafting the Postgres migration runbook by Wednesday is assigned to Priya. Quick note from standup — the billing DB move to Postgres is locked in for next sprint, Priya owning the cutover. FYI the finance team signed off on the Postgres migration budget today._
- Memory state: **Reinforced** (retention 1.0); reinforced: **True**.

## 4. Cross-channel roll-up (isolated scopes)

| Channel | Writes OK | Concept nodes | Shared terms surfaced |
|---------|-----------|---------------|------------------------|
| eng-backend | True | 3 | billing, cutover, migration, postgres, priya, read replica |
| ops-oncall | True | 3 | billing, cutover, migration, postgres, priya, read replica |
| finance-controls | True | 2 | billing, migration, postgres, priya |

- Scope isolation (distinct node ids across channels): **True**.
- Concepts that independently surfaced in **every** channel: **billing, migration, postgres, priya**.

## 5. Synthesis quality — Bonsai 1.7B vs 4B (opt-in upgrade)

Same prompt + grammar + deterministic sampling; only the model weights differ.

| Language | Script | 1.7B usable | 1.7B recap chars | 4B usable | 4B recap chars |
|----------|--------|-------------|------------------|-----------|----------------|
| English | Latin | True | 340 | True | 163 |
| French | Latin | True | 163 | True | 128 |
| German | Latin | True | 239 | True | 110 |
| Spanish | Latin | True | 344 | True | 166 |
| Japanese | CJK | False | 1 | True | 63 |
| Chinese | CJK | False | 1 | True | 61 |
