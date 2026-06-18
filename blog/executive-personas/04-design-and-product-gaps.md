# The UI, and What It Honestly Reveals

> **TL;DR:** The reference UI is built on a deliberate design system —
> audited against polished products (Linear, Vercel, Notion), with a
> non-monotone palette, a real accent system, depth, and proper
> interactive/empty states. It is also an honest test of the product: the
> chat panel models the two distinct memory surfaces (channel recap and
> user-memory) so a freshly synthesised briefing appears where it was
> triggered, and the Memory page is populated end-to-end from a live
> user-memory write path, so the decay state machine, concept graph and
> per-item memory list render real data — shown below.

A blog series that shows the product is also a forcing function for
looking at the product honestly. Two standards drive this post:
*nothing broken ships into a screenshot*, and *the UI is a deliberate
visual system, not a single monotone*. Both are visible below.

## The design system: deliberate, not monotone

A dev tool and a product differ in their visual system. The reference UI
is structurally sound — clear layout, sensible information architecture —
and its visual language is deliberate rather than flat: a layered neutral
palette, a dual accent, depth, and real interactive states, rather than
hierarchy carried by font size alone.

The design lives at the **design-token** level, not in per-component
reskins. The whole theme is defined once in
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

From those tokens: the brand mark and primary buttons are a blue→violet
gradient; the active nav item carries an accent tint and an inset bar;
cards and search results have shadow and a hover lift; inputs have an
accent focus ring; and the synthesis recap sits in a gradient-bordered
card so the model's output reads as a distinct artifact. The result is a
coherent, non-monotone visual language:

![The Conversations grid: gradient brand mark, accent-tinted active nav, card depth.](assets/01-conversations-grid.png)

The decay state machine is a good example of using **semantic** colour
to carry meaning instead of decoration — Candidate (amber), Reinforced
(green), Decaying (amber), Archived (red), Pinned (blue):

![The Memory page: gradient-bordered briefing, semantically coloured decay nodes, honest empty states.](assets/04-memory-cartonord.png)

Light mode and the settings surface use the same tokens:

![Settings: token-driven controls, active theme toggle, gateway health check.](assets/05-settings.png)

## The chat panel models both memory surfaces

The chat view has a right-hand "Synthesized memory" panel with a
**Synthesize now** button. The briefing that synthesis produces is the
*channel recap*, exposed at `GET /api/v1/memories/channel` — a distinct
surface from the *user-memory* list (`listMemories()`). The panel fetches
both, together, so a freshly synthesised briefing appears where the user
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

The panel shows the real briefing the moment it exists:

![Chat view: the live synthesised briefing appears in the right-hand panel as soon as synthesis produces it.](assets/02-chat-recap-fr.png)

Modelling the two distinct memory surfaces explicitly is what keeps the
chat panel and the Memory page in agreement about whether a scope has any
memory — both read the same sources, so the two screens never disagree.

## The Memory page is populated end-to-end

The Memory page has three sections below the briefing — **Decay state
machine**, **Concept graph**, and **Memories** — and they render from a
live **user-memory** write path. A public write route,
`POST /api/v1/memories`, is wired end-to-end (gateway → Go client →
substrate → the `add_user_memory` FFI), fail-closed and user-tier only,
and a read route, `GET /api/v1/memories/concept-graph`, projects the
per-scope concept graph from live user-memory. Against a scope with three
written observations, the page renders real data:

![The Memory page, populated: three user-memory observations in the decay state machine as Candidates, a live concept graph, and the per-item memory list.](assets/07-memory-page-populated.png)

The decay state machine shows **3 Candidate** observations, and the
concept graph projects a node per observation, coloured by lifecycle
state and sized by retention — the real Postgres-migration knowledge a
team wrote into the scope:

![The concept graph projected from live user-memory: one node per written observation, amber for the Candidate lifecycle state.](assets/06-concept-graph-populated.png)

The **Add a memory** form on the same page writes through that path:
an observation typed in becomes a `Candidate` in the decay machine and a
node in the graph immediately. The decay lifecycle
([Memory That Forgets](../03-memory-that-forgets.md)) has data to operate
on — and, as [post 3](03-synthesis-quality.md) shows, knowledge that
*recurs* across messages is promoted to **Reinforced** rather than
duplicated. The synthesis briefing for a cross-message roll-up is the
other half of the picture, and it too renders live:

![The synthesized briefing for a cross-message roll-up: six overlapping messages consolidated into one recap.](assets/08-rollup-briefing.png)

Channel memory (the synthesis briefing) and user-memory (the decay
machine, concept graph and per-item list) are **distinct surfaces** — the
distinction the chat panel models above — and both are populated
end-to-end, so the Memory page tells the whole truth.

## Honest by construction

The standard that nothing broken ships into a screenshot isn't ceremony —
it is what makes the captured artifacts trustworthy. The UI delivers:

- a **design system** that makes the product look like a product;
- a **chat panel** that models the two memory surfaces, so synthesis
  results are visible where they're triggered;
- a **Memory page** populated from a real write path rather than seed
  data or fixtures.

The browser console is clean across every screen, the captured
screenshots are real, and the user-memory surfaces render because the
capability behind them is built, not faked. That is the version of
"polish" worth shipping.
