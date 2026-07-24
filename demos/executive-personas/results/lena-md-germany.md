# Lena Brandt — Geschäftsführerin (Managing Director)
_Brandt Präzisionstechnik GmbH · Stuttgart, Germany · languages: German, English_

_Run at 2026-07-24T10:32:17.033496+00:00 against `http://localhost:8080`._

> Lena is Managing Director of Brandt Präzisionstechnik, a 90-person Mittelstand precision-CNC manufacturer near Stuttgart supplying automotive Tier-1s. Company knowledge is scattered across DATEV and Lexoffice (accounting), Personio (HR), email, Slack, a works-council folder, supplier portals and Zoom — predominantly German with some English customer threads.

**Situation.** A bearing-housing batch shows a tolerance deviation flagged by a Tier-1 automotive customer, triggering an 8D quality process; the works council (Betriebsrat) is consulting on related overtime; and an ex-employee has filed a DSGVO erasure request. Lena needs one place to answer 'where does the bearing 8D stand and what's our customer exposure?' and to action the erasure cleanly.

## The private compartments (scopes)

| Scope | Tier | What it holds |
| --- | --- | --- |
| `quality-8d-bearing` | channel | The bearing-housing tolerance deviation: the Tier-1 customer complaint, the 8D root-cause process and containment. |
| `compliance-dsgvo` | channel | Data-protection (DSGVO) requests and records-of-processing notes. |
| `works-council-betriebsrat` | channel | Works-council (Betriebsrat) consultations: overtime, shift changes and co-determination. |
| `accounting-datev` | channel | DATEV / Lexoffice accounting: month-end, a customer credit note and the warranty provision. |
| `supplier-audit` | channel | Audit of the steel supplier whose material is implicated in the bearing deviation. |
| `employee-keller-personal` | user | A former employee, Herr Keller, who filed a DSGVO Art. 17 erasure request. |

- **[PASS]** Gateway is healthy — HTTP 200

## Step 1 — Pull every source into one private store

Ingested **19/19** records across **6** scopes, **7** source types, languages: {'de': 17, 'en': 2}.

- **[PASS]** All business records ingested — 19/19

## Step 2 — Recall in the local language (and across languages)

**Q [German] (quality-8d-bearing):** BG-2025-14  
_Find the bearing deviation root cause across complaint + 8D ticket._
> 8D-Ticket Q-2025-77: Sofortmaßnahme (D3) — betroffene Charge BG-2025-14 gesperrt, Nacharbeit eingeleitet, Ersatzlieferung per Express. Ursachenhypothese (D4): Werkzeugverschleiß an Maschine M-07, der nicht rechtzeitig im SPC erkannt wurde.

- **[PASS]** Recall [German] 'BG-2025-14' — 3 hits, matched ['BG-2025-14', 'Werkzeugverschleiß', 'M-07', 'Toleranz']
**Q [English] (quality-8d-bearing):** containment inspection  
_Cross-language: English query over DE/EN 8D records to quantify exposure._
> Email to the Tier-1 customer (English): 'Containment is in place: affected lot BG-2025-14 is quarantined, replacement parts shipped by express, and we have implemented 100% inspection. Root cause is tool wear on machine M-07 not caught by SPC; corrective actions are tightening control limits and reducing tool life.'

- **[PASS]** Recall [English] 'containment inspection' — 1 hits, matched ['containment']
**Q [German] (accounting-datev):** DATEV Rückstellung  
_Tie the warranty provision and credit note to the quality issue._
> DATEV: Monatsabschluss Mai zu 95% fertig. Offen — Abstimmung der Rückstellung mit der Qualitätsabteilung und die Gutschrift G-2025-203.

- **[PASS]** Recall [German] 'DATEV Rückstellung' — 2 hits, matched ['Rückstellung', '25.000', 'G-2025-203', 'DATEV']
**Q [German] (supplier-audit):** Rundmaterial Spezifikation  
_Confirm the supplier is exonerated by material traceability._
> Audit beim Stahllieferanten: Die Materialchargen-Rückverfolgung zeigt, dass das für BG-2025-14 verwendete Rundmaterial innerhalb der Spezifikation lag. Die Abweichung ist also fertigungsseitig (Werkzeugverschleiß), nicht materialseitig.

- **[PASS]** Recall [German] 'Rundmaterial Spezifikation' — 1 hits, matched ['Rundmaterial', 'Spezifikation', 'Rückverfolg']

## Step 3 — Scope isolation (no cross-compartment leakage)

- **[PASS]** Control: 'Keller' retrievable in home scope `employee-keller-personal` — HTTP 200, 2 hit(s)
- **[PASS]** Isolation: 'Keller' does NOT leak into `works-council-betriebsrat` — HTTP 200, 0 hit(s) (want 0)

## Step 4 — Synthesise a briefing with the on-device model

**Business question:** Where does the bearing 8D stand, what is the root cause, and what is the customer exposure?

The model is given **3** evidence record(s) from `quality-8d-bearing` and asked for a JSON briefing.

- **[PASS]** Synthesis ran against the live model for `quality-8d-bearing` — HTTP 202, recap chars=1759
**Actual model output — recap written to channel memory:**

> The SPC-Regelkarte showed a trend but the intervention limit was set too far; D5/D7 tightened limits and reduced tool life on machine M-07. — Reklamation vom Tier-1-Kunden (Zulieferer für einen OEM): Charge BG-2025-14 der Lagergehä Liefervertrag bis 25.000 EUR, plus Sortierkosten. Wenn die 8D-Maßnahmen akzeptiert werden, entfällt die Pönale voraussichtlich.' customer (English): 'Containment is in place: affected lot BG-2025-14 is quarantined, replacement parts shipped by express, and w 8D-Ticket Q-2025-77: Sofortmaßnahme (D3) — betroffene Charge BG-2025- 8D-Ticket Q-2025-77: Sofortmaßnahme (D3) — betroffene Charge BG-2025-14 gesperr z. 1.200 Teile betroffen, Montagelinie des Kunden gestoppt. 8D-Report binnen 48 Stunden gefordert. Email to the Tier-1 customer (English): 'Containment is in place: affected lot ierung: 1.200 Teile, mögliche Pönale laut Liefervertrag bis 25.000 EUR, plus Sortierkosten. Wenn die 8D-Maßnahmen akzeptiert w abweichung am Innendurchmesser auf — 0,03 mm über Toleranz. 1.200 Teile betroffen, Montagelinie des Kunden gestoppt. 8D-Repor verschärfen und Werkzeugstandzeit von M-07 reduzieren. D6: 100%-Prüfung bis zur Bestätigung. Krisensitzung-Protokoll: Die SPC-Regelkarte zeigte einen Trend, aber die Eingriffsg eitet, Ersatzlieferung per Express. Ursachenhypothese (D4): Werkzeugverschleiß an Maschine M-07, der nicht rechtzeitig im SPC erkannt wurd r zu weit gesetzt. D5/D7 — Eingriffsgrenzen verschärfen und Werkzeugstandzeit von M-07 reduzieren. D6: 100%-Prüfung bis zur Bestätigung. perrt, Nacharbeit eingeleitet, Ersatzlieferung per Express. Ursachenhypothese (D4): Werkzeugverschleiß an Maschine M-07, der nicht rechtz Slack #qualitaet: 'Kundenexponierung: 1.200 Teile, mögliche Pönale laut Liefervertrag bis 25.000

_Business-term coverage: matched 7/10 expected terms (['bg-2025-14', '8d', 'toleranz', 'werkzeugverschleiß', 'm-07', 'pönale', 'containment'])._

**Actual model output — full structured bundle (replaying the production `SynthSummary` prompt + grammar under the deterministic sampling preset):**

_Sampling: fixed seed=0, temperature=0.0 (greedy), top_k=1. First-attempt budget n_predict=784 (adaptive to 3 rows)._

_Verify-and-retry engaged: the first attempt tripped a low-quality signal ({'recap_chars': 0, 'meta_commentary': False, 'too_short': True, 'exemplar_leak': False, 'list_exemplar_leak': False}); retried once at n_predict=1168 with the fact-only suffix; kept attempt #1._

```json
{
  "recap": " — Reklamation vom Tier-1-Kunden (Zulieferer für einen OEM): Charge BG-2025-14 der Lagergehä D-Ticket Q-2025-77: Sofortmaßnahme (D3) — betroffene Charge BG-2025-14 gesperrt, Nacharbeit eingeleitet, Ersatzlieferung per Expre 8D-Ticket Q-2025-77: Sofortmaßnahme (D3) — betroffene Charge BG-2025- 8D-Ticket Q-2025-77: Sofortmaßnahme (D3) — betroffene Charge BG-2025-14 gesperr z. 1.200 Teile betroffen, Montagelinie des Kunden gestoppt. 8D-Report binnen 48 Stunden gefordert. Reklamation vom Tier-1-Kunden (Zulieferer für einen OEM): Charge BG-2025-14 der La abweichung am Innendurchmesser auf — 0,03 mm über Toleranz. 1.200 Teile betroffen, Montagelinie des Kunden gestoppt. 8D-Repor ess. Ursachenhypothese (D4): Werkzeugverschleiß an Maschine M-07, der nicht rechtzeitig im SPC erkannt wurde. placement parts shipped by express, and we have implemented 100% inspection. Root cause is tool wear on machine M-07 not ca eitet, Ersatzlieferung per Express. Ursachenhypothese (D4): Werkzeugverschleiß an Maschine M-07, der nicht rechtzeitig im SPC erkannt wurd perrt, Nacharbeit eingeleitet, Ersatzlieferung per Express. Ursachenhypothese (D4): Werkzeugverschleiß an Maschine M-07, der nicht rechtz rge BG-2025-14 der Lagergehäuse weist eine Maßabweichung am Innendurchmesser auf — 0,03 mm über Toleranz. 1.200 Teile betroffen, Montage roffene Charge BG-2025-14 gesperrt, Nacharbeit eingeleitet, Ersatzlieferung per Express. Ursachenhypothese (D4): Werkzeugverschleiß an 8D-Ticket Q-2025-77: Sofortmaßnahme (D3) — betroffene Charge BG-2025-14 gesperrt, Nacharbeit ei r einen OEM): Charge BG-2025-14 der Lagergehäuse weist eine Maßabweichung am Innendurchmesser auf — 0,03 mm über Toleranz. 1.200 Teil",
  "decisions": [],
  "open_questions": [],
  "active_tasks": []
}
```

- **[PASS]** Synthesis is byte-reproducible across runs (fixed seed) — 2 runs, identical=True, 71 chars

## Step 5 — Cryptographic right to be forgotten

> Herr Keller filed an Art. 17 DSGVO erasure request; cryptographic key destruction renders his data unrecoverable, satisfying the right to be forgotten.

Before erase: **2** record(s); after erase: **0** record(s).

- **[PASS]** Deletion request accepted — HTTP 204
- **[PASS]** Data is unrecoverable after key destruction — HTTP 200→200, 2→0 records

## Result — 12/12 checks passed
