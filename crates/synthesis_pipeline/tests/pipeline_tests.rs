//! Integration tests for the synthesis pipeline (window, object,
//! schema, no-op pipeline).

use chrono::{Duration, Utc};
use evidence_store::ScopeId;
use synthesis_pipeline::{
    NoOpSynthesizer, PipelineError, SummaryBundle, SynthesisInputs, SynthesisObjectType,
    SynthesisPipeline, SynthesisWindow, SynthesisWindowManager, WindowStatus,
};

#[test]
fn window_manager_open_and_lookup() {
    let mut mgr = SynthesisWindowManager::new();
    let scope = ScopeId::new_v4();
    let now = Utc::now();
    let id = mgr
        .open_window(scope, now - Duration::hours(1), now)
        .unwrap();
    let w = mgr.get(id).expect("window present");
    assert_eq!(w.status, WindowStatus::Pending);
    assert_eq!(w.scope_id, scope);
}

#[test]
fn rolling_window_constructor_yields_pending_window() {
    let scope = ScopeId::new_v4();
    let w = SynthesisWindow::rolling(scope, Utc::now(), Duration::hours(1)).unwrap();
    assert_eq!(w.status, WindowStatus::Pending);
    assert_eq!(w.duration(), Duration::hours(1));
}

#[test]
fn invalid_window_is_rejected() {
    let scope = ScopeId::new_v4();
    let now = Utc::now();
    let err = SynthesisWindow::new(scope, now, now - Duration::seconds(1)).unwrap_err();
    assert!(matches!(err, PipelineError::InvalidWindow));
}

#[test]
fn no_op_synthesizer_emits_well_formed_summary() {
    let scope = ScopeId::new_v4();
    let now = Utc::now();
    let window = SynthesisWindow::new(scope, now - Duration::hours(1), now).unwrap();
    let synth = NoOpSynthesizer::new();
    let object = synth
        .synthesize(&window, &SynthesisInputs::from_recap("things happened"))
        .unwrap();
    assert_eq!(object.object_type, SynthesisObjectType::ChannelRecap);
    let bundle: SummaryBundle = serde_json::from_slice(&object.payload).unwrap();
    assert_eq!(bundle.recap, "things happened");
    assert!(bundle.decisions.is_empty());
}

#[test]
fn window_manager_lifecycle_pending_in_progress_complete() {
    let mut mgr = SynthesisWindowManager::new();
    let scope = ScopeId::new_v4();
    let now = Utc::now();
    let id = mgr
        .open_window(scope, now - Duration::hours(1), now)
        .unwrap();
    mgr.mark_in_progress(id).unwrap();
    mgr.mark_complete(id).unwrap();
    assert!(mgr.get(id).unwrap().status.is_terminal());
}

#[test]
fn complete_windows_cannot_be_resurrected() {
    let mut mgr = SynthesisWindowManager::new();
    let scope = ScopeId::new_v4();
    let now = Utc::now();
    let id = mgr
        .open_window(scope, now - Duration::hours(1), now)
        .unwrap();
    mgr.mark_in_progress(id).unwrap();
    mgr.mark_complete(id).unwrap();
    let err = mgr.mark_in_progress(id).unwrap_err();
    assert!(matches!(err, PipelineError::InvalidWindowTransition));
}

#[test]
fn windows_are_grouped_by_scope() {
    let mut mgr = SynthesisWindowManager::new();
    let a = ScopeId::new_v4();
    let b = ScopeId::new_v4();
    let now = Utc::now();
    mgr.open_window(a, now - Duration::hours(2), now - Duration::hours(1))
        .unwrap();
    mgr.open_window(a, now - Duration::hours(1), now).unwrap();
    mgr.open_window(b, now - Duration::hours(1), now).unwrap();
    assert_eq!(mgr.windows_for(a).len(), 2);
    assert_eq!(mgr.windows_for(b).len(), 1);
}

#[test]
fn window_for_unknown_id_returns_none() {
    let mgr = SynthesisWindowManager::new();
    assert!(mgr
        .get(synthesis_pipeline::WindowId::from_uuid(uuid::Uuid::nil()))
        .is_none());
}

#[test]
fn mark_complete_on_unknown_window_errors() {
    let mut mgr = SynthesisWindowManager::new();
    let id = synthesis_pipeline::WindowId::new_v4();
    let err = mgr.mark_complete(id).unwrap_err();
    assert!(matches!(err, PipelineError::WindowNotFound(_)));
}

#[test]
fn manager_len_tracks_windows() {
    let mut mgr = SynthesisWindowManager::new();
    let scope = ScopeId::new_v4();
    let now = Utc::now();
    mgr.open_window(scope, now - Duration::hours(1), now)
        .unwrap();
    mgr.open_window(scope, now - Duration::hours(2), now - Duration::hours(1))
        .unwrap();
    assert_eq!(mgr.len(), 2);
    assert!(!mgr.is_empty());
}
