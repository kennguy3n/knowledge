# Security finding template

Copy this template once per finding in the audit report. Keep one issue
per finding. See [audit-scope.md](audit-scope.md) for severity
conventions and expected deliverables.

---

## Finding: <short descriptive title>

| Field | Value |
|---|---|
| **ID** | `KNW-AUDIT-NNN` |
| **Severity** | Critical / High / Medium / Low / Informational |
| **CVSS v3.1** | `<base score>` (`<vector string>`) |
| **CWE** | `CWE-NNN: <name>` |
| **Status** | Open / Acknowledged / Fixed / Won't fix (with rationale) |
| **Affected component** | e.g. `crates/crypto/src/aead.rs` |

### Summary

One or two sentences: what the issue is and why it matters.

### Affected code

- File(s) and line range(s): `crates/<crate>/src/<file>.rs:<start>-<end>`
- Commit / version reviewed: `<git sha or tag>`

```rust
// Minimal excerpt of the affected code, if helpful.
```

### Description

Full technical explanation: the root cause, the conditions under which
it manifests, and the security property it violates (tie back to a
guarantee in [threat-model.md](threat-model.md) where applicable).

### Impact

What an attacker can achieve. State the trust boundary crossed (FFI
surface, substrate HTTP surface, or sync wire protocol) and the
realistic attacker model. If the issue is only reachable under one of
the documented non-goals, say so — it may be Informational.

### Reproduction steps

1. Step-by-step instructions.
2. Include a failing test, `cargo` command, or `cargo-fuzz` input where
   possible, e.g.:
   ```sh
   cargo test -p <crate> --test <suite> <case>
   ```
3. Attach any proof-of-concept input (minimized fuzz artifact, crafted
   wire message, etc.).

### Recommended fix

The correct long-term remediation (not just a symptomatic patch).
Reference the specific function/type to change and any invariant that
should be added to a test to prevent regression.

### References

- Related findings: `KNW-AUDIT-NNN`
- External: relevant CWE/CVE, spec sections, or upstream advisories.
```
