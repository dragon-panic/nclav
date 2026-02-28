# bootstrap/aws/variables.tf

# ── AWS region ─────────────────────────────────────────────────────────────────

variable "aws_region" {
  description = "AWS region for all platform resources. Must match --aws-default-region when running nclav serve."
  type        = string
  default     = "us-east-1"
}

# ── nclav AWS driver configuration ────────────────────────────────────────────

variable "aws_org_unit_id" {
  description = "AWS Organizations OU ID where new account enclaves will be placed. Format: 'ou-xxxx-yyyyyyyy'. Find it in the AWS Console under Organizations → Organizational units."
  type        = string
}

variable "aws_email_domain" {
  description = "Email domain for new account registration. New accounts get address: aws+{name}@{domain}. Must be a domain your organization controls."
  type        = string
}

variable "aws_account_prefix" {
  description = "Optional prefix prepended to every AWS account name. Avoids collisions: 'myorg' + 'product-a-dev' → 'myorg-product-a-dev'."
  type        = string
  default     = ""
}

variable "aws_cross_account_role" {
  description = "IAM role name to assume in each enclave account. Defaults to OrganizationAccountAccessRole (auto-created by AWS Organizations)."
  type        = string
  default     = "OrganizationAccountAccessRole"
}

# ── Image configuration ────────────────────────────────────────────────────────

variable "nclav_image_tag" {
  description = "Tag of the nclav container image to deploy. Use 'latest' for the most recent build."
  type        = string
  default     = "latest"
}

variable "nclav_image" {
  description = "Full container image URI for the nclav API server. Leave empty to use the ECR repo created by this module ({account}.dkr.ecr.{region}.amazonaws.com/nclav:{tag})."
  type        = string
  default     = ""
}

# ── Infrastructure sizing ──────────────────────────────────────────────────────

variable "rds_instance_class" {
  description = "RDS instance class for the PostgreSQL state store. db.t4g.micro is the cheapest option for low traffic."
  type        = string
  default     = "db.t4g.micro"
}

variable "task_cpu" {
  description = "ECS Fargate task CPU units (256 = 0.25 vCPU, 512 = 0.5 vCPU)."
  type        = number
  default     = 256
}

variable "task_memory" {
  description = "ECS Fargate task memory in MiB."
  type        = number
  default     = 512
}
