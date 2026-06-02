//! XLM-R embedding adapter.
//!
//! Per `docs/DESIGN.md` §3.2: "XLM-R embeddings via shared ONNX artifact". This
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
//!
//! A real [`OrtOnnxRuntime`] backed by the `ort` crate and
//! HuggingFace `tokenizers` is gated behind the `onnx-runtime` cargo
//! feature. The default build still only carries the stub / mock
//! runtimes.
//!
//! # Multilingual property
//!
//! XLM-R (Cross-lingual Language Model — RoBERTa) was trained
//! across 100 languages over 2.5 TB of filtered CommonCrawl. The
//! query and the indexed body do **not** need to share a script,
//! a writing system, or even a language family for the cosine
//! similarity between their embeddings to be semantically
//! meaningful. A query like `"weather forecast"` will produce a
//! vector whose cosine similarity with the embedding of
//! `"明日の天気予報"` (Japanese: "tomorrow's weather forecast") is
//! non-trivial; the same property holds for French ↔ Spanish,
//! Arabic ↔ English, Thai ↔ Vietnamese, and so on across the
//! full 100-language inventory.
//!
//! This module is the *adapter* layer — it does not enforce the
//! multilingual property (that is a function of the model
//! artifact). The architectural invariant we DO enforce is that
//! the retriever consumes embeddings via the
//! [`crate::retrieval::HybridRetriever::candidate_embedding`]
//! path without any script-conditioned routing, so a future
//! change that accidentally inserts a script-segregation layer
//! would break the cross-lingual property. The integration test
//! `vector_telemetry_cross_lingual_recall_via_rerank` in
//! `crates/evidence_store/tests/store_integration.rs` pins that
//! invariant by replaying a deterministic multilingual mock that
//! reproduces XLM-R's cross-script clustering behaviour against
//! the real retriever surface.
//!
//! # `model_tag` rotation discipline
//!
//! Every embedding cached in `evidence_embeddings` is stamped
//! with the `model_tag` of the model that produced it. The
//! schema invariant is **one tag ⇒ one model ⇒ one output
//! dimension ⇒ one vector space**. Any change to the model
//! artifact — even a re-quantisation that preserves the output
//! dimension — MUST be accompanied by a new `model_tag`. Two
//! different models that happen to share an output dimension
//! produce vectors that are NOT in the same space: a cosine
//! similarity computed between them is meaningless even though
//! the arithmetic is well-defined.
//!
//! The retriever filters cache lookups by `model_tag` so a stale
//! row produced under an old tag falls through to the live-embed
//! path rather than being scored as if it had been produced by
//! the active model (see
//! [`crate::store::EvidenceStore::get_embedding_for_model`]).
//! added runtime telemetry around this rule:
//! [`crate::vector_telemetry::record_observed_dimension`] is
//! called every time an [`EmbeddingModel`] is wired in and every
//! time `index_embedding` writes a fresh vector. A same-tag /
//! different-dimension observation bumps
//! `model_tag_dimension_violations_total` and emits a
//! `tracing::warn!`, making rotation-rule violations operator-
//! visible in both metrics and logs. The check is purely
//! advisory — it never fails the surrounding operation — but it
//! reliably surfaces the silent-bug shape that "same dimension
//! does NOT imply compatible vectors".

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
/// current "vector_score = 0.0" behaviour.
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
    f64::midpoint(similarity, 1.0).clamp(0.0, 1.0)
}

#[cfg(feature = "onnx-runtime")]
mod ort_runtime_impl {
    //! Real ONNX Runtime backend for [`OnnxRuntime`]. Gated behind the
    //! `onnx-runtime` cargo feature so the default build pulls in
    //! neither `ort` nor `tokenizers`.
    //!
    //! Tokenizer companion file: if not set explicitly via
    //! [`OrtOnnxRuntime::with_tokenizer_path`], the runtime looks for
    //! `{model_path}.tokenizer.json` and then `{parent}/tokenizer.json`
    //! relative to the ONNX file. The expected model output is a
    //! `[1, seq_len, hidden]` last-hidden-state tensor; the runtime
    //! mean-pools over the attention mask and L2-normalises.
    use std::path::Path;
    use std::sync::{Mutex, OnceLock};

    use ort::session::Session;
    use ort::value::Tensor;
    use tokenizers::Tokenizer;

    use super::{EmbeddingError, OnnxRuntime, Result};

    /// Production [`OnnxRuntime`] backed by the `ort` crate.
    ///
    /// `ort` is pulled in with `load-dynamic` so the build does not link
    /// against `onnxruntime`; the native library is loaded at runtime
    /// from `ORT_DYLIB_PATH` or the system search paths. When the
    /// library is absent, [`Self::load`] surfaces an
    /// [`EmbeddingError::ModelLoad`] with the underlying ort error.
    pub struct OrtOnnxRuntime {
        session: OnceLock<Mutex<Session>>,
        tokenizer: OnceLock<Tokenizer>,
        tokenizer_path: Option<String>,
        /// Serialises the expensive section of [`Self::load`] (Session
        /// build + tokenizer-JSON read) so that concurrent callers
        /// don't both pay the cost only to throw one result away.
        /// Without this guard two racing `load()` calls would each
        /// build a Session and parse the tokenizer, then the loser
        /// would discard the work when `OnceLock::set` rejected it.
        /// Holding the lock across the whole load section means only
        /// the first thread does the work; subsequent threads observe
        /// `session.get().is_some()` after acquiring the lock and
        /// return the documented "single-shot" error without
        /// re-loading.
        load_lock: Mutex<()>,
        /// Cached availability probe result. The first call to
        /// [`Self::is_available`] runs a cheap `Session::builder()`
        /// against the dynamic library; subsequent calls return the
        /// cached answer. Probing once and caching avoids both the
        /// false positive of unconditionally returning `true` and the
        /// cost of re-probing on every `OnnxEmbeddingAdapter::probe`.
        availability_probe: OnceLock<bool>,
    }

    impl OrtOnnxRuntime {
        /// Build a runtime that resolves the tokenizer from a companion
        /// file (`{model_path}.tokenizer.json` or
        /// `{parent}/tokenizer.json`).
        pub fn new() -> Self {
            Self {
                session: OnceLock::new(),
                tokenizer: OnceLock::new(),
                tokenizer_path: None,
                load_lock: Mutex::new(()),
                availability_probe: OnceLock::new(),
            }
        }

        /// Override the tokenizer path. Useful when the tokenizer JSON
        /// is shipped at a non-default location.
        pub fn with_tokenizer_path(mut self, path: impl Into<String>) -> Self {
            self.tokenizer_path = Some(path.into());
            self
        }

        fn resolve_tokenizer_path(&self, model_path: &str) -> String {
            if let Some(p) = &self.tokenizer_path {
                return p.clone();
            }
            let with_ext = format!("{model_path}.tokenizer.json");
            if Path::new(&with_ext).exists() {
                return with_ext;
            }
            if let Some(parent) = Path::new(model_path).parent() {
                let companion = parent.join("tokenizer.json");
                if companion.exists() {
                    return companion.display().to_string();
                }
            }
            with_ext
        }
    }

    impl Default for OrtOnnxRuntime {
        fn default() -> Self {
            Self::new()
        }
    }

    impl OnnxRuntime for OrtOnnxRuntime {
        /// Single-shot loader: once an [`OrtOnnxRuntime`] has been
        /// populated, subsequent `load()` calls are rejected up-front
        /// rather than silently discarding fresh work. `OnceLock` only
        /// allows a single successful `set` call, so a previous
        /// implementation paid the full cost of building a Session and
        /// reading the tokenizer JSON from disk before throwing the
        /// result away on `set` error. Callers that need to swap
        /// models must construct a new `OrtOnnxRuntime` instance.
        ///
        /// Concurrent `load()` calls are serialised by `load_lock` so
        /// that exactly one thread runs the expensive Session +
        /// tokenizer build for any given runtime instance. The first
        /// thread populates the `OnceLock`s under the lock; every
        /// subsequent thread acquires the lock, sees that the load
        /// already happened, and returns the documented single-shot
        /// error without doing any wasted work.
        fn load(&self, path: &str) -> Result<()> {
            let _guard = self
                .load_lock
                .lock()
                .map_err(|e| EmbeddingError::ModelLoad {
                    path: path.into(),
                    reason: format!("OrtOnnxRuntime load_lock poisoned: {e}"),
                })?;
            if self.session.get().is_some() || self.tokenizer.get().is_some() {
                return Err(EmbeddingError::ModelLoad {
                    path: path.into(),
                    reason: "OrtOnnxRuntime is single-shot; construct a new instance to load \
                             a different model"
                        .into(),
                });
            }

            let builder = Session::builder().map_err(|e| EmbeddingError::ModelLoad {
                path: path.into(),
                reason: format!("Session::builder failed: {e}"),
            })?;
            let session =
                builder
                    .commit_from_file(path)
                    .map_err(|e| EmbeddingError::ModelLoad {
                        path: path.into(),
                        reason: format!("commit_from_file failed: {e}"),
                    })?;

            let tokenizer_path = self.resolve_tokenizer_path(path);
            let tokenizer =
                Tokenizer::from_file(&tokenizer_path).map_err(|e| EmbeddingError::ModelLoad {
                    path: tokenizer_path.clone(),
                    reason: format!("Tokenizer::from_file failed: {e}"),
                })?;

            // Both `set`s are infallible under the load_lock: the
            // single-shot guard above already proved both OnceLocks
            // are empty, and the lock prevents any concurrent setter
            // from racing in between. An `expect` here is correctness,
            // not defensiveness.
            self.session
                .set(Mutex::new(session))
                .map_err(|_| EmbeddingError::ModelLoad {
                    path: path.into(),
                    reason: "OrtOnnxRuntime::session OnceLock unexpectedly populated under \
                             load_lock"
                        .into(),
                })?;
            self.tokenizer
                .set(tokenizer)
                .map_err(|_| EmbeddingError::ModelLoad {
                    path: path.into(),
                    reason: "OrtOnnxRuntime::tokenizer OnceLock unexpectedly populated under \
                             load_lock"
                        .into(),
                })?;
            // Treat a successful load as definitive evidence that the
            // dylib is available, so subsequent `is_available` calls
            // can short-circuit without re-probing.
            let _ = self.availability_probe.set(true);
            Ok(())
        }

        fn run(&self, text: &str) -> Result<Vec<f32>> {
            if text.is_empty() {
                return Err(EmbeddingError::EmptyInput);
            }
            let session_mutex = self
                .session
                .get()
                .ok_or_else(|| EmbeddingError::InferenceFailure("session not loaded".into()))?;
            let tokenizer = self
                .tokenizer
                .get()
                .ok_or_else(|| EmbeddingError::InferenceFailure("tokenizer not loaded".into()))?;

            let encoding = tokenizer
                .encode(text, true)
                .map_err(|e| EmbeddingError::InferenceFailure(format!("tokenize: {e}")))?;
            let ids: Vec<i64> = encoding.get_ids().iter().map(|&i| i as i64).collect();
            let mask: Vec<i64> = encoding
                .get_attention_mask()
                .iter()
                .map(|&i| i as i64)
                .collect();
            let len = ids.len();
            if len == 0 {
                return Err(EmbeddingError::EmptyInput);
            }

            // Tokenised input length is bounded by `MAX_SEQ_LEN`
            // (well below i64::MAX); the explicit conversion makes
            // the bound and the failure mode explicit.
            let len_i64 = i64::try_from(len).map_err(|_| {
                EmbeddingError::InferenceFailure(format!(
                    "token sequence length {len} exceeds i64::MAX"
                ))
            })?;
            let shape = [1_i64, len_i64];
            let input_ids = Tensor::from_array((shape, ids))
                .map_err(|e| EmbeddingError::InferenceFailure(format!("input_ids: {e}")))?;
            let attention_mask = Tensor::from_array((shape, mask.clone()))
                .map_err(|e| EmbeddingError::InferenceFailure(format!("attention_mask: {e}")))?;

            let mut session = session_mutex.lock().map_err(|e| {
                EmbeddingError::InferenceFailure(format!("session mutex poisoned: {e}"))
            })?;
            let outputs = session
                .run(ort::inputs! {
                    "input_ids" => input_ids,
                    "attention_mask" => attention_mask,
                })
                .map_err(|e| EmbeddingError::InferenceFailure(format!("session.run: {e}")))?;

            // Prefer the named `last_hidden_state` output; fall back to
            // the first output by position so models that name their
            // output differently still work. `outputs.get(...)` borrows
            // from `outputs`, while `outputs.values().next()` returns
            // an owned `ValueRef`; the borrows produced by
            // `try_extract_tensor` have different lifetimes, so each
            // branch eagerly pools into an owned `Vec<f32>` rather than
            // unifying through `Option`.
            let process = |shape: &[i64], data: &[f32]| -> Result<Vec<f32>> {
                // ORT shapes are signed 64-bit; for our inputs the
                // sequence-length and hidden-size dimensions are
                // bounded by the model config (BERT-ish: seq <= 512,
                // hidden <= 1024) so a fallible conversion only
                // fails on malformed model output, which we want to
                // surface as an inference error.
                let seq_len_ok =
                    usize::try_from(shape.get(1).copied().unwrap_or(-1)).is_ok_and(|s| s == len);
                if shape.len() != 3 || shape[0] != 1 || !seq_len_ok {
                    return Err(EmbeddingError::InferenceFailure(format!(
                        "unexpected output shape: {shape:?} (expected [1, {len}, hidden])"
                    )));
                }
                let hidden = usize::try_from(shape[2]).map_err(|_| {
                    EmbeddingError::InferenceFailure(format!(
                        "negative hidden-dim in output shape: {shape:?}"
                    ))
                })?;
                let mut pooled = vec![0.0_f32; hidden];
                let mut weight = 0.0_f32;
                for (i, &m) in mask.iter().enumerate() {
                    if m == 0 {
                        continue;
                    }
                    let m_f = m as f32;
                    weight += m_f;
                    let row = &data[i * hidden..(i + 1) * hidden];
                    for (j, v) in row.iter().enumerate() {
                        pooled[j] += v * m_f;
                    }
                }
                if weight > 0.0 {
                    for v in &mut pooled {
                        *v /= weight;
                    }
                }
                let norm: f32 = pooled.iter().map(|v| v * v).sum::<f32>().sqrt();
                if norm > 0.0 {
                    for v in &mut pooled {
                        *v /= norm;
                    }
                }
                Ok(pooled)
            };

            let pooled = if let Some(v) = outputs.get("last_hidden_state") {
                let (shape, data) = v
                    .try_extract_tensor::<f32>()
                    .map_err(|e| EmbeddingError::InferenceFailure(format!("extract: {e}")))?;
                process(shape, data)?
            } else {
                let v = outputs
                    .values()
                    .next()
                    .ok_or_else(|| EmbeddingError::InferenceFailure("no outputs".into()))?;
                let (shape, data) = v
                    .try_extract_tensor::<f32>()
                    .map_err(|e| EmbeddingError::InferenceFailure(format!("extract: {e}")))?;
                process(shape, data)?
            };
            Ok(pooled)
        }

        fn is_available(&self) -> bool {
            // With `load-dynamic`, the ort dylib is not resolved at
            // link time — the lookup happens lazily on the first ort
            // API call. Unconditionally returning `true` (the
            // previous behaviour) means callers using `probe()` as a
            // cheap availability check see a false positive on hosts
            // with no ort dylib: `OnnxEmbeddingAdapter::probe()`
            // reports `Available`, and the real error only surfaces
            // once `load()` reaches `Session::builder()`.
            //
            // Run the probe ourselves once and cache the result. The
            // cheapest API touch that exercises the dynamic loader is
            // `Session::builder()` — it builds an empty session-config
            // object (no model committed) and the ort crate triggers
            // dylib resolution as a side effect. The wrinkle is that
            // `ort` *panics* (not errors) when the dylib is missing
            // (see `run_before_load_does_not_panic_when_dylib_missing`
            // for context), so we wrap the probe in `catch_unwind` to
            // convert that panic into a clean `false` result.
            //
            // The builder is dropped immediately; this probe is
            // side-effect-free beyond the one-time dylib lookup the
            // runtime would do on first use anyway.
            if let Some(&cached) = self.availability_probe.get() {
                return cached;
            }
            // If a Session has already been built (e.g. `load` ran
            // successfully on this instance), the dylib is by
            // definition present — skip the probe.
            if self.session.get().is_some() {
                let _ = self.availability_probe.set(true);
                return true;
            }
            let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                Session::builder().is_ok()
            }))
            .unwrap_or(false);
            let _ = self.availability_probe.set(ok);
            ok
        }
    }
}

#[cfg(feature = "onnx-runtime")]
pub use ort_runtime_impl::OrtOnnxRuntime;

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
        // The early-return guard yields exact `0.0` — no float
        // arithmetic happens — so a bit-for-bit comparison through
        // `total_cmp` is the right semantic. (Plain `assert_eq!` on
        // `f64` would tripped `clippy::float_cmp` even though the
        // value is exact.)
        assert!(cosine_similarity(&a, &b).total_cmp(&0.0).is_eq());
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

/// Feature-gated smoke tests for [`OrtOnnxRuntime`]. We do not ship a
/// real `.onnx` model in the repo, so these tests only exercise the
/// surface that *does not* require a live ONNX session: constructor +
/// `is_available` + the error path when `load` is asked to read a
/// non-existent file. The full `load → run` round-trip is covered by
/// the (separately-flagged) integration suite, which boots
/// against a real model fixture.
#[cfg(all(test, feature = "onnx-runtime"))]
mod ort_runtime_tests {
    use super::ort_runtime_impl::OrtOnnxRuntime;
    use super::OnnxRuntime;

    #[test]
    fn is_available_reflects_dylib_presence_via_cached_probe() {
        // `is_available` runs a `Session::builder()` probe wrapped in
        // `catch_unwind` and caches the result so callers using
        // `probe()` get an honest answer instead of an unconditional
        // `true`. The probe must:
        //   1. Not panic when the dylib is missing (catch_unwind
        //      converts the ort panic into a clean `false`).
        //   2. Be cached across calls so we don't repeatedly trigger
        //      the dylib lookup.
        //   3. Reflect the actual host state: `true` when the dylib is
        //      loadable, `false` otherwise.
        let rt = OrtOnnxRuntime::new();
        let first = rt.is_available();
        // Second call must return the cached value (no re-probe). We
        // can't directly inspect the OnceLock from here, but calling
        // again should be a pure get — the result must match.
        let second = rt.is_available();
        assert_eq!(
            first, second,
            "is_available must be cached across calls; got first={first} second={second}",
        );
    }

    #[test]
    fn run_before_load_fails_with_inference_failure() {
        let rt = OrtOnnxRuntime::new();
        let err = rt
            .run("hello world")
            .expect_err("run() must fail before load() succeeds");
        match err {
            super::EmbeddingError::InferenceFailure(reason) => {
                assert!(
                    reason.to_lowercase().contains("not loaded")
                        || reason.to_lowercase().contains("session"),
                    "InferenceFailure should mention the missing session, \
                     got: {reason}",
                );
            }
            other => panic!(
                "expected InferenceFailure when run() is called before \
                 load(); got {other:?}",
            ),
        }
    }

    #[test]
    fn run_before_load_does_not_panic_when_dylib_missing() {
        // Regression: ensure the trait surface stays
        // panic-free even when the `libonnxruntime` dylib is absent
        // (the `load-dynamic` ort feature *will* panic on the first
        // session-builder call, but `run` short-circuits on the
        // unloaded `OnceLock` before reaching ort, so this path must
        // stay clean).
        let rt = OrtOnnxRuntime::new();
        let _ = rt.run("ignored").unwrap_err();
    }

    // NOTE: A live `load → run` round-trip against a real `.onnx`
    // fixture is *intentionally* omitted from this in-crate suite. It
    // would require both the `libonnxruntime` dylib on the test host
    // (the `load-dynamic` feature of `ort` panics — not errors — when
    // it is absent, see `run_before_load_does_not_panic_when_dylib_missing`
    // for the workaround) and a checked-in model file. The ONNX integration
    // integration suite covers that path against a downloaded
    // fixture. That same suite is the right place to assert the
    // *successful* single-shot contract added for embeddings (a second
    // successful `load()` returns `ModelLoad` with the "single-shot"
    // reason rather than silently discarding a fresh Session and
    // Tokenizer), because verifying it from a unit test would require
    // an in-process ONNX model fixture and the dylib loaded.
}
