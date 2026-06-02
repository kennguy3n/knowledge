//! Integration tests for `inference_router`.
//!
//! These tests pin the **observable contract** of the router as a
//! substrate-wide service:
//!
//! 1. **Adapter priority** — `MLX → LlamaCpp → Fallback` ordering.
//! 2. **DeviceTier gating** — a `Low` tier device must not use SLM.
//! 3. **Warm-up & idle-unload** lifecycle.
//! 4. **Task routing** — every [`InferenceTask`] variant lands at an
//!    adapter that supports it (or the router emits a structured
//!    error).
//! 5. **`FallbackAdapter` semantics** — succeeds on classification,
//!    errors on synthesis.
//! 6. **`RouterError` variants** — every variant is constructible via
//!    a real call path.
//!
//! The tests use the in-tree `MockLlamaServerClient` plus
//! `MlxAdapter::with_platform_override` so they exercise the
//! production three-adapter ladder without needing a real SLM.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use inference_router::adapters::llama_cpp::MockLlamaServerClient;
use inference_router::adapters::mlx::MlxAdapter;
use inference_router::{
    AdapterKind, DeviceTier, FallbackAdapter, InferenceAdapter, InferenceRouter, InferenceTask,
    LlamaCppAdapter, ProbeResult, RouterConfig, RouterError, IDLE_UNLOAD_TIMEOUT_SECS,
    WARM_UP_PROMPT,
};

/// Mock adapter used when we need precise control of probe / supports
/// / generate behaviour. The in-tree mock in `router.rs` is private,
/// so we re-implement a tiny one here for the integration suite.
struct ScriptedAdapter {
    kind: AdapterKind,
    available: AtomicBool,
    supported: Vec<InferenceTask>,
    response: Mutex<Result<String, RouterError>>,
}

impl ScriptedAdapter {
    fn new(kind: AdapterKind,
        available: bool,
        supported: Vec<InferenceTask>,
        response: Result<String, RouterError>,
    ) -> Self {
        Self {
            kind,
            available: AtomicBool::new(available),
            supported,
            response: Mutex::new(response),
        }
    }
}

impl InferenceAdapter for ScriptedAdapter {
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
    fn generate(&self,
        _task_tag: &str,
        _prompt: &str,
        _grammar: &str,
    ) -> Result<String, RouterError> {
        self.response.lock().expect("response").clone()
    }
}

fn high_tier_config() -> RouterConfig {
    RouterConfig::default().with_device_tier(DeviceTier::High)
}

// ───────────────────────── Adapter priority ─────────────────────────

#[test]
fn mlx_outranks_llama_cpp_outranks_fallback() {
    // All three adapters available and supporting TagImportance —
    // verify the router uses the first one in priority order.
    let mlx = ScriptedAdapter::new(AdapterKind::Mlx,
        true,
        vec![InferenceTask::TagImportance],
        Ok("mlx-served".into()),
    );
    let llama = ScriptedAdapter::new(AdapterKind::LlamaCpp,
        true,
        vec![InferenceTask::TagImportance],
        Ok("llama-served".into()),
    );
    let fallback = ScriptedAdapter::new(AdapterKind::Fallback,
        true,
        vec![InferenceTask::TagImportance],
        Ok("fallback-served".into()),
    );
    let router = InferenceRouter::new(high_tier_config(),
        vec![Box::new(mlx), Box::new(llama), Box::new(fallback)],
    );
    router.bootstrap();
    let out = router
        .dispatch(InferenceTask::TagImportance, "msg")
        .expect("dispatch ok");
    assert_eq!(out, "mlx-served");
}

#[test]
fn router_uses_llama_cpp_when_mlx_unavailable() {
    let mlx = ScriptedAdapter::new(AdapterKind::Mlx,
        false,
        vec![InferenceTask::TagImportance],
        Ok("never".into()),
    );
    let llama = ScriptedAdapter::new(AdapterKind::LlamaCpp,
        true,
        vec![InferenceTask::TagImportance],
        Ok("llama-served".into()),
    );
    let fallback = ScriptedAdapter::new(AdapterKind::Fallback,
        true,
        vec![InferenceTask::TagImportance],
        Ok("fallback-served".into()),
    );
    let router = InferenceRouter::new(high_tier_config(),
        vec![Box::new(mlx), Box::new(llama), Box::new(fallback)],
    );
    router.bootstrap();
    let out = router
        .dispatch(InferenceTask::TagImportance, "msg")
        .expect("dispatch ok");
    assert_eq!(out, "llama-served");
}

#[test]
fn router_uses_fallback_when_mlx_and_llama_unavailable() {
    let mlx = ScriptedAdapter::new(AdapterKind::Mlx,
        false,
        vec![InferenceTask::TagImportance],
        Ok("never".into()),
    );
    let llama = ScriptedAdapter::new(AdapterKind::LlamaCpp,
        false,
        vec![InferenceTask::TagImportance],
        Ok("never".into()),
    );
    let fallback = ScriptedAdapter::new(AdapterKind::Fallback,
        true,
        vec![InferenceTask::TagImportance],
        Ok("fallback-served".into()),
    );
    let router = InferenceRouter::new(high_tier_config(),
        vec![Box::new(mlx), Box::new(llama), Box::new(fallback)],
    );
    router.bootstrap();
    let out = router
        .dispatch(InferenceTask::TagImportance, "msg")
        .expect("dispatch ok");
    assert_eq!(out, "fallback-served");
}

// ──────────────────────── DeviceTier gating ─────────────────────────

#[test]
fn low_tier_blocks_slm_adapters() {
    // On a Low-tier device, MlxAdapter::probe and
    // LlamaCppAdapter::probe must both report Unavailable, even with
    // a reachable mock server. Only the FallbackAdapter is left.
    let cfg = RouterConfig::default().with_device_tier(DeviceTier::Low);
    let mlx = MlxAdapter::with_platform_override(cfg.clone(), true);
    let llama = LlamaCppAdapter::new(cfg.clone(), Box::new(MockLlamaServerClient::ok("never")));
    let fallback = FallbackAdapter::new();

    assert_eq!(mlx.probe(), ProbeResult::Unavailable);
    assert_eq!(llama.probe(), ProbeResult::Unavailable);
    assert_eq!(fallback.probe(), ProbeResult::Available);

    // The whole router resolves classification through the fallback.
    let router = InferenceRouter::new(cfg,
        vec![Box::new(mlx), Box::new(llama), Box::new(fallback)],
    );
    router.bootstrap();
    // The fallback adapter scores against a real lexicon; feed it a
    // body containing a 'useful'-class token so the classifier picks
    // the expected class deterministically.
    let prompt = "prefix\n\nMessage:\nplease investigate the question";
    let out = router
        .dispatch(InferenceTask::TagImportance, prompt)
        .expect("classification routes through fallback on low tier");
    assert!(out.contains("\"class\":\"useful\""), "got {out}");

    // Synthesis is not serviceable on Low tier — no adapter supports
    // it. Router must emit Unavailable.
    let err = router
        .dispatch(InferenceTask::SynthSummary, "msg")
        .unwrap_err();
    assert!(matches!(err, RouterError::Unavailable { .. }));
}

#[test]
fn medium_tier_runs_classification_but_not_synthesis_on_slm_adapters() {
    let cfg = RouterConfig::default().with_device_tier(DeviceTier::Medium);
    let mlx = MlxAdapter::with_platform_override(cfg.clone(), true);
    assert!(mlx.supports(InferenceTask::TagImportance));
    assert!(!mlx.supports(InferenceTask::SynthSummary));

    let llama = LlamaCppAdapter::new(cfg, Box::new(MockLlamaServerClient::ok("ok")));
    assert!(llama.supports(InferenceTask::ExtractEntities));
    assert!(!llama.supports(InferenceTask::SynthConcept));
}

#[test]
fn high_tier_runs_full_synthesis_through_llama() {
    let cfg = high_tier_config();
    let mlx = MlxAdapter::with_platform_override(cfg.clone(), false);
    let llama = LlamaCppAdapter::new(cfg.clone(),
        Box::new(MockLlamaServerClient::ok("Session summary: deadline reminder",
        )),
    );
    let fallback = FallbackAdapter::new();
    let router = InferenceRouter::new(cfg,
        vec![Box::new(mlx), Box::new(llama), Box::new(fallback)],
    );
    router.bootstrap();
    let out = router
        .dispatch(InferenceTask::SynthSummary, "session text")
        .expect("synth ok");
    assert!(out.contains("Session summary"));
}

// ─────────────────── Warm-up & idle-unload lifecycle ────────────────

#[test]
fn warm_up_uses_priority_order_and_marks_router_warmed() {
    let mlx = ScriptedAdapter::new(AdapterKind::Mlx,
        false,
        vec![InferenceTask::TagImportance],
        Ok("never".into()),
    );
    let llama = ScriptedAdapter::new(AdapterKind::LlamaCpp,
        true,
        vec![InferenceTask::TagImportance],
        Ok("llama-warmup".into()),
    );
    let fallback = ScriptedAdapter::new(AdapterKind::Fallback,
        true,
        vec![InferenceTask::TagImportance],
        Ok("fallback-warmup".into()),
    );
    let router = InferenceRouter::new(high_tier_config(),
        vec![Box::new(mlx), Box::new(llama), Box::new(fallback)],
    );
    router.bootstrap();

    // Pre-warm-up: nothing loaded.
    assert!(!router.is_warmed());
    assert!(!router.is_adapter_loaded(AdapterKind::LlamaCpp));

    // Warm-up should land on the first available adapter (LlamaCpp).
    let kind = router.warm_up().expect("warm-up ok");
    assert_eq!(kind, AdapterKind::LlamaCpp);
    assert!(router.is_warmed());
    assert!(router.is_adapter_loaded(AdapterKind::LlamaCpp));
    assert!(!router.is_adapter_loaded(AdapterKind::Mlx));
}

#[test]
fn idle_sweep_unloads_adapter_and_can_be_rewarmed() {
    let cfg = RouterConfig::default()
        .with_device_tier(DeviceTier::High)
        .with_idle_timeout(60);
    let llama = ScriptedAdapter::new(AdapterKind::LlamaCpp,
        true,
        vec![InferenceTask::TagImportance],
        Ok("served".into()),
    );
    let router = InferenceRouter::new(cfg, vec![Box::new(llama)]);
    router.bootstrap();
    router.warm_up().expect("warm-up ok");
    assert!(router.is_adapter_loaded(AdapterKind::LlamaCpp));

    // Simulate elapsed wall-clock without sleeping.
    let later = Instant::now() + Duration::from_secs(120);
    let unloaded = router.sweep_idle_adapters_at(later);
    assert_eq!(unloaded, vec![AdapterKind::LlamaCpp]);
    assert!(!router.is_adapter_loaded(AdapterKind::LlamaCpp));

    // A subsequent dispatch reloads the adapter — idle unload must
    // not break the dispatch path.
    let out = router
        .dispatch(InferenceTask::TagImportance, "x")
        .expect("dispatch reloads idle adapter");
    assert_eq!(out, "served");
    assert!(router.is_adapter_loaded(AdapterKind::LlamaCpp));
}

#[test]
fn warm_up_returns_none_when_no_adapter_is_available() {
    let mlx = ScriptedAdapter::new(AdapterKind::Mlx,
        false,
        vec![InferenceTask::TagImportance],
        Ok("never".into()),
    );
    let llama = ScriptedAdapter::new(AdapterKind::LlamaCpp,
        false,
        vec![InferenceTask::TagImportance],
        Ok("never".into()),
    );
    let router = InferenceRouter::new(high_tier_config(), vec![Box::new(mlx), Box::new(llama)]);
    router.bootstrap();
    assert!(router.warm_up().is_none());
    assert!(!router.is_warmed());
}

#[test]
fn warm_up_uses_configured_warm_up_prompt() {
    // The warm-up prompt should default to WARM_UP_PROMPT.
    assert_eq!(high_tier_config().warm_up_prompt, WARM_UP_PROMPT);
    // Default idle timeout should match the public constant so
    // operators tuning one value see the other.
    assert_eq!(high_tier_config().idle_timeout_secs,
        IDLE_UNLOAD_TIMEOUT_SECS
    );
}

// ─────────────────────────── Task routing ───────────────────────────

#[test]
fn every_task_variant_routes_through_high_tier_ladder() {
    // High-tier ladder with MLX off (off-Apple Silicon for CI),
    // llama.cpp reachable, fallback present. Every InferenceTask
    // variant must produce *some* response — either via llama.cpp
    // (synthesis + classification) or via the fallback.
    let cfg = high_tier_config();
    let llama_response =
        r#"{"class":"useful","confidence":0.5,"name":"x","summary":"y","facets":{}}"#;
    let make_router = || {
        let mlx = MlxAdapter::with_platform_override(cfg.clone(), false);
        let llama = LlamaCppAdapter::new(cfg.clone(),
            Box::new(MockLlamaServerClient::ok(llama_response)),
        );
        let fallback = FallbackAdapter::new();
        let r = InferenceRouter::new(cfg.clone(),
            vec![Box::new(mlx), Box::new(llama), Box::new(fallback)],
        );
        r.bootstrap();
        r
    };

    for task in [
        InferenceTask::TagImportance,
        InferenceTask::ExtractEntities,
        InferenceTask::PromoteObservation,
        InferenceTask::SynthSummary,
        InferenceTask::SynthConcept,
        InferenceTask::AdjudicateContradiction,
    ] {
        let router = make_router();
        let out = router
            .dispatch(task, "prompt")
            .unwrap_or_else(|e| panic!("task {task:?} failed to dispatch: {e}"));
        assert!(!out.is_empty(), "task {task:?} returned empty response");
    }
}

#[test]
fn dispatch_before_bootstrap_emits_not_probed() {
    let router = InferenceRouter::new(high_tier_config(), vec![Box::new(FallbackAdapter::new())]);
    let err = router
        .dispatch(InferenceTask::TagImportance, "x")
        .unwrap_err();
    assert!(matches!(err, RouterError::NotProbed { .. }));
}

#[test]
fn router_falls_through_on_fallback_signal_errors() {
    // If the primary errors with a fallback-class error
    // (Unavailable / TierTooLow), the router proceeds to the next
    // adapter — but a hard `InferenceFailure` halts dispatch.
    let primary = ScriptedAdapter::new(AdapterKind::Mlx,
        true,
        vec![InferenceTask::TagImportance],
        Err(RouterError::Unavailable {
            task: "tag_importance",
        }),
    );
    let secondary = ScriptedAdapter::new(AdapterKind::LlamaCpp,
        true,
        vec![InferenceTask::TagImportance],
        Ok("secondary".into()),
    );
    let router = InferenceRouter::new(high_tier_config(),
        vec![Box::new(primary), Box::new(secondary)],
    );
    router.bootstrap();
    let out = router
        .dispatch(InferenceTask::TagImportance, "x")
        .expect("router falls through on Unavailable");
    assert_eq!(out, "secondary");
}

#[test]
fn router_does_not_fall_through_on_inference_failure() {
    let primary = ScriptedAdapter::new(AdapterKind::Mlx,
        true,
        vec![InferenceTask::TagImportance],
        Err(RouterError::InferenceFailure("boom".into())),
    );
    let secondary = ScriptedAdapter::new(AdapterKind::LlamaCpp,
        true,
        vec![InferenceTask::TagImportance],
        Ok("secondary".into()),
    );
    let router = InferenceRouter::new(high_tier_config(),
        vec![Box::new(primary), Box::new(secondary)],
    );
    router.bootstrap();
    let err = router
        .dispatch(InferenceTask::TagImportance, "x")
        .unwrap_err();
    assert!(matches!(err, RouterError::InferenceFailure(_)));
}

// ────────────────── FallbackAdapter classification / synthesis ──────

#[test]
fn fallback_adapter_succeeds_on_classification_tasks() {
    let adapter = FallbackAdapter::new();
    adapter.probe();

    // Each (tag, prompt, marker) tuple uses a body designed to drive
    // the lexicon-based fallback to the expected class.
    let class_tasks: &[(&str, &str, &str)] = &[
        ("tag_importance",
            "x\n\nMessage:\nplease investigate the question",
            "\"class\":\"useful\"",
        ),
        ("extract_entities",
            "x\n\nMessage:\n@alice please review https://example.com",
            "\"entities\":",
        ),
        ("promote_observation",
            "x\n\nObservation:\nWe decided to approve",
            "\"promote\":true",
        ),
    ];
    for (tag, prompt, marker) in class_tasks.iter().copied() {
        let out = adapter
            .generate(tag, prompt, "")
            .unwrap_or_else(|e| panic!("classification {tag} should succeed but errored: {e}"));
        assert!(out.contains(marker),
            "classification {tag}: expected {marker:?} in {out:?}"
        );
    }
}

#[test]
fn fallback_adapter_errors_on_synthesis_tasks() {
    let adapter = FallbackAdapter::new();
    adapter.probe();
    for tag in ["synth_summary", "synth_concept", "adjudicate_contradiction"] {
        let err = adapter
            .generate(tag, "", "")
            .unwrap_err_or_panic_with(|e| format!("expected error for synthesis task {tag}: {e}"));
        assert!(matches!(err, RouterError::Unavailable { .. }));
        assert!(err.is_fallback());
    }
}

/// Tiny extension trait so the test above can assert "this Result
/// must be Err" without losing the `Ok` value's debug representation
/// in the panic message. Standard `unwrap_err()` panics with the
/// `Ok` branch's `Debug` impl which is fine but we want a custom
/// message here.
trait ResultExt<T, E> {
    fn unwrap_err_or_panic_with<F: FnOnce(&T) -> String>(self, msg: F) -> E;
}
impl<T: std::fmt::Debug, E> ResultExt<T, E> for Result<T, E> {
    fn unwrap_err_or_panic_with<F: FnOnce(&T) -> String>(self, msg: F) -> E {
        match self {
            Err(e) => e,
            Ok(v) => panic!("{}", msg(&v)),
        }
    }
}

// ─────────────────────────── RouterError ────────────────────────────

#[test]
fn router_error_variants_are_all_constructible_via_real_call_paths() {
    // Unavailable: comes from FallbackAdapter on synthesis tasks.
    let fallback = FallbackAdapter::new();
    fallback.probe();
    let err = fallback.generate("synth_summary", "", "").unwrap_err();
    assert!(matches!(err, RouterError::Unavailable { .. }));
    assert!(err.is_fallback());

    // InferenceFailure: a hard error that must NOT be a fallback.
    // (MlxAdapter no longer produces this variant — it correctly
    // returns Unavailable when the runtime is absent — but other
    // adapters may produce it, e.g. llama.cpp HTTP transport errors.)
    let inference_failure = RouterError::InferenceFailure("boom".into());
    assert!(!inference_failure.is_fallback());

    // MlxAdapter without a linked runtime now returns Unavailable
    // (a fallback error), which lets the router fall through.
    let cfg = high_tier_config();
    let mlx = MlxAdapter::with_platform_override(cfg.clone(), true);
    mlx.probe();
    let err = mlx.generate("tag_importance", "", "").unwrap_err();
    assert!(matches!(err, RouterError::Unavailable { .. }));
    assert!(err.is_fallback());

    // NotProbed: dispatch before bootstrap.
    let unbooted = InferenceRouter::new(high_tier_config(), vec![Box::new(FallbackAdapter::new())]);
    let err = unbooted
        .dispatch(InferenceTask::TagImportance, "x")
        .unwrap_err();
    assert!(matches!(err, RouterError::NotProbed { .. }));

    // TierTooLow: not produced by any adapter today, but the variant
    // must still construct & classify as fallback so the substrate's
    // routing logic stays correct if a future adapter starts emitting
    // it.
    let tier_too_low = RouterError::TierTooLow {
        tier: "low",
        task: "synth_summary",
    };
    assert!(tier_too_low.is_fallback());
    assert!(tier_too_low.to_string().contains("low"));
}

#[test]
fn router_unavailable_error_carries_task_tag() {
    let cfg = high_tier_config();
    let mlx = MlxAdapter::with_platform_override(cfg.clone(), false);
    let llama = LlamaCppAdapter::new(cfg.clone(), Box::new(MockLlamaServerClient::unreachable()));
    let fallback = FallbackAdapter::new();
    let router = InferenceRouter::new(cfg,
        vec![Box::new(mlx), Box::new(llama), Box::new(fallback)],
    );
    router.bootstrap();

    let err = router
        .dispatch(InferenceTask::SynthSummary, "x")
        .unwrap_err();
    match err {
        RouterError::Unavailable { task } => {
            assert_eq!(task, "synth_summary");
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }
}
