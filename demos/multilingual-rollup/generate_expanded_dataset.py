#!/usr/bin/env python3
"""
Generate an expanded multilingual benchmark dataset for credible real-world
synthesis evaluation.

3 scenarios × 15 languages × 6 messages × 8-12 expected terms = 45 sessions.
"""

import json
from pathlib import Path

HERE = Path(__file__).resolve().parent
DATASET_OUT = HERE / "dataset" / "expanded-benchmark.json"
FIXTURE_OUT = HERE / "fixtures" / "expanded-expected-terms.json"

# Scenario definitions with English source messages.
# Each message is a template with {placeholders} filled per-language.
SCENARIOS = [
    {
        "id": "db-migration",
        "domain": "infrastructure",
        "title": "Database migration from MySQL to Postgres",
        "messages": [
            "Decision: migrate the billing database from MySQL to Postgres next sprint. Owner is {owner}. Risk: service downtime during cutover.",
            "The {warehouse} warehouse reports an inventory discrepancy: {sku1} is short by {qty1} units versus the system of record; investigating a scan error.",
            "Open question: do we need a read replica before the Postgres cutover to avoid report downtime?",
            "Update: the Postgres cluster is provisioned on {host} with 16 vCPU and 64 GB RAM. pg_loader dry run completed in 42 minutes.",
            "Risk assessment: the cutover window is estimated at 4 hours. {owner} will notify {customer} 48 hours in advance.",
            "Decision: schedule the cutover for Saturday {date} at 02:00 UTC. Rollback plan: repoint DNS to MySQL master if health checks fail within 30 minutes.",
        ],
        "expected_terms": ["MySQL", "Postgres", "{owner}", "{sku1}", "{warehouse}", "replica", "{host}", "cutover"],
    },
    {
        "id": "payment-incident",
        "domain": "incident",
        "title": "Payment gateway outage and incident response",
        "messages": [
            "Alert: {gateway_old} payment gateway is returning 503 errors for 12% of checkout attempts since {time}. {owner} is leading the incident response.",
            "Rollback attempted on the {gateway_old} API client from v3.2 to v3.1 — error rate dropped from 12% to 3% but did not fully resolve.",
            "Decision: failover checkout traffic to {gateway_new} while {gateway_old} investigates. {owner} authorized the switch at {time2}.",
            "Customer impact: approximately {impact_count} transactions failed during the 47-minute outage. Refunds being processed automatically.",
            "Postmortem: root cause was a certificate chain error in {gateway_old} v3.2. The intermediate CA expired but the client did not validate the chain.",
            "Action item: {owner} to implement synthetic transaction monitoring for {gateway_old} and {gateway_new} by {deadline}. Severity: P1.",
        ],
        "expected_terms": ["{gateway_old}", "{gateway_new}", "{owner}", "503", "failover", "certificate", "refund", "P1"],
    },
    {
        "id": "vendor-dispute",
        "domain": "procurement",
        "title": "Supplier quality dispute and credit note negotiation",
        "messages": [
            "The supplier {supplier} delivered lot {lot_id} on {date}. Quality control found {defect}: measured {metric} at {measured_value} versus the spec limit of {spec_limit}.",
            "We issued a credit note request of {credit_amount} to {supplier} for the non-conforming {lot_id}. {supplier} disputes the claim and says goods were conforming at dispatch.",
            "The invoice {invoice_id} for {invoice_amount} is overdue by {overdue_days} days. Payment is blocked pending resolution of the credit note dispute.",
            "Production confirms: the {qty_affected} affected units in quarantine are unusable. We purchased from an alternate supplier at a surcharge of {surcharge}.",
            "{supplier} proposed a commercial gesture of {gesture_amount} instead of the full {credit_amount}. Decision needed from {owner}.",
            "Decision: {owner} accepts the reduced credit of {gesture_amount} and releases payment of invoice {invoice_id}, deducting the credit. Condition: {supplier} provides a QA report for the next shipment.",
        ],
        "expected_terms": ["{supplier}", "{lot_id}", "credit", "{invoice_id}", "quarantine", "{owner}", "{credit_amount}", "{gesture_amount}"],
    },
]

# Per-language parameters and full translations.
# Each language has: params (names, places, amounts) + translated message templates.
LANGS = {}

LANGS["English"] = {
    "script": "Latin",
    "params": {
        "db-migration": {"owner":"Priya","warehouse":"Shanghai","sku1":"SKU-8842","qty1":"120","host":"db-prod-01","customer":"Acme Corp","date":"July 20"},
        "payment-incident": {"owner":"Marcus","gateway_old":"Stripe","gateway_new":"Adyen","time":"14:30 UTC","time2":"14:47 UTC","impact_count":"840","deadline":"Friday"},
        "vendor-dispute": {"supplier":"CartoNord","lot_id":"BR-2505","date":"May 6","defect":"excess humidity","metric":"humidity","measured_value":"12.4%","spec_limit":"9%","credit_amount":"12,600 EUR","invoice_id":"FA-2025-0411","invoice_amount":"90,000 EUR","overdue_days":"15","qty_affected":"18 pallets","surcharge":"3,200 EUR","gesture_amount":"6,000 EUR","owner":"Élise"},
    },
}

LANGS["French"] = {
    "script": "Latin",
    "params": {
        "db-migration": {"owner":"Priya","warehouse":"Lyon","sku1":"SKU-8842","qty1":"120","host":"db-prod-01","customer":"BonjourBio","date":"20 juillet"},
        "payment-incident": {"owner":"Marc","gateway_old":"Stripe","gateway_new":"Adyen","time":"14h30 UTC","time2":"14h47 UTC","impact_count":"840","deadline":"vendredi"},
        "vendor-dispute": {"supplier":"CartoNord","lot_id":"BR-2505","date":"6 mai","defect":"un excès d'humidité","metric":"le taux d'humidité","measured_value":"12,4 %","spec_limit":"9 %","credit_amount":"12 600 EUR","invoice_id":"FA-2025-0411","invoice_amount":"90 000 EUR","overdue_days":"15","qty_affected":"18 palettes","surcharge":"3 200 EUR","gesture_amount":"6 000 EUR","owner":"Élise"},
    },
    "translations": {
        "db-migration": [
            "Décision : migrer la base de données de facturation de MySQL vers Postgres au prochain sprint. Responsable : {owner}. Risque : interruption de service pendant le basculement.",
            "L'entrepôt de {warehouse} signale un écart d'inventaire : {sku1} est en manque de {qty1} unités par rapport au système ; investigation en cours pour une erreur de scan.",
            "Question ouverte : faut-il une réplique en lecture avant le basculement Postgres pour éviter l'arrêt des rapports ?",
            "Mise à jour : le cluster Postgres est provisionné sur {host} avec 16 vCPU et 64 Go de RAM. Le test pg_loader a été terminé en 42 minutes.",
            "Évaluation des risques : la fenêtre de basculement est estimée à 4 heures. {owner} notifiera {customer} 48 heures à l'avance.",
            "Décision : planifier le basculement pour le samedi {date} à 02h00 UTC. Plan de retour arrière : repointer le DNS vers le master MySQL si les health checks échouent dans les 30 minutes.",
        ],
        "payment-incident": [
            "Alerte : la passerelle de paiement {gateway_old} renvoie des erreurs 503 pour 12 % des tentatives de paiement depuis {time}. {owner} dirige la réponse à l'incident.",
            "Retour arrière tenté sur le client API {gateway_old} de v3.2 à v3.1 — le taux d'erreur est passé de 12 % à 3 % mais n'est pas entièrement résolu.",
            "Décision : basculer le trafic de paiement vers {gateway_new} pendant que {gateway_old} enquête. {owner} a autorisé le changement à {time2}.",
            "Impact client : environ {impact_count} transactions ont échoué pendant la panne de 47 minutes. Remboursements traités automatiquement.",
            "Post-mortem : la cause racine était une erreur de chaîne de certificat dans {gateway_old} v3.2. Le certificat intermédiaire CA a expiré mais le client ne validait pas la chaîne.",
            "Action : {owner} doit implémenter une supervision par transactions synthétiques pour {gateway_old} et {gateway_new} d'ici {deadline}. Sévérité : P1.",
        ],
        "vendor-dispute": [
            "Le fournisseur {supplier} a livré le lot {lot_id} le {date}. Le contrôle qualité a constaté {defect} : {metric} mesuré à {measured_value} contre la limite de {spec_limit}.",
            "Nous avons émis une demande d'avoir de {credit_amount} auprès de {supplier} pour le lot {lot_id} non conforme. {supplier} conteste l'avoir et maintient que les marchandises étaient conformes au départ.",
            "La facture {invoice_id} d'un montant de {invoice_amount} est échue depuis {overdue_days} jours. Paiement bloqué en attendant la résolution du litige sur l'avoir de {credit_amount}.",
            "La production confirme : les {qty_affected} en quarantaine ne sont pas utilisables. Nous avons dû racheter en urgence chez un autre fournisseur, surcoût de {surcharge}.",
            "{supplier} propose un geste commercial de {gesture_amount} au lieu des {credit_amount} demandés. Décision à prendre par {owner}.",
            "Décision : {owner} accepte l'avoir réduit de {gesture_amount} et débloque le paiement de la facture {invoice_id}, déduction faite de l'avoir. Condition : {supplier} fournit un rapport qualité pour la prochaine livraison.",
        ],
    },
}

LANGS["German"] = {
    "script": "Latin",
    "params": {
        "db-migration": {"owner":"Lukas","warehouse":"München","sku1":"SKU-8842","qty1":"120","host":"db-prod-01","customer":"Bayerische Folien","date":"20. Juli"},
        "payment-incident": {"owner":"Anna","gateway_old":"Stripe","gateway_new":"Adyen","time":"14:30 UTC","time2":"14:47 UTC","impact_count":"840","deadline":"Freitag"},
        "vendor-dispute": {"supplier":"Bayerische Folien","lot_id":"FK-220","date":"6. Mai","defect":"eine Verzögerung","metric":"Lieferzeit","measured_value":"zwei Wochen","spec_limit":"eine Woche","credit_amount":"8.200 EUR","invoice_id":"RE-2025-0312","invoice_amount":"45.000 EUR","overdue_days":"12","qty_affected":"500 Rollen","surcharge":"1.800 EUR","gesture_amount":"4.000 EUR","owner":"Lena"},
    },
    "translations": {
        "db-migration": [
            "Entscheidung: Die Abrechnungsdatenbank wird im nächsten Sprint von MySQL auf Postgres migriert. Verantwortlich: {owner}. Risiko: Dienstausfall während des Umstiegs.",
            "Das Lager {warehouse} meldet eine Inventurdifferenz: {sku1} fehlen {qty1} Einheiten gegenüber dem System; eine Scan-Fehler wird untersucht.",
            "Offene Frage: Benötigen wir eine Read-Replica vor dem Postgres-Umstieg, um Report-Ausfälle zu vermeiden?",
            "Update: Der Postgres-Cluster ist auf {host} mit 16 vCPU und 64 GB RAM bereitgestellt. Der pg_loader-Testlauf wurde in 42 Minuten abgeschlossen.",
            "Risikobewertung: Das Umstellungsfenster wird auf 4 Stunden geschätzt. {owner} wird {customer} 48 Stunden im Voraus benachrichtigen.",
            "Entscheidung: Umstellung geplant für Samstag, {date}, um 02:00 UTC. Rollback-Plan: DNS auf MySQL-Master umleiten, wenn Health-Checks innerhalb von 30 Minuten fehlschlagen.",
        ],
        "payment-incident": [
            "Alarm: Das Zahlungs-Gateway {gateway_old} gibt seit {time} für 12 % der Checkout-Versuche 503-Fehler zurück. {owner} leitet die Incident-Response.",
            "Rollback des {gateway_old}-API-Clients von v3.2 auf v3.1 versucht — Fehlerrate sank von 12 % auf 3 %, aber nicht vollständig behoben.",
            "Entscheidung: Checkout-Traffic auf {gateway_new} umgeleitet, während {gateway_old} untersucht. {owner} hat die Umstellung um {time2} genehmigt.",
            "Kundenauswirkung: Etwa {impact_count} Transaktionen sind während des 47-minütigen Ausfalls fehlgeschlagen. Rückerstattungen werden automatisch verarbeitet.",
            "Postmortem: Ursache war ein Zertifikatskettenfehler in {gateway_old} v3.2. Das Intermediate-CA-Zertifikat war abgelaufen, aber der Client validierte die Kette nicht.",
            "Maßnahme: {owner} implementiert synthetische Transaktionsüberwachung für {gateway_old} und {gateway_new} bis {deadline}. Priorität: P1.",
        ],
        "vendor-dispute": [
            "Der Lieferant {supplier} hat die Charge {lot_id} am {date} geliefert. Die Qualitätskontrolle fand {defect}: {metric} bei {measured_value} gegenüber der Spezifikation von {spec_limit}.",
            "Wir haben eine Gutschrift über {credit_amount} bei {supplier} für die nicht konforme Charge {lot_id} beantragt. {supplier} bestreitet den Anspruch und sagt, die Ware war beim Versand konform.",
            "Die Rechnung {invoice_id} über {invoice_amount} ist seit {overdue_days} Tagen überfällig. Zahlung blockiert bis zur Klärung des Gutschriftsstreits.",
            "Produktion bestätigt: Die {qty_affected} in Quarantäne sind unbrauchbar. Wir mussten bei einem anderen Lieferanten mit einem Aufpreis von {surcharge} einkaufen.",
            "{supplier} schlägt eine kaufmännische Geste von {gesture_amount} statt der vollen {credit_amount} vor. Entscheidung von {owner} erforderlich.",
            "Entscheidung: {owner} akzeptiert die reduzierte Gutschrift von {gesture_amount} und gibt die Zahlung der Rechnung {invoice_id} unter Abzug der Gutschrift frei. Bedingung: {supplier} liefert einen QA-Bericht für die nächste Lieferung.",
        ],
    },
}

LANGS["Spanish"] = {
    "script": "Latin",
    "params": {
        "db-migration": {"owner":"Sofía","warehouse":"Bogotá","sku1":"SKU-3310","qty1":"80","host":"db-prod-01","customer":"TiendaAndina","date":"20 de julio"},
        "payment-incident": {"owner":"Diego","gateway_old":"Stripe","gateway_new":"Adyen","time":"14:30 UTC","time2":"14:47 UTC","impact_count":"840","deadline":"viernes"},
        "vendor-dispute": {"supplier":"Empaques Andina","lot_id":"LO-4502","date":"6 de mayo","defect":"un defecto de sellado","metric":"sellado","measured_value":"12% defectuoso","spec_limit":"3%","credit_amount":"8.500 USD","invoice_id":"FA-2025-0308","invoice_amount":"42.000 USD","overdue_days":"18","qty_affected":"2.000 cajas","surcharge":"1.900 USD","gesture_amount":"4.200 USD","owner":"Sofía"},
    },
    "translations": {
        "db-migration": [
            "Decisión: migramos la base de datos de facturación de MySQL a Postgres el próximo sprint. Responsable: {owner}. Riesgo: interrupción del servicio durante el corte.",
            "El almacén de {warehouse} reporta un faltante de inventario: {sku1} tiene {qty1} unidades menos que el sistema; se investiga un error de conteo.",
            "Pregunta abierta: ¿necesitamos una réplica de lectura antes del corte de Postgres para evitar el tiempo de inactividad de los informes?",
            "Actualización: el clúster de Postgres está aprovisionado en {host} con 16 vCPU y 64 GB de RAM. La prueba de pg_loader se completó en 42 minutos.",
            "Evaluación de riesgos: la ventana de corte se estima en 4 horas. {owner} notificará a {customer} con 48 horas de antelación.",
            "Decisión: programar el corte para el sábado {date} a las 02:00 UTC. Plan de reversión: redirigir el DNS al master de MySQL si los health checks fallan en 30 minutos.",
        ],
        "payment-incident": [
            "Alerta: la pasarela de pago {gateway_old} devuelve errores 503 para el 12% de los intentos de pago desde {time}. {owner} lidera la respuesta al incidente.",
            "Se intentó revertir el cliente API de {gateway_old} de v3.2 a v3.1 — la tasa de error bajó del 12% al 3% pero no se resolvió completamente.",
            "Decisión: desviar el tráfico de pago a {gateway_new} mientras {gateway_old} investiga. {owner} autorizó el cambio a las {time2}.",
            "Impacto al cliente: aproximadamente {impact_count} transacciones fallaron durante el corte de 47 minutos. Reembolsos procesándose automáticamente.",
            "Postmortem: la causa raíz fue un error de cadena de certificados en {gateway_old} v3.2. El certificado CA intermedio expiró pero el cliente no validaba la cadena.",
            "Acción: {owner} debe implementar monitoreo de transacciones sintéticas para {gateway_old} y {gateway_new} antes de {deadline}. Severidad: P1.",
        ],
        "vendor-dispute": [
            "El proveedor {supplier} entregó el lote {lot_id} el {date}. El control de calidad encontró {defect}: {metric} medido en {measured_value} frente al límite de especificación de {spec_limit}.",
            "Hemos emitido una solicitud de nota de crédito de {credit_amount} a {supplier} por el lote {lot_id} no conforme. {supplier} disputa el reclamo y dice que la mercancía era conforme al despacho.",
            "La factura {invoice_id} por {invoice_amount} está vencida hace {overdue_days} días. Pago bloqueado pendiente de resolución de la disputa de la nota de crédito.",
            "Producción confirma: las {qty_affected} en cuarentena no son utilizables. Tuvimos que comprar a otro proveedor con un recargo de {surcharge}.",
            "{supplier} propuso un gesto comercial de {gesture_amount} en lugar de los {credit_amount} solicitados. Decisión necesaria de {owner}.",
            "Decisión: {owner} acepta el crédito reducido de {gesture_amount} y libera el pago de la factura {invoice_id}, deduciendo el crédito. Condición: {supplier} proporciona un informe de calidad para el próximo envío.",
        ],
    },
}

LANGS["Japanese"] = {
    "script": "CJK",
    "params": {
        "db-migration": {"owner":"田中","warehouse":"東京","sku1":"SKU-8842","qty1":"120","host":"db-prod-01","customer":"丸紅","date":"7月20日"},
        "payment-incident": {"owner":"佐藤","gateway_old":"Stripe","gateway_new":"Adyen","time":"14:30 UTC","time2":"14:47 UTC","impact_count":"840","deadline":"金曜日"},
        "vendor-dispute": {"supplier":"キーエンス","lot_id":"AX-2503","date":"5月6日","defect":"過熱","metric":"温度","measured_value":"85°C","spec_limit":"70°C","credit_amount":"126,000円","invoice_id":"FA-2025-0411","invoice_amount":"900,000円","overdue_days":"15日","qty_affected":"40台","surcharge":"32,000円","gesture_amount":"60,000円","owner":"田中"},
    },
    "translations": {
        "db-migration": [
            "決定：来スプリントに請求データベースをMySQLからPostgresに移行する。担当者は{owner}。リスク：切替中のサービス停止。",
            "{warehouse}倉庫が在庫差異を報告：{sku1}がシステムより{qty1}台少ない。スキャンエラーを調査中。",
            "未解決の質問：Postgres切替前に読み取りレプリカが必要か？レポート停止を回避するため。",
            "更新：Postgresクラスタは{host}に16 vCPU、64 GB RAMでプロビジョニング済み。pg_loaderのドライランは42分で完了。",
            "リスク評価：切替ウィンドウは約4時間と見積もり。{owner}は{customer}に48時間前に通知する。",
            "決定：切替を土曜日{date}の02:00 UTCに予定。ロールバック計画：ヘルスチェックが30分以内に失敗した場合、DNSをMySQLマスターに再指向。",
        ],
        "payment-incident": [
            "アラート：{gateway_old}決済ゲートウェイが{time}以降、チェックアウト試行の12%で503エラーを返している。{owner}がインシデント対応を主導。",
            "{gateway_old}のAPIクライアントをv3.2からv3.1にロールバック試行 — エラー率は12%から3%に低下したが完全には解決せず。",
            "決定：{gateway_old}の調査中、チェックアウトトラフィックを{gateway_new}にフェイルオーバー。{owner}が{time2}に切替を承認。",
            "顧客影響：47分間の停止中に約{impact_count}件の取引が失敗。返金は自動処理中。",
            "事後分析：根本原因は{gateway_old} v3.2の証明書チェーンエラー。中間CAが期限切れだがクライアントがチェーンを検証していなかった。",
            "アクションアイテム：{owner}は{deadline}までに{gateway_old}と{gateway_new}の合成取引監視を実装する。重大度：P1。",
        ],
        "vendor-dispute": [
            "サプライヤー{supplier}が{date}にロット{lot_id}を納品。品質管理が{defect}を発見：{metric}は{measured_value}で、仕様上限は{spec_limit}。",
            "{supplier}に対し、不合格ロット{lot_id}のクレジット{credit_amount}を請求。{supplier}はクレームを争い、出荷時は適合していたと主張。",
            "請求書{invoice_id}（{invoice_amount}）は{overdue_days}延滞。クレジット紛争の解決まで支払い保留中。",
            "生産確認：{qty_affected}は検疫中で使用不可。代替サプライヤーから{surcharge}の割増で緊急購入。",
            "{supplier}は{credit_amount}の代わりに{gesture_amount}の商業的譲歩を提案。{owner}の決定が必要。",
            "決定：{owner}は{gesture_amount}の減額クレジットを承認し、請求書{invoice_id}の支払いをクレジット控除後に解除。条件：{supplier}は次回出荷の品質保証報告書を提出。",
        ],
    },
}

LANGS["Chinese"] = {
    "script": "CJK",
    "params": {
        "db-migration": {"owner":"Priya","warehouse":"上海","sku1":"SKU-8842","qty1":"120","host":"db-prod-01","customer":"Acme","date":"7月20日"},
        "payment-incident": {"owner":"李明","gateway_old":"Stripe","gateway_new":"Adyen","time":"14:30 UTC","time2":"14:47 UTC","impact_count":"840","deadline":"周五"},
        "vendor-dispute": {"supplier":"东方包装","lot_id":"BR-2505","date":"5月6日","defect":"湿度过高","metric":"湿度","measured_value":"12.4%","spec_limit":"9%","credit_amount":"12,600元","invoice_id":"FA-2025-0411","invoice_amount":"90,000元","overdue_days":"15天","qty_affected":"18托盘","surcharge":"3,200元","gesture_amount":"6,000元","owner":"王芳"},
    },
    "translations": {
        "db-migration": [
            "决定：下个迭代将计费数据库从MySQL迁移到Postgres。负责人是{owner}。风险：切换期间服务停机。",
            "{warehouse}仓库报告库存差异：{sku1}比系统记录少{qty1}件，正在调查是否为扫描错误。",
            "未决问题：Postgres切换前是否需要先建立只读副本以避免报表停机？",
            "更新：Postgres集群已在{host}上配置，16个vCPU和64 GB内存。pg_loader试运行在42分钟内完成。",
            "风险评估：切换窗口预计4小时。{owner}将提前48小时通知{customer}。",
            "决定：切换安排在周六{date}02:00 UTC。回滚计划：如果健康检查在30分钟内失败，将DNS重新指向MySQL主库。",
        ],
        "payment-incident": [
            "告警：{gateway_old}支付网关自{time}起对12%的结账请求返回503错误。{owner}正在主导事件响应。",
            "尝试将{gateway_old} API客户端从v3.2回滚到v3.1 — 错误率从12%降至3%但未完全解决。",
            "决定：在{gateway_old}调查期间，将结账流量切换到{gateway_new}。{owner}在{time2}授权了切换。",
            "客户影响：47分钟中断期间约{impact_count}笔交易失败。退款正在自动处理。",
            "事后分析：根本原因是{gateway_old} v3.2的证书链错误。中间CA已过期但客户端未验证链。",
            "行动项：{owner}须在{deadline}前为{gateway_old}和{gateway_new}实施合成交易监控。严重性：P1。",
        ],
        "vendor-dispute": [
            "供应商{supplier}于{date}交付了批次{lot_id}。质量控制发现{defect}：{metric}测量值为{measured_value}，规格上限为{spec_limit}。",
            "我们已向{supplier}就不合格批次{lot_id}发出{credit_amount}的贷记单请求。{supplier}对此提出异议，称货物在发货时是合格的。",
            "发票{invoice_id}（金额{invoice_amount}）已逾期{overdue_days}。在贷记单争议解决之前，付款被冻结。",
            "生产确认：{qty_affected}在隔离区不可用。我们不得不以{surcharge}的溢价从其他供应商紧急采购。",
            "{supplier}提出{gesture_amount}的商业让步，代替全额{credit_amount}。需要{owner}做出决定。",
            "决定：{owner}接受{gesture_amount}的减额贷记，并解除发票{invoice_id}的付款，扣除贷记金额。条件：{supplier}需为下次发货提供质量保证报告。",
        ],
    },
}

LANGS["Vietnamese"] = {
    "script": "Latin",
    "params": {
        "db-migration": {"owner":"Lan","warehouse":"Hải Phòng","sku1":"SKU-7720","qty1":"150","host":"db-prod-01","customer":"VinMart","date":"20 tháng 7"},
        "payment-incident": {"owner":"Hùng","gateway_old":"MoMo","gateway_new":"VNPay","time":"14:30 UTC","time2":"14:47 UTC","impact_count":"840","deadline":"thứ Sáu"},
        "vendor-dispute": {"supplier":"VinaPack","lot_id":"LOT-2207","date":"6 tháng 5","defect":"không đạt tiêu chuẩn","metric":"độ dày","measured_value":"0.18mm","spec_limit":"0.22mm","credit_amount":"12.600.000đ","invoice_id":"HD-2025-0411","invoice_amount":"90.000.000đ","overdue_days":"15","qty_affected":"500 cuộn","surcharge":"3.200.000đ","gesture_amount":"6.000.000đ","owner":"Lan"},
    },
    "translations": {
        "db-migration": [
            "Quyết định: chuyển hệ thống thanh toán từ MySQL sang Postgres trong quý tới. Người phụ trách là {owner}. Rủi ro: gián đoạn dịch vụ trong quá trình chuyển.",
            "Kho {warehouse} báo cáo thiếu hụt tồn kho: {sku1} ít hơn {qty1} đơn vị so với hệ thống ghi nhận; đang điều tra lỗi quét mã.",
            "Câu hỏi còn bỏ ngỏ: chúng ta có cần môi trường thử nghiệm Postgres trước khi chuyển lưu lượng sản xuất hay không?",
            "Cập nhật: cụm Postgres đã được cấp phát trên {host} với 16 vCPU và 64 GB RAM. pg_loader chạy thử hoàn thành trong 42 phút.",
            "Đánh giá rủi ro: thời gian chuyển đổi ước tính 4 giờ. {owner} sẽ thông báo cho {customer} trước 48 giờ.",
            "Quyết định: lên lịch chuyển đổi vào thứ Bảy {date} lúc 02:00 UTC. Kế hoạch hoàn lại: chuyển DNS về MySQL master nếu kiểm tra sức khỏe thất bại trong 30 phút.",
        ],
        "payment-incident": [
            "Cảnh báo: cổng thanh toán {gateway_old} trả lỗi 503 cho 12% giao dịch thanh toán từ {time}. {owner} đang dẫn dắt xử lý sự cố.",
            "Đã thử hoàn nguyên client API {gateway_old} từ v3.2 xuống v3.1 — tỷ lệ lỗi giảm từ 12% xuống 3% nhưng chưa khắc phục hoàn toàn.",
            "Quyết định: chuyển hướng giao dịch thanh toán sang {gateway_new} trong khi {gateway_old} điều tra. {owner} đã cho phép chuyển đổi lúc {time2}.",
            "Tác động khách hàng: khoảng {impact_count} giao dịch thất bại trong 47 phút gián đoạn. Hoàn tiền đang được xử lý tự động.",
            "Hậu khảo: nguyên nhân gốc là lỗi chuỗi chứng chỉ trong {gateway_old} v3.2. Chứng chỉ CA trung gian hết hạn nhưng client không xác thực chuỗi.",
            "Hành động: {owner} cần triển khai giám sát giao dịch tổng hợp cho {gateway_old} và {gateway_new} trước {deadline}. Mức độ: P1.",
        ],
        "vendor-dispute": [
            "Nhà cung cấp {supplier} đã giao lô {lot_id} vào {date}. Kiểm tra chất lượng phát hiện {defect}: {metric} đo được {measured_value} so với giới hạn quy cách {spec_limit}.",
            "Chúng tôi đã phát hành yêu cầu tín dụng {credit_amount} tới {supplier} cho lô {lot_id} không đạt. {supplier} tranh chấp và cho rằng hàng hóa đạt chuẩn khi xuất kho.",
            "Hóa đơn {invoice_id} trị giá {invoice_amount} đã quá hạn {overdue_days}. Thanh toán bị phong tỏa chờ giải quyết tranh chấp tín dụng.",
            "Sản xuất xác nhận: {qty_affected} trong khu cách ly không sử dụng được. Chúng tôi phải mua khẩn cấp từ nhà cung cấp khác với phụ phí {surcharge}.",
            "{supplier} đề xuất giảm trừ thương mại {gesture_amount} thay vì {credit_amount} đầy đủ. Cần quyết định từ {owner}.",
            "Quyết định: {owner} chấp nhận tín dụng giảm {gesture_amount} và giải thanh toán hóa đơn {invoice_id}, khấu trừ tín dụng. Điều kiện: {supplier} cung cấp báo cáo chất lượng cho lô hàng tiếp theo.",
        ],
    },
}

LANGS["Thai"] = {
    "script": "Thai",
    "params": {
        "db-migration": {"owner":"สมชาย","warehouse":"กรุงเทพ","sku1":"SKU-5410","qty1":"90","host":"db-prod-01","customer":"CP All","date":"20 กรกฎาคม"},
        "payment-incident": {"owner":"นภา","gateway_old":"2C2P","gateway_new":"Omise","time":"14:30 UTC","time2":"14:47 UTC","impact_count":"840","deadline":"วันศุกร์"},
        "vendor-dispute": {"supplier":"ThaiPack","lot_id":"LOT-3301","date":"6 พฤษภาคม","defect":"ความชื้นสูง","metric":"ความชื้น","measured_value":"12.4%","spec_limit":"9%","credit_amount":"12,600 บาท","invoice_id":"FA-2025-0411","invoice_amount":"90,000 บาท","overdue_days":"15","qty_affected":"18 แป้น","surcharge":"3,200 บาท","gesture_amount":"6,000 บาท","owner":"สมชาย"},
    },
    "translations": {
        "db-migration": [
            "การตัดสินใจ: ย้ายฐานข้อมูลการเรียกเก็บเงินจาก MySQL ไปยัง Postgres ในสปรินต์ถัดไป ผู้รับผิดชอบคือ{owner} ความเสี่ยงคือบริการหยุดชะงักระหว่างการเปลี่ยนระบบ",
            "คลังสินค้า{warehouse}รายงานความแตกต่างของสินค้าคงคลัง: {sku1} น้อยกว่าระบบ {qty1} ชิ้น กำลังตรวจสอบข้อผิดพลาดในการสแกน",
            "คำถามที่ยังไม่ได้ข้อสรุป: จำเป็นต้องมีฐานข้อมูลสำรองแบบอ่านอย่างเดียวก่อนการย้าย Postgres เพื่อหลีกเลี่ยงรายงานหยุดทำงานหรือไม่",
            "อัปเดต: คลัสเตอร์ Postgres ถูกจัดสรรบน {host} ด้วย 16 vCPU และ 64 GB RAM การทดสอบ pg_loader เสร็จสิ้นใน 42 นาที",
            "การประเมินความเสี่ยง: ช่วงเวลาเปลี่ยนระบบประมาณ 4 ชั่วโมง {owner} จะแจ้ง{customer}ล่วงหน้า 48 ชั่วโมง",
            "การตัดสินใจ: กำหนดเวลาเปลี่ยนระบบวันเสาร์ที่ {date} เวลา 02:00 UTC แผนสำรอง: เปลี่ยน DNS กลับไปยัง MySQL master หากการตรวจสอบล้มเหลวภายใน 30 นาที",
        ],
        "payment-incident": [
            "แจ้งเตือน: เกตเวย์การชำระเงิน {gateway_old} ส่งกลับข้อผิดพลาด 503 สำหรับ 12% ของการชำระเงินตั้งแต่ {time} {owner} กำลังนำการตอบสนองต่อเหตุการณ์",
            "พยายามย้อนกลับ API client ของ {gateway_old} จาก v3.2 เป็น v3.1 — อัตราข้อผิดพลาดลดจาก 12% เป็น 3% แต่ยังไม่ได้รับการแก้ไข",
            "การตัดสินใจ: เปลี่ยนเส้นทางการชำระเงินไปยัง {gateway_new} ขณะที่ {gateway_old} ตรวจสอบ {owner} อนุมัติการเปลี่ยนถ่ายเมื่อ {time2}",
            "ผลกระทบต่อลูกค้า: ประมาณ {impact_count} ธุรกรรมล้มเหลวระหว่างการหยุดทำงาน 47 นาที กำลังดำเนินการคืนเงินอัตโนมัติ",
            "สรุปเหตุการณ์: สาเหตุหลักคือข้อผิดพลาดของห่วงโซ่ใบรับรองใน {gateway_old} v3.2 ใบรับรอง CA ระดับกลางหมดอายุ แต่ไคลเอนต์ไม่ได้ตรวจสอบห่วงโซ่",
            "สิ่งที่ต้องดำเนินการ: {owner} ต้องติดตั้งการตรวจสอบธุรกรรมจำลองสำหรับ {gateway_old} และ {gateway_new} ภายใน {deadline} ระดับความรุนแรง: P1",
        ],
        "vendor-dispute": [
            "ซัพพลายเออร์ {supplier} ส่งมอบล็อต {lot_id} เมื่อ {date} การควบคุมคุณภาพพบ {defect}: {metric} วัดได้ {measured_value} เทียบกับขีดจำกัด {spec_limit}",
            "เราได้ออกคำขอเครดิต {credit_amount} ถึง {supplier} สำหรับล็อต {lot_id} ที่ไม่ได้มาตรฐาน {supplier} โต้แย้งและยืนยันว่าสินค้าได้มาตรฐานเมื่อส่งออก",
            "ใบแจ้งหนี้ {invoice_id} มูลค่า {invoice_amount} เกินกำหนด {overdue_days} การชำระเงินถูกระงับรอการแก้ไขข้อพิพาทเครดิต",
            "ฝ่ายผลิตยืนยัน: {qty_affected} ในพื้นที่กักกันไม่สามารถใช้งานได้ ต้องซื้อจากซัพพลายเออร์อื่นด้วยราคาสูงกว่า {surcharge}",
            "{supplier} เสนอท่าทีเชิงพาณิชย์ {gesture_amount} แทน {credit_amount} เต็มจำนวน ต้องการการตัดสินใจจาก {owner}",
            "การตัดสินใจ: {owner} ยอมรับเครดิตลดลง {gesture_amount} และปล่อยการชำระของใบแจ้งหนี้ {invoice_id} หักเครดิต เงื่อนไข: {supplier} ต้องรายงานคุณภาพสำหรับการส่งมอบครั้งต่อไป",
        ],
    },
}

LANGS["Indonesian"] = {
    "script": "Latin",
    "params": {
        "db-migration": {"owner":"Budi","warehouse":"Surabaya","sku1":"SKU-6310","qty1":"110","host":"db-prod-01","customer":"Tokopedia","date":"20 Juli"},
        "payment-incident": {"owner":"Rina","gateway_old":"Stripe","gateway_new":"Adyen","time":"14:30 UTC","time2":"14:47 UTC","impact_count":"840","deadline":"Jumat"},
        "vendor-dispute": {"supplier":"Packindo","lot_id":"LOT-5512","date":"6 Mei","defect":"kebocoran","metric":"segel","measured_value":"12% bocor","spec_limit":"3%","credit_amount":"Rp 12.600.000","invoice_id":"FA-2025-0411","invoice_amount":"Rp 90.000.000","overdue_days":"15","qty_affected":"2.000 kotak","surcharge":"Rp 3.200.000","gesture_amount":"Rp 6.000.000","owner":"Budi"},
    },
    "translations": {
        "db-migration": [
            "Keputusan: migrasikan basis data penagihan dari MySQL ke Postgres pada sprint berikutnya. Penanggung jawab: {owner}. Risiko: gangguan layanan selama peralihan.",
            "Gudang {warehouse} melaporkan selisih stok: {sku1} kurang {qty1} unit dibandingkan sistem; sedang menyelidiki kesalahan pemindaian.",
            "Pertanyaan terbuka: apakah kita memerlukan replika baca sebelum peralihan Postgres untuk menghindari laporan yang tidak berjalan?",
            "Pembaruan: cluster Postgres telah disiapkan di {host} dengan 16 vCPU dan 64 GB RAM. Uji coba pg_loader selesai dalam 42 menit.",
            "Penilaian risiko: jendela peralihan diperkirakan 4 jam. {owner} akan memberitahu {customer} 48 jam sebelumnya.",
            "Keputusan: jadwalkan peralihan untuk Sabtu {date} pukul 02:00 UTC. Rencana rollback: arahkan DNS kembali ke master MySQL jika health check gagal dalam 30 menit.",
        ],
        "payment-incident": [
            "Peringatan: gateway pembayaran {gateway_old} mengembalikan error 503 untuk 12% percobaan checkout sejak {time}. {owner} memimpin respons insiden.",
            "Rollback dicoba pada klien API {gateway_old} dari v3.2 ke v3.1 — tingkat error turun dari 12% ke 3% tetapi tidak sepenuhnya teratasi.",
            "Keputusan: alihkan lalu lintas checkout ke {gateway_new} sementara {gateway_old} menyelidiki. {owner} mengesahkan pengalihan pada {time2}.",
            "Dampak pelanggan: sekitar {impact_count} transaksi gagal selama pemadaman 47 menit. Pengembalian dana diproses secara otomatis.",
            "Postmortem: akar penyebab adalah error rantai sertifikat di {gateway_old} v3.2. Sertifikat CA perantara kedaluwarsa tetapi klien tidak memvalidasi rantai.",
            "Tindakan: {owner} harus menerapkan pemantauan transaksi sintetis untuk {gateway_old} dan {gateway_new} sebelum {deadline}. Tingkat keparahan: P1.",
        ],
        "vendor-dispute": [
            "Pemasok {supplier} mengirim lot {lot_id} pada {date}. Kontrol kualitas menemukan {defect}: {metric} terukur {measured_value} dibanding batas spesifikasi {spec_limit}.",
            "Kami menerbitkan permintaan nota kredit {credit_amount} kepada {supplier} untuk lot {lot_id} yang tidak sesuai. {supplier} mempersengketakan klaim dan mengatakan barang sesuai saat dikirim.",
            "Faktur {invoice_id} sebesar {invoice_amount} telah jatuh tempo {overdue_days}. Pembayaran diblokir menunggu penyelesaian sengketa nota kredit.",
            "Produksi mengonfirmasi: {qty_affected} dalam karantina tidak dapat digunakan. Kami harus membeli dari pemasok lain dengan biaya tambahan {surcharge}.",
            "{supplier} mengusulkan gestur komersial {gesture_amount} sebagai ganti {credit_amount} penuh. Keputusan diperlukan dari {owner}.",
            "Keputusan: {owner} menerima kredit yang dikurangi {gesture_amount} dan melepaskan pembayaran faktur {invoice_id}, dikurangi kredit. Syarat: {supplier} menyediakan laporan kualitas untuk pengiriman berikutnya.",
        ],
    },
}

LANGS["Arabic"] = {
    "script": "Arabic",
    "params": {
        "db-migration": {"owner":"بريا","warehouse":"دبي","sku1":"SKU-9920","qty1":"130","host":"db-prod-01","customer":"Aramex","date":"20 يوليو"},
        "payment-incident": {"owner":"أحمد","gateway_old":"Stripe","gateway_new":"Adyen","time":"14:30 UTC","time2":"14:47 UTC","impact_count":"840","deadline":"الجمعة"},
        "vendor-dispute": {"supplier":"الشركة العربية للتغليف","lot_id":"BR-2505","date":"6 مايو","defect":"رطوبة زائدة","metric":"الرطوبة","measured_value":"12.4%","spec_limit":"9%","credit_amount":"12,600 درهم","invoice_id":"FA-2025-0411","invoice_amount":"90,000 درهم","overdue_days":"15","qty_affected":"18 منصة","surcharge":"3,200 درهم","gesture_amount":"6,000 درهم","owner":"بريا"},
    },
    "translations": {
        "db-migration": [
            "القرار: ترحيل قاعدة بيانات الفوترة من MySQL إلى Postgres في الدورة القادمة. المسؤولة {owner}. الخطر: توقف الخدمة أثناء التحويل.",
            "مستودع {warehouse} يبلغ عن فرق في المخزون: {sku1} أقل بمقدار {qty1} وحدة مقارنة بالنظام. يجري التحقيق في خطأ مسح.",
            "سؤال مفتوح: هل نحتاج إلى نسخة قراءة احتياطية قبل تحويل Postgres لتجنب توقف التقارير؟",
            "تحديث: تم توفير مجموعة Postgres على {host} بـ 16 vCPU و64 GB RAM. اكتمل اختبار pg_loader في 42 دقيقة.",
            "تقييم المخاطر: نافذة التحويل تقدر بـ 4 ساعات. {owner} ستخط
