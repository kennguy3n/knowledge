# Observability Without Ops

> **TL;DR:** You can't SSH into a user's phone. Knowledge emits
> PII-free counters and per-subsystem health probes — scrapeable by
> Prometheus in the gateway modes — so you get crash and health
> telemetry and graceful degradation signals without collecting
> anything that would compromise the privacy model.

## The Business Problem

A product team ships an AI feature that runs partly on user devices and
partly behind a gateway. Something is wrong: a subset of users report
that synthesis "sometimes doesn't work." The team needs to know what's
failing and how often — but they cannot do the obvious thing. They
can't log the user's data to find out, because the entire value
proposition is that the data never leaves the device. Traditional
observability, which leans heavily on capturing request payloads and
content, is off the table.

So how do you operate a system whose defining feature is that you
*can't see* what's in it? You need telemetry that is rich enough to
diagnose problems but contains zero personal information by
construction.

## The Technical Approach

Knowledge instruments the substrate and gateway with **counters and
health signals that carry no PII** — covered in the
[monitoring guide](../docs/operator/monitoring.md). The design:

**PII-free counters.** Every meaningful operation increments a counter
keyed by *kind*, never by content. You learn that
`knowledge_errors_total{by_kind="..."}` went up, that ingest volume
spiked, or that a connector's sync failure count climbed — without ever
recording *what* was ingested or *whose* data it was. The counters
describe the system's behavior, not the user's data.

**Two scrape targets (gateway modes).** Prometheus scrapes the Go
gateway (`:8080/metrics`) for process and gateway counters, and the
Rust substrate's loopback endpoint (`:9090/internal/metrics`) for the
FFI counter set. The substrate endpoint is loopback-only and behind the
gateway, consistent with the [deployment topology](07-zero-to-production-deployment.md).

**Per-subsystem health probes.** The gateway exposes a `/health`
endpoint where each subsystem reports an up/down status. A `0` health
gauge is the signal that something is degraded, and the substrate
exposes **degradation levels** so partial failures surface *before*
they become a hard outage — e.g. a connector lagging is visible while
the rest of the system is still serving.

**Alerting rules.** The [monitoring guide](../docs/operator/monitoring.md)
ships alerting patterns built on these signals — error-rate thresholds,
health-gauge drops, degradation transitions — so on-call gets paged on
behavior, not on content.

## Implementation Walk-through

In the gateway modes, observability is wiring, not instrumentation work
— the counters already exist. Point Prometheus at the two targets and
the metric catalogue is populated; probe `/health` for subsystem
status:

```text
curl -s http://localhost:8080/health | jq .   # per-subsystem up/down
# Prometheus scrapes:
#   knowledge-gateway     :8080/metrics
#   knowledge-substrate   :9090/internal/metrics
```

The [monitoring guide](../docs/operator/monitoring.md) lists the metric
catalogue and recommended alerts; the
[runbook](../docs/operator/runbook.md) maps specific signals to incident
response per subsystem; and the
[troubleshooting guide](../docs/operator/troubleshooting.md) translates
counter and health patterns into likely root causes. For the product
team chasing the intermittent synthesis failures, the path is: check the
inference subsystem health gauge and the relevant error counter by kind,
correlate with degradation-level transitions, and follow the runbook —
all without touching a single byte of user content.

## Performance & Cost Implications

Counters are cheap to increment and cheap to scrape; the observability
layer is not a meaningful load on the substrate. Because the signals
are PII-free aggregates, there is also no expensive log-retention or
data-governance burden attached to them — you are not storing sensitive
payloads you then have to secure and expire.

In on-device-only deployments there is no gateway to scrape, so
telemetry is limited to what the host app chooses to surface through its
own (privacy-preserving) channels — a deliberate consequence of having
no server. The gateway modes are where the full Prometheus/health
stack applies, and even there the guarantee holds: rich operational
visibility, zero personal data.

## What's Next

That closes Series 2 — you can now deploy, scale, afford, sync, and
monitor the substrate. Series 3 turns to the real world: what Knowledge
looks like in regulated industries and across geographies. The next
post starts with the hardest privacy environment of all — healthcare.

---
*This is part 12 of the "Building Knowledge" series. [Previous: Sync Without Servers](11-sync-without-servers.md) | [Next: Knowledge for Healthcare](13-knowledge-for-healthcare.md)*
