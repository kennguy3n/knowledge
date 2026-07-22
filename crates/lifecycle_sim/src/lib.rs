//! Comprehensive real-world lifecycle benchmarking and dataset generation
//! for the Knowledge substrate.
//!
//! This crate generates deterministic, large-scale, multi-tenant /
//! multi-user / multi-community datasets across 22+ languages with real
//! media files, replays them through the full Knowledge substrate
//! lifecycle (ingest → extract → retrieve → synthesize → decay → forget),
//! and verifies correctness per-turn with JSON + markdown reporting.
//!
//! # Quick start
//!
//! ```no_run
//! use lifecycle_sim::{ScalePreset, run_simulation, DriverKind};
//!
//! let report = run_simulation(ScalePreset::Quick, DriverKind::RustNative, 42, None);
//! println!("Pass rate: {:.2}%", report.summary.pass_rate * 100.0);
//! ```

#![deny(missing_docs)]

pub mod dataset;
pub mod drivers;
pub mod media;
pub mod replay;
pub mod report;
pub mod scenarios;
pub mod verify;
pub mod world;

pub use dataset::{ScalePreset, SimConfig, WorldDataset};
pub use drivers::{
    ConceptGraphSnapshot, ContradictionResult, DecayResult, DriverKind, DriftResult,
    ExplainQueryResult, HealthCheck, IngestResult, MemoryRecord, QueryHit, SynthesisResult,
};
pub use replay::{run_simulation, run_simulation_with_config, SimReport};
pub use report::ReportOutput;
pub use world::{ScopeKind, SimScope, SimTenant, SimUser, World};
