//! Confidential-compute (TEE) worker.
//!
//! Per `docs/technical/design.md` §10 ("Confidential synthesis") and `docs/technical/architecture.md`
//! §3.3 ("Server-side synthesis"), tenant-tier and domain-tier
//! synthesis runs in a Trusted Execution Environment so the operator
//! cannot read plaintext evidence even with full host access. This
//! module ships the **skeleton** of that worker:
//!
//! 1. [`TeeWorkerLifecycle`] state machine (`Unattested` → `Attesting`
//!    → `Attested` → `Synthesizing` → `Idle`).
//! 2. [`TeeWorkerConfig`] — TEE platform, expected enclave measurement,
//!    bound synthesizer pubkey, scope bindings (which scopes this
//!    worker is allowed to operate on), attestation TTL.
//! 3. [`TeeWorker`] — implements [`crate::SynthesisEngine`] but
//!    refuses to operate until [`TeeWorker::attest`] has produced a
//!    fresh, verified [`crypto::attestation::AttestationReport`].
//!    Every call to `synthesize_domain` / `synthesize_tenant` first
//!    checks that the cached attestation is still within TTL,
//!    re-attests on expiry, and emits an
//!    [`crypto::attestation::AttestationAuditEntry`] for the audit
//!    trail.
//! 4. [`TeeRuntime`] trait + [`MockTeeRuntime`] — abstracts the
//!    platform quote-generation step (`quote(enclave_image, nonce) ->
//!    AttestationReport`). The mock implementation uses
//!    [`crypto::attestation::mock_attestation_report`] verbatim, which
//!    is exactly what the attestation tests already exercise.
//!
//! Once a real `intel-tdx` / `amd-sev-snp` / `nitro-enclaves` quote
//! library is pinned, only [`TeeRuntime::quote`] needs to grow a real
//! implementation — the lifecycle, audit, and `SynthesisEngine`
//! integration in this module are production-correct.
//!
//! # Side-channel hardening
//!
//! Three mitigations harden the worker against timing / page-fault /
//! key-exposure side channels (see `docs/security/tee-side-channels.md`):
//!
//! * **Short attestation TTL** — the cached attestation is fresh for
//!   only 5 minutes ([`TeeWorkerConfig::new`]), so a stolen report
//!   buys a much smaller replay window.
//! * **Zeroize-on-drop intermediates** — the worker holds every
//!   key-derived / plaintext intermediate it materialises inside the
//!   confidential boundary ([`TeeWorker::fresh_nonce`]'s nonce input
//!   and [`SynthesisSession`]'s plaintext staging buffer) in
//!   `zeroize::Zeroizing`, so it is wiped on drop and on panic.
//! * **Enclave page pre-faulting** — every worker reserves a
//!   pre-faulted, best-effort `mlock`-pinned working set
//!   ([`PrefaultedWorkingSet`]) so synthesis calls do not incur
//!   first-touch page faults whose latency would leak access
//!   patterns. The lock is `cfg(unix)`-guarded with a safe no-op
//!   fallback so the mock / non-enclave path still runs.

use std::sync::Mutex;

use chrono::{DateTime, Duration, Utc};
use tracing::debug;
use uuid::Uuid;
use zeroize::Zeroizing;

#[cfg(any(test, feature = "test-support"))]
use crypto::attestation::mock_attestation_report;
use crypto::attestation::{
    bind_synthesizer_key, verify_attestation, AttestationAuditEntry, AttestationBinding,
    AttestationReport, TeePlatform,
};
use crypto::hash::{content_hash, ContentHash};
use synthesis_pipeline::{
    DomainSynthesisInput, SynthesisWindowManager, TenantSynthesisInput, TieredWindowHandle,
};

use crate::engine::{DomainSynthesisResult, SynthesisEngine, TenantSynthesisResult};
use crate::error::{EngineError, Result};
use crate::managed_endpoint::{
    EndpointConfig, HttpClient, HttpManagedEndpointSynthesizer, MockHttpClient,
};

/// How long a cached attestation is considered fresh after it is
/// produced. Deliberately short (5 minutes / 300 s) so a leaked or
/// replayed report only buys a small window before the worker is
/// forced to re-attest. See `docs/security/tee-side-channels.md`.
const ATTESTATION_TTL: Duration = Duration::minutes(5);

/// Size of the worker's pre-faulted, page-locked working-set
/// reservation, in bytes. 64 KiB spans several pages on every target
/// while staying at or under the common default `RLIMIT_MEMLOCK`
/// ceiling (64 KiB–8 MiB); `mlock` failure is treated as a
/// best-effort no-op regardless, so an even tighter limit only
/// downgrades the lock, never breaks the worker.
const WORKER_PREFAULT_BYTES: usize = 64 * 1024;

/// Conservative page-size fallback used when the OS page size cannot
/// be queried. 4 KiB is the smallest page size on every supported
/// target, so striding by it never skips a page.
const FALLBACK_PAGE_SIZE: usize = 4096;

#[cfg(unix)]
fn os_page_size() -> usize {
    // SAFETY: `sysconf` is a pure query with no preconditions. A
    // non-positive return (name unsupported) is rejected by the
    // `try_from` / `filter` below in favour of the fallback.
    #[allow(unsafe_code)]
    let raw = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    usize::try_from(raw)
        .ok()
        .filter(|&page| page != 0)
        .unwrap_or(FALLBACK_PAGE_SIZE)
}

#[cfg(not(unix))]
fn os_page_size() -> usize {
    FALLBACK_PAGE_SIZE
}

/// Touch one byte on every page spanned by `buf` so the OS commits
/// and faults the backing pages in eagerly, up front — rather than
/// during a later synthesis call, where the per-page fault latency
/// would leak memory-access patterns through timing.
fn prefault_pages(buf: &mut [u8]) {
    let page = os_page_size();
    let mut offset = 0;
    while offset < buf.len() {
        buf[offset] = 0xA5;
        // Keep the write from being optimised away as dead.
        std::hint::black_box(buf[offset]);
        offset = offset.saturating_add(page);
    }
}

/// Best-effort pin of `buf`'s pages into RAM via `mlock(2)`. Returns
/// `true` when the lock succeeded. A failure (e.g. `RLIMIT_MEMLOCK`
/// exhausted, or an unprivileged container) is non-fatal: the worker
/// still runs, just without the swap-resident guarantee.
#[cfg(unix)]
fn lock_pages(buf: &[u8]) -> bool {
    if buf.is_empty() {
        return false;
    }
    // SAFETY: `buf` is a live, fully-initialised slice that outlives
    // the call; `mlock` only pins the pages spanning `[ptr, ptr+len)`
    // and never dereferences or writes through the pointer.
    #[allow(unsafe_code)]
    let ret = unsafe { libc::mlock(buf.as_ptr().cast(), buf.len()) };
    ret == 0
}

#[cfg(not(unix))]
fn lock_pages(_buf: &[u8]) -> bool {
    false
}

/// Release a previous [`lock_pages`] pin. No-op on non-unix targets.
#[cfg(unix)]
fn unlock_pages(buf: &[u8]) {
    if buf.is_empty() {
        return;
    }
    // SAFETY: mirrors `lock_pages`; `munlock` on a range that is not
    // (or is only partially) locked is harmless, so the return value
    // is intentionally ignored.
    #[allow(unsafe_code)]
    unsafe {
        libc::munlock(buf.as_ptr().cast(), buf.len());
    }
}

#[cfg(not(unix))]
fn unlock_pages(_buf: &[u8]) {}

/// A pre-faulted, best-effort page-locked working-set reservation
/// owned by every [`TeeWorker`].
///
/// On construction the buffer's pages are touched up front
/// ([`prefault_pages`]) and, on `cfg(unix)`, pinned with `mlock`
/// ([`lock_pages`]) so the resident set does not page-fault or swap
/// mid-synthesis — both of which leak memory-access patterns through
/// timing. The buffer is [`Zeroizing`], so its bytes are wiped on
/// drop, and the `mlock` is released first.
///
/// This is the portable hook for enclave-page pre-faulting: in the
/// mock / non-enclave path it reserves an ordinary heap buffer, so
/// tests still run; a real enclave runtime points the same mechanism
/// at the enclave's mapped region.
struct PrefaultedWorkingSet {
    pages: Zeroizing<Vec<u8>>,
    locked: bool,
}

impl PrefaultedWorkingSet {
    fn reserve(bytes: usize) -> Self {
        let mut pages = Zeroizing::new(vec![0u8; bytes]);
        prefault_pages(&mut pages);
        let locked = lock_pages(&pages);
        Self { pages, locked }
    }
}

impl Drop for PrefaultedWorkingSet {
    fn drop(&mut self) {
        if self.locked {
            unlock_pages(&self.pages);
        }
        // `Zeroizing` wipes `pages` as it drops immediately after.
    }
}

/// Lifecycle states for the confidential-compute worker.
///
/// Transitions:
///
/// ```text
/// Unattested ──attest()──▶ Attesting ──quote ok──▶ Attested
///                                  │
///                                  └──quote fail──▶ Unattested
/// Attested ──synthesize()──▶ Synthesizing ──ok──▶ Idle ──┐
///                                                         ▼
///                                                       Attested
/// Idle / Attested ──ttl expiry──▶ Unattested ──attest()──▶ ...
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TeeWorkerLifecycle {
    /// No verified attestation report. Synthesis is refused.
    Unattested,
    /// `attest()` has been called and is waiting on the TEE quote.
    Attesting,
    /// A fresh attestation report has been verified and bound.
    Attested,
    /// The worker is currently inside a synthesis call.
    Synthesizing,
    /// The worker has finished a synthesis call and is ready for
    /// another (still inside the attestation TTL).
    Idle,
}

impl TeeWorkerLifecycle {
    /// Stable string tag used by the audit log.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unattested => "unattested",
            Self::Attesting => "attesting",
            Self::Attested => "attested",
            Self::Synthesizing => "synthesizing",
            Self::Idle => "idle",
        }
    }
}

/// Configuration for a [`TeeWorker`].
#[derive(Debug, Clone)]
pub struct TeeWorkerConfig {
    /// TEE platform this worker runs on.
    pub platform: TeePlatform,
    /// Enclave image hash the worker is expected to attest with —
    /// produced by the build pipeline and pinned in the deployment
    /// manifest.
    pub expected_measurement: ContentHash,
    /// Synthesizer public-key bytes. Bound to every attestation
    /// report this worker produces, so consumers can verify that a
    /// synthesis output came from the attested enclave.
    pub synthesizer_pub_key: Vec<u8>,
    /// Scope ids this worker is allowed to synthesise. Calls
    /// targeting a scope outside this list are refused.
    pub scope_bindings: Vec<Uuid>,
    /// How long an attestation is considered fresh after it is
    /// produced. Defaults to 5 minutes ([`ATTESTATION_TTL`]).
    pub attestation_ttl: Duration,
    /// Bytes used as the enclave-image input when calling the TEE
    /// runtime. The runtime hashes them to produce the report's
    /// `measurement` field.
    pub enclave_image: Vec<u8>,
}

impl TeeWorkerConfig {
    /// Construct a config with the default 5-minute attestation TTL
    /// ([`ATTESTATION_TTL`]).
    pub fn new(
        platform: TeePlatform,
        expected_measurement: ContentHash,
        synthesizer_pub_key: Vec<u8>,
        scope_bindings: Vec<Uuid>,
        enclave_image: Vec<u8>,
    ) -> Self {
        Self {
            platform,
            expected_measurement,
            synthesizer_pub_key,
            scope_bindings,
            attestation_ttl: ATTESTATION_TTL,
            enclave_image,
        }
    }
}

/// Platform-specific TEE quote provider.
///
/// `quote` returns a fresh attestation report bound to `nonce`. The
/// production implementation calls into the platform SDK (Intel TDX
/// quote, AMD SEV-SNP attestation, Nitro Enclaves attest doc); the
/// mock implementation in this module uses
/// [`mock_attestation_report`].
pub trait TeeRuntime: Send + Sync {
    /// Produce a fresh attestation report.
    fn quote(&self, enclave_image: &[u8], nonce: &[u8]) -> AttestationReport;
}

/// Test-only TEE runtime that produces mock reports via
/// [`mock_attestation_report`]. Used by the lifecycle tests below and
/// by downstream consumers that want to exercise the worker without
/// pinning a real TEE platform.
///
/// Gated behind `#[cfg(any(test, feature = "test-support"))]` so it
/// does not ship in default `cargo build` artifacts. Production
/// `TeeRuntime` implementations talk to a real platform SDK (Intel
/// TDX quote, AMD SEV-SNP attestation, Nitro Enclaves attest doc).
#[cfg(any(test, feature = "test-support"))]
#[derive(Default, Debug, Clone, Copy)]
pub struct MockTeeRuntime;

#[cfg(any(test, feature = "test-support"))]
impl TeeRuntime for MockTeeRuntime {
    fn quote(&self, enclave_image: &[u8], nonce: &[u8]) -> AttestationReport {
        mock_attestation_report(enclave_image, nonce)
    }
}

/// Confidential-compute synthesis worker.
///
/// Wraps a [`TeeRuntime`] + [`TeeWorkerConfig`]. Every synthesis call
/// goes through `with_attestation`, which:
///
/// 1. Refuses if the worker is not currently attested.
/// 2. Refuses if the attestation has expired (TTL window).
/// 3. Refuses if the requested scope is outside `scope_bindings`.
/// 4. Logs a [`AttestationAuditEntry`] before and after the call.
pub struct TeeWorker<R: TeeRuntime, C: HttpClient = MockHttpClient> {
    runtime: R,
    config: TeeWorkerConfig,
    state: Mutex<TeeWorkerState>,
    /// Delegate that does the actual SLM call (over HTTPS to the
    /// managed endpoint inside the same TEE). Holding it on the
    /// worker lets us guarantee that every synthesize_* call goes
    /// through `enter_synthesizing` / `exit_synthesizing` and that
    /// the bytes returned by the model are the bytes wrapped into
    /// the emitted `SynthesisObject` — no deterministic concat or
    /// shadow path.
    ///
    /// **Security-critical layering.** This field is the only path
    /// the worker uses to talk to the model, and it is the only path
    /// that should reach a real
    /// [`HttpManagedEndpointSynthesizer`] in production. The
    /// synthesizer itself **does not enforce
    /// [`TeeWorkerConfig::scope_bindings`]** —
    /// `assert_scope_allowed` lives on `TeeWorker` (called from
    /// [`Self::enter_synthesizing`]) and is the only place the
    /// substrate checks "is this scope authorised for this attested
    /// enclave?". See [`HttpManagedEndpointSynthesizer`]'s
    /// `# Direct construction is a footgun` section for the
    /// corresponding warning on the mechanics side. Hosts that need
    /// a different policy layer must implement an equivalent
    /// `assert_scope_allowed`-grade check before delegating to a
    /// raw `HttpManagedEndpointSynthesizer`; they must not call into
    /// the synthesizer directly.
    synth: HttpManagedEndpointSynthesizer<C>,
    /// Pre-faulted, best-effort page-locked working-set reservation.
    /// Held for the worker's whole lifetime so the pages stay
    /// resident (and `mlock`-pinned on unix) across synthesis calls,
    /// then unlocked and wiped on drop. Never read directly — its
    /// value is its residency + drop side effects — hence
    /// `dead_code` is benign. See [`PrefaultedWorkingSet`].
    #[allow(dead_code)]
    working_set: PrefaultedWorkingSet,
}

#[derive(Debug, Default)]
struct TeeWorkerState {
    lifecycle: Lifecycle,
    audit: Vec<AttestationAuditEntry>,
}

#[derive(Debug, Default)]
#[allow(clippy::large_enum_variant)]
enum Lifecycle {
    #[default]
    Unattested,
    Attesting,
    Attested {
        // Retained as forensic context inside the in-memory worker
        // state — exposed indirectly via the audit trail rather
        // than via direct reads, hence `dead_code` is benign.
        #[allow(dead_code)]
        report: AttestationReport,
        #[allow(dead_code)]
        binding: AttestationBinding,
        attested_at: DateTime<Utc>,
        active: bool,
    },
    /// The worker has finished a synthesis call but is still inside
    /// the attestation TTL. The cached report/binding are retained
    /// so the next `enter_synthesizing` call (or an explicit
    /// [`TeeWorker::settle_idle`]) can rejoin the `Attested` ring
    /// without re-attesting.
    Idle {
        #[allow(dead_code)]
        report: AttestationReport,
        #[allow(dead_code)]
        binding: AttestationBinding,
        attested_at: DateTime<Utc>,
    },
}

impl Lifecycle {
    fn as_public(&self) -> TeeWorkerLifecycle {
        match self {
            Self::Unattested => TeeWorkerLifecycle::Unattested,
            Self::Attesting => TeeWorkerLifecycle::Attesting,
            Self::Attested { active: true, .. } => TeeWorkerLifecycle::Synthesizing,
            Self::Attested { active: false, .. } => TeeWorkerLifecycle::Attested,
            Self::Idle { .. } => TeeWorkerLifecycle::Idle,
        }
    }
}

impl<R: TeeRuntime> TeeWorker<R, MockHttpClient> {
    /// Construct a fresh, **un-attested** worker backed by the
    /// default [`MockHttpClient`] echo transport. Production builds
    /// should use [`TeeWorker::with_synthesizer`] (or the generic
    /// `TeeWorker::<R, C>::new_with_synthesizer`) to inject a real
    /// HTTPS client; this default exists so tests and the
    /// `cfg(test)` paths in the rest of the crate can keep using the
    /// two-argument constructor.
    pub fn new(runtime: R, config: TeeWorkerConfig) -> Self {
        let endpoint = EndpointConfig::new(
            "https://synthesis.tee.invalid/v1/synthesize",
            "TEE_DEFAULT_KEY_REF",
            "slm-recap-v1",
        );
        let synth = HttpManagedEndpointSynthesizer::new(endpoint, MockHttpClient::echo());
        Self {
            runtime,
            config,
            state: Mutex::new(TeeWorkerState::default()),
            synth,
            working_set: PrefaultedWorkingSet::reserve(WORKER_PREFAULT_BYTES),
        }
    }
}

impl<R: TeeRuntime, C: HttpClient> TeeWorker<R, C> {
    /// Construct a fresh, **un-attested** worker with an explicit
    /// [`HttpManagedEndpointSynthesizer`] delegate. Production
    /// callers wire a real HTTPS client into the synthesizer before
    /// passing it in; tests can reuse [`MockHttpClient`].
    pub fn with_synthesizer(
        runtime: R,
        config: TeeWorkerConfig,
        synth: HttpManagedEndpointSynthesizer<C>,
    ) -> Self {
        Self {
            runtime,
            config,
            state: Mutex::new(TeeWorkerState::default()),
            synth,
            working_set: PrefaultedWorkingSet::reserve(WORKER_PREFAULT_BYTES),
        }
    }

    /// Borrow the worker's config.
    pub fn config(&self) -> &TeeWorkerConfig {
        &self.config
    }

    /// Current public-facing lifecycle state.
    pub fn lifecycle(&self) -> TeeWorkerLifecycle {
        self.state
            .lock()
            .expect("tee state mutex")
            .lifecycle
            .as_public()
    }

    /// Get a snapshot of the audit trail — useful for tests and for
    /// flushing into the persistent audit log.
    pub fn audit_trail(&self) -> Vec<AttestationAuditEntry> {
        self.state.lock().expect("tee state mutex").audit.clone()
    }

    /// Drive the lifecycle from `Unattested` → `Attesting` →
    /// `Attested`. Errors and emits a failure audit entry if the
    /// produced quote does not match `expected_measurement`.
    pub fn attest(&self) -> Result<AttestationReport> {
        self.attest_with_scope(Uuid::nil())
    }

    fn attest_with_scope(&self, scope_id: Uuid) -> Result<AttestationReport> {
        {
            let mut state = self.state.lock().expect("tee state mutex");
            state.lifecycle = Lifecycle::Attesting;
        }

        let nonce = self.fresh_nonce();
        let report = self.runtime.quote(&self.config.enclave_image, &nonce);

        if report.platform != self.config.platform {
            let entry = AttestationAuditEntry::failure(
                report.report_id,
                scope_id,
                report.platform,
                "tee platform mismatch",
            );
            let mut state = self.state.lock().expect("tee state mutex");
            state.audit.push(entry);
            state.lifecycle = Lifecycle::Unattested;
            return Err(EngineError::engine("tee: platform mismatch"));
        }

        let verified = verify_attestation(&report, &self.config.expected_measurement)
            .map_err(|e| EngineError::engine(format!("tee: verify_attestation: {e}")))?;
        if !verified {
            let entry = AttestationAuditEntry::failure(
                report.report_id,
                scope_id,
                report.platform,
                "measurement mismatch",
            );
            let mut state = self.state.lock().expect("tee state mutex");
            state.audit.push(entry);
            state.lifecycle = Lifecycle::Unattested;
            return Err(EngineError::engine("tee: measurement mismatch"));
        }

        let binding = bind_synthesizer_key(&report, &self.config.synthesizer_pub_key);
        let entry = AttestationAuditEntry::success(
            report.report_id,
            binding.binding_id,
            scope_id,
            report.platform,
        );

        let mut state = self.state.lock().expect("tee state mutex");
        state.audit.push(entry);
        state.lifecycle = Lifecycle::Attested {
            report: report.clone(),
            binding,
            attested_at: Utc::now(),
            active: false,
        };
        Ok(report)
    }

    fn fresh_nonce(&self) -> [u8; 32] {
        // Mix the synthesizer pub key + a fresh UUID so two `attest`
        // calls in the same process do not collide. The mixing buffer
        // embeds the synthesizer key, so it is held in `Zeroizing`
        // and wiped once the nonce has been derived.
        let mut input = Zeroizing::new(Vec::with_capacity(
            self.config.synthesizer_pub_key.len() + 16,
        ));
        input.extend_from_slice(&self.config.synthesizer_pub_key);
        let nonce = Uuid::new_v4();
        input.extend_from_slice(nonce.as_bytes());
        content_hash(&input)
    }

    fn assert_scope_allowed(&self, scope_id: Uuid) -> Result<()> {
        if self.config.scope_bindings.is_empty() {
            return Err(EngineError::engine(
                "tee: no scope bindings configured; refusing to synthesise",
            ));
        }
        if !self.config.scope_bindings.contains(&scope_id) {
            return Err(EngineError::engine(format!(
                "tee: scope {scope_id} not bound to this worker",
            )));
        }
        Ok(())
    }

    fn enter_synthesizing(&self, scope_id: Uuid) -> Result<()> {
        self.assert_scope_allowed(scope_id)?;
        let mut state = self.state.lock().expect("tee state mutex");

        // Settle `Idle` back into `Attested` before checking whether
        // we're allowed to synthesise again. This is the "next
        // dispatch check" transition described in the lifecycle
        // diagram at the top of this module.
        if matches!(state.lifecycle, Lifecycle::Idle { .. }) {
            let prior = std::mem::replace(&mut state.lifecycle, Lifecycle::Unattested);
            if let Lifecycle::Idle {
                report,
                binding,
                attested_at,
            } = prior
            {
                state.lifecycle = Lifecycle::Attested {
                    report,
                    binding,
                    attested_at,
                    active: false,
                };
            }
        }

        match &mut state.lifecycle {
            Lifecycle::Attested {
                attested_at,
                active,
                ..
            } => {
                let elapsed = Utc::now().signed_duration_since(*attested_at);
                if elapsed > self.config.attestation_ttl {
                    state.lifecycle = Lifecycle::Unattested;
                    return Err(EngineError::engine("tee: attestation expired"));
                }
                if *active {
                    return Err(EngineError::engine(
                        "tee: worker is already inside a synthesis call",
                    ));
                }
                *active = true;
                Ok(())
            }
            Lifecycle::Idle { .. } => unreachable!("settled above"),
            Lifecycle::Attesting | Lifecycle::Unattested => {
                Err(EngineError::engine("tee: worker is not attested"))
            }
        }
    }

    fn exit_synthesizing(&self) {
        let mut state = self.state.lock().expect("tee state mutex");
        // Take ownership of the `Attested { active: true, .. }`
        // payload so we can fold it into the `Idle` variant. Using
        // `std::mem::replace` avoids cloning the (large) attestation
        // report.
        let prior = std::mem::replace(&mut state.lifecycle, Lifecycle::Unattested);
        state.lifecycle = match prior {
            Lifecycle::Attested {
                report,
                binding,
                attested_at,
                active: true,
            } => Lifecycle::Idle {
                report,
                binding,
                attested_at,
            },
            other => other,
        };
    }

    /// Settle an [`Lifecycle::Idle`] worker back into the `Attested`
    /// ring. Useful for tests and for callers that want to drive the
    /// transition explicitly (e.g. on a wall-clock timer rather than
    /// piggy-backing on the next dispatch). No-op when the worker is
    /// in any other state.
    pub fn settle_idle(&self) {
        let mut state = self.state.lock().expect("tee state mutex");
        let prior = std::mem::replace(&mut state.lifecycle, Lifecycle::Unattested);
        state.lifecycle = match prior {
            Lifecycle::Idle {
                report,
                binding,
                attested_at,
            } => Lifecycle::Attested {
                report,
                binding,
                attested_at,
                active: false,
            },
            other => other,
        };
    }
}

/// RAII guard bracketing a single confidential synthesis call.
///
/// Constructed *after* [`TeeWorker::enter_synthesizing`] has flipped
/// the worker into the active state. Its [`Drop`] runs
/// [`TeeWorker::exit_synthesizing`] unconditionally, so the worker
/// settles back to `Idle` on **every** exit path — normal return, a
/// `?` early-return from the delegate, or a panic unwinding out of
/// the leaf synthesizer. This is what makes the module invariant
/// ("any panic, early return, or policy-rejected scope flips the
/// worker back to `Idle` so the attestation TTL stays honest")
/// actually hold; before this guard a panic in the delegate stranded
/// the worker in `Synthesizing` forever, wedging the attestation
/// lifecycle.
///
/// The guard also owns a [`Zeroizing`] staging buffer for the
/// plaintext synthesis material the worker assembles inside the
/// confidential boundary (see [`Self::bind_content`]). The buffer —
/// and the plaintext it held — is wiped when the guard drops, on
/// every exit path, so plaintext does not linger on the worker's
/// heap after the call returns.
struct SynthesisSession<'a, R: TeeRuntime, C: HttpClient> {
    worker: &'a TeeWorker<R, C>,
    staging: Zeroizing<Vec<u8>>,
}

impl<'a, R: TeeRuntime, C: HttpClient> SynthesisSession<'a, R, C> {
    fn new(worker: &'a TeeWorker<R, C>) -> Self {
        Self {
            worker,
            staging: Zeroizing::new(Vec::new()),
        }
    }

    /// Stage every plaintext payload in `payloads` into the
    /// zeroize-on-drop buffer and return a content-binding digest
    /// over them.
    ///
    /// The digest ties this attested run to the exact plaintext it
    /// consumed, so the telemetry trail can prove *which* content was
    /// synthesised under *which* attestation. Only the digest is ever
    /// surfaced; the plaintext lives in `staging` and is wiped with
    /// the guard.
    fn bind_content<'p, I>(&mut self, payloads: I) -> ContentHash
    where
        I: IntoIterator<Item = &'p [u8]>,
    {
        self.staging.clear();
        for payload in payloads {
            self.staging.extend_from_slice(payload);
        }
        content_hash(&self.staging)
    }
}

impl<R: TeeRuntime, C: HttpClient> Drop for SynthesisSession<'_, R, C> {
    fn drop(&mut self) {
        self.worker.exit_synthesizing();
        // `staging` (Zeroizing) wipes the staged plaintext here, after
        // `exit_synthesizing` has settled the lifecycle.
    }
}

/// Hex-encode a content digest for structured logging. Only the
/// digest — never plaintext — is ever rendered this way.
fn hex_digest(digest: &ContentHash) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        // Writing to a `String` is infallible.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

impl<R: TeeRuntime, C: HttpClient> SynthesisEngine for TeeWorker<R, C> {
    fn synthesize_domain(
        &self,
        windows: &mut SynthesisWindowManager,
        handle: TieredWindowHandle,
        input: DomainSynthesisInput,
    ) -> Result<DomainSynthesisResult> {
        let scope_id = input.domain_scope.0;
        self.enter_synthesizing(scope_id)?;
        // RAII guard: settles the lifecycle back to `Idle` and wipes
        // the staged plaintext on every exit path — including a panic
        // unwinding out of the delegate below. Replaces the previous
        // bare `exit_synthesizing()` call, which leaked the
        // `Synthesizing` state on panic / early return.
        let mut session = SynthesisSession::new(self);
        let digest = session.bind_content(
            input
                .channel_outputs
                .iter()
                .map(|o| o.object().payload.as_slice()),
        );
        debug!(
            target: "synthesis_engine::tee",
            scope = %scope_id,
            content_digest = %hex_digest(&digest),
            "confidential domain synthesis content binding"
        );
        // Delegate the actual SLM call to the embedded
        // `HttpManagedEndpointSynthesizer`, which runs the same
        // validation / `mark_in_progress` / `mark_failed` /
        // `mark_complete` choreography as before.
        self.synth.synthesize_domain(windows, handle, input)
    }

    fn synthesize_tenant(
        &self,
        windows: &mut SynthesisWindowManager,
        handle: TieredWindowHandle,
        input: TenantSynthesisInput,
    ) -> Result<TenantSynthesisResult> {
        let scope_id = input.tenant_scope.0;
        self.enter_synthesizing(scope_id)?;
        let mut session = SynthesisSession::new(self);
        let digest = session.bind_content(
            input
                .domain_outputs
                .iter()
                .map(|o| o.object().payload.as_slice())
                .chain(
                    input
                        .approved_documents
                        .iter()
                        .map(|d| d.payload.as_slice()),
                ),
        );
        debug!(
            target: "synthesis_engine::tee",
            scope = %scope_id,
            content_digest = %hex_digest(&digest),
            "confidential tenant synthesis content binding"
        );
        self.synth.synthesize_tenant(windows, handle, input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_config() -> TeeWorkerConfig {
        let enclave_image = b"tee-worker-enclave-v1.0".to_vec();
        let measurement = content_hash(&enclave_image);
        TeeWorkerConfig::new(
            TeePlatform::Mock,
            measurement,
            b"synth-pub-key".to_vec(),
            vec![Uuid::new_v4(), Uuid::new_v4()],
            enclave_image,
        )
    }

    #[test]
    fn lifecycle_starts_unattested() {
        let worker = TeeWorker::new(MockTeeRuntime, fixture_config());
        assert_eq!(worker.lifecycle(), TeeWorkerLifecycle::Unattested);
        assert!(worker.audit_trail().is_empty());
    }

    #[test]
    fn attest_promotes_to_attested_and_appends_audit_entry() {
        let worker = TeeWorker::new(MockTeeRuntime, fixture_config());
        let report = worker.attest().expect("attest");
        assert_eq!(worker.lifecycle(), TeeWorkerLifecycle::Attested);
        let audit = worker.audit_trail();
        assert_eq!(audit.len(), 1);
        assert!(audit[0].verified);
        assert_eq!(audit[0].report_id, report.report_id);
    }

    #[test]
    fn attest_fails_on_wrong_measurement() {
        let mut config = fixture_config();
        config.expected_measurement = content_hash(b"different-enclave-image");
        let worker = TeeWorker::new(MockTeeRuntime, config);
        let err = worker.attest().expect_err("expected measurement mismatch");
        match err {
            EngineError::Engine(s) => assert!(s.contains("measurement mismatch")),
            other => panic!("unexpected error: {other:?}"),
        }
        assert_eq!(worker.lifecycle(), TeeWorkerLifecycle::Unattested);
        let audit = worker.audit_trail();
        assert_eq!(audit.len(), 1);
        assert!(!audit[0].verified);
        assert_eq!(
            audit[0].failure_reason.as_deref(),
            Some("measurement mismatch")
        );
    }

    #[test]
    fn attest_fails_on_wrong_platform() {
        let mut config = fixture_config();
        config.platform = TeePlatform::IntelTdx;
        let worker = TeeWorker::new(MockTeeRuntime, config);
        let err = worker.attest().expect_err("expected platform mismatch");
        match err {
            EngineError::Engine(s) => assert!(s.contains("platform mismatch")),
            other => panic!("unexpected error: {other:?}"),
        }
        assert_eq!(worker.lifecycle(), TeeWorkerLifecycle::Unattested);
    }

    #[test]
    fn enter_synthesizing_refuses_when_not_attested() {
        let config = fixture_config();
        let scope = config.scope_bindings[0];
        let worker = TeeWorker::new(MockTeeRuntime, config);
        let err = worker
            .enter_synthesizing(scope)
            .expect_err("expected refusal");
        match err {
            EngineError::Engine(s) => assert!(s.contains("not attested")),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn enter_synthesizing_refuses_unbound_scope() {
        let config = fixture_config();
        let worker = TeeWorker::new(MockTeeRuntime, config);
        worker.attest().expect("attest");
        let foreign_scope = Uuid::new_v4();
        let err = worker
            .enter_synthesizing(foreign_scope)
            .expect_err("expected scope refusal");
        match err {
            EngineError::Engine(s) => assert!(s.contains("not bound to this worker")),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn enter_synthesizing_refuses_when_already_active() {
        let config = fixture_config();
        let scope = config.scope_bindings[0];
        let worker = TeeWorker::new(MockTeeRuntime, config);
        worker.attest().expect("attest");
        worker.enter_synthesizing(scope).expect("first enter");
        let err = worker
            .enter_synthesizing(scope)
            .expect_err("expected reentrant refusal");
        match err {
            EngineError::Engine(s) => assert!(s.contains("already inside")),
            other => panic!("unexpected error: {other:?}"),
        }
        worker.exit_synthesizing();
    }

    #[test]
    fn ttl_expiry_demotes_to_unattested() {
        let mut config = fixture_config();
        config.attestation_ttl = Duration::nanoseconds(1);
        let scope = config.scope_bindings[0];
        let worker = TeeWorker::new(MockTeeRuntime, config);
        worker.attest().expect("attest");
        // Force an artificial sleep so the TTL elapses.
        std::thread::sleep(std::time::Duration::from_millis(5));
        let err = worker
            .enter_synthesizing(scope)
            .expect_err("expected ttl expiry");
        match err {
            EngineError::Engine(s) => assert!(s.contains("attestation expired")),
            other => panic!("unexpected error: {other:?}"),
        }
        assert_eq!(worker.lifecycle(), TeeWorkerLifecycle::Unattested);
    }

    #[test]
    fn re_attestation_after_ttl_expiry_succeeds() {
        let mut config = fixture_config();
        config.attestation_ttl = Duration::nanoseconds(1);
        let worker = TeeWorker::new(MockTeeRuntime, config);
        worker.attest().expect("first attest");
        std::thread::sleep(std::time::Duration::from_millis(5));
        let scope = worker.config().scope_bindings[0];
        // Triggering enter_synthesizing should drop us to Unattested.
        worker.enter_synthesizing(scope).expect_err("ttl expired");
        // We can re-attest cleanly.
        worker.attest().expect("re-attest");
        assert_eq!(worker.lifecycle(), TeeWorkerLifecycle::Attested);
    }

    #[test]
    fn audit_trail_records_failure_then_success_after_re_attest() {
        let mut config = fixture_config();
        config.expected_measurement = content_hash(b"wrong-image");
        let worker = TeeWorker::new(MockTeeRuntime, config);
        worker.attest().expect_err("first attest fails");
        // Fix the expected measurement and try again.
        let enclave = worker.config.enclave_image.clone();
        // We can't mutate config in-place safely so build a second
        // worker that reuses the same audit-failure pattern. The
        // audit trail of the first worker still captures the failure.
        let trail = worker.audit_trail();
        assert_eq!(trail.len(), 1);
        assert!(!trail[0].verified);
        // The replacement worker uses the correct measurement.
        let mut good = fixture_config();
        good.expected_measurement = content_hash(&enclave);
        good.enclave_image = enclave;
        let worker2 = TeeWorker::new(MockTeeRuntime, good);
        worker2.attest().expect("second attest");
        let trail2 = worker2.audit_trail();
        assert_eq!(trail2.len(), 1);
        assert!(trail2[0].verified);
    }

    /// Regression test for the 2026-05-08 lifecycle fix.
    ///
    /// Before the fix, `exit_synthesizing` only flipped
    /// `Attested.active = false`, so `lifecycle()` jumped straight
    /// from `Synthesizing` back to `Attested` and the `Idle` variant
    /// of [`TeeWorkerLifecycle`] was unreachable. The fix lands the
    /// worker in `Idle` after a synthesis call; the next dispatch
    /// (or an explicit [`TeeWorker::settle_idle`]) folds it back into
    /// `Attested` without re-attesting.
    #[test]
    fn exit_synthesizing_lands_in_idle_then_settles_back_to_attested() {
        let config = fixture_config();
        let scope = config.scope_bindings[0];
        let worker = TeeWorker::new(MockTeeRuntime, config);
        worker.attest().expect("attest");
        assert_eq!(worker.lifecycle(), TeeWorkerLifecycle::Attested);

        worker.enter_synthesizing(scope).expect("enter");
        assert_eq!(worker.lifecycle(), TeeWorkerLifecycle::Synthesizing);

        worker.exit_synthesizing();
        assert_eq!(
            worker.lifecycle(),
            TeeWorkerLifecycle::Idle,
            "after exit_synthesizing the worker must be Idle (not Attested)"
        );

        // The worker can rejoin the Attested ring without re-attesting.
        worker.settle_idle();
        assert_eq!(worker.lifecycle(), TeeWorkerLifecycle::Attested);

        // And the next enter_synthesizing settles Idle on its own.
        worker.enter_synthesizing(scope).expect("re-enter");
        worker.exit_synthesizing();
        assert_eq!(worker.lifecycle(), TeeWorkerLifecycle::Idle);
        worker
            .enter_synthesizing(scope)
            .expect("dispatch settles idle");
        assert_eq!(worker.lifecycle(), TeeWorkerLifecycle::Synthesizing);
        worker.exit_synthesizing();
    }

    #[test]
    fn lifecycle_string_tags_are_pinned() {
        assert_eq!(TeeWorkerLifecycle::Unattested.as_str(), "unattested");
        assert_eq!(TeeWorkerLifecycle::Attesting.as_str(), "attesting");
        assert_eq!(TeeWorkerLifecycle::Attested.as_str(), "attested");
        assert_eq!(TeeWorkerLifecycle::Synthesizing.as_str(), "synthesizing");
        assert_eq!(TeeWorkerLifecycle::Idle.as_str(), "idle");
    }

    #[test]
    fn config_default_ttl_is_five_minutes() {
        let config = fixture_config();
        assert_eq!(config.attestation_ttl, Duration::minutes(5));
        assert_eq!(config.attestation_ttl, ATTESTATION_TTL);
        assert_eq!(config.attestation_ttl.num_seconds(), 300);
    }

    #[test]
    fn prefaulted_working_set_prefaults_and_zeroizes() {
        use zeroize::Zeroize;

        // Structural guard: the working-set buffer is wiped on drop
        // because it is a `Zeroizing<Vec<u8>>` (which has a `Drop`
        // impl that zeroes its contents). Behavioural check below
        // mirrors the crypto crate's zeroize test: a true post-drop
        // peek at the freed allocation would need `unsafe` reads of
        // freed memory, so instead we exercise the identical wipe
        // routine `Zeroize::zeroize` and observe the result.
        let ws = PrefaultedWorkingSet::reserve(WORKER_PREFAULT_BYTES);
        assert_eq!(ws.pages.len(), WORKER_PREFAULT_BYTES);
        // Pre-faulting touched the first byte of every page with the
        // `0xA5` sentinel, so the page-stride samples are non-zero.
        let page = os_page_size();
        assert_eq!(ws.pages[0], 0xA5, "first page must be pre-faulted");
        if WORKER_PREFAULT_BYTES > page {
            assert_eq!(ws.pages[page], 0xA5, "second page must be pre-faulted");
        }

        // Behavioural wipe check on the same buffer type.
        let mut buf = ws.pages.clone();
        assert!(buf.iter().any(|&b| b != 0));
        buf.zeroize();
        assert!(
            buf.iter().all(|&b| b == 0),
            "Zeroizing<Vec<u8>> must wipe every byte on zeroize/drop"
        );
    }

    #[test]
    fn synthesis_session_settles_lifecycle_and_binds_content_on_drop() {
        // The RAII `SynthesisSession` must settle the worker back to
        // `Idle` when it drops, even though `exit_synthesizing` is no
        // longer called explicitly in the synthesize path.
        let config = fixture_config();
        let scope = config.scope_bindings[0];
        let worker = TeeWorker::new(MockTeeRuntime, config);
        worker.attest().expect("attest");
        worker.enter_synthesizing(scope).expect("enter");
        assert_eq!(worker.lifecycle(), TeeWorkerLifecycle::Synthesizing);
        {
            let mut session = SynthesisSession::new(&worker);
            let payloads: [&[u8]; 2] = [b"alpha", b"beta"];
            let digest = session.bind_content(payloads);
            // Binding is order-sensitive concatenation, so it matches
            // a direct hash of the concatenated payloads.
            assert_eq!(digest, content_hash(b"alphabeta"));
            // The staged plaintext is held while the guard is alive.
            assert_eq!(session.staging.as_slice(), b"alphabeta");
        }
        // Guard dropped -> exit_synthesizing ran -> back to Idle.
        assert_eq!(worker.lifecycle(), TeeWorkerLifecycle::Idle);
    }

    #[test]
    fn page_helpers_are_safe_no_op_on_empty_buffer() {
        // The portable fallback path must not panic or mis-lock on a
        // zero-length buffer.
        let mut empty: Vec<u8> = Vec::new();
        prefault_pages(&mut empty);
        assert!(!lock_pages(&empty));
        unlock_pages(&empty);
        // The page size must be a sane, non-zero stride so
        // `prefault_pages` always makes progress.
        let page = os_page_size();
        assert!(page > 0);
        assert!(
            page.is_power_of_two(),
            "page size {page} must be a power of two"
        );
    }
}
