# AWS (EKS) — Terraform starting point

> **This is a starting point, not a turnkey production module.** It
> provisions a minimal EKS cluster suitable for evaluating the Knowledge
> Helm chart. Review networking, IAM scoping, logging, and node sizing
> before any production use.

## What it creates

- An EKS control plane (`aws_eks_cluster`).
- A managed node group (`aws_eks_node_group`).
- The control-plane and node IAM roles with the AWS-managed policies EKS
  requires, plus `AmazonEBSCSIDriverPolicy` on the node role so the CSI
  driver can provision volumes (baseline; see hardening note on IRSA).
- The `aws-ebs-csi-driver` add-on, so the substrate's
  `PersistentVolumeClaim` binds to a gp3 EBS volume (block storage with
  the locking semantics SQLCipher needs — the reason this module uses
  EKS rather than ECS+EFS).

It does **not** create a VPC: pass existing `subnet_ids` spanning at
least two AZs.

## Usage

```bash
terraform init
terraform apply \
  -var 'subnet_ids=["subnet-aaa","subnet-bbb"]'

# Point kubectl at the new cluster (see the kubeconfig_command output):
aws eks update-kubeconfig --region us-east-1 --name knowledge-eks

# Deploy the app with the Helm chart:
helm install knowledge ../../helm/knowledge \
  --set secrets.masterKey="$(openssl rand -hex 32)" \
  --set ingress.enabled=true \
  --set ingress.className=alb
```

## Inputs

| Variable              | Default        | Description                              |
|-----------------------|----------------|------------------------------------------|
| `region`              | `us-east-1`    | AWS region.                              |
| `name_prefix`         | `knowledge`    | Prefix for resource names.               |
| `subnet_ids`          | —              | Existing subnets (≥ 2 AZs). **Required.**|
| `kubernetes_version`  | `1.30`         | EKS version.                             |
| `node_instance_types` | `["t3.large"]` | Worker instance types.                   |
| `node_desired_size`   | `2`            | Desired node count.                      |
| `node_min_size`       | `2`            | Min node count.                          |
| `node_max_size`       | `4`            | Max node count.                          |

## Production hardening checklist

- [ ] Restrict the public API endpoint or make it private.
- [ ] Add cluster logging (`enabled_cluster_log_types`).
- [ ] Move the EBS CSI driver to an IRSA-scoped role. The baseline
      attaches `AmazonEBSCSIDriverPolicy` to the *node* role (so PVCs bind
      out of the box); IRSA keeps those EC2 permissions off every node and
      scopes them to the driver's service account instead.
- [ ] Manage `master_key` in AWS Secrets Manager and mount via the chart.
- [ ] Pin add-on and AMI versions.
