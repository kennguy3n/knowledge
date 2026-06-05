# FAQ

## What is Knowledge, in one sentence?

A privacy-first, post-quantum-secure knowledge substrate for AI
applications that runs on-device by default.

## Is it a product or a library?

A substrate you embed. You build your product on top of it via the
`ffi`/`napi` surfaces (native apps) or the Go gateway REST API (server
deployments).

## How is "$0/user/month" possible?

On-device synthesis means inference runs on the user's hardware, so you
don't pay per-token cloud inference that scales with your user base.
This is bounded, not magical — see
[cost-model.md](../operator/cost-model.md) for where costs *do* appear
(connectors, server-side synthesis, enterprise infra).

## Does it really work offline?

Yes. Retrieval and extraction run locally. Connector sync and optional
server-side synthesis need network, but the core memory loop does not.

## What does "cryptographic forgetting" mean?

Forgetting destroys the encryption key for a scope, making the
ciphertext unrecoverable — not a soft-delete flag. This is what makes
GDPR Article 17 erasure enforceable. See
[crypto-spec.md](../technical/crypto-spec.md).

## Why post-quantum crypto now?

Because of harvest-now, decrypt-later: data with a long confidentiality
horizon (health, finance, legal) captured today could be decrypted once
a quantum computer exists. Knowledge uses a hybrid classical+PQC KEM so
it's at least as strong as the stronger half.

## How many languages does it support?

22 languages for multilingual extraction, with script-aware routing so
CJK and other scripts aren't dropped even when language detection is
uncertain. See
[extraction-quality.md](../technical/extraction-quality.md).

## Which connectors are available?

Ten production connectors (Google Drive, OneDrive, Notion, Jira,
Confluence, Figma, HubSpot, Slack, Email, GitHub). See
[connector-protocol.md](../technical/connector-protocol.md).

## Has it been security-audited?

Not yet. The cryptographic design, threat model, and known limitations
are documented honestly in [SECURITY.md](../../SECURITY.md) and
[threat-model.md](../security/threat-model.md). Treat 1.0 accordingly.

## What platforms are supported?

iOS, Android, macOS, and Windows via the FFI/N-API surfaces, plus
Linux/server via the Go gateway. See
[platforms.md](../technical/platforms.md).

## How do I choose a deployment mode?

Use the decision tree in
[deployment-scenarios.md](deployment-scenarios.md).

## How do I contribute?

See [CONTRIBUTING.md](../../CONTRIBUTING.md) and the
[roadmap](roadmap.md).
