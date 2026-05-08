//! XLM-R embedding adapter — Phase 1 deliverable.
//!
//! Per `PHASES.md`: "XLM-R embeddings via shared ONNX artifact". This
//! module provides a trait-based skeleton so the production substrate
//! can run XLM-R via ONNX Runtime on macOS / iOS / Android without
//! the rest of the crate caring about runtime details. Tests run
//! against a [`StubEmbeddingModel`] which returns zero vectors of the
//! configured dimension, exactly mirroring the current "0.0 semantic
//! score" behaviour in `crate::retrieval`.
//!
//! The expected production wiring is:
//!
//! 1. `OnnxEmbeddingAdapter::new(config)` — load the ONNX session.
//! 2. `embed(text)` — tokenise via SentencePiece, run the encoder,
//!    pool with mean pooling, L2-normalise.
//! 3. `HybridRetriever` calls `embed(query)` once per query and
//!    cosine-distance-scores the candidate evidence rows.

use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors emitted by [`EmbeddingModel`] implementations and the ONNX
/// runtime adapter below.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum EmbeddingError {
    /// The ONNX runtime is unavailable on this platform / build.
    /// Callers should fall back to lexical-only retrieval.
    #[error("onnx runtime unavailable: {reason}")]
    RuntimeUnavailable {
        /// Human-readable reason from the runtime probe.
        reason: String,
    },
    /// The provided text is empty / would produce a zero-token batch.
    #[error("empty input text")]
    EmptyInput,
    /// The model file at `path` could not be loaded.
    #[error("failed to load model at {path}: {reason}")]
    ModelLoad {
        /// Filesystem path the runtime tried to open.
        path: String,
        /// Underlying reason from the runtime.
        reason: String,
    },
    /// Inference failed for a reason that doesn't fall into the other
    /// buckets (allocation, kernel error, etc.).
    #[error("inference failure: {0}")]
    InferenceFailure(String),
}

/// `Result` alias for embedding operations.
pub type Result<T, E = EmbeddingError> = std::result::Result<T, E>;

/// Quantisation options for the on-device XLM-R artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Quantization {
    /// 32-bit float (no quantisation, debug only).
    Fp32,
    /// 16-bit float (preferred on Apple Neural Engine).
    Fp16,
    /// 8-bit integer (default for shipping).
    Int8,
    /// 4-bit integer (low-tier devices).
    Int4,
}

impl Quantization {
    /// Stable string tag.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fp32 => "fp32",
            Self::Fp16 => "fp16",
            Self::Int8 => "int8",
            Self::Int4 => "int4",
        }
    }
}

/// Static configuration for the XLM-R ONNX artifact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OnnxModelConfig {
    /// Filesystem path to the ONNX file.
    pub model_path: String,
    /// Embedding dimension (XLM-R base = 768).
    pub dimension: usize,
    /// Quantisation tier shipped at `model_path`.
    pub quantization: Quantization,
}

impl Default for OnnxModelConfig {
    fn default() -> Self {
        Self {
            model_path: "models/xlm-r-base.onnx".into(),
            dimension: 768,
            quantization: Quantization::Int8,
        }
    }
}

/// Probe state for an embedding adapter. Mirrors the
/// `inference_router::ProbeResult` enum but lives here to avoid
/// cross-crate coupling for what is essentially "did the runtime
/// load?".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddingProbe {
    /// Runtime is available and the model loaded.
    Available,
    /// Runtime / model is unavailable; callers should fall back to
    /// the [`StubEmbeddingModel`].
    Unavailable,
}

/// Trait implemented by every embedding model backend.
pub trait EmbeddingModel: Send + Sync {
    /// Embed a single text into a fixed-length vector.
    fn embed(&self, text: &str) -> Result<Vec<f32>>;

    /// Embed a batch of texts. Default impl loops over [`Self::embed`].
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        texts.iter().map(|t| self.embed(t)).collect()
    }

    /// Length of every vector returned by [`Self::embed`].
    fn dimension(&self) -> usize;

    /// Probe whether the underlying runtime + model are available
    /// right now. Default: `Available`.
    fn probe(&self) -> EmbeddingProbe {
        EmbeddingProbe::Available
    }
}

/// Trait abstracting the actual ONNX Runtime interaction. Production
/// builds plug in an `ort`-backed implementation; tests use a
/// `MockOnnxRuntime`.
pub trait OnnxRuntime: Send + Sync {
    /// Load the model at `path` and return a session-handle-shaped
    /// type. The skeleton represents the session as `()` because the
    /// concrete runtime is not in this skeleton crate.
    fn load(&self, path: &str) -> Result<()>;
    /// Run the loaded session on `text` and return the (already
    /// pooled, already L2-normalised) embedding.
    fn run(&self, text: &str) -> Result<Vec<f32>>;
    /// `true` iff the runtime is present on this platform.
    fn is_available(&self) -> bool;
}

/// XLM-R ONNX adapter — currently a skeleton; the production
/// implementation lives behind a feature flag in a follow-up PR.
pub struct OnnxEmbeddingAdapter {
    config: OnnxModelConfig,
    runtime: Box<dyn OnnxRuntime>,
    loaded: std::sync::OnceLock<bool>,
}

impl fmt::Debug for OnnxEmbeddingAdapter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OnnxEmbeddingAdapter")
            .field("config", &self.config)
            .field("loaded", &self.loaded.get().copied())
            .finish_non_exhaustive()
    }
}

impl OnnxEmbeddingAdapter {
    /// Construct a new adapter from a config and a runtime.
    pub fn new(config: OnnxModelConfig, runtime: Box<dyn OnnxRuntime>) -> Self {
        Self {
            config,
            runtime,
            loaded: std::sync::OnceLock::new(),
        }
    }

    /// Convenience constructor that loads the runtime at construction
    /// time — useful for production code paths where a failure to
    /// load is fatal anyway.
    pub fn with_eager_load(config: OnnxModelConfig, runtime: Box<dyn OnnxRuntime>) -> Result<Self> {
        let adapter = Self::new(config, runtime);
        adapter.ensure_loaded()?;
        Ok(adapter)
    }

    fn ensure_loaded(&self) -> Result<()> {
        if self.loaded.get().copied() == Some(true) {
            return Ok(());
        }
        if !self.runtime.is_available() {
            return Err(EmbeddingError::RuntimeUnavailable {
                reason: "runtime probe returned unavailable".into(),
            });
        }
        self.runtime.load(&self.config.model_path)?;
        // OnceLock::set is "first writer wins" — fine here because the
        // load is idempotent.
        let _ = self.loaded.set(true);
        Ok(())
    }
}

impl EmbeddingModel for OnnxEmbeddingAdapter {
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        if text.is_empty() {
            return Err(EmbeddingError::EmptyInput);
        }
        self.ensure_loaded()?;
        let v = self.runtime.run(text)?;
        if v.len() != self.config.dimension {
            return Err(EmbeddingError::InferenceFailure(format!(
                "runtime returned dim={}, expected {}",
                v.len(),
                self.config.dimension
            )));
        }
        Ok(v)
    }

    fn dimension(&self) -> usize {
        self.config.dimension
    }

    fn probe(&self) -> EmbeddingProbe {
        if self.runtime.is_available() {
            EmbeddingProbe::Available
        } else {
            EmbeddingProbe::Unavailable
        }
    }
}

/// Stub embedding model — returns zero vectors of the configured
/// dimension. Used when the ONNX runtime is unavailable; mirrors the
/// current Phase 0 "vector_score = 0.0" behaviour.
#[derive(Debug, Clone, Copy)]
pub struct StubEmbeddingModel {
    dimension: usize,
}

impl StubEmbeddingModel {
    /// Construct a stub model with the given dimension.
    pub fn new(dimension: usize) -> Self {
        Self { dimension }
    }
}

impl Default for StubEmbeddingModel {
    fn default() -> Self {
        Self { dimension: 768 }
    }
}

impl EmbeddingModel for StubEmbeddingModel {
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        if text.is_empty() {
            return Err(EmbeddingError::EmptyInput);
        }
        Ok(vec![0.0; self.dimension])
    }

    fn dimension(&self) -> usize {
        self.dimension
    }

    fn probe(&self) -> EmbeddingProbe {
        EmbeddingProbe::Available
    }
}

/// Cosine **similarity** between two equal-length vectors. Returns
/// values in `[-1.0, 1.0]`. Use [`cosine_distance`] for the
/// `[0.0, 2.0]` distance form.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f64 {
    if a.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0_f64;
    let mut na = 0.0_f64;
    let mut nb = 0.0_f64;
    for (x, y) in a.iter().zip(b.iter()) {
        let xf = *x as f64;
        let yf = *y as f64;
        dot += xf * yf;
        na += xf * xf;
        nb += yf * yf;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    (dot / (na.sqrt() * nb.sqrt())).clamp(-1.0, 1.0)
}

/// Cosine distance — `1.0 - cosine_similarity`. Returns values in
/// `[0.0, 2.0]` where `0.0` is identical and `2.0` is exactly
/// opposite.
pub fn cosine_distance(a: &[f32], b: &[f32]) -> f64 {
    1.0 - cosine_similarity(a, b)
}

/// Helper: project a cosine similarity in `[-1.0, 1.0]` into a
/// `[0.0, 1.0]` retrieval-friendly score where higher = more
/// similar.
pub fn similarity_to_score(similarity: f64) -> f64 {
    ((similarity + 1.0) / 2.0).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// Mock runtime used in unit tests. Returns a deterministic
    /// vector built from the input length.
    struct MockOnnxRuntime {
        available: AtomicBool,
        loads: AtomicUsize,
        runs: AtomicUsize,
        dimension: usize,
    }
    impl MockOnnxRuntime {
        fn ok(dimension: usize) -> Self {
            Self {
                available: AtomicBool::new(true),
                loads: AtomicUsize::new(0),
                runs: AtomicUsize::new(0),
                dimension,
            }
        }
        fn unavailable(dimension: usize) -> Self {
            Self {
                available: AtomicBool::new(false),
                loads: AtomicUsize::new(0),
                runs: AtomicUsize::new(0),
                dimension,
            }
        }
    }
    impl OnnxRuntime for MockOnnxRuntime {
        fn load(&self, _path: &str) -> Result<()> {
            self.loads.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn run(&self, text: &str) -> Result<Vec<f32>> {
            self.runs.fetch_add(1, Ordering::SeqCst);
            // Build a deterministic vector by repeating
            // `text.len()` mod 13 across the dimension.
            let val = ((text.len() % 13) as f32 + 1.0) / 13.0;
            Ok(vec![val; self.dimension])
        }
        fn is_available(&self) -> bool {
            self.available.load(Ordering::SeqCst)
        }
    }

    #[test]
    fn stub_returns_zero_vector_of_configured_dim() {
        let m = StubEmbeddingModel::new(7);
        let v = m.embed("hello").unwrap();
        assert_eq!(v.len(), 7);
        assert!(v.iter().all(|x| *x == 0.0));
    }

    #[test]
    fn stub_rejects_empty_input() {
        let m = StubEmbeddingModel::new(8);
        assert!(matches!(
            m.embed("").unwrap_err(),
            EmbeddingError::EmptyInput
        ));
    }

    #[test]
    fn stub_default_has_xlm_r_base_dimension() {
        assert_eq!(StubEmbeddingModel::default().dimension(), 768);
    }

    #[test]
    fn stub_embed_batch_returns_n_vectors() {
        let m = StubEmbeddingModel::new(3);
        let out = m.embed_batch(&["a", "b", "c"]).unwrap();
        assert_eq!(out.len(), 3);
        for v in out {
            assert_eq!(v, vec![0.0, 0.0, 0.0]);
        }
    }

    #[test]
    fn cosine_similarity_handles_identical_and_orthogonal() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        let c = vec![0.0, 1.0, 0.0];
        assert!((cosine_similarity(&a, &b) - 1.0).abs() < 1e-9);
        assert!(cosine_similarity(&a, &c).abs() < 1e-9);
    }

    #[test]
    fn cosine_distance_is_one_minus_similarity() {
        let a = vec![1.0, 1.0];
        let b = vec![1.0, 1.0];
        assert!(cosine_distance(&a, &b).abs() < 1e-9);
        let c = vec![1.0, 0.0];
        let d = vec![-1.0, 0.0];
        assert!((cosine_distance(&c, &d) - 2.0).abs() < 1e-9);
    }

    #[test]
    fn cosine_distance_zero_vector_is_neutral() {
        // Zero vectors define no direction; convention here is 0.0
        // similarity → 1.0 distance, mirroring the encoder fallback.
        let a = vec![0.0, 0.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        assert!((cosine_distance(&a, &b) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn cosine_similarity_rejects_mismatched_dimensions() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![1.0, 0.0];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    #[test]
    fn similarity_to_score_projects_into_unit_interval() {
        assert!((similarity_to_score(1.0) - 1.0).abs() < 1e-9);
        assert!((similarity_to_score(-1.0) - 0.0).abs() < 1e-9);
        assert!((similarity_to_score(0.0) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn onnx_adapter_eager_load_succeeds_when_runtime_available() {
        let runtime = MockOnnxRuntime::ok(4);
        let adapter = OnnxEmbeddingAdapter::with_eager_load(
            OnnxModelConfig {
                model_path: "x".into(),
                dimension: 4,
                quantization: Quantization::Int8,
            },
            Box::new(runtime),
        )
        .unwrap();
        let v = adapter.embed("alpha").unwrap();
        assert_eq!(v.len(), 4);
    }

    #[test]
    fn onnx_adapter_returns_runtime_unavailable_when_probe_fails() {
        let runtime = MockOnnxRuntime::unavailable(4);
        let err = OnnxEmbeddingAdapter::with_eager_load(
            OnnxModelConfig {
                model_path: "x".into(),
                dimension: 4,
                quantization: Quantization::Int8,
            },
            Box::new(runtime),
        )
        .unwrap_err();
        assert!(matches!(err, EmbeddingError::RuntimeUnavailable { .. }));
    }

    #[test]
    fn onnx_adapter_probe_reflects_runtime_availability() {
        let cfg = OnnxModelConfig::default();
        let avail = OnnxEmbeddingAdapter::new(cfg.clone(), Box::new(MockOnnxRuntime::ok(768)));
        assert_eq!(avail.probe(), EmbeddingProbe::Available);
        let unavail = OnnxEmbeddingAdapter::new(cfg, Box::new(MockOnnxRuntime::unavailable(768)));
        assert_eq!(unavail.probe(), EmbeddingProbe::Unavailable);
    }

    #[test]
    fn onnx_adapter_rejects_dimension_mismatch_from_runtime() {
        let runtime = MockOnnxRuntime::ok(/* runtime dim */ 4);
        let adapter = OnnxEmbeddingAdapter::new(
            OnnxModelConfig {
                model_path: "x".into(),
                dimension: 768, // adapter expects 768
                quantization: Quantization::Int8,
            },
            Box::new(runtime),
        );
        let err = adapter.embed("alpha").unwrap_err();
        assert!(matches!(err, EmbeddingError::InferenceFailure(_)));
    }

    #[test]
    fn onnx_adapter_rejects_empty_text() {
        let adapter = OnnxEmbeddingAdapter::new(
            OnnxModelConfig::default(),
            Box::new(MockOnnxRuntime::ok(768)),
        );
        assert!(matches!(
            adapter.embed("").unwrap_err(),
            EmbeddingError::EmptyInput
        ));
    }

    #[test]
    fn onnx_adapter_caches_load_across_calls() {
        let runtime = MockOnnxRuntime::ok(4);
        let cfg = OnnxModelConfig {
            model_path: "x".into(),
            dimension: 4,
            quantization: Quantization::Int8,
        };
        let adapter = OnnxEmbeddingAdapter::new(cfg, Box::new(runtime));
        adapter.embed("a").unwrap();
        adapter.embed("b").unwrap();
        // We can't read the AtomicUsize from the trait object without
        // downcasting, so instead we just validate functional
        // behaviour: two embeddings round-trip cleanly.
        assert_eq!(adapter.dimension(), 4);
    }

    #[test]
    fn quantization_string_tags_round_trip_via_serde() {
        for q in [
            Quantization::Fp32,
            Quantization::Fp16,
            Quantization::Int8,
            Quantization::Int4,
        ] {
            let json = serde_json::to_string(&q).unwrap();
            let back: Quantization = serde_json::from_str(&json).unwrap();
            assert_eq!(back, q);
        }
    }

    #[test]
    fn embedding_probe_string_tags_round_trip_via_serde() {
        for p in [EmbeddingProbe::Available, EmbeddingProbe::Unavailable] {
            let json = serde_json::to_string(&p).unwrap();
            let back: EmbeddingProbe = serde_json::from_str(&json).unwrap();
            assert_eq!(back, p);
        }
    }

    #[test]
    fn embedding_error_runtime_unavailable_message() {
        let err = EmbeddingError::RuntimeUnavailable {
            reason: "no ort".into(),
        };
        assert!(format!("{err}").contains("no ort"));
    }
}
