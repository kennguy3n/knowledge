#!/usr/bin/env python3
"""Generate expanded benchmark dataset: 3 scenarios × 15 languages × 6 msgs × 8+ terms."""
import json, os
from pathlib import Path

HERE = Path(__file__).resolve().parent
DATASET_OUT = HERE / "dataset" / "expanded-benchmark.json"
FIXTURE_OUT = HERE / "fixtures" / "expanded-expected-terms.json"

# 3 scenario templates (English source). {placeholders} filled per-language.
SCENARIOS = [
    {"id":"db-migration","domain":"infrastructure","title":"Database migration from MySQL to Postgres",
     "messages":[
        "Decision: migrate the billing database from MySQL to Postgres next sprint. Owner is {owner}. Risk: service downtime during cutover.",
        "The {warehouse} warehouse reports an inventory discrepancy: {sku1} is short by {qty1} units versus the system of record; investigating a scan error.",
        "Open question: do we need a read replica before the Postgres cutover to avoid report downtime?",
        "Update: the Postgres cluster is provisioned on {host} with 16 vCPU and 64 GB RAM. pg_loader dry run completed in 42 minutes.",
        "Risk assessment: the cutover window is estimated at 4 hours. {owner} will notify {customer} 48 hours in advance.",
        "Decision: schedule the cutover for Saturday {date} at 02:00 UTC. Rollback plan: repoint DNS to MySQL master if health checks fail within 30 minutes."],
     "expected_terms":["MySQL","Postgres","{owner}","{sku1}","{warehouse}","replica","{host}","cutover"]},
    {"id":"payment-incident","domain":"incident","title":"Payment gateway outage and incident response",
     "messages":[
        "Alert: {gateway_old} payment gateway is returning 503 errors for 12% of checkout attempts since {time}. {owner} is leading the incident response.",
        "Rollback attempted on the {gateway_old} API client from v3.2 to v3.1. Error rate dropped from 12% to 3% but did not fully resolve.",
        "Decision: failover checkout traffic to {gateway_new} while {gateway_old} investigates. {owner} authorized the switch at {time2}.",
        "Customer impact: approximately {impact_count} transactions failed during the 47-minute outage. Refunds being processed automatically.",
        "Postmortem: root cause was a certificate chain error in {gateway_old} v3.2. The intermediate CA expired but the client did not validate the chain.",
        "Action item: {owner} to implement synthetic transaction monitoring for {gateway_old} and {gateway_new} by {deadline}. Severity: P1."],
     "expected_terms":["{gateway_old}","{gateway_new}","{owner}","503","failover","certificate","refund","P1"]},
    {"id":"vendor-dispute","domain":"procurement","title":"Supplier quality dispute and credit note negotiation",
     "messages":[
        "The supplier {supplier} delivered lot {lot_id} on {date}. Quality control found {defect}: {metric} measured at {measured_value} versus the spec limit of {spec_limit}.",
        "We issued a credit note request of {credit_amount} to {supplier} for the non-conforming {lot_id}. {supplier} disputes the claim and says goods were conforming at dispatch.",
        "The invoice {invoice_id} for {invoice_amount} is overdue by {overdue_days} days. Payment is blocked pending resolution of the credit note dispute.",
        "Production confirms: the {qty_affected} affected units in quarantine are unusable. We purchased from an alternate supplier at a surcharge of {surcharge}.",
        "{supplier} proposed a commercial gesture of {gesture_amount} instead of the full {credit_amount}. Decision needed from {owner}.",
        "Decision: {owner} accepts the reduced credit of {gesture_amount} and releases payment of invoice {invoice_id}, deducting the credit. Condition: {supplier} provides a QA report for the next shipment."],
     "expected_terms":["{supplier}","{lot_id}","credit","{invoice_id}","quarantine","{owner}","{credit_amount}","{gesture_amount}"]},
]

# Per-language params. Messages use English templates with localized params.
# This is realistic: business chats mix local names with English tech terms.
# The model must still synthesize a coherent recap IN the target language.
LANGS = {
    "English":{"script":"Latin","params":{
        "db-migration":{"owner":"Priya","warehouse":"Shanghai","sku1":"SKU-8842","qty1":"120","host":"db-prod-01","customer":"Acme Corp","date":"July 20"},
        "payment-incident":{"owner":"Marcus","gateway_old":"Stripe","gateway_new":"Adyen","time":"14:30 UTC","time2":"14:47 UTC","impact_count":"840","deadline":"Friday"},
        "vendor-dispute":{"supplier":"CartoNord","lot_id":"BR-2505","date":"May 6","defect":"excess humidity","metric":"humidity","measured_value":"12.4%","spec_limit":"9%","credit_amount":"12,600 EUR","invoice_id":"FA-2025-0411","invoice_amount":"90,000 EUR","overdue_days":"15 days","qty_affected":"18 pallets","surcharge":"3,200 EUR","gesture_amount":"6,000 EUR","owner":"Elise"}}},
    "French":{"script":"Latin","params":{
        "db-migration":{"owner":"Priya","warehouse":"Lyon","sku1":"SKU-8842","qty1":"120","host":"db-prod-01","customer":"BonjourBio","date":"20 juillet"},
        "payment-incident":{"owner":"Marc","gateway_old":"Stripe","gateway_new":"Adyen","time":"14h30 UTC","time2":"14h47 UTC","impact_count":"840","deadline":"vendredi"},
        "vendor-dispute":{"supplier":"CartoNord","lot_id":"BR-2505","date":"6 mai","defect":"un excès d'humidité","metric":"le taux d'humidité","measured_value":"12,4 %","spec_limit":"9 %","credit_amount":"12 600 EUR","invoice_id":"FA-2025-0411","invoice_amount":"90 000 EUR","overdue_days":"15 jours","qty_affected":"18 palettes","surcharge":"3 200 EUR","gesture_amount":"6 000 EUR","owner":"Elise"}}},
    "German":{"script":"Latin","params":{
        "db-migration":{"owner":"Lukas","warehouse":"München","sku1":"SKU-8842","qty1":"120","host":"db-prod-01","customer":"Bayerische Folien","date":"20. Juli"},
        "payment-incident":{"owner":"Anna","gateway_old":"Stripe","gateway_new":"Adyen","time":"14:30 UTC","time2":"14:47 UTC","impact_count":"840","deadline":"Freitag"},
        "vendor-dispute":{"supplier":"Bayerische Folien","lot_id":"FK-220","date":"6. Mai","defect":"eine Verzögerung","metric":"Lieferzeit","measured_value":"zwei Wochen","spec_limit":"eine Woche","credit_amount":"8.200 EUR","invoice_id":"RE-2025-0312","invoice_amount":"45.000 EUR","overdue_days":"12 Tage","qty_affected":"500 Rollen","surcharge":"1.800 EUR","gesture_amount":"4.000 EUR","owner":"Lena"}}},
    "Spanish":{"script":"Latin","params":{
        "db-migration":{"owner":"Sofia","warehouse":"Bogota","sku1":"SKU-3310","qty1":"80","host":"db-prod-01","customer":"TiendaAndina","date":"20 de julio"},
        "payment-incident":{"owner":"Diego","gateway_old":"Stripe","gateway_new":"Adyen","time":"14:30 UTC","time2":"14:47 UTC","impact_count":"840","deadline":"viernes"},
        "vendor-dispute":{"supplier":"Empaques Andina","lot_id":"LO-4502","date":"6 de mayo","defect":"un defecto de sellado","metric":"sellado","measured_value":"12% defectuoso","spec_limit":"3%","credit_amount":"8.500 USD","invoice_id":"FA-2025-0308","invoice_amount":"42.000 USD","overdue_days":"18 dias","qty_affected":"2.000 cajas","surcharge":"1.900 USD","gesture_amount":"4.200 USD","owner":"Sofia"}}},
    "Portuguese":{"script":"Latin","params":{
        "db-migration":{"owner":"Carlos","warehouse":"Sao Paulo","sku1":"SKU-2204","qty1":"95","host":"db-prod-01","customer":"Magalu","date":"20 de julho"},
        "payment-incident":{"owner":"Juliana","gateway_old":"Stripe","gateway_new":"Adyen","time":"14:30 UTC","time2":"14:47 UTC","impact_count":"840","deadline":"sexta-feira"},
        "vendor-dispute":{"supplier":"PackBR","lot_id":"LOT-7715","date":"6 de maio","defect":"vazamento","metric":"vedacao","measured_value":"12% com falha","spec_limit":"3%","credit_amount":"R$ 12.600","invoice_id":"NF-2025-0411","invoice_amount":"R$ 90.000","overdue_days":"15 dias","qty_affected":"2.000 caixas","surcharge":"R$ 3.200","gesture_amount":"R$ 6.000","owner":"Carlos"}}},
    "Japanese":{"script":"CJK","params":{
        "db-migration":{"owner":"Tanaka","warehouse":"Tokyo","sku1":"SKU-8842","qty1":"120","host":"db-prod-01","customer":"Marubeni","date":"7/20"},
        "payment-incident":{"owner":"Sato","gateway_old":"Stripe","gateway_new":"Adyen","time":"14:30 UTC","time2":"14:47 UTC","impact_count":"840","deadline":"Friday"},
        "vendor-dispute":{"supplier":"Keyence","lot_id":"AX-2503","date":"5/6","defect":"overheating","metric":"temperature","measured_value":"85C","spec_limit":"70C","credit_amount":"126,000 JPY","invoice_id":"FA-2025-0411","invoice_amount":"900,000 JPY","overdue_days":"15 days","qty_affected":"40 units","surcharge":"32,000 JPY","gesture_amount":"60,000 JPY","owner":"Tanaka"}}},
    "Chinese":{"script":"CJK","params":{
        "db-migration":{"owner":"Priya","warehouse":"Shanghai","sku1":"SKU-8842","qty1":"120","host":"db-prod-01","customer":"Acme","date":"7/20"},
        "payment-incident":{"owner":"Li Ming","gateway_old":"Stripe","gateway_new":"Adyen","time":"14:30 UTC","time2":"14:47 UTC","impact_count":"840","deadline":"Friday"},
        "vendor-dispute":{"supplier":"DongFang Pack","lot_id":"BR-2505","date":"5/6","defect":"excess humidity","metric":"humidity","measured_value":"12.4%","spec_limit":"9%","credit_amount":"12,600 CNY","invoice_id":"FA-2025-0411","invoice_amount":"90,000 CNY","overdue_days":"15 days","qty_affected":"18 pallets","surcharge":"3,200 CNY","gesture_amount":"6,000 CNY","owner":"Wang Fang"}}},
    "Korean":{"script":"CJK","params":{
        "db-migration":{"owner":"Kim","warehouse":"Seoul","sku1":"SKU-8842","qty1":"120","host":"db-prod-01","customer":"Coupang","date":"7/20"},
        "payment-incident":{"owner":"Park","gateway_old":"Stripe","gateway_new":"Adyen","time":"14:30 UTC","time2":"14:47 UTC","impact_count":"840","deadline":"Friday"},
        "vendor-dispute":{"supplier":"HanPack","lot_id":"KR-3301","date":"5/6","defect":"sealing defect","metric":"seal","measured_value":"12% defective","spec_limit":"3%","credit_amount":"12,600,000 KRW","invoice_id":"FA-2025-0411","invoice_amount":"90,000,000 KRW","overdue_days":"15 days","qty_affected":"2,000 boxes","surcharge":"3,200,000 KRW","gesture_amount":"6,000,000 KRW","owner":"Kim"}}},
    "Vietnamese":{"script":"Latin","params":{
        "db-migration":{"owner":"Lan","warehouse":"Hai Phong","sku1":"SKU-7720","qty1":"150","host":"db-prod-01","customer":"VinMart","date":"20/7"},
        "payment-incident":{"owner":"Hung","gateway_old":"MoMo","gateway_new":"VNPay","time":"14:30 UTC","time2":"14:47 UTC","impact_count":"840","deadline":"Friday"},
        "vendor-dispute":{"supplier":"VinaPack","lot_id":"LOT-2207","date":"6/5","defect":"thickness non-compliant","metric":"thickness","measured_value":"0.18mm","spec_limit":"0.22mm","credit_amount":"12,600,000 VND","invoice_id":"HD-2025-0411","invoice_amount":"90,000,000 VND","overdue_days":"15 days","qty_affected":"500 rolls","surcharge":"3,200,000 VND","gesture_amount":"6,000,000 VND","owner":"Lan"}}},
    "Thai":{"script":"Thai","params":{
        "db-migration":{"owner":"Somchai","warehouse":"Bangkok","sku1":"SKU-5410","qty1":"90","host":"db-prod-01","customer":"CP All","date":"20/7"},
        "payment-incident":{"owner":"Napha","gateway_old":"2C2P","gateway_new":"Omise","time":"14:30 UTC","time2":"14:47 UTC","impact_count":"840","deadline":"Friday"},
        "vendor-dispute":{"supplier":"ThaiPack","lot_id":"LOT-3301","date":"6/5","defect":"excess moisture","metric":"moisture","measured_value":"12.4%","spec_limit":"9%","credit_amount":"12,600 THB","invoice_id":"FA-2025-0411","invoice_amount":"90,000 THB","overdue_days":"15 days","qty_affected":"18 sheets","surcharge":"3,200 THB","gesture_amount":"6,000 THB","owner":"Somchai"}}},
    "Indonesian":{"script":"Latin","params":{
        "db-migration":{"owner":"Budi","warehouse":"Surabaya","sku1":"SKU-6310","qty1":"110","host":"db-prod-01","customer":"Tokopedia","date":"20/7"},
        "payment-incident":{"owner":"Rina","gateway_old":"Stripe","gateway_new":"Adyen","time":"14:30 UTC","time2":"14:47 UTC","impact_count":"840","deadline":"Jumat"},
        "vendor-dispute":{"supplier":"Packindo","lot_id":"LOT-5512","date":"6/5","defect":"leakage","metric":"seal","measured_value":"12% leaking","spec_limit":"3%","credit_amount":"Rp 12,600,000","invoice_id":"FA-2025-0411","invoice_amount":"Rp 90,000,000","overdue_days":"15 days","qty_affected":"2,000 boxes","surcharge":"Rp 3,200,000","gesture_amount":"Rp 6,000,000","owner":"Budi"}}},
    "Arabic":{"script":"Arabic","params":{
        "db-migration":{"owner":"Priya","warehouse":"Dubai","sku1":"SKU-9920","qty1":"130","host":"db-prod-01","customer":"Aramex","date":"20/7"},
        "payment-incident":{"owner":"Ahmed","gateway_old":"Stripe","gateway_new":"Adyen","time":"14:30 UTC","time2":"14:47 UTC","impact_count":"840","deadline":"Friday"},
        "vendor-dispute":{"supplier":"ArabPack","lot_id":"BR-2505","date":"6/5","defect":"excess humidity","metric":"humidity","measured_value":"12.4%","spec_limit":"9%","credit_amount":"12,600 AED","invoice_id":"FA-2025-0411","invoice_amount":"90,000 AED","overdue_days":"15 days","qty_affected":"18 pallets","surcharge":"3,200 AED","gesture_amount":"6,000 AED","owner":"Priya"}}},
    "Malay":{"script":"Latin","params":{
        "db-migration":{"owner":"Siti","warehouse":"Johor","sku1":"SKU-4820","qty1":"100","host":"db-prod-01","customer":"Shopee","date":"20/7"},
        "payment-incident":{"owner":"Faiz","gateway_old":"Stripe","gateway_new":"Adyen","time":"14:30 UTC","time2":"14:47 UTC","impact_count":"840","deadline":"Jumaat"},
        "vendor-dispute":{"supplier":"Malaysian Pack","lot_id":"LOT-9914","date":"6/5","defect":"sealing defect","metric":"seal","measured_value":"12% defective","spec_limit":"3%","credit_amount":"RM 12,600","invoice_id":"FA-2025-0411","invoice_amount":"RM 90,000","overdue_days":"15 days","qty_affected":"2,000 boxes","surcharge":"RM 3,200","gesture_amount":"RM 6,000","owner":"Siti"}}},
    "Tagalog":{"script":"Latin","params":{
        "db-migration":{"owner":"Andrea","warehouse":"Cebu","sku1":"SKU-6720","qty1":"95","host":"db-prod-01","customer":"Globe","date":"20/7"},
        "payment-incident":{"owner":"Miguel","gateway_old":"Stripe","gateway_new":"Adyen","time":"14:30 UTC","time2":"14:47 UTC","impact_count":"840","deadline":"Biyernes"},
        "vendor-dispute":{"supplier":"PhilPack","lot_id":"LOT-4408","date":"6/5","defect":"printing defect","metric":"print","measured_value":"12% defective","spec_limit":"3%","credit_amount":"PHP 12,600","invoice_id":"FA-2025-0411","invoice_amount":"PHP 90,000","overdue_days":"15 days","qty_affected":"2,000 boxes","surcharge":"PHP 3,200","gesture_amount":"PHP 6,000","owner":"Andrea"}}},
    "Hindi":{"script":"Devanagari","params":{
        "db-migration":{"owner":"Raj","warehouse":"Mumbai","sku1":"SKU-8842","qty1":"120","host":"db-prod-01","customer":"Flipkart","date":"20/7"},
        "payment-incident":{"owner":"Priya","gateway_old":"Stripe","gateway_new":"Adyen","time":"14:30 UTC","time2":"14:47 UTC","impact_count":"840","deadline":"Friday"},
        "vendor-dispute":{"supplier":"BharatPack","lot_id":"BR-2505","date":"6/5","defect":"moisture damage","metric":"moisture","measured_value":"12.4%","spec_limit":"9%","credit_amount":"Rs 12,600","invoice_id":"FA-2025-0411","invoice_amount":"Rs 90,000","overdue_days":"15 days","qty_affected":"18 pallets","surcharge":"Rs 3,200","gesture_amount":"Rs 6,000","owner":"Raj"}}},
    "Turkish":{"script":"Latin","params":{
        "db-migration":{"owner":"Mehmet","warehouse":"Istanbul","sku1":"SKU-8842","qty1":"120","host":"db-prod-01","customer":"Trendyol","date":"20/7"},
        "payment-incident":{"owner":"Ayse","gateway_old":"Stripe","gateway_new":"Adyen","time":"14:30 UTC","time2":"14:47 UTC","impact_count":"840","deadline":"Cuma"},
        "vendor-dispute":{"supplier":"TurkPack","lot_id":"TR-5519","date":"6/5","defect":"sealing defect","metric":"seal","measured_value":"12% defective","spec_limit":"3%","credit_amount":"12,600 TRY","invoice_id":"FA-2025-0411","invoice_amount":"90,000 TRY","overdue_days":"15 days","qty_affected":"2,000 boxes","surcharge":"3,200 TRY","gesture_amount":"6,000 TRY","owner":"Mehmet"}}},
}

def fill(template, params):
    for k, v in params.items():
        template = template.replace("{"+k+"}", str(v))
    return template

def main():
    dataset = {"scenarios": []}
    fixtures = {"sessions": {}}

    for lang_name, lang_data in sorted(LANGS.items()):
        for sc in SCENARIOS:
            sid = f"{lang_name}::{sc['id']}"
            params = lang_data["params"][sc["id"]]
            messages = [fill(m, params) for m in sc["messages"]]
            terms = [fill(t, params) for t in sc["expected_terms"]]

            dataset["scenarios"].append({
                "id": sid,
                "language": lang_name,
                "script": lang_data["script"],
                "domain": sc["domain"],
                "title": sc["title"],
                "messages": messages,
            })
            fixtures["sessions"][sid] = {
                "language": lang_name,
                "script": lang_data["script"],
                "expected_terms": terms,
            }

    DATASET_OUT.parent.mkdir(parents=True, exist_ok=True)
    FIXTURE_OUT.parent.mkdir(parents=True, exist_ok=True)
    DATASET_OUT.write_text(json.dumps(dataset, ensure_ascii=False, indent=2), encoding="utf-8")
    FIXTURE_OUT.write_text(json.dumps(fixtures, ensure_ascii=False, indent=2), encoding="utf-8")

    n_sessions = len(dataset["scenarios"])
    n_langs = len(LANGS)
    n_terms = sum(len(f["expected_terms"]) for f in fixtures["sessions"].values())
    print(f"Generated {n_sessions} sessions across {n_langs} languages ({n_terms} total expected terms)")
    print(f"  Dataset: {DATASET_OUT}")
    print(f"  Fixtures: {FIXTURE_OUT}")

if __name__ == "__main__":
    main()
