# Security Policy

## Reporting a Vulnerability

If you discover a security vulnerability in this project, please report
it responsibly. **Do not open a public GitHub issue.**

Send an email to **ken@uney.com** with:

- A description of the vulnerability and its potential impact.
- Steps to reproduce (proof of concept appreciated).
- Any suggested mitigation or fix.

We will acknowledge receipt within 72 hours and aim to provide a fix or
mitigation plan within 14 days, depending on severity.

## Scope

This policy covers the Rust workspace in this repository: every crate
under `crates/`, the CI pipeline, and the build artifacts they produce
(UniFFI `.xcframework`, JNI `.so`, N-API addon). It does **not** cover
the Go gateway, host UI shells, or production deployment infrastructure.

## Threat Model

The Knowledge substrate is designed to protect user data at rest on a
personal device. The threat model assumes:

- The device's OS provides process isolation and filesystem-level
  encryption.
- An attacker who obtains a copy of the encrypted SQLCipher database
  does **not** have the master key.
- An attacker who compromises the running process has full access to
  decrypted data in memory (defence-in-depth measures like
  zeroize-on-drop reduce the window, but do not eliminate it).

## Known Security Limitations

The following are honest gaps:

1. **No live connector traffic.** All connectors consume JSON fixtures.
   OAuth2 token handling and live API transport are type-surface only.

## Supported Versions

This project is pre-1.0 and does not yet have a stable release. All
security fixes target the `main` branch.
