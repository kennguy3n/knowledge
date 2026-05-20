//! GraphRAG-style community detection and summarisation.
//!
//! Per `docs/DESIGN.md` §11.1 (reasoning plane), the substrate
//! detects clusters ("communities") in the
//! sparse concept graph and pre-computes a structured summary for
//! each one. Subsequent queries are routed through a
//! [`CommunityQueryRouter`] that picks the most-relevant
//! community summaries as cheap context — the GraphRAG pattern
//! adapted to this substrate's scope-aware permission model.
//!
//! # Pipeline
//!
//! 1. [`CommunityDetector::detect`] — runs a connected-component
//!    scan over a scope-filtered subgraph and returns the leaf
//!    [`Community`] set (level 0).
//! 2. [`CommunityHierarchy::build`] — recursively merges leaf
//!    communities by shared scopes / cross-edges to build a
//!    multi-level hierarchy (level 0 = leaves, level N = roots).
//! 3. [`CommunitySummaryGenerator::summarise`] — collects
//!    canonical concepts in each community, groups them by
//!    relation type, and emits a structured [`CommunitySummary`].
//! 4. [`CommunityQueryRouter::route`] — given a free-form query
//!    and the calling [`SubjectRef`], returns the visible
//!    summaries whose key concepts overlap with the query terms.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fmt::Write as _;

use chrono::{DateTime, Utc};
use concept_graph::{ConceptGraph, NodeId, NodeState, RelationType};
use evidence_store::ScopeId;
use permission_service::{
    check_permission, NamespaceRegistry, ObjectRef, ObjectType, Relation, SubjectRef, TupleStore,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Object type used to model a [`ScopeId`] in the permission graph.
/// Scopes inhabit `Channel` because channels are the smallest
/// subject-bearing scope in `docs/DESIGN.md` §7.1.
const SCOPE_OBJECT_TYPE: ObjectType = ObjectType::Channel;
/// Relation required to read a community summary.
const VIEW_RELATION: Relation = Relation::Viewer;

/// Identifier for a [`Community`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CommunityId(pub Uuid);

impl CommunityId {
    /// Generate a fresh id.
    pub fn new_v4() -> Self {
        Self(Uuid::new_v4())
    }
}

impl std::fmt::Display for CommunityId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// One detected cluster of concept-graph nodes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Community {
    /// Stable id.
    pub id: CommunityId,
    /// Hierarchy level — `0` for leaves, higher numbers for
    /// aggregations.
    pub level: usize,
    /// Member [`NodeId`]s. For non-leaf communities this is the
    /// union of every leaf member transitively under this node.
    pub member_node_ids: BTreeSet<NodeId>,
    /// Scopes touched by any member. Used by the permission filter.
    pub scope_ids: BTreeSet<ScopeId>,
    /// Optional human-readable label (the most common label among
    /// canonical members, or `"community-{prefix}"` if no labels
    /// are available).
    pub label: String,
    /// Children (only for level > 0).
    pub child_ids: Vec<CommunityId>,
}

impl Community {
    /// Construct a leaf community.
    pub fn leaf(member_node_ids: BTreeSet<NodeId>, scope_ids: BTreeSet<ScopeId>) -> Self {
        let id = CommunityId::new_v4();
        Self {
            id,
            level: 0,
            member_node_ids,
            scope_ids,
            label: format!("community-{}", &id.to_string()[..8]),
            child_ids: Vec::new(),
        }
    }

    /// Override the label.
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = label.into();
        self
    }
}

/// Detects communities in a scope-filtered subgraph using
/// connected-component analysis over canonical edges.
///
/// The detector treats edges as undirected for the purpose of
/// clustering — concepts are grouped if they are reachable from
/// each other along *any* canonical relation. Non-canonical
/// nodes (candidates / contradicted / superseded) are skipped.
#[derive(Debug, Clone, Default)]
pub struct CommunityDetector {
    /// Optional scope filter — only nodes within these scopes are
    /// considered. Empty means "all scopes".
    pub scopes: Vec<ScopeId>,
    /// Edge types to use as connectivity. Empty means "all
    /// relation types".
    pub relation_types: Vec<RelationType>,
}

impl CommunityDetector {
    /// New detector with no filters.
    pub fn new() -> Self {
        Self::default()
    }

    /// Restrict to specific scopes.
    pub fn with_scopes(mut self, scopes: Vec<ScopeId>) -> Self {
        self.scopes = scopes;
        self
    }

    /// Restrict to specific relation types.
    pub fn with_relation_types(mut self, types: Vec<RelationType>) -> Self {
        self.relation_types = types;
        self
    }

    /// Run detection. Returns the leaf [`Community`] set in stable
    /// (lexicographic-by-smallest-member) order.
    pub fn detect(&self, graph: &ConceptGraph) -> Vec<Community> {
        let mut adj: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
        let mut included: HashSet<NodeId> = HashSet::new();
        let mut node_label: HashMap<NodeId, String> = HashMap::new();
        let mut node_scope: HashMap<NodeId, ScopeId> = HashMap::new();

        for node in graph.iter_nodes() {
            if !matches!(node.state, NodeState::Canonical) {
                continue;
            }
            if !self.scopes.is_empty() && !self.scopes.contains(&node.scope_id) {
                continue;
            }
            included.insert(node.id);
            node_label.insert(node.id, node.label.clone());
            node_scope.insert(node.id, node.scope_id);
            adj.entry(node.id).or_default();
        }

        for edge in graph.iter_edges() {
            if !self.relation_types.is_empty() && !self.relation_types.contains(&edge.relation) {
                continue;
            }
            if !included.contains(&edge.from) || !included.contains(&edge.to) {
                continue;
            }
            adj.entry(edge.from).or_default().push(edge.to);
            adj.entry(edge.to).or_default().push(edge.from);
        }

        let mut visited: HashSet<NodeId> = HashSet::new();
        let mut sorted_nodes: Vec<NodeId> = included.iter().copied().collect();
        sorted_nodes.sort_by_key(concept_graph::NodeId::as_uuid);

        let mut communities: Vec<Community> = Vec::new();
        for start in sorted_nodes {
            if visited.contains(&start) {
                continue;
            }
            let mut members: BTreeSet<NodeId> = BTreeSet::new();
            let mut scopes: BTreeSet<ScopeId> = BTreeSet::new();
            let mut frontier: VecDeque<NodeId> = VecDeque::from([start]);
            while let Some(node_id) = frontier.pop_front() {
                if !visited.insert(node_id) {
                    continue;
                }
                members.insert(node_id);
                if let Some(scope) = node_scope.get(&node_id) {
                    scopes.insert(*scope);
                }
                if let Some(neighbours) = adj.get(&node_id) {
                    for n in neighbours {
                        if !visited.contains(n) {
                            frontier.push_back(*n);
                        }
                    }
                }
            }
            if members.is_empty() {
                continue;
            }
            let community = Community::leaf(members.clone(), scopes);
            let community =
                if let Some(label) = members.iter().find_map(|id| node_label.get(id).cloned()) {
                    community.with_label(label)
                } else {
                    // Fall through to `Community::leaf`'s default label,
                    // which is built from the community's own id — see
                    // `Community::leaf`. The previous fallback path
                    // generated a brand-new `CommunityId` solely to
                    // synthesise a label string, then threw it away, so
                    // the rendered "community-XXXXXXXX" prefix did not
                    // match the community's actual id.
                    community
                };
            communities.push(community);
        }
        communities.sort_by(|a, b| {
            a.member_node_ids
                .iter()
                .next()
                .map(concept_graph::NodeId::as_uuid)
                .cmp(
                    &b.member_node_ids
                        .iter()
                        .next()
                        .map(concept_graph::NodeId::as_uuid),
                )
        });
        communities
    }
}

/// Multi-level community hierarchy.
///
/// Level 0 contains the raw [`CommunityDetector::detect`] output.
/// Higher levels merge sibling leaves whose scope sets overlap
/// — the simplest GraphRAG-style aggregation step. The hierarchy
/// always terminates at a single root once successive levels stop
/// merging anything.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommunityHierarchy {
    /// All communities, keyed by id.
    pub communities: HashMap<CommunityId, Community>,
    /// Communities at each level (level 0 = leaves).
    pub levels: Vec<Vec<CommunityId>>,
}

impl CommunityHierarchy {
    /// Build a hierarchy from the leaf communities returned by a
    /// [`CommunityDetector`].
    pub fn build(leaves: Vec<Community>) -> Self {
        let mut communities: HashMap<CommunityId, Community> = HashMap::new();
        let mut levels: Vec<Vec<CommunityId>> = Vec::new();
        let level0: Vec<CommunityId> = leaves.iter().map(|c| c.id).collect();
        for c in leaves {
            communities.insert(c.id, c);
        }
        levels.push(level0);

        loop {
            let current: Vec<&Community> = levels
                .last()
                .expect("at least one level")
                .iter()
                .filter_map(|id| communities.get(id))
                .collect();
            if current.len() <= 1 {
                break;
            }

            // Group communities by overlapping scope sets using a
            // union-find pass. A greedy single-pass grouping
            // misses *transitive* merges: if A overlaps B and B
            // overlaps C, but A does not directly overlap C, the
            // earlier algorithm could place A+B in one group and C
            // alone (depending on iteration order). Union-find
            // collapses A, B and C into a single component.
            let n = current.len();
            let mut parent: Vec<usize> = (0..n).collect();
            for i in 0..n {
                for j in (i + 1)..n {
                    let overlap = current[i]
                        .scope_ids
                        .iter()
                        .any(|s| current[j].scope_ids.contains(s));
                    if overlap {
                        uf_union(&mut parent, i, j);
                    }
                }
            }
            let mut by_root: BTreeMap<usize, Vec<&Community>> = BTreeMap::new();
            for (i, c) in current.iter().enumerate() {
                let r = uf_find(&mut parent, i);
                by_root.entry(r).or_default().push(*c);
            }
            let groups: Vec<Vec<&Community>> = by_root.into_values().collect();

            // Stop merging if no group merged anything.
            if groups.len() == current.len() {
                break;
            }

            let next_level_idx = levels.len();
            let mut next_ids: Vec<CommunityId> = Vec::new();
            let mut new_communities: Vec<Community> = Vec::new();
            for group in groups {
                let mut members: BTreeSet<NodeId> = BTreeSet::new();
                let mut scopes: BTreeSet<ScopeId> = BTreeSet::new();
                let mut child_ids: Vec<CommunityId> = Vec::new();
                let mut label = String::new();
                for c in &group {
                    members.extend(c.member_node_ids.iter().copied());
                    scopes.extend(c.scope_ids.iter().copied());
                    child_ids.push(c.id);
                    if label.is_empty() {
                        label.clone_from(&c.label);
                    }
                }
                let mut parent = Community::leaf(members, scopes).with_label(label);
                parent.level = next_level_idx;
                parent.child_ids = child_ids;
                next_ids.push(parent.id);
                new_communities.push(parent);
            }
            for c in new_communities {
                communities.insert(c.id, c);
            }
            levels.push(next_ids);
        }

        Self {
            communities,
            levels,
        }
    }

    /// All leaf communities in stable order.
    pub fn leaves(&self) -> Vec<&Community> {
        self.levels
            .first()
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| self.communities.get(id))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Communities at `level` (0 = leaves).
    pub fn at_level(&self, level: usize) -> Vec<&Community> {
        self.levels
            .get(level)
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| self.communities.get(id))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Look up a community by id.
    pub fn get(&self, id: CommunityId) -> Option<&Community> {
        self.communities.get(&id)
    }

    /// Number of levels (1 = only leaves, no aggregation).
    pub fn level_count(&self) -> usize {
        self.levels.len()
    }
}

/// Concise, structured summary of one [`Community`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommunitySummary {
    /// Community this summary describes.
    pub community_id: CommunityId,
    /// Hierarchy level of the community.
    pub level: usize,
    /// Free-form rendered summary text.
    pub summary_text: String,
    /// Canonical concepts (node labels) in this community,
    /// deduplicated.
    pub key_concepts: Vec<String>,
    /// Relations between members, grouped by relation type.
    pub key_relations: BTreeMap<RelationType, Vec<(String, String)>>,
    /// Wall-clock time the summary was generated.
    pub generated_at: DateTime<Utc>,
    /// Scopes the community spans.
    pub scope_ids: BTreeSet<ScopeId>,
}

/// Generates [`CommunitySummary`]s from the underlying graph.
#[derive(Debug, Clone, Default)]
pub struct CommunitySummaryGenerator;

impl CommunitySummaryGenerator {
    /// New generator.
    pub fn new() -> Self {
        Self
    }

    /// Render a summary for `community` against `graph`.
    pub fn summarise(&self, graph: &ConceptGraph, community: &Community) -> CommunitySummary {
        let mut key_concepts: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut sorted_members: Vec<NodeId> = community.member_node_ids.iter().copied().collect();
        sorted_members.sort_by_key(concept_graph::NodeId::as_uuid);
        for id in &sorted_members {
            if let Some(node) = graph.get_node(*id) {
                if seen.insert(node.label.clone()) {
                    key_concepts.push(node.label.clone());
                }
            }
        }

        let mut key_relations: BTreeMap<RelationType, Vec<(String, String)>> = BTreeMap::new();
        for edge in graph.iter_edges() {
            if !community.member_node_ids.contains(&edge.from)
                || !community.member_node_ids.contains(&edge.to)
            {
                continue;
            }
            let from_label = graph
                .get_node(edge.from)
                .map_or_else(|| edge.from.to_string(), |n| n.label.clone());
            let to_label = graph
                .get_node(edge.to)
                .map_or_else(|| edge.to.to_string(), |n| n.label.clone());
            key_relations
                .entry(edge.relation)
                .or_default()
                .push((from_label, to_label));
        }
        for vec in key_relations.values_mut() {
            vec.sort();
            vec.dedup();
        }

        let mut summary_text = String::new();
        let _ = writeln!(
            summary_text,
            "Community {} (level {})",
            community.label, community.level
        );
        let _ = write!(summary_text, "Concepts ({}): ", key_concepts.len());
        summary_text.push_str(&key_concepts.join(", "));
        for (rel, pairs) in &key_relations {
            let _ = write!(summary_text, "\n{}:", rel.as_str());
            for (a, b) in pairs {
                let _ = write!(summary_text, " {a} → {b};");
            }
        }

        CommunitySummary {
            community_id: community.id,
            level: community.level,
            summary_text,
            key_concepts,
            key_relations,
            generated_at: Utc::now(),
            scope_ids: community.scope_ids.clone(),
        }
    }

    /// Generate summaries for every community in `hierarchy`.
    pub fn summarise_all(
        &self,
        graph: &ConceptGraph,
        hierarchy: &CommunityHierarchy,
    ) -> Vec<CommunitySummary> {
        let mut out = Vec::with_capacity(hierarchy.communities.len());
        let mut sorted_ids: Vec<&CommunityId> = hierarchy.communities.keys().collect();
        sorted_ids.sort();
        for id in sorted_ids {
            if let Some(community) = hierarchy.communities.get(id) {
                out.push(self.summarise(graph, community));
            }
        }
        out
    }
}

/// Routes free-form queries to the most-relevant community
/// summaries, applying the substrate's permission filter so a
/// caller only sees communities whose constituent scopes they
/// have `viewer` (or higher) access to.
#[derive(Debug, Clone, Default)]
pub struct CommunityQueryRouter;

impl CommunityQueryRouter {
    /// New router.
    pub fn new() -> Self {
        Self
    }

    /// Pick the top-`limit` summaries that match `query` and are
    /// visible to `subject`. Visibility is checked via
    /// [`check_permission`]: every scope the community spans must
    /// be viewable by the subject.
    pub fn route<'a>(
        &self,
        query: &str,
        summaries: &'a [CommunitySummary],
        subject: SubjectRef,
        store: &TupleStore,
        registry: &NamespaceRegistry,
        limit: usize,
    ) -> Vec<&'a CommunitySummary> {
        let q_terms = tokenise(query);
        if q_terms.is_empty() {
            return Vec::new();
        }
        let mut scored: Vec<(usize, &CommunitySummary)> = Vec::new();
        for s in summaries {
            if !is_visible(&s.scope_ids, subject, store, registry) {
                continue;
            }
            let score = relevance_score(&q_terms, s);
            if score > 0 {
                scored.push((score, s));
            }
        }
        scored.sort_by_key(|s| std::cmp::Reverse(s.0));
        scored.into_iter().take(limit).map(|(_, s)| s).collect()
    }
}

/// Union-find `find` with path compression: returns the representative
/// element of `i`'s component and flattens the parent chain on the
/// way up so subsequent queries on the same component are O(α(n)).
fn uf_find(parent: &mut [usize], mut i: usize) -> usize {
    while parent[i] != i {
        parent[i] = parent[parent[i]];
        i = parent[i];
    }
    i
}

/// Union-find `union`: merge the components containing `a` and `b`
/// by pointing `a`'s root at `b`'s. No-op if they were already in
/// the same component.
fn uf_union(parent: &mut [usize], a: usize, b: usize) {
    let ra = uf_find(parent, a);
    let rb = uf_find(parent, b);
    if ra != rb {
        parent[ra] = rb;
    }
}

fn tokenise(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty() && s.len() > 2)
        .map(String::from)
        .collect()
}

fn relevance_score(terms: &[String], summary: &CommunitySummary) -> usize {
    let mut score = 0;
    let label_lower = summary.summary_text.to_lowercase();
    for t in terms {
        for c in &summary.key_concepts {
            if c.to_lowercase().contains(t) {
                score += 2;
            }
        }
        if label_lower.contains(t) {
            score += 1;
        }
    }
    score
}

fn is_visible(
    scopes: &BTreeSet<ScopeId>,
    subject: SubjectRef,
    store: &TupleStore,
    registry: &NamespaceRegistry,
) -> bool {
    if scopes.is_empty() {
        return false;
    }
    for scope in scopes {
        let object = ObjectRef::new(SCOPE_OBJECT_TYPE, scope.0);
        if !check_permission(store, registry, object, VIEW_RELATION, subject) {
            return false;
        }
    }
    true
}

#[cfg(test)]
#[allow(clippy::many_single_char_names)]
mod tests {
    use super::*;
    use concept_graph::{ConceptEdge, ConceptNode};
    use permission_service::{NamespaceConfig, RelationTuple, SubjectType};

    fn user_subject() -> SubjectRef {
        SubjectRef::direct(SubjectType::User, Uuid::new_v4())
    }

    fn scope() -> ScopeId {
        ScopeId::new_v4()
    }

    fn canonical(label: &str, scope_id: ScopeId) -> ConceptNode {
        let mut n = ConceptNode::new_candidate(label, "definition", scope_id);
        n.mark_canonical();
        n
    }

    fn add(graph: &mut ConceptGraph, node: ConceptNode) -> NodeId {
        graph.add_node(node).expect("add node")
    }

    fn link(graph: &mut ConceptGraph, from: NodeId, to: NodeId, rel: RelationType, s: ScopeId) {
        graph
            .add_edge(ConceptEdge::new(from, to, rel, s))
            .expect("add edge");
    }

    #[test]
    fn single_scope_yields_one_community() {
        let s = scope();
        let mut g = ConceptGraph::new();
        let a = add(&mut g, canonical("Atlas", s));
        let b = add(&mut g, canonical("Q3 Launch", s));
        let c = add(&mut g, canonical("Spec", s));
        link(&mut g, a, b, RelationType::PartOf, s);
        link(&mut g, c, b, RelationType::PartOf, s);
        let detector = CommunityDetector::new();
        let leaves = detector.detect(&g);
        assert_eq!(leaves.len(), 1);
        let only = &leaves[0];
        assert_eq!(only.member_node_ids.len(), 3);
        assert!(only.member_node_ids.contains(&a));
        assert!(only.member_node_ids.contains(&b));
        assert!(only.member_node_ids.contains(&c));
        assert_eq!(only.scope_ids.len(), 1);
        assert!(only.scope_ids.contains(&s));
    }

    #[test]
    fn disconnected_subgraphs_become_separate_communities() {
        let s = scope();
        let mut g = ConceptGraph::new();
        let a = add(&mut g, canonical("Apple", s));
        let b = add(&mut g, canonical("Banana", s));
        let c = add(&mut g, canonical("Carrot", s));
        let d = add(&mut g, canonical("Daikon", s));
        link(&mut g, a, b, RelationType::IsA, s);
        link(&mut g, c, d, RelationType::IsA, s);
        let leaves = CommunityDetector::new().detect(&g);
        assert_eq!(leaves.len(), 2);
        let sizes: Vec<usize> = leaves.iter().map(|c| c.member_node_ids.len()).collect();
        assert_eq!(sizes, vec![2, 2]);
    }

    #[test]
    fn multi_scope_cross_edges_unify_communities() {
        let s1 = scope();
        let s2 = scope();
        let mut g = ConceptGraph::new();
        let a = add(&mut g, canonical("a", s1));
        let b = add(&mut g, canonical("b", s2));
        link(&mut g, a, b, RelationType::DerivedFrom, s1);
        let leaves = CommunityDetector::new().detect(&g);
        assert_eq!(leaves.len(), 1);
        let only = &leaves[0];
        assert_eq!(only.scope_ids.len(), 2);
        assert!(only.scope_ids.contains(&s1));
        assert!(only.scope_ids.contains(&s2));
    }

    #[test]
    fn detector_ignores_non_canonical_nodes() {
        let s = scope();
        let mut g = ConceptGraph::new();
        let a = add(&mut g, canonical("Atlas", s));
        let candidate = add(&mut g, ConceptNode::new_candidate("Draft", "x", s));
        link(&mut g, a, candidate, RelationType::PartOf, s);
        let leaves = CommunityDetector::new().detect(&g);
        assert_eq!(leaves.len(), 1);
        assert_eq!(leaves[0].member_node_ids.len(), 1);
        assert!(leaves[0].member_node_ids.contains(&a));
    }

    #[test]
    fn hierarchy_aggregates_by_shared_scope() {
        let s1 = scope();
        let s2 = scope();
        let s3 = scope();
        let mut g = ConceptGraph::new();
        let a = add(&mut g, canonical("a", s1));
        let b = add(&mut g, canonical("b", s1));
        let c = add(&mut g, canonical("c", s2));
        let d = add(&mut g, canonical("d", s3));
        // Two separate s1 components share scope s1 — should be
        // merged at level 1.
        let e = add(&mut g, canonical("e", s1));
        let f = add(&mut g, canonical("f", s1));
        link(&mut g, a, b, RelationType::IsA, s1);
        link(&mut g, e, f, RelationType::IsA, s1);
        link(&mut g, c, c, RelationType::IsA, s2); // self-loop, but harmless
        let _ = (c, d);
        let leaves = CommunityDetector::new().detect(&g);
        let hier = CommunityHierarchy::build(leaves);
        assert!(hier.level_count() >= 1);
        // Leaves: {a,b}, {c}, {d}, {e,f} = 4 communities. After
        // merging by shared scopes, {a,b} and {e,f} both touch s1
        // and should end up under one parent.
        assert!(hier.at_level(0).len() >= 4);
    }

    #[test]
    fn summary_groups_relations_by_type() {
        let s = scope();
        let mut g = ConceptGraph::new();
        let a = add(&mut g, canonical("Atlas", s));
        let b = add(&mut g, canonical("Q3 Launch", s));
        let c = add(&mut g, canonical("@sara", s));
        link(&mut g, a, b, RelationType::PartOf, s);
        link(&mut g, b, c, RelationType::DecidedBy, s);
        let leaves = CommunityDetector::new().detect(&g);
        let community = leaves.into_iter().next().unwrap();
        let gen = CommunitySummaryGenerator::new();
        let summary = gen.summarise(&g, &community);
        assert!(summary.key_concepts.contains(&"Atlas".to_string()));
        assert!(summary.key_relations.contains_key(&RelationType::PartOf));
        assert!(summary.key_relations.contains_key(&RelationType::DecidedBy));
        assert!(summary.summary_text.contains("part_of"));
        assert!(summary.summary_text.contains("decided_by"));
    }

    #[test]
    fn summarise_all_handles_full_hierarchy() {
        let s = scope();
        let mut g = ConceptGraph::new();
        let a = add(&mut g, canonical("a", s));
        let b = add(&mut g, canonical("b", s));
        link(&mut g, a, b, RelationType::IsA, s);
        let leaves = CommunityDetector::new().detect(&g);
        let hier = CommunityHierarchy::build(leaves);
        let summaries = CommunitySummaryGenerator::new().summarise_all(&g, &hier);
        assert_eq!(summaries.len(), hier.communities.len());
        for s in &summaries {
            assert!(!s.summary_text.is_empty());
        }
    }

    fn build_perm(scopes: &[ScopeId], viewer: SubjectRef) -> (TupleStore, NamespaceRegistry) {
        let mut store = TupleStore::new();
        for scope in scopes {
            let object = ObjectRef::new(SCOPE_OBJECT_TYPE, scope.0);
            store
                .insert(RelationTuple::new(object, VIEW_RELATION, viewer))
                .expect("insert tuple");
        }
        let mut registry = NamespaceRegistry::new();
        registry
            .register(NamespaceConfig::new(SCOPE_OBJECT_TYPE))
            .expect("register namespace");
        (store, registry)
    }

    #[test]
    fn router_returns_only_visible_communities() {
        let s_visible = scope();
        let s_hidden = scope();
        let mut g = ConceptGraph::new();
        let _ = add(&mut g, canonical("alpha", s_visible));
        let _ = add(&mut g, canonical("beta", s_hidden));
        let leaves = CommunityDetector::new().detect(&g);
        let hier = CommunityHierarchy::build(leaves);
        let summaries = CommunitySummaryGenerator::new().summarise_all(&g, &hier);

        let user = user_subject();
        let (store, registry) = build_perm(&[s_visible], user);
        let router = CommunityQueryRouter::new();

        // Query mentions "alpha" — should hit the visible
        // community only.
        let hits = router.route("alpha details", &summaries, user, &store, &registry, 5);
        assert!(hits.iter().all(|s| s.scope_ids.contains(&s_visible)));
        assert!(!hits.iter().any(|s| s.scope_ids.contains(&s_hidden)));
    }

    #[test]
    fn router_filters_by_query_relevance() {
        let s = scope();
        let mut g = ConceptGraph::new();
        let _ = add(&mut g, canonical("Apple", s));
        let _ = add(&mut g, canonical("Carrot", s));
        let leaves = CommunityDetector::new().detect(&g);
        let hier = CommunityHierarchy::build(leaves);
        let summaries = CommunitySummaryGenerator::new().summarise_all(&g, &hier);
        let user = user_subject();
        let (store, registry) = build_perm(&[s], user);
        let router = CommunityQueryRouter::new();
        let hits = router.route("apple recipes", &summaries, user, &store, &registry, 5);
        assert!(hits
            .iter()
            .any(|s| s.key_concepts.iter().any(|c| c.contains("Apple"))));
        let empty = router.route("", &summaries, user, &store, &registry, 5);
        assert!(empty.is_empty());
    }

    #[test]
    fn router_returns_empty_when_user_has_no_access() {
        let s = scope();
        let mut g = ConceptGraph::new();
        let _ = add(&mut g, canonical("alpha", s));
        let leaves = CommunityDetector::new().detect(&g);
        let hier = CommunityHierarchy::build(leaves);
        let summaries = CommunitySummaryGenerator::new().summarise_all(&g, &hier);
        let user = user_subject();
        let (store, registry) = build_perm(&[], user);
        let router = CommunityQueryRouter::new();
        let hits = router.route("alpha", &summaries, user, &store, &registry, 5);
        assert!(hits.is_empty());
    }
}
