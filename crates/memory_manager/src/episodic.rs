//! Episodic memory — session / thread summaries.
//!
//! Per `docs/technical/design.md` §4: "Episodic memory — session / thread
//! summaries via on-device SLM." Each session collapses
//! a window of [`Observation`]s into one [`EpisodicSummary`], which
//! lives in the decay state machine like every other
//! [`MemoryObject`]: starts as `Candidate`, promotes to `Reinforced`
//! on retrieval, can be consolidated / canonicalized / archived.
//!
//! The summarizer side is plumbed through
//! [`inference_router::InferenceRouter`] with the
//! [`inference_router::InferenceTask::SynthSummary`] task. Devices
//! without a usable SLM use the [`StubSummarizer`] which concatenates
//! the bodies — same wire shape, lower information density.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use evidence_store::{EvidenceId, ScopeId};
use inference_router::{
    InferenceRouter, InferenceTask, ModelClass, RouterError, SummaryBundle,
    SYNTH_EXEMPLAR_TOKENS,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{MemoryError, Result};
use crate::object::SensitivityClass;
use crate::state::MemoryState;

/// Default session-boundary gap. If consecutive observations are more
/// than this far apart the session is considered closed.
pub const DEFAULT_SESSION_GAP: Duration = Duration::minutes(30);

/// Minimum number of observations required before a session is
/// considered "summarisable". Single-line sessions are dropped to
/// keep the episodic table from filling up with greeting-only rows.
pub const DEFAULT_MIN_OBSERVATIONS: usize = 2;

/// One observation feeding into the episodic summariser.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    /// Evidence row this observation was extracted from.
    pub evidence_id: EvidenceId,
    /// Scope (channel / domain / personal) this observation lives in.
    pub scope_id: ScopeId,
    /// Wall-clock time of the originating evidence row.
    pub occurred_at: DateTime<Utc>,
    /// Plaintext body (already privacy-stripped by the caller).
    pub body: String,
}

impl Observation {
    /// Construct a new observation.
    pub fn new(
        evidence_id: EvidenceId,
        scope_id: ScopeId,
        occurred_at: DateTime<Utc>,
        body: impl Into<String>,
    ) -> Self {
        Self {
            evidence_id,
            scope_id,
            occurred_at,
            body: body.into(),
        }
    }
}

/// Why the session detector decided the session ended at this point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionBoundary {
    /// `current.occurred_at - previous.occurred_at >= gap`.
    TimeGap,
    /// Explicit end-of-session marker observed (e.g. /end command,
    /// "kbye, talk soon" pattern, calendar-marked meeting end).
    ExplicitAction,
    /// The topic / entity centroid shifted enough to count as a new
    /// session. The detector itself is heuristic; production code can
    /// replace `TopicShiftDetector` with an SLM-backed classifier.
    TopicShift,
}

impl SessionBoundary {
    /// Stable string tag.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TimeGap => "time_gap",
            Self::ExplicitAction => "explicit_action",
            Self::TopicShift => "topic_shift",
        }
    }
}

/// One session worth of observations grouped by the detector.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    /// Stable session id (UUID v4) — also used as the episodic
    /// summary's `session_id`.
    pub id: Uuid,
    /// Scope id.
    pub scope_id: ScopeId,
    /// Wall-clock start of the session.
    pub started_at: DateTime<Utc>,
    /// Wall-clock end of the session.
    pub ended_at: DateTime<Utc>,
    /// Reason the session was closed.
    pub boundary: SessionBoundary,
    /// Observations grouped into this session.
    pub observations: Vec<Observation>,
}

impl Session {
    /// `true` if the session has at least
    /// [`DEFAULT_MIN_OBSERVATIONS`] observations and is therefore
    /// worth summarising.
    pub fn is_summarisable(&self) -> bool {
        self.observations.len() >= DEFAULT_MIN_OBSERVATIONS
    }
}

/// Episodic summary — the persisted output of summarising a session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EpisodicSummary {
    /// Episodic summary id (UUID v4).
    pub id: Uuid,
    /// Scope id.
    pub scope_id: ScopeId,
    /// Session id this summary corresponds to.
    pub session_id: Uuid,
    /// Plaintext summary as produced by the summariser.
    pub summary_text: String,
    /// Verbatim "key observations" extracted from the session — used
    /// by retrieval to surface the most-cited evidence rows behind
    /// the summary.
    pub key_observations: Vec<EvidenceId>,
    /// Wall-clock start of the session covered by this summary.
    pub time_range_start: DateTime<Utc>,
    /// Wall-clock end of the session covered by this summary.
    pub time_range_end: DateTime<Utc>,
    /// Wall-clock creation time of this summary row.
    pub created_at: DateTime<Utc>,
    /// Current state in the decay state machine. Episodic summaries
    /// start as `Candidate`, promote to `Reinforced` on retrieval.
    pub state: MemoryState,
    /// Retention score in `0.0 ..= 1.0`.
    pub retention_score: f64,
    /// Sensitivity class — drives the decay schedule.
    pub sensitivity_class: SensitivityClass,
}

impl EpisodicSummary {
    /// Construct a new candidate episodic summary.
    pub fn new_candidate(
        scope_id: ScopeId,
        session_id: Uuid,
        summary_text: impl Into<String>,
        key_observations: Vec<EvidenceId>,
        time_range_start: DateTime<Utc>,
        time_range_end: DateTime<Utc>,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            scope_id,
            session_id,
            summary_text: summary_text.into(),
            key_observations,
            time_range_start,
            time_range_end,
            created_at: Utc::now(),
            state: MemoryState::Candidate,
            retention_score: 0.0,
            sensitivity_class: SensitivityClass::Useful,
        }
    }

    /// Promote `Candidate -> Reinforced` and bump the retention score.
    pub fn record_retrieval(&mut self) {
        if self.state == MemoryState::Candidate {
            self.state = MemoryState::Reinforced;
        }
        // Simple retention bump capped at 1.0; the production
        // retention scorer in `crate::retention` will recompute on
        // the next decay sweep.
        self.retention_score = (self.retention_score + 0.1).min(1.0);
    }
}

/// Trait that any episodic summariser implements.
pub trait Summarizer {
    /// Summarise a session into a single string. Must not panic.
    fn summarize(&self, session: &Session) -> Result<String>;
}

/// Stub summariser — concatenates observation bodies. Used on Low-
/// tier devices and when the SLM is not available.
#[derive(Debug, Default, Clone, Copy)]
pub struct StubSummarizer;

impl StubSummarizer {
    /// Construct a fresh stub summariser.
    pub fn new() -> Self {
        Self
    }
}

impl Summarizer for StubSummarizer {
    fn summarize(&self, session: &Session) -> Result<String> {
        if session.observations.is_empty() {
            return Err(MemoryError::Validation(
                "cannot summarise an empty session".into(),
            ));
        }
        let parts: Vec<&str> = session
            .observations
            .iter()
            .map(|o| o.body.as_str())
            .collect();
        Ok(parts.join(" / "))
    }
}

/// SLM-backed summariser — dispatches to
/// [`InferenceRouter`] with the [`InferenceTask::SynthSummary`] task.
/// Falls back to [`StubSummarizer`] when the router signals fallback.
///
/// When a NER extraction closure is attached via
/// [`Self::with_ner_extraction`], the summariser first runs NER
/// extraction on the session observations, then dispatches
/// [`InferenceTask::SynthSummaryRephrase`] with the extracted facts
/// instead of [`InferenceTask::SynthSummary`]. This mirrors the
/// `HybridSynthesizer` two-stage pipeline: deterministic extraction
/// followed by SLM rephrasing. Falls back to the direct
/// [`InferenceTask::SynthSummary`] path when NER extraction produces
/// no facts.
pub struct SlmSummarizer {
    router: std::sync::Arc<InferenceRouter>,
    fallback: StubSummarizer,
    ner: Option<Box<dyn Fn(&str) -> Option<String> + Send + Sync>>,
}

impl std::fmt::Debug for SlmSummarizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlmSummarizer")
            .field("fallback", &self.fallback)
            .finish_non_exhaustive()
    }
}

impl SlmSummarizer {
    /// Construct a new SLM-backed summariser.
    pub fn new(router: std::sync::Arc<InferenceRouter>) -> Self {
        Self {
            router,
            fallback: StubSummarizer::new(),
            ner: None,
        }
    }

    /// Attach a NER extraction closure to enable the hybrid
    /// synthesis path (NER extraction + `SynthSummaryRephrase`).
    /// The closure receives the combined session observation text
    /// and returns the formatted rephrase body string, or `None` if
    /// no facts were extracted (caller falls back to the direct
    /// `SynthSummary` path).
    ///
    /// This closure-based approach avoids a hard dependency on
    /// `ner_engine` in `memory_manager` (which would create a
    /// cyclic dependency via `observation_engine`). Callers
    /// (e.g. the FFI, `synthesis_pipeline`) construct the closure
    /// wrapping their `NerExtractor` and pass it in.
    #[must_use]
    pub fn with_ner_extraction(
        mut self,
        ner: Box<dyn Fn(&str) -> Option<String> + Send + Sync>,
    ) -> Self {
        self.ner = Some(ner);
        self
    }

    fn render_prompt(router: &InferenceRouter, session: &Session) -> String {
        let body: String = session
            .observations
            .iter()
            .map(|o| format!("- {}", o.body))
            .collect::<Vec<_>>()
            .join("\n");
        let model_class = ModelClass::from_model_path(&router.config().model_path);
        InferenceTask::SynthSummary
            .prompt_template_for_class(model_class)
            .replace("{body}", &body)
    }
}

impl Summarizer for SlmSummarizer {
    /// Dispatches the session through
    /// [`InferenceTask::SynthSummary`], which is grammar-constrained
    /// to emit a JSON [`SummaryBundle`] (see
    /// `inference_router::task::GRAMMAR_SYNTH_SUMMARY`). Episodic
    /// summaries are stored as **plaintext** in
    /// [`EpisodicSummary::summary_text`], so we parse the bundle and
    /// keep only the `recap` field; the structured `decisions`,
    /// `open_questions`, and `active_tasks` arrays are produced by
    /// the SLM but not yet plumbed into the episodic schema and are
    /// dropped here. When the synthesiser-tier consumer needs them
    /// it parses the raw bundle directly via
    /// `synthesis_pipeline::LlamaCppSynthesizer`.
    ///
    /// A parse failure means the SLM (or, more likely, a
    /// non-grammar-constrained adapter such as the stub) emitted
    /// something other than [`SummaryBundle`]-shaped JSON. In that
    /// case we fall back to [`StubSummarizer`] rather than returning
    /// raw JSON to consumers that expect prose.
    fn summarize(&self, session: &Session) -> Result<String> {
        // When a NER extraction closure is attached, try NER
        // extraction + SynthSummaryRephrase first. Falls back to the
        // direct SynthSummary path when NER extraction produces no
        // facts.
        if let Some(ner) = &self.ner {
            let combined: String = session
                .observations
                .iter()
                .map(|o| o.body.as_str())
                .collect::<Vec<_>>()
                .join("\n\n");
            if let Some(rephrase_body) = ner(&combined) {
                let prompt = InferenceTask::SynthSummaryRephrase
                    .prompt_template()
                    .replace("{body}", &rephrase_body);
                return self.dispatch_rephrase(session, &prompt);
            }
        }

        let prompt = Self::render_prompt(&self.router, session);
        self.dispatch_summary(session, &prompt)
    }
}

impl SlmSummarizer {
    /// Dispatch the `SynthSummaryRephrase` task and handle fallback.
    ///
    /// Uses `from_slm_str_salvaged` to recover partial JSON from
    /// token-capped output, and checks the recap for leaked exemplar
    /// tokens (falling back to the stub if the model copied the
    /// prompt's one-shot example into the recap).
    fn dispatch_rephrase(&self, session: &Session, prompt: &str) -> Result<String> {
        match self.router.dispatch(InferenceTask::SynthSummaryRephrase, prompt) {
            Ok(text) => match SummaryBundle::from_slm_str_salvaged(&text) {
                Ok((bundle, _truncated)) if !recap_has_exemplar_leak(&bundle.recap) => {
                    Ok(bundle.recap)
                }
                Ok(_) => {
                    // Exemplar leak in the recap — fall back to stub.
                    self.fallback.summarize(session)
                }
                Err(_) => self.fallback.summarize(session),
            },
            Err(err) if err.is_fallback() => self.fallback.summarize(session),
            Err(RouterError::InferenceFailure(_)) => self.fallback.summarize(session),
            Err(err) => Err(MemoryError::Validation(format!(
                "summariser router error: {err}"
            ))),
        }
    }

    /// Dispatch the `SynthSummary` task and handle fallback.
    ///
    /// Uses `from_slm_str_salvaged` to recover partial JSON from
    /// token-capped output, and checks the recap for leaked exemplar
    /// tokens (falling back to the stub if the model copied the
    /// prompt's one-shot example into the recap).
    fn dispatch_summary(&self, session: &Session, prompt: &str) -> Result<String> {
        match self.router.dispatch(InferenceTask::SynthSummary, prompt) {
            // `from_slm_str_salvaged` salvages output a token-capped SLM
            // truncated mid-emission (closing the open string + brackets)
            // so a recap cut off at `n_predict` is still usable; only
            // genuinely unsalvageable output falls back to the stub
            // summariser. The exemplar-leak check catches a 2-bit model
            // that copies the prompt's `EXAMPLE_DECISION` / `EXAMPLE_TASK`
            // placeholders into the recap.
            Ok(text) => match SummaryBundle::from_slm_str_salvaged(&text) {
                Ok((bundle, _truncated)) if !recap_has_exemplar_leak(&bundle.recap) => {
                    Ok(bundle.recap)
                }
                Ok(_) => {
                    // Exemplar leak in the recap — fall back to stub.
                    self.fallback.summarize(session)
                }
                Err(_) => self.fallback.summarize(session),
            },
            Err(err) if err.is_fallback() => self.fallback.summarize(session),
            Err(RouterError::InferenceFailure(_)) => self.fallback.summarize(session),
            Err(err) => Err(MemoryError::Validation(format!(
                "summariser router error: {err}"
            ))),
        }
    }
}

/// Check whether the recap text contains any leaked exemplar
/// placeholder token from the synthesis prompt's one-shot example.
/// A 2-bit model may copy `EXAMPLE_DECISION` or `EXAMPLE_TASK` into
/// its output; such a recap is unusable and the caller should fall
/// back to the stub summariser.
fn recap_has_exemplar_leak(recap: &str) -> bool {
    SYNTH_EXEMPLAR_TOKENS
        .iter()
        .any(|token| recap.contains(token))
}

/// In-memory CRUD store for [`EpisodicSummary`].
#[derive(Debug, Default)]
pub struct EpisodicStore {
    by_id: HashMap<Uuid, EpisodicSummary>,
}

impl EpisodicStore {
    /// Construct a fresh store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of summaries in the store.
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    /// `true` iff there are no summaries.
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// Insert a new episodic summary. Returns the inserted id.
    pub fn insert(&mut self, summary: EpisodicSummary) -> Uuid {
        let id = summary.id;
        self.by_id.insert(id, summary);
        id
    }

    /// Get a summary by id.
    pub fn get(&self, id: &Uuid) -> Option<&EpisodicSummary> {
        self.by_id.get(id)
    }

    /// List all summaries for a given scope, sorted by `time_range_end`
    /// descending (most-recent first).
    pub fn list_by_scope(&self, scope_id: ScopeId) -> Vec<EpisodicSummary> {
        let mut hits: Vec<EpisodicSummary> = self
            .by_id
            .values()
            .filter(|s| s.scope_id == scope_id)
            .cloned()
            .collect();
        hits.sort_by_key(|s| std::cmp::Reverse(s.time_range_end));
        hits
    }

    /// Mark `id` as retrieved — bumps state to `Reinforced` and
    /// nudges the retention score upward.
    pub fn record_retrieval(&mut self, id: &Uuid) -> Result<()> {
        let entry = self.by_id.get_mut(id).ok_or(MemoryError::NotFound(*id))?;
        entry.record_retrieval();
        Ok(())
    }

    /// Forget — drops the row entirely.
    pub fn forget(&mut self, id: &Uuid) -> Result<()> {
        if self.by_id.remove(id).is_none() {
            return Err(MemoryError::NotFound(*id));
        }
        Ok(())
    }
}

/// Detects session boundaries from a stream of [`Observation`]s.
///
/// Stateless — every call to [`Self::detect`] consumes the full slice
/// and returns the contiguous sessions it found.
#[derive(Debug, Clone)]
pub struct SessionDetector {
    /// Time gap that triggers `SessionBoundary::TimeGap`.
    pub gap: Duration,
    /// Lower-cased substrings that, if present in an observation
    /// body, force `SessionBoundary::ExplicitAction` for that point.
    pub explicit_markers: Vec<String>,
}

impl Default for SessionDetector {
    fn default() -> Self {
        Self {
            gap: DEFAULT_SESSION_GAP,
            explicit_markers: vec![
                "/end".into(),
                "/wrap".into(),
                "session ended".into(),
                "let's wrap up".into(),
                "ttyl".into(),
            ],
        }
    }
}

impl SessionDetector {
    /// Construct a new detector with explicit knobs.
    pub fn new(gap: Duration, explicit_markers: Vec<String>) -> Self {
        Self {
            gap,
            explicit_markers,
        }
    }

    /// Produce a list of sessions from a chronological stream of
    /// observations. Observations must already be sorted by
    /// `occurred_at` ascending.
    pub fn detect(&self, observations: &[Observation]) -> Vec<Session> {
        if observations.is_empty() {
            return Vec::new();
        }
        let mut sessions = Vec::new();
        let mut current: Vec<Observation> = vec![observations[0].clone()];
        let mut last_time = observations[0].occurred_at;
        let mut close_reason: Option<SessionBoundary> = None;

        for obs in observations.iter().skip(1) {
            // Different scopes always force a new session — episodic
            // memory is per-scope.
            let scope_changed = obs.scope_id != current[0].scope_id;
            let gap_too_big = obs.occurred_at - last_time >= self.gap;
            let explicit = self.explicit_markers.iter().any(|m| {
                let last_body = current
                    .last()
                    .map(|o| o.body.to_ascii_lowercase())
                    .unwrap_or_default();
                last_body.contains(m.as_str())
            });

            if scope_changed || gap_too_big || explicit {
                let boundary = match (scope_changed, explicit, gap_too_big) {
                    (_, true, _) => SessionBoundary::ExplicitAction,
                    (true, _, _) => SessionBoundary::TopicShift,
                    _ => SessionBoundary::TimeGap,
                };
                close_reason = Some(boundary);
                sessions.push(Self::close(&mut current, boundary));
                current.push(obs.clone());
                last_time = obs.occurred_at;
                continue;
            }

            last_time = obs.occurred_at;
            current.push(obs.clone());
        }

        // Close the trailing session. The last buffered session was
        // never explicitly ended by any event in the input stream —
        // we only fell off the end of the iterator — so its boundary
        // is always `TimeGap`, not whatever caused the *previous*
        // session to close. Inheriting `close_reason` here mis-tagged
        // the trailing session as e.g. `ExplicitAction` when nothing
        // explicit ended it.
        let _ = close_reason; // retained for readability; intentionally unused.
        let final_boundary = SessionBoundary::TimeGap;
        sessions.push(Self::close(&mut current, final_boundary));
        sessions
    }

    fn close(buf: &mut Vec<Observation>, boundary: SessionBoundary) -> Session {
        let observations: Vec<Observation> = std::mem::take(buf);
        let started_at = observations
            .first()
            .map_or_else(Utc::now, |o| o.occurred_at);
        let ended_at = observations.last().map_or(started_at, |o| o.occurred_at);
        let scope_id = observations
            .first()
            .map_or_else(ScopeId::new_v4, |o| o.scope_id);
        Session {
            id: Uuid::new_v4(),
            scope_id,
            started_at,
            ended_at,
            boundary,
            observations,
        }
    }
}

/// Top-level façade tying the pieces together — detect sessions,
/// summarise, persist.
pub struct EpisodicMemory<S: Summarizer> {
    summarizer: S,
    detector: SessionDetector,
    store: EpisodicStore,
}

impl<S: Summarizer + std::fmt::Debug> std::fmt::Debug for EpisodicMemory<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EpisodicMemory")
            .field("summarizer", &self.summarizer)
            .field("detector", &self.detector)
            .field("store_size", &self.store.len())
            .finish()
    }
}

impl<S: Summarizer> EpisodicMemory<S> {
    /// Construct a new façade.
    pub fn new(summarizer: S, detector: SessionDetector) -> Self {
        Self {
            summarizer,
            detector,
            store: EpisodicStore::new(),
        }
    }

    /// Borrow the underlying store.
    pub fn store(&self) -> &EpisodicStore {
        &self.store
    }

    /// Mutably borrow the underlying store.
    pub fn store_mut(&mut self) -> &mut EpisodicStore {
        &mut self.store
    }

    /// Run the full pipeline — detect sessions, summarise each, and
    /// insert the resulting [`EpisodicSummary`] rows. Returns the
    /// list of inserted summaries.
    pub fn ingest(&mut self, observations: &[Observation]) -> Result<Vec<EpisodicSummary>> {
        let sessions = self.detector.detect(observations);
        let mut out = Vec::new();
        for session in sessions {
            if !session.is_summarisable() {
                continue;
            }
            let text = self.summarizer.summarize(&session)?;
            let key_obs = session.observations.iter().map(|o| o.evidence_id).collect();
            let summary = EpisodicSummary::new_candidate(
                session.scope_id,
                session.id,
                text,
                key_obs,
                session.started_at,
                session.ended_at,
            );
            self.store.insert(summary.clone());
            out.push(summary);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use evidence_store::EvidenceId;
    use inference_router::{
        AdapterKind, InferenceAdapter, InferenceRouter, ProbeResult, RouterConfig, RouterError,
    };
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

    fn obs(scope: ScopeId, ts: DateTime<Utc>, body: &str) -> Observation {
        Observation::new(EvidenceId::new_v4(), scope, ts, body)
    }

    #[test]
    fn detector_groups_observations_within_gap() {
        let scope = ScopeId::new_v4();
        let t0 = Utc::now();
        let stream = vec![
            obs(scope, t0, "starting work"),
            obs(scope, t0 + Duration::minutes(5), "more work"),
            obs(scope, t0 + Duration::minutes(7), "wrapping up"),
        ];
        let sessions = SessionDetector::default().detect(&stream);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].observations.len(), 3);
    }

    #[test]
    fn detector_splits_on_time_gap() {
        let scope = ScopeId::new_v4();
        let t0 = Utc::now();
        let stream = vec![
            obs(scope, t0, "first"),
            obs(scope, t0 + Duration::minutes(45), "after gap"),
        ];
        let sessions = SessionDetector::default().detect(&stream);
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[1].boundary, SessionBoundary::TimeGap);
    }

    #[test]
    fn detector_splits_on_explicit_marker() {
        let scope = ScopeId::new_v4();
        let t0 = Utc::now();
        let stream = vec![
            obs(scope, t0, "let's wrap up the meeting"),
            obs(scope, t0 + Duration::minutes(2), "follow-up note"),
        ];
        let sessions = SessionDetector::default().detect(&stream);
        assert_eq!(sessions.len(), 2);
        // The explicit marker closed session 0, so its boundary records
        // the trigger. The trailing session was never explicitly ended
        // and so always falls back to `TimeGap`.
        assert_eq!(sessions[0].boundary, SessionBoundary::ExplicitAction);
        assert_eq!(sessions[1].boundary, SessionBoundary::TimeGap);
    }

    #[test]
    fn detector_splits_on_scope_change() {
        let s1 = ScopeId::new_v4();
        let s2 = ScopeId::new_v4();
        let t0 = Utc::now();
        let stream = vec![
            obs(s1, t0, "scope1"),
            obs(s2, t0 + Duration::minutes(1), "scope2"),
        ];
        let sessions = SessionDetector::default().detect(&stream);
        assert_eq!(sessions.len(), 2);
        // Scope change closed session 0 with `TopicShift`. Session 1
        // (the trailing session under a new scope) was never
        // explicitly ended, so its boundary is always `TimeGap`.
        assert_eq!(sessions[0].boundary, SessionBoundary::TopicShift);
        assert_eq!(sessions[1].boundary, SessionBoundary::TimeGap);
    }

    #[test]
    fn stub_summarizer_concatenates_bodies() {
        let scope = ScopeId::new_v4();
        let t0 = Utc::now();
        let session = Session {
            id: Uuid::new_v4(),
            scope_id: scope,
            started_at: t0,
            ended_at: t0,
            boundary: SessionBoundary::TimeGap,
            observations: vec![obs(scope, t0, "alpha"), obs(scope, t0, "beta")],
        };
        let s = StubSummarizer::new();
        let out = s.summarize(&session).unwrap();
        assert_eq!(out, "alpha / beta");
    }

    #[test]
    fn stub_summarizer_errors_on_empty_session() {
        let session = Session {
            id: Uuid::new_v4(),
            scope_id: ScopeId::new_v4(),
            started_at: Utc::now(),
            ended_at: Utc::now(),
            boundary: SessionBoundary::TimeGap,
            observations: vec![],
        };
        let s = StubSummarizer::new();
        assert!(s.summarize(&session).is_err());
    }

    /// Mock adapter for the SLM summariser tests.
    struct ConstAdapter {
        response: Mutex<Result<String, RouterError>>,
        available: AtomicBool,
    }
    impl ConstAdapter {
        fn ok(text: &str) -> Self {
            Self {
                response: Mutex::new(Ok(text.into())),
                available: AtomicBool::new(true),
            }
        }
        fn err(err: RouterError) -> Self {
            Self {
                response: Mutex::new(Err(err)),
                available: AtomicBool::new(true),
            }
        }
    }
    impl InferenceAdapter for ConstAdapter {
        fn kind(&self) -> AdapterKind {
            AdapterKind::Mock
        }
        fn probe(&self) -> ProbeResult {
            ProbeResult::Available
        }
        fn is_available(&self) -> bool {
            self.available.load(Ordering::SeqCst)
        }
        fn supports(&self, _t: InferenceTask) -> bool {
            true
        }
        fn generate(
            &self,
            _t: &str,
            _p: &str,
            _g: &str,
        ) -> std::result::Result<String, RouterError> {
            self.response.lock().unwrap().clone()
        }
    }

    fn router(adapter: Box<dyn InferenceAdapter>) -> std::sync::Arc<InferenceRouter> {
        let router = InferenceRouter::new(RouterConfig::default(), vec![adapter]);
        router.bootstrap();
        std::sync::Arc::new(router)
    }

    #[test]
    fn slm_summarizer_extracts_recap_from_summary_bundle_json() {
        // SlmSummarizer dispatches the grammar-constrained
        // `SynthSummary` task, which produces SummaryBundle JSON
        // (`inference_router::task::GRAMMAR_SYNTH_SUMMARY`). The
        // episodic store records summaries as **plaintext** in
        // `EpisodicSummary::summary_text`, so the summariser must
        // unwrap the JSON and store only `bundle.recap`. This
        // regression test guards against the bug where raw JSON
        // (`{"recap":"…","decisions":[],…}`) leaks into a field
        // documented as prose.
        let scope = ScopeId::new_v4();
        let t0 = Utc::now();
        let session = Session {
            id: Uuid::new_v4(),
            scope_id: scope,
            started_at: t0,
            ended_at: t0,
            boundary: SessionBoundary::TimeGap,
            observations: vec![obs(scope, t0, "alpha"), obs(scope, t0, "beta")],
        };
        let bundle_json = serde_json::to_string(&SummaryBundle {
            recap: "the team did alpha and beta".to_string(),
            decisions: vec!["proceed".to_string()],
            open_questions: vec!["who owns it?".to_string()],
            active_tasks: vec!["draft RFC".to_string()],
        })
        .unwrap();
        let s = SlmSummarizer::new(router(Box::new(ConstAdapter::ok(&bundle_json))));
        let out = s.summarize(&session).unwrap();
        // Must be the recap field, not the raw JSON.
        assert_eq!(out, "the team did alpha and beta");
        // Defensive: the raw JSON would have started with `{` and
        // contained `"recap"`; the plaintext must not.
        assert!(
            !out.contains('{') && !out.contains("\"recap\""),
            "summary_text leaked raw JSON: {out}"
        );
    }

    #[test]
    fn slm_summarizer_falls_back_when_router_output_is_not_json() {
        // Non-JSON SLM output (e.g. a non-grammar-constrained
        // adapter such as the stub-mock used here) must not be
        // surfaced verbatim as `summary_text` — that would put
        // unparseable noise in a field documented as plaintext.
        // We fall back to `StubSummarizer` instead.
        let scope = ScopeId::new_v4();
        let t0 = Utc::now();
        let session = Session {
            id: Uuid::new_v4(),
            scope_id: scope,
            started_at: t0,
            ended_at: t0,
            boundary: SessionBoundary::TimeGap,
            observations: vec![obs(scope, t0, "alpha"), obs(scope, t0, "beta")],
        };
        let s = SlmSummarizer::new(router(Box::new(ConstAdapter::ok(
            "the team did alpha and beta",
        ))));
        let out = s.summarize(&session).unwrap();
        // StubSummarizer concatenates observation bodies.
        assert_eq!(out, "alpha / beta");
    }

    #[test]
    fn slm_summarizer_falls_back_on_unavailable_router() {
        let scope = ScopeId::new_v4();
        let t0 = Utc::now();
        let session = Session {
            id: Uuid::new_v4(),
            scope_id: scope,
            started_at: t0,
            ended_at: t0,
            boundary: SessionBoundary::TimeGap,
            observations: vec![obs(scope, t0, "alpha"), obs(scope, t0, "beta")],
        };
        let s = SlmSummarizer::new(router(Box::new(ConstAdapter::err(
            RouterError::Unavailable {
                task: "synth_summary",
            },
        ))));
        let out = s.summarize(&session).unwrap();
        // Falls back to StubSummarizer: "alpha / beta".
        assert_eq!(out, "alpha / beta");
    }

    #[test]
    fn slm_summarizer_falls_back_on_inference_failure() {
        let scope = ScopeId::new_v4();
        let t0 = Utc::now();
        let session = Session {
            id: Uuid::new_v4(),
            scope_id: scope,
            started_at: t0,
            ended_at: t0,
            boundary: SessionBoundary::TimeGap,
            observations: vec![obs(scope, t0, "alpha"), obs(scope, t0, "beta")],
        };
        let s = SlmSummarizer::new(router(Box::new(ConstAdapter::err(
            RouterError::InferenceFailure("crash".into()),
        ))));
        let out = s.summarize(&session).unwrap();
        assert_eq!(out, "alpha / beta");
    }

    #[test]
    fn store_crud_round_trips_summary() {
        let scope = ScopeId::new_v4();
        let t0 = Utc::now();
        let mut store = EpisodicStore::new();
        let summary =
            EpisodicSummary::new_candidate(scope, Uuid::new_v4(), "summary text", vec![], t0, t0);
        let id = store.insert(summary.clone());
        assert_eq!(store.len(), 1);
        assert_eq!(store.get(&id).unwrap().summary_text, "summary text");
        store.forget(&id).unwrap();
        assert!(store.is_empty());
    }

    #[test]
    fn store_record_retrieval_promotes_to_reinforced() {
        let scope = ScopeId::new_v4();
        let t0 = Utc::now();
        let mut store = EpisodicStore::new();
        let summary = EpisodicSummary::new_candidate(scope, Uuid::new_v4(), "x", vec![], t0, t0);
        let id = store.insert(summary);
        store.record_retrieval(&id).unwrap();
        let after = store.get(&id).unwrap();
        assert_eq!(after.state, MemoryState::Reinforced);
        assert!(after.retention_score > 0.0);
    }

    #[test]
    fn store_lists_by_scope_in_recency_order() {
        let scope = ScopeId::new_v4();
        let mut store = EpisodicStore::new();
        let t = Utc::now();
        let s1 = EpisodicSummary::new_candidate(scope, Uuid::new_v4(), "old", vec![], t, t);
        let s2 = EpisodicSummary::new_candidate(
            scope,
            Uuid::new_v4(),
            "new",
            vec![],
            t + Duration::minutes(10),
            t + Duration::minutes(11),
        );
        store.insert(s1);
        store.insert(s2);
        let listed = store.list_by_scope(scope);
        assert_eq!(listed[0].summary_text, "new");
        assert_eq!(listed[1].summary_text, "old");
    }

    #[test]
    fn store_forget_unknown_id_returns_not_found() {
        let mut store = EpisodicStore::new();
        let id = Uuid::new_v4();
        assert!(matches!(
            store.forget(&id).unwrap_err(),
            MemoryError::NotFound { .. }
        ));
    }

    #[test]
    fn end_to_end_episodic_pipeline_with_stub_summarizer() {
        let scope = ScopeId::new_v4();
        let t0 = Utc::now();
        let observations = vec![
            obs(scope, t0, "started kickoff meeting"),
            obs(scope, t0 + Duration::minutes(5), "agreed on scope"),
            obs(scope, t0 + Duration::minutes(40), "follow-up: docs"),
            obs(scope, t0 + Duration::minutes(45), "doc draft shared"),
        ];
        let mut episodic = EpisodicMemory::new(StubSummarizer::new(), SessionDetector::default());
        let summaries = episodic.ingest(&observations).unwrap();
        // Two sessions because of the 35-minute gap → both have 2
        // observations → both summarisable.
        assert_eq!(summaries.len(), 2);
        for s in &summaries {
            assert_eq!(s.scope_id, scope);
            assert_eq!(s.state, MemoryState::Candidate);
        }
    }

    #[test]
    fn unsummarisable_sessions_are_dropped() {
        let scope = ScopeId::new_v4();
        let t0 = Utc::now();
        let observations = vec![obs(scope, t0, "lonely message")];
        let mut episodic = EpisodicMemory::new(StubSummarizer::new(), SessionDetector::default());
        let summaries = episodic.ingest(&observations).unwrap();
        assert!(summaries.is_empty());
    }

    /// Regression test for the 2026-05-08 trailing-session fix.
    ///
    /// Before the fix, a stream that started with an explicit-end
    /// marker would leak that marker's boundary onto the *trailing*
    /// session that followed the gap. The trailing session was never
    /// explicitly ended, so its boundary should always be `TimeGap`.
    #[test]
    fn trailing_session_boundary_is_always_time_gap_after_explicit_close() {
        let scope = ScopeId::new_v4();
        let t0 = Utc::now();
        let stream = vec![
            // First session ends with an explicit marker.
            obs(scope, t0, "starting work"),
            obs(scope, t0 + Duration::minutes(1), "/end"),
            // Second session starts after a gap and is never
            // explicitly ended — its boundary must be TimeGap.
            obs(scope, t0 + Duration::minutes(45), "after gap"),
            obs(scope, t0 + Duration::minutes(46), "still after gap"),
        ];
        let sessions = SessionDetector::default().detect(&stream);
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].boundary, SessionBoundary::ExplicitAction);
        assert_eq!(
            sessions[1].boundary,
            SessionBoundary::TimeGap,
            "trailing session must default to TimeGap, not inherit prior close reason"
        );
    }

    #[test]
    fn boundary_string_tags_are_stable() {
        assert_eq!(SessionBoundary::TimeGap.as_str(), "time_gap");
        assert_eq!(SessionBoundary::ExplicitAction.as_str(), "explicit_action");
        assert_eq!(SessionBoundary::TopicShift.as_str(), "topic_shift");
    }
}
