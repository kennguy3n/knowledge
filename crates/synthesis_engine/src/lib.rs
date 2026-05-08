//! `synthesis_engine` — server-side synthesis engine skeleton.
//!
//! Per `PHASES.md` Phase 3, the substrate ships a server-side
//! synthesis service composed of a Go gateway and a Rust synthesis
//! engine. This crate is the **Rust side**; the Go gateway lands in a
//! later phase.
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
//! Phase 3 ships a [`ManagedEndpointSynthesizer`] stub that
//! deterministically concatenates the input payloads. The real
//! managed-AI endpoint adapter lands when the SLM gateway is wired
//! through.

#![deny(missing_docs)]

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
