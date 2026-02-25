# nclav

Cloud infrastructure orchestration via a YAML-driven reconcile loop. nclav manages **enclaves** — isolated network/cost/identity boundaries — and the **partitions** (infrastructure units) inside them. Dependencies between enclaves are declared explicitly through import/export contracts and validated before anything is provisioned.

## Concepts

| Term | Meaning |
|---|---|
| **Enclave** | An isolated boundary (VPC, subscription, cost center). Owns its partitions and declares what it exports to other enclaves. |
| **Partition** | A deployable unit inside an enclave (a service, a database, a queue). Can be managed by nclav directly or backed by Terraform/OpenTofu. |
| **Export** | A named endpoint an enclave makes available to others, with type (`http`, `tcp`, `queue`) and access control (`to:`). |
| **Import** | A reference to another enclave's export, given a local alias used for template substitution. |
| **Reconcile** | The loop that diffs desired state (YAML) against actual state (store), then provisions/tears down in dependency order. |

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
cargo run -p nclav-cli -- bootstrap --cloud local

# In another terminal — see what would change (token read automatically from ~/.nclav/token)
cargo run -p nclav-cli -- diff ./enclaves

# Apply changes (provisions resources via the running server)
cargo run -p nclav-cli -- apply ./enclaves

# Render the dependency graph (fetched from the running server)
cargo run -p nclav-cli -- graph --output text
```

## CLI reference

```
nclav [--remote <url>] [--token <token>] <command>
```

`bootstrap` starts the API server. All other commands send HTTP requests to it — defaulting to `http://localhost:8080`. Pass `--remote <url>` (or set `NCLAV_URL`) to target a remote server. The token is read from `~/.nclav/token` automatically; override with `--token` or `NCLAV_TOKEN`.

### `nclav bootstrap`

Bootstrap provisions the nclav **platform** — the API server and its state store. It is a one-time, idempotent operation. After bootstrap completes, the API runs continuously and all other CLI commands talk to it via HTTP.

The platform location (set by `--cloud`) and each enclave's target cloud (`cloud:` in YAML) are **independent**. An API running locally can provision enclaves into GCP. See [BOOTSTRAP.md](BOOTSTRAP.md) for full design rationale.

#### Local bootstrap

```bash
# Ephemeral — in-memory state, lost on restart (CI / quick tests)
nclav bootstrap --cloud local --ephemeral

# Persistent — redb at ~/.nclav/state.redb (default; single-operator dev)
nclav bootstrap --cloud local

# Local default + GCP also registered for mixed enclave environments
nclav bootstrap --cloud local \
  --enable-cloud gcp \
  --gcp-parent folders/123 \
  --gcp-billing-account billingAccounts/XXX-YYYY-ZZZZ
```

Every driver must be explicitly requested — `--cloud` registers the default driver, `--enable-cloud` adds more. Binds `http://127.0.0.1:8080` by default; use `--bind 0.0.0.0` / `NCLAV_BIND` to expose on all interfaces, `--port` / `NCLAV_PORT` to change the port.

On first run a 64-character bearer token is generated and written to `~/.nclav/token` (mode 0600). Subsequent restarts reuse the same token — clients stay connected. Pass `--rotate-token` to force a new token (invalidates existing clients).

#### GCP bootstrap

Initialises the GCP driver (via Application Default Credentials) and starts the API server locally with GCP as the default cloud. Enclave provisioning targets real GCP projects; the API process itself still runs on your machine.

**Flags / env vars:**

| Flag | Env var | Required | Description |
|---|---|:---:|---|
| `--gcp-parent` | `NCLAV_GCP_PARENT` | yes | Resource parent: `folders/123` or `organizations/456` |
| `--gcp-billing-account` | `NCLAV_GCP_BILLING_ACCOUNT` | yes | Billing account: `billingAccounts/XXXX-YYYY-ZZZZ` |
| `--gcp-project-prefix` | `NCLAV_GCP_PROJECT_PREFIX` | no | Prefix for GCP project IDs: `acme` → `acme-product-a-dev` |
| `--gcp-default-region` | `NCLAV_GCP_DEFAULT_REGION` | no | Default region for enclave projects (default: `us-central1`) |

```bash
export NCLAV_GCP_PARENT="folders/123456789"
export NCLAV_GCP_BILLING_ACCOUNT="billingAccounts/ABCD-1234-EFGH-5678"
export NCLAV_GCP_PROJECT_PREFIX="acme"

nclav bootstrap --cloud gcp
```

GCP credentials must include the `cloud-billing` scope:

```bash
gcloud auth application-default login \
  --scopes=https://www.googleapis.com/auth/cloud-platform,https://www.googleapis.com/auth/cloud-billing
```

**Partition type mapping (GCP managed driver):**

| Partition `produces` | GCP resource |
|---|---|
| `http` | Cloud Run service |
| `tcp` | Externally managed — reads `hostname`/`port` from partition `inputs:` |
| `queue` | Pub/Sub topic + subscription |

See [GCP.md](GCP.md) for full GCP driver and bootstrap reference.

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

Scan GCP enclave projects for resources labeled `nclav-managed=true` whose
`nclav-partition` label does not match any active partition in nclav state.
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

Start the server with `nclav bootstrap`, then use the token from `~/.nclav/token`. All endpoints require `Authorization: Bearer <token>`.

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
      config.yml        ← partition declaration (managed driver)
    db/
      config.yml        ← partition declaration (terraform backend)
      main.tf           ← your terraform code
```

### Enclave `config.yml`

```yaml
id: product-a-dev
name: Product A (Development)
cloud: local          # local | gcp | azure  — optional; omit to use the API's default cloud
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

### Partition `config.yml` — managed backend

```yaml
id: api
name: API Service
produces: http        # http | tcp | queue (optional)

# Intra- or cross-enclave imports
imports:
  - from: product-a-dev       # enclave id
    export_name: db-tcp
    alias: database           # local name for template substitution

# Template-resolved inputs passed to the driver at provision time
inputs:
  db_host: "{{ database.hostname }}"
  db_port: "{{ database.port }}"

# Keys the driver will populate in resolved_outputs
declared_outputs:
  - hostname
  - port
```

`declared_outputs` must cover the keys required by `produces`:

| produces | required outputs |
|---|---|
| `http` | `hostname`, `port` |
| `tcp` | `hostname`, `port` |
| `queue` | `queue_url` |

### Partition `config.yml` — IaC backend (Terraform / OpenTofu)

Set `backend: terraform` (or `opentofu`) to hand provisioning off to an IaC tool. Place `.tf` files alongside `config.yml` in the partition directory, and declare the variables you need via `inputs:`:

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
  nclav-driver/       Driver trait + DriverRegistry + LocalDriver + GcpDriver + TerraformBackend
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

- **Azure driver** — the `Driver` trait is defined; the Azure implementation is deferred
- **AWS driver**
- **Postgres store** — `StateStore` is designed for it; feature-flag planned
- **GCP platform bootstrap** — deploying the nclav API itself to Cloud Run + Cloud SQL (currently `--cloud gcp` runs the API server locally with the GCP driver for enclave provisioning)
- **IaC drift detection** — `terraform plan` on a schedule to detect out-of-band changes
- **Live log streaming** — IaC run logs are currently stored as a single blob after completion; streaming via SSE is future work
- **Web UI**
