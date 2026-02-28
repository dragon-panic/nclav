# nclav

Cloud infrastructure orchestration via a YAML-driven reconcile loop. nclav manages **enclaves** — isolated network/cost/identity boundaries — and the **partitions** (infrastructure units) inside them. Dependencies between enclaves are declared explicitly through import/export contracts and validated before anything is provisioned.

## Concepts

| Term | Meaning |
|---|---|
| **Enclave** | An isolated boundary (VPC, subscription, cost center). Owns its partitions and declares what it exports to other enclaves. |
| **Partition** | A deployable unit inside an enclave (a service, a database, a queue). Backed by Terraform or OpenTofu; nclav creates the partition service account and runs the IaC tool. |
| **Export** | A named endpoint an enclave makes available to others, with type (`http`, `tcp`, `queue`) and access control (`to:`). |
| **Import** | A reference to another enclave's export, given a local alias used for template substitution. |
| **Reconcile** | The loop that diffs desired state (YAML) against actual state (store), then provisions/tears down in dependency order. |

> For the theory behind the isolation model, state machine, driver architecture, and design decisions, see the [design documents](docs/prd/README.md).

## Requirements

- Rust 1.75+ (the workspace uses async traits and the 2021 edition)
- `cargo` on your PATH
- No cloud credentials required for local mode
- `terraform` or `tofu` on your PATH only if you use IaC-backed partitions

## Quick start

```bash
git clone <repo>
cd nclav

# Build everything
cargo build --workspace

# Start the HTTP API server — binds 127.0.0.1:8080, persists state to ~/.nclav/state.redb
# Prints a bearer token and writes it to ~/.nclav/token on first run
cargo run -p nclav-cli -- serve --cloud local

# In another terminal — see what would change (token read automatically from ~/.nclav/token)
cargo run -p nclav-cli -- diff ./enclaves

# Apply changes (provisions resources via the running server)
cargo run -p nclav-cli -- apply ./enclaves

# Render the dependency graph (fetched from the running server)
cargo run -p nclav-cli -- graph --output text
```

## Hosted deployment (GCP)

For a persistent, cloud-hosted nclav API run the Terraform bootstrap in
`bootstrap/gcp/`. This provisions a Cloud Run service backed by Cloud SQL — no
local process required after setup.

**What it creates:**

| Resource | Name | Purpose |
|---|---|---|
| Service Account | `nclav-server@{project}.iam.gserviceaccount.com` | Identity for the Cloud Run service |
| Cloud SQL (Postgres 16) | `nclav-state` | Persistent state store |
| Secret Manager secret | `nclav-api-token` | Bearer token for CLI authentication |
| Secret Manager secret | `nclav-db-url` | Postgres connection URL for the Cloud Run service |
| Cloud Run service | `nclav-api` | The nclav API server |

### Prerequisites

- A GCP folder or organization under which enclave projects will be created
- A billing account for enclave projects
- A GCP project for the nclav platform (not an enclave project — a dedicated ops project).
  Create one if you don't have it yet:
  ```bash
  gcloud projects create myorg-nclav --organization=ORGANIZATION_ID
  gcloud billing projects link myorg-nclav --billing-account=XXXXXX-YYYYYY-ZZZZZZ
  ```
- Terraform or OpenTofu installed locally
- ADC with billing scope:
  ```bash
  gcloud auth application-default login \
    --scopes=https://www.googleapis.com/auth/cloud-platform,https://www.googleapis.com/auth/cloud-billing
  ```
### Step 1 — Configure and deploy

Copy the vars template and fill in your values:

```bash
cp bootstrap/gcp/terraform.tfvars.example bootstrap/gcp/terraform.tfvars
# edit bootstrap/gcp/terraform.tfvars
```

Then run the full bootstrap in one command:

```bash
make bootstrap-gcp GCP_PROJECT=my-platform-project
```

This handles the two-phase Terraform apply (APIs + AR repo first, then Cloud Run + Cloud SQL) and builds and pushes the container image in between. Terraform will prompt for plan approval twice.

**Doing it manually** (if you prefer to see each step):

```bash
cd bootstrap/gcp
terraform init
# Phase 1: AR repo + APIs
terraform apply -target=google_artifact_registry_repository.nclav \
                -target=google_project_service.apis
# Build and push the image
cd ../..
make push-ar GCP_PROJECT=my-platform-project AR_REGION=us-central1
# Phase 2: everything else
cd bootstrap/gcp
terraform apply
```

### Step 2 — Grant folder-level IAM

Terraform cannot grant folder/org-level IAM without admin credentials. After
`terraform apply`, print the required `gcloud` commands:

```bash
cd bootstrap/gcp && terraform output -raw iam_setup_commands
```

These grant `nclav-server` the roles it needs (`projectCreator`,
`iam.serviceAccountAdmin`, `compute.networkAdmin`, etc.) at the folder or org
level, plus `billing.user` on the billing account. **If you are the org admin,
run them yourself.** Otherwise send them to whoever manages your GCP org.
These are one-time grants and do not need to be repeated per enclave.

### Step 3 — Connect the CLI

The Cloud Run service is private by default. One command starts the proxy and
sets the required env vars:

```bash
eval $(make connect GCP_PROJECT=my-platform-project)
nclav status
nclav apply enclaves/
```

`make connect` runs `gcloud run services proxy` in the background (logs to
`/tmp/nclav-proxy.log`) and fetches the API token from Secret Manager. Add the
exported vars to your shell profile to avoid re-running after the proxy is
already up:

```bash
export NCLAV_URL=http://localhost:8080
export NCLAV_TOKEN=$(gcloud secrets versions access latest \
  --secret=nclav-api-token --project=my-platform-project)
```

To stop the proxy: `pkill -f 'gcloud run services proxy'`

### Token rotation

```bash
cd bootstrap/gcp

# Generate a new token and update the Secret Manager version
terraform apply -replace=random_bytes.api_token

# Cloud Run reads secrets at revision creation time, so force a new revision.
# Updating an env var is the standard way to trigger this without redeploying.
gcloud run services update nclav-api \
  --project=my-platform-project \
  --region=us-central1 \
  --update-env-vars=_RESTART=$(date +%s)

# Fetch the new token
export NCLAV_TOKEN=$(gcloud secrets versions access latest \
  --secret=nclav-api-token --project=my-platform-project)
```

---

## Hosted deployment (Azure)

For a persistent, cloud-hosted nclav API run the Terraform bootstrap in
`bootstrap/azure/`. This provisions a Container App backed by PostgreSQL
Flexible Server — no local process required after setup.

**What it creates:**

| Resource | Name | Purpose |
|---|---|---|
| User-Assigned Identity | `nclav-server` | Identity for the Container App |
| Container Registry | `{acr_name}` | Hosts the nclav container image |
| Key Vault | `{key_vault_name}` | Bearer token for CLI authentication |
| PostgreSQL Flexible Server | `nclav-state-{suffix}` | Persistent state store |
| Container App | `nclav-api` | The nclav API server |

### Prerequisites

- An Azure subscription for the nclav platform (dedicated ops subscription)
- A management group under which subscription enclaves will be created
- An MCA billing account with a billing profile and invoice section
- Azure CLI installed and authenticated (`az login`)
- Terraform or OpenTofu installed locally
- Docker installed (to push the nclav image to ACR)
- Required resource providers registered:
  ```bash
  az provider register --namespace Microsoft.App
  az provider register --namespace Microsoft.DBforPostgreSQL
  az provider register --namespace Microsoft.KeyVault
  ```

### Step 1 — Configure and deploy

Copy the vars template and fill in your values:

```bash
cp bootstrap/azure/terraform.tfvars.example bootstrap/azure/terraform.tfvars
# edit bootstrap/azure/terraform.tfvars
```

Then run the full bootstrap in one command:

```bash
make bootstrap-azure AZURE_ACR=myorgnclav
```

This creates the ACR repo, builds and pushes the container image, then deploys
the PostgreSQL server, Key Vault, and Container App. Terraform will prompt for
plan approval twice.

**Doing it manually** (if you prefer to see each step):

```bash
cd bootstrap/azure
terraform init
# Phase 1: ACR repo (image must exist before Container App starts)
terraform apply -target=azurerm_resource_group.nclav \
                -target=azurerm_container_registry.nclav
# Build and push the image
cd ../..
make push-acr AZURE_ACR=myorgnclav
# Phase 2: everything else
cd bootstrap/azure
terraform apply
```

### Step 2 — Grant management group IAM

Terraform cannot grant management group or billing roles without admin
credentials. After `terraform apply`, print the required `az` commands:

```bash
cd bootstrap/azure && terraform output -raw iam_setup_commands
```

These grant `nclav-server` the roles it needs (`Owner` on the management group
and `Invoice Section Contributor` on the MCA invoice section). **If you are the
Azure admin, run them yourself.** Otherwise send them to whoever manages your
Azure tenant. These are one-time grants and do not need to be repeated per enclave.

### Step 3 — Connect the CLI

The Container App exposes a public HTTPS endpoint secured by the nclav Bearer
token. One command sets the required env vars:

```bash
eval $(make connect-azure)
nclav status
nclav apply enclaves/
```

`make connect-azure` fetches the app URL and API token from Terraform outputs
and Key Vault. Add the exported vars to your shell profile:

```bash
export NCLAV_URL=https://nclav-api.{region}.azurecontainerapps.io
export NCLAV_TOKEN=$(az keyvault secret show \
  --vault-name myorg-nclav-kv --name nclav-api-token --query value -o tsv)
```

### Token rotation

```bash
cd bootstrap/azure

# Generate a new token (updates Key Vault secret and ACA secret)
terraform apply -replace=random_bytes.api_token

# Fetch the new token
eval $(make connect-azure)
```

---

## Hosted deployment (AWS)

For a persistent, cloud-hosted nclav API run the Terraform bootstrap in
`bootstrap/aws/`. This provisions an ECS Fargate service backed by RDS
PostgreSQL and an Application Load Balancer — no local process required after
setup.

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

**No NAT Gateway required.** ECS tasks run in public subnets with
`assign_public_ip=ENABLED` and reach AWS APIs directly via the Internet
Gateway. This saves ~$32/month versus a typical VPC setup.

### Prerequisites

- AWS CLI configured (`aws configure` or environment variables)
- AWS Organizations: must be run from the **management account** (or a delegated
  admin with Organizations permissions)
- An OU ID where new account enclaves will be placed
- A domain you control for new account email registration
- Terraform or OpenTofu installed locally
- Docker installed (to build and push the nclav image)

### Step 1 — Configure and deploy

Copy the vars template and fill in your values:

```bash
cp bootstrap/aws/terraform.tfvars.example bootstrap/aws/terraform.tfvars
# edit bootstrap/aws/terraform.tfvars
```

Then run the full bootstrap in one command:

```bash
make bootstrap-aws AWS_ACCOUNT=123456789012
```

This creates the ECR repository, builds and pushes the container image, then
deploys the ECS service, RDS database, and ALB. Terraform will prompt for plan
approval twice.

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

### Step 2 — Grant IAM permissions

The `nclav-server` ECS task role is created by Terraform with the permissions
needed for account lifecycle management. After `terraform apply`, check the
IAM setup output:

```bash
cd bootstrap/aws && terraform output iam_setup_commands
```

This shows any additional steps needed (e.g. enabling Organizations policy
types). **If you are the AWS org admin, run them yourself.** Otherwise send
them to whoever manages your AWS organization. These are one-time grants and
do not need to be repeated per enclave.

### Step 3 — Connect the CLI

The ALB exposes a public HTTP endpoint secured by the nclav bearer token. One
command sets the required env vars:

```bash
eval $(make connect-aws)
nclav status
nclav apply enclaves/
```

`make connect-aws` fetches the ALB URL and API token from Terraform outputs and
Secrets Manager. Add the exported vars to your shell profile:

```bash
export NCLAV_URL=http://nclav-api-123456789.us-east-1.elb.amazonaws.com
export NCLAV_TOKEN=$(aws secretsmanager get-secret-value \
  --secret-id nclav-api-token --query SecretString --output text)
```

### Token rotation

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

---

## CLI reference

```
nclav [--remote <url>] [--token <token>] <command>
```

`bootstrap` starts the API server. All other commands send HTTP requests to it — defaulting to `http://localhost:8080`. Pass `--remote <url>` (or set `NCLAV_URL`) to target a remote server. The token is read from `~/.nclav/token` automatically; override with `--token` or `NCLAV_TOKEN`.

### `nclav serve`

Start the nclav API server. All other commands send HTTP requests to it.

```bash
# Persistent state (default) — redb at ~/.nclav/state.redb
nclav serve --cloud local

# Ephemeral — in-memory, lost on restart (CI / quick tests)
nclav serve --cloud local --ephemeral

# GCP as default cloud for enclaves
nclav serve --cloud gcp \
  --gcp-parent folders/123456789 \
  --gcp-billing-account billingAccounts/XXXX-YYYY-ZZZZ

# Azure as default cloud for enclaves
nclav serve --cloud azure \
  --azure-tenant-id $TENANT_ID \
  --azure-management-group-id $MG_ID \
  --azure-billing-account-name $BILLING_ACCOUNT \
  --azure-billing-profile-name $BILLING_PROFILE \
  --azure-invoice-section-name $INVOICE_SECTION

# AWS as default cloud for enclaves
nclav serve --cloud aws \
  --aws-org-unit-id $ORG_UNIT_ID \
  --aws-email-domain $EMAIL_DOMAIN
```

Every driver must be explicitly requested — `--cloud` registers the default driver, `--enable-cloud` adds more. Binds `http://127.0.0.1:8080` by default; use `--bind 0.0.0.0` / `NCLAV_BIND` to expose on all interfaces, `--port` / `NCLAV_PORT` to change the port.

On first run a 64-character bearer token is generated and written to `~/.nclav/token` (mode 0600). Subsequent restarts reuse the same token — clients stay connected. Pass `--rotate-token` to force a new token (invalidates existing clients).

**GCP flags / env vars:**

| Flag | Env var | Required | Description |
|---|---|:---:|---|
| `--gcp-parent` | `NCLAV_GCP_PARENT` | yes | Resource parent: `folders/123` or `organizations/456` |
| `--gcp-billing-account` | `NCLAV_GCP_BILLING_ACCOUNT` | yes | Billing account: `billingAccounts/XXXX-YYYY-ZZZZ` |
| `--gcp-project-prefix` | `NCLAV_GCP_PROJECT_PREFIX` | no | Prefix for GCP project IDs: `acme` → `acme-product-a-dev` |
| `--gcp-default-region` | `NCLAV_GCP_DEFAULT_REGION` | no | Default region for enclave projects (default: `us-central1`) |

GCP credentials must include the `cloud-billing` scope:

```bash
gcloud auth application-default login \
  --scopes=https://www.googleapis.com/auth/cloud-platform,https://www.googleapis.com/auth/cloud-billing
```

For a persistent, cloud-hosted deployment see [Hosted deployment (GCP)](#hosted-deployment-gcp) above.

**Azure flags / env vars:**

| Flag | Env var | Required | Description |
|---|---|:---:|---|
| `--azure-tenant-id` | `NCLAV_AZURE_TENANT_ID` | yes | Azure tenant ID |
| `--azure-management-group-id` | `NCLAV_AZURE_MANAGEMENT_GROUP_ID` | yes | Management group for enclave subscriptions |
| `--azure-billing-account-name` | `NCLAV_AZURE_BILLING_ACCOUNT_NAME` | yes | MCA billing account name |
| `--azure-billing-profile-name` | `NCLAV_AZURE_BILLING_PROFILE_NAME` | yes | MCA billing profile name |
| `--azure-invoice-section-name` | `NCLAV_AZURE_INVOICE_SECTION_NAME` | yes | MCA invoice section name |
| `--azure-default-location` | `NCLAV_AZURE_DEFAULT_LOCATION` | no | Default Azure region (default: `eastus2`) |
| `--azure-subscription-prefix` | `NCLAV_AZURE_SUBSCRIPTION_PREFIX` | no | Prefix for subscription aliases: `myorg` → `myorg-product-a-dev` |
| `--azure-client-id` | `NCLAV_AZURE_CLIENT_ID` | no | SP client ID (falls back to Managed Identity / Azure CLI) |
| `--azure-client-secret` | `NCLAV_AZURE_CLIENT_SECRET` | no | SP client secret |

Azure auth is selected automatically: SP credentials → IMDS (Managed Identity) → Azure CLI fallback. No explicit credential flag is needed when running on Azure (e.g. the Container App bootstrap uses the managed identity).

For a persistent, cloud-hosted deployment see [Hosted deployment (Azure)](#hosted-deployment-azure) above.

**AWS flags / env vars:**

| Flag | Env var | Required | Description |
|---|---|:---:|---|
| `--aws-org-unit-id` | `NCLAV_AWS_ORG_UNIT_ID` | yes | OU ID where new account enclaves are placed |
| `--aws-email-domain` | `NCLAV_AWS_EMAIL_DOMAIN` | yes | Email domain for new account registration |
| `--aws-default-region` | `NCLAV_AWS_DEFAULT_REGION` | no | Default AWS region (default: `us-east-1`) |
| `--aws-account-prefix` | `NCLAV_AWS_ACCOUNT_PREFIX` | no | Prefix for account names: `myorg` → `myorg-product-a-dev` |
| `--aws-cross-account-role` | `NCLAV_AWS_CROSS_ACCOUNT_ROLE` | no | IAM role assumed in enclave accounts (default: `OrganizationAccountAccessRole`) |
| `--aws-role-arn` | `NCLAV_AWS_ROLE_ARN` | no | IAM role ARN assumed for management API calls |

AWS credentials are resolved automatically: env vars (`AWS_ACCESS_KEY_ID`) → ECS task credentials → EC2 IMDSv2 → AWS CLI fallback. No explicit credential flag is needed when running on ECS Fargate (the bootstrap uses the ECS task role).

For a persistent, cloud-hosted deployment see [Hosted deployment (AWS)](#hosted-deployment-aws) above.

### `nclav diff <enclaves-dir>`

Dry-run: loads YAML, validates the graph, computes the diff, and prints what would change — without touching any state.

```
+ enclave product-a-dev
  + partition product-a-dev/api
  + partition product-a-dev/db
  > export product-a-dev/api-http
  > export product-a-dev/db-tcp
  < import product-a-dev/database
6 change(s) would be applied.
```

Prefix key: `+` create, `~` update, `-` delete, `>` export wired, `<` import wired.

### `nclav apply <enclaves-dir>`

Reconcile and apply: same as `diff` but actually provisions resources and persists state. IaC-backed partitions will have `terraform init` + `terraform apply` run automatically.

### `nclav status`

Prints a summary of enclave health from the server. Includes enclave count, default cloud, and active drivers.

### `nclav graph [--output text|json|dot] [--enclave <id>]`

Render the dependency graph from the running server's applied state.

```bash
nclav graph                             # human-readable text
nclav graph --output dot | dot -Tsvg > graph.svg
nclav graph --output json
nclav graph --enclave product-a-dev     # filter to one enclave
```

### `nclav destroy [<enclave-id>...] [--all]`

Tear down one or more enclaves, destroying all their infrastructure and removing them from state. For IaC-backed partitions this runs `terraform destroy` before tearing down the enclave itself.

```bash
# Destroy a specific enclave
nclav destroy product-a-dev

# Destroy several at once
nclav destroy product-a-dev product-b-staging

# Nuclear option — destroy everything the server knows about
nclav destroy --all

# Destroy a single partition (runs terraform destroy, clears state)
nclav destroy product-a-dev --partition db
```

This is the imperative escape hatch. The declarative equivalent is to remove the enclave from your YAML and run `nclav apply`. Either approach follows the same teardown path; `destroy` is more convenient when testing or resetting an environment.

### `nclav orphans [--enclave <id>]`

Scan enclave cloud projects for resources tagged `nclav-managed=true` whose
`nclav-partition` tag/label does not match any active partition in nclav state.
Queries Cloud Asset Inventory (GCP) or Azure Resource Graph (Azure).
Useful after a failed destroy to surface what was left behind.

Exit 0 if no orphans found; exit 1 if any are reported (CI-friendly).

### `nclav iac runs <enclave-id> <partition-id>`

List IaC run history for a partition (newest first):

```
ID                                     OPERATION    STATUS       STARTED              EXIT
──────────────────────────────────────────────────────────────────────────────────────────
3f6d9e1a-...                           provision    succeeded    2024-01-15T10:30:00  0
```

### `nclav iac logs <enclave-id> <partition-id> [run-id]`

Print the full combined stdout+stderr log from an IaC run. If `run-id` is omitted, prints the most recent run.

```bash
nclav iac logs product-a-dev db
nclav iac logs product-a-dev db 3f6d9e1a-c4b2-4d91-a8f0-123456789abc
```

## HTTP API

Start the server with `nclav serve`, then use the token from `~/.nclav/token`. All endpoints require `Authorization: Bearer <token>`.

| Method | Path | Description |
|---|---|---|
| `GET` | `/health` | Always 200 OK |
| `GET` | `/ready` | 200 if store is reachable |
| `POST` | `/reconcile` | Apply changes |
| `POST` | `/reconcile/dry-run` | Diff only |
| `GET` | `/enclaves` | List all enclave states |
| `GET` | `/enclaves/{id}` | Single enclave state |
| `DELETE` | `/enclaves/{id}` | Destroy an enclave and all its infrastructure |
| `GET` | `/enclaves/{id}/graph` | Import/export graph for one enclave |
| `GET` | `/graph` | System-wide dependency graph |
| `GET` | `/events` | Audit log (`?enclave_id=&limit=`) |
| `GET` | `/status` | Summary: enclave count, default cloud, active drivers |
| `DELETE` | `/enclaves/{id}/partitions/{part}` | Destroy a single partition and its infrastructure |
| `GET` | `/enclaves/{id}/partitions/{part}/iac/runs` | List IaC runs for a partition |
| `GET` | `/enclaves/{id}/partitions/{part}/iac/runs/latest` | Most recent IaC run |
| `GET` | `/enclaves/{id}/partitions/{part}/iac/runs/{run-id}` | Specific IaC run |
| `GET` | `/terraform/state/{enc}/{part}` | TF HTTP backend: get state |
| `POST` | `/terraform/state/{enc}/{part}` | TF HTTP backend: save state |
| `DELETE` | `/terraform/state/{enc}/{part}` | TF HTTP backend: delete state |
| `POST` | `/terraform/state/{enc}/{part}/lock` | TF HTTP backend: acquire lock |
| `DELETE` | `/terraform/state/{enc}/{part}/lock` | TF HTTP backend: release lock. Send no body to force-unlock (clears any lock regardless of ID) |

```bash
TOKEN=$(cat ~/.nclav/token)

# Apply via HTTP
curl -X POST http://localhost:8080/reconcile \
  -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"enclaves_dir": "./enclaves"}'

# Destroy an enclave via HTTP
curl -X DELETE http://localhost:8080/enclaves/product-a-dev \
  -H "Authorization: Bearer $TOKEN"

# Audit log
curl -H "Authorization: Bearer $TOKEN" 'http://localhost:8080/events?limit=20'
```

## Writing enclave YAML

An enclaves directory has one subdirectory per enclave. Each enclave subdirectory contains a `config.yml` and one subdirectory per partition.

```text
enclaves/
  product-a/dev/
    config.yml          ← enclave declaration
    api/
      config.yml        ← partition declaration (terraform backend)
      main.tf           ← your terraform code
    db/
      config.yml        ← partition declaration (terraform backend)
      main.tf           ← your terraform code
```

### Enclave `config.yml`

```yaml
id: product-a-dev
name: Product A (Development)
cloud: local          # local | gcp | azure | aws  — optional; omit to use the API's default cloud
region: local-1
identity: product-a-dev-identity

network:
  vpc_cidr: "10.0.0.0/16"
  subnets: ["10.0.1.0/24"]

dns:
  zone: product-a.dev.local

# What this enclave exposes to others
exports:
  - name: api-http
    target_partition: api   # which partition backs this export
    type: http              # http | tcp | queue
    to: any_enclave         # public | any_enclave | vpn | {enclave: <id>}
    auth: token             # none | token | oauth | mtls | native

# What this enclave pulls in from others (cross-enclave)
imports: []
```

**Auth/type compatibility matrix:**

| type | none | token | oauth | mtls | native |
|---|:---:|:---:|:---:|:---:|:---:|
| http | yes | yes | yes | yes | — |
| tcp | yes | — | — | yes | yes |
| queue | yes | yes | — | — | yes |

### Partition `config.yml`

Every partition is backed by Terraform or OpenTofu. Place `.tf` files alongside `config.yml` in the partition directory and declare the variables you need via `inputs:`:

```yaml
id: db
name: Database
produces: tcp
backend: terraform    # terraform | opentofu

inputs:
  project_id: "{{ nclav_project_id }}"   # opt in to context tokens you need
  region:     "{{ nclav_region }}"
  db_host:    "{{ database.hostname }}"  # cross-partition import value

declared_outputs:
  - hostname
  - port
```

When nclav reconciles this partition it:

1. Creates a workspace at `~/.nclav/workspaces/{enclave_id}/{partition_id}/`
2. Symlinks all `.tf` files from the partition directory into the workspace
3. Writes `nclav_backend.tf` (configures the Terraform HTTP state backend — no separate backend setup required)
4. Writes `nclav_context.auto.tfvars` with a preamble containing `nclav_enclave` and `nclav_partition` (always injected), followed by the keys declared in `inputs:` after resolving all template tokens
5. Runs `terraform init` then `terraform apply -auto-approve`
6. Reads the declared outputs and stores them for downstream partitions to consume
7. Records the full combined log as an `IacRun` record (viewable with `nclav iac logs`)

**`inputs:` template tokens:**

| Token | Value |
|---|---|
| `{{ nclav_enclave_id }}` | Enclave ID |
| `{{ nclav_partition_id }}` | Partition ID |
| `{{ nclav_project_id }}` | Cloud project ID (GCP: enclave's GCP project; local: `""`) |
| `{{ nclav_region }}` | Cloud region (GCP: configured region; local: `""`) |
| `{{ alias.key }}` | Output of a declared cross-partition import |

Only the keys listed in `inputs:` are written to `nclav_context.auto.tfvars`. Your `.tf` files must declare matching `variable` blocks for whatever keys you use.

**Referencing an external module** — add `terraform.source` instead of writing `.tf` files. nclav generates the entire workspace; the partition directory must contain no `.tf` files:

```yaml
backend: terraform
terraform:
  source: "git::https://github.com/myorg/platform-modules.git//postgres?ref=v1.2.0"

inputs:
  project_id: "{{ nclav_project_id }}"
  region:     "{{ nclav_region }}"

declared_outputs:
  - hostname
  - port
```

**Override the binary** (e.g. to pin a version or use a wrapper):

```yaml
terraform:
  tool: /usr/local/bin/terraform-1.6
```

**Terraform state** is stored inside nclav via the HTTP backend — no S3 bucket, GCS bucket, or Terraform Cloud account required.

**Teardown** (`nclav destroy` or removing the enclave from YAML then re-applying) runs `terraform destroy -auto-approve` before removing the enclave from state.

## Workspace layout

```text
crates/
  nclav-domain/       Pure types — no I/O
  nclav-config/       YAML parsing, Raw* -> domain conversion
  nclav-graph/        Petgraph validation: dangling imports, access control, cycles, topo sort
  nclav-store/        StateStore trait + InMemoryStore + RedbStore (persistent local)
  nclav-driver/       Driver trait + DriverRegistry + LocalDriver + GcpDriver + AzureDriver + AwsDriver + TerraformBackend
  nclav-reconciler/   Reconcile loop: diff -> provision -> persist
  nclav-api/          Axum HTTP server (bearer token auth)
  nclav-cli/          Clap binary
```

Dependency order (no cycles): `domain` → `config / graph / store / driver` → `reconciler` → `api / cli`

## Running tests

```bash
cargo test --workspace
```

No external services or credentials required. All tests use `InMemoryStore` and `LocalDriver`.

## Tracing / debug output

Set `RUST_LOG` to enable structured log output:

```bash
RUST_LOG=debug cargo run -p nclav-cli -- apply ./enclaves
RUST_LOG=nclav_reconciler=info,nclav_driver=debug cargo run -p nclav-cli -- apply ./enclaves

# Watch IaC subprocess output specifically
RUST_LOG=nclav::iac=debug cargo run -p nclav-cli -- apply ./enclaves
```

## What's not implemented yet

- **IaC drift detection** — `terraform plan` on a schedule to detect out-of-band changes
- **Live log streaming** — IaC run logs are currently stored as a single blob after completion; streaming via SSE is future work
- **Web UI**
