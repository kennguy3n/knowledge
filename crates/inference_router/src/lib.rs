//! `inference_router` — on-device SLM inference routing for the
//! Knowledge substrate.
//!
//! Per `docs/technical/architecture.md` §3 and `docs/technical/design.md` §6, every classification
//! / extraction / synthesis call into a Small Language Model goes
//! through one place: the [`InferenceRouter`]. The router holds an
//! ordered list of [`InferenceAdapter`]s — currently `MLX → llama.cpp
//! → Fallback` — probes them at boot, and dispatches every
//! [`InferenceTask`] to the highest-priority adapter that is available
//! and supports the task.
//!
//! The router is deliberately small and synchronous so it can be
//! unit-tested against in-memory mock adapters; the production runtime
//! drives it from background tokio tasks.

#![cfg_attr(docsrs, feature(doc_cfg))]
#![deny(missing_docs)]

// STABLE
pub mod adapter;
// UNSTABLE — adapter implementations; internal wiring may change.
#[doc(hidden)]
pub mod adapters;
// STABLE
pub mod config;
// STABLE
pub mod error;
// STABLE
pub mod latency;
// STABLE
pub mod router;
// STABLE
pub mod task;

// STABLE
pub use adapter::{AdapterKind, InferenceAdapter, ProbeResult};
// STABLE
#[cfg(feature = "http-client")]
pub use adapters::HttpLlamaServerClient;
// UNSTABLE — adapter internals; prefer InferenceRouter.
#[doc(hidden)]
pub use adapters::{
    get_mlx_generate_fn, set_mlx_generate_fn, set_mlx_runtime_linked, FallbackAdapter,
    LlamaCppAdapter, LlamaServerClient, MlxAdapter, MlxGenerateFn,
};
// STABLE
pub use config::{DeviceTier, RouterConfig, IDLE_UNLOAD_TIMEOUT_SECS, WARM_UP_PROMPT};
// STABLE
pub use error::RouterError;
// STABLE
pub use latency::{LatencyHistogram, LATENCY_BUCKETS_SECONDS};
// STABLE
pub use router::{AdapterState, DispatchLatency, InferenceRouter};
// STABLE
pub use task::{InferenceTask, SummaryBundle, TaskTag};
