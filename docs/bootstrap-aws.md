# Hosted deployment — AWS

Run the nclav API as an ECS Fargate service backed by RDS PostgreSQL and an Application Load Balancer. No local process required after setup. The Terraform bootstrap lives in `bootstrap/aws/`.

**What it creates:**

| Resource | Name | Purpose |
|---|---|---|
| ECR Repository | `nclav` | Hosts the nclav container image |
| RDS PostgreSQL | `nclav-state-{suffix}` | Persistent state store |
| Secrets Manager | `nclav-api-token` | Bearer token for CLI authentication |
| Secrets Manager | `nclav-db-url` | Postgres connection URL for ECS task |
| IAM Role | `nclav-server` | ECS task role (Organizations + STS + Secrets Manager) |
| ECS Cluster + Service | `nclav` / `nclav-api` | Fargate compute |
| ALB | `nclav-api` | Public HTTP endpoint |

**No NAT Gateway required.** ECS tasks run in public subnets with `assign_public_ip=ENABLED` and reach AWS APIs directly via the Internet Gateway. This saves ~$32/month versus a typical VPC setup.

## Prerequisites

- AWS CLI configured (`aws configure` or environment variables)
- AWS Organizations: must be run from the **management account** (or a delegated admin with Organizations permissions)
- An OU ID where new account enclaves will be placed
- A domain you control for new account email registration
- Terraform or OpenTofu installed locally
- Docker installed (to build and push the nclav image)

## Step 1 — Configure and deploy

Copy the vars template and fill in your values:

```bash
cp bootstrap/aws/terraform.tfvars.example bootstrap/aws/terraform.tfvars
# edit bootstrap/aws/terraform.tfvars
```

Then run the full bootstrap in one command:

```bash
make bootstrap-aws AWS_ACCOUNT=123456789012
```

This creates the ECR repository, builds and pushes the container image, then deploys the ECS service, RDS database, and ALB. Terraform will prompt for plan approval twice.

**Doing it manually** (if you prefer to see each step):

```bash
cd bootstrap/aws
terraform init
# Phase 1: ECR repo (image must exist before ECS service starts)
terraform apply -target=aws_ecr_repository.nclav
# Build and push the image
cd ../..
make push-ecr AWS_ACCOUNT=123456789012
# Phase 2: everything else
cd bootstrap/aws
terraform apply
```

## Step 2 — Grant IAM permissions

The `nclav-server` ECS task role is created by Terraform with the permissions needed for account lifecycle management. After `terraform apply`, check the IAM setup output:

```bash
cd bootstrap/aws && terraform output iam_setup_commands
```

This shows any additional steps needed (e.g. enabling Organizations policy types). **If you are the AWS org admin, run them yourself.** Otherwise send them to whoever manages your AWS organization. These are one-time grants and do not need to be repeated per enclave.

## Step 3 — Connect the CLI

The ALB exposes a public HTTP endpoint secured by the nclav bearer token. One command sets the required env vars:

```bash
eval $(make connect-aws)
nclav status
nclav apply enclaves/
```

`make connect-aws` fetches the ALB URL and API token from Terraform outputs and Secrets Manager. Add the exported vars to your shell profile:

```bash
export NCLAV_URL=http://nclav-api-123456789.us-east-1.elb.amazonaws.com
export NCLAV_TOKEN=$(aws secretsmanager get-secret-value \
  --secret-id nclav-api-token --query SecretString --output text)
```

## Token rotation

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

# Export the new token
export NCLAV_TOKEN=$NEW_TOKEN
```
