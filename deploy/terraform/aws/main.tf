# Starting-point EKS cluster for the Knowledge stack.
#
# This module provisions the *cluster* (control plane + a managed node
# group + the IAM roles they require). The application itself is then
# deployed with the Helm chart in deploy/helm/knowledge — see README.md.
#
# EKS is chosen over ECS because the substrate is a stateful service
# backed by a SQLCipher database that needs block storage with proper
# POSIX/locking semantics (EBS via the chart's PVC), which ECS+EFS does
# not provide cleanly for SQLite.

locals {
  cluster_name = "${var.name_prefix}-eks"

  tags = merge({
    "app.kubernetes.io/part-of" = "knowledge"
    "managed-by"                = "terraform"
  }, var.tags)
}

# ─────────────────────────── Control plane ──────────────────────────
resource "aws_eks_cluster" "this" {
  name     = local.cluster_name
  role_arn = aws_iam_role.cluster.arn
  version  = var.kubernetes_version

  vpc_config {
    subnet_ids = var.subnet_ids
  }

  # The node-group/cluster roles must exist before the cluster is torn
  # down; declaring the dependency keeps destroy ordering correct.
  depends_on = [
    aws_iam_role_policy_attachment.cluster_eks,
  ]

  tags = local.tags
}

# ────────────────────────── Managed nodes ───────────────────────────
resource "aws_eks_node_group" "default" {
  cluster_name    = aws_eks_cluster.this.name
  node_group_name = "${var.name_prefix}-ng"
  node_role_arn   = aws_iam_role.node.arn
  subnet_ids      = var.subnet_ids
  instance_types  = var.node_instance_types

  scaling_config {
    desired_size = var.node_desired_size
    min_size     = var.node_min_size
    max_size     = var.node_max_size
  }

  depends_on = [
    aws_iam_role_policy_attachment.node_worker,
    aws_iam_role_policy_attachment.node_cni,
    aws_iam_role_policy_attachment.node_ecr,
  ]

  tags = local.tags
}

# EBS CSI driver — required for the substrate's PersistentVolumeClaim to
# bind to a gp3 volume. Installed as a managed add-on.
resource "aws_eks_addon" "ebs_csi" {
  cluster_name = aws_eks_cluster.this.name
  addon_name   = "aws-ebs-csi-driver"

  depends_on = [aws_eks_node_group.default]

  tags = local.tags
}
