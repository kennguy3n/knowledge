# Anand Iyer — VP of Customer Success
_Dhruva Cloud · Bengaluru, India · languages: English, Hindi_

_Run at 2026-07-24T05:18:19.684887+00:00 against `http://localhost:8080`._

> Anand runs Customer Success at Dhruva Cloud, a B2B data-observability SaaS serving enterprises across India, the US and the Gulf. Each customer is a separate tenant. Account knowledge is spread across Salesforce-style CRM notes, Slack, email, Zendesk, Zoom QBR transcripts and a Jira-like tracker, mostly English with Hindi support threads.

**Situation.** A flagship tenant (Acme Manufacturing) is up for a 1.2 crore INR renewal and showing churn signals, while a new tenant (Globex) is mid-onboarding. Anand must keep each customer's data strictly compartmentalised, understand Acme's renewal risk, and answer 'why is Acme at risk and what's the save plan?' without any cross-tenant leakage.

## The private compartments (scopes)

| Scope | Tier | What it holds |
| --- | --- | --- |
| `tenant-acme-renewal` | domain | The Acme Manufacturing tenant: renewal risk, usage decline, exec sponsor change and the save plan. |
| `tenant-globex-onboarding` | domain | The Globex tenant: onboarding milestones, data-source connections and time-to-value. |
| `churn-risk-signals` | channel | Cross-account churn signals: health scores, support volume and login trends (aggregated, anonymised). |
| `support-escalations` | channel | Active support escalations across tenants, including Hindi-language threads. |
| `product-feedback` | channel | Feature requests and product feedback gathered from QBRs and tickets. |
| `customer-rohan-personal` | user | A single end user, Rohan, who filed a DPDP (India) data-deletion request. |

- **[PASS]** Gateway is healthy — HTTP 200

## Step 1 — Pull every source into one private store

Ingested **20/20** records across **6** scopes, **8** source types, languages: {'en': 17, 'hi': 3}.

- **[PASS]** All business records ingested — 20/20

## Step 2 — Recall in the local language (and across languages)

**Q [English] (tenant-acme-renewal):** Acme renewal health  
_Surface the renewal-risk drivers across CRM + QBR + Jira._
> CRM: Acme Manufacturing renewal of 1.2 crore INR due in 45 days. Health score dropped from 82 to 51 this quarter. Risk flagged: usage down 40%, and the exec sponsor (CTO) left the company last month.

- **[PASS]** Recall [English] 'Acme renewal health' — 1 hits, matched ['health score', 'usage down', 'sponsor']
**Q [English] (tenant-acme-renewal):** save plan Okta SSO  
_Find the save-plan mechanics._
> Slack #cs-acme: 'The save plan hinges on shipping Okta SSO and getting the new VP Eng to sponsor. If we land both, renewal probability goes from 35% to ~70%.'

- **[PASS]** Recall [English] 'save plan Okta SSO' — 1 hits, matched ['Okta', 'SSO', '70%', 'save plan']
**Q [Hindi] (support-escalations):** लोड  
_Cross-language recall: surface the Hindi support thread about dashboard latency._
> Zendesk टिकट: 'डैशबोर्ड लोड होने में बहुत समय लग रहा है जब हम 90 दिनों का डेटा देखते हैं।' समाधान: क्वेरी को ऑप्टिमाइज़ किया और कैशिंग चालू की; अब लोड समय 8 सेकंड से घटकर 2 सेकंड हो गया।

- **[PASS]** Recall [Hindi] 'लोड' — 1 hits, matched ['लोड', 'कैशिंग', '2', '8']
**Q [English] (churn-risk-signals):** champion departure  
_Find the cross-account churn pattern._
> Slack #cs-leadership: 'Pattern across at-risk accounts: a champion leaves, usage craters, then renewal stalls. We need a champion-departure playbook that triggers an exec review automatically.'

- **[PASS]** Recall [English] 'champion departure' — 1 hits, matched ['champion', 'exec review', 'usage']

## Step 3 — Scope isolation (no cross-compartment leakage)

- **[PASS]** Control: 'Globex' retrievable in home scope `tenant-globex-onboarding` — HTTP 200, 3 hit(s)
- **[PASS]** Isolation: 'Globex' does NOT leak into `tenant-acme-renewal` — HTTP 200, 0 hit(s) (want 0)
- **[PASS]** Control: 'Acme' retrievable in home scope `tenant-acme-renewal` — HTTP 200, 3 hit(s)
- **[PASS]** Isolation: 'Acme' does NOT leak into `tenant-globex-onboarding` — HTTP 200, 0 hit(s) (want 0)

## Step 4 — Synthesise a briefing with the on-device model

**Business question:** Why is the Acme renewal at risk, and what is the save plan?

The model is given **5** evidence record(s) from `tenant-acme-renewal` and asked for a JSON briefing.

- **[PASS]** Synthesis ran against the live model for `tenant-acme-renewal` — HTTP 202, recap chars=1786
**Actual model output — recap written to channel memory:**

> The company is undergoing a renewal of 1.2 crore INR due in 45 days. The health score dropped from 82 to 51 this quarter. Risk flagged: usage down 40%, and the exec sponsor (CTO) left the company last month. — seeing ROI — half my team doesn't log in.' Action items: a 30-day enablement sprint, an exec business review, and a usage-bas CRM: Acme Manufacturing renewal of 1.2 crore INR due in 45 days. Health score dropp a 30-day enablement sprint, an exec business review, and a usage-based success plan tied to two pipelines. , SSO delivery date, and an exec review with our CRO. Goal: demonstrate ROI before the renewal date. getting the new VP Eng to sponsor. If we land both, renewal probability goes from 35% to ~70%.' a custom anomaly threshold per pipeline, and a Slack alert integration. Engineering committed SSO for next sprint. maly threshold per pipeline, and a Slack alert integration. Engineering committed SSO for next sprint. QBR transcript: Acme's new VP Eng said 'we're not seeing ROI — half my tea ROI — half my team doesn't log in.' Action items: a 30-day enablement sprint, an exec business review, and a usage-based success business review, and a usage-based success plan tied to two pipelines. Email to Acme VP Eng: proposing a joint success plan — weekly enablement, SSO delivery date ure gaps blocking adoption — SSO via Okta, a custom anomaly threshold per pipeline, and a Slack alert integration. Engineering co ld per pipeline, and a Slack alert integration. Engineering committed SSO for next sprint. log in.' Action items: a 30-day enablement sprint, an exec business review, and a usage-based success plan tied to two pipeline ng: proposing a joint success plan — weekly enablement, SSO delivery date, and an exec review with our CRO. Goal: demonstrate RO

_Business-term coverage: matched 9/10 expected terms (['acme', 'renewal', 'risk', 'sso', 'okta', 'sponsor', 'usage', 'health', 'roi'])._

**Actual model output — full structured bundle (replaying the production `SynthSummary` prompt + grammar under the deterministic sampling preset):**

_Sampling: fixed seed=0, temperature=0.0 (greedy), top_k=1. First-attempt budget n_predict=880 (adaptive to 5 rows)._

_Verify-and-retry engaged: the first attempt tripped a low-quality signal ({'recap_chars': 697, 'meta_commentary': True, 'too_short': False, 'exemplar_leak': False, 'list_exemplar_leak': False}); retried once at n_predict=1264 with the fact-only suffix; kept attempt #1._

```json
{
  "recap": "The session discusses the proposal for a joint success plan with weekly enablement, SSO delivery date, and an exec review with our CRO. The goal is to demonstrate ROI before the renewal date. This is critical because it enables us to get the new VP Eng to sponsor. If we land both, renewal probability goes from 35% to ~70%. We need to ensure that the new VP Eng can deliver on this proposal and get them to sponsor. The goal is to demonstrate ROI before the renewal date. This is critical because it enables us to get the new VP Eng to sponsor. If we land both, renewal probability goes from 35% to ~70%. We need to ensure that the new VP Eng can deliver on this proposal and get them to sponsor. — seeing ROI — half my team doesn't log in.' Action items: a 30-day enablement sprint, an exec business review, and a usage-bas CRM: Acme Manufacturing renewal of 1.2 crore INR due in 45 days. Health score dropped from 82 to 5 CRM: Acme Manufacturing renewal of 1.2 crore INR due in 45 days. Health score dropp a custom anomaly threshold per pipeline, and a Slack alert integration. Engineering committed SSO for next sprint. maly threshold per pipeline, and a Slack alert integration. Engineering committed SSO for next sprint. a 30-day enablement sprint, an exec business review, and a usage-based success plan tied to two pipelines. QBR transcript: Acme's new VP Eng said 'we're not seeing ROI — half my tea Email to Acme VP Eng: proposing a joint success plan — weekly enablement, SSO delivery date ure gaps blocking adoption — SSO via Okta, a custom anomaly threshold per pipeline, and a Slack alert integration. Engineering co ld per pipeline, and a Slack alert integration. Engineering committed SSO for next sprint. business review, and a usage-based success plan tied to two pipelines. Jira: Acme has 3 open feature gaps blocking adoption — SSO via Okta, a custom anomaly threshold per pip Jira: Acme has 3 open feature gaps blocking adoption — SSO via Okta, a custom anomaly thr
```

- **[PASS]** Synthesis is byte-reproducible across runs (fixed seed) — 2 runs, identical=True, 1827 chars

## Step 5 — Cryptographic right to be forgotten

> Rohan filed a DPDP deletion request; the scope DEK is destroyed so his data is unrecoverable, while other tenants are untouched.

Before erase: **1** record(s); after erase: **0** record(s).

- **[PASS]** Deletion request accepted — HTTP 204
- **[PASS]** Data is unrecoverable after key destruction — HTTP 200→200, 1→0 records

## Result — 14/14 checks passed
