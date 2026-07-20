//! Privacy strip — the type-system-enforced invariant that every
//! synthesis output carries a description of where the synthesis ran,
//! which model produced it, and how many bytes left the device.
//!
//! Per `docs/technical/design.md` §6: "privacy strip on every synthesis output
//! (compute, model, egress)".
//!
//! The invariant is enforced by hiding [`SynthesisOutput`]'s field and
//! exposing only [`SynthesisOutput::new`], which takes a
//! [`PrivacyStrip`] by value. There is no way to construct a
//! [`SynthesisOutput<T>`] without a strip.

use serde::{Deserialize, Serialize};

/// Where the compute that produced a synthesis output ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ComputeLocation {
    /// On the user's device — the privacy-preferred path.
    OnDevice,
    /// On a server (managed AI endpoint, connector pipeline, …).
    Server,
    /// Inside an attested confidential-compute enclave.
    ConfidentialCompute,
}

impl ComputeLocation {
    /// Stable string tag.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OnDevice => "on_device",
            Self::Server => "server",
            Self::ConfidentialCompute => "confidential_compute",
        }
    }
}

/// Privacy strip rendered alongside every synthesis output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivacyStrip {
    compute_location: ComputeLocation,
    model_name: String,
    model_version: String,
    egress_bytes: u64,
    data_scope: String,
}

impl PrivacyStrip {
    /// Where the compute ran.
    pub fn compute_location(&self) -> ComputeLocation {
        self.compute_location
    }
    /// Model name (e.g. `"qwen3.5-2b"`).
    pub fn model_name(&self) -> &str {
        &self.model_name
    }
    /// Model version (e.g. `"q1_0_g128-2026-04-01"`).
    pub fn model_version(&self) -> &str {
        &self.model_version
    }
    /// Bytes that left the device for this synthesis. `0` when
    /// [`compute_location`] is [`ComputeLocation::OnDevice`].
    pub fn egress_bytes(&self) -> u64 {
        self.egress_bytes
    }
    /// Human-readable data scope label (e.g. `"user:42"`,
    /// `"channel:product-launch"`).
    pub fn data_scope(&self) -> &str {
        &self.data_scope
    }
}

/// Builder for [`PrivacyStrip`]. Each setter is chainable; `build`
/// returns the finished strip and is the only path to construct one.
#[derive(Debug, Clone, Default)]
pub struct PrivacyStripBuilder {
    compute_location: Option<ComputeLocation>,
    model_name: Option<String>,
    model_version: Option<String>,
    egress_bytes: u64,
    data_scope: Option<String>,
}

impl PrivacyStripBuilder {
    /// Construct a fresh empty builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the compute location.
    pub fn compute_location(mut self, location: ComputeLocation) -> Self {
        self.compute_location = Some(location);
        self
    }

    /// Set the model name.
    pub fn model_name(mut self, name: impl Into<String>) -> Self {
        self.model_name = Some(name.into());
        self
    }

    /// Set the model version.
    pub fn model_version(mut self, version: impl Into<String>) -> Self {
        self.model_version = Some(version.into());
        self
    }

    /// Set the egress in bytes.
    pub fn egress_bytes(mut self, bytes: u64) -> Self {
        self.egress_bytes = bytes;
        self
    }

    /// Set the data scope label.
    pub fn data_scope(mut self, scope: impl Into<String>) -> Self {
        self.data_scope = Some(scope.into());
        self
    }

    /// Finish the build. Missing fields fall back to safe placeholders
    /// — the goal is that **a strip is always present**, not that the
    /// caller must supply every field.
    pub fn build(self) -> PrivacyStrip {
        PrivacyStrip {
            compute_location: self.compute_location.unwrap_or(ComputeLocation::OnDevice),
            model_name: self.model_name.unwrap_or_else(|| "unknown".to_string()),
            model_version: self.model_version.unwrap_or_else(|| "unknown".to_string()),
            egress_bytes: self.egress_bytes,
            data_scope: self.data_scope.unwrap_or_else(|| "unspecified".to_string()),
        }
    }
}

/// Wrapper that pairs any synthesis result `T` with a mandatory
/// [`PrivacyStrip`].
///
/// **Invariant** (enforced by the type system): every
/// `SynthesisOutput` carries a privacy strip. There is no public way
/// to construct one without supplying a strip.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SynthesisOutput<T> {
    /// The synthesised payload.
    payload: T,
    /// The mandatory privacy strip.
    privacy_strip: PrivacyStrip,
}

impl<T> SynthesisOutput<T> {
    /// Pair `payload` with `privacy_strip`. This is the **only**
    /// public constructor.
    pub fn new(payload: T, privacy_strip: PrivacyStrip) -> Self {
        Self {
            payload,
            privacy_strip,
        }
    }

    /// Borrow the payload.
    pub fn payload(&self) -> &T {
        &self.payload
    }

    /// Borrow the privacy strip.
    pub fn privacy_strip(&self) -> &PrivacyStrip {
        &self.privacy_strip
    }

    /// Decompose into the payload and the privacy strip.
    pub fn into_parts(self) -> (T, PrivacyStrip) {
        (self.payload, self.privacy_strip)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_strip() -> PrivacyStrip {
        PrivacyStripBuilder::new()
            .compute_location(ComputeLocation::OnDevice)
            .model_name("qwen3.5-2b")
            .model_version("q1_0_g128-2026-04-01")
            .egress_bytes(0)
            .data_scope("user:42")
            .build()
    }

    #[test]
    fn synthesis_output_always_carries_a_strip() {
        let s = SynthesisOutput::new("a summary".to_string(), build_strip());
        assert_eq!(
            s.privacy_strip().compute_location(),
            ComputeLocation::OnDevice
        );
        assert_eq!(s.privacy_strip().model_name(), "qwen3.5-2b");
        assert_eq!(s.payload(), "a summary");
    }

    #[test]
    fn builder_falls_back_to_safe_defaults() {
        let strip = PrivacyStripBuilder::new().build();
        assert_eq!(strip.compute_location(), ComputeLocation::OnDevice);
        assert_eq!(strip.model_name(), "unknown");
        assert_eq!(strip.model_version(), "unknown");
        assert_eq!(strip.egress_bytes(), 0);
        assert_eq!(strip.data_scope(), "unspecified");
    }

    #[test]
    fn synthesis_output_serializes_with_strip() {
        let s = SynthesisOutput::new(42_u32, build_strip());
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"privacy_strip\""));
        assert!(json.contains("qwen3.5-2b"));
        let round_trip: SynthesisOutput<u32> = serde_json::from_str(&json).unwrap();
        assert_eq!(round_trip, s);
    }
}
