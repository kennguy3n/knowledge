# The UI, and What It Honestly Reveals

> **TL;DR:** Before capturing a single screenshot we audited the
> reference UI against polished products (Linear, Vercel, Notion),
> found it competent but monotone, and did a root-cause design pass: a
> non-monotone palette, a real accent system, depth, and proper
> interactive/empty states. The audit also surfaced two things a
> screenshot can't hide — one real bug (now fixed) and one honest
> product gap.

A blog series that shows the product is also a forcing function for
looking at the product honestly. Two requirements drove this post:
*fix anything broken before capturing artifacts*, and *make the UI
pleasing, not a single monotone*. Both turned up real work.

## The audit: competent but monotone

The reference UI was structurally fine — clear layout, sensible
information architecture — but visually flat. Every surface was a
near-identical blue-grey, hierarchy came only from font size, buttons
had no states, and there was no depth. Those are the "amateur tells"
that separate a dev tool from a product: not bugs, but the absence of a
deliberate visual system.

The fix was at the **design-token** level, not a per-component reskin.
The whole theme is defined once in
[`apps/knowledge-ui/src/app/globals.css`](../../apps/knowledge-ui/src/app/globals.css):

```css
:root, [data-theme='dark'] {
  /* Layered neutrals carry a faint indigo cast so the surface stack
     reads as intentional depth rather than monotone. */
  --bg: #0b0e16; --bg-elev: #141925; --bg-elev-2: #1c2230;
  --accent: #6e8bff; --accent-2: #c084fc;        /* dual accent */
  --ok: #3fd07f; --warn: #f0b429; --bad: #ff6b66; --info: #34d3e0;
  --grad-brand: linear-gradient(135deg, #6e8bff 0%, #c084fc 100%);
  --shadow-md: 0 6px 20px -6px rgba(0,0,0,0.55);
  --shadow-accent: 0 4px 14px -4px rgba(90,120,245,0.5);
}
```

From those tokens: the brand mark and primary buttons became a
blue→violet gradient; the active nav item gained an accent tint and an
inset bar; cards and search results gained shadow and a hover lift;
inputs gained an accent focus ring; and the synthesis recap got a
gradient-bordered card so the model's output reads as a distinct
artifact. The result is the same UI, now with a coherent, non-monotone
visual language:

![The Conversations grid after the design pass: gradient brand mark, accent-tinted active nav, card depth.](assets/01-conversations-grid.png)

The decay state machine is a good example of using **semantic** colour
to carry meaning instead of decoration — Candidate (amber), Reinforced
(green), Decaying (amber), Archived (red), Pinned (blue):

![The Memory page: gradient-bordered briefing, semantically coloured decay nodes, honest empty states.](assets/04-memory-cartonord.png)

Light mode and the settings surface got the same token treatment:

![Settings: token-driven controls, active theme toggle, gateway health check.](assets/05-settings.png)

## The bug the audit caught

The chat view has a right-hand "Synthesized memory" panel with a
**Synthesize now** button. Auditing it revealed a real defect: after
triggering synthesis, the panel still said *"No memory yet for this
scope."* The user would run synthesis and see nothing.

Root cause: the panel fetched only `listMemories()` — the *user-memory*
surface — while the briefing that synthesis actually produces is the
*channel recap*, exposed at `GET /api/v1/memories/channel`. The Memory
page read the channel recap; the chat panel didn't. So the two screens
disagreed about whether a scope had any memory.

The fix is to make the chat panel reflect both surfaces, fetched
together, so a freshly synthesised briefing appears where the user
triggered it:

```tsx
// ChatView.tsx — fetch the channel recap alongside user memories
const [recapRow, rows] = await Promise.all([
  channelMemory(scopeId, signal),
  listMemories(scopeId, { limit: 50 }, signal),
]);
setRecap(recapRow);
setMemories(rows);
```

Now the panel shows the real French briefing the moment it exists:

![Chat view after the fix: ingested French notes on the left, the live synthesised briefing in the panel on the right.](assets/02-chat-recap-fr.png)

This is the kind of bug that only an honest audit catches — everything
"worked," the API returned 200s, and the screen was simply reading the
wrong source. It is fixed at the root (the panel now models the two
distinct memory surfaces) rather than papered over.

## The gap the UI can't hide

The Memory page has three sections below the briefing — **Decay state
machine**, **Concept graph**, and **Memories** — and across every
persona they render empty: `0 Candidate / 0 Reinforced / …`, *"No
concepts to graph,"* *"Memories (0)."*

This is not a rendering bug. It is an honest reflection of the current
product state: the **user-memory** subsystem (the decay machine, the
concept graph, the per-item memory list) has **no public write path
through the gateway**. Only **channel memory** — the synthesis briefing
— is populated end-to-end. So those sections are correctly showing that
there is nothing to show.

We made a deliberate choice here: **show the honest empty state, not
fake data.** A dashboard full of invented decay nodes would demo better
and lie. The empty states instead point at the real next piece of work —
wiring a public ingest path for user-memory so the decay lifecycle
([Memory That Forgets](../03-memory-that-forgets.md)) and the concept
graph have data to operate on. The capability exists in the substrate;
it isn't yet reachable from the gateway.

## What "audit before artifacts" actually bought

The instruction to verify the UI and logs before capturing screenshots
wasn't ceremony. It produced:

- a **design system** that makes the product look like a product;
- a **fixed bug** where synthesis results were invisible in the chat
  panel; and
- a clear-eyed **product gap** documented honestly rather than hidden
  behind seed data.

The browser console is clean across every screen, the captured
screenshots are real, and the one thing the UI couldn't do — show
user-memory — is named instead of faked. That is the version of "polish"
worth shipping.
