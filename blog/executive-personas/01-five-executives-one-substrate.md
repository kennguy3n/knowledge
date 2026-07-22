# Five Executives, One Substrate

> **TL;DR:** A CFO at month-end has the answer to "where does the
> CartoNord dispute stand?" scattered across six tools. Knowledge pulls
> all of it into private, per-topic encrypted compartments and answers
> the question in one place — on-device, in her language. This post uses
> Élise's real run to explain how the system is put together.

## The problem, concretely

Élise Moreau is the CFO of Atelier Verdoyant, a 60-person
sustainable-packaging manufacturer in Lyon. It is month-end close, and
three things are happening at once:

- A key supplier, **CartoNord**, delivered defective board stock and is
  disputing a credit note while a **90,000 EUR** invoice sits overdue.
- The **statutory auditor** has started fieldwork.
- The **board pack** is due Friday.

Her institutional knowledge for answering "what do we actually know
about the CartoNord dispute, and where does the close stand?" lives in
Qonto (banking), Pennylane (accounting), PayFit (payroll), GoCardless
(SEPA), email, Slack and shared docs. Six tools, no single view.

## Scopes: private compartments, not folders

The substrate's organising primitive is the **scope** — an encrypted
compartment with its own data-encryption key. A scope is not a folder;
it is a cryptographic boundary. Élise's world maps to six of them:

| Scope | Tier | What it holds |
| --- | --- | --- |
| `finance-month-end` | channel | Accruals, reconciliations, the close checklist |
| `supplier-cartonord` | channel | Defective stock, the disputed credit note, the overdue invoice |
| `audit-2025` | channel | Auditor PBC list, confirmations, findings |
| `treasury-cashflow` | channel | Qonto balances, GoCardless SEPA, 13-week forecast |
| `board-reporting` | domain | Board pack, runway, quarterly narrative |
| `customer-bonjourbio` | user | One customer's data, subject to an RGPD erasure request |

The three **tiers** — `user`, `channel`, `domain` — are the same ladder
that lets one architecture serve both a single consumer and a
multi-tenant enterprise (see the capstone post,
[The AI Privacy Spectrum](../24-the-ai-privacy-spectrum.md)). For Élise
they simply mean: personal-and-erasable, team-shared, and
organisation-wide.

The reference UI lists each scope as an isolated conversation:

![The Conversations view: every scope is its own encrypted compartment. The reference UI is seeded with three of the five personas — Élise, Sofía and Lena.](assets/01-conversations-grid.png)

## Step 1 — Unified ingest

The first promise is that scattered data lands in one private store. In
Élise's run:

```
Ingested 27/27 records across 6 scopes, 9 source types,
languages: {'en': 5, 'fr': 22}.
✓ All business records ingested — 27/27
```

Each record is ingested as **evidence** — banking lines, accounting
journals, emails, Slack messages, shared-doc excerpts — tagged with a
source and an importance. Nothing leaves the device; ingest is a
`POST /api/v1/ingest` to the local gateway, which hands the plaintext to
the Rust substrate for per-scope encryption.

In the UI, sending a message *is* an ingest. Here Élise drops two
month-end notes into `supplier-cartonord`, and the right-hand panel
shows the briefing the system already holds for that scope:

![Chat view of `supplier-cartonord`: the panel on the right shows the synthesized briefing the system already holds for that scope.](assets/02-chat-recap-fr.png)

## Step 2 — Recall, in her language

With everything in one store, Élise asks questions in French and gets
ranked answers. These are verbatim from the run:

> **Q [French] (`supplier-cartonord`):** `facture CartoNord avoir`
>
> → *Pennylane : la facture fournisseur CartoNord FA-2025-0411 d'un
> montant de 90 000 EUR est échue depuis 15 jours. Paiement bloqué en
> attendant la résolution du litige sur l'avoir de 12 600 EUR.*
>
> ✓ matched `['90 000', 'FA-2025-0411', '12 600', 'avoir']`

One query ties the overdue invoice to the disputed credit note — the
exact linkage she needed, pulled from the accounting connector's data
without opening Pennylane. The recall mechanics (hybrid full-text +
semantic, and how seven languages are handled) are the subject of
[post 2](02-multilingual-recall.md).

## Step 3 — Isolation is real, not cosmetic

Because each scope is its own encrypted compartment, a term in one scope
cannot surface in another. The run proves this directly:

```
✓ Control:  'BonjourBio' retrievable in home scope customer-bonjourbio — 3 hits
✓ Isolation:'BonjourBio' does NOT leak into supplier-cartonord       — 0 hits (want 0)
```

This is what makes the `user` tier safe for an RGPD/LGPD/DSGVO subject:
their data lives in a compartment that other scopes physically cannot
read.

## Step 4 — Synthesis, on-device

Finally, Élise asks the system to *condense* a scope into a briefing.
The gateway gathers the scope's evidence and asks the on-device model
(Qwen3.5-2B, served by `llama-server`) for a structured summary. Her
result:

> **Actual model output — recap written to channel memory (closing
> line):**
>
> *Email to CartoNord (English): 'We will release payment of the 90,000
> EUR invoice FA-2025-0411 only once a credit note of 12,600 EUR for the
> non-conforming BR-2505 lot is issued. Your 6,000 EUR offer does not
> cover our verified quarantine and re-purchase costs.'*

That is a genuinely useful negotiating position, written by a Qwen3.5-2B model
running on CPU — and the deterministic pipeline now produces it
byte-for-byte on every run. It is also not always this good: the recap
that precedes this closing line restates the blocked-payment point twice,
the honest verbosity that [post 3](03-synthesis-quality.md) takes as its
subject.

## Step 5 — The right to be forgotten

`customer-bonjourbio` exists to be deleted. When the erasure request
comes, the substrate destroys the scope's data-encryption key:

```
✓ Deletion request accepted — HTTP 204
✓ Data unrecoverable after key destruction — 2 → 0 records
```

No key, no plaintext, ever again. Cryptographic erasure is the
mechanism behind [Memory That Forgets](../03-memory-that-forgets.md).

## The whole loop, in one persona

Ingest → recall → isolation → synthesis → forget. Six tools collapsed
into one private store; the month-end question answered in French;
nothing leaving the device. The next three posts widen the lens to the
other four executives, scrutinise recall across ten languages and four
script families, and hold the model's output up to honest light.

**Result for Élise: 12/12 business checks passed.**
