//! `bench_concept_graph_traversal` — typed BFS at 100K nodes.
//!
//! Builds a 100K-node concept graph as a balanced tree with typed
//! `IsA` edges at two branching factors (fan-out 10 and 100), then
//! times bounded `traverse_typed` BFS at depths 1, 3, and 5 from the
//! root.
//!
//! Run with:
//!
//! ```bash
//! cargo bench -p benchmarks --bench bench_concept_graph_traversal
//! ```

use std::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use std::hint::black_box;

use concept_graph::{ConceptEdge, ConceptGraph, ConceptNode, NodeId, RelationType};
use evidence_store::ScopeId;

const NODE_COUNT: usize = 100_000;
const FAN_OUTS: &[usize] = &[10, 100];
const DEPTHS: &[usize] = &[1, 3, 5];

/// Build an `n`-node tree where node `i`'s parent is `i / fan_out`,
/// giving a balanced tree with branching factor `fan_out`, wired with
/// typed `IsA` edges.
fn build_tree(n: usize, fan_out: usize) -> (ConceptGraph, Vec<NodeId>) {
    let scope = ScopeId::new_v4();
    let mut graph = ConceptGraph::new();
    let mut ids = Vec::with_capacity(n);
    for i in 0..n {
        let node = ConceptNode::new_candidate(format!("t{i}"), format!("def {i}"), scope);
        ids.push(graph.add_node(node).expect("add_node"));
    }
    for i in 1..n {
        let parent = i / fan_out;
        let edge = ConceptEdge::new(ids[parent], ids[i], RelationType::IsA, scope);
        graph.add_edge(edge).expect("add_edge");
    }
    (graph, ids)
}

fn bench_concept_graph_traversal(c: &mut Criterion) {
    let mut group = c.benchmark_group("concept_graph/bfs_100k");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(12));

    for &fan_out in FAN_OUTS {
        let (graph, ids) = build_tree(NODE_COUNT, fan_out);
        let root = ids[0];
        for &depth in DEPTHS {
            let label = format!("fanout_{fan_out}/depth_{depth}");
            group.bench_with_input(BenchmarkId::from_parameter(&label), &(), |b, ()| {
                b.iter(|| {
                    let reachable =
                        graph.traverse_typed(black_box(root), RelationType::IsA, Some(depth));
                    black_box(reachable.len());
                });
            });
        }
    }
    group.finish();
}

criterion_group!(graph_benches, bench_concept_graph_traversal);
criterion_main!(graph_benches);
