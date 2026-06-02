# Compliance Mapping

This document maps the Knowledge substrate's technical capabilities to
major compliance frameworks. Each entry references the implementing code
so auditors and reviewers can trace the control directly.

> **Scope.** Only the Rust workspace in this repository is covered.
> Host-shell applications (iOS, Android, macOS, Windows, Electron) and
> production deployment infrastructure live in sibling repositories and
> are out of scope.

---

## 1. GDPR

### 1.1 Right to Erasure (Art. 17)

| Requirement | Substrate implementation |
|---|---|
| Delete all personal data on request | **Cryptographic forgetting** via DEK destruction. `ffi::forget_scope` (`crates/ffi/src/lib.rs`) and the shared `forget_scope_state` helper destroy the per-scope Data Encryption Key, record a durable tombstone in `forgotten_scopes`, purge FTS5 indexes, body-key wraps, memory blobs, connector state, approved-document payloads, synthesis history, and scope-DEK rows. Once the DEK is destroyed, all ciphertext encrypted under it is cryptographically unrecoverable — even if the on-disk rows remain. |
| Erasure must be verifiable | The `forgotten_scopes` tombstone table (`crates/evidence_store/src/schema.rs` v4) persists the scope UUID and `forgotten_at` timestamp. On next `open_store`, tombstones are replayed into the in-memory `DekRegistry` so the scope remains inaccessible across restarts. |
| Erasure applies to derived data | `forget_scope_state` tears down memory objects (working memory, channel/domain/tenant synthesis windows), connector instances, connector tokens, approved-document payloads, and synthesis version history — not just raw evidence. |

**Code references:**
- `crates/ffi/src/lib.rs` — `forget_scope`, `forget_scope_state`
- `crates/evidence_store/src/store.rs` — `record_forgotten_scope`, `purge_fts_for_scope`, `purge_body_key_wraps_for_scope`
- `crates/crypto/src/forgetting.rs` — `destroy_scope_dek`, `DekRegistry`

### 1.2 Data Portability (Art. 20)

| Requirement | Substrate implementation |
|---|---|
| Export personal data in structured, machine-readable format | The `export_plane` crate (`crates/export_plane/`) provides portable concept profiles (`PortableConceptProfile`) with a policy engine (`PolicyEngine`) that evaluates export constraints and sensitivity ceilings before releasing data. |
| Right to transmit to another controller | `ExportPolicy` supports configurable constraints and sensitivity classes. The `PolicySimulator` (`crates/export_plane/src/simulator.rs`) allows dry-run evaluation of export decisions. |

**Code references:**
- `crates/export_plane/src/profile.rs` — `PortableConceptProfile`, `ApprovedConcept`
- `crates/export_plane/src/policy.rs` — `ExportPolicy`, `PolicyEngine`, `ExportDecision`
- `crates/export_plane/src/simulator.rs` — `PolicySimulator`

### 1.3 Consent / Lawful Basis (Art. 6–7)

| Requirement | Substrate implementation |
|---|---|
| Process only with valid legal basis | The `agent_contract` crate (`crates/agent_contract/`) enforces a **proposal-only model**: agents cannot directly write to the evidence store. Every agent output (observation, concept, relation, summary) must pass through `AgentProposal` → `validate_proposal` → `ProposalStore` lifecycle before it becomes a `CanonicalArtifact`. |
| Consent withdrawal | Proposals can be rejected (`ProposalDecision`); the lifecycle state machine (`ProposalState`) tracks pending → approved → canonical transitions. Combined with `forget_scope` for full data removal on consent withdrawal. |

**Code references:**
- `crates/agent_contract/src/lib.rs` — `AgentProposal`, `ProposalKind`
- `crates/agent_contract/src/schema.rs` — `validate_proposal`, `ProposalValidationError`
- `crates/agent_contract/src/lifecycle.rs` — `ProposalState`, `ProposalStore`, `AutoPromotionPolicy`

### 1.4 Data Minimisation (Art. 5(1)(c))

| Requirement | Substrate implementation |
|---|---|
| Collect only what is necessary | **Decay state machine** in `memory_manager` (`crates/memory_manager/`) implements automatic memory decay: `Candidate → Reinforced → Decaying → Archived`. Unreinforced memories decay and are eventually archived or purged. Retention scoring (`crates/memory_manager/src/retention.rs`) determines which memories survive each sweep. |
| Time-limited retention | **Noise ring buffer** in `evidence_store` (`crates/evidence_store/src/store.rs`): low-importance evidence (noise class) is written to a size-capped ring buffer with FIFO eviction. Once the buffer exceeds `ring_buffer_max_bytes` (default 5 MiB), oldest entries are deleted. Evicted ciphertext is unrecoverable because the rows are physically deleted. |
| Privacy by design | The `privacy_strip` module (`crates/memory_manager/src/privacy_strip.rs`) enforces a privacy-strip invariant on all memory operations. |

**Code references:**
- `crates/memory_manager/src/decay.rs` — decay state machine
- `crates/memory_manager/src/retention.rs` — retention scoring
- `crates/memory_manager/src/transitions.rs` — state transitions
- `crates/evidence_store/src/store.rs` — `ring_buffer_insert`, `ring_buffer_max_bytes`
- `crates/memory_manager/src/privacy_strip.rs` — privacy-strip invariant

---

## 2. SOC 2 Readiness

The following maps the SOC 2 Trust Services Criteria (CC1–CC9) to
substrate capabilities. Status reflects the substrate layer only — the
host application and deployment infrastructure contribute additional
controls.

| Control | Description | Status | Substrate capability |
|---|---|---|---|
| **CC1** | Control Environment | Partially Implemented | `deny.toml` enforces license + advisory policy. `unsafe_code = "deny"` workspace-wide with per-crate `forbid` on `crypto`. CI gates (fmt, clippy, audit, deny, MSRV, unsafe-code allowlist scan) enforce code quality. CODEOWNERS (if configured) restricts merge authority. |
| **CC2** | Communication & Information | Partially Implemented | `SECURITY.md` documents threat model, audit posture, RNG rationale. `docs/HOST_KEY_HANDLING.md` provides per-platform key guidance. This `COMPLIANCE.md` maps controls. |
| **CC3** | Risk Assessment | Requires Host Integration | The substrate's threat model (`SECURITY.md`) identifies risks. Formal risk assessment processes (risk registers, periodic reviews) require host-organisation policy. |
| **CC4** | Monitoring Activities | Partially Implemented | `audit_service` crate (`crates/audit_service/`) provides append-only audit logging with per-scope queries. `tracing` instrumentation throughout the substrate. Metrics counters on FFI operations (`crates/ffi/src/metrics.rs`). Host integration needed for alerting and SIEM forwarding. |
| **CC5** | Control Activities | Implemented | `permission_service` crate (`crates/permission_service/`) implements Zanzibar-style access control with reachability checks, secondary indexes, and audit-log integration for every grant/revoke. Agent contract proposal-only model prevents unauthorised writes. |
| **CC6** | Logical & Physical Access | Partially Implemented | SQLCipher encryption at rest with per-scope DEKs. Hybrid PQC key exchange (X25519 + ML-KEM-768). `KeyStorage` trait enforces hardware-backed key material on supported platforms. Physical access controls require host integration. |
| **CC7** | System Operations | Partially Implemented | Schema migrations are versioned and forward-only (`crates/evidence_store/src/schema.rs`). CI pipeline enforces build, test, lint, security checks on every PR. Operational monitoring (uptime, incident response) requires host integration. |
| **CC8** | Change Management | Implemented | Branch protection, CI gates (fmt, clippy, test, audit, deny, unsafe-code scan, MSRV), SBOM generation (CycloneDX). All changes go through PR review. |
| **CC9** | Risk Mitigation | Partially Implemented | Cryptographic forgetting mitigates data-breach impact. PQC posture mitigates harvest-now-decrypt-later. Ring-buffer eviction limits noise-class data retention. Full risk mitigation program requires host-organisation policy. |

---

## 3. HIPAA Considerations

The Knowledge substrate provides technical safeguards relevant to HIPAA
compliance when deployed in a healthcare context. A Business Associate
Agreement (BAA) is required before processing Protected Health
Information (PHI).

### 3.1 Encryption at Rest (§ 164.312(a)(2)(iv))

| Requirement | Substrate implementation |
|---|---|
| Encrypt ePHI at rest | **SQLCipher** page-level AES-256 encryption on the evidence store database. Per-scope evidence bodies are additionally encrypted with **XChaCha20-Poly1305 AEAD** under per-scope, per-epoch keys derived via HKDF-SHA256 from the user master key. |
| Key management | Master key stored via platform hardware-backed `KeyStorage` (iOS Keychain, Android Keystore, Windows DPAPI/TPM, macOS Secure Enclave). Per-scope DEKs randomly generated from OS RNG and AEAD-wrapped under a master-derived wrapping key (`scope_deks` table, schema v6). |

**Code references:**
- `crates/evidence_store/src/store.rs` — SQLCipher `PRAGMA key`, `encrypt_aead` on ingest
- `crates/crypto/src/aead.rs` — XChaCha20-Poly1305 AEAD
- `crates/crypto/src/kdf.rs` — HKDF-SHA256 key derivation
- `crates/crypto/src/key_storage.rs` — `KeyStorage` trait

### 3.2 Audit Trail (§ 164.312(b))

| Requirement | Substrate implementation |
|---|---|
| Record access to ePHI | `audit_service` crate (`crates/audit_service/`) provides an append-only, encrypted audit log. Every permission grant/revoke is logged. Entries are queryable by scope, action type, actor, and time range. |
| Tamper-evident logging | Audit entries are assigned monotonically increasing sequence numbers. The `PersistentAuditLog` (`crates/audit_service/src/persist.rs`) stores entries in an encrypted SQLCipher database. |

**Code references:**
- `crates/audit_service/src/log.rs` — `AuditLog`, `AuditEntry`, `AuditQuery`
- `crates/audit_service/src/persist.rs` — `PersistentAuditLog`
- `crates/audit_service/src/entry.rs` — `AuditEntry`, `AuditActionType`

### 3.3 Access Controls (§ 164.312(a)(1))

| Requirement | Substrate implementation |
|---|---|
| Unique user identification | `ScopeId` (UUID v4) uniquely identifies each data scope. `permission_service` uses typed tuples (`crates/permission_service/src/tuple.rs`) with subject/relation/object triples. |
| Role-based access | Zanzibar-style reachability checks in `permission_service` (`crates/permission_service/src/check.rs`). Relations include `Owner`, `Viewer`, `Synthesizer`, `Proposer` with defined inheritance chains. |
| Emergency access | Not implemented at the substrate level — requires host integration. |

**Code references:**
- `crates/permission_service/src/check.rs` — reachability check
- `crates/permission_service/src/store.rs` — tuple store
- `crates/permission_service/src/namespace.rs` — namespace definitions

### 3.4 BAA Requirement

> **Note:** Deploying the Knowledge substrate in a HIPAA-covered context
> requires a Business Associate Agreement (BAA) between the data
> controller (covered entity) and the entity operating the substrate.
> The substrate provides the technical safeguards documented above, but
> administrative and physical safeguards (workforce training, facility
> access controls, disaster recovery) are the responsibility of the
> operating organisation and must be addressed in the BAA.
