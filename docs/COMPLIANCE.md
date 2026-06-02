# Compliance Mapping

This document maps the substrate's technical capabilities to the
controls expected by **GDPR**, **SOC 2**, and **HIPAA**. It is a
*substrate-side* mapping: the substrate is an embeddable Rust library,
not a hosted service, so several controls are explicitly the
responsibility of the **host application** that links it. Those are
called out as **Requires host integration** rather than glossed over.

Every code reference below was verified against the tree at the time
of writing; line numbers are approximate and may drift as the code
evolves, but the cited symbol names are stable.

See also: [`SECURITY.md`](../SECURITY.md) (threat model, audit scope,
disclosure) and [`docs/SUPPLY_CHAIN.md`](SUPPLY_CHAIN.md)
(dependency policy, SBOM, CI gates).

---

## GDPR

The substrate is designed so that a host can satisfy the
data-subject rights in GDPR Chapter 3 without bolting on a separate
deletion or export pipeline.

### Right to erasure (Art. 17) — cryptographic forgetting

The substrate implements erasure as **key destruction**, not row
deletion: every evidence body is encrypted under a per-scope Data
Encryption Key (DEK), so destroying that key renders all ciphertext
for the scope permanently unrecoverable in the process and on disk.

- **FFI entry point:** `ffi::forget_scope`
  ([`crates/ffi/src/lib.rs:694`](../crates/ffi/src/lib.rs)) →
  `forget_scope_state`
  ([`crates/ffi/src/lib.rs:777`](../crates/ffi/src/lib.rs)). The
  documented teardown is a 9-step sequence whose **load-bearing**
  step 1 is in-memory DEK destruction plus an on-disk tombstone
  (atomic via `FfiRuntime::forget_scope` → `TombstoneStore`); steps
  2–9 are best-effort secondary cleanups that the tombstone already
  makes unreachable through the public read path.
- **Key destruction:** `crypto::forgetting::destroy_scope_dek`
  ([`crates/crypto/src/forgetting.rs:427`](../crates/crypto/src/forgetting.rs))
  and the per-epoch `destroy_epoch_dek`
  ([`crates/crypto/src/forgetting.rs:508`](../crates/crypto/src/forgetting.rs)),
  which emit `KeyDestructionEvent`s for the audit trail.
- **Plaintext-index purge:** the FTS5 secondary index holds
  tokenised plaintext independent of the DEK, so erasure also purges
  it via `EvidenceStore::purge_fts_for_scope`
  ([`crates/evidence_store/src/store.rs:3398`](../crates/evidence_store/src/store.rs)).
- **Durability across crashes:** the erasure tombstone is persisted
  by `EvidenceStore::record_forgotten_scope`
  ([`crates/evidence_store/src/store.rs:1574`](../crates/evidence_store/src/store.rs))
  and replayed on re-open via `load_forgotten_scopes`
  ([`crates/evidence_store/src/store.rs:1608`](../crates/evidence_store/src/store.rs)),
  so a forget interrupted mid-purge still completes after restart.
  This is pinned by
  `crates/evidence_store/tests/recovery_hardening.rs`
  (`interrupted_forget_is_completed_by_tombstone_replay_on_reopen`).

**Status: Implemented.** Caveat: if the host filesystem retains
pre-image snapshots *beneath* the SQLCipher layer, that residue is a
host-OS concern outside the substrate's control (see
[`SECURITY.md`](../SECURITY.md) threat model).

### Right to data portability (Art. 20) — export plane

Structured, host-readable export of a subject's approved knowledge is
produced by the `export_plane` crate:

- `export_plane::PortableConceptProfile`
  ([`crates/export_plane/src/profile.rs:142`](../crates/export_plane/src/profile.rs))
  — the portable, serialisable concept profile.
- `export_plane::EvidencePack`
  ([`crates/export_plane/src/profile.rs:230`](../crates/export_plane/src/profile.rs))
  and `export_plane::ExportView`
  ([`crates/export_plane/src/profile.rs:323`](../crates/export_plane/src/profile.rs))
  — the materialised export view and its evidence bundle.
- Export is policy-gated by `export_plane::PolicyEngine`
  ([`crates/export_plane/src/lib.rs:46`](../crates/export_plane/src/lib.rs))
  so an export cannot leak data the requester is not entitled to.

**Status: Implemented** (substrate produces the portable structure;
the host serialises and transmits it).

### Lawfulness / consent (Art. 6, 7) — proposal-only agent contract

Agents never read-modify-write canonical state. They submit
*proposals* that a human or policy must promote, which gives the host
an explicit consent/authorisation checkpoint:

- `agent_contract::ProposalState`
  ([`crates/agent_contract/src/lifecycle.rs:42`](../crates/agent_contract/src/lifecycle.rs))
  is a strict lifecycle — `Proposed → UnderReview → Promoted |
  Rejected`, with `Promoted`/`Rejected` terminal.
- Submissions are write-only via `ProposalStore::submit_observation`
  / `submit_concept` / `submit_relation` / `submit_summary`
  ([`crates/agent_contract/src/lifecycle.rs:441`](../crates/agent_contract/src/lifecycle.rs)
  onward).

**Status: Implemented** (substrate enforces the proposal-only
contract; **the promotion authority/UX is host integration**).

### Data minimisation & storage limitation (Art. 5(1)(c),(e))

Two mechanisms bound how much data is retained and for how long:

- **Decay state machine:** `memory_manager::MemoryState`
  ([`crates/memory_manager/src/state.rs:26`](../crates/memory_manager/src/state.rs))
  decays `Candidate` objects below
  `DEFAULT_CANDIDATE_ARCHIVE_THRESHOLD = 0.15`
  ([`crates/memory_manager/src/decay.rs:20`](../crates/memory_manager/src/decay.rs))
  to `Archived`, and ages `Superseded` objects out after
  `DEFAULT_SUPERSEDED_TTL_DAYS = 90`
  ([`crates/memory_manager/src/decay.rs:25`](../crates/memory_manager/src/decay.rs)),
  via `decay_sweep`
  ([`crates/memory_manager/src/decay.rs:53`](../crates/memory_manager/src/decay.rs)).
- **Noise ring buffer:** low-value (`Noise`-class) messages never
  enter durable evidence; they land in a FIFO-evicted ring buffer
  capped at `DEFAULT_RING_BUFFER_MAX_BYTES = 5 MiB`
  ([`crates/evidence_store/src/store.rs:31`](../crates/evidence_store/src/store.rs))
  via `EvidenceStore::ring_buffer_insert`
  ([`crates/evidence_store/src/store.rs:1403`](../crates/evidence_store/src/store.rs)).
  Eviction is pinned by
  `crates/evidence_store/tests/recovery_hardening.rs`
  (`ring_buffer_evicts_oldest_first_and_keeps_insertion_order`).

**Status: Implemented.**

---

## SOC 2 — Trust Services Criteria readiness (CC1–CC9)

The Common Criteria are organisation-level controls; a library cannot
satisfy them alone. The table records what the substrate *provides*
toward each, and what the host/organisation must still supply.

| Criterion | Substrate capability | Status |
| --------- | -------------------- | ------ |
| **CC1** Control environment | Security-critical surfaces are gated by `CODEOWNERS` (crypto, ffi, napi, evidence_store, SECURITY.md → `@kennguy3n`); see [`docs/SUPPLY_CHAIN.md`](SUPPLY_CHAIN.md). | Partially — org governance is host. |
| **CC2** Communication & information | Documented threat model and disclosure process in [`SECURITY.md`](../SECURITY.md); machine-readable SBOM published in CI. | Implemented (substrate scope). |
| **CC3** Risk assessment | Property-based + adversarial test suites (`crates/crypto/tests/proptest_audit.rs`, `crates/permission_service/tests/adversarial_tests.rs`) and the new `security_hardening.rs` / `recovery_hardening.rs` suites. | Partially — org-level risk program is host. |
| **CC4** Monitoring | Metrics surface (`ffi::metrics::snapshot` / `MetricsSnapshot`) and the append-only audit log (`audit_service`). | Partially — aggregation/alerting is host. |
| **CC5** Control activities | Permission checks (`permission_service::check_permission`, [`crates/permission_service/src/check.rs:81`](../crates/permission_service/src/check.rs)) on every lookup. | Implemented (substrate scope). |
| **CC6** Logical & physical access | Encryption at rest (SQLCipher + XChaCha20-Poly1305), hardware-backed master-key storage via `crypto::KeyStorage`, Zanzibar-style authorisation. | Partially — physical access & key custody are host. |
| **CC7** System operations | Cryptographic forgetting, crash-recovery via tombstone replay, schema migrations with integrity tests (`recovery_hardening.rs`). | Implemented (substrate scope). |
| **CC8** Change management | `cargo-audit` + `cargo-deny` + SBOM CI gates (`.github/workflows/ci.yml`); CODEOWNERS review. | Implemented (substrate scope). |
| **CC9** Risk mitigation | Supply-chain policy (`deny.toml`), harvest-now-decrypt-later PQ posture (hybrid X25519 + ML-KEM-768). | Requires host integration for vendor/BCP. |

---

## HIPAA — Security Rule (45 CFR §164.312) technical safeguards

The substrate provides building blocks for a Covered Entity or
Business Associate; it does not itself constitute a compliant system.

### Encryption at rest — §164.312(a)(2)(iv), (e)(2)(ii)

- The SQLCipher database is keyed by an HKDF-derived page key
  (`EvidenceStore::open`,
  [`crates/evidence_store/src/store.rs:222`](../crates/evidence_store/src/store.rs);
  `cipher_page_size`/`kdf_iter` pragmas at
  [`crates/evidence_store/src/store.rs:235`](../crates/evidence_store/src/store.rs)).
- Evidence bodies, ring-buffer entries, and wrapped DEKs are sealed
  with XChaCha20-Poly1305 (`crypto::encrypt_aead`,
  [`crates/crypto/src/aead.rs:39`](../crates/crypto/src/aead.rs)).
- Secret key material is wiped on drop — `HybridSecretKey` derives
  `Zeroize` with `#[zeroize(drop)]`
  ([`crates/crypto/src/hybrid_kem.rs:67`](../crates/crypto/src/hybrid_kem.rs)),
  pinned by `crates/crypto/tests/security_hardening.rs`
  (`hybrid_secret_key_zeroize_wipes_every_secret_byte`).

**Status: Implemented** (substrate scope).

### Audit controls — §164.312(b)

- `audit_service::AuditActionType`
  ([`crates/audit_service/src/entry.rs:28`](../crates/audit_service/src/entry.rs))
  includes a `KeyDestruction` action
  ([`crates/audit_service/src/entry.rs:50`](../crates/audit_service/src/entry.rs)),
  so cryptographic-forgetting events are first-class audit records.
- Entries are appended via the in-memory log
  ([`crates/audit_service/src/log.rs:156`](../crates/audit_service/src/log.rs))
  and the durable persistence layer
  ([`crates/audit_service/src/persist.rs:201`](../crates/audit_service/src/persist.rs)).

**Status: Implemented** (substrate records events; **log shipping /
retention / tamper-evidence at rest are host integration**).

### Access controls — §164.312(a)(1), (d)

- Every authorisation decision is a Zanzibar-style reachability query
  via `permission_service::check_permission`
  ([`crates/permission_service/src/check.rs:81`](../crates/permission_service/src/check.rs)).
- Person/entity authentication and session management are
  **host-owned** — the substrate consumes an already-authenticated
  identity.

**Status: Partially — authorisation Implemented; authentication
Requires host integration.**

### Business Associate Agreement (BAA) note

The substrate ships as source/library and stores no PHI off-device on
its own. A Covered Entity embedding it remains responsible for
executing BAAs with any downstream processor (e.g. cloud sync targets
the *host* wires up) and for the administrative/physical safeguards
(§164.308, §164.310) that fall outside the substrate's technical
boundary.

---

## Summary of host responsibilities

The following controls are **not** satisfiable by the substrate alone
and must be implemented by the embedding host application:

- User authentication, session lifecycle, and consent UX.
- Master-key custody in a hardware-backed store (see
  [`SECURITY.md`](../SECURITY.md) "Key storage").
- Audit-log shipping, retention, and tamper-evident archival.
- Organisation-level governance, vendor management, and BCP/DR.
- Transmission security for any off-device sync the host enables.
