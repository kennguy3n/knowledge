# Post-Quantum Threat-Model Whitepaper

**Status:** code-grounded, pre-audit. Every load-bearing claim below is
cited to the implementing source; where the code does **not** yet do
something, the gap is named explicitly rather than implied. This
document is intended to be readable by a procurement reviewer or a
third-party cryptographic auditor, not only by maintainers.

**Companion documents:** this whitepaper is the procurement-grade
expansion of the [cryptographic specification](../technical/crypto-spec.md)
and the [substrate threat model](threat-model.md). For key custody see
[key-management.md](key-management.md); for rotation see
[key-rotation.md](key-rotation.md); for the confidential-compute
side-channel posture see [tee-side-channels.md](tee-side-channels.md);
for the control-by-control regulatory mapping that this document's
§5 builds on, see [operator/compliance.md](../operator/compliance.md).

---

## 0. Scope and honest-claims contract

This whitepaper covers the cryptography implemented in the
`knowledge_crypto` crate (`crates/crypto`, ~8.9k LOC) and the way the
rest of the substrate consumes it. It deliberately separates three
states that procurement reviews routinely conflate:

1. **Primitive implemented and tested** — the algorithm exists in
   `crates/crypto`, has round-trip / tamper / KAT coverage, and is
   benchmarked.
2. **Primitive wired into a production data path** — a non-test caller
   (the FFI runtime, the Go gateway, the evidence store) actually
   invokes it on real user data.
3. **Primitive externally verified** — a third party has audited the
   implementation against the spec.

A claim that is true at level (1) is **not** automatically true at
level (2) or (3). The matrix in [§6](#6-external-review-readiness)
records the level for each primitive. The headline honest facts a
buyer must internalise:

- **Cryptographic forgetting is wired** (level 2): `ffi::forget_scope`
  → `crypto::forgetting::destroy_scope_dek` runs on real scopes.
- **The hybrid post-quantum KEM is implemented and tested** (level 1)
  and exposed via `POST /crypto/hybrid_keypair`, **but its intended
  production consumer — multi-device sync key transport — is a Phase-2
  stub** (`crates/sync_engine` has the CRDT merge but no network
  transport). See [§2.4](#24-honest-gap-what-the-kem-does-not-yet-protect).
- **TEE attestation is mock-only today** (level 1): real Intel TDX /
  AMD SEV-SNP / Nitro quote verification is feature-flagged future
  work. See [§4.3](#43-confidential-compute-attestation).
- **No external cryptographic audit has been completed** (level 3 is
  empty). See [SECURITY.md → Audit status](../../SECURITY.md#audit-status).

---

## 1. Primitive inventory and parameter sets (as implemented)

Every entry is the algorithm and parameter set **actually compiled
into the substrate**, with the source module and the wire-size
constants the code pins. Sizes are taken from the constants in
`crates/crypto`, not from the specifications, so they reflect what the
code enforces at runtime.

| Purpose | Primitive / parameter set | Backing crate | Source | Pinned sizes |
|---|---|---|---|---|
| Content hashing | **BLAKE3** (256-bit) | `blake3` | `crypto::hash` (`hash.rs`) | 32-byte digest (`CONTENT_HASH_LEN`) |
| Symmetric AEAD | **XChaCha20-Poly1305** | `chacha20poly1305` | `crypto::aead` (`aead.rs`) | 32-byte key (`AEAD_KEY_LEN`), 24-byte nonce (`AEAD_NONCE_LEN`), 16-byte tag |
| Key derivation | **HKDF-SHA256** | `hkdf` + `sha2` | `crypto::kdf` (`kdf.rs`) | 32-byte master key (`MASTER_KEY_LEN`); salt `knowledge-substrate-v1` |
| KEM (classical half) | **X25519** | `x25519-dalek` v2 | `crypto::hybrid_kem` | 32-byte pk/sk/shared |
| KEM (PQ half) | **ML-KEM-768** (FIPS 203) | `ml-kem` (RustCrypto) | `crypto::kem` (`kem.rs`) | pk 1184 B, sk 2400 B, ct 1088 B, ss 32 B |
| Hybrid KEM combiner | **concatenate-then-HKDF-SHA256** | — | `crypto::hybrid_kem` | 32-byte combined secret |
| Signatures (primary) | **ML-DSA-65** (FIPS 204) | `ml-dsa` (RustCrypto) | `crypto::signer_backend` | signature 3309 B (`ML_DSA_65_SIGNATURE_LEN`), 32-byte signing seed |
| Signatures (archival co-sign) | **SPHINCS+-SHAKE-128f-simple** | `pqcrypto-sphincsplus` (PQClean) | `crypto::sphincs` | signature 17,088 B, pk 32 B, sk 64 B |

### 1.1 Hybrid KEM combiner (exact construction)

The combiner is **concatenate-then-KDF**, implemented in
`hybrid_kem.rs::combine`:

```text
ikm           = X25519_dh (32 B) || ML-KEM-768_ss (32 B)
shared_secret = HKDF-SHA256(
                    ikm  = ikm,
                    salt = "knowledge-hybrid-kem-v1",
                    info = "x25519+mlkem768",
                    L    = 32 bytes)
```

Both halves run on every encap **and** decap — there is no code path
that silently drops the post-quantum half (`hybrid_kem_encap` /
`hybrid_kem_decap` always call the configured `KemBackend`). The
concatenated `ikm` is zeroized off the stack immediately after the
HKDF expand. This construction is a standard
"dual-PRF / KDF-combiner" hybrid: the output is a secure key as long as
**either** the X25519 secret or the ML-KEM-768 secret is unknown to the
adversary, which is exactly the property required to hedge a future
quantum break of X25519 against a possible analysis flaw in the
younger lattice scheme.

> **Standards-tracking caveat (honest).** The combiner uses a fixed
> string label and the raw concatenation order `x25519 || mlkem768`; it
> is *a* sound KDF-combiner but is **not** byte-for-byte the IETF
> `hybrid` X25519MLKEM768 construction now being standardised for TLS.
> Interop with external PQ-TLS stacks is therefore **not** claimed.
> Internal interop is fine because both peers run this same code.

### 1.2 Backend abstraction (intentional swap point)

ML-KEM-768 is consumed only through the `KemBackend` trait
(`kem.rs`) and the signers only through `SignerBackend` /
`ProvenanceSigner`. The default `MlKem768Backend` is the **pure-Rust**
`ml-kem` crate. The stated intent (module docs in `kem.rs` and
`architecture.md §2.5`) is to swap in a formally-audited `liboqs`
backend later. **This swap has not happened** — today the PQ KEM and
signatures depend on the RustCrypto `ml-kem` / `ml-dsa` crates, which
are young (pre-1.0) implementations of recently-finalised standards.
That dependency risk is tracked in
[§4.2](#42-implementation-maturity-risk).

### 1.3 Memory hygiene

All long-lived secret-key state is wiped on drop:
`HybridSecretKey` is `#[zeroize(drop)]` (`hybrid_kem.rs`),
`MlDsa65Signer` secret state is `ZeroizeOnDrop` via the `ml-dsa`
`zeroize` feature, and `SphincsPlusSigner` holds its encoded secret key
in a `Zeroizing<Vec<u8>>` because the PQClean FFI type is not itself
zeroizing. This is pinned by
`crates/crypto/tests/zeroize_verification.rs` and
`security_hardening.rs`. Zeroize-on-drop shrinks but does **not**
eliminate the in-process memory-disclosure window (see
[§3.4](#34-what-cryptographic-forgetting-does-not-guarantee)).

---

## 2. Harvest-now, decrypt-later (HNDL) threat model

### 2.1 The threat

A **harvest-now, decrypt-later** adversary records ciphertext today and
stores it until a cryptographically relevant quantum computer (CRQC)
exists. Any data whose confidentiality must outlive the arrival of a
CRQC — health records (HIPAA), financial records (SOX), student records
(FERPA) — is already inside the window of concern, because the *capture*
happens now and the *decryption* happens later. The relevant question
for procurement is therefore not "is there a quantum computer today"
but "what is the confidentiality horizon of the data, and is the
key-establishment cryptography quantum-resistant **at capture time**."

### 2.2 Which substrate cryptography is HNDL-relevant

HNDL is an **asymmetric-cryptography** problem. It is critical to be
precise here, because the substrate uses two very different kinds of
cryptography and only one of them is the HNDL concern:

| Data state | Protection | HNDL exposure |
|---|---|---|
| Evidence bodies / archive at rest | XChaCha20-Poly1305 (256-bit symmetric) under a per-scope DEK, inside SQLCipher (AES-256) | **Low.** Symmetric ciphers face only Grover's quadratic speedup; a 256-bit key retains ≈128-bit post-quantum strength. Harvested at-rest ciphertext is not directly recoverable by a CRQC. |
| Key material **transferred** between devices / parties | Hybrid X25519 + ML-KEM-768 KEM, output feeds the symmetric layer | **This is the HNDL surface.** A pure-X25519 transfer recorded today would be retroactively breakable; the ML-KEM-768 half is what removes that. |

The corollary, stated honestly: **the substrate's at-rest data is not
the primary HNDL target** as long as the master key never leaves the
device's secure element (see [key-management.md](key-management.md)).
The master key is symmetric and never transmitted, so there is no
harvested asymmetric handshake protecting it. The hybrid KEM matters
specifically for the **transfer/sync** path — moving wrapped key
material between devices or to a peer — which is where an asymmetric
key establishment would otherwise be recorded on the wire.

### 2.3 How the hybrid KEM mitigates HNDL (where it runs)

The hybrid KEM derives the session/transfer secret from **both** an
X25519 ephemeral DH and an ML-KEM-768 encapsulation, combined through
HKDF-SHA256 ([§1.1](#11-hybrid-kem-combiner-exact-construction)). To
recover a harvested transfer secret an adversary must break **both**
halves. A CRQC that breaks X25519 still faces ML-KEM-768; an analyst
who finds a flaw in ML-KEM-768 still faces X25519.

**Downgrade resistance is structural today.** `hybrid_kem_encap` /
`hybrid_kem_decap` always run *both* halves — there is no classical-only
or PQ-only branch in the KEM functions themselves — so the current
implementation cannot silently drop the post-quantum half. The
protection is a property of there being only one (hybrid) code path, not
of a runtime policy check.

**A policy/audit layer exists but is not yet wired** (honest gap). The
crate also ships an operator-posture surface,
`crypto::hybrid_enforcement` (`CryptoPolicy` / `HybridMode` /
`KeyExchangeAudit`), implemented and unit-tested:

- `HybridMode::HybridTransition` — the intended **production default**
  (`CryptoPolicy::production_default`); requires both X25519 and
  ML-KEM-768.
- `HybridMode::PostQuantumOnly` — hardening profile whose
  `enforce`/`validate` path rejects any classical-only exchange **and**
  flags a hybrid exchange tagged as a classical fallback
  (downgrade-attempt detection), emitting a `KeyExchangeAudit` row to an
  optional `KeyExchangeAuditor` sink.
- `HybridMode::ClassicalOnly` — migration/test only; documented as
  "never enabled in production."

The honest caveat: **as of this writing nothing calls that enforcement /
audit path** — `hybrid_kem_encap` / `hybrid_kem_decap` do not invoke it,
and no consumer wires a `KeyExchangeAuditor`. So the operator-selectable
posture and the per-exchange audit trail are a tested *primitive*
(level 1) awaiting integration, not an active runtime control. A
procurement reviewer should read downgrade resistance as
"structural — only a hybrid path exists" today, with policy-driven
enforcement and audit as planned wiring.

### 2.4 Honest gap: what the KEM does *not* yet protect

The hybrid KEM is **implemented, tested, benchmarked, and exposed via
an API** (`POST /crypto/hybrid_keypair` in `substrate_server`; round-trip
in `crates/integration_tests/tests/crypto_round_trip.rs`; benchmarked in
`crates/benchmarks`). What it is **not** is wired into an
end-to-end multi-device sync transport:

- `crates/sync_engine` implements the CRDT merge math (`crdt.rs`,
  `delta.rs`, `op_log.rs`, `persist.rs`) but has **no network transport
  module** and is documented as "a deliberate stub until Phase 2."
- Consequently there is today **no automatic, always-on PQ-protected
  channel between two real devices**. The KEM protects key transfers a
  host explicitly performs through the primitive/endpoint; it does not
  yet protect a shipped sync feature, because that feature's transport
  is not built.

The honest procurement statement is therefore: *the substrate has a
hybrid PQ KEM primitive whose only code path is hybrid (so it cannot be
silently downgraded), plus a tested-but-not-yet-wired policy layer to
make that posture operator-selectable and auditable; it uses the KEM for
the key-transfer operations that exist today; the broader "multi-device
sync is post-quantum secure" story is gated on the Phase-2 sync
transport landing.*

---

## 3. Key hierarchy, per-scope DEK lifecycle, and cryptographic forgetting

### 3.1 Hierarchy as implemented

```text
Master key (32 B, per user) ── lives in platform secure element; never a
   │                            long-lived plaintext in process
   │  HKDF-SHA256("scope-dek-wrap:v1")
   ├── DEK wrapping key ──────── AEAD-wraps each per-scope DEK at rest
   │        │                    (scope_deks.wrapped_dek)
   │        └── Scope DEK (32 B, RANDOM, per scope) ── AEAD-wraps per-row CEKs
   │                 │                                  (body_store_key_wraps.wrapped_cek)
   │                 └── Content Encryption Key (CEK, 32 B, RANDOM, per body row)
   │                          └── XChaCha20-Poly1305 over the evidence body
   └── (other context-labelled subkeys: SQLCipher page key, permission-tuple key)
```

Two correctness points that matter for the forgetting guarantee and
that **refine** the simplified diagram in
[crypto-spec.md](../technical/crypto-spec.md):

1. **Scope DEKs are random, not HKDF-derived.** New scopes generate a
   fresh DEK from the OS RNG and store it **only** as a blob wrapped
   under the master-derived wrapping key (`scope_deks` table, schema in
   `crates/evidence_store/src/schema.rs`; `store_scope_dek` /
   `dek_wrapping_key` in `store.rs`). This is deliberate: if DEKs were
   deterministically derived from the master key, "destroying the DEK"
   would be meaningless because `master_key + context` could always
   re-derive it. Because the DEK is random and persisted *only* wrapped,
   deleting the wrap makes it unrecoverable **even if the master key is
   later compromised**. (A legacy HKDF-derived DEK path exists for
   pre-existing scopes; new scopes use the random+wrapped path.)
2. **Bodies are deduplicated under per-row CEKs.** A shared body row has
   one ciphertext and N per-scope wraps of its random CEK; forgetting a
   scope drops that scope's wraps, and when no wraps remain the body is
   unrecoverable.

### 3.2 Epoch lifecycle and rotation

`crypto::forgetting` models per-scope, per-epoch keys
(`ScopeDek` / `EpochDek`, both zeroize-on-drop and on explicit
`destroy()`). `EpochManager` advances epochs under an
`EpochRotationPolicy` (default: rotate after **24 h** or **16 GiB**
encrypted, whichever first), with triggers `TimeElapsed` /
`SizeExceeded` / `PolicyForced`. `EpochId::next()` returns a hard
`CryptoError::EpochOverflow` rather than saturating, which preserves the
monotonic-epoch (forward-secrecy) invariant — a previously-fixed bug
where a saturating counter could rebind the terminal epoch id to a new
DEK. Master-key rotation (a separate, offline operation) is documented
in [key-rotation.md](key-rotation.md); it re-wraps DEKs and re-keys
SQLCipher pages but **does not** re-encrypt bodies and **does not**
resurrect forgotten scopes.

### 3.3 What cryptographic forgetting *guarantees*

Forgetting is **key destruction**, not soft-delete. `forget_scope` is
wired end-to-end: `ffi::forget_scope` → `forget_scope_state` calls
`crypto::forgetting::destroy_scope_dek`, which:

1. Zeroizes the in-memory scope DEK and every per-epoch DEK for the
   scope, and removes them from the `DekRegistry`.
2. Records a tombstone per `(scope, epoch)` and a scope-wide
   `forgotten_scopes` entry, and emits one `KeyDestructionEvent` per
   destroyed key for the audit trail.
3. Persists the tombstone durably through the `TombstoneStore` hook
   (the FFI runtime backs this with the SQLCipher `forgotten_scopes` /
   `epoch_tombstones` tables, `INSERT OR IGNORE` for idempotency), so a
   forget interrupted mid-purge is **completed on the next reopen** via
   tombstone replay (pinned by
   `crates/evidence_store/tests/recovery_hardening.rs`).
4. Deletes the wrapped DEK row, so even a later master-key compromise
   cannot reconstruct the key.

The substrate side additionally purges the FTS5 plaintext index for the
scope (`EvidenceStore::purge_fts_for_scope`), because that index holds
tokenised plaintext independent of the DEK. Net guarantee: **after a
successful `forget_scope`, the scope's evidence bodies are permanently
undecryptable in-process and on disk, and the destruction is auditable
and crash-durable.**

### 3.4 What cryptographic forgetting does *not* guarantee

Stated plainly, because procurement and legal reviewers need the
boundary, not the marketing line:

- **It does not erase data beneath the SQLCipher layer.** If the host
  filesystem, a backup tool, or a copy-on-write/snapshotting volume
  retains pre-image pages, that residue survives outside the substrate's
  control. This is a host-OS concern.
- **It does not protect against a compromised running process.** An
  attacker with code execution in the live process can read a DEK or
  plaintext *while it is in memory*. Zeroize-on-drop and scope-bound
  keys shrink the window; they do not close it.
- **Durable forgetting depends on the host's `TombstoneStore` succeeding.**
  If the host passes `None` (ephemeral mode) or the persist call fails
  and is ignored, an in-memory destroy is effective for the current
  process but a restart may not see the tombstone. The API surfaces the
  persist error precisely so callers do not silently swallow it.
- **It is not yet externally audited** (see [§6](#6-external-review-readiness)).

---

## 4. Residual risks, assumptions, and side channels

### 4.1 Trust assumptions (must hold for the guarantees above)

- The host OS provides process isolation and filesystem protection.
- The platform secure store (Keychain / Keystore / DPAPI / TPM / TEE
  sealed memory) behaves as documented and holds the master key.
- The host shell does not leak the master key across the FFI boundary
  (see the severity matrix in [key-management.md §2](key-management.md)).
- The OS CSPRNG is healthy: all keys and nonces come from `getrandom` /
  OS RNG, and the substrate **panics** rather than continuing if it
  cannot draw entropy (see [SECURITY.md → RNG](../../SECURITY.md#random-number-generation)).

### 4.2 Implementation-maturity risk

The PQ primitives currently run on **pure-Rust, pre-1.0 RustCrypto
crates** (`ml-kem`, `ml-dsa`) implementing recently-finalised FIPS
203/204 standards, plus the PQClean SPHINCS+ reference via
`pqcrypto-sphincsplus`. These are reputable but young; the intended
`liboqs` swap ([§1.2](#12-backend-abstraction-intentional-swap-point))
has not been made. Mitigations in place: NIST KAT vectors
(`crates/crypto/tests/nist_kat_vectors.rs`), property/adversarial tests
(`proptest_audit.rs`), and `cargo-fuzz` harnesses for KEM round-trip,
AEAD, HKDF, and both signers. SPHINCS+ has no deterministic seeded
keygen exposed by the safe façade, so the substrate does not advertise
one. Residual risk: an upstream implementation flaw in a young crate is
exactly the scenario the **hybrid** KEM hedges against on the KEM side,
but the **signature** path (ML-DSA-65) does not have an
always-on classical co-signer — SPHINCS+ co-signing is reserved for the
archival `CoSigner` path, not per-synthesis provenance.

### 4.3 Confidential-compute attestation

The TEE synthesis worker's side-channel posture (short attestation TTL,
zeroize-on-drop intermediates, page pre-faulting + `mlock` pinning,
constant-time tag verification, ML-KEM implicit rejection) is documented
in full in [tee-side-channels.md](tee-side-channels.md) and should be
read alongside this section. **The critical honesty caveat:**
`crypto::attestation` ships **mock attestation for all platforms**
today — real Intel TDX / AMD SEV-SNP / Nitro quote verification is
feature-flagged future work (`intel-tdx`, `amd-sev-snp`,
`nitro-enclaves`). Until those land, the attestation report binds a
synthesizer key to a *mock* measurement and must not be relied on as a
hardware root of trust in production. Relatedly, the per-call
content-binding BLAKE3 digest is an **audit signal that is only logged**,
not embedded in a signed attestation/provenance record, so a downstream
verifier cannot yet cryptographically check *which* content ran under
*which* attestation.

### 4.4 Side channels in the primitives

Cache/timing leakage inside the underlying primitives (ChaCha20,
Poly1305, X25519, ML-KEM, ML-DSA, SHA-256, BLAKE3) is the responsibility
of the vetted upstream crates; the substrate does not re-implement them.
Microarchitectural data-sampling (MDS / transient-execution) is a
CPU/firmware concern. The substrate's own constant-time discipline is
limited to where it controls comparisons (AEAD tag verification, ML-KEM
implicit rejection), audited in
`crates/crypto/tests/security_hardening.rs` and
`tests/timing_side_channel.rs`.

### 4.5 Residual-risk register (summary)

| Risk | Likelihood | Impact | Mitigation / status |
|---|---|---|---|
| CRQC breaks X25519 (HNDL on transfers) | Long-horizon | High if PQ absent | Hybrid ML-KEM-768 always combined into the transfer secret (structural, not policy-gated) |
| Flaw in young `ml-kem`/`ml-dsa` crate | Low–Med | Med | Hybrid hedges KEM; KATs/fuzz; `liboqs` swap planned (not done) |
| In-process memory disclosure | Med | High | Zeroize-on-drop, scope-bound keys; window not closed |
| Host retains pre-image snapshots | Med | High | Out of substrate scope; documented host-OS concern |
| Reliance on mock attestation | Current | High if trusted | Documented as mock-only; real TEE quote verify is future work |
| Tombstone persist failure ignored by host | Low | Med | Fallible API surfaces error; replay-on-reopen |
| Downgrade to classical-only KEM | Low | High | No classical-only path exists in `hybrid_kem_encap`/`decap` (structural); the `PostQuantumOnly`/`HybridTransition` enforcement + audit layer is implemented and tested but **not yet wired** to any callsite |

---

## 5. Compliance mapping — HIPAA / SOX / FERPA

The substrate is an embeddable library, **not** a hosted compliant
system: it provides technical building blocks a Covered Entity / issuer
/ educational institution composes into a compliant deployment. Each row
below marks whether the control is **Substrate-provided**, **Shared**,
or **Host-owned**. The GDPR / SOC 2 / HIPAA control-by-control mapping
with line-level code citations already lives in
[operator/compliance.md](../operator/compliance.md); this section adds
the **SOX** and **FERPA** lenses and ties the PQC-specific controls
together.

### 5.1 HIPAA Security Rule (45 CFR §164.312)

| Safeguard | Substrate capability | Ownership |
|---|---|---|
| §164.312(a)(2)(iv), (e)(2)(ii) Encryption at rest/in transit | XChaCha20-Poly1305 bodies + SQLCipher AES-256 page key (`crypto::encrypt_aead`, `EvidenceStore::open`); hybrid PQ KEM for transfer | Substrate-provided (transfer requires host to invoke) |
| §164.312(b) Audit controls | `audit_service` `KeyDestruction` action is first-class; every forget emits `KeyDestructionEvent` | Shared (substrate records; host ships/retains logs) |
| §164.312(a)(1),(d) Access control / authentication | Zanzibar reachability (`permission_service::check_permission`); authentication is host-owned | Shared |
| §164.312(c) Integrity | BLAKE3 content hashing + AEAD tags + ML-DSA-65 provenance signatures | Substrate-provided |
| §164.310(d)(2)(i),(ii) Media disposal / re-use (data destruction) | Cryptographic forgetting (key destruction) renders a scope's PHI ciphertext permanently unrecoverable in place | Substrate-provided (host-OS snapshots excepted) |

> **HIPAA scope note (honest).** The HIPAA Security Rule has **no
> GDPR-style "right to erasure."** Cryptographic forgetting is mapped
> here to the **media-disposal / re-use** safeguard
> (§164.310(d)(2)(i),(ii)) — destroying PHI so it cannot be recovered —
> not to a deletion *right*. A patient's Privacy-Rule rights to *access*
> (§164.524) and *amendment* (§164.526) are separate, host-owned
> workflows that the substrate's `export_plane` `ExportView` and
> proposal-only write contract (see [§5.3](#53-ferpa-student-education-records))
> help support but do not themselves satisfy.

### 5.2 SOX (financial-records integrity & retention)

SOX §302/§404 turn on **integrity, retention, and auditability** of
financial records, not on a specific cipher. Mapping:

| SOX concern | Substrate capability | Ownership |
|---|---|---|
| Record integrity / non-repudiation | ML-DSA-65 provenance signatures on synthesis outputs; SPHINCS+ archival co-signing (`CoSigner`, dual-signature, both halves must verify) | Substrate-provided |
| Tamper-evident audit trail | Append-only `audit_service` log with durable persistence; `KeyDestruction` events | Shared |
| Long-horizon confidentiality of harvested records | Hybrid PQ KEM on transfers; 256-bit symmetric at rest (Grover-resistant) | Substrate-provided (transfer path gated on §2.4) |
| Retention / legal hold (do **not** forget under hold) | Forgetting is explicit and auditable; host must gate `forget_scope` behind a legal-hold check | **Host-owned** — the substrate will forget if asked; it has no built-in legal-hold lock |
| Change management over crypto-critical code | `CODEOWNERS` gating on `crypto`/`ffi`/`evidence_store`; `cargo-audit`/`cargo-deny`/SBOM CI gates | Shared |

> **Honest SOX caveat.** Cryptographic forgetting and SOX retention are
> in direct tension: a successful `forget_scope` is **irreversible**.
> The substrate provides no internal "legal hold" that blocks
> forgetting; preventing destruction of records under hold is a
> host-integration responsibility.

### 5.3 FERPA (student education records)

FERPA centres on **disclosure control** and **parent/eligible-student
rights** over education records:

| FERPA concern | Substrate capability | Ownership |
|---|---|---|
| Disclosure limited to authorized parties | Zanzibar reachability checks on every lookup; connector source ACLs projected into the permission graph (a connector cannot widen access) | Substrate-provided |
| Right to inspect/review records | `export_plane` portable export (`ExportView` / `EvidencePack`), policy-gated by `export_plane::PolicyEngine` | Shared (substrate produces structure; host serialises/transmits) |
| Right to request amendment | Proposal-only agent contract (`agent_contract::ProposalState`): agents propose, a human/policy promotes — a built-in amendment checkpoint | Shared |
| Confidentiality of harvested records | Encryption at rest + hybrid PQ KEM on transfer | Substrate-provided |
| Data minimisation / retention limits | Decay state machine + 5 MiB noise ring buffer (`memory_manager`, `evidence_store`) | Substrate-provided |

### 5.4 Evidence / export checklist (what to hand an auditor)

A reviewer assembling an evidence pack for any of the three regimes can
collect, today:

- [ ] **Primitive inventory** — this document §1 + `crates/crypto` source.
- [ ] **KAT / interop proof** — `crates/crypto/tests/nist_kat_vectors.rs`.
- [ ] **Encryption-at-rest proof** — `aead.rs` + SQLCipher pragmas in
      `evidence_store/src/store.rs`; AEAD boundary tests.
- [ ] **PQ key-exchange posture** — `hybrid_kem.rs` (both halves always
      combined; no classical-only path) plus the implemented-but-unwired
      `hybrid_enforcement.rs` (`HybridMode`, downgrade-rejection,
      `KeyExchangeAudit`) for the operator-selectable posture once wired.
- [ ] **Forgetting proof** — `destroy_scope_dek` events +
      `recovery_hardening.rs` tombstone-replay test + the durable
      `forgotten_scopes` / `epoch_tombstones` tables.
- [ ] **Audit-trail proof** — `audit_service` `KeyDestruction` entries.
- [ ] **Provenance/integrity proof** — ML-DSA-65 signature
      verification on synthesis outputs.
- [ ] **Data-subject/portability export** — `export_plane` `ExportView`.
- [ ] **SBOM + dependency policy** — CycloneDX SBOM artifact + `deny.toml`.
- [ ] **Gaps register** — this document §4.5 + [§6](#6-external-review-readiness),
      so the auditor sees the known-incomplete items up front.

---

## 6. External-review readiness

### 6.1 Implementation-state matrix

| Primitive / control | (1) Implemented & tested | (2) Wired to a prod data path | (3) Externally audited |
|---|---|---|---|
| XChaCha20-Poly1305 AEAD | yes | yes (evidence bodies) | pending |
| SQLCipher (AES-256) at rest | yes | yes | pending |
| HKDF-SHA256 derivation | yes | yes | pending |
| Hybrid X25519+ML-KEM-768 KEM | yes | partial — endpoint + ad-hoc transfer; **sync transport stub** | pending |
| ML-DSA-65 provenance signatures | yes | yes (FFI synthesis) | pending |
| SPHINCS+ archival co-sign | yes | yes (archival `CoSigner` path) | pending |
| Cryptographic forgetting | yes | yes (`ffi::forget_scope`) | pending |
| Epoch rotation policy | yes | partial | pending |
| TEE attestation | yes (mock only) | no — real quote verify is future work | pending |

### 6.2 What a third-party crypto audit would verify

A credible external review (the substrate is positioned as audit-ready;
scope/onboarding live in [audit-scope.md](audit-scope.md) and
[audit-guide.md](audit-guide.md)) should confirm:

1. **Parameter sets** match FIPS 203 (ML-KEM-768) and FIPS 204
   (ML-DSA-65), and SPHINCS+-SHAKE-128f-simple, against NIST KATs.
2. **The hybrid combiner is sound** — no length-extension / domain-
   separation weakness in the `concatenate-then-HKDF` construction, and
   no silent drop of the PQ half on any encap/decap path.
3. **No downgrade path** exists structurally (the KEM has only a hybrid
   code path), and — once it is wired — that the `HybridTransition` /
   `PostQuantumOnly` enforcement layer rejects and audits downgrade
   attempts as specified.
4. **Forgetting is complete and irreversible** — DEK destruction +
   wrap-row deletion + FTS purge + crash-durable tombstone replay leave
   no recoverable plaintext or re-derivable DEK (including after
   master-key compromise).
5. **Zeroize discipline** holds on all long-lived secret state, on both
   the normal path and panic unwinds.
6. **RNG sourcing** — every production key/nonce comes from the OS
   CSPRNG with fail-closed behaviour on entropy failure.
7. **Constant-time** comparisons on secret-dependent paths (AEAD tag,
   ML-KEM implicit rejection).
8. **Dependency provenance** — `deny.toml` (no advisories, no yanked
   crates, license allow-list, crates.io-only) and the CycloneDX SBOM.
9. **Gap closure** — independent confirmation of the §4 caveats,
   especially that the attestation path is mock-only and the sync KEM
   consumer is not yet wired, so the published posture matches reality.

### 6.3 Honest bottom line

The substrate's defensible wedge — **privacy + post-quantum +
cryptographic forgetting + on-device + multilingual breadth** — is real
at the primitive and at-rest levels and is genuinely differentiated. The
work between "true claim" and "procurement checkbox" is: (a) wire the
hybrid KEM into a real sync transport, (b) replace mock attestation with
real TEE quote verification, (c) optionally land the `liboqs` backend,
and (d) obtain an external audit. None of these is fatal; all are named
here so a buyer can price the gap accurately rather than discover it.

---

## Further reading

- [crypto-spec.md](../technical/crypto-spec.md) — primitive inventory and key hierarchy.
- [threat-model.md](threat-model.md) — substrate-wide threat model and non-goals.
- [key-management.md](key-management.md) — master-key custody, per-platform secure storage.
- [key-rotation.md](key-rotation.md) — master-key rotation procedure and guarantees.
- [tee-side-channels.md](tee-side-channels.md) — confidential-compute side-channel posture.
- [operator/compliance.md](../operator/compliance.md) — GDPR / SOC 2 / HIPAA control-by-control mapping with code citations.
- [audit-scope.md](audit-scope.md) / [audit-guide.md](audit-guide.md) — external-audit scope and onboarding.
- [SECURITY.md](../../SECURITY.md) — policy, disclosure, audit status, RNG posture.
