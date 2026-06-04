# Terraform modules (starting points)

Minimal, **opinionated starting points** for running the Knowledge stack
on a managed Kubernetes cluster. They are intentionally small and are not
production-hardened — read each module's own README and hardening
checklist before relying on it.

| Module      | Provider | What it provisions                                  |
|-------------|----------|-----------------------------------------------------|
| [`aws/`](aws/) | EKS      | EKS control plane + managed node group + EBS CSI.   |
| [`gcp/`](gcp/) | GKE      | GKE Autopilot cluster.                              |

## Why Kubernetes (and not ECS / Cloud Run)?

The substrate is a **stateful** service: it owns a SQLCipher database
that needs block storage with real POSIX/locking semantics. Kubernetes
gives it that via a `PersistentVolumeClaim` (EBS on EKS, PD on GKE
Autopilot). ECS+EFS and Cloud Run are a poor fit for SQLite-style
workloads, so both modules provision a cluster and then hand off to the
Helm chart, which models the gateway/substrate topology directly.

## Workflow

1. `terraform apply` the module for your cloud to create the cluster.
2. Configure `kubectl` using the module's `kubeconfig_command` output.
3. `helm install knowledge ../../helm/knowledge --set secrets.masterKey=...`
   (see [`deploy/helm/knowledge`](../helm/knowledge)).

Each module reuses existing network resources (subnets / VPC) rather than
creating them, so they slot into an existing landing zone.
