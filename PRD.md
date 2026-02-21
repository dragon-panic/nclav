# nclav — PRD v2

## The Problem

Cloud environments accumulate shared state. Developers stomp on each other's resources, costs are invisible until the bill arrives, and networking is either locked down so hard nothing works or open so wide nothing is safe. The fix is usually a platform team writing bespoke automation that only they understand.

**nclav's answer:** isolation as the default primitive, not an afterthought. A YAML file describes intent. The reconciler enforces it. The cloud is an implementation detail.

---

## Design Principles

**Cloud-agnostic domain model.** Data structures describe intent, not cloud resources. Azure-specific handles live at the edges, inside drivers, opaque to everything else. Adding a new cloud target means adding a driver, not touching the domain.

**Declarations are the interface.** Everything that crosses a boundary — network, auth, credentials — must be declared. The graph of what talks to what is readable from YAML alone, without reading Terraform or app code.

**Local driver as abstraction proof.** A local driver runs the full reconcile loop with no cloud credentials. If the YAML has to change for local vs Azure, the abstraction is broken. The local driver is the enforcement mechanism for that constraint.

---

## Core Primitives

### Enclave

An enclave is the outer isolation boundary. It bounds:

- **Network** — a private /16 address space, non-routable from outside by default
- **Cost** — all resources roll up to a single billing boundary
- **Identity** — a service principal scoped to create resources inside; cannot modify the enclave's own network, permissions, or billing linkage

### Partition

A partition is a contained unit of infrastructure within an enclave — a set of resources roped off with a declared interface. Partitions have typed outputs and declare what they produce. App developers own the Terraform inside a partition; nclav owns the boundary around it.

**Enclaves contain partitions. Partitions produce typed outputs. Exports expose those outputs. Imports consume them.**

---

## Imports and Exports

Imports and exports operate at two scopes with different enforcement mechanisms:

| Scope | Network | Boundary enforcement |
|---|---|---|
| Cross-enclave | Non-routable by default | Private Link, DNS, driver provisioning |
| Intra-enclave | Flat /16 | NSG generation from declarations (intent; not yet enforced) |

The same YAML shape applies at both scopes. The driver behavior underneath differs.

### Export Types

The export type determines what the driver provisions and what the importer receives. It is the discriminator everything else flows from.

| `type:` | What it is | Driver provisions |
|---|---|---|
| `http` | HTTP/HTTPS endpoint | Load balancer, TLS termination, health probes |
| `tcp` | Raw TCP host:port | NLB, Private Link Service, TCP health probes |
| `queue` | Event stream or message queue | Broker topic/queue, credentials, no network primitive |

Auth options valid per type:

| `type:` | Valid `auth:` |
|---|---|
| `http` | none, token, oauth, mtls |
| `tcp` | native, mtls |
| `queue` | native, token |

Type mismatches and invalid auth combinations are caught at parse time before anything is provisioned.

### Export Targets

| `to:` | Scope |
|---|---|
| `public` | Internet |
| `enclave:name` | Single named enclave, Private Link |
| `enclave:*` | Any enclave, Private Link with approval |
| `vpn` | Authenticated humans via Point-to-Site VPN |
| `partition:name` | Another partition within the same enclave |

### Enclave Exports

```yaml
exports:
  - name: api
    target: api             # References api/ partition
    type: http
    to: public
    auth: token
    hostname: api.product-a.acme.com

  - name: events
    target: events          # References events/ partition
    type: queue
    to: enclave:*
    auth: native
```

### Enclave Imports

```yaml
imports:
  - from: enclave:shared-db
    export: postgres
    as: main-db
```

When nclav reconciles a cross-enclave import:
1. Validates the source exports to this enclave
2. Creates Private Link Service on exporter (if not exists)
3. Creates Private Endpoint on importer
4. Registers DNS entry for local resolution

### Partition Exports and Imports

Partitions declare their interface the same way. Within an enclave, cross-partition declarations are used to:
- Generate NSG rules expressing least-privilege intent
- Provide a readable communication graph
- Thread output references to consuming partitions

Intra-enclave NSG generation from declarations is implemented. Enforcement — ensuring Terraform inside partitions actually complies — is a future concern.

```yaml
# partitions within an enclave referencing each other
exports:
  - name: postgres
    type: tcp
    port: 5432
    to: partition:api
    auth: native

imports:
  - from: partition:db
    export: postgres
    as: database
```

---

## Partition Interface

A partition declares what it produces, and outputs that other partitions or the enclave boundary can reference.

```yaml
# enclaves/product-a/dev/api/config.yml
name: api
produces: http

inputs:
  DATABASE_URL: "{{ database.connection_string }}"   # from declared import

outputs:
  - endpoint_url
  - managed_identity_id
```

The reconciler validates that `produces` matches the export `type` that targets this partition. A partition declaring `produces: http` must emit `endpoint_url` in its Terraform outputs. The driver uses that output to wire the load balancer backend — it does not inspect the Terraform internals.

**Produces → required outputs:**

| `produces:` | Required Terraform outputs |
|---|---|
| `http` | `endpoint_url` |
| `tcp` | `host`, `port` |
| `queue` | `connection_string`, `topic_name` |

---

## Driver Architecture

The reconciler operates on the cloud-agnostic domain model. Drivers are invoked at the edges.

```
Reconciler (domain)
      │
      ▼
DriverInterface
      ├── LocalDriver     (v1 — no credentials, in-process state)
      ├── GcpDriver       (v1 — Cloud Run, Pub/Sub, VPC, IAM)
      ├── AzureDriver     (future)
      └── AwsDriver       (future)
```

The domain model describes intent. Drivers produce cloud-specific handles — opaque receipts stored in state so the driver can locate its resources on subsequent reconciles. The reconciler never reads inside handles. Only the driver that produced them reads them.

Drivers implement two categories of methods:

- **Mutating** (`provision_*`, `teardown_*`) — create, update, or delete cloud resources; called by the reconcile loop
- **Reading** (`observe_*`) — read current cloud state without modifying anything; called for drift detection

```
# Domain — cloud agnostic
Enclave
  name, owner, network_spec, identity_spec
  imports: [Import]
  exports: [Export]

Partition
  name, produces, inputs, outputs
  imports: [Import]
  exports: [Export]

# Driver output — opaque to domain
ProvisionResult
  handle:  Handle                    // opaque cloud ID receipt
  outputs: HashMap<String, String>   // resolved output values

ObservedState
  exists:   bool
  healthy:  bool
  outputs:  HashMap<String, String>  // current values from cloud
  raw:      Handle                   // full cloud response, for debugging
```

### Local Driver

The local driver is a first-class v1 target, not a future convenience.

It runs the full reconcile loop — YAML parsing, graph validation, import/export resolution, type checking — with no cloud credentials. Partitions are not provisioned; their declared outputs are stubbed with local values. Private endpoints become localhost port mappings. DNS is written to a local resolver.

**What the local driver validates:**
- Import/export graph (no dangling references, no missing exports)
- Type compatibility (export type matches partition `produces`)
- Auth validity per export type
- Circular dependency detection
- Cross-enclave access (source must export to this enclave)
- Output contract (partition outputs satisfy `produces` requirements)

If the YAML requires changes to run locally vs on Azure, the abstraction has leaked. Local driver parity is a hard requirement.

---

## GitOps Structure

Each enclave is a directory. The `config.yml` defines the boundary. Partitions are subdirectories.

```
enclaves/
├── product-a/
│   ├── dev/
│   │   ├── config.yml          # Enclave: metadata, imports, exports
│   │   ├── api/
│   │   │   ├── config.yml      # Partition: produces http
│   │   │   └── main.tf
│   │   └── db/
│   │       ├── config.yml      # Partition: produces tcp
│   │       └── main.tf
│   └── prod/
│       ├── config.yml
│       ├── api/
│       │   ├── config.yml
│       │   └── main.tf
│       └── db/
│           ├── config.yml
│           └── main.tf
└── shared-db/
    └── prod/
        ├── config.yml
        └── postgres/
            ├── config.yml
            └── main.tf
```

### Enclave Config

```yaml
# enclaves/product-a/dev/config.yml
name: product-a-dev
owner: team-alpha
cost_center: CC-1234
cloud: azure                    # or: local
region: eastus2

dns:
  parent: dev.internal.acme.com

imports:
  - from: enclave:shared-db
    export: postgres
    as: main-db

exports:
  - name: api
    target: api
    type: http
    to: public
    auth: token
    hostname: api.product-a-dev.acme.com
```

### Partition Config

```yaml
# enclaves/product-a/dev/api/config.yml
name: api
produces: http

imports:
  - from: partition:db
    export: postgres
    as: database

inputs:
  DATABASE_URL: "{{ database.connection_string }}"

outputs:
  - endpoint_url
```

```yaml
# enclaves/product-a/dev/db/config.yml
name: db
produces: tcp

exports:
  - name: postgres
    type: tcp
    port: 5432
    to: partition:api
    auth: native

outputs:
  - host
  - port
  - connection_string
```

---

## Data Flow

```
CI Pipeline                         nclav
───────────                         ─────
git push
  │
pipeline runs
  │
POST /reconcile ───────────────────► parse + validate YAML
                                     resolve import/export graph
                                     hash desired config per resource
                                     diff desired_hash → stored_hash
                                     skip unchanged resources
                                     invoke driver for changes
                                       set status = Provisioning/Updating
                                       on success: status = Active
                                                   update timestamps + hash
                                       on failure: status = Error
                                                   persist last_error
                                     store handles + resolved_outputs
                                     increment generation
                                     append audit event
                                     ◄──────────────────────────
{ status, changes, errors } ◄───────┘
  │
exit 0 / 1
```

nclav does not poll git. The pipeline owns the trigger.

---

## State Model

nclav tracks three distinct layers of state for every enclave and partition.
Understanding the difference between them is important — conflating them is
how state management goes wrong.

| Layer | What it is | Where it lives |
|---|---|---|
| **Desired** | What the YAML says right now | Loaded fresh on every reconcile from the GitOps directory |
| **Applied** | What the driver last successfully provisioned | Persisted: `Handle` (cloud ID receipt) + `resolved_outputs` |
| **Observed** | What actually exists in the cloud right now | Fetched on demand via `Driver::observe()`; not cached permanently |

The reconciler diffs **desired vs applied** to decide what to change. The
observer diffs **observed vs applied** to detect cloud drift. These are separate
concerns and run on separate schedules.

### Resource metadata

Every enclave and partition in the store carries a `ResourceMeta` block:

```
ResourceMeta {
    status:        ProvisioningStatus   // lifecycle state (see below)
    created_at:    DateTime<Utc>?       // first successful provision
    updated_at:    DateTime<Utc>?       // last successful update
    last_seen_at:  DateTime<Utc>?       // last Driver::observe() confirmed it exists
    last_error:    ResourceError?       // most recent failure, if any
    desired_hash:  String?              // SHA-256 of desired config at last apply
    generation:    u64                  // monotonically increasing; for optimistic concurrency
}
```

`desired_hash` is what makes the common case cheap: before diffing the full
config struct, compare hashes. If they match, the config hasn't changed since
the last apply and provisioning can be skipped without further work.

`last_error` is persisted — not just returned from the API call — so that
`nclav status` can surface it without re-running anything. An error from three
hours ago should be visible without waiting for the next reconcile.

`generation` increments on every successful write. When two reconcile runs
overlap (pipeline triggered twice, manual run while CI is in-flight), the
second write will fail if its generation is stale. The loser retries from
scratch rather than silently clobbering the winner's state.

### Lifecycle states

```
              ┌─────────────────────────────────────────────────┐
              │                                                 │
  (YAML seen) │                                                 ▼
   Pending ──► Provisioning ──► Active ──► Updating ──► Active
                    │                         │
                    ▼                         ▼
                  Error ◄────────────────── Error
                    │
                    │  (YAML removed)
              Active ──► Deleting ──► Deleted
```

| Status | Meaning |
|---|---|
| `Pending` | Resource is in desired config but provision has not started |
| `Provisioning` | Driver call in-flight for initial creation |
| `Active` | Last provision/update succeeded; resource should exist |
| `Updating` | Driver call in-flight for an update |
| `Degraded` | `observe()` returned success but reported unhealthy state |
| `Error` | Last driver call failed; `last_error` is populated |
| `Deleting` | Driver teardown call in-flight |
| `Deleted` | Teardown confirmed; record retained briefly for audit |

Only `Active` resources are included in drift detection and graph rendering.
`Error` resources appear in `nclav status` with their error message and
timestamp. A failed resource does not block the rest of the reconcile unless
other resources depend on its outputs.

### Two kinds of drift

**Config drift** — the YAML changed since the last apply. Detected cheaply by
comparing `ResourceMeta.desired_hash` against a hash of the current YAML.
Resolved by running `nclav apply`.

**Cloud drift** — the cloud resource diverged from what was applied (manual
change, cloud-side event, resource deleted outside nclav). Detected by calling
`Driver::observe()` and comparing the result against the stored `Handle` and
`resolved_outputs`. Observable on demand via `GET /enclaves/{id}` with
`?observe=true`, or on a background schedule when running as a server.

Neither type of drift is auto-corrected. nclav reports drift; the operator
decides when to apply. This is intentional — silent auto-correction can
destroy in-flight changes during a deployment.

### What nclav does NOT store

- Full cloud resource attribute maps (that is the driver's concern via the Handle)
- Terraform state (Terraform manages its own state; nclav stores the outputs)
- Historical versions of desired config (the Git log is the history)
- Credentials or secrets (injected at runtime, never persisted)

---

## Storage

Postgres stores:
- Enclave and partition state records (`ResourceMeta` + `Handle` + `resolved_outputs`)
- Export and import handles (wiring receipts, keyed by name and alias)
- Append-only audit event log (reconcile runs, provision outcomes, drift events)

The in-memory store (`InMemoryStore`) implements the same `StateStore` trait
and is used for local mode and all tests. Postgres is the production backend.
State is never stored in the YAML directory — the GitOps directory is read-only
from nclav's perspective.

---

## Bootstrap

Bootstrap provisions the nclav **platform** — the API server and its state
store — so the system runs persistently without a local process. It is a
one-time, idempotent operation distinct from enclave provisioning.

The platform location (`bootstrap --cloud`) and each enclave's target cloud
(`cloud:` in YAML) are independent. An API running on GCP can provision
enclaves into GCP, local, or Azure simultaneously. Enclaves omitting `cloud:`
inherit the API's configured default.

See [BOOTSTRAP.md](BOOTSTRAP.md) for full design rationale, local/GCP
bootstrap specs, per-enclave cloud targeting, the DriverRegistry architecture,
and all design decisions.

---

## CLI

```bash
nclav bootstrap              # One-time platform setup
nclav apply ./enclaves       # Reconcile against actual state
nclav diff ./enclaves        # Show what apply would change
nclav status                 # Enclave health and drift summary
nclav graph ./enclaves       # Render full import/export graph
```

---

## Technology

- **Language:** Rust
- **Database:** Postgres
- **IaC:** Terraform
- **Clouds (v1):** Local, GCP

---

## Non-Goals (v1)

- Azure and AWS drivers (abstraction layer is built; drivers are future)
- Org-level policy enforcement and compliance controls
- Template and mixin system
- Intra-enclave NSG enforcement (declarations generated, compliance not verified)
- Web UI
- In-enclave service mesh or workload-to-workload zero trust
- Git polling (pipelines push to API)
- Multi-tenant SaaS