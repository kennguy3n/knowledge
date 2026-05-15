//! `inference_router` — on-device SLM inference routing for the
//! Knowledge substrate.
//!
//! Per `ARCHITECTURE.md` §3 and `docs/DESIGN.md` §6, every classification
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

pub mod adapter;
pub mod adapters;
pub mod config;
pub mod error;
pub mod router;
pub mod task;

pub use adapter::{AdapterKind, InferenceAdapter, ProbeResult};
#[cfg(feature = "http-client")]
pub use adapters::HttpLlamaServerClient;
pub use adapters::{FallbackAdapter, LlamaCppAdapter, LlamaServerClient, MlxAdapter};
pub use config::{DeviceTier, RouterConfig, IDLE_UNLOAD_TIMEOUT_SECS, WARM_UP_PROMPT};
pub use error::RouterError;
pub use router::InferenceRouter;
pub use task::{InferenceTask, SummaryBundle, TaskTag};
