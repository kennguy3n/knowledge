# TEE synthesis-worker side-channel posture

This document describes the side-channel threat model for the
TEE-backed synthesis worker (`crates/synthesis_engine/src/tee_worker.rs`),
the mitigations now in place, and the residual risks that remain out of
scope. It complements the substrate [threat model](threat-model.md) and
the [crypto design](../technical/crypto-spec.md).

The worker runs synthesis inside a confidential-compute boundary
(AWS Nitro Enclaves / equivalent TEE). It attests its enclave
measurement, binds the synthesizer's public key to the attestation
report, and only then performs synthesis over decrypted, scope-bound
content. Because synthesis touches **plaintext** and **key-derived
material**, the worker is the substrate component most exposed to
microarchitectural and key-exposure side channels.

## Threat

An adversary co-resident on the host (a malicious hypervisor is out of
scope for a TEE; a co-tenant process, a compromised host daemon, or a
remote attacker able to time requests is in scope) may try to recover
plaintext or keys without breaking the enclave boundary directly:

- **Timing side channels.** Data-dependent branches or memory accesses
  during synthesis or tag/MAC verification leak secret bits through
  observable latency. The canonical case is a byte-by-byte `==` on an
  authentication tag, which lets an attacker forge a tag one byte at a
  time by measuring rejection latency.
- **Page-fault side channels.** A controlling host can unmap enclave
  pages and observe the fault sequence as the worker touches memory,
  reconstructing access patterns (and thus secret-dependent control
  flow / indices) from the order and timing of first-touch faults.
- **Cache side channels.** Shared-cache eviction timing (Prime+Probe /
  Flush+Reload) can recover secret-dependent access patterns.
- **Key exposure over time.** A leaked or replayed attestation report,
  or plaintext / key-derived intermediates left resident on the heap
  after a synthesis call, widens the window in which a memory-disclosure
  bug elsewhere can harvest secrets.

## Mitigations in place

### 1. Short attestation TTL (5 minutes)

`ATTESTATION_TTL` is **5 minutes (300 s)** and is the default
`TeeWorkerConfig::attestation_ttl`. A cached attestation older than the
TTL demotes the worker to `Unattested` and forces re-attestation before
the next synthesis call. This shrinks the replay window for a leaked or
stolen report from the previous one-hour default to five minutes, and
bounds how long a key binding is trusted without a fresh enclave
measurement.

### 2. Zeroize-on-drop for synthesis intermediates

Every plaintext / key-derived intermediate the worker materialises
inside the confidential boundary is wrapped in `zeroize::Zeroizing`, so
its backing memory is wiped when it drops — on the normal path **and**
on panic. This matches the wipe discipline `crates/crypto` already
applies to long-lived secret-key state (`#[zeroize(drop)]` on
`HybridSecretKey`). Specifically:

- **Nonce-derivation input** (`TeeWorker::fresh_nonce`) — the buffer
  mixing the synthesizer public key with fresh randomness is
  `Zeroizing`, so the pre-hash material never lingers.
- **Plaintext staging** (`SynthesisSession`) — the input payloads are
  streamed for content binding through the worker's pre-faulted, pinned
  working set (see §3), which is itself `Zeroizing`. Each payload is
  copied into the pinned pages in reservation-sized chunks and folded
  into a streaming digest, and every chunk is wiped before the next one
  reuses the buffer — so no plaintext lingers in the reservation while
  the call runs. A final wipe on the `SynthesisSession` guard's drop
  covers every exit path, including a panic unwinding out of the leaf
  synthesizer.

The `SynthesisSession` guard also closes a lifecycle hole: its `Drop`
runs `exit_synthesizing` unconditionally, so a panic or early return in
the delegate can no longer strand the worker in the `Synthesizing`
state (which would have wedged the attestation lifecycle and kept the
TTL clock from being honoured).

### 3. Enclave page pre-faulting and pinning

On construction, each worker reserves a `WORKER_PREFAULT_BYTES`
(64 KiB) working set (`PrefaultedWorkingSet`) that also backs each
synthesis call's plaintext staging (see §2), so the content the worker
binds lands in already-resident, pinned pages rather than a
freshly-faulted per-call heap buffer. On construction the worker:

- **Pre-faults** it by touching one byte on every spanned page, so the
  OS commits and faults the pages in eagerly, up front — rather than
  during a later synthesis call where the per-page first-touch fault
  latency would leak access patterns.
- **Pins** it with `mlock(2)` so the pages stay resident and are not
  swapped out (and re-faulted) under memory pressure.

The reservation is held for the worker's whole lifetime, lent to the
single in-flight synthesis call (the `enter_synthesizing` lifecycle
admits only one at a time), and is wiped (it is `Zeroizing`) and
`munlock`-ed on drop. Page locking is
`cfg(unix)`-guarded; on non-unix targets the lock/unlock helpers are
safe no-ops, and `mlock` failure (e.g. an exhausted `RLIMIT_MEMLOCK`,
or an unprivileged container) is treated as best-effort — the worker
still runs, just without the swap-residency guarantee — so the mock /
non-enclave path keeps building and the test suite keeps passing.

### 4. Constant-time comparisons

Security-sensitive equality never short-circuits on secret bytes. The
worker and the crypto crate route such comparisons through primitives
that are constant-time by construction:

- **Authentication-tag verification** — AEAD decryption
  (`crypto::decrypt_aead`, XChaCha20-Poly1305) verifies the Poly1305
  tag in constant time and returns a unit error with no positional
  information, rather than comparing tags with `==`.
- **ML-KEM-768 implicit rejection** — a wrong decapsulation key yields
  a pseudo-random shared secret instead of an `==`-style early abort.

These are audited in `crates/crypto/tests/security_hardening.rs`
(section 5, "Constant-time comparison audit"), which asserts both that
flipping *any* single tag bit is rejected (whole-tag coverage, no
prefix short-circuit) and that rejection time does not depend on which
tag byte differs (position-independent timing, using the robust
best-of-N median estimator shared with the AEAD timing test).

## Residual risks (out of scope)

These mitigations reduce, but do not eliminate, side-channel exposure.
The following remain out of scope for the substrate's own guarantees
and are inherited from the platform / underlying libraries:

- **Microarchitectural data-sampling** (e.g. MDS / transient-execution
  attacks) is a CPU/firmware concern; the substrate relies on the TEE
  platform and host microcode for mitigation.
- **Cache-timing leakage inside the cryptographic primitives**
  themselves (ChaCha20, Poly1305, X25519, ML-KEM/ML-DSA) is the
  responsibility of the vetted upstream crates; the substrate does not
  re-implement these primitives.
- **`mlock` not guaranteed.** When `RLIMIT_MEMLOCK` is exhausted or the
  process is unprivileged, pinning degrades to a no-op and the working
  set may be swapped; pre-faulting still applies.
- **Reservation size is a throughput trade-off, not a security limit.**
  Payloads larger than the 64 KiB reservation are *streamed* through the
  pinned, pre-faulted pages in reservation-sized chunks (each wiped
  before the next reuses the buffer), so no plaintext is hashed from an
  unpinned buffer regardless of size. A smaller reservation only means
  more chunk iterations, not a loss of the residency/pinning guarantee.
- **A compromised running process** that already has code execution in
  the enclave can read intermediates while they are live; zeroize-on-drop
  shrinks but does not close this window (see the
  [threat model](threat-model.md) non-goals).

## Further reading

- [threat-model.md](threat-model.md) — substrate-wide threat model.
- [../../SECURITY.md](../../SECURITY.md) — security policy summary.
- [../technical/crypto-spec.md](../technical/crypto-spec.md) —
  cryptographic primitives and key hierarchy.
