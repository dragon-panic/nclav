# bootstrap/aws — Deploy nclav to AWS ECS Fargate

This Terraform module provisions the nclav API server on AWS as an ECS Fargate service, backed by RDS PostgreSQL for state and an Application Load Balancer for HTTP access.

## Prerequisites

- AWS CLI configured (`aws configure` or environment variables)
- AWS Organizations: this module must be applied from the **management account** (or a delegated administrator account with Organizations permissions)
- Terraform ≥ 1.9 or OpenTofu ≥ 1.8
- Docker (for building and pushing the nclav image)
- An OU ID where new account enclaves will be placed
- A domain you control for new account email registration

## Architecture

```text
Internet
    │
    ▼
ALB (port 80)
    │
    ▼
ECS Fargate (nclav-api, public subnet, assign_public_ip=ENABLED)
    │           │
    │           └── SG-to-SG → RDS PostgreSQL (private, same VPC)
    │
    └── IAM role (nclav-server)
            ├── organizations:CreateAccount / MoveAccount / ...
            └── sts:AssumeRole → OrganizationAccountAccessRole in enclave accounts
```

**No NAT Gateway required.** ECS tasks run in public subnets and reach AWS APIs directly via the Internet Gateway. RDS is only reachable within the VPC via the ECS security group.

## Quick start

```bash
# 1. Copy and fill in the tfvars file
cp terraform.tfvars.example terraform.tfvars
# edit terraform.tfvars with your values

# 2. Run the full bootstrap (two-phase: ECR first, then ECS + RDS)
make bootstrap-aws AWS_ACCOUNT=123456789012

# 3. Configure your nclav CLI
eval $(make connect-aws)
nclav status
```

## Two-phase bootstrap

Phase 1 creates the ECR repository so you can push the image before deploying the ECS service:

```bash
cd bootstrap/aws && terraform init
cd bootstrap/aws && terraform apply -target=aws_ecr_repository.nclav
make push-ecr AWS_ACCOUNT=123456789012
cd bootstrap/aws && terraform apply
```

`make bootstrap-aws` runs all these steps automatically.

## Variables

| Variable | Required | Default | Description |
|---|---|---|---|
| `aws_region` | | `us-east-1` | AWS region for all platform resources |
| `aws_org_unit_id` | ✓ | | OU ID where enclave accounts are placed |
| `aws_email_domain` | ✓ | | Email domain for new account registration |
| `aws_account_prefix` | | `""` | Prefix prepended to account names |
| `aws_cross_account_role` | | `OrganizationAccountAccessRole` | Role assumed in enclave accounts |
| `nclav_image_tag` | | `latest` | Tag of the nclav container image |
| `nclav_image` | | *(ECR URL)* | Override full image URI |
| `rds_instance_class` | | `db.t4g.micro` | RDS PostgreSQL instance class |
| `task_cpu` | | `256` | ECS task CPU (256 = 0.25 vCPU) |
| `task_memory` | | `512` | ECS task memory (MiB) |

## Connecting

```bash
# Print and export env vars for the nclav CLI
eval $(make connect-aws)

# Verify the server is reachable
nclav status

# Apply enclave configs
nclav apply ./enclaves
```

`make connect-aws` fetches the API token from Secrets Manager and the ALB URL from Terraform output.

## IAM setup

The `nclav-server` ECS task role has the following permissions:

- **Organizations**: `CreateAccount`, `DescribeCreateAccountStatus`, `ListParents`, `MoveAccount`, `DescribeAccount`, `CloseAccount` — required for account lifecycle management
- **STS**: `AssumeRole` on `arn:aws:iam::*:role/OrganizationAccountAccessRole` — for cross-account operations
- **Secrets Manager**: Read the API token and DB URL secrets

After `terraform apply`, check the `iam_setup_commands` output for any additional steps required:

```bash
terraform output iam_setup_commands
```

## Token rotation

The API token is stored in Secrets Manager (`nclav-api-token`) and injected into the ECS task at startup via `NCLAV_TOKEN`. To rotate the token:

```bash
# Generate a new token
NEW_TOKEN=$(openssl rand -hex 32)

# Update the secret
aws secretsmanager put-secret-value \
  --secret-id nclav-api-token \
  --secret-string "$NEW_TOKEN"

# Force a new ECS task deployment (picks up the new secret)
aws ecs update-service \
  --cluster nclav \
  --service nclav-api \
  --force-new-deployment
```

## Tear down

```bash
cd bootstrap/aws && terraform destroy
```

Note: RDS has `skip_final_snapshot = true` and `deletion_protection = false` for easy teardown in dev environments. For production, set both to their secure defaults.

## Cost estimate

At minimum configuration (`us-east-1`):

| Resource | Monthly cost (approx.) |
|---|---|
| ECS Fargate (0.25 vCPU / 0.5 GB, always-on) | ~$7 |
| RDS PostgreSQL (db.t4g.micro, 20 GB gp3) | ~$15 |
| ALB | ~$16 + data transfer |
| ECR storage | ~$0.10/GB |
| Secrets Manager | ~$0.40/secret |
| **Total** | **~$38–40/month** |

No NAT Gateway saves ~$32/month vs. the typical VPC setup.
