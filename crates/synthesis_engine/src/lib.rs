//! `synthesis_engine` — server-side synthesis engine skeleton.
//!
//! The substrate ships a server-side synthesis service composed of a
//! Go gateway and a Rust synthesis engine. This crate is the **Rust
//! side**; the Go gateway is not yet implemented.
//!
//! The engine exposes two methods:
//!
//! * [`SynthesisEngine::synthesize_domain`] — consume the channel
//!   outputs registered on a [`memory_manager::DomainMemoryObject`]
//!   and emit a [`synthesis_pipeline::SynthesisObject`] of type
//!   [`synthesis_pipeline::SynthesisObjectType::DomainSummary`].
//! * [`SynthesisEngine::synthesize_tenant`] — consume the domain
//!   outputs + approved-document references registered on a
//!   [`memory_manager::TenantMemoryObject`] and emit a
//!   [`synthesis_pipeline::SynthesisObject`] of type
//!   [`synthesis_pipeline::SynthesisObjectType::TenantSummary`].
//!
//! Both methods enforce the hierarchy rules in
//! [`synthesis_pipeline::hierarchy`] — they take typed
//! [`synthesis_pipeline::DomainSynthesisInput`] /
//! [`synthesis_pipeline::TenantSynthesisInput`] arguments and refuse
//! to operate on raw evidence rows or cross-scope objects.
//!
//! Ships two leaf implementations of the [`SynthesisEngine`] trait
//! plus one TEE-attested wrapper that delegates to the production
//! leaf:
//!
//! * [`ManagedEndpointSynthesizer`] (in the `stub` module) — a
//!   deterministic test scaffold that concatenates the input
//!   payloads with a hierarchy-tier prefix. Used by end-to-end
//!   tests and the demo binary to pin contract behaviour without
//!   issuing real network calls. The `stub` name is descriptive,
//!   not aspirational — a deterministic concatenator is exactly
//!   what those tests need.
//! * [`HttpManagedEndpointSynthesizer`] (in the `managed_endpoint`
//!   module) — the production synthesizer. POSTs the serialised
//!   channel- / domain-input payloads to a managed AI endpoint
//!   over the framework's `HttpClient`, parses the response, and
//!   emits the corresponding `SynthesisObject`.
//! * [`TeeWorker`] (in the `tee_worker` module) — the TEE-attested
//!   wrapper. Also implements `SynthesisEngine` directly so hosts
//!   can dispatch through a single trait object, but its
//!   `synthesize_domain` / `synthesize_tenant` bodies just wrap
//!   `enter_synthesizing` / `exit_synthesizing` attestation guards
//!   around a delegated call to its embedded
//!   `HttpManagedEndpointSynthesizer`. Not a third leaf
//!   implementation — a wrapper that adds the attestation
//!   choreography around the leaf.

#![deny(missing_docs)]

#[cfg(all(feature = "test-support", not(debug_assertions)))]
compile_error!("test-support must not be enabled in release builds");

pub mod engine;
pub mod error;
pub mod managed_endpoint;
pub mod stub;
pub mod tee_worker;

pub use engine::{DomainSynthesisResult, SynthesisEngine, TenantSynthesisResult};
pub use error::{EngineError, Result};
pub use managed_endpoint::{
    EndpointConfig, EndpointError, HttpClient, HttpManagedEndpointSynthesizer, InputObjectRef,
    MockHttpClient, SynthesisRequest, SynthesisResponse, DEFAULT_DOMAIN_PROMPT, DEFAULT_MAX_TOKENS,
    DEFAULT_TENANT_PROMPT, DEFAULT_TIMEOUT,
};
pub use stub::ManagedEndpointSynthesizer;
