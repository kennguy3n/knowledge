//! Apple Silicon MLX adapter.
//!
//! The production adapter binds to the MLX runtime via Swift / Obj-C++
//! and runs the SLM with INT4 quantisation on the Apple Neural Engine.
//! In Rust-only land this crate ships a skeleton: [`MlxAdapter`]
//! detects whether it is running on Apple Silicon, gates dispatch by
//! the configured device tier, and **verifies the MLX runtime is
//! actually linked** before reporting itself as `Available`. If the
//! runtime is absent (as in the pure-Rust crate), both `probe()` and
//! `generate()` return [`crate::RouterError::Unavailable`] so the
//! router falls through to the next adapter (e.g. llama.cpp).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, RwLock};

use crate::adapter::{AdapterKind, InferenceAdapter, ProbeResult};
use crate::config::{DeviceTier, RouterConfig, SamplingConfig};
use crate::error::RouterError;
use crate::task::InferenceTask;

/// Signature of the native MLX `generate` callback registered by the
/// iOS / macOS Swift shell.
///
/// Parameters mirror [`InferenceAdapter::generate`]:
/// * `task_tag` — stable task identifier (`"synth_summary"`, …).
/// * `prompt` — fully-rendered prompt text the SLM should answer.
/// * `grammar` — GBNF grammar constraint; empty string when the task
///   does not need one.
///
/// On success the callback returns the model's textual output (the
/// JSON that the GBNF grammar constrains). On failure it returns an
/// `Err(String)` describing why; the adapter wraps that string in a
/// [`RouterError::InferenceFailure`] which the router treats as a
/// non-fallback error (the router does NOT try other adapters after
/// a hard inference failure — the model itself ran but produced
/// invalid output).
pub type MlxGenerateFn = fn(task_tag: &str, prompt: &str, grammar: &str) -> Result<String, String>;

/// Global slot for the native MLX generate callback.
///
/// `MlxGenerateFn` is a plain `fn` pointer (`Copy` + `Send` + `Sync`),
/// so the `Option<MlxGenerateFn>` inside the lock is the entire state:
/// `None` means "no callback registered", `Some(f)` means "call `f`".
///
/// We use an `RwLock` so the hot path (`MlxAdapter::generate`) takes
/// the *read* lock, which is uncontended even under concurrent
/// inference dispatches — the only write happens at boot when the
/// Swift shell registers the callback. The lock is necessary because
/// the substrate forbids `unsafe`, ruling out the `AtomicPtr` /
/// `transmute<usize, fn(…)>` shortcut.
static MLX_GENERATE_FN: LazyLock<RwLock<Option<MlxGenerateFn>>> =
    LazyLock::new(|| RwLock::new(None));

/// Register the native MLX generate callback.
///
/// The iOS / macOS native shell calls this at app launch, after it
/// has constructed its MLX engine and is ready to serve inference
/// calls. The Rust adapter calls the registered function from inside
/// [`MlxAdapter::generate`] when the runtime-linked flag
/// ([`set_mlx_runtime_linked`]) is set.
///
/// Calling this more than once replaces the previous callback. The
/// substrate's expected usage is a single registration at boot, but
/// re-registration is supported so the host can swap implementations
/// when reconfiguring the SLM (e.g. switching device tiers without a
/// process restart) and so unit tests can exercise both the success
/// and failure branches within the same `cargo test` process.
pub fn set_mlx_generate_fn(f: MlxGenerateFn) {
    let mut guard = MLX_GENERATE_FN
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if guard.is_some() {
        // Production usage expects a single registration at app
        // launch; a second call indicates one of (a) a misconfigured
        // shell registering twice, (b) a hot-reload scenario, or (c)
        // a test swapping the implementation. Emit a `warn` so the
        // double-registration shows up in the substrate's audit
        // log instead of silently swapping the in-flight callback
        // out from under live dispatchers.
        tracing::warn!(
            "MLX generate callback re-registered \u{2014} discarding previous registration. \
             Expected usage is a single registration at app launch; double registration \
             may indicate a misconfigured platform shell.",
        );
    }
    *guard = Some(f);
}

/// Read the currently registered generate callback, or `None` when
/// no callback has been registered yet.
///
/// Used internally by [`MlxAdapter::generate`] on every dispatch and
/// exposed publicly so the host (and tests) can probe registration
/// state without invoking the model. The returned function pointer
/// is owned by the caller (the `RwLock` is released before this
/// function returns) so concurrent re-registration cannot race with
/// an in-flight dispatch.
pub fn get_mlx_generate_fn() -> Option<MlxGenerateFn> {
    let guard = MLX_GENERATE_FN
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *guard
}

/// Test-only helper: clear the registered callback. Production code
/// never calls this — the only motivation is letting unit tests
/// exercise both the "no callback" and "callback registered" branches
/// of [`MlxAdapter::generate`] within the same `cargo test` process.
#[cfg(any(test, feature = "test-support"))]
pub fn clear_mlx_generate_fn() {
    let mut guard = MLX_GENERATE_FN
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *guard = None;
}

/// Signature of the *optional*, sampling-aware native MLX generate
/// callback. Identical to [`MlxGenerateFn`] but additionally receives
/// the per-call [`SamplingConfig`], so the native runtime can honour the
/// synthesis adaptive token budget / verify-and-retry larger `n_predict`
/// (and the deterministic seed + sampling knobs) instead of falling back
/// to its built-in generation defaults.
///
/// Registering this is **additive and fully optional**: a shell that only
/// registers [`set_mlx_generate_fn`] keeps working exactly as before —
/// [`MlxAdapter::generate_with_sampling`] then delegates to the
/// sampling-unaware callback, so the verify-and-retry *decision* and the
/// fact-only retry prompt still apply on MLX, only the per-call token
/// budget is a no-op. A shell that can map these knobs onto its MLX
/// engine registers this callback too, and the adapter routes through it
/// so the budget actually reaches the runtime. The plain callback remains
/// the wire for every non-synthesis (classification/extraction) task.
pub type MlxGenerateWithSamplingFn = fn(
    task_tag: &str,
    prompt: &str,
    grammar: &str,
    sampling: &SamplingConfig,
) -> Result<String, String>;

/// Global slot for the optional sampling-aware MLX generate callback.
/// Mirrors [`MLX_GENERATE_FN`]: an `RwLock` whose hot path is the
/// uncontended read taken on every sampling-aware dispatch, written only
/// once at boot when the native shell registers its callback.
static MLX_GENERATE_WITH_SAMPLING_FN: LazyLock<RwLock<Option<MlxGenerateWithSamplingFn>>> =
    LazyLock::new(|| RwLock::new(None));

/// Register the optional sampling-aware native MLX generate callback
/// (see [`MlxGenerateWithSamplingFn`]). Re-registration replaces the
/// previous callback and warns, matching [`set_mlx_generate_fn`].
pub fn set_mlx_generate_with_sampling_fn(f: MlxGenerateWithSamplingFn) {
    let mut guard = MLX_GENERATE_WITH_SAMPLING_FN
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if guard.is_some() {
        tracing::warn!(
            "MLX sampling-aware generate callback re-registered \u{2014} discarding previous \
             registration. Expected usage is a single registration at app launch.",
        );
    }
    *guard = Some(f);
}

/// Read the currently registered sampling-aware callback, or `None` when
/// the shell has not registered one (the common case — the adapter then
/// falls back to the sampling-unaware [`get_mlx_generate_fn`]).
pub fn get_mlx_generate_with_sampling_fn() -> Option<MlxGenerateWithSamplingFn> {
    let guard = MLX_GENERATE_WITH_SAMPLING_FN
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *guard
}

/// Test-only helper: clear the registered sampling-aware callback so the
/// unit suite can exercise both the "registered" and "fall back to the
/// plain callback" branches of [`MlxAdapter::generate_with_sampling`].
#[cfg(any(test, feature = "test-support"))]
pub fn clear_mlx_generate_with_sampling_fn() {
    let mut guard = MLX_GENERATE_WITH_SAMPLING_FN
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *guard = None;
}

/// MLX adapter. Available on Apple Silicon (`aarch64-apple-darwin`)
/// when the device tier is `Medium` or `High`.
pub struct MlxAdapter {
    config: RouterConfig,
    available: AtomicBool,
    /// Test override — when `Some(true)` the adapter pretends to be
    /// running on Apple Silicon, when `Some(false)` it pretends not
    /// to. `None` consults the actual platform.
    platform_override: Option<bool>,
}

impl MlxAdapter {
    /// Construct a new MLX adapter from the given router config.
    pub fn new(config: RouterConfig) -> Self {
        Self {
            config,
            available: AtomicBool::new(false),
            platform_override: None,
        }
    }

    /// Construct an MLX adapter pinned to a fixed platform-detection
    /// answer. Used by tests so the unit suite can exercise both
    /// branches on any host.
    pub fn with_platform_override(config: RouterConfig, on_apple_silicon: bool) -> Self {
        Self {
            config,
            available: AtomicBool::new(false),
            platform_override: Some(on_apple_silicon),
        }
    }

    /// Whether this build is running on Apple Silicon. Honours the
    /// test override if set.
    fn on_apple_silicon(&self) -> bool {
        if let Some(forced) = self.platform_override {
            return forced;
        }
        cfg!(all(target_arch = "aarch64", target_os = "macos"))
    }
}

impl InferenceAdapter for MlxAdapter {
    fn kind(&self) -> AdapterKind {
        AdapterKind::Mlx
    }

    fn probe(&self) -> ProbeResult {
        let is_apple = self.on_apple_silicon();
        let tier_ok = matches!(
            self.config.device_tier,
            DeviceTier::Medium | DeviceTier::High
        );
        let runtime_linked = mlx_runtime_linked();
        let available = is_apple && tier_ok && runtime_linked;
        self.available.store(available, Ordering::SeqCst);
        if available {
            ProbeResult::Available
        } else {
            ProbeResult::Unavailable
        }
    }

    fn is_available(&self) -> bool {
        self.available.load(Ordering::SeqCst)
    }

    fn supports(&self, task: InferenceTask) -> bool {
        match self.config.device_tier {
            DeviceTier::Low => false,
            DeviceTier::Medium => task.is_classification(),
            DeviceTier::High => true,
        }
    }

    fn generate(&self, task_tag: &str, prompt: &str, grammar: &str) -> Result<String, RouterError> {
        if !self.is_available() {
            return Err(RouterError::Unavailable {
                task: task_tag_static(task_tag),
            });
        }
        // The Rust crate cannot link the MLX runtime on its own; the
        // production binding lives in the iOS / macOS native shell.
        // The shell calls `set_mlx_generate_fn` at init time with a
        // function pointer that wraps Swift's MLX engine — invoking
        // it from here drives the on-device inference.
        //
        // When the callback has not been registered we return
        // Unavailable (a fallback error) so the router falls through
        // to the next adapter (llama.cpp loopback or the encoder-only
        // fallback). When it *is* registered any error it returns is
        // an InferenceFailure (a hard error) — the model ran but its
        // output is not usable, and falling through to llama.cpp
        // would give a confusing "second attempt" experience.
        let Some(callback) = get_mlx_generate_fn() else {
            return Err(RouterError::Unavailable {
                task: task_tag_static(task_tag),
            });
        };
        callback(task_tag, prompt, grammar).map_err(RouterError::InferenceFailure)
    }

    fn generate_with_sampling(
        &self,
        task_tag: &str,
        prompt: &str,
        grammar: &str,
        sampling: &SamplingConfig,
    ) -> Result<String, RouterError> {
        if !self.is_available() {
            return Err(RouterError::Unavailable {
                task: task_tag_static(task_tag),
            });
        }
        // Prefer the sampling-aware native callback when the shell has
        // registered one, so the synthesis adaptive budget / retry
        // `n_predict` (and the deterministic seed + knobs) reach the MLX
        // runtime. When only the sampling-unaware callback is registered
        // we fall back to it: the verify-and-retry *decision* and the
        // fact-only retry prompt still apply on MLX — only the larger
        // token budget is a no-op, which a shell opts into by also
        // registering `set_mlx_generate_with_sampling_fn`.
        if let Some(callback) = get_mlx_generate_with_sampling_fn() {
            return callback(task_tag, prompt, grammar, sampling)
                .map_err(RouterError::InferenceFailure);
        }
        let Some(callback) = get_mlx_generate_fn() else {
            return Err(RouterError::Unavailable {
                task: task_tag_static(task_tag),
            });
        };
        callback(task_tag, prompt, grammar).map_err(RouterError::InferenceFailure)
    }
}

/// Adapter-side helper: convert a runtime task tag string back into
/// the matching `&'static str` used by [`crate::RouterError`]. Falls
/// back to a stable `"unknown"` constant for untagged calls so the
/// error type can stay `'static`-ful.
fn task_tag_static(task_tag: &str) -> &'static str {
    match task_tag {
        "tag_importance" => "tag_importance",
        "extract_entities" => "extract_entities",
        "promote_observation" => "promote_observation",
        "synth_summary" => "synth_summary",
        "synth_concept" => "synth_concept",
        "adjudicate_contradiction" => "adjudicate_contradiction",
        _ => "unknown",
    }
}

/// Returns `true` only when the MLX Swift runtime is linked into this
/// binary. The pure-Rust crate never has it; the iOS / macOS native
/// shell sets the runtime-linked flag at init time via
/// [`set_mlx_runtime_linked`]. When the flag is not set (default),
/// this returns `false` so `probe()` reports `Unavailable` and the
/// router falls through to the next adapter.
fn mlx_runtime_linked() -> bool {
    MLX_RUNTIME_LINKED.load(Ordering::Acquire)
}

/// Global flag: set to `true` by the native shell (iOS / macOS) once
/// the MLX Swift runtime is initialised and ready for inference.
static MLX_RUNTIME_LINKED: AtomicBool = AtomicBool::new(false);

/// Called by the native shell to signal that the MLX runtime is
/// available and ready. This must be called before [`MlxAdapter::probe`]
/// for the adapter to report `Available`.
pub fn set_mlx_runtime_linked(linked: bool) {
    MLX_RUNTIME_LINKED.store(linked, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    /// Tests that touch the global `MLX_RUNTIME_LINKED` flag must be
    /// serialized — Cargo runs tests in parallel by default and the
    /// flag is process-global mutable state.
    fn mlx_lock() -> MutexGuard<'static, ()> {
        static M: OnceLock<Mutex<()>> = OnceLock::new();
        M.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn probe_unavailable_off_apple_silicon() {
        let cfg = RouterConfig::default().with_device_tier(DeviceTier::High);
        let adapter = MlxAdapter::with_platform_override(cfg, false);
        assert_eq!(adapter.probe(), ProbeResult::Unavailable);
        assert!(!adapter.is_available());
    }

    #[test]
    fn probe_unavailable_on_apple_silicon_without_runtime() {
        let _g = mlx_lock();
        set_mlx_runtime_linked(false);
        let cfg = RouterConfig::default().with_device_tier(DeviceTier::High);
        let adapter = MlxAdapter::with_platform_override(cfg, true);
        assert_eq!(adapter.probe(), ProbeResult::Unavailable);
        assert!(!adapter.is_available());
    }

    #[test]
    fn probe_available_on_apple_silicon_high_tier_with_runtime() {
        let _g = mlx_lock();
        set_mlx_runtime_linked(true);
        let cfg = RouterConfig::default().with_device_tier(DeviceTier::High);
        let adapter = MlxAdapter::with_platform_override(cfg, true);
        assert_eq!(adapter.probe(), ProbeResult::Available);
        assert!(adapter.is_available());
        set_mlx_runtime_linked(false);
    }

    #[test]
    fn low_tier_disables_adapter_even_on_apple_silicon() {
        let cfg = RouterConfig::default().with_device_tier(DeviceTier::Low);
        let adapter = MlxAdapter::with_platform_override(cfg, true);
        assert_eq!(adapter.probe(), ProbeResult::Unavailable);
    }

    #[test]
    fn medium_tier_supports_only_classification() {
        let cfg = RouterConfig::default().with_device_tier(DeviceTier::Medium);
        let adapter = MlxAdapter::with_platform_override(cfg, true);
        assert!(adapter.supports(InferenceTask::TagImportance));
        assert!(!adapter.supports(InferenceTask::SynthSummary));
    }

    #[test]
    fn high_tier_supports_synthesis() {
        let cfg = RouterConfig::default().with_device_tier(DeviceTier::High);
        let adapter = MlxAdapter::with_platform_override(cfg, true);
        assert!(adapter.supports(InferenceTask::SynthSummary));
    }

    #[test]
    fn generate_when_unavailable_returns_unavailable() {
        let cfg = RouterConfig::default().with_device_tier(DeviceTier::Low);
        let adapter = MlxAdapter::with_platform_override(cfg, false);
        adapter.probe();
        let err = adapter.generate("tag_importance", "", "").unwrap_err();
        assert!(matches!(err, RouterError::Unavailable { .. }));
    }

    #[test]
    fn generate_returns_fallback_error_when_runtime_not_linked() {
        let _g = mlx_lock();
        set_mlx_runtime_linked(true);
        let cfg = RouterConfig::default().with_device_tier(DeviceTier::High);
        let adapter = MlxAdapter::with_platform_override(cfg, true);
        adapter.probe();
        assert!(adapter.is_available());
        // Even when probe says available, generate returns Unavailable
        // (a fallback error) because the MLX runtime isn't truly linked
        // — the Rust crate always hits this path.
        set_mlx_runtime_linked(false);
        // Force re-probe so is_available correctly reflects the state
        adapter.probe();
        let err = adapter.generate("synth_summary", "", "").unwrap_err();
        assert!(
            err.is_fallback(),
            "MLX generate() must return a fallback error"
        );
    }

    /// Bridge fixture used by [`generate_dispatches_through_registered_callback`].
    /// Plain `fn` (not a closure) so it satisfies [`MlxGenerateFn`].
    /// The signature is dictated by [`MlxGenerateFn`] — clippy's
    /// "unnecessary Result wrap" lint must be suppressed here because
    /// the trait alias type requires `Result<String, String>`.
    #[allow(clippy::unnecessary_wraps)]
    fn test_callback(task_tag: &str, prompt: &str, grammar: &str) -> Result<String, String> {
        // Encode the inputs in the output so the assertion below can
        // verify they round-tripped verbatim. Real Swift shells emit
        // grammar-constrained JSON; the test substitute just echoes.
        Ok(format!(
            "{{\"task\":\"{task_tag}\",\"prompt_len\":{p},\"grammar_len\":{g}}}",
            p = prompt.len(),
            g = grammar.len(),
        ))
    }

    #[test]
    fn generate_dispatches_through_registered_callback() {
        // The MLX generate slot is a `OnceLock` — once set in this
        // process it stays set for every subsequent test. Hold the
        // mlx_lock to serialise with the runtime-linked-flag tests,
        // register the callback (idempotent), probe with the runtime
        // marked linked, and verify the callback round-trips its
        // arguments to the adapter return value.
        let _g = mlx_lock();
        set_mlx_generate_fn(test_callback);
        set_mlx_runtime_linked(true);
        let cfg = RouterConfig::default().with_device_tier(DeviceTier::High);
        let adapter = MlxAdapter::with_platform_override(cfg, true);
        adapter.probe();
        assert!(adapter.is_available());
        let out = adapter
            .generate("synth_summary", "hello world", "root::=\".+\"")
            .expect("registered callback should succeed");
        assert!(out.contains("\"task\":\"synth_summary\""), "got {out}");
        assert!(out.contains("\"prompt_len\":11"), "got {out}");
        // Restore the global flag so unrelated tests that follow do
        // not observe runtime-linked=true.
        set_mlx_runtime_linked(false);
    }

    /// Bridge fixture used by [`generate_propagates_callback_error_as_inference_failure`].
    fn failing_callback(_task_tag: &str, _prompt: &str, _grammar: &str) -> Result<String, String> {
        Err("model produced ill-formed JSON".into())
    }

    #[test]
    fn generate_propagates_callback_error_as_inference_failure() {
        // Swap the success callback for one that always fails, run
        // generate, and verify the error is propagated as
        // `RouterError::InferenceFailure` (a non-fallback error —
        // the router must NOT retry on the next adapter when the
        // model itself ran but produced invalid output).
        let _g = mlx_lock();
        set_mlx_generate_fn(failing_callback);
        set_mlx_runtime_linked(true);
        let cfg = RouterConfig::default().with_device_tier(DeviceTier::High);
        let adapter = MlxAdapter::with_platform_override(cfg, true);
        adapter.probe();
        let err = adapter.generate("synth_summary", "hi", "").unwrap_err();
        assert!(
            matches!(err, RouterError::InferenceFailure(_)),
            "expected InferenceFailure, got {err:?}"
        );
        assert!(
            !err.is_fallback(),
            "InferenceFailure must not be a fallback error"
        );
        // Restore the test_callback so neighbouring tests that
        // depend on the success path keep observing it.
        set_mlx_generate_fn(test_callback);
        set_mlx_runtime_linked(false);
    }

    #[test]
    fn generate_unavailable_when_callback_not_registered() {
        let _g = mlx_lock();
        clear_mlx_generate_fn();
        set_mlx_runtime_linked(true);
        let cfg = RouterConfig::default().with_device_tier(DeviceTier::High);
        let adapter = MlxAdapter::with_platform_override(cfg, true);
        adapter.probe();
        let err = adapter.generate("synth_summary", "", "").unwrap_err();
        assert!(
            err.is_fallback(),
            "missing callback must surface as a fallback error",
        );
        set_mlx_runtime_linked(false);
    }

    /// Sampling-aware bridge fixture: echoes the per-call `n_predict` and
    /// `seed` so the assertion can prove the [`SamplingConfig`] reached
    /// the native callback verbatim. Plain `fn` to satisfy
    /// [`MlxGenerateWithSamplingFn`].
    #[allow(clippy::unnecessary_wraps, clippy::trivially_copy_pass_by_ref)]
    fn sampling_test_callback(
        task_tag: &str,
        _prompt: &str,
        _grammar: &str,
        sampling: &SamplingConfig,
    ) -> Result<String, String> {
        Ok(format!(
            "{{\"task\":\"{task_tag}\",\"n_predict\":{n},\"seed\":{s}}}",
            n = sampling.n_predict,
            s = sampling.seed,
        ))
    }

    #[test]
    fn generate_with_sampling_routes_through_sampling_aware_callback() {
        // When a sampling-aware callback is registered, the per-call
        // budget (the adaptive / retry `n_predict`) must reach the
        // native runtime verbatim instead of being silently dropped.
        let _g = mlx_lock();
        set_mlx_generate_with_sampling_fn(sampling_test_callback);
        set_mlx_runtime_linked(true);
        let cfg = RouterConfig::default().with_device_tier(DeviceTier::High);
        let adapter = MlxAdapter::with_platform_override(cfg, true);
        adapter.probe();
        assert!(adapter.is_available());
        let sampling = SamplingConfig::default().with_n_predict(1536);
        let out = adapter
            .generate_with_sampling("synth_summary", "hello", "", &sampling)
            .expect("sampling-aware callback should succeed");
        assert!(out.contains("\"n_predict\":1536"), "got {out}");
        clear_mlx_generate_with_sampling_fn();
        set_mlx_runtime_linked(false);
    }

    #[test]
    fn generate_with_sampling_falls_back_to_plain_callback() {
        // With no sampling-aware callback registered, the override path
        // must still serve the request via the sampling-unaware callback
        // (the retry decision + fact-only prompt already applied upstream;
        // only the budget bump is forgone) rather than erroring.
        let _g = mlx_lock();
        clear_mlx_generate_with_sampling_fn();
        set_mlx_generate_fn(test_callback);
        set_mlx_runtime_linked(true);
        let cfg = RouterConfig::default().with_device_tier(DeviceTier::High);
        let adapter = MlxAdapter::with_platform_override(cfg, true);
        adapter.probe();
        let sampling = SamplingConfig::default().with_n_predict(1536);
        let out = adapter
            .generate_with_sampling("synth_summary", "hello world", "", &sampling)
            .expect("fallback to plain callback should succeed");
        // The plain callback echoes prompt_len, not the sampling fields.
        assert!(out.contains("\"prompt_len\":11"), "got {out}");
        assert!(
            !out.contains("n_predict"),
            "plain callback must not see sampling: {out}"
        );
        set_mlx_runtime_linked(false);
    }
}
