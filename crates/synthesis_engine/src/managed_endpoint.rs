//! Managed AI endpoint synthesizer (Phase 3 / 4).
//!
//! The Phase 3 [`crate::stub::ManagedEndpointSynthesizer`] is a
//! deterministic byte-concatenator useful for end-to-end tests but
//! not for real synthesis. This module fleshes out the real adapter
//! surface that the Go gateway will sit in front of:
//!
//! * [`EndpointConfig`] — connection metadata for the remote SLM /
//!   LLM service (URL, secret reference, model id, decoding caps).
//! * [`SynthesisRequest`] / [`SynthesisResponse`] — request /
//!   response envelopes the [`HttpClient`] trait shuttles between
//!   the engine and the remote endpoint.
//! * [`HttpClient`] — pluggable transport. The real implementation
//!   sits in the gateway crate (Go or Rust) and isn't part of this
//!   crate's surface; the `MockHttpClient` here makes the unit tests
//!   self-contained.
//! * [`HttpManagedEndpointSynthesizer`] — implements
//!   [`crate::engine::SynthesisEngine`] by:
//!     1. Validating the incoming hierarchy contract (channel ⇒
//!        domain, domain + approved-docs ⇒ tenant) via the existing
//!        [`synthesis_pipeline::HierarchyEnforcedWindowManager`].
//!     2. Building a [`SynthesisRequest`] with grammar-constrained
//!        decoding parameters drawn from
//!        [`EndpointConfig::default_grammar`] and the prompt
//!        template.
//!     3. Sending the request through the supplied [`HttpClient`]
//!        and parsing the response.
//!     4. Wrapping the response payload in a typed
//!        [`synthesis_pipeline::SynthesisObject`] and marking the
//!        window complete.
//!
//! Cross-references:
//!
//! * Phase 3 deliverables: `docs/internal/PHASES.md` Phase 3.
//! * Hierarchy contract: `synthesis_pipeline::hierarchy`.
//! * Module map: `ARCHITECTURE.md` §2.1 (`synthesis_engine`).

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Mutex,
};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use evidence_store::ScopeId;
use synthesis_pipeline::{
    build_domain_summary_object, build_tenant_summary_object, DomainSynthesisInput,
    HierarchyEnforcedWindowManager, PipelineError, SynthesisWindowManager, TenantSynthesisInput,
    TieredWindowHandle, WindowScopeTier,
};

use crate::engine::{DomainSynthesisResult, SynthesisEngine, TenantSynthesisResult};
use crate::error::{EngineError, Result};

/// Default per-request timeout used when [`EndpointConfig::timeout`]
/// is left unset.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Default token cap when [`EndpointConfig::max_tokens`] is left
/// unset. Sized to fit a domain-summary recap comfortably.
pub const DEFAULT_MAX_TOKENS: u32 = 1024;

/// Connection metadata for the remote SLM / LLM endpoint.
///
/// `api_key_ref` is intentionally a *reference* (e.g. an
/// environment-variable name or a secret-store key) rather than the
/// raw secret value — the synthesizer never sees the cleartext key,
/// the [`HttpClient`] adapter resolves the reference at call time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointConfig {
    /// HTTPS URL of the synthesis endpoint.
    pub url: String,
    /// Secret-store reference for the API key (NOT the raw key).
    pub api_key_ref: String,
    /// Model identifier (e.g. `"slm-recap-v1"`).
    pub model_id: String,
    /// Hard cap on response tokens.
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Per-request timeout. Serialised as integer milliseconds for
    /// JSON safety (`Duration` does not have a stable serde repr in
    /// the standard library).
    #[serde(default, with = "duration_millis_opt")]
    pub timeout: Option<Duration>,
    /// Default GBNF / structured-output grammar to attach to every
    /// request. Phase 3 keeps this freeform; the real adapter will
    /// compile it into the model-specific schema slot.
    #[serde(default)]
    pub default_grammar: Option<String>,
}

impl EndpointConfig {
    /// Construct a fresh endpoint config.
    pub fn new(
        url: impl Into<String>,
        api_key_ref: impl Into<String>,
        model_id: impl Into<String>,
    ) -> Self {
        Self {
            url: url.into(),
            api_key_ref: api_key_ref.into(),
            model_id: model_id.into(),
            max_tokens: None,
            timeout: None,
            default_grammar: None,
        }
    }

    /// Set the response token cap.
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    /// Set the per-request timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Attach a default decoding grammar.
    pub fn with_grammar(mut self, grammar: impl Into<String>) -> Self {
        self.default_grammar = Some(grammar.into());
        self
    }

    /// Effective token cap honouring [`DEFAULT_MAX_TOKENS`].
    pub fn effective_max_tokens(&self) -> u32 {
        self.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS)
    }

    /// Effective timeout honouring [`DEFAULT_TIMEOUT`].
    pub fn effective_timeout(&self) -> Duration {
        self.timeout.unwrap_or(DEFAULT_TIMEOUT)
    }
}

mod duration_millis_opt {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    #[allow(clippy::ref_option)]
    pub fn serialize<S: Serializer>(d: &Option<Duration>, s: S) -> Result<S::Ok, S::Error> {
        match d {
            Some(d) => s.serialize_some(&(d.as_millis() as u64)),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
        let opt: Option<u64> = Option::deserialize(d)?;
        Ok(opt.map(Duration::from_millis))
    }
}

/// One input object surfaced to the synthesizer prompt.
///
/// The synthesizer uses `payload_preview` to produce a stable
/// content fingerprint without committing to a specific
/// serialisation; the `object_id` keeps the link back to the
/// canonical [`synthesis_pipeline::SynthesisObject`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputObjectRef {
    /// Canonical object id this preview came from.
    pub object_id: Uuid,
    /// Scope the object lives in. Surfaced to the prompt so the
    /// model can mention the originating channel / domain.
    pub scope_id: ScopeId,
    /// Tier tag.
    pub tier: WindowScopeTier,
    /// First N bytes of the payload, redacted of obvious binary
    /// noise. Phase 3 lifts the bytes verbatim; the real adapter
    /// will redact PII.
    pub payload_preview: String,
}

/// Request envelope sent through the [`HttpClient`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynthesisRequest {
    /// Tier of the output the engine is asking for.
    pub scope_tier: WindowScopeTier,
    /// Scope id of the target window.
    pub target_scope: ScopeId,
    /// Input objects surfaced to the prompt.
    pub input_objects: Vec<InputObjectRef>,
    /// Prompt template. Implementations are free to interpolate the
    /// `input_objects` into it; the gateway passes it through
    /// verbatim.
    pub prompt_template: String,
    /// Grammar / structured-output schema to constrain decoding.
    /// `None` means "use the model's default" — the gateway will
    /// substitute [`EndpointConfig::default_grammar`] before
    /// sending.
    pub grammar: Option<String>,
    /// Token cap.
    pub max_tokens: u32,
    /// Model id to route to.
    pub model_id: String,
}

/// Response envelope returned by the [`HttpClient`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynthesisResponse {
    /// Generated text. The synthesizer copies this into the
    /// [`synthesis_pipeline::SynthesisObject`]'s payload bytes.
    pub output_text: String,
    /// Server-reported model version (may differ from the requested
    /// `model_id` when the gateway routes through a fallback).
    pub model_version: String,
    /// Tokens used (prompt + completion).
    pub tokens_used: u32,
    /// Latency in milliseconds reported by the gateway (so the
    /// caller can audit time budgets).
    pub latency_ms: u64,
}

/// Errors raised by the [`HttpClient`] / managed-endpoint adapter.
///
/// `Clone` is implemented so that test fixtures (e.g.
/// [`MockBehaviour::Error`]) can be cloned without losing the
/// original variant — see the `Clone` impl on [`MockBehaviour`].
#[derive(Debug, Clone, thiserror::Error)]
pub enum EndpointError {
    /// The request exceeded the configured timeout.
    #[error("synthesis request timed out after {0:?}")]
    Timeout(Duration),
    /// The remote endpoint reported a rate-limit error
    /// (HTTP 429-equivalent).
    #[error("synthesis endpoint rate-limited: {0}")]
    RateLimited(String),
    /// The remote endpoint returned a malformed response that
    /// failed schema validation.
    #[error("malformed synthesis response: {0}")]
    InvalidResponse(String),
    /// The remote endpoint reported a non-recoverable error.
    #[error("synthesis endpoint error: {0}")]
    Endpoint(String),
    /// The request itself was malformed — e.g. empty input object
    /// list when the contract requires at least one.
    #[error("malformed synthesis request: {0}")]
    InvalidRequest(String),
    /// A transport-level I/O failure (connection refused, TLS
    /// failure, ...).
    #[error("synthesis transport error: {0}")]
    Transport(String),
}

/// Pluggable HTTP transport for the managed-endpoint synthesizer.
///
/// The trait is sync on purpose — the production gateway lives in a
/// Tokio runtime that wraps a sync `block_on` around the call so
/// callers don't need to thread an executor through the synthesis
/// engine. Implementations are expected to:
///
/// * Resolve `cfg.api_key_ref` to an actual API key.
/// * Honour `cfg.effective_timeout()` and surface
///   [`EndpointError::Timeout`] if the deadline passes.
/// * Treat HTTP 429 / 503 as [`EndpointError::RateLimited`] and
///   non-200 as [`EndpointError::Endpoint`].
pub trait HttpClient: Send + Sync {
    /// Send `req` to the endpoint described by `cfg` and return the
    /// parsed response.
    ///
    /// # Errors
    ///
    /// See [`EndpointError`] for the variants surfaced.
    fn send(
        &self,
        cfg: &EndpointConfig,
        req: &SynthesisRequest,
    ) -> std::result::Result<SynthesisResponse, EndpointError>;
}

/// Mock HTTP client for unit tests. Records every request it sees
/// and returns a configured response (or error). Useful for
/// asserting the engine produces the expected request shape and
/// handles errors correctly.
#[derive(Default)]
pub struct MockHttpClient {
    requests: Mutex<Vec<SynthesisRequest>>,
    behaviour: Mutex<MockBehaviour>,
    call_count: AtomicU64,
}

#[derive(Default)]
enum MockBehaviour {
    /// Echo the joined input previews back as `output_text`.
    #[default]
    EchoInputs,
    /// Return a fixed response.
    Fixed(SynthesisResponse),
    /// Return a fixed error.
    Error(EndpointError),
    /// Return the first response, then the second, then loop the
    /// second.
    Sequence(Vec<SynthesisResponse>),
}

impl Clone for MockBehaviour {
    fn clone(&self) -> Self {
        match self {
            Self::EchoInputs => Self::EchoInputs,
            Self::Fixed(r) => Self::Fixed(r.clone()),
            // `EndpointError` itself derives `Clone`, so the cloned
            // mock preserves the original variant (e.g. `Timeout`,
            // `RateLimited`) instead of being collapsed into
            // `EndpointError::Endpoint("(cloned mock error) …")`.
            Self::Error(e) => Self::Error(e.clone()),
            Self::Sequence(s) => Self::Sequence(s.clone()),
        }
    }
}

impl MockHttpClient {
    /// Construct a mock that echoes joined input payload previews.
    pub fn echo() -> Self {
        Self::default()
    }

    /// Configure the mock to return `response` for every call.
    pub fn fixed(response: SynthesisResponse) -> Self {
        let mc = Self::default();
        *mc.behaviour.lock().expect("mutex") = MockBehaviour::Fixed(response);
        mc
    }

    /// Configure the mock to fail with `err` for every call.
    pub fn failing(err: EndpointError) -> Self {
        let mc = Self::default();
        *mc.behaviour.lock().expect("mutex") = MockBehaviour::Error(err);
        mc
    }

    /// Configure the mock to walk through `sequence` and stick on
    /// the last entry once exhausted.
    pub fn sequence(sequence: Vec<SynthesisResponse>) -> Self {
        let mc = Self::default();
        *mc.behaviour.lock().expect("mutex") = MockBehaviour::Sequence(sequence);
        mc
    }

    /// Recorded requests in call order.
    pub fn recorded_requests(&self) -> Vec<SynthesisRequest> {
        self.requests.lock().expect("mutex").clone()
    }

    /// Number of calls observed.
    pub fn call_count(&self) -> u64 {
        self.call_count.load(Ordering::SeqCst)
    }
}

impl HttpClient for MockHttpClient {
    fn send(
        &self,
        cfg: &EndpointConfig,
        req: &SynthesisRequest,
    ) -> std::result::Result<SynthesisResponse, EndpointError> {
        self.requests.lock().expect("mutex").push(req.clone());
        let n = self.call_count.fetch_add(1, Ordering::SeqCst);
        let behaviour = self.behaviour.lock().expect("mutex").clone();
        match behaviour {
            MockBehaviour::EchoInputs => {
                let joined = req
                    .input_objects
                    .iter()
                    .map(|o| o.payload_preview.clone())
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(SynthesisResponse {
                    output_text: format!("{}: {}", req.scope_tier_tag(), joined),
                    model_version: cfg.model_id.clone(),
                    tokens_used: joined.len() as u32,
                    latency_ms: 1,
                })
            }
            MockBehaviour::Fixed(r) => Ok(r),
            MockBehaviour::Error(e) => Err(e),
            MockBehaviour::Sequence(s) => {
                if s.is_empty() {
                    return Err(EndpointError::Endpoint("mock sequence is empty".into()));
                }
                let idx = (n as usize).min(s.len() - 1);
                Ok(s[idx].clone())
            }
        }
    }
}

impl SynthesisRequest {
    /// Stable string tag for the scope tier — matches the
    /// `WindowScopeTier::as_str` convention used elsewhere.
    pub fn scope_tier_tag(&self) -> &'static str {
        match self.scope_tier {
            WindowScopeTier::Channel => "channel",
            WindowScopeTier::Domain => "domain",
            WindowScopeTier::Tenant => "tenant",
        }
    }
}

/// Truncate `payload` to a UTF-8 preview suitable for prompt
/// inclusion. We try valid UTF-8 first, fall back to a
/// length-limited hex repr if the bytes aren't text.
fn render_preview(payload: &[u8], max_chars: usize) -> String {
    use std::fmt::Write;
    let limit = max_chars.max(16);
    if let Ok(s) = std::str::from_utf8(payload) {
        if s.chars().count() <= limit {
            s.to_string()
        } else {
            let mut out = String::with_capacity(limit + 1);
            for (i, c) in s.chars().enumerate() {
                if i >= limit {
                    out.push('…');
                    break;
                }
                out.push(c);
            }
            out
        }
    } else {
        // Hex-encode the first `limit` bytes — useful for
        // binary blobs in tests.
        let head = &payload[..payload.len().min(limit)];
        head.iter()
            .fold(String::with_capacity(head.len() * 2), |mut acc, b| {
                let _ = write!(acc, "{b:02x}");
                acc
            })
    }
}

/// Default prompt template used when the caller does not supply
/// one. Hand-crafted to be model-agnostic: it just lists the input
/// previews and asks for a structured recap.
pub const DEFAULT_DOMAIN_PROMPT: &str =
    "Summarise the following channel recaps for the domain. Output a concise recap.";

/// Default prompt template for tenant synthesis.
pub const DEFAULT_TENANT_PROMPT: &str =
    "Summarise the following domain summaries and approved documents \
     for the tenant. Output a concise institutional recap.";

const PAYLOAD_PREVIEW_CHARS: usize = 512;

/// Real-world managed-endpoint synthesizer.
///
/// Holds an [`EndpointConfig`] and a boxed [`HttpClient`].
/// Implements [`SynthesisEngine`] by validating the hierarchy
/// contract, building a [`SynthesisRequest`], dispatching it through
/// the client, and wrapping the response payload in a typed
/// [`synthesis_pipeline::SynthesisObject`].
pub struct HttpManagedEndpointSynthesizer<C: HttpClient> {
    cfg: EndpointConfig,
    client: C,
    /// Provenance reference attached to every emitted synthesis
    /// object. Set by callers that already published a provenance
    /// bundle for the run.
    pub provenance_ref: Uuid,
    /// Override prompt template for domain synthesis. Defaults to
    /// [`DEFAULT_DOMAIN_PROMPT`].
    pub domain_prompt: Option<String>,
    /// Override prompt template for tenant synthesis. Defaults to
    /// [`DEFAULT_TENANT_PROMPT`].
    pub tenant_prompt: Option<String>,
}

impl<C: HttpClient> HttpManagedEndpointSynthesizer<C> {
    /// Construct a fresh synthesizer.
    pub fn new(cfg: EndpointConfig, client: C) -> Self {
        Self {
            cfg,
            client,
            provenance_ref: Uuid::nil(),
            domain_prompt: None,
            tenant_prompt: None,
        }
    }

    /// Borrow the active config.
    pub fn config(&self) -> &EndpointConfig {
        &self.cfg
    }

    /// Replace the active config.
    pub fn set_config(&mut self, cfg: EndpointConfig) {
        self.cfg = cfg;
    }

    /// Borrow the underlying transport (useful for asserting on
    /// recorded requests in tests).
    pub fn client(&self) -> &C {
        &self.client
    }

    fn build_request(
        &self,
        scope_tier: WindowScopeTier,
        target_scope: ScopeId,
        input_objects: Vec<InputObjectRef>,
        prompt_template: &str,
    ) -> SynthesisRequest {
        SynthesisRequest {
            scope_tier,
            target_scope,
            input_objects,
            prompt_template: prompt_template.to_string(),
            grammar: self.cfg.default_grammar.clone(),
            max_tokens: self.cfg.effective_max_tokens(),
            model_id: self.cfg.model_id.clone(),
        }
    }

    fn dispatch(&self, req: &SynthesisRequest) -> Result<SynthesisResponse> {
        let resp = self
            .client
            .send(&self.cfg, req)
            .map_err(|e| EngineError::Endpoint(e.to_string()))?;
        if resp.output_text.is_empty() {
            return Err(EngineError::Endpoint(
                "synthesis endpoint returned empty output_text".into(),
            ));
        }
        if resp.model_version.is_empty() {
            return Err(EngineError::Endpoint(
                "synthesis endpoint returned empty model_version".into(),
            ));
        }
        Ok(resp)
    }
}

fn map_validation_error(e: PipelineError) -> EngineError {
    match e {
        PipelineError::HierarchyViolation(msg) => EngineError::Hierarchy(msg),
        other => EngineError::Pipeline(other),
    }
}

impl<C: HttpClient> SynthesisEngine for HttpManagedEndpointSynthesizer<C> {
    fn synthesize_domain(
        &self,
        windows: &mut SynthesisWindowManager,
        handle: TieredWindowHandle,
        input: DomainSynthesisInput,
    ) -> Result<DomainSynthesisResult> {
        windows
            .validate_domain_input(&handle, &input)
            .map_err(map_validation_error)?;
        if input.channel_outputs.is_empty() {
            return Err(EngineError::Hierarchy(
                "domain synthesis requires at least one channel output".into(),
            ));
        }
        windows.mark_in_progress(handle.window_id)?;

        let inputs: Vec<InputObjectRef> = input
            .channel_outputs
            .iter()
            .map(|c| {
                let obj = c.object();
                InputObjectRef {
                    object_id: obj.id.0,
                    scope_id: c.channel_scope,
                    tier: WindowScopeTier::Channel,
                    payload_preview: render_preview(&obj.payload, PAYLOAD_PREVIEW_CHARS),
                }
            })
            .collect();

        let prompt = self
            .domain_prompt
            .as_deref()
            .unwrap_or(DEFAULT_DOMAIN_PROMPT);
        let req = self.build_request(WindowScopeTier::Domain, input.domain_scope, inputs, prompt);
        // If `dispatch` fails we have to flip the window from
        // `InProgress` to `Failed` ourselves; otherwise a transport
        // hiccup leaves the window pinned in `InProgress` forever
        // and the retry path can never reopen it.
        let resp = match self.dispatch(&req) {
            Ok(r) => r,
            Err(e) => {
                let _ = windows.mark_failed(handle.window_id);
                return Err(e);
            }
        };

        let object = build_domain_summary_object(
            input.domain_scope,
            handle.window_id,
            resp.output_text.into_bytes(),
            self.provenance_ref,
        );
        windows.mark_complete(handle.window_id)?;
        Ok(DomainSynthesisResult { object })
    }

    fn synthesize_tenant(
        &self,
        windows: &mut SynthesisWindowManager,
        handle: TieredWindowHandle,
        input: TenantSynthesisInput,
    ) -> Result<TenantSynthesisResult> {
        windows
            .validate_tenant_input(&handle, &input)
            .map_err(map_validation_error)?;
        if input.domain_outputs.is_empty() {
            return Err(EngineError::Hierarchy(
                "tenant synthesis requires at least one domain output".into(),
            ));
        }
        windows.mark_in_progress(handle.window_id)?;

        let mut inputs: Vec<InputObjectRef> = input
            .domain_outputs
            .iter()
            .map(|d| {
                let obj = d.object();
                InputObjectRef {
                    object_id: obj.id.0,
                    scope_id: d.domain_scope,
                    tier: WindowScopeTier::Domain,
                    payload_preview: render_preview(&obj.payload, PAYLOAD_PREVIEW_CHARS),
                }
            })
            .collect();
        for doc in &input.approved_documents {
            inputs.push(InputObjectRef {
                object_id: doc.reference.id,
                scope_id: input.tenant_scope,
                tier: WindowScopeTier::Tenant,
                payload_preview: render_preview(&doc.payload, PAYLOAD_PREVIEW_CHARS),
            });
        }

        let prompt = self
            .tenant_prompt
            .as_deref()
            .unwrap_or(DEFAULT_TENANT_PROMPT);
        let req = self.build_request(WindowScopeTier::Tenant, input.tenant_scope, inputs, prompt);
        // Mirror the `synthesize_domain` recovery path: a failed
        // dispatch must leave the window in `Failed` so the retry
        // path can reopen it.
        let resp = match self.dispatch(&req) {
            Ok(r) => r,
            Err(e) => {
                let _ = windows.mark_failed(handle.window_id);
                return Err(e);
            }
        };

        let object = build_tenant_summary_object(
            input.tenant_scope,
            handle.window_id,
            resp.output_text.into_bytes(),
            self.provenance_ref,
        );
        windows.mark_complete(handle.window_id)?;
        Ok(TenantSynthesisResult { object })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration as ChronoDuration, Utc};
    use evidence_store::ScopeId;
    use memory_manager::{ApprovedDocumentRef, DomainMemoryObject, TenantMemoryObject};
    use synthesis_pipeline::{
        ApprovedDocument, ChannelOutput, DomainOutput, HierarchyEnforcedWindowManager,
        SynthesisObject, SynthesisObjectType, SynthesisWindowManager, WindowScopeTier,
        WindowStatus,
    };
    use uuid::Uuid;

    fn cfg() -> EndpointConfig {
        EndpointConfig::new("https://example.test/synth", "TEST_API_KEY", "slm-recap-v1")
            .with_max_tokens(64)
            .with_timeout(Duration::from_secs(5))
            .with_grammar("{root: 'string'}")
    }

    fn open_managed_window(
        mgr: &mut SynthesisWindowManager,
        scope: ScopeId,
    ) -> synthesis_pipeline::WindowId {
        let now = Utc::now();
        mgr.open_window(scope, now - ChronoDuration::seconds(60), now)
            .unwrap()
    }

    fn channel_recap(scope: ScopeId, payload: &[u8]) -> SynthesisObject {
        let mut mgr = SynthesisWindowManager::new();
        let win = open_managed_window(&mut mgr, scope);
        SynthesisObject::new(
            scope,
            win,
            SynthesisObjectType::ChannelRecap,
            payload.to_vec(),
            Uuid::nil(),
        )
    }

    fn domain_summary(scope: ScopeId, payload: &[u8]) -> SynthesisObject {
        let mut mgr = SynthesisWindowManager::new();
        let win = open_managed_window(&mut mgr, scope);
        SynthesisObject::new(
            scope,
            win,
            SynthesisObjectType::DomainSummary,
            payload.to_vec(),
            Uuid::nil(),
        )
    }

    fn open_domain_window(
        mgr: &mut SynthesisWindowManager,
        domain_scope: ScopeId,
    ) -> TieredWindowHandle {
        let now = Utc::now();
        mgr.open_tiered_window(
            domain_scope,
            WindowScopeTier::Domain,
            now - ChronoDuration::seconds(60),
            now,
        )
        .unwrap()
    }

    fn open_tenant_window(
        mgr: &mut SynthesisWindowManager,
        tenant_scope: ScopeId,
    ) -> TieredWindowHandle {
        let now = Utc::now();
        mgr.open_tiered_window(
            tenant_scope,
            WindowScopeTier::Tenant,
            now - ChronoDuration::seconds(60),
            now,
        )
        .unwrap()
    }

    #[test]
    fn endpoint_config_round_trips_through_serde() {
        let c = cfg();
        let json = serde_json::to_string(&c).expect("serialize");
        let back: EndpointConfig = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(c, back);
    }

    #[test]
    fn endpoint_config_defaults_apply() {
        let c = EndpointConfig::new("u", "k", "m");
        assert_eq!(c.effective_max_tokens(), DEFAULT_MAX_TOKENS);
        assert_eq!(c.effective_timeout(), DEFAULT_TIMEOUT);
        assert!(c.default_grammar.is_none());
    }

    #[test]
    fn synthesize_domain_dispatches_request_and_wraps_response() {
        let domain_scope = ScopeId::new_v4();
        let channel_a = ScopeId::new_v4();
        let channel_b = ScopeId::new_v4();
        let mut domain = DomainMemoryObject::new(domain_scope);
        domain.attach_channel_scope(channel_a);
        domain.attach_channel_scope(channel_b);
        let outputs = vec![
            ChannelOutput::from_channel_object(channel_recap(channel_a, b"a-recap")).unwrap(),
            ChannelOutput::from_channel_object(channel_recap(channel_b, b"b-recap")).unwrap(),
        ];
        let input = DomainSynthesisInput::new(&domain, outputs).unwrap();

        let mut mgr = SynthesisWindowManager::new();
        let handle = open_domain_window(&mut mgr, domain_scope);

        let synth = HttpManagedEndpointSynthesizer::new(cfg(), MockHttpClient::echo());
        let r = synth.synthesize_domain(&mut mgr, handle, input).unwrap();

        // The mock echoes joined previews back: we should see both
        // recap bodies in the wrapped object.
        let payload = String::from_utf8(r.object.payload.clone()).unwrap();
        assert!(payload.contains("a-recap"));
        assert!(payload.contains("b-recap"));
        assert_eq!(r.object.object_type, SynthesisObjectType::DomainSummary);

        // The recorded request preserved every input.
        let recorded = synth.client().recorded_requests();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].scope_tier, WindowScopeTier::Domain);
        assert_eq!(recorded[0].target_scope, domain_scope);
        assert_eq!(recorded[0].input_objects.len(), 2);
        assert_eq!(recorded[0].max_tokens, 64);
        assert_eq!(recorded[0].grammar.as_deref(), Some("{root: 'string'}"));
    }

    #[test]
    fn synthesize_tenant_dispatches_request_and_wraps_response() {
        let tenant_scope = ScopeId::new_v4();
        let domain_a = ScopeId::new_v4();
        let domain_b = ScopeId::new_v4();
        let mut tenant = TenantMemoryObject::new(tenant_scope);
        tenant.attach_domain_scope(domain_a);
        tenant.attach_domain_scope(domain_b);

        let approved_ref = ApprovedDocumentRef::new("Handbook", "@admin");
        tenant.admit_approved_document(approved_ref.clone());

        let outputs = vec![
            DomainOutput::from_domain_object(domain_summary(domain_a, b"a-domain")).unwrap(),
            DomainOutput::from_domain_object(domain_summary(domain_b, b"b-domain")).unwrap(),
        ];
        let docs = vec![ApprovedDocument::new(
            approved_ref,
            b"approved-blob".to_vec(),
        )];
        let input = TenantSynthesisInput::new(&tenant, outputs, docs).unwrap();

        let mut mgr = SynthesisWindowManager::new();
        let handle = open_tenant_window(&mut mgr, tenant_scope);

        let synth = HttpManagedEndpointSynthesizer::new(cfg(), MockHttpClient::echo());
        let r = synth.synthesize_tenant(&mut mgr, handle, input).unwrap();
        let payload = String::from_utf8(r.object.payload.clone()).unwrap();
        assert!(payload.contains("a-domain"));
        assert!(payload.contains("b-domain"));
        assert!(payload.contains("approved-blob"));
        assert_eq!(r.object.object_type, SynthesisObjectType::TenantSummary);

        let recorded = synth.client().recorded_requests();
        assert_eq!(recorded[0].scope_tier, WindowScopeTier::Tenant);
        assert_eq!(recorded[0].input_objects.len(), 3);
        // Approved doc is surfaced under the tenant scope so the
        // model can attribute it correctly.
        let last = recorded[0].input_objects.last().unwrap();
        assert_eq!(last.tier, WindowScopeTier::Tenant);
        assert_eq!(last.scope_id, tenant_scope);
    }

    #[test]
    fn empty_channel_inputs_are_rejected() {
        let domain_scope = ScopeId::new_v4();
        let domain = DomainMemoryObject::new(domain_scope);
        let input = DomainSynthesisInput::new(&domain, Vec::new()).unwrap();
        let mut mgr = SynthesisWindowManager::new();
        let handle = open_domain_window(&mut mgr, domain_scope);
        let synth = HttpManagedEndpointSynthesizer::new(cfg(), MockHttpClient::echo());
        let err = synth
            .synthesize_domain(&mut mgr, handle, input)
            .unwrap_err();
        assert!(matches!(err, EngineError::Hierarchy(_)));
        assert_eq!(synth.client().call_count(), 0);
    }

    #[test]
    fn endpoint_timeout_propagates_as_engine_error() {
        let domain_scope = ScopeId::new_v4();
        let channel = ScopeId::new_v4();
        let mut domain = DomainMemoryObject::new(domain_scope);
        domain.attach_channel_scope(channel);
        let outputs =
            vec![ChannelOutput::from_channel_object(channel_recap(channel, b"x")).unwrap()];
        let input = DomainSynthesisInput::new(&domain, outputs).unwrap();
        let mut mgr = SynthesisWindowManager::new();
        let handle = open_domain_window(&mut mgr, domain_scope);

        let mock = MockHttpClient::failing(EndpointError::Timeout(Duration::from_secs(5)));
        let synth = HttpManagedEndpointSynthesizer::new(cfg(), mock);
        let err = synth
            .synthesize_domain(&mut mgr, handle, input)
            .unwrap_err();
        assert!(matches!(err, EngineError::Endpoint(_)));
        let msg = err.to_string();
        assert!(msg.contains("timed out"), "got: {msg}");
    }

    #[test]
    fn rate_limit_propagates_as_engine_error() {
        let domain_scope = ScopeId::new_v4();
        let channel = ScopeId::new_v4();
        let mut domain = DomainMemoryObject::new(domain_scope);
        domain.attach_channel_scope(channel);
        let outputs =
            vec![ChannelOutput::from_channel_object(channel_recap(channel, b"x")).unwrap()];
        let input = DomainSynthesisInput::new(&domain, outputs).unwrap();
        let mut mgr = SynthesisWindowManager::new();
        let handle = open_domain_window(&mut mgr, domain_scope);

        let mock = MockHttpClient::failing(EndpointError::RateLimited("retry-after 5s".into()));
        let synth = HttpManagedEndpointSynthesizer::new(cfg(), mock);
        let err = synth
            .synthesize_domain(&mut mgr, handle, input)
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("rate-limited"), "got: {msg}");
    }

    #[test]
    fn empty_response_is_treated_as_endpoint_error() {
        let domain_scope = ScopeId::new_v4();
        let channel = ScopeId::new_v4();
        let mut domain = DomainMemoryObject::new(domain_scope);
        domain.attach_channel_scope(channel);
        let outputs =
            vec![ChannelOutput::from_channel_object(channel_recap(channel, b"x")).unwrap()];
        let input = DomainSynthesisInput::new(&domain, outputs).unwrap();
        let mut mgr = SynthesisWindowManager::new();
        let handle = open_domain_window(&mut mgr, domain_scope);

        let mock = MockHttpClient::fixed(SynthesisResponse {
            output_text: String::new(),
            model_version: "v1".into(),
            tokens_used: 0,
            latency_ms: 1,
        });
        let synth = HttpManagedEndpointSynthesizer::new(cfg(), mock);
        let err = synth
            .synthesize_domain(&mut mgr, handle, input)
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("empty output_text"), "got: {msg}");
    }

    #[test]
    fn cross_tier_handle_rejected_at_validate() {
        // Building a tenant-tier handle while passing a domain
        // input — the hierarchy validator must refuse before any
        // request goes out.
        let tenant_scope = ScopeId::new_v4();
        let mut mgr = SynthesisWindowManager::new();
        let handle = open_tenant_window(&mut mgr, tenant_scope);

        let domain_scope = ScopeId::new_v4();
        let mut domain = DomainMemoryObject::new(domain_scope);
        domain.attach_channel_scope(domain_scope);
        let outputs =
            vec![ChannelOutput::from_channel_object(channel_recap(domain_scope, b"x")).unwrap()];
        let input = DomainSynthesisInput::new(&domain, outputs).unwrap();

        let synth = HttpManagedEndpointSynthesizer::new(cfg(), MockHttpClient::echo());
        let err = synth
            .synthesize_domain(&mut mgr, handle, input)
            .unwrap_err();
        assert!(matches!(err, EngineError::Hierarchy(_)));
        assert_eq!(synth.client().call_count(), 0);
    }

    /// Regression test for the 2026-05-08 dispatch-failure fix.
    ///
    /// Before the fix, `synthesize_domain` did `let resp =
    /// self.dispatch(&req)?;` after marking the window
    /// `InProgress` — a transport failure left the window pinned
    /// in `InProgress`, blocking the retry path. The fix flips the
    /// window to `Failed` on dispatch error.
    #[test]
    fn dispatch_failure_marks_domain_window_as_failed() {
        let domain_scope = ScopeId::new_v4();
        let channel = ScopeId::new_v4();
        let mut domain = DomainMemoryObject::new(domain_scope);
        domain.attach_channel_scope(channel);
        let outputs =
            vec![ChannelOutput::from_channel_object(channel_recap(channel, b"x")).unwrap()];
        let input = DomainSynthesisInput::new(&domain, outputs).unwrap();
        let mut mgr = SynthesisWindowManager::new();
        let handle = open_domain_window(&mut mgr, domain_scope);
        let window_id = handle.window_id;

        let mock = MockHttpClient::failing(EndpointError::Timeout(Duration::from_secs(5)));
        let synth = HttpManagedEndpointSynthesizer::new(cfg(), mock);
        let _ = synth
            .synthesize_domain(&mut mgr, handle, input)
            .expect_err("dispatch should fail");

        let after = mgr.get(window_id).expect("window present");
        assert_eq!(
            after.status,
            WindowStatus::Failed,
            "dispatch failure must transition the window to Failed (not leave it InProgress)"
        );
        // And the retry path can reopen it.
        mgr.mark_in_progress(window_id)
            .expect("Failed → InProgress retry must be allowed");
    }

    #[test]
    fn dispatch_failure_marks_tenant_window_as_failed() {
        let tenant_scope = ScopeId::new_v4();
        let domain = ScopeId::new_v4();
        let mut tenant = TenantMemoryObject::new(tenant_scope);
        tenant.attach_domain_scope(domain);
        let outputs = vec![DomainOutput::from_domain_object(domain_summary(domain, b"x")).unwrap()];
        let input = TenantSynthesisInput::new(&tenant, outputs, Vec::new()).unwrap();
        let mut mgr = SynthesisWindowManager::new();
        let handle = open_tenant_window(&mut mgr, tenant_scope);
        let window_id = handle.window_id;

        let mock = MockHttpClient::failing(EndpointError::RateLimited("retry-after 5s".into()));
        let synth = HttpManagedEndpointSynthesizer::new(cfg(), mock);
        let _ = synth
            .synthesize_tenant(&mut mgr, handle, input)
            .expect_err("dispatch should fail");

        let after = mgr.get(window_id).expect("window present");
        assert_eq!(after.status, WindowStatus::Failed);
    }

    /// Regression test for the 2026-05-08 `MockBehaviour::Error`
    /// clone fix. Before the fix the clone collapsed every variant
    /// into `EndpointError::Endpoint("(cloned mock error) …")`,
    /// which silently corrupted any test that exercised retry
    /// behaviour off the mock client. The clone now preserves the
    /// original variant.
    #[test]
    fn mock_behaviour_error_clone_preserves_timeout_variant() {
        let domain_scope = ScopeId::new_v4();
        let channel = ScopeId::new_v4();
        let mut domain = DomainMemoryObject::new(domain_scope);
        domain.attach_channel_scope(channel);
        let outputs =
            vec![ChannelOutput::from_channel_object(channel_recap(channel, b"x")).unwrap()];
        let input = DomainSynthesisInput::new(&domain, outputs).unwrap();
        let mut mgr = SynthesisWindowManager::new();
        let handle = open_domain_window(&mut mgr, domain_scope);

        // Build the mock and then clone it before wiring it into
        // the synthesizer — the cloned mock must produce the same
        // `Timeout` error, not a generic `Endpoint` error.
        let mock = MockHttpClient::failing(EndpointError::Timeout(Duration::from_secs(7)));
        let cloned_behaviour = mock.behaviour.lock().expect("mutex").clone();
        match cloned_behaviour {
            MockBehaviour::Error(EndpointError::Timeout(d)) => {
                assert_eq!(d, Duration::from_secs(7));
            }
            MockBehaviour::Error(other) => {
                panic!("clone collapsed Timeout into a different EndpointError variant: {other}");
            }
            _ => panic!("clone replaced Error variant entirely"),
        }

        // End-to-end: drive the synthesizer with the original mock
        // and confirm the engine surface still sees a Timeout.
        let synth = HttpManagedEndpointSynthesizer::new(cfg(), mock);
        let err = synth
            .synthesize_domain(&mut mgr, handle, input)
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("timed out"),
            "expected Timeout variant to survive, got: {msg}"
        );
    }

    #[test]
    fn render_preview_truncates_long_strings() {
        let s = "a".repeat(2000);
        let p = render_preview(s.as_bytes(), PAYLOAD_PREVIEW_CHARS);
        assert!(p.chars().count() <= PAYLOAD_PREVIEW_CHARS + 1);
        assert!(p.ends_with('…'));
    }

    #[test]
    fn render_preview_falls_back_to_hex_for_binary() {
        let bin = [0x00u8, 0xff, 0xab, 0xcd];
        let p = render_preview(&bin, 8);
        assert_eq!(p, "00ffabcd");
    }
}
