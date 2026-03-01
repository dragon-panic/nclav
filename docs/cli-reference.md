# CLI reference

```
nclav [--remote <url>] [--token <token>] <command>
```

`serve` starts the API server in-process. All other commands send HTTP requests to it — defaulting to `http://localhost:8080`. Pass `--remote <url>` (or set `NCLAV_URL`) to target a remote server. The token is read from `~/.nclav/token` automatically; override with `--token` or `NCLAV_TOKEN`.

## `nclav serve`

Start the nclav API server.

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

### GCP flags

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

For a persistent, cloud-hosted deployment see [Hosted deployment (GCP)](bootstrap-gcp.md).

### Azure flags

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

For a persistent, cloud-hosted deployment see [Hosted deployment (Azure)](bootstrap-azure.md).

### AWS flags

| Flag | Env var | Required | Description |
|---|---|:---:|---|
| `--aws-org-unit-id` | `NCLAV_AWS_ORG_UNIT_ID` | yes | OU ID where new account enclaves are placed |
| `--aws-email-domain` | `NCLAV_AWS_EMAIL_DOMAIN` | yes | Email domain for new account registration |
| `--aws-default-region` | `NCLAV_AWS_DEFAULT_REGION` | no | Default AWS region (default: `us-east-1`) |
| `--aws-account-prefix` | `NCLAV_AWS_ACCOUNT_PREFIX` | no | Prefix for account names: `myorg` → `myorg-product-a-dev` |
| `--aws-cross-account-role` | `NCLAV_AWS_CROSS_ACCOUNT_ROLE` | no | IAM role assumed in enclave accounts (default: `OrganizationAccountAccessRole`) |
| `--aws-role-arn` | `NCLAV_AWS_ROLE_ARN` | no | IAM role ARN assumed for management API calls |

AWS credentials are resolved automatically: env vars (`AWS_ACCESS_KEY_ID`) → ECS task credentials → EC2 IMDSv2 → AWS CLI fallback. No explicit credential flag is needed when running on ECS Fargate (the bootstrap uses the ECS task role).

For a persistent, cloud-hosted deployment see [Hosted deployment (AWS)](bootstrap-aws.md).

## `nclav diff <enclaves-dir>`

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

## `nclav apply <enclaves-dir>`

Reconcile and apply: same as `diff` but actually provisions resources and persists state. IaC-backed partitions will have `terraform init` + `terraform apply` run automatically.

## `nclav status`

Prints a summary of enclave health from the server. Includes enclave count, default cloud, and active drivers.

## `nclav graph [--output text|json|dot] [--enclave <id>]`

Render the dependency graph from the running server's applied state.

```bash
nclav graph                             # human-readable text
nclav graph --output dot | dot -Tsvg > graph.svg
nclav graph --output json
nclav graph --enclave product-a-dev     # filter to one enclave
```

## `nclav destroy [<enclave-id>...] [--all]`

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

## `nclav orphans [--enclave <id>]`

Scan enclave cloud projects for resources tagged `nclav-managed=true` whose `nclav-partition` tag/label does not match any active partition in nclav state. Queries Cloud Asset Inventory (GCP) or Azure Resource Graph (Azure). Useful after a failed destroy to surface what was left behind.

Exit 0 if no orphans found; exit 1 if any are reported (CI-friendly).

## `nclav iac runs <enclave-id> <partition-id>`

List IaC run history for a partition (newest first):

```
ID                                     OPERATION    STATUS       STARTED              EXIT
──────────────────────────────────────────────────────────────────────────────────────────
3f6d9e1a-...                           provision    succeeded    2024-01-15T10:30:00  0
```

## `nclav iac logs <enclave-id> <partition-id> [run-id]`

Print the full combined stdout+stderr log from an IaC run. If `run-id` is omitted, prints the most recent run.

```bash
nclav iac logs product-a-dev db
nclav iac logs product-a-dev db 3f6d9e1a-c4b2-4d91-a8f0-123456789abc
```
