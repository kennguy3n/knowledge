# Anand Iyer — VP of Customer Success
_Dhruva Cloud · Bengaluru, India · languages: English, Hindi_

_Run at 2026-06-12T01:42:04.820970+00:00 against `http://127.0.0.1:8080`._

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

- **[PASS]** Synthesis ran against the live model for `tenant-acme-renewal` — HTTP 202, recap chars=332
**Actual model output — recap written to channel memory:**

> Acme's new VP Eng proposed a joint success plan with CRM and Jira, aiming to demonstrate ROI before the renewal date. The company faces three open feature gaps that need to be addressed: SSO via Okta, custom anomaly threshold per pipeline, and Slack alert integration. Engineering has committed to shipping Okta SSO for next sprint.

_Business-term coverage: matched 5/10 expected terms (['acme', 'renewal', 'sso', 'okta', 'roi'])._

**Actual model output — full structured bundle (replaying the production `SynthSummary` prompt + grammar under the deterministic sampling preset):**

_Sampling: fixed seed=0, temperature=0.0 (greedy), top_k=1. First-attempt budget n_predict=632 (adaptive to 5 rows)._

_Verify-and-retry: first attempt passed the quality gate ({'recap_chars': 106, 'meta_commentary': False, 'too_short': False}); no retry needed._

```json
{
  "recap": "Proposed joint success plan for Acme with VP Eng, focusing on SSO delivery date and executive sponsorship.",
  "decisions": [
    "Propose joint success plan for Acme with VP Eng"
  ],
  "open_questions": [
    "What is the ROI timeline?"
  ],
  "active_tasks": [
    "Schedule enablement sprint",
    "Prepare exec business review",
    "Develop usage-based success plan"
  ]
}
```

- **[PASS]** Synthesis is byte-reproducible across runs (fixed seed) — 2 runs, identical=True, 352 chars

## Step 5 — Cryptographic right to be forgotten

> Rohan filed a DPDP deletion request; the scope DEK is destroyed so his data is unrecoverable, while other tenants are untouched.

Before erase: **1** record(s); after erase: **0** record(s).

- **[PASS]** Deletion request accepted — HTTP 204
- **[PASS]** Data is unrecoverable after key destruction — HTTP 200→200, 1→0 records

## Result — 14/14 checks passed
