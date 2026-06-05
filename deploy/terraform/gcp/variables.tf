variable "project_id" {
  type        = string
  description = "GCP project ID to deploy into."
}

variable "region" {
  type        = string
  description = "GCP region for the GKE Autopilot cluster."
  default     = "us-central1"
}

variable "name_prefix" {
  type        = string
  description = "Prefix applied to resource names."
  default     = "knowledge"
}

variable "network" {
  type        = string
  description = "VPC network self-link or name to attach the cluster to."
  default     = "default"
}

variable "subnetwork" {
  type        = string
  description = "Subnetwork self-link or name for the cluster nodes."
  default     = "default"
}

variable "release_channel" {
  type        = string
  description = "GKE release channel (RAPID, REGULAR, or STABLE)."
  default     = "REGULAR"

  validation {
    condition     = contains(["RAPID", "REGULAR", "STABLE"], var.release_channel)
    error_message = "release_channel must be one of RAPID, REGULAR, or STABLE."
  }
}

variable "deletion_protection" {
  type        = bool
  description = "Guard against accidental cluster deletion. Set false to allow `terraform destroy`."
  default     = true
}
