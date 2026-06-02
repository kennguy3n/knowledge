//! Synthesis request batcher.
//!
//! At server-side scale, many scopes may need synthesis at the
//! same time. The naive flow is "thread spawns → each thread
//! drives its own [`HttpManagedEndpointSynthesizer::synthesize_domain`]
//! → upstream sees N concurrent connections" — which both wastes
//! upstream rate-limit budget and (because the inference endpoint
//! is paid by the request) inflates the operator bill.
//!
//! [`SynthesisBatcher`] sits in front of the synthesizer and
//! converts that fan-out into a serialised flush:
//!
//! 1. Callers `submit_domain` / `submit_tenant` their work and
//!    receive a [`PendingHandle`] that exposes a oneshot
//!    [`std::sync::mpsc::Receiver`] on which the result will land.
//! 2. The batcher buffers pending requests in an internal `Mutex`.
//! 3. When the queue depth reaches `batch_size`, *or* when the
//!    oldest pending request is older than `batch_timeout`, the
//!    next call to [`SynthesisBatcher::flush_if_ready`] (or an
//!    explicit [`SynthesisBatcher::flush_now`]) drains the queue
//!    and dispatches **sequentially** through the wrapped
//!    synthesizer. Each result is forwarded on the corresponding
//!    pending handle.
//!
//! "Batching" here is intentionally serial — the wrapped
//! synthesizer holds an `Arc<RateLimiter>` (Item 19) and the
//! batcher additionally exposes a way to install a *shared*
//! limiter across the whole batch. Serial dispatch through one
//! limiter is the simplest design that achieves the cost-control
//! goal:
//!
//! * Upstream sees one request at a time, never a thundering herd.
//! * The rate-limiter cap is enforced across the entire batch
//!   rather than per-call, so a single very large batch cannot
//!   overshoot the cap.
//! * Submission is non-blocking (the caller is not parked on the
//!   network round-trip) so the substrate's three-phase locking
//!   discipline still holds.
//!
//! A future revision can promote dispatch to a worker thread with
//! a parallelism cap — the public surface here stays the same.
//!
//! ## Why `std::sync::mpsc` instead of an async oneshot
//!
//! The synthesis engine is sync-on-purpose (the FFI surface is
//! sync, and the TEE worker's `&mut SynthesisWindowManager`
//! signature would require `Send`-and-`'static` lifetimes that
//! `tokio::sync::oneshot` is happy to satisfy but `async-std` and
//! `smol` are not). `std::sync::mpsc::sync_channel(1)` is a
//! one-shot in all but name, ships in std, and has zero runtime
//! dependencies — which matches the rest of the synthesis_engine
//! crate.

use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use synthesis_pipeline::{
    DomainSynthesisInput, SynthesisWindowManager, TenantSynthesisInput, TieredWindowHandle,
};

use crate::engine::{DomainSynthesisResult, SynthesisEngine, TenantSynthesisResult};
use crate::error::{EngineError, Result};
use crate::managed_endpoint::{HttpClient, HttpManagedEndpointSynthesizer};
use crate::rate_limiter::RateLimiter;

/// One pending request inside the batcher.
///
/// The variant tags whether the dispatch must hit
/// `synthesize_domain` vs. `synthesize_tenant`; the embedded
/// `TieredWindowHandle` carries the per-window lifecycle ID that
/// the downstream synthesizer marks `InProgress` → `Complete` /
/// `Failed` against.
enum PendingKind {
    Domain {
        handle: TieredWindowHandle,
        input: DomainSynthesisInput,
        reply: SyncSender<Result<DomainSynthesisResult>>,
    },
    Tenant {
        handle: TieredWindowHandle,
        input: TenantSynthesisInput,
        reply: SyncSender<Result<TenantSynthesisResult>>,
    },
}

struct Pending {
    kind: PendingKind,
    enqueued_at: Instant,
}

/// Handle returned from `submit_domain` / `submit_tenant`.
///
/// Block on [`wait`](PendingHandle::wait) or poll
/// [`try_take`](PendingHandle::try_take) to receive the synthesis
/// result. The result type is generic over the synthesis tier so
/// each submission slot only exposes the variant it can actually
/// observe.
#[derive(Debug)]
pub struct PendingHandle<T> {
    rx: Receiver<Result<T>>,
}

impl<T> PendingHandle<T> {
    /// Park the calling thread until the batch flushes the
    /// request and the synthesizer returns. The channel is
    /// rendezvous-style so this is the only way for the
    /// dispatch to observe back-pressure from a slow caller.
    pub fn wait(self) -> Result<T> {
        match self.rx.recv() {
            Ok(r) => r,
            // The sender side was dropped without sending — only
            // happens if the batcher itself was dropped mid-flush
            // (e.g. shutdown), in which case treat it as an engine
            // refusal so callers can surface a stable error to
            // upstream consumers.
            Err(_) => Err(EngineError::engine("batcher dropped before this request was dispatched",
            )),
        }
    }

    /// Non-blocking poll. Returns `Ok(None)` if the dispatch has
    /// not yet flushed the request, `Ok(Some(result))` once it
    /// has, and `Err(...)` if the batcher was dropped.
    pub fn try_take(&mut self) -> Result<Option<T>> {
        use std::sync::mpsc::TryRecvError;
        match self.rx.try_recv() {
            Ok(r) => r.map(Some),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(EngineError::engine("batcher dropped before this request was dispatched",
            )),
        }
    }
}

/// Server-side synthesis batcher.
///
/// See the module-level docs for the dispatch model. The generic
/// `C: HttpClient` parameter is the same one the wrapped
/// synthesizer takes so test code can wire the
/// [`crate::managed_endpoint::MockHttpClient`] in directly.
pub struct SynthesisBatcher<C: HttpClient> {
    synthesizer: HttpManagedEndpointSynthesizer<C>,
    batch_size: usize,
    batch_timeout: Duration,
    pending: Mutex<Vec<Pending>>,
}

impl<C: HttpClient> SynthesisBatcher<C> {
    /// Construct a fresh batcher.
    ///
    /// `batch_size` is the queue depth at which
    /// [`Self::flush_if_ready`] becomes a no-op-to-flush
    /// transition. `batch_timeout` is the maximum age of the
    /// oldest pending request that the batcher will tolerate
    /// before flushing regardless of queue depth. Both are
    /// independent triggers — the first to fire flushes.
    pub fn new(synthesizer: HttpManagedEndpointSynthesizer<C>,
        batch_size: usize,
        batch_timeout: Duration,
    ) -> Self {
        assert!(batch_size > 0, "batch_size must be >= 1");
        Self {
            synthesizer,
            batch_size,
            batch_timeout,
            pending: Mutex::new(Vec::new()),
        }
    }

    /// Constructor variant that wires a shared [`RateLimiter`]
    /// onto the wrapped synthesizer before storing it.
    ///
    /// This is the cost-control hook called out in: a
    /// single limiter shared across every synthesizer instance
    /// the batcher dispatches through, so a many-scope flush
    /// respects the operator's per-minute cap even when the
    /// batch is "the entire queue at once".
    ///
    /// We expose this as a constructor (rather than a post-
    /// construction builder method) because [`HttpClient`] is
    /// not `Clone` — mutating the synthesizer in place would
    /// require either an `Option<...>::take` dance or a
    /// throwaway `HttpClient`, both of which are clumsier than
    /// "wire it up before you hand the synthesizer to the
    /// batcher".
    pub fn with_shared_rate_limiter(synthesizer: HttpManagedEndpointSynthesizer<C>,
        batch_size: usize,
        batch_timeout: Duration,
        limiter: Arc<RateLimiter>,
    ) -> Self {
        let synthesizer = synthesizer.with_shared_rate_limiter(limiter);
        Self::new(synthesizer, batch_size, batch_timeout)
    }

    /// Borrow the active batch size.
    pub fn batch_size(&self) -> usize {
        self.batch_size
    }

    /// Borrow the active batch timeout.
    pub fn batch_timeout(&self) -> Duration {
        self.batch_timeout
    }

    /// Borrow the active queue depth.
    pub fn pending_len(&self) -> usize {
        self.pending
            .lock()
            .expect("batcher pending mutex poisoned")
            .len()
    }

    /// Borrow the wrapped synthesizer.
    pub fn synthesizer(&self) -> &HttpManagedEndpointSynthesizer<C> {
        &self.synthesizer
    }

    /// Enqueue a domain-tier synthesis request.
    ///
    /// Returns a [`PendingHandle`] that yields the result once
    /// the batch is flushed. The caller is responsible for
    /// driving the flush (typically via
    /// [`Self::flush_if_ready`] on a worker tick).
    pub fn submit_domain(&self,
        handle: TieredWindowHandle,
        input: DomainSynthesisInput,
    ) -> PendingHandle<DomainSynthesisResult> {
        let (tx, rx) = sync_channel::<Result<DomainSynthesisResult>>(1);
        self.pending
            .lock()
            .expect("batcher pending mutex poisoned")
            .push(Pending {
                kind: PendingKind::Domain {
                    handle,
                    input,
                    reply: tx,
                },
                enqueued_at: Instant::now(),
            });
        PendingHandle { rx }
    }

    /// Enqueue a tenant-tier synthesis request.
    pub fn submit_tenant(&self,
        handle: TieredWindowHandle,
        input: TenantSynthesisInput,
    ) -> PendingHandle<TenantSynthesisResult> {
        let (tx, rx) = sync_channel::<Result<TenantSynthesisResult>>(1);
        self.pending
            .lock()
            .expect("batcher pending mutex poisoned")
            .push(Pending {
                kind: PendingKind::Tenant {
                    handle,
                    input,
                    reply: tx,
                },
                enqueued_at: Instant::now(),
            });
        PendingHandle { rx }
    }

    /// Returns `true` iff the queue depth has reached
    /// `batch_size` *or* the oldest pending request is older than
    /// `batch_timeout`. Used by the caller-side flush loop to
    /// decide whether to flush on a tick.
    pub fn should_flush(&self) -> bool {
        let pending = self.pending.lock().expect("batcher pending mutex poisoned");
        if pending.len() >= self.batch_size {
            return true;
        }
        match pending.first() {
            Some(first) => first.enqueued_at.elapsed() >= self.batch_timeout,
            None => false,
        }
    }

    /// Flush the queue iff [`Self::should_flush`] returns `true`.
    ///
    /// Returns the number of requests dispatched (zero on a
    /// no-op flush).
    pub fn flush_if_ready(&self, windows: &mut SynthesisWindowManager) -> usize {
        if !self.should_flush() {
            return 0;
        }
        self.flush_now(windows)
    }

    /// Drain the queue unconditionally and dispatch every
    /// pending request sequentially through the wrapped
    /// synthesizer. Returns the number of requests dispatched.
    ///
    /// Each request's result is forwarded on its own oneshot
    /// channel; a dispatch failure is forwarded as an `Err`
    /// (the receiver still wakes up). The wrapped synthesizer's
    /// per-window failure path (`mark_failed` on dispatch error)
    /// is invoked by the synthesizer itself; the batcher does
    /// not need to replicate that bookkeeping.
    pub fn flush_now(&self, windows: &mut SynthesisWindowManager) -> usize {
        let drained: Vec<Pending> = {
            let mut pending = self.pending.lock().expect("batcher pending mutex poisoned");
            std::mem::take(&mut *pending)
        };
        let count = drained.len();

        for item in drained {
            match item.kind {
                PendingKind::Domain {
                    handle,
                    input,
                    reply,
                } => {
                    let result = self.synthesizer.synthesize_domain(windows, handle, input);
                    // If the receiver is gone we silently drop
                    // the result; that is the correct behaviour
                    // for "caller stopped waiting".
                    let _ = reply.send(result);
                }
                PendingKind::Tenant {
                    handle,
                    input,
                    reply,
                } => {
                    let result = self.synthesizer.synthesize_tenant(windows, handle, input);
                    let _ = reply.send(result);
                }
            }
        }
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::managed_endpoint::{EndpointConfig, MockHttpClient};
    use chrono::{Duration as ChronoDuration, Utc};
    use evidence_store::ScopeId;
    use memory_manager::{ApprovedDocumentRef, DomainMemoryObject, TenantMemoryObject};
    use synthesis_pipeline::{
        ApprovedDocument, ChannelOutput, DomainOutput, HierarchyEnforcedWindowManager,
        SynthesisObject, SynthesisObjectType, SynthesisWindowManager, WindowScopeTier,
    };
    use uuid::Uuid;

    fn cfg() -> EndpointConfig {
        EndpointConfig::new("https://example.test/synth", "TEST_API_KEY", "slm-recap-v1")
            .with_max_tokens(64)
            .with_timeout(Duration::from_secs(5))
            .with_grammar("{root: 'string'}")
    }

    fn channel_recap(scope: ScopeId, payload: &[u8]) -> SynthesisObject {
        let mut mgr = SynthesisWindowManager::new();
        let now = Utc::now();
        let win = mgr
            .open_window(scope, now - ChronoDuration::seconds(60), now)
            .unwrap();
        SynthesisObject::new(scope,
            win,
            SynthesisObjectType::ChannelRecap,
            payload.to_vec(),
            Uuid::nil(),
        )
    }

    fn domain_summary(scope: ScopeId, payload: &[u8]) -> SynthesisObject {
        let mut mgr = SynthesisWindowManager::new();
        let now = Utc::now();
        let win = mgr
            .open_window(scope, now - ChronoDuration::seconds(60), now)
            .unwrap();
        SynthesisObject::new(scope,
            win,
            SynthesisObjectType::DomainSummary,
            payload.to_vec(),
            Uuid::nil(),
        )
    }

    fn build_domain_input(domain_scope: ScopeId,
        channel: ScopeId,
        body: &[u8],
    ) -> DomainSynthesisInput {
        let mut domain = DomainMemoryObject::new(domain_scope);
        domain.attach_channel_scope(channel);
        let outputs =
            vec![ChannelOutput::from_channel_object(channel_recap(channel, body)).unwrap()];
        DomainSynthesisInput::new(&domain, outputs).unwrap()
    }

    fn build_tenant_input(tenant_scope: ScopeId,
        domain_scope: ScopeId,
        body: &[u8],
    ) -> TenantSynthesisInput {
        let mut tenant = TenantMemoryObject::new(tenant_scope);
        tenant.attach_domain_scope(domain_scope);
        let approved_ref = ApprovedDocumentRef::new("Handbook", "@admin");
        tenant.admit_approved_document(approved_ref.clone());
        let outputs =
            vec![DomainOutput::from_domain_object(domain_summary(domain_scope, body)).unwrap()];
        let docs = vec![ApprovedDocument::new(approved_ref,
            b"approved-blob".to_vec(),
        )];
        TenantSynthesisInput::new(&tenant, outputs, docs).unwrap()
    }

    fn open_domain(mgr: &mut SynthesisWindowManager, scope: ScopeId) -> TieredWindowHandle {
        let now = Utc::now();
        mgr.open_tiered_window(scope,
            WindowScopeTier::Domain,
            now - ChronoDuration::seconds(60),
            now,
        )
        .unwrap()
    }

    fn open_tenant(mgr: &mut SynthesisWindowManager, scope: ScopeId) -> TieredWindowHandle {
        let now = Utc::now();
        mgr.open_tiered_window(scope,
            WindowScopeTier::Tenant,
            now - ChronoDuration::seconds(60),
            now,
        )
        .unwrap()
    }

    #[test]
    fn submit_does_not_dispatch_until_flush() {
        let synth = HttpManagedEndpointSynthesizer::new(cfg(), MockHttpClient::echo());
        let batcher = SynthesisBatcher::new(synth, 3, Duration::from_secs(60));

        let domain_scope = ScopeId::new_v4();
        let channel = ScopeId::new_v4();
        let mut mgr = SynthesisWindowManager::new();
        let handle = open_domain(&mut mgr, domain_scope);
        let input = build_domain_input(domain_scope, channel, b"a-recap");
        let mut pending = batcher.submit_domain(handle, input);

        assert_eq!(batcher.pending_len(), 1);
        assert!(!batcher.should_flush(), "queue is below batch_size");
        assert!(matches!(pending.try_take(), Ok(None)),
            "submit must not dispatch eagerly"
        );

        // No requests recorded on the mock yet, confirming the
        // batcher buffered without dispatching.
        assert_eq!(batcher.synthesizer().client().recorded_requests().len(), 0);
    }

    #[test]
    fn flush_at_batch_size_dispatches_all_pending() {
        let synth = HttpManagedEndpointSynthesizer::new(cfg(), MockHttpClient::echo());
        let batcher = SynthesisBatcher::new(synth, 2, Duration::from_secs(60));

        let domain_scope = ScopeId::new_v4();
        let channel = ScopeId::new_v4();
        let mut mgr = SynthesisWindowManager::new();

        let handle_a = open_domain(&mut mgr, domain_scope);
        let pa = batcher.submit_domain(handle_a, build_domain_input(domain_scope, channel, b"a"));
        let handle_b = open_domain(&mut mgr, domain_scope);
        let pb = batcher.submit_domain(handle_b, build_domain_input(domain_scope, channel, b"b"));

        assert!(batcher.should_flush(), "queue == batch_size triggers flush");
        let dispatched = batcher.flush_if_ready(&mut mgr);
        assert_eq!(dispatched, 2);
        assert_eq!(batcher.pending_len(), 0);
        assert_eq!(batcher.synthesizer().client().recorded_requests().len(), 2);

        let ra = pa.wait().expect("a must succeed");
        let rb = pb.wait().expect("b must succeed");
        assert!(String::from_utf8_lossy(&ra.object.payload).contains('a'));
        assert!(String::from_utf8_lossy(&rb.object.payload).contains('b'));
    }

    #[test]
    fn flush_now_drains_below_batch_size() {
        let synth = HttpManagedEndpointSynthesizer::new(cfg(), MockHttpClient::echo());
        let batcher = SynthesisBatcher::new(synth, 10, Duration::from_secs(60));

        let domain_scope = ScopeId::new_v4();
        let channel = ScopeId::new_v4();
        let mut mgr = SynthesisWindowManager::new();
        let handle = open_domain(&mut mgr, domain_scope);
        let _p = batcher.submit_domain(handle, build_domain_input(domain_scope, channel, b"x"));

        assert!(!batcher.should_flush());
        let dispatched = batcher.flush_now(&mut mgr);
        assert_eq!(dispatched, 1);
        assert_eq!(batcher.pending_len(), 0);
    }

    #[test]
    fn batch_timeout_triggers_flush_even_under_size() {
        let synth = HttpManagedEndpointSynthesizer::new(cfg(), MockHttpClient::echo());
        let batcher = SynthesisBatcher::new(synth, 10, Duration::from_millis(50));
        let domain_scope = ScopeId::new_v4();
        let channel = ScopeId::new_v4();
        let mut mgr = SynthesisWindowManager::new();
        let handle = open_domain(&mut mgr, domain_scope);
        let _p = batcher.submit_domain(handle, build_domain_input(domain_scope, channel, b"x"));

        // Sleep past the timeout; the next `should_flush` must
        // see the oldest pending crossing the threshold.
        std::thread::sleep(Duration::from_millis(80));
        assert!(batcher.should_flush(),
            "batch_timeout must trigger flush even when below batch_size"
        );
        assert_eq!(batcher.flush_if_ready(&mut mgr), 1);
    }

    #[test]
    fn shared_rate_limiter_caps_across_batch() {
        let synth = HttpManagedEndpointSynthesizer::new(cfg(), MockHttpClient::echo());
        let limiter = Arc::new(RateLimiter::new(2));
        let batcher = SynthesisBatcher::with_shared_rate_limiter(synth,
            10,
            Duration::from_secs(60),
            Arc::clone(&limiter),
        );

        let domain_scope = ScopeId::new_v4();
        let channel = ScopeId::new_v4();
        let mut mgr = SynthesisWindowManager::new();
        let mut pendings = Vec::new();
        for i in 0..3 {
            let handle = open_domain(&mut mgr, domain_scope);
            let body = format!("recap-{i}");
            pendings.push(batcher.submit_domain(handle,
                build_domain_input(domain_scope, channel, body.as_bytes()),
            ));
        }
        assert_eq!(batcher.flush_now(&mut mgr), 3);

        let outcomes: Vec<Result<DomainSynthesisResult>> =
            pendings.into_iter().map(PendingHandle::wait).collect();
        let ok = outcomes.iter().filter(|r| r.is_ok()).count();
        let err = outcomes.iter().filter(|r| r.is_err()).count();
        assert_eq!(ok, 2,
            "shared rate limiter must admit exactly `cap` across the batch"
        );
        assert_eq!(err, 1,
            "the third request must be rejected by the shared limiter"
        );
        assert_eq!(limiter.current_window_count(), 2);
    }

    #[test]
    fn tenant_submission_round_trips_through_flush() {
        let synth = HttpManagedEndpointSynthesizer::new(cfg(), MockHttpClient::echo());
        let batcher = SynthesisBatcher::new(synth, 1, Duration::from_secs(60));

        let tenant_scope = ScopeId::new_v4();
        let domain_scope = ScopeId::new_v4();
        let mut mgr = SynthesisWindowManager::new();
        let handle = open_tenant(&mut mgr, tenant_scope);

        let pending = batcher.submit_tenant(handle,
            build_tenant_input(tenant_scope, domain_scope, b"a-domain"),
        );
        assert!(batcher.should_flush());
        assert_eq!(batcher.flush_if_ready(&mut mgr), 1);

        let r = pending.wait().expect("tenant synthesis succeeds");
        assert_eq!(r.object.object_type, SynthesisObjectType::TenantSummary);
        assert!(String::from_utf8_lossy(&r.object.payload).contains("a-domain"));
    }
}
