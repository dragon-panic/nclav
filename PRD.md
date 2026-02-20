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
      ├── AzureDriver     (v1)
      ├── AwsDriver       (future)
      └── GcpDriver       (future)
```

The domain model describes intent. Drivers produce cloud-specific handles — opaque receipts stored in state so the driver can locate its resources on subsequent reconciles. The reconciler never reads inside handles. Only the driver that produced them reads them.

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
ProvisionedEnclave
  enclave: Enclave
  handles: AzureHandles | AwsHandles | LocalHandles
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
                                     diff desired → actual state
                                     invoke driver (azure or local)
                                     store state + handles
                                     ◄──────────────────────────
{ status, changes } ◄───────────────┘
  │
exit 0 / 1
```

nclav does not poll git. The pipeline owns the trigger.

---

## Storage

Postgres stores:
- Enclave and partition desired state
- Driver handles (cloud-specific, opaque to domain)
- Terraform state per partition
- Event log (append-only) for audit and drift detection

---

## Bootstrap

One-time setup using cloud admin credentials.

```
$ nclav bootstrap --cloud azure --region eastus2

✓ Created subscription: nclav-platform
✓ Created resource group: nclav-core
✓ Created vnet: 10.255.0.0/16
✓ Provisioned postgres
✓ Deployed nclav service
✓ Configured exports: api (http, token), web (http, none)
✓ Created seed identity

nclav endpoint: https://nclav.acme.com
Admin token:    nclav_xxxxxxxx

Save this token. You can now revoke your cloud admin credentials.
```

For local:

```
$ nclav bootstrap --cloud local

✓ Provisioned local postgres
✓ Started nclav service
✓ Configured local resolver

nclav endpoint: http://localhost:8080
Admin token:    nclav_xxxxxxxx
```

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
- **Clouds (v1):** Local, Azure

---

## Non-Goals (v1)

- AWS and GCP drivers (abstraction layer is built; drivers are future)
- Org-level policy enforcement and compliance controls
- Template and mixin system
- Intra-enclave NSG enforcement (declarations generated, compliance not verified)
- Web UI
- In-enclave service mesh or workload-to-workload zero trust
- Git polling (pipelines push to API)
- Multi-tenant SaaS