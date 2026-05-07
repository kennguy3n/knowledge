//! Integration tests for the synthesizer election protocol skeleton.

use chrono::{Duration, Utc};
use uuid::Uuid;

use synthesis_pipeline::{
    DeviceTier, ElectionCandidate, PipelineError, SynthesizerElection, SynthesizerRole,
};

fn make(election: &mut SynthesizerElection, tier: DeviceTier, online: bool, battery: u8) -> Uuid {
    let id = Uuid::new_v4();
    election.register(ElectionCandidate::new(id, tier, online, battery));
    id
}

#[test]
fn single_eligible_candidate_wins() {
    let mut e = SynthesizerElection::new();
    let id = make(&mut e, DeviceTier::High, true, 100);
    assert_eq!(e.elect().unwrap(), id);
    assert!(e.is_synthesizer(id));
}

#[test]
fn higher_tier_beats_lower_tier() {
    let mut e = SynthesizerElection::new();
    let _mid = make(&mut e, DeviceTier::Medium, true, 100);
    let high = make(&mut e, DeviceTier::High, true, 95);
    assert_eq!(e.elect().unwrap(), high);
}

#[test]
fn offline_candidate_is_skipped() {
    let mut e = SynthesizerElection::new();
    let _offline = make(&mut e, DeviceTier::High, false, 100);
    let online = make(&mut e, DeviceTier::Medium, true, 100);
    assert_eq!(e.elect().unwrap(), online);
}

#[test]
fn step_down_then_reelect_picks_another_candidate() {
    let mut e = SynthesizerElection::new();
    let a = make(&mut e, DeviceTier::High, true, 100);
    let b = make(&mut e, DeviceTier::High, true, 100);
    let elected = e.elect().unwrap();
    e.step_down(elected).unwrap();
    let other = e.elect().unwrap();
    assert_ne!(elected, other);
    assert!(other == a || other == b);
}

#[test]
fn election_with_no_eligible_candidates_errors() {
    let mut e = SynthesizerElection::new();
    let _low = make(&mut e, DeviceTier::Low, true, 100);
    let _drained = make(&mut e, DeviceTier::High, true, 1);
    let err = e.elect().unwrap_err();
    assert!(matches!(err, PipelineError::NoEligibleSynthesizer));
}

#[test]
fn heartbeat_then_reelection_recovers_offline_device() {
    let mut e = SynthesizerElection::new().with_heartbeat_ttl(Duration::seconds(60));
    let id = Uuid::new_v4();
    let mut c = ElectionCandidate::new(id, DeviceTier::High, true, 100);
    c.last_heartbeat = Utc::now() - Duration::seconds(120);
    e.register(c);
    assert!(e.elect().is_err());
    e.heartbeat(id).unwrap();
    assert_eq!(e.elect().unwrap(), id);
}

#[test]
fn election_re_runs_after_synthesizer_marked_offline() {
    let mut e = SynthesizerElection::new();
    let a = make(&mut e, DeviceTier::High, true, 100);
    let b = make(&mut e, DeviceTier::High, true, 100);
    let first = e.elect().unwrap();
    e.mark_offline(first).unwrap();
    let second = e.elect().unwrap();
    assert!(second == a || second == b);
    assert_ne!(first, second);
}

#[test]
fn synthesizer_role_str_tags_match_proposal_table() {
    assert_eq!(SynthesizerRole::ElectedDevice.as_str(), "elected_device");
    assert_eq!(
        SynthesizerRole::ManagedEndpoint.as_str(),
        "managed_endpoint"
    );
    assert_eq!(
        SynthesizerRole::ConfidentialCompute.as_str(),
        "confidential_compute"
    );
}

#[test]
fn battery_floor_is_respected_for_eligibility() {
    let mut e = SynthesizerElection::new().with_battery_floor(50);
    let _drained = make(&mut e, DeviceTier::High, true, 49);
    let charged = make(&mut e, DeviceTier::High, true, 51);
    assert_eq!(e.elect().unwrap(), charged);
}

#[test]
fn unregistered_device_cannot_heartbeat() {
    let mut e = SynthesizerElection::new();
    let err = e.heartbeat(Uuid::new_v4()).unwrap_err();
    assert!(matches!(err, PipelineError::CandidateNotFound(_)));
}

#[test]
fn unregistered_device_cannot_step_down() {
    let mut e = SynthesizerElection::new();
    let err = e.step_down(Uuid::new_v4()).unwrap_err();
    assert!(matches!(err, PipelineError::CandidateNotFound(_)));
}
