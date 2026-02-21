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
| Postgres | `--store postgres --store-url <url>` | External Postgres | Multi-operator or production-local |

Local bootstrap always registers LocalDriver. If GCP credentials and config are
also provided (via flags or env vars), GcpDriver is registered too — enabling
enclaves with `cloud: gcp` to provision into GCP from a locally-running API.

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
needed: `RedbStore` (local dev) and `PostgresStore` (production). Store
selection does not affect which drivers are available.
