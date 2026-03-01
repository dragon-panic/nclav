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

## Documentation

| Guide | What it covers |
|---|---|
| [Enclave YAML](docs/enclave-yaml.md) | Writing `config.yml` files for enclaves and partitions, template tokens, IaC wiring |
| [CLI reference](docs/cli-reference.md) | All subcommands, flags, and environment variables |
| [HTTP API reference](docs/api-reference.md) | All endpoints with examples |
| [Bootstrap — GCP](docs/bootstrap-gcp.md) | Hosting nclav on Cloud Run backed by Cloud SQL |
| [Bootstrap — Azure](docs/bootstrap-azure.md) | Hosting nclav on Container Apps backed by PostgreSQL Flexible Server |
| [Bootstrap — AWS](docs/bootstrap-aws.md) | Hosting nclav on ECS Fargate backed by RDS PostgreSQL |

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
