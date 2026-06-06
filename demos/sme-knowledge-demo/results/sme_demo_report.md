# Lotus & Bean — Knowledge system business demonstration

_Run at 2026-06-06T00:29:11.300779+00:00 against `http://localhost:8080`._

A 25-person specialty coffee-equipment retailer and servicer selling across Vietnam, Thailand, Singapore, and the UAE. Like most SMEs, its institutional knowledge is scattered across support email, team chat, a CRM, shared docs, a project tracker, and regional messaging apps. Nobody can answer 'what do we actually know about X?' without reading five tools.

## The business scopes (private compartments)

| Scope | What it holds |
| --- | --- |
| `support-x200` | All support activity for the X200 home espresso machine — the product line at the centre of a quality issue this quarter. |
| `sales-gulf-hotels` | The B2B sales pursuit of Al Noor Hotels (Dubai) for a 40-unit commercial order. |
| `ops-policy` | Company-wide policies: returns, warranty, and data retention. High-importance, slow-decay knowledge. |
| `customer-mai-vn` | A single Vietnamese retail customer (Mai Trần). Used to demonstrate per-customer scope isolation and the cryptographic 'right to be forgotten'. |
| `regional-inbox` | Inbound customer messages from regional channels in Vietnamese, Thai, and Arabic — demonstrates multilingual extraction and search. |

## Step 0 — Is the system running?

- **[PASS]** Gateway health endpoint returns ok — HTTP 200

## Step 1 — Pull every source into one private store

In a real SME this data lives in five or six different tools. Knowledge ingests it all through one API so it can be searched and synthesised together.

Ingested **41 of 41** business records across **5 scopes** and **7 source types**:

- Email: 10
- Other: 9
- GoogleWorkspace: 7
- Slack: 6
- Manual: 4
- Atlassian: 3
- HubSpot: 2

- **[PASS]** All business records ingested — 41/41

## Step 2 — Ask plain-English business questions

Each question is answered by searching the ranked evidence in the relevant scope. The full top record is shown — the way a staff member would read it after clicking the result.

**Q (support-x200):** What is the root cause of the X200 leaks?
> Created ticket OPS-481: 'X200-2503 batch gasket recall'. Scope: identify every X200 unit from the March batch, contact owners proactively, ship replacement gasket kits. Owner: Linh. Priority: High.

- **[PASS]** Answer found for: What is the root cause of the X200 leaks? — 3 hits, expected 'gasket'
**Q (support-x200):** Which production batch is affected and how many units?
> @linh: gasket replacement kits ordered for all 212 affected units. ETA 9 days. I'll track delivery confirmations in OPS-481.

- **[PASS]** Answer found for: Which production batch is affected and how many units? — 3 hits, expected '212'
**Q (support-x200):** What are we offering affected customers?
> Customer in Singapore replied to our proactive recall email: 'My X200 has been fine, but thanks for the heads up — please send the gasket kit anyway.' Logged and kit dispatched.

- **[PASS]** Answer found for: What are we offering affected customers? — 2 hits, expected 'kit'
**Q (sales-gulf-hotels):** What is blocking the Al Noor Hotels deal?
> Sent Al Noor our answers: (1) yes, 24-hour on-site SLA in Dubai and Abu Dhabi via our Al Quoz service partner, (2) yes, the C900 display supports Arabic, (3) 3-year warranty included with the service contract. Proposed AED 11,800 per unit for 40 units.

- **[PASS]** Answer found for: What is blocking the Al Noor Hotels deal? — 3 hits, expected 'service'
**Q (sales-gulf-hotels):** What price did we commit to Al Noor?
> New opportunity: Al Noor Hotels (Dubai) wants to standardise on commercial espresso machines across 8 properties. Initial ask is 40 units plus a 3-year service contract. Estimated deal value AED 480,000. Primary contact: Omar Haddad, Procurement Lead.

- **[PASS]** Answer found for: What price did we commit to Al Noor? — 3 hits, expected '11,800'
**Q (ops-policy):** What is our returns window?
> Returns policy: retail customers may return any machine within 30 days for a full refund, no questions asked. Defective units are covered for the full warranty period regardless of the 30-day window. Refunds are issued to the original payment method within 5 business days.

- **[PASS]** Answer found for: What is our returns window? — 1 hits, expected '30 days'
**Q (ops-policy):** How fast must we honour a data-deletion request?
> Data retention and privacy policy: customer personal data is retained for 24 months after the last interaction, then deleted. Customers may request deletion of their personal data at any time under Vietnam's PDPD and the UAE PDPL; such requests must be honoured within 30 days and logged.

- **[PASS]** Answer found for: How fast must we honour a data-deletion request? — 1 hits, expected '30 days'

## Step 3 — Search works across languages

The same store holds Vietnamese, Thai and Arabic customer messages. Searching a local-language term still finds them.

**Search `C900` in `regional-inbox`** (Arabic/Vietnamese customers asking about the commercial C900): 3 hit(s)
> ข้อความจากลูกค้าเชียงใหม่: 'มีบริการติดตั้งและสอนใช้งานเครื่อง C900 ไหมครับ' (Do you offer installation and training for the C900 machine?)

- **[PASS]** Multilingual search 'C900' in regional-inbox returns a hit
**Search `X200` in `regional-inbox`** (Regional customers mentioning the X200): 2 hit(s)
> ข้อความ LINE จากลูกค้าที่กรุงเทพฯ: 'เครื่อง X200 รุ่นเดือนมีนาคมมีปัญหาน้ำรั่วไหม ผมเพิ่งซื้อมา' (Does the March-batch X200 have the water leak problem? I just …

- **[PASS]** Multilingual search 'X200' in regional-inbox returns a hit
**Search `X200` in `customer-mai-vn`** (Vietnamese record for customer Mai): 2 hit(s)
> Mai asked (in Vietnamese) whether the replacement X200 is from the new batch with the fixed gasket. We confirmed yes — her replacement serial starts X200-2504, …

- **[PASS]** Multilingual search 'X200' in customer-mai-vn returns a hit

## Step 4 — Each compartment is isolated

A sales question must not leak into the support compartment. We search a support-only term inside the sales scope and expect nothing.

- **[PASS]** Support-only term 'gasket' does NOT appear in the sales scope — 0 hits (want 0)

## Step 5 — Turn raw evidence into a briefing

Synthesis condenses everything in a scope into a short memory: a recap, the decisions made, open questions, and active tasks. This needs the on-device language model (the `llama-server` sidecar or a managed endpoint).

The system read every support record and wrote this briefing:

> We have had 3 confirmed X200 base-seal leaks, all from the X200-2503 batch. The common factor across all three units is the base gasket, which is undersized. We are escalating to the supplier and will keep OPS-481 open until 100% of kits are delivered and we have 30 days with zero new leak reports.

- **[PASS]** Synthesis briefing for `support-x200` captures the defect + recall — matched 2/4 expected business terms

## Step 6 — Cryptographic 'right to be forgotten'

Customer Mai Trần filed a data-deletion request. We erase her entire scope. Because each scope is encrypted under its own key, destroying that key makes the data unrecoverable — not just hidden.

Before deletion: searching Mai's scope returns **2** record(s).
- **[PASS]** Deletion request accepted by the system — HTTP 204
After deletion: searching Mai's scope returns **0** record(s).

- **[PASS]** Mai's data is gone after the deletion request — 2 → 0

## Result

**16 of 16 business checks passed.**

This is what an SME gets: every scattered source searchable in one place, in any language, kept in isolated encrypted compartments, condensed into briefings, and erasable on request.

