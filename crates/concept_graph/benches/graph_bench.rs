//! Criterion benchmarks for `concept_graph` — in-memory mutation,
//! typed-edge traversal, and SQLCipher round-trip.
//!
//! Run with:
//!
//! ```bash
//! cargo bench -p concept_graph
//! cargo bench -p concept_graph -- traversal
//! ```

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use tempfile::TempDir;

use concept_graph::{
    ConceptEdge, ConceptGraph, ConceptNode, NodeId, PersistentConceptGraph, RelationType,
};
use crypto::{MasterKey, MASTER_KEY_LEN};
use evidence_store::ScopeId;

/// Number of nodes inserted by the `add_node` bench.
const ADD_NODE_COUNT: usize = 10_000;
/// Number of nodes in the chain traversal bench.
const TRAVERSAL_NODE_COUNT: usize = 10_000;
/// Number of nodes round-tripped through SQLCipher persistence.
const PERSIST_NODE_COUNT: usize = 1_000;

fn fixture_master_key() -> MasterKey {
    let mut k = [0u8; MASTER_KEY_LEN];
    for (i, byte) in k.iter_mut().enumerate() {
        // `i` is bounded by MASTER_KEY_LEN (= 32).
        *byte = u8::try_from(i)
            .unwrap_or(0xA1)
            .wrapping_mul(31)
            .wrapping_add(7);
    }
    k
}

fn bench_add_node(c: &mut Criterion) {
    let scope = ScopeId::new_v4();
    c.bench_function("concept_graph/add_node/10k", |b| {
        b.iter_with_setup(ConceptGraph::new, |mut g| {
            for i in 0..ADD_NODE_COUNT {
                let n = ConceptNode::new_candidate(format!("bench-node-{i}"), "definition", scope);
                g.add_node(black_box(n))
                    .expect("add_node must not fail with fresh ids");
            }
            black_box(g);
        });
    });
}

fn bench_traversal(c: &mut Criterion) {
    let scope = ScopeId::new_v4();

    // Build a chain a -> b -> c -> ... -> z of length TRAVERSAL_NODE_COUNT
    // linked by IsA edges, plus a couple of PartOf branches off every
    // node so `neighbors(_, None)` is non-trivial.
    let mut g = ConceptGraph::new();
    let mut ids: Vec<NodeId> = Vec::with_capacity(TRAVERSAL_NODE_COUNT);
    for i in 0..TRAVERSAL_NODE_COUNT {
        let n = ConceptNode::new_candidate(format!("n-{i}"), "definition", scope);
        ids.push(g.add_node(n).expect("add_node"));
    }
    for w in ids.windows(2) {
        g.add_edge(ConceptEdge::new(w[0], w[1], RelationType::IsA, scope))
            .expect("add IsA edge");
    }
    // Add a PartOf branch on every other node — small enough to be
    // realistic, large enough that `neighbors(_, None)` actually has
    // multiple candidates. `i % 2 == 0` rather than `is_multiple_of`
    // because the workspace MSRV (1.85) predates stabilization of
    // `usize::is_multiple_of`.
    for (i, &id) in ids.iter().enumerate().skip(1) {
        if i % 2 == 0 {
            let leaf = g
                .add_node(ConceptNode::new_candidate(
                    format!("leaf-{i}"),
                    "leaf def",
                    scope,
                ))
                .expect("add leaf");
            g.add_edge(ConceptEdge::new(id, leaf, RelationType::PartOf, scope))
                .expect("add PartOf");
        }
    }

    let root = ids[0];
    c.bench_function("concept_graph/traversal/typed_chain_10k", |b| {
        b.iter(|| {
            // `traverse_typed` walks the IsA edges only — bounded by
            // the chain depth so it is independent of leaves. No
            // depth bound (None) so the whole chain is enumerated.
            let reached = g.traverse_typed(black_box(root), RelationType::IsA, None);
            black_box(reached);
        });
    });
    c.bench_function("concept_graph/traversal/neighbors_all_relations", |b| {
        b.iter(|| {
            // Hit every node in the chain and ask for all-relation
            // neighbors. Exercises the per-node edge bucket lookup.
            let mut total: usize = 0;
            for &id in &ids {
                total = total.saturating_add(g.neighbors(black_box(id), None).len());
            }
            black_box(total);
        });
    });
}

fn bench_persist_load(c: &mut Criterion) {
    let scope = ScopeId::new_v4();
    let key = fixture_master_key();

    let mut group = c.benchmark_group("concept_graph/persist");

    group.bench_function("write_1000_nodes", |b| {
        b.iter_with_setup(
            || {
                let dir = TempDir::new().expect("tempdir");
                let path = dir.path().join("concepts.db");
                let g = PersistentConceptGraph::open(&path, &key).expect("open");
                (dir, g)
            },
            |(_dir, mut g)| {
                for i in 0..PERSIST_NODE_COUNT {
                    let n = ConceptNode::new_candidate(
                        format!("persist-node-{i}"),
                        "definition",
                        scope,
                    );
                    g.add_node(black_box(n)).expect("add_node + persist");
                }
                black_box(g);
            },
        );
    });

    group.bench_function("load_scope_1000_nodes", |b| {
        // Build the database once outside the timed loop, then bench
        // the reopen + rehydrate cost.
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("concepts.db");
        {
            let mut g = PersistentConceptGraph::open(&path, &key).expect("open");
            for i in 0..PERSIST_NODE_COUNT {
                g.add_node(ConceptNode::new_candidate(
                    format!("persist-node-{i}"),
                    "definition",
                    scope,
                ))
                .expect("add_node + persist");
            }
        }

        b.iter(|| {
            let mut g = PersistentConceptGraph::open(&path, &key).expect("reopen");
            let (n, e) = g.load_scope(black_box(scope)).expect("load_scope");
            black_box((n, e));
        });
    });

    group.finish();
}

criterion_group!(
    graph_benches,
    bench_add_node,
    bench_traversal,
    bench_persist_load,
);
criterion_main!(graph_benches);
