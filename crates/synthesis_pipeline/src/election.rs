//! Synthesizer role election — small-group "elected member device"
//! protocol skeleton.
//!
//! Per `docs/technical/design.md` §6.4: "for small groups (≤ ~12 members),
//! the elected member device runs the synthesis; the managed AI
//! endpoint or a confidential-compute worker runs it for everything
//! else". This module provides the protocol *skeleton*: candidate
//! registration, eligibility filtering, election, heartbeat /
//! step-down, and the corresponding [`SynthesizerRole`] enum.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{PipelineError, Result};

/// Default battery floor below which a device is considered
/// ineligible to be elected (per `docs/technical/design.md` §6.4: "battery > 20%").
pub const DEFAULT_BATTERY_FLOOR: u8 = 20;

/// Default battery floor below which a device, while still *eligible*
/// to be elected, defers medium-importance synthesis work to keep
/// background CPU wake-ups down.
///
/// This is a second, higher threshold layered on top of
/// [`DEFAULT_BATTERY_FLOOR`]: below 20% the device drops out of the
/// election entirely (heavy synthesis skipped); below 50% it stays
/// elected but only services high-importance observations + lexicon
/// tagging, deferring medium-importance observations and
/// non-foreground channel synthesis to AC / Wi-Fi (see
/// `docs/technical/platforms.md` "Battery").
///
/// The floor is carried per-candidate ([`ElectionCandidate::battery_defer_medium_floor`])
/// rather than on the election so a device can advertise a stricter
/// (or, for plugged-in kiosks, a relaxed) policy without the elector
/// needing global configuration.
pub const DEFAULT_BATTERY_DEFER_MEDIUM_FLOOR: u8 = 50;

/// Default heartbeat TTL — a device that has not heart-beated within
/// this many seconds is considered offline and ineligible.
pub const DEFAULT_HEARTBEAT_TTL_SECS: i64 = 60 * 5;

/// Where the synthesizer for a given scope is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SynthesizerRole {
    /// Elected member-device path (small groups, on-device, peer-to-peer).
    ElectedDevice,
    /// Managed AI endpoint (typical B2B path: Knowledge runs the
    /// synthesizer behind a managed endpoint).
    ManagedEndpoint,
    /// Attested confidential-compute worker (TEE).
    ConfidentialCompute,
}

impl SynthesizerRole {
    /// Stable string tag.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ElectedDevice => "elected_device",
            Self::ManagedEndpoint => "managed_endpoint",
            Self::ConfidentialCompute => "confidential_compute",
        }
    }
}

/// Coarse device tier — drives synthesizer eligibility.
///
/// Per `docs/technical/architecture.md` §3.2: "Tier-A handsets, Tier-B desktops, and
/// Tier-C low-end devices route differently. We require **Medium**
/// (Tier-B) or higher to be eligible for the elected-device role."
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceTier {
    /// Tier-C (low-end / wearables / older phones).
    Low,
    /// Tier-B (desktops, mid-range phones).
    Medium,
    /// Tier-A (current-gen flagship phones, workstations).
    High,
}

impl DeviceTier {
    /// Stable rank used for ordering (`Low < Medium < High`).
    pub const fn rank(self) -> u8 {
        match self {
            Self::Low => 0,
            Self::Medium => 1,
            Self::High => 2,
        }
    }
}

/// One candidate device in the election pool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ElectionCandidate {
    /// Unique device id (UUID v4).
    pub device_id: Uuid,
    /// Device tier.
    pub tier: DeviceTier,
    /// Whether the device claims to be online.
    pub online: bool,
    /// Battery percentage in `0..=100`. Plugged-in devices report
    /// `100`.
    pub battery_pct: u8,
    /// Last heartbeat timestamp.
    pub last_heartbeat: DateTime<Utc>,
    /// Whether the device has voluntarily stepped down (e.g. user
    /// disabled the role).
    pub stepped_down: bool,
    /// Battery percentage below which this device defers
    /// medium-importance synthesis work while remaining elected.
    ///
    /// Defaults to [`DEFAULT_BATTERY_DEFER_MEDIUM_FLOOR`]. A device
    /// whose `battery_pct` is at or above this floor services every
    /// importance class; below it (but at or above the eligibility
    /// floor) it continues high-importance observations + lexicon
    /// tagging only — see [`Self::defers_medium_importance`].
    ///
    /// `#[serde(default)]` so candidate snapshots serialized before
    /// this field existed deserialize with the standard
    /// [`DEFAULT_BATTERY_DEFER_MEDIUM_FLOOR`] rather than failing.
    #[serde(default = "default_battery_defer_medium_floor")]
    pub battery_defer_medium_floor: u8,
}

/// serde default for [`ElectionCandidate::battery_defer_medium_floor`].
fn default_battery_defer_medium_floor() -> u8 {
    DEFAULT_BATTERY_DEFER_MEDIUM_FLOOR
}

impl ElectionCandidate {
    /// Construct a fresh candidate with the current time as
    /// `last_heartbeat`.
    pub fn new(device_id: Uuid, tier: DeviceTier, online: bool, battery_pct: u8) -> Self {
        Self {
            device_id,
            tier,
            online,
            battery_pct,
            last_heartbeat: Utc::now(),
            stepped_down: false,
            battery_defer_medium_floor: DEFAULT_BATTERY_DEFER_MEDIUM_FLOOR,
        }
    }

    /// Override the medium-importance deferral floor for this device.
    ///
    /// Returns `self` for builder-style chaining. Hosts that want a
    /// stricter battery policy (defer medium-importance work sooner)
    /// or a relaxed one (e.g. an always-plugged-in desktop that never
    /// defers) set it here; the default is
    /// [`DEFAULT_BATTERY_DEFER_MEDIUM_FLOOR`].
    pub fn with_battery_defer_medium_floor(mut self, floor: u8) -> Self {
        self.battery_defer_medium_floor = floor;
        self
    }

    /// Whether this device should defer medium-importance synthesis
    /// work at its current battery level.
    ///
    /// `true` when `battery_pct < battery_defer_medium_floor`. Callers
    /// use this to gate medium-importance observations and
    /// non-foreground channel synthesis: an elected device that is
    /// eligible (battery ≥ [`DEFAULT_BATTERY_FLOOR`]) but draining
    /// (battery < the defer floor) keeps running high-importance work
    /// and lexicon tagging while shedding the medium-importance tail
    /// until it is back on AC / Wi-Fi.
    pub fn defers_medium_importance(&self) -> bool {
        self.battery_pct < self.battery_defer_medium_floor
    }

    fn is_eligible(&self, now: DateTime<Utc>, ttl: Duration, battery_floor: u8) -> bool {
        if self.stepped_down || !self.online {
            return false;
        }
        if self.battery_pct < battery_floor {
            return false;
        }
        if self.tier.rank() < DeviceTier::Medium.rank() {
            return false;
        }
        (now - self.last_heartbeat) < ttl
    }
}

/// Synthesizer election over a pool of [`ElectionCandidate`]s.
#[derive(Debug, Clone)]
pub struct SynthesizerElection {
    candidates: HashMap<Uuid, ElectionCandidate>,
    elected: Option<Uuid>,
    heartbeat_ttl: Duration,
    battery_floor: u8,
}

impl Default for SynthesizerElection {
    fn default() -> Self {
        Self::new()
    }
}

impl SynthesizerElection {
    /// Construct a fresh empty election with substrate defaults.
    pub fn new() -> Self {
        Self {
            candidates: HashMap::new(),
            elected: None,
            heartbeat_ttl: Duration::seconds(DEFAULT_HEARTBEAT_TTL_SECS),
            battery_floor: DEFAULT_BATTERY_FLOOR,
        }
    }

    /// Override the heartbeat TTL.
    pub fn with_heartbeat_ttl(mut self, ttl: Duration) -> Self {
        self.heartbeat_ttl = ttl;
        self
    }

    /// Override the battery floor.
    pub fn with_battery_floor(mut self, floor: u8) -> Self {
        self.battery_floor = floor;
        self
    }

    /// Register a candidate (or update an existing one).
    pub fn register(&mut self, candidate: ElectionCandidate) {
        self.candidates.insert(candidate.device_id, candidate);
    }

    /// Refresh `device_id`'s heartbeat.
    pub fn heartbeat(&mut self, device_id: Uuid) -> Result<()> {
        let candidate = self
            .candidates
            .get_mut(&device_id)
            .ok_or(PipelineError::CandidateNotFound(device_id))?;
        candidate.last_heartbeat = Utc::now();
        candidate.stepped_down = false;
        Ok(())
    }

    /// Voluntary step-down — the device asks to relinquish the role.
    /// If the device was the elected synthesizer, the next call to
    /// [`Self::elect`] picks a fresh candidate.
    pub fn step_down(&mut self, device_id: Uuid) -> Result<()> {
        let candidate = self
            .candidates
            .get_mut(&device_id)
            .ok_or(PipelineError::CandidateNotFound(device_id))?;
        candidate.stepped_down = true;
        if self.elected == Some(device_id) {
            self.elected = None;
        }
        Ok(())
    }

    /// Mark a device offline (used when peer liveness checks fail).
    pub fn mark_offline(&mut self, device_id: Uuid) -> Result<()> {
        let candidate = self
            .candidates
            .get_mut(&device_id)
            .ok_or(PipelineError::CandidateNotFound(device_id))?;
        candidate.online = false;
        if self.elected == Some(device_id) {
            self.elected = None;
        }
        Ok(())
    }

    /// True iff `device_id` is the currently elected synthesizer.
    pub fn is_synthesizer(&self, device_id: Uuid) -> bool {
        self.elected == Some(device_id)
    }

    /// The currently elected synthesizer, if any.
    pub fn elected(&self) -> Option<Uuid> {
        self.elected
    }

    /// All currently registered candidates.
    pub fn candidates(&self) -> impl Iterator<Item = &ElectionCandidate> {
        self.candidates.values()
    }

    /// Pick the best eligible candidate.
    ///
    /// Ordering:
    /// 1. Highest [`DeviceTier`].
    /// 2. Most recent heartbeat.
    /// 3. Highest battery.
    /// 4. Stable tie-break by `device_id` to keep the choice
    ///    deterministic when everything else is equal.
    ///
    /// # Errors
    ///
    /// [`PipelineError::NoEligibleSynthesizer`] when the pool has no
    /// eligible candidate (all offline / stepped down / drained
    /// battery / stale heartbeat).
    pub fn elect(&mut self) -> Result<Uuid> {
        let now = Utc::now();
        let best = self
            .candidates
            .values()
            .filter(|c| c.is_eligible(now, self.heartbeat_ttl, self.battery_floor))
            .max_by(|a, b| {
                a.tier
                    .rank()
                    .cmp(&b.tier.rank())
                    .then(a.last_heartbeat.cmp(&b.last_heartbeat))
                    .then(a.battery_pct.cmp(&b.battery_pct))
                    .then_with(|| a.device_id.as_bytes().cmp(b.device_id.as_bytes()))
            })
            .map(|c| c.device_id);
        if let Some(id) = best {
            self.elected = Some(id);
            Ok(id)
        } else {
            self.elected = None;
            Err(PipelineError::NoEligibleSynthesizer)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh(
        election: &mut SynthesizerElection,
        tier: DeviceTier,
        online: bool,
        battery: u8,
    ) -> Uuid {
        let id = Uuid::new_v4();
        election.register(ElectionCandidate::new(id, tier, online, battery));
        id
    }

    #[test]
    fn election_with_no_candidates_errors() {
        let mut e = SynthesizerElection::new();
        let err = e.elect().unwrap_err();
        assert!(matches!(err, PipelineError::NoEligibleSynthesizer));
    }

    #[test]
    fn highest_tier_wins() {
        let mut e = SynthesizerElection::new();
        let _low = fresh(&mut e, DeviceTier::Medium, true, 95);
        let high = fresh(&mut e, DeviceTier::High, true, 95);
        let elected = e.elect().unwrap();
        assert_eq!(elected, high);
        assert!(e.is_synthesizer(high));
    }

    #[test]
    fn low_battery_disqualifies_candidate() {
        let mut e = SynthesizerElection::new();
        let _drained = fresh(&mut e, DeviceTier::High, true, 5);
        let mid = fresh(&mut e, DeviceTier::Medium, true, 100);
        let elected = e.elect().unwrap();
        assert_eq!(elected, mid);
    }

    #[test]
    fn low_tier_devices_are_ineligible() {
        let mut e = SynthesizerElection::new();
        let _low = fresh(&mut e, DeviceTier::Low, true, 100);
        let err = e.elect().unwrap_err();
        assert!(matches!(err, PipelineError::NoEligibleSynthesizer));
    }

    #[test]
    fn step_down_reelects_a_different_device() {
        let mut e = SynthesizerElection::new();
        let a = fresh(&mut e, DeviceTier::High, true, 100);
        let b = fresh(&mut e, DeviceTier::High, true, 100);
        let first = e.elect().unwrap();
        e.step_down(first).unwrap();
        let second = e.elect().unwrap();
        assert_ne!(first, second);
        // The two ids partition the only two candidates, so the
        // second pick must be the other one.
        assert!(second == a || second == b);
    }

    #[test]
    fn mark_offline_drops_elected_device() {
        let mut e = SynthesizerElection::new();
        let a = fresh(&mut e, DeviceTier::High, true, 100);
        e.elect().unwrap();
        e.mark_offline(a).unwrap();
        assert!(!e.is_synthesizer(a));
        let err = e.elect().unwrap_err();
        assert!(matches!(err, PipelineError::NoEligibleSynthesizer));
    }

    #[test]
    fn stale_heartbeat_disqualifies_device() {
        let mut e = SynthesizerElection::new().with_heartbeat_ttl(Duration::seconds(60));
        let id = Uuid::new_v4();
        let mut candidate = ElectionCandidate::new(id, DeviceTier::High, true, 100);
        candidate.last_heartbeat = Utc::now() - Duration::seconds(120);
        e.register(candidate);
        let err = e.elect().unwrap_err();
        assert!(matches!(err, PipelineError::NoEligibleSynthesizer));
    }

    #[test]
    fn heartbeat_revives_disqualified_device() {
        let mut e = SynthesizerElection::new().with_heartbeat_ttl(Duration::seconds(60));
        let id = Uuid::new_v4();
        let mut candidate = ElectionCandidate::new(id, DeviceTier::High, true, 100);
        candidate.last_heartbeat = Utc::now() - Duration::seconds(120);
        e.register(candidate);
        assert!(e.elect().is_err());
        e.heartbeat(id).unwrap();
        let elected = e.elect().unwrap();
        assert_eq!(elected, id);
    }

    #[test]
    fn heartbeat_clears_step_down() {
        let mut e = SynthesizerElection::new();
        let id = fresh(&mut e, DeviceTier::High, true, 100);
        e.step_down(id).unwrap();
        assert!(e.elect().is_err());
        e.heartbeat(id).unwrap();
        assert_eq!(e.elect().unwrap(), id);
    }

    #[test]
    fn heartbeat_for_unknown_device_errors() {
        let mut e = SynthesizerElection::new();
        let err = e.heartbeat(Uuid::new_v4()).unwrap_err();
        assert!(matches!(err, PipelineError::CandidateNotFound(_)));
    }

    #[test]
    fn medium_importance_deferral_tracks_the_50pct_floor() {
        // Default floor is 50%: a device draining below it defers
        // medium-importance work, while one at/above it does not.
        let drained = ElectionCandidate::new(Uuid::new_v4(), DeviceTier::High, true, 35);
        assert!(drained.defers_medium_importance());
        let healthy = ElectionCandidate::new(Uuid::new_v4(), DeviceTier::High, true, 80);
        assert!(!healthy.defers_medium_importance());
        // Exactly at the floor does NOT defer (floor is a strict `<`).
        let at_floor = ElectionCandidate::new(
            Uuid::new_v4(),
            DeviceTier::High,
            true,
            DEFAULT_BATTERY_DEFER_MEDIUM_FLOOR,
        );
        assert!(!at_floor.defers_medium_importance());
    }

    #[test]
    fn deferral_remains_independent_of_election_eligibility() {
        // A device at 35% battery is still *eligible* (≥ 20% floor) and
        // wins election, yet it defers medium-importance work.
        let mut e = SynthesizerElection::new();
        let id = Uuid::new_v4();
        e.register(ElectionCandidate::new(id, DeviceTier::High, true, 35));
        assert_eq!(e.elect().unwrap(), id);
        let elected = e.candidates().find(|c| c.device_id == id).unwrap();
        assert!(elected.defers_medium_importance());
    }

    #[test]
    fn battery_defer_medium_floor_is_configurable() {
        // A plugged-in kiosk can relax the floor to 0 so it never
        // defers, even at low battery.
        let kiosk = ElectionCandidate::new(Uuid::new_v4(), DeviceTier::High, true, 5)
            .with_battery_defer_medium_floor(0);
        assert!(!kiosk.defers_medium_importance());
        // A conservative device can tighten it.
        let strict = ElectionCandidate::new(Uuid::new_v4(), DeviceTier::High, true, 70)
            .with_battery_defer_medium_floor(80);
        assert!(strict.defers_medium_importance());
    }

    #[test]
    fn candidate_without_defer_floor_field_deserializes_to_default() {
        // Backward-compat: a snapshot serialized before the field
        // existed must deserialize with the standard floor.
        let legacy = serde_json::json!({
            "device_id": Uuid::new_v4(),
            "tier": "high",
            "online": true,
            "battery_pct": 90,
            "last_heartbeat": Utc::now(),
            "stepped_down": false,
        });
        let candidate: ElectionCandidate = serde_json::from_value(legacy).unwrap();
        assert_eq!(
            candidate.battery_defer_medium_floor,
            DEFAULT_BATTERY_DEFER_MEDIUM_FLOOR
        );
    }
}
