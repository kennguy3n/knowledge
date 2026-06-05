# Starting-point GKE Autopilot cluster for the Knowledge stack.
#
# Autopilot is used so Google manages node provisioning, scaling, and
# patching — the operator only manages the workload via the Helm chart in
# deploy/helm/knowledge (see README.md). Autopilot provisions block-backed
# PersistentVolumes on demand, which the stateful substrate requires.

locals {
  cluster_name = "${var.name_prefix}-gke"
}

resource "google_container_cluster" "this" {
  name     = local.cluster_name
  location = var.region

  # Autopilot mode: no explicit node pools to manage.
  enable_autopilot = true

  network    = var.network
  subnetwork = var.subnetwork

  release_channel {
    channel = var.release_channel
  }

  # Required for Autopilot clusters.
  ip_allocation_policy {}

  deletion_protection = var.deletion_protection
}
