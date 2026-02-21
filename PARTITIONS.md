# Partition Backends — Design Plan

A partition's `produces` type (http / tcp / queue) defines its **interface contract** with the rest of
the enclave graph. Its **backend** defines *how that workload is provisioned*. These are orthogonal.

---

## 1. New `backend` field

Add `backend` to `RawPartition` / `Partition`. Default is `managed` (current behaviour — the cloud
driver handles everything with its own built-in logic, e.g. Cloud Run, Pub/Sub).

```yaml
# enclaves/product-a/dev/db/config.yml
id: db
name: Database
produces: tcp
backend: terraform          # new; absent or "managed" = current behaviour
terraform:
  tool: terraform            # or "opentofu"; absent = auto-detect binary on PATH
declared_outputs:
  - hostname
  - port
inputs:
  db_name: myapp
  db_tier: db-f1-micro
```

The `.tf` files live alongside `config.yml` in the partition directory. Because these are standard
Terraform files, users can reference any module source natively — git URLs, local paths, the
Terraform Registry — without nclav needing to understand or replicate module sourcing:

```hcl
# enclaves/product-a/dev/db/main.tf
module "rds" {
  source  = "git::https://github.com/myorg/platform-modules.git//rds?ref=v1.2.0"
  db_name = var.db_name
  db_tier = var.db_tier
}

output "hostname" { value = module.rds.hostname }
output "port"     { value = module.rds.port }
```

nclav does not implement module sourcing. Terraform's own mechanisms handle it.

### Supported backend values

| `backend` | binary |
|---|---|
| `managed` (default) | — (cloud driver built-in logic) |
| `terraform` | `terraform` |
| `opentofu` | `tofu` |

The `terraform:` sub-block is optional and only read when `backend` is `terraform` or `opentofu`.
If absent, the binary is auto-detected from PATH (`terraform` first, then `tofu`).

---

## 2. Execution flow

All IaC execution happens through a **workspace** under `~/.nclav/workspaces/{enclave_id}/{partition_id}/`.
User source files are never modified.

### 2a. Workspace setup

```text
~/.nclav/workspaces/product-a-dev/db/
  nclav_backend.tf          ← generated: declares backend "http" {}
  nclav_context.auto.tfvars ← generated: resolved_inputs + nclav context vars
  *.tf  (symlinks)          ← symlinks to all .tf files in the partition directory
  .terraform/               ← terraform cache (stays in workspace, not user source)
```

All `*.tf` files in the partition directory are symlinked into the workspace. User source files
are never modified. The two generated files (`nclav_backend.tf`, `nclav_context.auto.tfvars`)
exist only in the workspace.

### 2b. Generated `nclav_backend.tf`

```hcl
terraform {
  backend "http" {}
}
```

The backend address is provided via `-backend-config` flags at `terraform init` time (see §4).

### 2c. Generated `nclav_context.auto.tfvars`

Contains two things merged together:

1. **nclav context variables** — injected by nclav, prefixed `nclav_*`
2. **Resolved partition inputs** — the partition's `inputs:` map after `{{ alias.key }}`
   substitution. This is the same `resolve_inputs()` path used by managed partitions, so
   cross-partition import values (e.g. a database hostname exported by another partition) flow
   through automatically. The user declares them in `inputs:` using template syntax; by the time
   the tfvars file is written they are already fully resolved strings.

```hcl
# nclav context — do not edit
nclav_enclave_id   = "product-a-dev"
nclav_partition_id = "api"
nclav_region       = "us-central1"
nclav_project_id   = "myorg-product-a-dev"   # GCP: from enclave handle; "" for local

# resolved partition inputs (includes values resolved from cross-partition imports)
db_host = "10.0.1.5"      # resolved from {{ database.hostname }}
db_port = "5432"           # resolved from {{ database.port }}
db_name = "myapp"
```

The user's `.tf` files declare these as ordinary variables:

```hcl
variable "db_host" {}
variable "db_port" {}
variable "db_name" {}
```

`nclav_project_id` (and future cloud-specific vars) is extracted from the enclave handle by a new
`Driver::context_vars(&self, enclave: &Enclave, handle: &Handle) -> HashMap<String, String>`
method. LocalDriver returns an empty map; GcpDriver extracts `project_id`, `region`, etc.

### 2d. Cloud identity — `auth_env`

Authentication to the cloud provider is injected as **environment variables** on the terraform
subprocess, not as tfvars. This is a deliberate separation:

- **`context_vars`** → data the user's `.tf` files can reference as `var.nclav_*`
- **`auth_env`** → credentials the provider SDK reads automatically from the environment;
  the user's `.tf` files do not reference these at all

The variables are produced by a new `Driver::auth_env(&self, enclave: &Enclave, handle: &Handle)
-> HashMap<String, String>` method. The enclave's `identity` field is the service
principal/account name scoped to that enclave; `auth_env` constructs the full credential
reference from it.

**GCP example** — service account impersonation on top of the operator's ADC credentials:

```
GOOGLE_IMPERSONATE_SERVICE_ACCOUNT = product-a-dev-sa@myorg-product-a-dev.iam.gserviceaccount.com
GOOGLE_PROJECT                     = myorg-product-a-dev
```

The full SA email is `{enclave.identity}@{project_id}.iam.gserviceaccount.com`, where
`project_id` comes from the enclave handle written by `provision_enclave`. No key files are
needed. The operator's existing ADC credentials are used to impersonate the enclave SA.

The user's `google` provider picks this up with no explicit configuration:

```hcl
terraform {
  required_providers {
    google = { source = "hashicorp/google" }
  }
}
# No provider block required — nclav sets GOOGLE_PROJECT and
# GOOGLE_IMPERSONATE_SERVICE_ACCOUNT in the subprocess environment.
```

**LocalDriver** returns an empty map (no auth needed).

Future cloud drivers (Azure, AWS) implement `auth_env` with their own standard env vars
(`ARM_CLIENT_ID`, `AWS_ROLE_ARN`, etc.) following the same pattern.

### 2e. Command sequence

`auth_env` variables are merged with `TF_HTTP_PASSWORD` and set on every subprocess.

```
# Environment set on all terraform subprocesses:
#   TF_HTTP_PASSWORD=<nclav_token>
#   + all entries from Driver::auth_env()  (e.g. GOOGLE_IMPERSONATE_SERVICE_ACCOUNT, GOOGLE_PROJECT)

terraform init \
  -reconfigure \
  -backend-config="address=http://127.0.0.1:8080/terraform/state/product-a-dev/db" \
  -backend-config="lock_address=http://127.0.0.1:8080/terraform/state/product-a-dev/db/lock" \
  -backend-config="unlock_address=http://127.0.0.1:8080/terraform/state/product-a-dev/db/lock" \
  -backend-config="lock_method=POST" \
  -backend-config="unlock_method=DELETE" \
  -backend-config="username=nclav"

terraform apply -auto-approve -no-color

terraform output -json
```

The nclav token and cloud identity credentials are never written to disk or passed as CLI flags.

### 2f. Output extraction

`terraform output -json` returns a map of `{ "key": { "value": ..., "type": ... } }`. nclav
extracts the keys listed in `declared_outputs` and returns them as `ProvisionResult.outputs`.
Missing declared output keys are a `DriverError`.

### 2g. Teardown

```
terraform destroy -auto-approve -no-color
```

Runs in the same workspace directory with the same environment. State is deleted from the state
store after successful destroy.

Teardown is triggered in two ways:

- **Declarative** — remove the enclave from YAML, then run `nclav apply`. The reconciler detects
  the diff and tears down IaC partitions before deleting the enclave from state.
- **Imperative** — `nclav destroy <enclave-id>` (or `--all` to nuke every enclave). Calls
  `DELETE /enclaves/:id` on the API, which follows the identical teardown path. Useful for testing
  and environment resets without editing YAML.

### 2h. Drift observation (`observe_partition`)

Run `terraform output -json` (no plan, no apply). If it fails (state missing / workspace not
initialised), report `exists: false`.

---

## 3. State store — Terraform HTTP backend

nclav implements the [Terraform HTTP backend protocol][tf-http] inside its existing API server.
State blobs are stored in redb (local) / postgres (future). No external state backend is needed.

[tf-http]: https://developer.hashicorp.com/terraform/language/backend/http

### New `StateStore` trait methods

```rust
// Raw state blob (JSON bytes as-is from Terraform)
async fn get_tf_state(&self, key: &str) -> Result<Option<Vec<u8>>, StoreError>;
async fn put_tf_state(&self, key: &str, state: Vec<u8>) -> Result<(), StoreError>;
async fn delete_tf_state(&self, key: &str) -> Result<(), StoreError>;

// Advisory locking (Terraform lock protocol)
async fn lock_tf_state(&self, key: &str, lock_info: serde_json::Value)
    -> Result<(), StoreError>;   // Err(Conflict) if already locked
async fn unlock_tf_state(&self, key: &str, lock_id: &str) -> Result<(), StoreError>;
```

State key format: `"{enclave_id}/{partition_id}"`.

### New API routes (all protected by bearer-token auth)

```
GET    /terraform/state/:enclave_id/:partition_id        → 200 + body, or 204 (no state)
POST   /terraform/state/:enclave_id/:partition_id        → 200 (state updated)
DELETE /terraform/state/:enclave_id/:partition_id        → 200 (state deleted)
POST   /terraform/state/:enclave_id/:partition_id/lock   → 200 (locked) or 409 (locked by other)
DELETE /terraform/state/:enclave_id/:partition_id/lock   → 200 (unlocked)
```

---

## 4. Reconciler dispatch

The reconciler, before calling `driver.provision_partition`, checks `partition.backend`:

```
if managed  → driver.provision_partition(...)       // existing path
if terraform/opentofu → TerraformBackend::provision(...)  // new path
```

The `Driver` trait is **not called** for IaC-backed partitions — the driver is only responsible for
enclave-level infra (VPC, project, IAM) and managed partition types. This means all clouds
automatically get IaC partition support without any driver changes (except `context_vars` and
`auth_env`).

---

## 5. IaC run logs

Every terraform invocation (provision, update, teardown) produces a structured `IacRun` record
stored in the state store. stdout and stderr are captured interleaved in arrival order, accumulated
into a single `log` string, and written atomically when the run finishes (succeeded or failed).
Live streaming is future work.

### `IacRun` struct (`nclav-store`)

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IacRun {
    pub id: Uuid,
    pub enclave_id: EnclaveId,
    pub partition_id: PartitionId,
    pub operation: IacOperation,      // Provision | Update | Teardown
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub status: IacRunStatus,         // Running | Succeeded | Failed
    pub exit_code: Option<i32>,
    pub log: String,                  // combined stdout+stderr, interleaved in order
    pub reconcile_run_id: Option<Uuid>,  // links to the reconcile that triggered this
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IacOperation { Provision, Update, Teardown }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IacRunStatus { Running, Succeeded, Failed }
```

### New `StateStore` trait methods

```rust
async fn append_iac_run(&self, run: &IacRun) -> Result<(), StoreError>;
async fn list_iac_runs(
    &self,
    enclave_id: &EnclaveId,
    partition_id: &PartitionId,
) -> Result<Vec<IacRun>, StoreError>;   // ordered newest-first, capped at last 100
async fn get_iac_run(&self, run_id: Uuid) -> Result<Option<IacRun>, StoreError>;
```

redb: new `IAC_RUNS` table keyed by `run_id` (UUID bytes); a secondary index table
`IAC_RUNS_BY_PARTITION` keyed by `"{enclave_id}/{partition_id}/{started_at_rfc3339}/{run_id}"`
(lexicographic sort gives newest-first when reversed).

### Log capture in `TerraformBackend`

```rust
// Conceptual — merge stdout+stderr with tokio::process
let mut child = Command::new(&self.binary)
    .args(&["apply", "-auto-approve", "-no-color"])
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()?;

// Read both streams line-by-line, interleave into log buffer,
// and mirror each line to tracing::debug!() so it appears in nclav's own logs.
```

The run record is written to the store twice:
1. **Start**: `status: Running`, `finished_at: None`, `log: ""` — allows future streaming to query.
2. **End**: `status: Succeeded/Failed`, `finished_at`, full `log`, `exit_code`.

`init` and `apply` (or `destroy`) are concatenated into one log with a separator line, under a
single `IacRun` per reconcile operation.

### New API routes

```
GET /enclaves/:enclave_id/partitions/:partition_id/iac/runs
    → 200 [ IacRun summary (id, operation, status, started_at, finished_at) ... ]
    newest-first, last 100

GET /enclaves/:enclave_id/partitions/:partition_id/iac/runs/:run_id
    → 200 IacRun (full, including log)

GET /enclaves/:enclave_id/partitions/:partition_id/iac/runs/latest
    → 200 IacRun (full log of the most recent run)
```

All routes protected by bearer-token auth.

### CLI subcommands

```
nclav iac runs <enclave_id> <partition_id>
    Table output: RUN_ID | OPERATION | STATUS | STARTED | DURATION

nclav iac logs <enclave_id> <partition_id> <run_id>
    Prints the full log to stdout.

nclav iac logs <enclave_id> <partition_id> --last
    Shorthand for the most recent run.
```

### Future work

- SSE endpoint `GET /iac/runs/:run_id/stream` to tail a live run in real time.
- Log retention policy / pruning older than N runs or N days.

---

## 6. New `TerraformBackend` struct (`nclav-driver`)

```rust
pub struct TerraformBackend {
    /// "terraform" or "tofu"
    pub binary: String,
    /// nclav API base URL (for state backend config and run-log writes)
    pub api_base: String,
    /// nclav auth token (for TF_HTTP_PASSWORD)
    pub auth_token: Arc<String>,
    /// Store handle for writing IacRun records
    pub store: Arc<dyn StateStore>,
}

impl TerraformBackend {
    pub async fn provision(
        &self,
        enclave: &Enclave,
        partition: &Partition,
        tf_config: &TerraformConfig,
        resolved_inputs: &HashMap<String, String>,
        context_vars: &HashMap<String, String>,  // from Driver::context_vars — written to tfvars
        auth_env: &HashMap<String, String>,       // from Driver::auth_env — set on subprocess env
        reconcile_run_id: Option<Uuid>,
    ) -> Result<ProvisionResult, DriverError>;

    pub async fn teardown(
        &self,
        enclave: &Enclave,
        partition: &Partition,
        auth_env: &HashMap<String, String>,
        reconcile_run_id: Option<Uuid>,
    ) -> Result<(), DriverError>;

    pub async fn observe(
        &self,
        enclave: &Enclave,
        partition: &Partition,
        auth_env: &HashMap<String, String>,
        handle: &Handle,
    ) -> Result<ObservedState, DriverError>;
}
```

---

## 7. Domain / config changes

### `nclav-domain/src/types.rs`

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PartitionBackend {
    #[default]
    Managed,
    Terraform(TerraformConfig),
    OpenTofu(TerraformConfig),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerraformConfig {
    /// Binary override ("terraform" or "tofu"). None = auto-detect from PATH.
    pub tool: Option<String>,
}
```

`Partition` gains `pub backend: PartitionBackend`.

### `nclav-config/src/raw.rs`

```rust
pub struct RawPartition {
    // existing fields ...
    #[serde(default)]
    pub backend: String,           // "managed" | "terraform" | "opentofu"
    pub terraform: Option<RawTerraformConfig>,
}

pub struct RawTerraformConfig {
    pub tool: Option<String>,
}
```

---

## 8. Files to create / modify

| File | Change |
|---|---|
| `nclav-domain/src/types.rs` | Add `PartitionBackend`, `TerraformConfig`; add `backend` to `Partition` |
| `nclav-config/src/raw.rs` | Add `backend`, `terraform` to `RawPartition`; add `RawTerraformConfig` |
| `nclav-config/src/loader.rs` | Parse `backend` + `terraform` block → `PartitionBackend` |
| `nclav-store/src/store.rs` | Add TF state methods + `IacRun` types + `append/list/get_iac_run` |
| `nclav-store/src/memory.rs` | In-memory impl for TF state, locks, and IaC run log |
| `nclav-store/src/redb_store.rs` | redb impl: `TF_STATE`, `TF_LOCKS`, `IAC_RUNS`, `IAC_RUNS_BY_PARTITION` tables |
| `nclav-driver/src/terraform.rs` | New `TerraformBackend`: workspace setup (symlinks + generated files), process execution, log capture |
| `nclav-driver/src/driver.rs` | Add `context_vars` and `auth_env` methods to `Driver` trait |
| `nclav-driver/src/local.rs` | Implement both (return empty maps) |
| `nclav-driver/src/gcp.rs` | `context_vars`: extracts `project_id`, `region` from handle; `auth_env`: sets `GOOGLE_IMPERSONATE_SERVICE_ACCOUNT` + `GOOGLE_PROJECT` from `enclave.identity` |
| `nclav-driver/src/lib.rs` | Re-export `TerraformBackend` |
| `nclav-reconciler/src/reconcile.rs` | Backend dispatch; pass `reconcile_run_id` to `TerraformBackend` |
| `nclav-api/src/handlers.rs` | TF HTTP state backend routes + IaC run log routes |
| `nclav-api/src/app.rs` | Register new routes |
| `nclav-api/src/state.rs` | Pass `api_base` + `auth_token` + `store` to `TerraformBackend` |

---

## 9. Out of scope (future)

- `backend: script` — arbitrary `provision.sh` / `teardown.sh` (security implications need design)
- Soft-delete / destroy protection flag
- Pulumi, CDK, Helm backends
- Workspace pruning / cleanup policy for stale workspaces
- SSE streaming of live IaC run output (`GET /iac/runs/:run_id/stream`)
- IaC run log retention policy (prune older than N runs or N days)
