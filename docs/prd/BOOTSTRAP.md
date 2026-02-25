# Bootstrap — Product Design

This document covers the design of `nclav bootstrap`: what it does, how the
"which cloud" question is answered at the platform level and the enclave level,
and the key decisions that shape the implementation.

See [README.md](README.md) for operator invocation reference.
See [GCP.md](GCP.md) for what GCP bootstrap provisions in detail.

---

## Core: two concerns, one command

Bootstrap answers a single question: **where does the nclav API run and how
does it persist state?** This is the *platform* question. It is separate from
the *enclave* question: which cloud does a given enclave provision into?

| Concern | Answered by |
|---|---|
| Where the nclav API runs | `bootstrap --cloud <target>` |
| Where each enclave provisions | `cloud:` in enclave YAML (or API default) |

These two concerns are orthogonal. An API running on GCP can provision enclaves
into GCP, local, or (future) Azure. An API running locally can provision
enclaves into GCP. The platform location does not constrain enclave placement.

---

## What bootstrap produces

After a successful bootstrap:

1. A **running API endpoint** — the HTTP server operators point `NCLAV_URL` at
2. A **persistent state store** — survives restarts; holds enclave handles, outputs, audit log
3. A set of **configured drivers** — at minimum the default cloud driver; local is always available

Bootstrap is **one-time and idempotent**. Re-running detects existing resources
and skips them. A timed-out bootstrap can be safely re-run.

---

## Local bootstrap

Runs the nclav API as a local process. No cloud credentials required. Three
modes covering different operator needs:

| Mode | Flag | Persistence | When to use |
|---|---|---|---|
| Ephemeral | `--ephemeral` | In-memory; lost on restart | CI, quick tests, throwaway runs |
| Persistent | *(default)* | redb at `~/.nclav/state.redb` | Daily single-operator dev |

Every driver must be explicitly opted in — nothing is registered automatically. The
default cloud (`--cloud`) registers its driver; additional drivers require `--enable-cloud`:

```bash
# local only (default)
nclav bootstrap --cloud local

# local default + GCP also available for mixed enclaves
nclav bootstrap --cloud local \
  --enable-cloud gcp \
  --gcp-parent folders/123 \
  --gcp-billing-account billingAccounts/XXX

# GCP default only (no local unless explicitly requested)
nclav bootstrap --cloud gcp \
  --gcp-parent folders/123 \
  --gcp-billing-account billingAccounts/XXX

# GCP default + local also available
nclav bootstrap --cloud gcp \
  --gcp-parent folders/123 \
  --gcp-billing-account billingAccounts/XXX \
  --enable-cloud local
```

---

## GCP bootstrap

Provisions a dedicated **platform GCP project** that hosts the nclav API and
its state store. After bootstrap the API runs continuously on Cloud Run; the
operator no longer needs to keep a local process alive.

**Platform project contents:**
- Cloud Run service — the nclav HTTP API
- Cloud SQL Postgres — persistent state store
- Service Account — identity used by the API to provision enclave projects
- (Optional, not default) Cloud DNS record — stable human-readable URL

**Auth flow:** Bootstrap runs locally via ADC (operator's `gcloud` credentials
or a bootstrap SA). It creates the platform project and deploys the API. After
that, all API calls use the platform SA — the operator's credentials are no
longer needed for day-to-day use.

**Default cloud for enclaves:** GcpDriver is registered and set as the API
default. Enclaves without an explicit `cloud:` provision into GCP.

---

## Per-enclave cloud targeting

### The `cloud:` field in YAML

```yaml
id: product-a-dev
cloud: gcp          # explicit — always use GCP driver
```

```yaml
id: dev-scratch
cloud: local        # always local — even if API default is gcp
```

```yaml
id: product-b-staging
# cloud: omitted — inherits the API's configured default
```

`cloud:` is **optional**. When absent, the API's configured default cloud is
used. The effective cloud is resolved at reconcile time and stored in state —
the display always shows a resolved value, never blank or "inherited".

Making `cloud:` optional is intentional: the same YAML works in a local dev
environment (default: local) and a production GCP environment (default: gcp)
without modification. Cloud targeting is a deployment concern, not a code
concern.

### What `cloud:` does not mean

- Not where the nclav API runs (that is the bootstrap concern)
- Not a billing boundary (billing is handled per-enclave-project on GCP)
- Not a graph constraint (graph validation is cloud-agnostic)

### Display

The API and CLI always show the resolved cloud — explicit or defaulted:

```
Enclaves (3):
  product-a-dev    [gcp]    active    2 partitions
  dev-scratch      [local]  active    1 partition
  product-b-stg    [gcp]    active    3 partitions   ← cloud: was omitted; default applied
```

---

## Driver registry

The API maintains a `DriverRegistry` rather than a single global driver:

- LocalDriver is **always registered** — no credentials required
- Other drivers are registered when their config is supplied at startup
- The registry has a **default cloud** (set by `bootstrap --cloud`)
- Each reconcile dispatches to the driver matching `enclave.cloud` (resolved)
- If an enclave references an unconfigured driver, reconcile fails with a clear error

Multiple drivers active simultaneously is supported and expected — a GCP-default
API with some `cloud: local` enclaves is a normal configuration.

---

## State store

The store is a **deployment-level concern**, shared across all enclaves and
all clouds in a given nclav installation.

| Bootstrap | Default store | Override |
|---|---|---|
| `local --ephemeral` | In-memory | — |
| `local` | redb (`~/.nclav/state.redb`) | `--store postgres --store-url <dsn>` |
| `gcp` | Cloud SQL Postgres (provisioned by bootstrap) | `--store-url <dsn>` (use existing) |

**redb** is chosen over SQLite for local persistent storage: it is pure Rust
(no C toolchain dependency, no bundled C source to compile), has a significantly
smaller binary footprint (~200 KB vs ~1–2 MB for bundled SQLite), and faster
cold build times. nclav's access patterns — keyed reads/writes and an
append-only event log — map directly onto redb's typed table model without
needing SQL. SQLite is skipped entirely; there is no middle tier between redb
and Postgres.

The `StateStore` trait is already cloud-agnostic. Implementation additions
needed: `RedbStore` (local dev — **done**) and `PostgresStore` (production).
Store selection does not affect which drivers are available.

---

## Platform secrets

Bootstrap generates a bearer token that all CLI-to-API communication requires.
The token must survive process restarts and, for GCP, must be readable by
stateless Cloud Run instances without ever appearing in plaintext in environment
variables, deployment manifests, or container image layers.

### The `SecretStore` abstraction

The driver layer exposes a `SecretStore` trait that lives alongside `Driver` in
`nclav-driver`. Both implementations are registered in the `DriverRegistry`:

```rust
// nclav-driver/src/secret_store.rs (to be created)
#[async_trait]
pub trait SecretStore: Send + Sync {
    /// Write (or overwrite) a named secret value.
    async fn write(&self, name: &str, value: &str) -> Result<(), DriverError>;
    /// Read a named secret. Returns None if it does not exist.
    async fn read(&self, name: &str) -> Result<Option<String>, DriverError>;
    /// Delete a named secret entirely.
    async fn delete(&self, name: &str) -> Result<(), DriverError>;
}
```

The `DriverRegistry` holds one `Arc<dyn SecretStore>` bound to the default
cloud (the platform). Secrets are a **platform-level concern**, not per-enclave:
the same store is used regardless of which cloud individual enclaves target.

```rust
// Extension to DriverRegistry (nclav-driver/src/registry.rs)
pub struct DriverRegistry {
    pub default_cloud: CloudTarget,
    drivers: HashMap<CloudTarget, Arc<dyn Driver>>,
    secret_store: Arc<dyn SecretStore>,   // ← new field
}

impl DriverRegistry {
    pub fn secret_store(&self) -> Arc<dyn SecretStore> {
        Arc::clone(&self.secret_store)
    }
}
```

Bootstrap constructs the registry with the appropriate secret store before
starting the server.

### Per-mode secret backend

| Bootstrap mode | Secret backend | Token location |
|---|---|---|
| `local --ephemeral` | `LocalSecretStore` | `~/.nclav/secrets/nclav-api-token` (same file as today; in-memory option would be lossy) |
| `local` | `LocalSecretStore` | `~/.nclav/secrets/nclav-api-token` |
| `gcp` | `GcpSecretStore` | `projects/{platform-project}/secrets/nclav-api-token/versions/latest` |

`LocalSecretStore` is a thin wrapper over the current file-based approach.
The token file path `~/.nclav/token` can be migrated to `~/.nclav/secrets/nclav-api-token`
as part of this work, or kept at its current location (just change who writes it).

### Token lifecycle

1. **First bootstrap:** generate token → `secret_store.write("nclav-api-token", token)`
2. **Restart, no `--rotate-token`:** `secret_store.read("nclav-api-token")` → reuse if present
3. **`--rotate-token`:** generate new token → `secret_store.write(...)` overwrites (or adds a new Secret Manager version)
4. **GCP Cloud Run startup:** the running binary calls `secret_store.read("nclav-api-token")` via the `nclav-runner` SA; no token is passed as an environment variable

### Effect on `build_app` and API startup

Currently `commands::bootstrap` resolves the token from the local file and
passes it explicitly to `build_app(store, registry, Arc::new(token))`. This
contract does not change — `build_app` still takes a concrete `Arc<String>`.

What changes is **who resolves the token and when**:

- **Local bootstrap:** unchanged. CLI reads/generates token from file, passes it in.
- **GCP Cloud Run binary:** on startup, before calling `build_app`, the binary calls:
  ```rust
  let token = registry.secret_store()
      .read("nclav-api-token")
      .await?
      .context("nclav-api-token missing from Secret Manager — was bootstrap run?")?;
  let app = nclav_api::build_app(store, registry, Arc::new(token));
  ```

The Cloud Run entrypoint is a separate binary path (or a `--serve` flag) that
does not write the token — it only reads it. Bootstrap always runs locally
and always writes the token first.

### LocalSecretStore implementation sketch

```rust
pub struct LocalSecretStore {
    dir: PathBuf,   // ~/.nclav/secrets/
}

impl LocalSecretStore {
    pub fn new(dir: PathBuf) -> Self { Self { dir } }
}

#[async_trait]
impl SecretStore for LocalSecretStore {
    async fn write(&self, name: &str, value: &str) -> Result<(), DriverError> {
        let path = self.dir.join(name);
        fs::create_dir_all(&self.dir)?;
        fs::write(&path, value)?;
        // chmod 600
        Ok(())
    }
    async fn read(&self, name: &str) -> Result<Option<String>, DriverError> {
        match fs::read_to_string(self.dir.join(name)) {
            Ok(s) => Ok(Some(s.trim().to_string())),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(DriverError::from(e)),
        }
    }
    async fn delete(&self, name: &str) -> Result<(), DriverError> {
        let path = self.dir.join(name);
        if path.exists() { fs::remove_file(path)?; }
        Ok(())
    }
}
```

### Files to create / modify

| File | Action |
|---|---|
| `nclav-driver/src/secret_store.rs` | New — `SecretStore` trait + `LocalSecretStore` |
| `nclav-driver/src/gcp_secret.rs` | New — `GcpSecretStore` (Secret Manager REST) |
| `nclav-driver/src/registry.rs` | Add `secret_store` field; `new()` takes `Arc<dyn SecretStore>` |
| `nclav-driver/src/lib.rs` | Export new modules |
| `nclav-cli/src/commands.rs` | Bootstrap builds `LocalSecretStore` or `GcpSecretStore`; reads/writes token through it |
| `nclav-api/src/app.rs` | No change — still takes `Arc<String>`; token resolution happens before `build_app` |
