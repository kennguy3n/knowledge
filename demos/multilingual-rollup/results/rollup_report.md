# Multilingual & cross-scope roll-up — evidence run

_Generated 2026-06-12T02:10:32.725769+00:00 against `http://localhost:8080` (gateway) with the on-device Bonsai-1.7B Q2_0 model._

## Determinism (PR #223)

- Fired the identical synthesis prompt **2×** at the on-device model.
- Byte-identical output: **True** (351 chars).
- Sampling preset: `{"seed": 0, "temperature": 0.0, "top_k": 1, "top_p": 0.9, "min_p": 0.05, "repeat_penalty": 1.1}`.

## 1. Multilingual synthesis matrix

Same situation, expressed natively per language; recap must return in-language.

_`In-language` compares the recap's alphabetic characters by script (tolerating embedded Latin product names like `MySQL`/`SKU-6310`); `usable` is the pipeline quality gate and does **not** check language._

| Language | Script | Recap chars | Usable | In-language | Recap |
|----------|--------|-------------|--------|-------------|-------|
| English | Latin | 133 | yes | yes | A discrepancy in inventory for SKU-8842 was detected, indicating a pot… |
| French | Latin | 127 | yes | yes | Le laboratoire a confirmé une humidité de 12,4% sur le lot BR-2505, au… |
| German | Latin | 138 | yes | yes | Die Produktionslinie 3 wird auf das neue Etikettiersystem umgestellt; … |
| Spanish | Latin | 120 | yes | yes | El almacén de Bogotá reporta un faltante de 80 unidades del SKU-3310 f… |
| Japanese | CJK | 79 | yes | yes | AX-7サーボの過熱はハードウェア故障ではなく、センサーのファームウェアのオフセットが原因である。暫定対策は2503ロットに80%のデューテ… |
| Chinese | CJK | 287 | yes | **no** | PostgreSQL migration from MySQL to be scheduled for next iteration, wi… |
| Vietnamese | Latin | 86 | yes | yes | Quyết định chuyển hệ thống thanh toán từ MoMo sang VNPay trong quý tới… |
| Thai | Thai | 141 | yes | yes | การตัดสินใจย้ายระบบชำระเงินจาก 2C2P เป็นไปยัง Omise ในไตรมาสหน้า โดยผู… |
| Indonesian | Latin | 127 | yes | yes | Gudang Surabaya melaporkan selisih stok: SKU-6310 kurang 110 unit diba… |
| Arabic | Arabic | 126 | yes | yes | القرار: ترحيل قاعدة بيانات الفوتر من MySQL إلى Postgres في الدورة القا… |

## 2. Code-switched (mixed-language) messages

- All 4 mixed-language messages ingested: **True**.
- Recall `checkout` (english-lane token): **1** hit(s).
- Recall `Postgres` (japanese-lane token): **1** hit(s).
- Recall `hotfix` (spanish-lane token): **2** hit(s).
- Synthesised recap: _The Postgres read-replica lag is 8 seconds, so reports are stale right now._ (usable: True).

## 3. Cross-message roll-up (one channel)

- Ingested **6** messages (517 chars), three restating the same decision.
- Consolidated recap (**135** chars, ~3.8× compression): _The finance team signed off on the Postgres migration budget today and decided to migrate the billing database to Postgres next sprint._
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

_`in-lang` = recap written in the session's own script; `usable` = passed the quality gate (non-placeholder, non-meta, length OK)._

| Language | Script | 1.7B usable | 1.7B in-lang | 4B usable | 4B in-lang |
|----------|--------|-------------|-------------|-----------|------------|
| English | Latin | True | yes | True | yes |
| French | Latin | True | yes | True | yes |
| German | Latin | True | yes | True | yes |
| Spanish | Latin | True | yes | True | yes |
| Japanese | CJK | False | **no** | True | yes |
| Chinese | CJK | False | **no** | True | yes |
| Vietnamese | Latin | True | yes | True | yes |
| Thai | Thai | True | yes | True | yes |
| Indonesian | Latin | True | yes | True | yes |
| Arabic | Arabic | True | **no** | True | yes |
