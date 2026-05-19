//! Workflow memory — record successful reasoning traces and
//! abstract them into reusable patterns.
//!
//! Per `docs/DESIGN.md` §3.4 (reasoning plane), the substrate
//! keeps a small library of *workflow
//! traces* — ordered records of (query, plan, steps, outcome)
//! — and abstracts repeated traces into [`WorkflowPattern`]s
//! the planner can prime against next time.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use evidence_store::ScopeId;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{ReasoningError, Result};
use crate::planner::{QueryClass, RetrievalMode};

/// One step recorded inside a [`WorkflowTrace`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordedStep {
    /// Retrieval mode that ran.
    pub mode: RetrievalMode,
    /// Did the step produce a useful answer?
    pub succeeded: bool,
    /// Wall-clock duration of the step in milliseconds.
    pub elapsed_ms: u64,
    /// Free-form note for debugging / audit (e.g. "FTS hit on
    /// 3 rows", "graph traversal returned no path").
    pub note: Option<String>,
}

/// Successful reasoning trace.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowTrace {
    /// Stable id (UUID v4).
    pub id: Uuid,
    /// Query text.
    pub query: String,
    /// Class assigned to the query at planning time.
    pub class: QueryClass,
    /// Steps taken, in order.
    pub steps: Vec<RecordedStep>,
    /// Mode that produced the final answer.
    pub answered_by: Option<RetrievalMode>,
    /// Total elapsed time in milliseconds.
    pub total_elapsed_ms: u64,
    /// Scope the trace ran in.
    pub scope: ScopeId,
    /// Wall-clock recording time.
    pub recorded_at: DateTime<Utc>,
}

/// Pattern abstracted from one or more traces.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowPattern {
    /// Stable pattern id.
    pub id: Uuid,
    /// Query class the pattern applies to.
    pub class: QueryClass,
    /// Common steps shared across the contributing traces.
    pub steps: Vec<RetrievalMode>,
    /// Trace ids that contributed to this pattern.
    pub trace_ids: Vec<Uuid>,
    /// Number of contributing traces.
    pub support: usize,
    /// Fraction of contributing traces that succeeded
    /// (`answered_by.is_some()`).
    pub success_rate: f64,
    /// Wall-clock time the pattern was created / last updated.
    pub updated_at: DateTime<Utc>,
}

/// Builder that records a session step-by-step.
#[derive(Debug, Clone)]
pub struct TraceRecorder {
    query: String,
    class: QueryClass,
    scope: ScopeId,
    steps: Vec<RecordedStep>,
    answered_by: Option<RetrievalMode>,
    accumulated_ms: u64,
}

impl TraceRecorder {
    /// Begin recording a new trace.
    pub fn begin(query: impl Into<String>, class: QueryClass, scope: ScopeId) -> Self {
        Self {
            query: query.into(),
            class,
            scope,
            steps: Vec::new(),
            answered_by: None,
            accumulated_ms: 0,
        }
    }

    /// Record one step.
    pub fn record_step(
        &mut self,
        mode: RetrievalMode,
        succeeded: bool,
        elapsed_ms: u64,
        note: Option<String>,
    ) {
        if succeeded && self.answered_by.is_none() {
            self.answered_by = Some(mode);
        }
        self.accumulated_ms = self.accumulated_ms.saturating_add(elapsed_ms);
        self.steps.push(RecordedStep {
            mode,
            succeeded,
            elapsed_ms,
            note,
        });
    }

    /// Finalise the trace.
    pub fn finish(self) -> WorkflowTrace {
        WorkflowTrace {
            id: Uuid::new_v4(),
            query: self.query,
            class: self.class,
            steps: self.steps,
            answered_by: self.answered_by,
            total_elapsed_ms: self.accumulated_ms,
            scope: self.scope,
            recorded_at: Utc::now(),
        }
    }
}

/// In-memory store of traces and abstracted patterns.
#[derive(Debug, Clone, Default)]
pub struct WorkflowMemory {
    traces: HashMap<Uuid, WorkflowTrace>,
    patterns: HashMap<Uuid, WorkflowPattern>,
}

impl WorkflowMemory {
    /// Construct an empty memory.
    pub fn new() -> Self {
        Self::default()
    }

    /// Store a trace.
    pub fn record(&mut self, trace: WorkflowTrace) -> Uuid {
        let id = trace.id;
        self.traces.insert(id, trace);
        id
    }

    /// Look up a trace by id.
    pub fn get_trace(&self, id: Uuid) -> Result<&WorkflowTrace> {
        self.traces.get(&id).ok_or(ReasoningError::TraceNotFound)
    }

    /// All recorded traces.
    pub fn traces(&self) -> impl Iterator<Item = &WorkflowTrace> {
        self.traces.values()
    }

    /// All patterns.
    pub fn patterns(&self) -> impl Iterator<Item = &WorkflowPattern> {
        self.patterns.values()
    }

    /// Number of recorded traces.
    pub fn len(&self) -> usize {
        self.traces.len()
    }

    /// True iff no traces have been recorded.
    pub fn is_empty(&self) -> bool {
        self.traces.is_empty()
    }

    /// Build (or refresh) patterns by grouping traces by class
    /// and the *successful step prefix* — i.e. the longest
    /// prefix of `steps.iter().map(|s| s.mode)` that ends at
    /// the answering mode (or the full chain if no step
    /// succeeded). Patterns with `min_support` or fewer
    /// supporting traces are dropped.
    pub fn build_patterns(&mut self, min_support: usize) {
        self.patterns.clear();
        let mut by_key: HashMap<(QueryClass, Vec<RetrievalMode>), Vec<&WorkflowTrace>> =
            HashMap::new();
        for trace in self.traces.values() {
            let prefix: Vec<RetrievalMode> = trace
                .steps
                .iter()
                .take_while_inclusive(|s| !s.succeeded)
                .map(|s| s.mode)
                .collect();
            // If the trace eventually succeeded, ensure the
            // prefix ends at the answering mode. Otherwise,
            // keep the full chain as the prefix.
            let prefix = if let Some(answered_by) = trace.answered_by {
                let mut p = prefix;
                if p.last() != Some(&answered_by) {
                    if let Some(idx) = p.iter().position(|m| *m == answered_by) {
                        p.truncate(idx + 1);
                    } else {
                        p.push(answered_by);
                    }
                }
                p
            } else {
                prefix
            };
            by_key.entry((trace.class, prefix)).or_default().push(trace);
        }
        for ((class, steps), traces) in by_key {
            if traces.len() < min_support {
                continue;
            }
            let success_count = traces.iter().filter(|t| t.answered_by.is_some()).count();
            let pattern = WorkflowPattern {
                id: Uuid::new_v4(),
                class,
                steps,
                trace_ids: traces.iter().map(|t| t.id).collect(),
                support: traces.len(),
                success_rate: success_count as f64 / traces.len() as f64,
                updated_at: Utc::now(),
            };
            self.patterns.insert(pattern.id, pattern);
        }
    }
}

/// Returns the best pattern for an incoming query, given the
/// classifier output. Ties broken by `support` then
/// `success_rate`.
#[derive(Debug, Clone, Default)]
pub struct PatternMatcher;

impl PatternMatcher {
    /// Find the best matching pattern for `class` in `memory`.
    pub fn best_for<'a>(
        &self,
        memory: &'a WorkflowMemory,
        class: QueryClass,
    ) -> Option<&'a WorkflowPattern> {
        memory
            .patterns()
            .filter(|p| p.class == class)
            .max_by(|a, b| {
                a.success_rate
                    .partial_cmp(&b.success_rate)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(a.support.cmp(&b.support))
            })
    }
}

/// Itertools-free `take_while_inclusive`.
trait TakeWhileInclusive: Iterator + Sized {
    fn take_while_inclusive<P>(self, pred: P) -> TakeWhileInclusiveIter<Self, P>
    where
        P: FnMut(&Self::Item) -> bool,
    {
        TakeWhileInclusiveIter {
            inner: self,
            pred,
            done: false,
        }
    }
}

impl<I: Iterator> TakeWhileInclusive for I {}

struct TakeWhileInclusiveIter<I, P> {
    inner: I,
    pred: P,
    done: bool,
}

impl<I, P> Iterator for TakeWhileInclusiveIter<I, P>
where
    I: Iterator,
    P: FnMut(&I::Item) -> bool,
{
    type Item = I::Item;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let item = self.inner.next()?;
        if !(self.pred)(&item) {
            self.done = true;
        }
        Some(item)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope() -> ScopeId {
        ScopeId::new_v4()
    }

    #[test]
    fn recorder_captures_steps_and_outcome() {
        let mut r = TraceRecorder::begin("Who is Sara?", QueryClass::PointLookup, scope());
        r.record_step(RetrievalMode::Summary, false, 5, None);
        r.record_step(RetrievalMode::Fts, true, 12, Some("hit".into()));
        let trace = r.finish();
        assert_eq!(trace.steps.len(), 2);
        assert_eq!(trace.answered_by, Some(RetrievalMode::Fts));
        assert_eq!(trace.total_elapsed_ms, 17);
    }

    #[test]
    fn build_patterns_groups_by_class_and_step_prefix() {
        let mut mem = WorkflowMemory::new();
        let s = scope();
        // Three identical PointLookup traces ending at FTS.
        for _ in 0..3 {
            let mut r = TraceRecorder::begin("a", QueryClass::PointLookup, s);
            r.record_step(RetrievalMode::Summary, false, 1, None);
            r.record_step(RetrievalMode::Fts, true, 5, None);
            mem.record(r.finish());
        }
        // One Holistic trace ending at GraphTraversal.
        let mut r = TraceRecorder::begin("b", QueryClass::Holistic, s);
        r.record_step(RetrievalMode::GraphTraversal, true, 30, None);
        mem.record(r.finish());

        mem.build_patterns(3);
        let patterns: Vec<_> = mem.patterns().collect();
        assert_eq!(patterns.len(), 1);
        let p = patterns[0];
        assert_eq!(p.class, QueryClass::PointLookup);
        assert_eq!(p.steps, vec![RetrievalMode::Summary, RetrievalMode::Fts]);
        assert_eq!(p.support, 3);
        assert!((p.success_rate - 1.0).abs() < 1e-9);
    }

    #[test]
    fn matcher_picks_best_pattern_for_class() {
        let mut mem = WorkflowMemory::new();
        let s = scope();
        for _ in 0..2 {
            let mut r = TraceRecorder::begin("a", QueryClass::PointLookup, s);
            r.record_step(RetrievalMode::Fts, true, 5, None);
            mem.record(r.finish());
        }
        for _ in 0..3 {
            let mut r = TraceRecorder::begin("b", QueryClass::PointLookup, s);
            r.record_step(RetrievalMode::Summary, true, 5, None);
            mem.record(r.finish());
        }
        mem.build_patterns(2);
        let m = PatternMatcher;
        let best = m.best_for(&mem, QueryClass::PointLookup).unwrap();
        // Both patterns have 100% success; matcher breaks ties
        // by support — `Summary` pattern has 3.
        assert_eq!(best.support, 3);
        assert_eq!(best.steps, vec![RetrievalMode::Summary]);
    }

    #[test]
    fn drops_patterns_below_min_support() {
        let mut mem = WorkflowMemory::new();
        let s = scope();
        let mut r = TraceRecorder::begin("a", QueryClass::PointLookup, s);
        r.record_step(RetrievalMode::Fts, true, 5, None);
        mem.record(r.finish());
        mem.build_patterns(2);
        assert!(mem.patterns.is_empty());
    }

    #[test]
    fn get_trace_round_trips() {
        let mut mem = WorkflowMemory::new();
        let r = TraceRecorder::begin("a", QueryClass::PointLookup, scope()).finish();
        let id = r.id;
        mem.record(r);
        assert_eq!(mem.get_trace(id).unwrap().id, id);
        let err = mem.get_trace(Uuid::new_v4()).unwrap_err();
        assert_eq!(err, ReasoningError::TraceNotFound);
    }
}
