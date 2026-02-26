# Bootstrap — Deploying the nclav Platform

"Bootstrap" means deploying the nclav API server itself — the HTTP process, its
state store, and the credentials it needs to manage cloud resources. This is a
one-time, operator-level step. After bootstrap, you use `nclav apply` /
`nclav diff` / etc. against the running API; the bootstrap step does not repeat.

Bootstrap is **separate** from enclave provisioning. An API running on Cloud Run
can provision enclaves into GCP, local, or (future) Azure. The platform
location does not constrain where enclaves land.

| Concern | Answered by |
|---|---|
| Where the nclav API runs | bootstrap (this doc) |
| Where each enclave provisions | `cloud:` in enclave YAML (or API default) |

---

## Local (`nclav serve`)

For development and CI, run the API server locally with `nclav serve`. No cloud
infrastructure required.

```bash
# Persistent state (default) — redb at ~/.nclav/state.redb
nclav serve --cloud local

# Ephemeral — in-memory, lost when the process stops (CI / quick tests)
nclav serve --cloud local --ephemeral

# GCP default driver — enclaves without cloud: target GCP
nclav serve --cloud gcp \
  --gcp-parent folders/123456789 \
  --gcp-billing-account billingAccounts/XXXX-YYYY-ZZZZ

# Mixed: local default with GCP also available
nclav serve --cloud local \
  --enable-cloud gcp \
  --gcp-parent folders/123 \
  --gcp-billing-account billingAccounts/XXX
```

On first run, `nclav serve` generates a 64-character bearer token, writes it to
`~/.nclav/token` (mode 0600), and prints it. Subsequent restarts reuse the same
token so clients stay connected. Pass `--rotate-token` to force a new one.

When the `NCLAV_TOKEN` environment variable is set (non-empty), that value is
used as-is and no file is read or written — this is how the Cloud Run deployment
injects the token from Secret Manager.

---

## GCP hosted (`bootstrap/gcp/`)

For production, run nclav as a Cloud Run service so it stays up without a local
process. The `bootstrap/gcp/` directory contains a Terraform module that
provisions the entire platform in one `terraform apply`.

See [bootstrap/gcp/README.md](../../bootstrap/gcp/README.md) for the full
operator walkthrough. Quick summary:

**What it creates:**

| Resource | Name | Purpose |
|---|---|---|
| Service Account | `nclav-server@{project}.iam.gserviceaccount.com` | Identity for the Cloud Run service |
| GCS Bucket | `{project}-nclav-state` | Persistent storage for the redb state file (GCS volume mount) |
| Secret Manager secret | `nclav-api-token` | Bearer token for CLI authentication |
| Cloud Run service | `nclav-api` | The nclav API server |

**One-time setup:**

```sh
cd bootstrap/gcp

cat > terraform.tfvars <<EOF
project_id         = "my-platform-project"
region             = "us-central1"
nclav_image        = "ghcr.io/your-org/nclav:latest"
gcp_parent         = "folders/123456789"
billing_account    = "XXXXXX-YYYYYY-ZZZZZZ"
gcp_project_prefix = "myorg"
EOF

terraform init
terraform apply
```

**Connecting the CLI after bootstrap:**

```sh
export NCLAV_REMOTE=$(terraform output -raw api_url)
export NCLAV_TOKEN=$(terraform output -raw token_fetch_command | bash)

nclav status
nclav apply enclaves/
```

The Cloud Run service runs `nclav serve --bind 0.0.0.0 --cloud gcp ...`
internally, with the token injected from Secret Manager via `NCLAV_TOKEN`.

---

## State store

The store holds all enclave handles, outputs, Terraform state, IaC run logs,
and the audit event log.

| Mode | Store | Notes |
|---|---|---|
| `nclav serve --ephemeral` | In-memory | Lost on restart |
| `nclav serve` (default) | redb at `~/.nclav/state.redb` | Persistent; survives restarts |
| Cloud Run (`bootstrap/gcp/`) | redb on GCS volume mount | Persistent; survives service restarts and redeployments |

redb is used instead of SQLite: it is pure Rust (no C toolchain), ~200 KB,
and nclav's access patterns (keyed reads/writes, append-only event log) map
directly onto its typed table model.

---

## Per-enclave cloud targeting

The `cloud:` field in enclave YAML is independent of where the nclav API runs:

```yaml
id: product-a-dev
cloud: gcp          # always GCP regardless of API default
```

```yaml
id: dev-scratch
cloud: local        # always local even if API default is gcp
```

```yaml
id: product-b-staging
# cloud: absent — inherits the API's configured default
```

Omitting `cloud:` makes YAML portable: the same files work against a local
`nclav serve --cloud local` in development and a Cloud Run deployment with
`--cloud gcp` in production, without modification.

---

## Driver registry

The API maintains a `DriverRegistry`:

- `LocalDriver` is always registered — no credentials required
- Other drivers are registered at startup when their config is supplied
- The registry has a **default cloud** (set by `--cloud`)
- Each reconcile dispatches to the driver matching `enclave.cloud` (resolved)
- If an enclave references an unconfigured driver, reconcile fails with a clear error

Multiple drivers active simultaneously is normal — a GCP-default server with
some `cloud: local` enclaves is a supported and tested configuration.
