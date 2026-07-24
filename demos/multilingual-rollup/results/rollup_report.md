# Multilingual & cross-scope roll-up — evidence run

_Generated 2026-07-23T10:42:47.940142+00:00 against `http://localhost:8080` (gateway) with the on-device Qwen3.5-2B Q4_K_M model._

## Determinism (PR #223)

- Fired the identical synthesis prompt **2×** at the on-device model.
- Byte-identical output: **True** (472 chars).
- Sampling preset: `{"seed": 0, "temperature": 0.0, "top_k": 1, "top_p": 0.9, "min_p": 0.05, "repeat_penalty": 1.1}`.

## 1. Multilingual synthesis matrix

Same situation, expressed natively per language; recap must return in-language.

_`In-language` compares the recap's alphabetic characters by script (tolerating embedded Latin product names like `MySQL`/`SKU-6310`); `usable` is the pipeline quality gate and does **not** check language._

| Language | Script | Recap chars | Usable | In-language | Recap |
|----------|--------|-------------|--------|-------------|-------|
| English | Latin | 331 | yes | yes | The Shanghai warehouse reports an inventory discrepancy: SKU-8842 is s… |
| French | Latin | 136 | yes | yes | Le rapport du laboratoire confirme une humidité de 12,4% sur le lot BR… |
| German | Latin | 138 | yes | yes | Die Produktionslinie 3 wird auf das neue Etikettiersystem umgestellt; … |
| Spanish | Latin | 225 | yes | yes | Pregunta abierta: ¿necesitamos un entorno de pruebas con Adyen antes d… |
| Japanese | CJK | 71 | yes | yes | 恒久対策: Keyenceのファームウェアv2.4.1を来週OTAで配信する。主要顧客Marubeniの40台を優先し、サービス料を免除する… |
| Chinese | CJK | 108 | yes | yes | 将计费数据库从 MySQL 迁移到 Postgres，定于下个迭代执行，负责人为 Priya，主要风险是切换期间的停机。上海仓库报告库存差异… |
| Vietnamese | Latin | 388 | yes | yes | Kho Hải Phòng báo cáo thiếu hụt tồn kho: SKU-7720 ít hơn 150 đơn vị so… |
| Thai | Thai | 136 | yes | yes | การตัดสินใจ: ย้ายระบบชำระเงินจาก 2C2P ไปยัง Omise ในไตรมาสหน้า ผู้รับผ… |
| Indonesian | Latin | 137 | yes | yes | Pertanyaan terbuka mengenai apakah kita memerlukan replika baca sebelu… |
| Arabic | Arabic | 101 | yes | yes | الفوترة ترحيل قاعدة بيانات من MySQL إلى Postgres، المسؤولة بريا، والخط… |
| Malay | Latin | 115 | yes | yes | Gudang Johor melaporkan perbezaan stok: SKU-4820 kurang 100 unit berba… |
| Tagalog | Latin | 498 | yes | yes | Iniulat ng bodega ng Cebu ang pagkakaiba sa imbentaryo, pagkakaiba sa … |

## 2. Code-switched (mixed-language) messages

- All 4 mixed-language messages ingested: **True**.
- Recall `checkout` (english-lane token): **1** hit(s).
- Recall `Postgres` (japanese-lane token): **1** hit(s).
- Recall `hotfix` (spanish-lane token): **2** hit(s).
- Synthesised recap: _確認お願いします, postgres, replica, seconds, reports, bonjourbio, demande, rollback, dernier, checkout, returns, payments, confirmado, cliente, tarjeta, desplegar, deployed, latency, 4471_ (usable: True).

## 3. Cross-message roll-up (one channel)

- Ingested **6** messages (517 chars), three restating the same decision.
- Consolidated recap (**90** chars, ~5.7× compression): _The decision was agreed that we will migrate the billing database to Postgres next sprint._
- Memory state: **Reinforced** (retention 1.0); reinforced: **True**.

## 4. Cross-channel roll-up (isolated scopes)

| Channel | Writes OK | Concept nodes | Shared terms surfaced |
|---------|-----------|---------------|------------------------|
| eng-backend | True | 3 | billing, cutover, migration, postgres, priya, read replica |
| ops-oncall | True | 3 | billing, cutover, migration, postgres, priya, read replica |
| finance-controls | True | 2 | billing, migration, postgres, priya |

- Scope isolation (distinct node ids across channels): **True**.
- Concepts that independently surfaced in **every** channel: **billing, migration, postgres, priya**.
