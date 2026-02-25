# Partition Backends

A partition's `produces` type (`http` / `tcp` / `queue`) defines its **interface
contract** with the rest of the enclave graph. Its `backend` defines *how that workload
is provisioned*. These two concerns are independent — any backend can produce any type.

---

## Backends at a glance

| `backend` | Who provisions | Files in partition directory |
|---|---|---|
| `terraform` (default) | `terraform` binary | `.tf` files you write |
| `opentofu` | `tofu` binary | `.tf` files you write |
| `terraform` + `source:` | `terraform` binary, module fetched from URL | None — nclav generates everything |
| `opentofu` + `source:` | `tofu` binary, module fetched from URL | None — nclav generates everything |

---

## IaC-backed partitions

Every partition is backed by Terraform or OpenTofu. nclav creates a per-partition
service account, then manages the workspace, state backend, credentials, and run logs —
you only write the infrastructure code.

### Writing your own `.tf` files

Place `.tf` files alongside `config.yml` in the partition directory. nclav symlinks
them into the workspace and runs `terraform apply` against them.

```text
enclaves/product-a/dev/db/
  config.yml    ← partition declaration
  main.tf       ← your terraform code
  variables.tf  ← your variable declarations
```

```yaml
# config.yml
id: db
name: Database
produces: tcp
backend: terraform

inputs:
  db_name: myapp
  db_host: "{{ database.hostname }}"   # resolved from cross-partition import

declared_outputs:
  - hostname
  - port
```

```hcl
# main.tf
variable "db_name" {}
variable "db_host" {}

resource "google_sql_database_instance" "main" {
  name   = var.db_name
  # ...
}

output "hostname" { value = google_sql_database_instance.main.ip_address[0].ip_address }
output "port"     { value = 5432 }
```

### Referencing an external module

Add `source:` to the `terraform:` block to point at a Terraform module by URL instead
of writing `.tf` files. nclav generates the entire workspace — the partition directory
must contain no `.tf` files.

```yaml
# config.yml
id: db
name: Database
produces: tcp
backend: terraform

terraform:
  source: "git::https://github.com/myorg/platform-modules.git//postgres?ref=v1.2.0"

inputs:
  db_name: myapp
  db_tier: db-f1-micro
  subnet: "{{ network.subnet_id }}"

declared_outputs:
  - hostname
  - port
```

Ref pinning and all other source options use Terraform's native URL syntax
(`?ref=`, `?depth=`, etc.) — nclav passes the value through verbatim.

If `source:` is set and any `.tf` files are found in the partition directory, nclav
errors before running any Terraform command. Use `backend: terraform` without `source:`
if you want to manage the `.tf` files yourself.

**The platform module is plain Terraform** — no nclav-specific variables or conventions
are required:

```hcl
# git::https://github.com/myorg/platform-modules.git//postgres
variable "db_name" {}
variable "db_tier" {}
variable "subnet"  {}

# ... provider, resources ...

output "hostname" { value = google_sql_database_instance.main.ip_address[0].ip_address }
output "port"     { value = 5432 }
```

### Overriding the binary

Use `tool:` in the `terraform:` block to pin a specific binary path:

```yaml
terraform:
  tool: /usr/local/bin/terraform-1.9
```

When `tool:` is absent, nclav auto-detects: `terraform` is tried first, then `tofu`.

---

## `inputs:` and template substitution

The `inputs:` map supplies variable values to the partition. Every value is resolved
through the template engine before provisioning runs — the Terraform subprocess always
receives plain strings.

Three forms are supported:

| Form | Example | Resolves to |
|---|---|---|
| Literal value | `db_name: myapp` | `"myapp"` |
| Cross-partition import | `db_host: "{{ database.hostname }}"` | Output of the aliased import |
| nclav context token | `project: "{{ nclav_project_id }}"` | Cloud project ID for the enclave |

**Available `{{ nclav_* }}` tokens:**

| Token | Value |
|---|---|
| `{{ nclav_enclave_id }}` | Enclave ID |
| `{{ nclav_partition_id }}` | Partition ID |
| `{{ nclav_project_id }}` | Cloud project ID (GCP: enclave's GCP project; local: `""`) |
| `{{ nclav_region }}` | Cloud region (GCP: configured region; local: `""`) |

nclav always injects `nclav_enclave` and `nclav_partition` into
`nclav_context.auto.tfvars` as a preamble, regardless of what is listed in `inputs:`.
You do not need to add them to `inputs:`. After the preamble, the keys listed in
`inputs:` are written with their resolved values. If your `.tf` files don't declare a
variable, don't put it in `inputs:`.

For module-sourced partitions, resolved `inputs:` values become module arguments in the
generated `nclav_module.tf` instead of `auto.tfvars` entries. The template resolution
step is identical.

### Recommended labeling pattern

Apply the always-injected variables as resource labels in your Terraform code. This
convention enables `nclav orphans` to find your resources after a failed teardown:

```hcl
variable "nclav_enclave"   { default = "" }
variable "nclav_partition" { default = "" }

locals {
  nclav_labels = {
    "nclav-enclave"   = var.nclav_enclave
    "nclav-partition" = var.nclav_partition
    "nclav-managed"   = "true"
  }
}
# Apply local.nclav_labels to every resource you create.
```

Managed partitions (Cloud Run, Pub/Sub) are labeled automatically by the driver.

---

## `declared_outputs`

List the output keys the partition will produce after provisioning. These must match
`terraform output` keys in your `.tf` files (or in the referenced module). nclav reads
them after `terraform apply` and makes them available to downstream partitions via
import resolution.

```yaml
declared_outputs:
  - hostname
  - port
```

Required outputs by `produces` type:

| `produces` | Required output keys |
|---|---|
| `http` | `hostname`, `port` |
| `tcp` | `hostname`, `port` |
| `queue` | `queue_url` |

---

## Workspace layout

nclav maintains a workspace for each IaC-backed partition under
`~/.nclav/workspaces/{enclave_id}/{partition_id}/`. User source files are never
modified.

**Raw `.tf` mode:**

```text
~/.nclav/workspaces/product-a-dev/db/
  nclav_backend.tf            ← generated: HTTP state backend
  nclav_context.auto.tfvars   ← generated: resolved inputs
  main.tf  →  (symlink)       ← symlink to your partition directory
  variables.tf  →  (symlink)
  .terraform/                 ← Terraform cache
```

**Module-sourced mode:**

```text
~/.nclav/workspaces/product-a-dev/db/
  nclav_backend.tf    ← generated: HTTP state backend
  nclav_module.tf     ← generated: module block with resolved inputs as arguments
  nclav_outputs.tf    ← generated: output forwarding from module to root
  .terraform/         ← Terraform cache (module source fetched here)
```

The generated `nclav_backend.tf` in both modes:

```hcl
# Generated by nclav — do not edit
terraform {
  backend "http" {}
}
```

The backend URL, lock addresses, and auth token are supplied via `-backend-config`
flags at `terraform init` time and are never written to disk.

---

## Cloud provider authentication

Credentials are injected as **environment variables** on the Terraform subprocess — not
as tfvars and not as CLI flags. Your `.tf` files require no explicit credential
configuration; the provider picks them up automatically from the environment.

**GCP — local dev (Application Default Credentials):**

```
GOOGLE_PROJECT = myorg-product-a-dev
```

**GCP — production (service account key):**

```
GOOGLE_APPLICATION_CREDENTIALS    = ~/.nclav/gcp-credentials.json
GOOGLE_IMPERSONATE_SERVICE_ACCOUNT = partition-db@myorg-product-a-dev.iam.gserviceaccount.com
GOOGLE_PROJECT                     = myorg-product-a-dev
```

nclav creates one service account per partition (`partition-{id}@{project}.iam.gserviceaccount.com`)
for least-privilege isolation. The Terraform subprocess impersonates that partition-scoped SA,
not the enclave-level SA, so each IaC run has a clear IAM audit trail and the minimum
permissions needed for that partition alone.

Your `google` provider block therefore needs no `credentials` or `project` attributes:

```hcl
terraform {
  required_providers {
    google = { source = "hashicorp/google" }
  }
}
```

Future cloud drivers (Azure, AWS) follow the same pattern with their own standard env
vars (`ARM_CLIENT_ID`, `AWS_ROLE_ARN`, etc.).

---

## Terraform state

nclav implements the [Terraform HTTP backend protocol][tf-http] directly in its API
server. State blobs are stored in the same redb database used for enclave state. No
separate S3 bucket, GCS bucket, or Terraform Cloud account is required.

[tf-http]: https://developer.hashicorp.com/terraform/language/backend/http

The complete command sequence for a provision run:

```
terraform init \
  -reconfigure \
  -backend-config="address=http://127.0.0.1:8080/terraform/state/{enclave}/{partition}" \
  -backend-config="lock_address=…/lock" \
  -backend-config="unlock_address=…/lock" \
  -backend-config="lock_method=POST" \
  -backend-config="unlock_method=DELETE" \
  -backend-config="username=nclav"

terraform apply -auto-approve -no-color

terraform output -json
```

All subprocess environment variables include `TF_IN_AUTOMATION=1` and `TF_INPUT=0`.
stdin is `/dev/null`. A 30-minute hard timeout kills a hung process.

---

## IaC run logs

Every Terraform invocation (provision, update, teardown) is recorded as an `IacRun`
with combined stdout+stderr captured in arrival order. Logs are stored in the nclav
state store and viewable immediately after the run completes.

```bash
# List runs for a partition (newest first)
nclav iac runs product-a-dev db

# Print the most recent run log
nclav iac logs product-a-dev db

# Print a specific run log
nclav iac logs product-a-dev db 3f6d9e1a-c4b2-4d91-a8f0-123456789abc
```

Runs are also accessible via the HTTP API:

```
GET /enclaves/{id}/partitions/{part}/iac/runs
GET /enclaves/{id}/partitions/{part}/iac/runs/latest
GET /enclaves/{id}/partitions/{part}/iac/runs/{run-id}
```

A single `IacRun` record covers the full `init` + `apply` (or `destroy`) sequence, with
a separator line in the log between phases.

---

## Teardown

When an enclave is removed — either by deleting it from YAML and running `nclav apply`,
or by running `nclav destroy <enclave-id>` — nclav runs `terraform destroy
-auto-approve` in the workspace before removing the enclave from state. The same log
capture and `IacRun` recording applies.

The workspace directory is left in place after teardown so the run log remains
inspectable. It is reused if the enclave is re-provisioned.

After `terraform destroy` completes, the API response includes a `remaining_resources`
field listing any GCP resources still labeled to that partition. An empty array means
clean teardown; a non-empty array indicates resources that were not destroyed (e.g. due
to a partial failure) and should be investigated.

---

## Orphan detection

When `nclav destroy … --partition` completes, the API response includes a
`remaining_resources` field: an array of GCP resource names still labeled
`nclav-partition={id}`. An empty array means clean teardown.

To scan across all partitions at any time:

```bash
nclav orphans                          # all enclaves
nclav orphans --enclave product-a-dev  # one enclave
```

For this to work, your Terraform code must apply `local.nclav_labels` to every
resource it creates (see "Recommended labeling pattern" above).

---

## Future work

- Live log streaming via SSE (`GET /iac/runs/{run-id}/stream`)
- IaC run log retention / pruning policy
- Module registry — operator-managed catalog mapping short names to source URLs, with
  per-enclave policy controlling which modules are permitted
- `backend: script` — arbitrary `provision.sh` / `teardown.sh`
- Pulumi, Helm, CDK backends
- Workspace pruning policy for stale workspaces
