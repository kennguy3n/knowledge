# GCP (GKE Autopilot) — Terraform starting point

> **This is a starting point, not a turnkey production module.** It
> provisions a minimal GKE Autopilot cluster for evaluating the Knowledge
> Helm chart. Review networking, private-cluster settings, and IAM before
> production use.

## What it creates

- A GKE Autopilot cluster (`google_container_cluster` with
  `enable_autopilot = true`). Google manages nodes, scaling, and
  patching; Autopilot provisions block-backed PersistentVolumes on
  demand, which the stateful substrate needs.

It uses the project's `default` network/subnetwork unless overridden, and
does not create a VPC.

## Usage

```bash
terraform init
terraform apply -var 'project_id=my-gcp-project'

# Point kubectl at the new cluster (see the kubeconfig_command output):
gcloud container clusters get-credentials knowledge-gke \
  --region us-central1 --project my-gcp-project

# Deploy the app with the Helm chart:
helm install knowledge ../../helm/knowledge \
  --set secrets.masterKey="$(openssl rand -hex 32)" \
  --set ingress.enabled=true
```

To allow `terraform destroy`, set `-var 'deletion_protection=false'`.

## Inputs

| Variable              | Default       | Description                          |
|-----------------------|---------------|--------------------------------------|
| `project_id`          | —             | GCP project ID. **Required.**        |
| `region`              | `us-central1` | Cluster region.                      |
| `name_prefix`         | `knowledge`   | Prefix for resource names.           |
| `network`             | `default`     | VPC network.                         |
| `subnetwork`          | `default`     | Subnetwork.                          |
| `release_channel`     | `REGULAR`     | RAPID / REGULAR / STABLE.            |
| `deletion_protection` | `true`        | Guard against accidental deletion.   |

## Production hardening checklist

- [ ] Use a private cluster with authorized networks.
- [ ] Create a dedicated VPC/subnet instead of `default`.
- [ ] Enable Workload Identity and scope service accounts.
- [ ] Manage `master_key` in Secret Manager and mount via the chart.
