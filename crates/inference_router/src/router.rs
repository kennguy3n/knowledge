//! [`InferenceRouter`] — orchestrates adapter probing, dispatch, and
//! warm-up / idle-unload.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::adapter::{AdapterKind, InferenceAdapter, ProbeResult};
use crate::config::RouterConfig;
use crate::error::RouterError;
use crate::task::InferenceTask;

/// Internal record of the most recent dispatch — drives the
/// idle-unload sweep.
#[derive(Debug, Clone, Copy)]
struct AdapterActivity {
    kind: AdapterKind,
    last_dispatch: Instant,
    loaded: bool,
}

/// The on-device inference router.
///
/// Holds an ordered list of [`InferenceAdapter`]s — typically `MLX →
/// llama.cpp → Fallback`. Probes them at boot, dispatches each
/// [`InferenceTask`] to the highest-priority adapter that is available
/// and supports the task, and unloads adapters whose last dispatch is
/// older than [`RouterConfig::idle_timeout_secs`].
pub struct InferenceRouter {
    config: RouterConfig,
    adapters: Vec<Box<dyn InferenceAdapter>>,
    bootstrapped: AtomicBool,
    activity: Mutex<Vec<AdapterActivity>>,
    warmed: AtomicBool,
}

impl InferenceRouter {
    /// Construct a router from a config and an ordered list of
    /// adapters. Order is priority order — index 0 is highest
    /// priority. Adapters must be probed via [`Self::bootstrap`]
    /// before [`Self::dispatch`] is called.
    pub fn new(config: RouterConfig, adapters: Vec<Box<dyn InferenceAdapter>>) -> Self {
        let activity = adapters
            .iter()
            .map(|a| AdapterActivity {
                kind: a.kind(),
                last_dispatch: Instant::now(),
                loaded: false,
            })
            .collect();
        Self {
            config,
            adapters,
            bootstrapped: AtomicBool::new(false),
            activity: Mutex::new(activity),
            warmed: AtomicBool::new(false),
        }
    }

    /// Run [`InferenceAdapter::probe`] on every adapter. Must be
    /// called before [`Self::dispatch`].
    pub fn bootstrap(&self) -> Vec<(AdapterKind, ProbeResult)> {
        let results = self
            .adapters
            .iter()
            .map(|a| (a.kind(), a.probe()))
            .collect();
        self.bootstrapped.store(true, Ordering::SeqCst);
        results
    }

    /// Borrow the underlying config.
    pub fn config(&self) -> &RouterConfig {
        &self.config
    }

    /// `true` once [`Self::bootstrap`] has run.
    pub fn is_bootstrapped(&self) -> bool {
        self.bootstrapped.load(Ordering::SeqCst)
    }

    /// `true` after [`Self::warm_up`] succeeds for at least one
    /// adapter.
    pub fn is_warmed(&self) -> bool {
        self.warmed.load(Ordering::SeqCst)
    }

    /// Send a no-op completion to the highest-priority available
    /// adapter so it pages model weights in. Returns the adapter that
    /// served the warm-up, or `None` when no adapter is available.
    ///
    /// The warm-up text comes from [`RouterConfig::warm_up_prompt`].
    pub fn warm_up(&self) -> Option<AdapterKind> {
        for (idx, adapter) in self.adapters.iter().enumerate() {
            if !adapter.is_available() {
                continue;
            }
            let res = adapter.generate("tag_importance", self.config.warm_up_prompt.as_str(), "");
            if res.is_ok() {
                self.mark_active(idx, true);
                self.warmed.store(true, Ordering::SeqCst);
                return Some(adapter.kind());
            }
        }
        None
    }

    /// Dispatch `task` to the first adapter that is available and
    /// supports it.
    pub fn dispatch(&self, task: InferenceTask, prompt: &str) -> Result<String, RouterError> {
        if !self.is_bootstrapped() {
            return Err(RouterError::NotProbed { adapter: "router" });
        }
        for (idx, adapter) in self.adapters.iter().enumerate() {
            if !adapter.is_available() || !adapter.supports(task) {
                continue;
            }
            let result = adapter.generate(task.tag(), prompt, task.grammar());
            self.mark_active(idx, result.is_ok());
            match result {
                Ok(out) => return Ok(out),
                Err(err) if err.is_fallback() => continue,
                Err(err) => return Err(err),
            }
        }
        Err(RouterError::Unavailable { task: task.tag() })
    }

    /// Walk the adapters and "unload" any whose last dispatch is
    /// older than `idle_timeout_secs`. Returns the list of adapters
    /// that were unloaded. The substrate's production runtime calls
    /// this on a wall-clock cadence.
    pub fn sweep_idle_adapters(&self) -> Vec<AdapterKind> {
        self.sweep_idle_adapters_at(Instant::now())
    }

    /// Lower-level variant that takes the wall-clock instant
    /// explicitly so unit tests can simulate elapsed time without
    /// sleeping.
    pub fn sweep_idle_adapters_at(&self, now: Instant) -> Vec<AdapterKind> {
        let mut activity = self.activity.lock().expect("activity lock");
        let timeout = Duration::from_secs(self.config.idle_timeout_secs);
        let mut unloaded = Vec::new();
        for entry in activity.iter_mut() {
            if entry.loaded && now.duration_since(entry.last_dispatch) >= timeout {
                entry.loaded = false;
                unloaded.push(entry.kind);
            }
        }
        unloaded
    }

    /// `true` iff the adapter is currently loaded into memory (i.e.
    /// has been dispatched to and not yet swept idle).
    pub fn is_adapter_loaded(&self, kind: AdapterKind) -> bool {
        let activity = self.activity.lock().expect("activity lock");
        activity
            .iter()
            .any(|entry| entry.kind == kind && entry.loaded)
    }

    fn mark_active(&self, idx: usize, loaded: bool) {
        let mut activity = self.activity.lock().expect("activity lock");
        if let Some(entry) = activity.get_mut(idx) {
            entry.last_dispatch = Instant::now();
            entry.loaded = loaded || entry.loaded;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::{AdapterKind, InferenceAdapter, ProbeResult};
    use crate::adapters::{FallbackAdapter, LlamaCppAdapter};
    use crate::config::DeviceTier;
    use std::sync::atomic::AtomicUsize;

    /// Recording mock adapter that captures every dispatch.
    struct MockAdapter {
        kind: AdapterKind,
        available: AtomicBool,
        supported: Vec<InferenceTask>,
        response: Mutex<Result<String, RouterError>>,
        calls: AtomicUsize,
    }

    impl MockAdapter {
        fn new(
            kind: AdapterKind,
            available: bool,
            supported: Vec<InferenceTask>,
            response: Result<String, RouterError>,
        ) -> Self {
            Self {
                kind,
                available: AtomicBool::new(available),
                supported,
                response: Mutex::new(response),
                calls: AtomicUsize::new(0),
            }
        }
    }

    impl InferenceAdapter for MockAdapter {
        fn kind(&self) -> AdapterKind {
            self.kind
        }

        fn probe(&self) -> ProbeResult {
            if self.available.load(Ordering::SeqCst) {
                ProbeResult::Available
            } else {
                ProbeResult::Unavailable
            }
        }

        fn is_available(&self) -> bool {
            self.available.load(Ordering::SeqCst)
        }

        fn supports(&self, task: InferenceTask) -> bool {
            self.supported.contains(&task)
        }

        fn generate(
            &self,
            _task_tag: &str,
            _prompt: &str,
            _grammar: &str,
        ) -> Result<String, RouterError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.response
                .lock()
                .expect("response")
                .clone()
                .map_err(|e| match e {
                    RouterError::Unavailable { task } => RouterError::Unavailable { task },
                    RouterError::TierTooLow { tier, task } => {
                        RouterError::TierTooLow { tier, task }
                    }
                    RouterError::InferenceFailure(msg) => RouterError::InferenceFailure(msg),
                    RouterError::NotProbed { adapter } => RouterError::NotProbed { adapter },
                })
        }
    }

    fn router_with(adapters: Vec<Box<dyn InferenceAdapter>>) -> InferenceRouter {
        let config = RouterConfig::default().with_device_tier(DeviceTier::High);
        InferenceRouter::new(config, adapters)
    }

    #[test]
    fn dispatch_before_bootstrap_errors() {
        let router = router_with(vec![Box::new(FallbackAdapter::new())]);
        let err = router
            .dispatch(InferenceTask::TagImportance, "x")
            .unwrap_err();
        assert!(matches!(err, RouterError::NotProbed { .. }));
    }

    #[test]
    fn priority_routes_to_first_available_adapter_supporting_task() {
        let primary = MockAdapter::new(
            AdapterKind::Mlx,
            true,
            vec![InferenceTask::TagImportance],
            Ok("primary-response".into()),
        );
        let secondary = MockAdapter::new(
            AdapterKind::LlamaCpp,
            true,
            vec![InferenceTask::TagImportance],
            Ok("secondary-response".into()),
        );
        let router = router_with(vec![Box::new(primary), Box::new(secondary)]);
        router.bootstrap();
        let out = router
            .dispatch(InferenceTask::TagImportance, "x")
            .expect("dispatch ok");
        assert_eq!(out, "primary-response");
    }

    #[test]
    fn router_falls_through_when_primary_unsupported() {
        let primary = MockAdapter::new(
            AdapterKind::Mlx,
            true,
            vec![InferenceTask::TagImportance],
            Ok("never".into()),
        );
        let secondary = MockAdapter::new(
            AdapterKind::LlamaCpp,
            true,
            vec![InferenceTask::SynthSummary],
            Ok("synth-response".into()),
        );
        let router = router_with(vec![Box::new(primary), Box::new(secondary)]);
        router.bootstrap();
        let out = router
            .dispatch(InferenceTask::SynthSummary, "x")
            .expect("dispatch ok");
        assert_eq!(out, "synth-response");
    }

    #[test]
    fn router_falls_through_when_primary_unavailable() {
        let primary = MockAdapter::new(
            AdapterKind::Mlx,
            false,
            vec![InferenceTask::TagImportance],
            Ok("never".into()),
        );
        let secondary = MockAdapter::new(
            AdapterKind::LlamaCpp,
            true,
            vec![InferenceTask::TagImportance],
            Ok("secondary-response".into()),
        );
        let router = router_with(vec![Box::new(primary), Box::new(secondary)]);
        router.bootstrap();
        let out = router
            .dispatch(InferenceTask::TagImportance, "x")
            .expect("dispatch ok");
        assert_eq!(out, "secondary-response");
    }

    #[test]
    fn router_falls_through_on_unavailable_error() {
        let primary = MockAdapter::new(
            AdapterKind::Mlx,
            true,
            vec![InferenceTask::TagImportance],
            Err(RouterError::Unavailable {
                task: "tag_importance",
            }),
        );
        let secondary = MockAdapter::new(
            AdapterKind::LlamaCpp,
            true,
            vec![InferenceTask::TagImportance],
            Ok("secondary".into()),
        );
        let router = router_with(vec![Box::new(primary), Box::new(secondary)]);
        router.bootstrap();
        let out = router
            .dispatch(InferenceTask::TagImportance, "x")
            .expect("dispatch ok");
        assert_eq!(out, "secondary");
    }

    #[test]
    fn router_does_not_fall_through_on_inference_failure() {
        let primary = MockAdapter::new(
            AdapterKind::Mlx,
            true,
            vec![InferenceTask::TagImportance],
            Err(RouterError::InferenceFailure("boom".into())),
        );
        let secondary = MockAdapter::new(
            AdapterKind::LlamaCpp,
            true,
            vec![InferenceTask::TagImportance],
            Ok("secondary".into()),
        );
        let router = router_with(vec![Box::new(primary), Box::new(secondary)]);
        router.bootstrap();
        let err = router
            .dispatch(InferenceTask::TagImportance, "x")
            .unwrap_err();
        assert!(matches!(err, RouterError::InferenceFailure(_)));
    }

    #[test]
    fn router_emits_unavailable_when_no_adapter_serves_task() {
        let only = MockAdapter::new(
            AdapterKind::Fallback,
            true,
            vec![InferenceTask::TagImportance],
            Ok("never".into()),
        );
        let router = router_with(vec![Box::new(only)]);
        router.bootstrap();
        let err = router
            .dispatch(InferenceTask::SynthSummary, "x")
            .unwrap_err();
        assert!(matches!(err, RouterError::Unavailable { .. }));
    }

    #[test]
    fn warm_up_marks_router_warmed_and_loads_adapter() {
        let adapter = MockAdapter::new(
            AdapterKind::LlamaCpp,
            true,
            vec![InferenceTask::TagImportance],
            Ok("warmup".into()),
        );
        let router = router_with(vec![Box::new(adapter)]);
        router.bootstrap();
        let kind = router.warm_up().expect("warm-up ok");
        assert_eq!(kind, AdapterKind::LlamaCpp);
        assert!(router.is_warmed());
        assert!(router.is_adapter_loaded(AdapterKind::LlamaCpp));
    }

    #[test]
    fn idle_sweep_unloads_adapter_after_timeout() {
        let cfg = RouterConfig::default()
            .with_device_tier(DeviceTier::High)
            .with_idle_timeout(60);
        let adapter = MockAdapter::new(
            AdapterKind::LlamaCpp,
            true,
            vec![InferenceTask::TagImportance],
            Ok("ok".into()),
        );
        let router = InferenceRouter::new(cfg, vec![Box::new(adapter)]);
        router.bootstrap();
        router.warm_up();
        assert!(router.is_adapter_loaded(AdapterKind::LlamaCpp));
        // Simulate 120s passing.
        let later = Instant::now() + Duration::from_secs(120);
        let unloaded = router.sweep_idle_adapters_at(later);
        assert!(unloaded.contains(&AdapterKind::LlamaCpp));
        assert!(!router.is_adapter_loaded(AdapterKind::LlamaCpp));
    }

    #[test]
    fn integration_mlx_llama_fallback_priority() {
        // Boot a router with the production three-adapter ladder
        // using mocks for MLX/LlamaCpp and the real FallbackAdapter.
        // MLX disabled (off Apple Silicon), llama.cpp reachable.
        use crate::adapters::llama_cpp::MockLlamaServerClient;
        use crate::adapters::mlx::MlxAdapter;
        let cfg = RouterConfig::default().with_device_tier(DeviceTier::High);
        let mlx = MlxAdapter::with_platform_override(cfg.clone(), false);
        let llama = LlamaCppAdapter::new(
            cfg.clone(),
            Box::new(MockLlamaServerClient::ok(
                r#"{"class":"useful","confidence":0.4}"#,
            )),
        );
        let fallback = FallbackAdapter::new();
        let router = InferenceRouter::new(
            cfg,
            vec![Box::new(mlx), Box::new(llama), Box::new(fallback)],
        );
        router.bootstrap();
        // TagImportance — llama.cpp should serve it.
        let out = router
            .dispatch(InferenceTask::TagImportance, "msg")
            .unwrap();
        assert!(out.contains("useful"));
        // SynthSummary — only llama.cpp supports synthesis (high tier);
        // returns the mock response.
        let out = router
            .dispatch(InferenceTask::SynthSummary, "session")
            .unwrap();
        assert!(out.contains("useful"));
    }

    #[test]
    fn integration_falls_through_to_fallback_when_llama_unreachable() {
        use crate::adapters::llama_cpp::MockLlamaServerClient;
        use crate::adapters::mlx::MlxAdapter;
        let cfg = RouterConfig::default().with_device_tier(DeviceTier::High);
        let mlx = MlxAdapter::with_platform_override(cfg.clone(), false);
        let llama =
            LlamaCppAdapter::new(cfg.clone(), Box::new(MockLlamaServerClient::unreachable()));
        let fallback = FallbackAdapter::new();
        let router = InferenceRouter::new(
            cfg,
            vec![Box::new(mlx), Box::new(llama), Box::new(fallback)],
        );
        router.bootstrap();
        let out = router
            .dispatch(InferenceTask::TagImportance, "msg")
            .unwrap();
        assert!(out.contains("useful"));
        // Synthesis routes only to llama.cpp/MLX; with both
        // unavailable the router emits Unavailable.
        let err = router
            .dispatch(InferenceTask::SynthSummary, "x")
            .unwrap_err();
        assert!(matches!(err, RouterError::Unavailable { .. }));
    }

    /// Apple-Silicon MLX has higher priority than llama.cpp at the
    /// router level. With both available and supporting the task,
    /// the router must dispatch to MLX *first*.
    #[test]
    fn integration_mlx_wins_when_both_mlx_and_llama_available_high_tier() {
        use crate::adapters::llama_cpp::MockLlamaServerClient;
        let cfg = RouterConfig::default().with_device_tier(DeviceTier::High);
        // Mocked adapters in priority order.
        let mlx = MockAdapter::new(
            AdapterKind::Mlx,
            true,
            vec![InferenceTask::TagImportance, InferenceTask::SynthSummary],
            Ok("from-mlx".into()),
        );
        let llama_inner = MockLlamaServerClient::ok(r#"{"class":"useful"}"#);
        let llama = LlamaCppAdapter::new(cfg.clone(), Box::new(llama_inner));
        let fallback = FallbackAdapter::new();
        let router = InferenceRouter::new(
            cfg,
            vec![Box::new(mlx), Box::new(llama), Box::new(fallback)],
        );
        router.bootstrap();
        let out = router
            .dispatch(InferenceTask::TagImportance, "msg")
            .unwrap();
        assert_eq!(out, "from-mlx");
    }

    /// Low tier — both real adapters refuse to load and refuse to
    /// support tasks. Classification still resolves via the
    /// `FallbackAdapter`; synthesis returns `Unavailable`.
    #[test]
    fn low_tier_blocks_real_slm_adapters_but_fallback_serves_classification() {
        use crate::adapters::llama_cpp::MockLlamaServerClient;
        use crate::adapters::mlx::MlxAdapter;
        let cfg = RouterConfig::default().with_device_tier(DeviceTier::Low);
        let mlx = MlxAdapter::with_platform_override(cfg.clone(), true);
        let llama = LlamaCppAdapter::new(cfg.clone(), Box::new(MockLlamaServerClient::ok("y")));
        let fallback = FallbackAdapter::new();
        let router = InferenceRouter::new(
            cfg,
            vec![Box::new(mlx), Box::new(llama), Box::new(fallback)],
        );
        router.bootstrap();
        // Classification must work via FallbackAdapter (lexicon).
        let out = router
            .dispatch(InferenceTask::TagImportance, "deadline tomorrow")
            .unwrap();
        assert!(
            !out.is_empty(),
            "fallback adapter should serve classification"
        );
        // Synthesis must be Unavailable on a Low tier device — no
        // adapter supports it.
        let err = router
            .dispatch(InferenceTask::SynthSummary, "session body")
            .unwrap_err();
        assert!(matches!(err, RouterError::Unavailable { .. }));
    }

    /// Medium tier allows classification on the real SLM adapters
    /// but gates synthesis. With both MLX (off — non-Apple test
    /// host) and llama.cpp (Medium) reachable, classification routes
    /// to llama.cpp and synthesis returns `Unavailable`.
    #[test]
    fn medium_tier_allows_classification_but_gates_synthesis() {
        use crate::adapters::llama_cpp::MockLlamaServerClient;
        use crate::adapters::mlx::MlxAdapter;
        let cfg = RouterConfig::default().with_device_tier(DeviceTier::Medium);
        let mlx = MlxAdapter::with_platform_override(cfg.clone(), false);
        let llama = LlamaCppAdapter::new(
            cfg.clone(),
            Box::new(MockLlamaServerClient::ok(
                r#"{"class":"useful","confidence":0.6}"#,
            )),
        );
        let fallback = FallbackAdapter::new();
        let router = InferenceRouter::new(
            cfg,
            vec![Box::new(mlx), Box::new(llama), Box::new(fallback)],
        );
        router.bootstrap();
        // Classification supported on Medium → llama.cpp serves it.
        let out = router
            .dispatch(InferenceTask::TagImportance, "anything")
            .unwrap();
        assert!(out.contains("useful"));
        // Synthesis is gated off the SLM at Medium and the
        // `FallbackAdapter` does not support it either.
        let err = router
            .dispatch(InferenceTask::SynthSummary, "session body")
            .unwrap_err();
        assert!(matches!(err, RouterError::Unavailable { .. }));
    }

    /// `warm_up` is a no-op (`None`) when no adapter is available
    /// (e.g. probe failed everywhere). The router must not panic
    /// or set the warmed flag.
    #[test]
    fn warm_up_returns_none_when_no_adapter_is_available() {
        let adapter = MockAdapter::new(
            AdapterKind::LlamaCpp,
            false,
            vec![InferenceTask::TagImportance],
            Ok("never".into()),
        );
        let router = router_with(vec![Box::new(adapter)]);
        router.bootstrap();
        assert!(router.warm_up().is_none());
        assert!(!router.is_warmed());
        assert!(!router.is_adapter_loaded(AdapterKind::LlamaCpp));
    }

    /// Idle sweep is a no-op for adapters that haven't been
    /// dispatched / warmed up — they were never loaded.
    #[test]
    fn idle_sweep_does_not_unload_never_loaded_adapter() {
        let adapter = MockAdapter::new(
            AdapterKind::LlamaCpp,
            true,
            vec![InferenceTask::TagImportance],
            Ok("ok".into()),
        );
        let router = router_with(vec![Box::new(adapter)]);
        router.bootstrap();
        // No dispatch / warm-up — adapter was never loaded.
        let later = Instant::now() + Duration::from_secs(3600);
        let unloaded = router.sweep_idle_adapters_at(later);
        assert!(unloaded.is_empty());
    }

    /// Dispatch refreshes the activity clock on the serving
    /// adapter, so an idle sweep that lands within the timeout
    /// must not unload it.
    #[test]
    fn dispatch_refreshes_activity_clock_so_inside_timeout_is_not_unloaded() {
        let cfg = RouterConfig::default()
            .with_device_tier(DeviceTier::High)
            .with_idle_timeout(60);
        let adapter = MockAdapter::new(
            AdapterKind::LlamaCpp,
            true,
            vec![InferenceTask::TagImportance],
            Ok("ok".into()),
        );
        let router = InferenceRouter::new(cfg, vec![Box::new(adapter)]);
        router.bootstrap();
        router.dispatch(InferenceTask::TagImportance, "x").unwrap();
        // 30 seconds < 60 second idle timeout — must NOT be unloaded.
        let inside = Instant::now() + Duration::from_secs(30);
        let unloaded = router.sweep_idle_adapters_at(inside);
        assert!(unloaded.is_empty());
        assert!(router.is_adapter_loaded(AdapterKind::LlamaCpp));
    }
}
