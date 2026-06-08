# memory_manager

Decay state machine, retention scoring, working memory, and user
memory CRUD for the Knowledge substrate.

## Purpose

The on-device authority for the lifecycle of every `MemoryObject`:
decay transitions, retention scoring, pinning/unpinning/forgetting,
and working-memory context windows with TTL eviction. Also enforces
the privacy-strip invariant — every synthesis output must carry a
`PrivacyStrip` describing its compute location, model, and egress.

## Public API summary

| Type / Function | Description |
|---|---|
| `MemoryObject` / `MemoryState` | Core memory model and state. |
| `MemoryStateMachine` | Decay state transitions. |
| `UserMemoryObject` | Per-user memory with CRUD. |
| `ChannelMemoryObject` | Channel-level memory (decisions, tasks, open questions). |
| `DomainMemoryObject` | Domain-level memory (workstreams, risks, procedures). |
| `TenantMemoryObject` | Tenant-level memory (policies, approved docs, taxonomy). |
| `compute_retention_score` | Retention scoring function. |
| `decay_sweep` | Batch decay sweep across objects. |
| `PrivacyStrip` / `SynthesisOutput` | Privacy-strip invariant types. |
| `WorkingMemory` | Context window with TTL eviction. |

## Links

- [ARCHITECTURE.md](../../docs/technical/architecture.md) §2.1, §7 — Memory manager, decay state machine.
- [docs/technical/design.md](../../docs/technical/design.md) §4 — Memory model.
- [docs/getting-started/for-developers.md](../../docs/getting-started/for-developers.md) — Consumer integration guide.
