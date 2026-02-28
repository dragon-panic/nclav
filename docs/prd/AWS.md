# AWS Driver Reference

This document describes how the `AwsDriver` maps nclav concepts to AWS primitives, the API call sequences for each driver method, handle shapes, idempotency strategy, and how to bootstrap nclav itself on AWS.

---

## Concept → AWS Primitive Mapping

| nclav concept | AWS primitive |
|---|---|
| `Enclave` | AWS **Account** (created via Organizations `CreateAccount`, placed in an OU) |
| `Enclave.identity` | **IAM Role** (`{identity}` in the enclave account) |
| `Partition` (driver-managed) | **IAM Role** (`nclav-partition-{id}` in the enclave account) |
| `Enclave.network` | **VPC** + **Subnets** (created via EC2 in enclave account) |
| `Enclave.dns.zone` | **Route53 Private Hosted Zone** |
| `Export` / `Import` | Terraform-managed (VPC Endpoint Services, SQS, etc.) |
| Cross-account access | STS `AssumeRole` via `OrganizationAccountAccessRole` |

---

## Auth Modes

Credential resolution order at driver startup (mirrors the AWS SDK credential chain):

1. **Env vars** — `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY` (+ optional `AWS_SESSION_TOKEN`)
2. **ECS task credentials** — `AWS_CONTAINER_CREDENTIALS_RELATIVE_URI` → `http://169.254.170.2{uri}`
3. **EC2 IMDSv2** — probe `http://169.254.169.254/latest/api/token` with a 2-second timeout
4. **AWS CLI fallback** — `aws sts get-session-token --duration-seconds 3600`

For management-account calls (Organizations, STS global), the selected provider is used directly.
For enclave-account calls (EC2, IAM, Route53), the driver first assumes `OrganizationAccountAccessRole`
(or the configured `cross_account_role`) via STS.

---

## SigV4 Signing Overview

All AWS API calls use SigV4 request signing. The driver implements signing manually (consistent with the GCP and Azure drivers which also avoid the official SDKs):

1. SHA-256 hash the request body
2. Build a canonical request (method, URI, query string, sorted canonical headers, signed-headers list, body hash)
3. Build the string-to-sign: `AWS4-HMAC-SHA256\n{timestamp}\n{scope}\n{hash-of-canonical-request}`
4. Derive signing key: HMAC chain `AWS4{secret}` → date → region → service → `"aws4_request"`
5. Compute signature: HMAC-SHA256(signing-key, string-to-sign) → hex
6. Inject `Authorization` header

**Dependencies added:** `hmac = "0.12"` (HMAC-SHA256); `sha2 = "0.10"` already in workspace; `quick-xml = "0.37"` for XML response parsing.

### API Protocols

| Service | Protocol | Content-Type | Target header |
|---|---|---|---|
| EC2, IAM, STS | Query/XML | `application/x-www-form-urlencoded` | n/a |
| Organizations, ResourceGroupsTagging | JSON/Target | `application/x-amz-json-1.1` | `X-Amz-Target: Service_Date.Operation` |
| Route53 | REST/XML | `text/xml` | n/a |

---

## Driver Method Specifications

### `provision_enclave`

**Idempotency guard:** returns early if `provisioning_complete == true` in the existing handle.

**API call sequence:**

1. `Organizations:CreateAccount(AccountName, Email)` → `CreateAccountRequestId`
   - Account name: `{account_prefix}-{enclave_id}` sanitized (max 50 chars, alphanumeric + space + hyphen)
   - Email: `aws+{account-name-no-spaces}@{email_domain}`
2. Poll `Organizations:DescribeCreateAccountStatus` until `State == SUCCEEDED` (max 120 polls × `[1,2,4,8,16,30]s` backoff; accounts take 5–15 min; INFO log every 10 polls)
3. `Organizations:ListParents(ChildId=AccountId)` → current parent (root ID)
4. `Organizations:MoveAccount(AccountId, SourceParentId=root, DestinationParentId=org_unit_id)`; `DuplicateAccountException` → success (idempotent)
5. `STS:AssumeRole(RoleArn=arn:aws:iam::{account_id}:role/{cross_account_role}, RoleSessionName=nclav-session)` → temporary credentials
6. Using enclave credentials:
   - `EC2:CreateVpc(CidrBlock)` + `EC2:ModifyVpcAttribute` (enable DNS hostnames + resolution)
   - For each `network.subnets` entry: `EC2:CreateSubnet(VpcId, CidrBlock)`
   - If `dns.zone` set: `Route53:CreateHostedZone` (private, associated with VPC)
   - If `identity` set: `IAM:CreateRole({identity})` with trust policy for the nclav-server's role ARN
7. Stamp `provisioning_complete: true` in handle

**Enclave handle shape:**
```json
{
  "driver": "aws",
  "kind": "enclave",
  "account_id": "123456789012",
  "account_name": "myorg-product-a-dev",
  "region": "us-east-1",
  "vpc_id": "vpc-0123456789abcdef0",
  "subnet_ids": ["subnet-abc"],
  "route53_zone_id": "Z1234567890ABC",
  "identity_role_arn": "arn:aws:iam::123456789012:role/nclav-identity",
  "provisioning_complete": true
}
```

---

### `teardown_enclave`

`Organizations:CloseAccount(AccountId)` — soft-delete with 90-day hold. AWS suspends the account immediately but retains it for 90 days before final deletion.

- `AccountNotFoundException` → log warning and succeed (idempotent)

---

### `provision_partition`

**Idempotency guard:** returns early if handle has `driver == "aws"` and `kind == "partition"`.

**API call sequence:**

1. Determine enclave `account_id` from `resolved_inputs["nclav_account_id"]` (injected by reconciler via `context_vars`)
2. `STS:AssumeRole` → temporary credentials for enclave account
3. `IAM:CreateRole(RoleName=nclav-partition-{id})` with trust policy allowing the nclav-server role ARN
   - `EntityAlreadyExists` → `IAM:GetRole` to retrieve existing ARN (idempotent)
4. `IAM:AttachRolePolicy(RoleName, PolicyArn=arn:aws:iam::aws:policy/AdministratorAccess)`
5. `IAM:TagRole` — tags: `nclav-managed=true`, `nclav-enclave={id}`, `nclav-partition={id}`

**Role name:** `nclav-partition-{partition_id}` (truncated + 8-char hex hash of partition_id if > 64 chars).

**Partition handle shape:**
```json
{
  "driver": "aws",
  "kind": "partition",
  "type": "iac",
  "account_id": "123456789012",
  "partition_role_arn": "arn:aws:iam::123456789012:role/nclav-partition-api"
}
```

---

### `teardown_partition`

1. `STS:AssumeRole` in enclave account
2. `IAM:ListAttachedRolePolicies` → `IAM:DetachRolePolicy` for each
3. `IAM:ListRolePolicies` → `IAM:DeleteRolePolicy` for each inline policy
4. `IAM:DeleteRole` — `NoSuchEntity` → success (idempotent)

---

### `provision_export` / `provision_import`

No direct AWS API calls. Resources (VPC Endpoint Services, SQS queues, etc.) are Terraform-managed inside partitions.

- `provision_export`: reads `partition_outputs` (Terraform outputs), constructs the export handle
- `provision_import`: reads export handle, returns import outputs

---

### `observe_enclave`

`Organizations:DescribeAccount(AccountId)` — maps `Status` field:
- `ACTIVE` → `exists=true, healthy=true`
- `SUSPENDED` → `exists=true, healthy=false`
- `AccountNotFoundException` → `exists=false, healthy=false`

---

### `observe_partition`

Returns `exists=true, healthy=true` if handle has `driver=="aws"` and `kind=="partition"`.

---

## `context_vars` Reference

Injected into `nclav_context.auto.tfvars` for Terraform runs within each partition.

| Variable | Value | Notes |
|---|---|---|
| `nclav_project_id` | `account_id` | GCP-compat alias for modules that use this name |
| `nclav_region` | `region` | AWS region from enclave handle |
| `nclav_account_id` | `account_id` | AWS account ID of the enclave |
| `nclav_role_arn` | `partition_role_arn` | IAM role ARN for the partition |
| `nclav_enclave` | enclave ID | nclav enclave ID |

---

## `auth_env` Reference

Environment variables set on the Terraform subprocess for AWS authentication.

| Variable | Value | Purpose |
|---|---|---|
| `AWS_DEFAULT_REGION` | region | Sets the default region for the AWS Terraform provider |
| `AWS_ROLE_ARN` | `partition_role_arn` | Terraform provider automatically assumes this role |

Terraform's AWS provider follows the same credential chain and then assumes `AWS_ROLE_ARN` automatically. No static credential injection occurs.

---

## `AwsDriverConfig` Fields and CLI Flags

| Field | CLI flag | Env var | Required | Default | Description |
|---|---|---|---|---|---|
| `org_unit_id` | `--aws-org-unit-id` | `NCLAV_AWS_ORG_UNIT_ID` | ✓ | | OU ID where new account enclaves are placed |
| `email_domain` | `--aws-email-domain` | `NCLAV_AWS_EMAIL_DOMAIN` | ✓ | | Email domain for new account registration |
| `default_region` | `--aws-default-region` | `NCLAV_AWS_DEFAULT_REGION` | | `us-east-1` | Default AWS region |
| `account_prefix` | `--aws-account-prefix` | `NCLAV_AWS_ACCOUNT_PREFIX` | | | Prefix for account names |
| `cross_account_role` | `--aws-cross-account-role` | `NCLAV_AWS_CROSS_ACCOUNT_ROLE` | | `OrganizationAccountAccessRole` | IAM role assumed in enclave accounts |
| `role_arn` | `--aws-role-arn` | `NCLAV_AWS_ROLE_ARN` | | | IAM role ARN assumed for management API calls |

---

## Orphan Detection

Uses the **AWS Resource Groups Tagging API** (`ResourceGroupsTaggingAPI_20170126.GetResources`).

All nclav-managed resources are tagged at creation:
- `nclav-managed=true`
- `nclav-enclave={enclave_id}`
- `nclav-partition={partition_id}` (partitions only)

**`list_partition_resources`**: GetResources with filters `nclav-managed=true`, `nclav-partition={id}`, `nclav-enclave={enc_id}` — called in the enclave account via assumed role.

**`list_orphaned_resources`**: GetResources with `nclav-managed=true`, `nclav-enclave={enc_id}` — returns resources whose `nclav-partition` tag doesn't match any known partition ID.

---

## Idempotency Strategy

| Operation | Idempotency mechanism |
|---|---|
| `provision_enclave` | `provisioning_complete` flag in handle |
| `CreateAccount` | DuplicateAccountException → error with recovery instructions |
| `MoveAccount` | DuplicateAccountException → treated as success |
| `CreateRole` | EntityAlreadyExists → GetRole to retrieve ARN |
| `AttachRolePolicy` | Idempotent by AWS |
| `provision_partition` | handle `kind=="partition"` check |
| `teardown_partition` | NoSuchEntity → success |
| `teardown_enclave` | AccountNotFoundException → success |

---

## Bootstrap

See [bootstrap/aws/](../../bootstrap/aws/) for the Terraform module that deploys nclav to AWS ECS Fargate.

**What the bootstrap module creates:**

| Resource | Name | Purpose |
|---|---|---|
| VPC (public, 2 AZs) | `nclav-vpc` | Networking for ECS + RDS |
| Subnets ×2 | `nclav-public-{az}` | ECS tasks + ALB (public, `assign_public_ip=ENABLED`) |
| Internet Gateway | `nclav-igw` | Outbound AWS API access (no NAT Gateway) |
| Security Groups | `nclav-alb-sg`, `nclav-ecs-sg`, `nclav-rds-sg` | Traffic control |
| ECR Repository | `nclav` | Container image hosting |
| RDS PostgreSQL | `nclav-state-{suffix}` | Persistent state store |
| Secrets Manager | `nclav-api-token` | Bearer token for CLI |
| Secrets Manager | `nclav-db-url` | Postgres URL for ECS task |
| IAM Role | `nclav-server` | ECS task role (Organizations + STS + Secrets Manager) |
| ECS Cluster | `nclav` | Fargate compute |
| ECS Task Definition | `nclav-api` | Container spec |
| ECS Service | `nclav-api` | Running task |
| ALB | `nclav-api` | Public HTTP endpoint |
| ALB Target Group | `nclav-api` | Routes to ECS task port 8080 |

**Quick start:**
```bash
cp bootstrap/aws/terraform.tfvars.example bootstrap/aws/terraform.tfvars
# fill in aws_org_unit_id, aws_email_domain, aws_account_prefix
make bootstrap-aws AWS_ACCOUNT=123456789012
eval $(make connect-aws)
nclav status
```

---

## Out of Scope

- **GovCloud / AWS China regions** — endpoint URLs differ; future work
- **AWS Control Tower** — account vending via Control Tower is a different flow
- **AWS IAM Identity Center (SSO)** — authentication via SSO is not supported; use env vars or IMDS
- **Multi-region partitions** — each enclave has one region; multi-region is a future mode
- **AWS Service Control Policies** — SCPs are not managed by nclav; apply them separately
