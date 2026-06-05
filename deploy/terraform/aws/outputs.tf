output "cluster_name" {
  description = "EKS cluster name."
  value       = aws_eks_cluster.this.name
}

output "cluster_endpoint" {
  description = "EKS API server endpoint."
  value       = aws_eks_cluster.this.endpoint
}

output "cluster_certificate_authority" {
  description = "Base64 cluster CA certificate."
  value       = aws_eks_cluster.this.certificate_authority[0].data
}

output "kubeconfig_command" {
  description = "Command to configure kubectl against the new cluster."
  value       = "aws eks update-kubeconfig --region ${var.region} --name ${aws_eks_cluster.this.name}"
}
