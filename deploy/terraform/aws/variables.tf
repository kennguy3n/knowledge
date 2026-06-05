variable "region" {
  type        = string
  description = "AWS region to deploy into."
  default     = "us-east-1"
}

variable "name_prefix" {
  type        = string
  description = "Prefix applied to all resource names (cluster, roles, etc.)."
  default     = "knowledge"
}

variable "subnet_ids" {
  type        = list(string)
  description = "Existing subnet IDs for the EKS control plane and worker nodes (at least two AZs)."
}

variable "kubernetes_version" {
  type        = string
  description = "EKS control-plane Kubernetes version."
  default     = "1.30"
}

variable "node_instance_types" {
  type        = list(string)
  description = "EC2 instance types for the managed node group."
  default     = ["t3.large"]
}

variable "node_desired_size" {
  type        = number
  description = "Desired number of worker nodes."
  default     = 2
}

variable "node_min_size" {
  type        = number
  description = "Minimum number of worker nodes."
  default     = 2
}

variable "node_max_size" {
  type        = number
  description = "Maximum number of worker nodes."
  default     = 4
}

variable "tags" {
  type        = map(string)
  description = "Tags applied to all resources."
  default     = {}
}
