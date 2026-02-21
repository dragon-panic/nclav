# nclav

Cloud infrastructure orchestration via a YAML-driven reconcile loop. nclav manages **enclaves** — isolated network/cost/identity boundaries — and the **partitions** (infrastructure units) inside them. Dependencies between enclaves are declared explicitly through import/export contracts and validated before anything is provisioned.

## Concepts

| Term | Meaning |
|---|---|
| **Enclave** | An isolated boundary (VPC, subscription, cost center). Owns its partitions and declares what it exports to other enclaves. |
| **Partition** | A deployable unit inside an enclave (a service, a database, a queue). Declares what it produces and what it imports. |
| **Export** | A named endpoint an enclave makes available to others, with type (`http`, `tcp`, `queue`) and access control (`to:`). |
| **Import** | A reference to another enclave's export, given a local alias used for template substitution. |
| **Reconcile** | The loop that diffs desired state (YAML) against actual state (store), then provisions/tears down in dependency order. |

## Requirements

- Rust 1.75+ (the workspace uses async traits and the 2021 edition)
- `cargo` on your PATH
- No cloud credentials required for local mode

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
nclav [--remote <url>] <command>
```

`bootstrap` starts the API server. All other commands (`diff`, `apply`, `graph`, `status`) send HTTP requests to it — defaulting to `http://localhost:8080`. Pass `--remote <url>` (or set `NCLAV_URL`) to target a remote server.

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

Reconcile and apply: same as `diff` but actually provisions resources and persists state.

### `nclav graph [--output text|json|dot] [--enclave <id>]`

Render the dependency graph from the running server's applied state.

```bash
# Human-readable
nclav graph

# Graphviz — pipe to dot for an SVG
nclav graph --output dot | dot -Tsvg > graph.svg

# Machine-readable JSON
nclav graph --output json

# Filter to one enclave
nclav graph --enclave product-a-dev
```

### `nclav bootstrap`

Bootstrap provisions the nclav **platform** — the API server and its state
store. It is a one-time, idempotent operation. After bootstrap completes,
the API runs continuously and all other CLI commands talk to it via HTTP.

The platform location (set by `--cloud`) and each enclave's target cloud
(`cloud:` in YAML) are **independent**. An API running on GCP can provision
enclaves into GCP, local, or Azure simultaneously. See [BOOTSTRAP.md](BOOTSTRAP.md)
for full design rationale.

#### Local bootstrap

```bash
# Ephemeral — in-memory state, lost on restart (CI / quick tests)
nclav bootstrap --cloud local --ephemeral

# Persistent — redb at ~/.nclav/state.redb (default; single-operator dev)
nclav bootstrap --cloud local
```

Binds `http://127.0.0.1:8080` by default. Use `--bind 0.0.0.0` (or `NCLAV_BIND`) to
expose on all interfaces. Override the port with `--port` / `NCLAV_PORT`.

On first run a 64-character bearer token is generated and written to `~/.nclav/token`
(mode 0600). Subsequent restarts reuse the same token automatically — clients stay
connected. Pass `--rotate-token` to force a new token (invalidates existing clients).

LocalDriver is always available; enclaves with `cloud: local` (or no `cloud:` when the
default is local) use it.

#### GCP bootstrap

Initialises the GCP driver (via Application Default Credentials) and starts the API
server locally with GCP as the default cloud. Enclave provisioning targets real GCP
projects; the API process itself still runs on your machine.

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

**Partition type mapping (GCP driver):**

| Partition `produces` | GCP resource |
|---|---|
| `http` | Cloud Run service |
| `tcp` | Externally managed — reads `hostname`/`port` from partition `inputs:` |
| `queue` | Pub/Sub topic + subscription |

See [GCP.md](GCP.md) for full GCP driver and bootstrap reference.

### `nclav status`

Prints a summary of enclave health from the server. Includes enclave count, default cloud,
and active drivers.

## HTTP API

Start the server with `nclav bootstrap`, then use the token from `~/.nclav/token`.
All endpoints except `/health` and `/ready` require `Authorization: Bearer <token>`.

| Method | Path | Description |
|---|---|---|
| `GET` | `/health` | Always 200 OK (no auth required) |
| `GET` | `/ready` | 200 if store is reachable (no auth required) |
| `POST` | `/reconcile` | Apply changes |
| `POST` | `/reconcile/dry-run` | Diff only |
| `GET` | `/enclaves` | List all enclave states |
| `GET` | `/enclaves/{id}` | Single enclave state |
| `GET` | `/enclaves/{id}/graph` | Import/export graph for one enclave |
| `GET` | `/graph` | System-wide dependency graph |
| `GET` | `/events` | Audit log (`?enclave_id=&limit=`) |
| `GET` | `/status` | Summary: enclave count, default cloud, active drivers |

```bash
# Start the server
cargo run -p nclav-cli -- bootstrap --cloud local &

TOKEN=$(cat ~/.nclav/token)

# Apply via HTTP
curl -X POST http://localhost:8080/reconcile \
  -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"enclaves_dir": "./enclaves"}'

# Dry-run via HTTP
curl -X POST http://localhost:8080/reconcile/dry-run \
  -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"enclaves_dir": "./enclaves"}'

# Check status
curl -H "Authorization: Bearer $TOKEN" http://localhost:8080/status

# Audit log
curl -H "Authorization: Bearer $TOKEN" 'http://localhost:8080/events?limit=20'
```

## Writing enclave YAML

An enclaves directory has one subdirectory per enclave. Each enclave subdirectory contains a `config.yml` and one subdirectory per partition.

```
enclaves/
  product-a/dev/
    config.yml          <- enclave declaration
    api/
      config.yml        <- partition declaration
    db/
      config.yml        <- partition declaration
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

### Partition `config.yml`

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

## Workspace layout

```text
crates/
  nclav-domain/       Pure types — no I/O
  nclav-config/       YAML parsing, Raw* -> domain conversion
  nclav-graph/        Petgraph validation: dangling imports, access control, cycles, topo sort
  nclav-store/        StateStore trait + InMemoryStore + RedbStore (persistent local)
  nclav-driver/       Driver trait + DriverRegistry + LocalDriver + GcpDriver
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
RUST_LOG=nclav_reconciler=info cargo run -p nclav-cli -- apply ./enclaves
```

## What's not implemented yet

- **Azure driver** — the `Driver` trait is defined; the Azure implementation is deferred
- **AWS driver**
- **Postgres store** — `StateStore` is designed for it; feature-flag planned
- **GCP platform bootstrap** — deploying the nclav API itself to Cloud Run + Cloud SQL (currently `--cloud gcp` runs the API server locally with the GCP driver for enclave provisioning)
- **Web UI**
