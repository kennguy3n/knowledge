//! Concept graph traversal benchmarks.
//!
//! Measures BFS/DFS traversal performance over graphs with 10K and
//! 100K nodes using typed-edge traversal.
//!
//! Run with:
//!
//! ```bash
//! cargo bench -p integration_tests --bench graph_bench
//! ```

use std::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::hint::black_box;

use concept_graph::{ConceptEdge, ConceptGraph, ConceptNode, NodeId, RelationType};
use evidence_store::ScopeId;

/// Graph sizes to benchmark.
const GRAPH_SIZES: &[(&str, usize)] = &[("10K_nodes", 10_000), ("100K_nodes", 100_000)];

/// Build a graph with `n` nodes connected in a chain via `IsA`
/// relations, plus random cross-links via `PartOf`.
fn build_chain_graph(n: usize) -> (ConceptGraph, Vec<NodeId>) {
    let scope = ScopeId::new_v4();
    let mut graph = ConceptGraph::new();
    let mut ids = Vec::with_capacity(n);

    for i in 0..n {
        let node = ConceptNode::new_candidate(format!("n{i}"), format!("def {i}"), scope);
        ids.push(graph.add_node(node).unwrap());
    }

    // Chain edges: each node -> next via IsA (creates a linear path).
    for i in 0..n.saturating_sub(1) {
        let edge = ConceptEdge::new(ids[i], ids[i + 1], RelationType::IsA, scope);
        graph.add_edge(edge).unwrap();
    }

    // Cross-links: every 10th node links to a node 100 ahead via PartOf.
    for i in (0..n).step_by(10) {
        let target = (i + 100) % n;
        if target != i {
            let edge = ConceptEdge::new(ids[i], ids[target], RelationType::PartOf, scope);
            graph.add_edge(edge).unwrap();
        }
    }

    (graph, ids)
}

/// Build a wide tree graph (branching factor ~10) for BFS benchmarks.
fn build_tree_graph(n: usize) -> (ConceptGraph, Vec<NodeId>) {
    let scope = ScopeId::new_v4();
    let mut graph = ConceptGraph::new();
    let mut ids = Vec::with_capacity(n);

    for i in 0..n {
        let node = ConceptNode::new_candidate(format!("t{i}"), format!("tree {i}"), scope);
        ids.push(graph.add_node(node).unwrap());
    }

    // Build a tree: each node i has parent at i/10.
    for i in 1..n {
        let parent = i / 10;
        let edge = ConceptEdge::new(ids[parent], ids[i], RelationType::IsA, scope);
        graph.add_edge(edge).unwrap();
    }

    (graph, ids)
}

fn bench_typed_traversal_chain(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph/traverse_typed_chain");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(15));

    for &(label, size) in GRAPH_SIZES {
        let (graph, ids) = build_chain_graph(size);
        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(BenchmarkId::new("IsA_unbounded", label), &(), |b, _| {
            b.iter(|| {
                let reachable = graph.traverse_typed(ids[0], RelationType::IsA, None);
                black_box(reachable.len());
            });
        });
    }
    group.finish();
}

fn bench_typed_traversal_bounded(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph/traverse_typed_bounded");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(10));

    for &(label, size) in GRAPH_SIZES {
        let (graph, ids) = build_chain_graph(size);
        // Bounded traversal at depth 5, 50, 500.
        for max_depth in [5usize, 50, 500] {
            let bench_label = format!("{label}/depth_{max_depth}");
            group.bench_with_input(BenchmarkId::new("IsA", &bench_label), &(), |b, _| {
                b.iter(|| {
                    let reachable =
                        graph.traverse_typed(ids[0], RelationType::IsA, Some(max_depth));
                    black_box(reachable.len());
                });
            });
        }
    }
    group.finish();
}

fn bench_tree_bfs(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph/tree_bfs");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(15));

    for &(label, size) in GRAPH_SIZES {
        let (graph, ids) = build_tree_graph(size);
        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(BenchmarkId::new("full_traversal", label), &(), |b, _| {
            b.iter(|| {
                let reachable = graph.traverse_typed(ids[0], RelationType::IsA, None);
                black_box(reachable.len());
            });
        });
    }
    group.finish();
}

fn bench_neighbors_lookup(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph/neighbors");
    group.sample_size(50);
    group.measurement_time(Duration::from_secs(10));

    let (graph, ids) = build_chain_graph(100_000);
    group.throughput(Throughput::Elements(1000));
    group.bench_function("1000_lookups_in_100K_graph", |b| {
        b.iter(|| {
            let mut total = 0usize;
            for i in (0..100_000).step_by(100) {
                let n = graph.neighbors(ids[i], Some(RelationType::IsA));
                total = total.saturating_add(n.len());
            }
            black_box(total);
        });
    });
    group.finish();
}

fn bench_node_add_remove(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph/mutation");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(10));

    let scope = ScopeId::new_v4();
    group.throughput(Throughput::Elements(10_000));
    group.bench_function("add_10K_nodes", |b| {
        b.iter(|| {
            let mut graph = ConceptGraph::new();
            for i in 0..10_000 {
                let node = ConceptNode::new_candidate(format!("m{i}"), format!("mut {i}"), scope);
                graph.add_node(node).unwrap();
            }
            black_box(graph.node_count());
        });
    });
    group.finish();
}

criterion_group!(
    graph_benches,
    bench_typed_traversal_chain,
    bench_typed_traversal_bounded,
    bench_tree_bfs,
    bench_neighbors_lookup,
    bench_node_add_remove,
);
criterion_main!(graph_benches);
