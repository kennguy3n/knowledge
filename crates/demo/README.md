# demo

End-to-end demonstration binary for the Knowledge substrate.

## Purpose

Drives a synthetic multi-scope dataset through every public substrate
API — evidence ingestion, observation extraction, memory management,
the concept graph, the synthesis pipeline, permissions, crypto,
export, the agent contract, reasoning, connectors, and audit.
Writes a reconciled markdown report to `results/demo_results.md`.

## Running

```bash
cargo run -p demo --release
```

## Notes

- This is a **binary** crate (`src/main.rs`); it does not export a
  library.
- An in-tree integration test re-runs the binary to pin the public
  contract.
- CI runs this as part of the test suite.

## Links

- [README.md](../../README.md) §Quick start — Demo section.
- [docs/INTEGRATION_GUIDE.md](../../docs/INTEGRATION_GUIDE.md) — Consumer integration guide.
