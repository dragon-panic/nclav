# bootstrap/aws/outputs.tf

output "alb_url" {
  description = "Public HTTP URL of the nclav ALB. Set NCLAV_URL=http://{alb_url}."
  value       = "http://${aws_lb.nclav.dns_name}"
}

output "ecr_repository_url" {
  description = "ECR repository URL for pushing the nclav image. Used by 'make push-ecr'."
  value       = aws_ecr_repository.nclav.repository_url
}

output "api_token_secret_arn" {
  description = "ARN of the Secrets Manager secret holding the nclav API token. Used by 'make connect-aws'."
  value       = aws_secretsmanager_secret.api_token.arn
}

output "db_url_secret_arn" {
  description = "ARN of the Secrets Manager secret holding the PostgreSQL URL."
  value       = aws_secretsmanager_secret.db_url.arn
}

output "nclav_server_role_arn" {
  description = "ARN of the nclav-server IAM role. Pass this as --aws-role-arn when running nclav serve locally against the same org."
  value       = aws_iam_role.nclav_server.arn
}

output "iam_setup_commands" {
  description = "Instructions for granting nclav-server the AWS Organizations permissions it needs. Run these as your AWS management-account admin."
  value       = <<-EOT

    # ── Send to your AWS Organizations admin — run once after terraform apply ──
    # nclav-server needs Organizations permissions to create and manage accounts.
    # The ECS task role already has OrganizationsManagement + STS permissions
    # for the platform account, but cross-account Organizations actions require
    # that the nclav-server role is trusted by the management account's SCP.

    ROLE_ARN="${aws_iam_role.nclav_server.arn}"

    # If this is the Organizations management account, no extra steps are needed
    # (the task role policy in main.tf grants the required Organizations actions).

    # If you want to delegate a sub-account to run nclav with Organizations access,
    # attach a delegated administrator for Organizations:
    aws organizations register-delegated-administrator \
      --account-id ${data.aws_caller_identity.current.account_id} \
      --service-principal organizations.amazonaws.com

    # Verify the nclav-server role can assume OrganizationAccountAccessRole:
    aws sts assume-role \
      --role-arn "$ROLE_ARN" \
      --role-session-name test \
      --query 'Credentials.AccessKeyId'

  EOT
}

output "image_push_command" {
  description = "Commands to build and push the nclav image to ECR."
  value       = <<-EOT
    # Authenticate Docker to ECR:
    aws ecr get-login-password --region ${var.aws_region} | \
      docker login --username AWS --password-stdin ${aws_ecr_repository.nclav.repository_url}

    # Build and push from the repo root:
    make push-ecr AWS_ACCOUNT=${data.aws_caller_identity.current.account_id} AWS_REGION=${var.aws_region}
  EOT
}
