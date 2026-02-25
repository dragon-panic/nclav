# GCP Driver Reference

This document maps every nclav concept to a concrete GCP primitive, specifies
the exact API calls the driver makes at each method, and defines what the opaque
`Handle` and output map look like for GCP resources.

---

## Concept → GCP primitive mapping

| nclav concept | GCP primitive | Notes |
|---|---|---|
| `Enclave` | **Project** | Billing, IAM, API enablement, and network are all project-scoped |
| `Enclave.identity` | **Service Account** | One SA per enclave; name taken from `identity` field |
| `Partition` (identity) | **Service Account** | One SA per partition: `partition-{id}@{project}.iam.gserviceaccount.com` |
| `Enclave.network` | **VPC network + subnets** | Custom-mode VPC; one subnet per entry in `subnets` list |
| `Enclave.dns.zone` | **Cloud DNS managed zone** | Private zone, visible only within the VPC |
| `Partition` (http) | **Cloud Run service** | Serverless; region from enclave |
| `Partition` (tcp) | **externally managed** | nclav validates wiring; hostname/port read from partition `inputs:` |
| `Partition` (queue) | **Pub/Sub topic** | One topic per partition; subscriptions created at import time |
| `Export` (http) | **Cloud Run IAM binding** + URL | IAM: `roles/run.invoker` on the service |
| `Export` (tcp) | **Private Service Connect** endpoint | Exposes a service across project boundaries without VPC peering |
| `Export` (queue) | **Pub/Sub topic** + IAM binding | `roles/pubsub.publisher` granted to importer SA |
| `Import` (http) | **OIDC token source** config | Importer gets the URL and its SA gets `run.invoker` on target |
| `Import` (tcp) | **PSC endpoint** in importer VPC | DNS record created in importer's Cloud DNS zone |
| `Import` (queue) | **Pub/Sub subscription** | Cross-project subscription created in importer's project |
| Cross-enclave `to: vpn` | **Cloud VPN** or **Cloud Interconnect** | HA VPN with BGP; out of scope for initial GCP driver |

---

## Auth type → GCP mechanism

| Export type | auth: none | auth: token | auth: oauth | auth: mtls | auth: native |
|---|---|---|---|---|---|
| `http` | Cloud Run allow-unauthenticated | OIDC token (metadata server) | OAuth2 user identity | **Certificate Authority Service** client cert | — |
| `tcp` | No auth on PSC endpoint | — | — | CAS mutual TLS | **IAM** (SA-to-SA) |
| `queue` | Public topic (not recommended) | Short-lived access token | — | — | **IAM** `roles/pubsub.publisher` |

`native` always means IAM service-account-to-service-account binding — the GCP-idiomatic choice.

---

## GCP APIs to enable (per project)

The driver enables these via `serviceusage.googleapis.com` at enclave creation time:

```
compute.googleapis.com          # VPC, subnets, firewall
run.googleapis.com              # Cloud Run
iam.googleapis.com              # Service accounts, bindings
cloudresourcemanager.googleapis.com  # Project operations
dns.googleapis.com              # Cloud DNS
pubsub.googleapis.com           # Pub/Sub topics and subscriptions
servicenetworking.googleapis.com # Private service access / PSC
cloudbilling.googleapis.com     # Billing account linkage
```

The **platform project** additionally requires:

```
secretmanager.googleapis.com    # Stores the nclav API bearer token
sqladmin.googleapis.com         # Cloud SQL (platform state store — future)
```

---

## Required IAM roles for the nclav runner

### Local dev (ADC mode)

When running locally with `gcloud auth application-default login`, the **operator's account**
needs these roles on the **enclave projects** it creates (automatically granted as project
creator/owner):

| Role | Why |
|---|---|
| `roles/resourcemanager.projectCreator` | Create one project per enclave (org/folder level) |
| `roles/billing.user` | Link billing account to new projects (billing account level) |
| `roles/owner` (on each enclave project) | Automatically granted to the project creator |

Because the operator is project owner, Terraform runs under their ADC credentials directly —
**no service account impersonation is needed**.

### Production (SA key mode)

When `provision_platform` has been run and `~/.nclav/gcp-credentials.json` exists, the
**`nclav-server` service account** needs these at the **organization or folder level**:

| Role | Why |
|---|---|
| `roles/resourcemanager.projectCreator` | Create one project per enclave |
| `roles/billing.user` | Link billing account to new projects |
| `roles/iam.serviceAccountAdmin` | Create per-enclave service accounts |
| `roles/iam.serviceAccountTokenCreator` | Impersonate enclave SAs for Terraform |
| `roles/compute.networkAdmin` | Create VPCs, subnets, firewall rules |
| `roles/dns.admin` | Create Cloud DNS zones and records |
| `roles/run.admin` | Deploy and configure Cloud Run services |
| `roles/pubsub.admin` | Create topics and subscriptions |
| `roles/servicenetworking.networksAdmin` | Configure Private Service Connect |

The `nclav-runner` SA also needs these on the **platform project** specifically:

| Role | Scope | Why |
|---|---|---|
| `roles/secretmanager.admin` | Platform project | Create and version the `nclav-api-token` secret at bootstrap time |
| `roles/secretmanager.secretAccessor` | `nclav-api-token` secret (or project) | Read the token at Cloud Run startup |

The **bootstrap operator** (human or bootstrap SA) needs `roles/secretmanager.admin` on
the platform project to perform the initial secret write. After bootstrap, `nclav-runner`
takes over all Secret Manager operations.

---

## Driver method specifications

### `provision_enclave`

Creates the GCP project that backs this enclave.

**Sequence:**

1. **Create project**
   ```
   POST https://cloudresourcemanager.googleapis.com/v3/projects
   {
     "projectId": "<enclave-id>",       // max 30 chars, lowercase, hyphens
     "displayName": "<enclave.name>",
     "parent": "folders/<folder-id>"    // from GcpDriverConfig
   }
   ```
   → Poll `operations/{name}` until done.

2. **Link billing account**
   ```
   PUT https://cloudbilling.googleapis.com/v1/projects/<project-id>/billingInfo
   { "billingAccountName": "billingAccounts/<account-id>" }
   ```

3. **Enable required APIs**
   ```
   POST https://serviceusage.googleapis.com/v1/projects/<project-id>/services:batchEnable
   { "serviceIds": [ "compute.googleapis.com", "run.googleapis.com", ... ] }
   ```
   → Poll operation until done (can take 30–60 s on a new project).

4. **Create enclave service account**
   ```
   POST https://iam.googleapis.com/v1/projects/<project-id>/serviceAccounts
   {
     "accountId": "<enclave.identity or enclave-id>",
     "serviceAccount": { "displayName": "<enclave.name>" }
   }
   ```

5. **Create VPC network** (if `enclave.network` is set)
   ```
   POST https://compute.googleapis.com/compute/v1/projects/<project-id>/global/networks
   { "name": "nclav-vpc", "autoCreateSubnetworks": false }
   ```

6. **Create subnets** (one per `network.subnets` entry)
   ```
   POST https://compute.googleapis.com/compute/v1/projects/<project-id>/regions/<region>/subnetworks
   {
     "name": "subnet-<index>",
     "network": "global/networks/nclav-vpc",
     "ipCidrRange": "<cidr>",
     "region": "<enclave.region>",
     "privateIpGoogleAccess": true
   }
   ```

7. **Create private DNS zone** (if `enclave.dns.zone` is set)
   ```
   POST https://dns.googleapis.com/dns/v1/projects/<project-id>/managedZones
   {
     "name": "nclav-zone",
     "dnsName": "<dns.zone>.",
     "visibility": "private",
     "privateVisibilityConfig": {
       "networks": [{ "networkUrl": "...nclav-vpc" }]
     }
   }
   ```

**Handle shape:**
```json
{
  "driver": "gcp",
  "kind": "enclave",
  "project_id": "acme-product-a-dev",
  "project_number": "123456789012",
  "service_account_email": "product-a-dev-sa@acme-product-a-dev.iam.gserviceaccount.com",
  "vpc_self_link": "https://www.googleapis.com/compute/v1/projects/.../networks/nclav-vpc",
  "dns_zone_name": "nclav-zone",
  "region": "us-central1",
  "provisioning_complete": true
}
```

> **`provisioning_complete` flag:** This field is only written after all five setup steps (project, billing, APIs, SA, VPC) complete successfully. The idempotency early-return requires this flag to be present and `true`. If a previous run timed out partway through, the flag will be absent and all steps will be re-executed on the next apply (each step is itself idempotent — ALREADY_EXISTS responses are treated as success).

---

### `teardown_enclave`

Deletes the project (cascades to all resources within it).

```
DELETE https://cloudresourcemanager.googleapis.com/v3/projects/<project-id>
```

GCP soft-deletes projects; they enter a 30-day pending-deletion period. Poll
`projects.get` until `lifecycleState == DELETE_REQUESTED`.

---

### `provision_partition`

Behavior depends on `partition.produces`:

#### `produces: http` → Cloud Run service

```
POST https://run.googleapis.com/v2/projects/<project-id>/locations/<region>/services?serviceId=<partition-id>
{
  "template": {
    "serviceAccount": "<enclave-sa-email>",
    "containers": [{
      "image": "<resolved_inputs["image"] or placeholder>",
      "env": [ { "name": "K", "value": "V" } ... ]   // from resolved_inputs
    }]
  },
  "ingress": "INGRESS_TRAFFIC_INTERNAL_ONLY"           // tightened at export time
}
```

> **Note:** The Cloud Run v2 API requires the service ID to be passed as a `?serviceId=` query parameter. The `name` field must be **absent** (or empty) in the request body — the API returns an error if it is set on create. The full resource name is derived locally after creation as `projects/<p>/locations/<r>/services/<partition-id>`.

The GCP driver automatically applies `nclav-managed=true`, `nclav-enclave={enclave-id}`,
and `nclav-partition={partition-id}` labels to the Cloud Run service.

**Outputs:**
```
hostname  →  <service-hash>-<project-hash>.<region>.run.app
port      →  443
```

#### `produces: tcp` → externally managed (passthrough)

nclav does not provision TCP backing services (databases, caches, etc.). The choice
of database engine, instance tier, HA topology, and backup policy is an
application-level concern outside nclav's scope.

For a `tcp` partition, nclav:
1. Validates all consumers can reach this partition (access-control graph check)
2. Reads `hostname` and `port` from the partition's `inputs:` block at provision time
3. Stores those values in the handle so subsequent `observe_partition` calls work without a live cloud call

**Partition `config.yml` example:**
```yaml
id: db
name: Database
produces: tcp
inputs:
  hostname: "10.10.5.3"
  port: "5432"
declared_outputs:
  - hostname
  - port
```

**Outputs:**
```
hostname  →  inputs["hostname"]   (empty string if not set — warning logged)
port      →  inputs["port"]       (empty string if not set)
```

**Handle shape:**
```json
{
  "driver": "gcp",
  "kind": "partition",
  "type": "tcp_passthrough",
  "project_id": "acme-product-a-dev",
  "hostname": "10.10.5.3",
  "port": "5432"
}
```

#### `produces: queue` → Pub/Sub topic

```
PUT https://pubsub.googleapis.com/v1/projects/<project-id>/topics/<partition-id>
{}
```

The GCP driver automatically applies `nclav-managed=true`, `nclav-enclave={enclave-id}`,
and `nclav-partition={partition-id}` labels to the Pub/Sub topic.

**Outputs:**
```
queue_url  →  projects/<project-id>/topics/<partition-id>
```

**Handle shape (Cloud Run example):**
```json
{
  "driver": "gcp",
  "kind": "partition",
  "type": "cloud_run",
  "project_id": "acme-product-a-dev",
  "region": "us-central1",
  "service_name": "projects/acme-product-a-dev/locations/us-central1/services/api",
  "service_url": "https://api-abc123-uc.a.run.app"
}
```

---

### `provision_export`

Makes a partition's outputs reachable from other enclaves according to `export.to` and `export.auth`.

#### `type: http`

1. **Adjust Cloud Run ingress** based on `export.to`:
   - `Public` → `INGRESS_TRAFFIC_ALL`
   - `AnyEnclave` / `Enclave(_)` → `INGRESS_TRAFFIC_INTERNAL_LOAD_BALANCER` (use a regional LB)
   - `Vpn` → `INGRESS_TRAFFIC_INTERNAL_ONLY`

2. **Set IAM policy** based on `export.auth`:
   - `none` → bind `allUsers` to `roles/run.invoker`
   - `token` / `oauth` → leave unauthenticated binding absent; importer uses OIDC
   - `native` → bind importer enclave's SA to `roles/run.invoker` (wired at import time)
   - `mtls` → bind importer SA + configure Certificate Authority Service (deferred)

   ```
   POST https://run.googleapis.com/v2/<service-name>:setIamPolicy
   {
     "policy": {
       "bindings": [{
         "role": "roles/run.invoker",
         "members": [ "allUsers" ]           // or specific SA
       }]
     }
   }
   ```

**Outputs** (forwarded from partition outputs):
```
hostname  →  <cloud-run-url-host>
port      →  443
```

#### `type: tcp` → Private Service Connect

1. Create a **PSC service attachment** in the exporter project:
   ```
   POST https://compute.googleapis.com/compute/v1/projects/<p>/regions/<r>/serviceAttachments
   {
     "name": "<export-name>-psc",
     "targetService": "<forwarding-rule-url>",     // pointing at the partition
     "connectionPreference": "ACCEPT_AUTOMATIC",
     "natSubnets": ["<psc-nat-subnet-url>"]
   }
   ```

**Outputs:**
```
psc_attachment_uri  →  projects/<p>/regions/<r>/serviceAttachments/<name>
hostname            →  <private-ip-of-attachment>
port                →  <partition port>
```

#### `type: queue` → Pub/Sub IAM

Grant the importer SA publish rights on the topic (done at import time; export just records the topic):

**Outputs:**
```
queue_url  →  projects/<project-id>/topics/<partition-id>
```

**Handle shape:**
```json
{
  "driver": "gcp",
  "kind": "export",
  "type": "http",
  "project_id": "product-a-dev",
  "export_name": "api-http",
  "cloud_run_service": "projects/.../services/api",
  "iam_bindings_applied": ["allUsers:roles/run.invoker"]
}
```

---

### `provision_import`

Wires the importer enclave to the exporter's resource. Called after both
enclaves are fully provisioned.

#### `type: http`

1. If `auth: native` — bind the importer SA to `roles/run.invoker` on the
   Cloud Run service (cross-project IAM is supported):
   ```
   POST https://run.googleapis.com/v2/<service-name>:setIamPolicy
   { "policy": { "bindings": [{ "role": "roles/run.invoker",
       "members": ["serviceAccount:<importer-sa>"] }] } }
   ```

2. Inject the service URL into the importer's resolved outputs under `alias.*`:
   ```
   hostname  →  <cloud-run-url-host>
   port      →  443
   ```

#### `type: tcp` → PSC consumer endpoint

Create a **PSC endpoint** in the importer VPC that connects to the exporter's
service attachment:

```
POST https://compute.googleapis.com/compute/v1/projects/<importer-project>/regions/<r>/forwardingRules
{
  "name": "<alias>-psc-endpoint",
  "loadBalancingScheme": "",
  "target": "<psc-attachment-uri-from-export-handle>",
  "network": "projects/<importer-project>/global/networks/nclav-vpc",
  "IPAddress": "<available-ip-in-importer-subnet>"
}
```

Then create a Cloud DNS record in the importer's private zone:
```
POST https://dns.googleapis.com/dns/v1/projects/<importer-project>/managedZones/nclav-zone/changes
{
  "additions": [{
    "name": "<alias>.<dns-zone>.",
    "type": "A",
    "ttl": 300,
    "rrdatas": ["<psc-endpoint-ip>"]
  }]
}
```

**Outputs:**
```
hostname  →  <alias>.<dns-zone>      (resolves inside importer VPC)
port      →  <forwarded port>
```

#### `type: queue` → Cross-project Pub/Sub subscription

1. Grant importer SA `roles/pubsub.subscriber` on the topic:
   ```
   POST https://pubsub.googleapis.com/v1/<topic>:setIamPolicy
   { "policy": { "bindings": [{ "role": "roles/pubsub.subscriber",
       "members": ["serviceAccount:<importer-sa>"] }] } }
   ```

2. Create subscription in the **importer** project pointing at the exporter topic:
   ```
   PUT https://pubsub.googleapis.com/v1/projects/<importer-project>/subscriptions/<alias>
   {
     "topic": "projects/<exporter-project>/topics/<partition-id>",
     "ackDeadlineSeconds": 60
   }
   ```

**Outputs:**
```
queue_url  →  projects/<importer-project>/subscriptions/<alias>
```

---

## GCP driver configuration

A `GcpDriverConfig` struct would be injected at startup (not in YAML per-enclave):

```rust
pub struct GcpDriverConfig {
    /// GCP organization or folder to create projects under.
    pub parent: String,           // "folders/123" or "organizations/456"
    /// Billing account to attach to every new project.
    pub billing_account: String,  // "billingAccounts/XXXXXX-YYYYYY-ZZZZZZ"
    /// Default region used when enclave.region is not set.
    pub default_region: String,   // "us-central1"
    /// Optional prefix prepended to every GCP project ID.
    /// e.g. prefix "acme" + enclave "product-a-dev" → project "acme-product-a-dev".
    /// Avoids global project ID collisions without changing enclave YAML IDs.
    pub project_prefix: Option<String>,
}
```

Authentication against GCP APIs uses **Application Default Credentials** in
order of preference:
1. `GOOGLE_APPLICATION_CREDENTIALS` env var (service account JSON key)
2. Workload Identity (when running on GCP infrastructure)
3. `gcloud auth application-default login` for local development

---

## Rust crate dependencies for the GCP driver

```toml
[dependencies]
# HTTP client
reqwest = { version = "0.12", features = ["json"] }

# GCP auth — ADC resolution (gcloud, Workload Identity, SA key file)
gcp_auth = "0.12"

# Base64 for SA key decoding (provision_platform)
base64 = "0.22"
```

The driver uses raw REST via `reqwest` + `gcp_auth` for token acquisition.
`gcp_auth::provider()` resolves ADC; `gcp_auth::CustomServiceAccount::from_file`
loads a SA key JSON file for production mode.

---

## Operation polling pattern

Many GCP API calls return a long-running operation (`google.longrunning.Operation`).
The driver must poll until completion:

```rust
async fn wait_for_operation(
    client: &reqwest::Client,
    token: &str,
    operation_url: &str,
) -> Result<serde_json::Value, DriverError> {
    let backoff = [1, 2, 4, 8, 16, 30]; // seconds, cycling
    for (i, &delay) in backoff.iter().cycle().take(120).enumerate() {
        let op: serde_json::Value = client
            .get(operation_url)
            .bearer_auth(token)
            .send().await?
            .json().await?;

        if op["done"].as_bool().unwrap_or(false) {
            if let Some(err) = op.get("error") {
                return Err(DriverError::ProvisionFailed(err.to_string()));
            }
            return Ok(op["response"].clone());
        }
        if i % 10 == 0 {
            tracing::info!(poll = i, url = operation_url, "waiting for GCP operation");
        }
        tokio::time::sleep(Duration::from_secs(delay)).await;
    }
    Err(DriverError::ProvisionFailed(
        format!("operation timed out after 120 polls: {operation_url}")
    ))
}
```

Max polls: **120** (backoff cycles `[1,2,4,8,16,30]` — ceiling ~58 minutes). An INFO log is emitted every 10 polls. The timeout error includes the operation URL to aid debugging.

Operation endpoint patterns by API:
- Resource Manager: `https://cloudresourcemanager.googleapis.com/v3/{operation}`
- Compute: `https://compute.googleapis.com/compute/v1/projects/{p}/global/operations/{op}`
  (or regional: `.../regions/{r}/operations/{op}`)
- Cloud Run: `https://run.googleapis.com/v2/{operation}`

---

## Idempotency strategy

Every GCP create call is idempotent:

- **`provision_enclave` early return:** if the existing handle contains `"provisioning_complete": true`, all five setup steps are skipped. This flag is only set after every step completes successfully, so a partial run (e.g., a timeout during API enablement) will re-execute all steps on the next apply — safely, because each step is itself idempotent.
- **ALREADY_EXISTS:** GCP APIs sometimes return `ALREADY_EXISTS` status (409) and sometimes return `UNKNOWN` with "already exists" in the message body (Compute API quirk). Both cases are treated as success.
- Cloud Run `services.patch` with `updateMask` is safe to call repeatedly.
- Pub/Sub `topics.create`: `ALREADY_EXISTS` → success.
- Project creation: if it exists and is active, proceed to billing/API steps.
- PSC attachments and forwarding rules: check by name before creating.

---

## Observe method specifications

`observe_*` methods are read-only. They are called by the drift detection path
(`GET /enclaves/{id}?observe=true` or the background drift scanner) and must
never modify any cloud resource. They populate `ObservedState` which the
reconciler uses to set `ResourceMeta.last_seen_at` and `ProvisioningStatus`.

### `observe_enclave`

```
GET https://cloudresourcemanager.googleapis.com/v3/projects/<project-id>
```

| GCP response field | Maps to |
|---|---|
| `lifecycleState == ACTIVE` | `exists: true`, `healthy: true` |
| `lifecycleState == DELETE_REQUESTED` | `exists: true`, `healthy: false` |
| HTTP 404 | `exists: false` |
| Any other HTTP error | propagate as `DriverError` |

Additionally check VPC and service account presence with two parallel GETs:

```
GET https://compute.googleapis.com/compute/v1/projects/<p>/global/networks/nclav-vpc
GET https://iam.googleapis.com/v1/projects/<p>/serviceAccounts/<identity>@<p>.iam.gserviceaccount.com
```

Set `healthy: false` (→ `Degraded`) if the project exists but either the VPC
or the service account is missing — the enclave is partially provisioned.

**`ObservedState.outputs`:** empty for enclaves (no keyed outputs).

---

### `observe_partition`

#### Cloud Run (http)

```
GET https://run.googleapis.com/v2/projects/<p>/locations/<r>/services/<partition-id>
```

Map the `conditions` array:

| Condition | `status` | Meaning |
|---|---|---|
| `type: Ready`, `status: True` | `Active` | Service is live and handling traffic |
| `type: Ready`, `status: False` | `Degraded` | Service exists but is not ready |
| `type: Ready`, `status: Unknown` | `Provisioning` | Deployment still rolling out |
| HTTP 404 | report `exists: false` | — |

Populate `ObservedState.outputs` from the live service:

```
hostname  →  response.uri  (strip "https://")
port      →  443
```

#### TCP passthrough

TCP partitions are externally managed; no cloud API is called. `observe_partition`
reads hostname and port from the stored handle:

```
hostname  →  handle["hostname"]
port      →  handle["port"]
```

| Condition | `ProvisioningStatus` |
|---|---|
| handle present, hostname non-empty | `Active` |
| handle present, hostname empty | `Degraded` |
| no handle | `exists: false` |

#### Pub/Sub topic (queue)

```
GET https://pubsub.googleapis.com/v1/projects/<p>/topics/<partition-id>
```

Pub/Sub topics have no health state beyond existence:

| Response | `ProvisioningStatus` |
|---|---|
| HTTP 200 | `Active` |
| HTTP 404 | `exists: false` |

```
queue_url  →  response.name   ("projects/<p>/topics/<partition-id>")
```

---

## GCP health signals → ProvisioningStatus

The following table is the complete mapping the GCP driver uses when
translating cloud API responses into the nclav lifecycle state machine.
The reconciler owns the state transitions; this table defines what the
driver reports via `ObservedState`.

| Resource | Signal | `ProvisioningStatus` |
|---|---|---|
| Project | `lifecycleState: ACTIVE` | `Active` |
| Project | `lifecycleState: DELETE_REQUESTED` | `Deleting` |
| Project | VPC or SA missing | `Degraded` |
| Project | HTTP 404 | (caller treats as `Deleted`) |
| Cloud Run | `conditions[Ready].status: True` | `Active` |
| Cloud Run | `conditions[Ready].status: False` | `Degraded` |
| Cloud Run | `conditions[Ready].status: Unknown` | `Provisioning` |
| Cloud Run | HTTP 404 | (caller treats as `Deleted`) |
| TCP passthrough | handle present, hostname non-empty | `Active` |
| TCP passthrough | handle present, hostname empty | `Degraded` |
| TCP passthrough | no handle | (caller treats as `Deleted`) |
| Pub/Sub topic | HTTP 200 | `Active` |
| Pub/Sub topic | HTTP 404 | (caller treats as `Deleted`) |

The reconciler — not the driver — writes to `ResourceMeta.status`. The driver
returns `ObservedState`; the reconciler decides the transition.

---

## GCP error format → ResourceError

GCP REST APIs return errors in this envelope:

```json
{
  "error": {
    "code": 403,
    "status": "PERMISSION_DENIED",
    "message": "The caller does not have permission",
    "details": [
      { "@type": "type.googleapis.com/google.rpc.ErrorInfo",
        "reason": "IAM_PERMISSION_DENIED",
        "domain": "iam.googleapis.com",
        "metadata": { "permission": "compute.networks.create" } }
    ]
  }
}
```

The GCP driver extracts `ResourceError.message` as:

```
"<status>: <message>"
```

e.g. `"PERMISSION_DENIED: The caller does not have permission"`.

If `details` is non-empty, append the first `ErrorInfo.reason` and the
`metadata` values to give operators enough context to act without reading
raw JSON:

```
"PERMISSION_DENIED: The caller does not have permission [IAM_PERMISSION_DENIED — compute.networks.create]"
```

For long-running operations that fail, the error is nested at
`operation.error` with the same shape. The polling helper extracts it
the same way before surfacing it as a `DriverError::ProvisionFailed`.

---

## GCP authentication modes

The GCP driver supports two authentication modes, selected automatically at startup:

| Mode | When | How |
|---|---|---|
| **ADC (local dev)** | `~/.nclav/gcp-credentials.json` absent | `gcp_auth::provider()` — gcloud ADC, Workload Identity, or `GOOGLE_APPLICATION_CREDENTIALS` |
| **SA key (production)** | `~/.nclav/gcp-credentials.json` present | `gcp_auth::CustomServiceAccount::from_file` — written by `provision_platform` |

In ADC mode the operator is project owner on all enclave projects and Terraform runs under
their credentials directly — no impersonation is needed. In SA key mode the `nclav-server`
SA authenticates for both the GCP driver API calls and (via `GOOGLE_IMPERSONATE_SERVICE_ACCOUNT`)
for Terraform subprocesses.

ADC must include the `cloud-billing` scope for billing account linkage:

```bash
gcloud auth application-default login \
  --scopes=https://www.googleapis.com/auth/cloud-platform,https://www.googleapis.com/auth/cloud-billing
```

---

## `provision_platform` — SA key provisioning

`GcpDriver::provision_platform()` bootstraps a dedicated `nclav-server` service account for
production use. It is **not called automatically** by `nclav bootstrap`; call it once when an
operator with Org/Folder Admin rights sets up a new environment:

**What it does:**
1. Creates the platform project `{prefix}-nclav`
2. Enables required APIs in the platform project
3. Creates the `nclav-server` service account
4. Grants `roles/resourcemanager.projectCreator` at the parent (prints a manual `gcloud` command if `PERMISSION_DENIED`)
5. Grants `roles/billing.user` on the billing account (same fallback)
6. Creates and returns a SA key (base64-decoded JSON)

The caller writes the key JSON to `~/.nclav/gcp-credentials.json` (mode 0600). On the next
`nclav bootstrap --cloud gcp` the key file is detected and the driver switches to SA key mode.

---

## GCP bootstrap (platform provisioning)

`nclav bootstrap --cloud gcp` provisions a dedicated **platform GCP project**
that hosts the nclav API and its state store. This is separate from the enclave
projects the driver creates during `nclav apply`.

### What the platform project contains

| Resource | Name | Purpose |
|---|---|---|
| GCP Project | `{prefix}-nclav` | Billing and IAM boundary for the platform |
| Cloud Run service | `nclav-api` | The nclav HTTP API |
| Cloud SQL Postgres | `nclav-state` | Persistent state store (enclave handles, outputs, audit log) |
| Service Account | `nclav-runner@{platform-project}.iam.gserviceaccount.com` | Identity the API uses to provision enclave projects |
| Secret Manager secret | `nclav-api-token` | Bearer token read by Cloud Run at startup; never in env vars |

The platform project is created using the same `--gcp-parent` and
`--gcp-billing-account` as enclave projects. Override the parent with
`--gcp-platform-parent` if you need the platform in a separate folder.

### Platform project naming

| Config | Platform project ID |
|---|---|
| `--gcp-project-prefix acme` | `acme-nclav` |
| `--gcp-platform-project my-nclav` | `my-nclav` (explicit override) |
| No prefix | `nclav` (globally unique; only safe in a dedicated org) |

Enclave projects are named `{prefix}-{enclave-id}` (e.g. `acme-product-a-dev`).
Platform and enclave projects follow the same naming convention but the
platform always has the suffix `-nclav`.

### Platform bootstrap sequence

1. **Create platform project** under `--gcp-parent`
2. **Link billing account**
3. **Enable APIs**: `run.googleapis.com`, `sqladmin.googleapis.com`, `iam.googleapis.com`,
   `cloudresourcemanager.googleapis.com`, `secretmanager.googleapis.com`
4. **Create platform service account** `nclav-runner` with required IAM roles (see below)
5. **Generate API token** (or reuse from `--rotate-token` flag)
6. **Write token to Secret Manager**
   ```
   POST https://secretmanager.googleapis.com/v1/projects/{platform-project}/secrets
   { "replication": { "automatic": {} } }
   ```
   then add the first version:
   ```
   POST .../secrets/nclav-api-token:addVersion
   { "payload": { "data": "<base64(token)>" } }
   ```
   then grant `nclav-runner` accessor on that secret:
   ```
   POST .../secrets/nclav-api-token:setIamPolicy
   { "policy": { "bindings": [{
     "role": "roles/secretmanager.secretAccessor",
     "members": ["serviceAccount:nclav-runner@{platform-project}.iam.gserviceaccount.com"]
   }]}}
   ```
7. **Provision Cloud SQL Postgres** `nclav-state` (private IP, same VPC as the platform project)
8. **Deploy Cloud Run service** `nclav-api` using the `nclav-runner` SA, injecting the Cloud SQL
   connection string. The binary reads the token from Secret Manager at startup — the token is
   **not** passed as an environment variable.
9. **Print API endpoint** — operator sets `NCLAV_URL` to this value; prints token for
   `~/.nclav/token` so CLI commands work immediately

Bootstrap is idempotent: each step checks for the resource before creating it. For the
token specifically: if `nclav-api-token` already exists in Secret Manager and `--rotate-token`
was not passed, the existing value is preserved.

### Required IAM roles for `nclav-runner` SA

These must be granted at the **organization or folder level** (the `--gcp-parent`
folder) so the API can create and manage enclave projects beneath it:

| Role | Why |
|---|---|
| `roles/resourcemanager.projectCreator` | Create one project per enclave |
| `roles/billing.user` | Attach billing account to new projects |
| `roles/iam.serviceAccountAdmin` | Create per-enclave service accounts |
| `roles/compute.networkAdmin` | Create VPCs and subnets in enclave projects |
| `roles/run.admin` | Deploy Cloud Run services in enclave projects |
| `roles/pubsub.admin` | Create Pub/Sub topics in enclave projects |
| `roles/dns.admin` | Create Cloud DNS zones in enclave projects |

The bootstrap operator (who runs `nclav bootstrap`) needs these same roles
locally to set up the platform. After bootstrap, the `nclav-runner` SA handles
all subsequent GCP calls.

### Authentication during bootstrap vs. runtime

| Phase | Who authenticates | How |
|---|---|---|
| `nclav bootstrap --cloud gcp` | Operator (local) | ADC: `gcloud auth application-default login` or SA key |
| `nclav apply` / `nclav diff` (post-bootstrap) | `nclav-runner` SA | Cloud Run workload identity / attached SA |

Bootstrap is always a **local CLI operation** — it never runs on Cloud Run.
After the Cloud Run service is deployed, the API's SA takes over and the
operator's credentials are no longer needed for routine use.

### Cloud SQL details

| Setting | Value |
|---|---|
| Engine | Postgres 16 |
| Tier | `db-n1-standard-2` (default; override via `--gcp-platform-sql-tier`) |
| Edition | Enterprise |
| Network | Private IP only; no public IP |
| Region | Same as `--gcp-platform-region` (default: `us-central1`) |

### Platform Cloud Run details

| Setting | Value |
|---|---|
| Region | `--gcp-platform-region` (default: `us-central1`) |
| Ingress | `INGRESS_TRAFFIC_INTERNAL_LOAD_BALANCER` |
| Auth | Cloud IAM (operators authenticate with their GCP identity) |
| URL | Auto-assigned `.run.app` URL (custom DNS: future, `--gcp-platform-dns-name`) |

---

## Secret Manager integration

The `GcpSecretStore` implements the `SecretStore` trait using the Secret Manager REST API.
It is registered in `DriverRegistry` when `--cloud gcp` is used and is used exclusively for
**platform-level secrets** (currently just the API bearer token). It is not used to store
enclave-level secrets or partition inputs.

### `GcpSecretStore` struct

```rust
pub struct GcpSecretStore {
    client: reqwest::Client,
    project: String,   // platform project ID, e.g. "acme-nclav"
    // token_provider: same gcp_auth token source as GcpDriver
}
```

### `write(name, value)` sequence

1. Attempt to create the secret (idempotent — `ALREADY_EXISTS` is success):
   ```
   POST https://secretmanager.googleapis.com/v1/projects/{project}/secrets
   {
     "secretId": "{name}",
     "replication": { "automatic": {} }
   }
   ```
2. Add a new version:
   ```
   POST https://secretmanager.googleapis.com/v1/projects/{project}/secrets/{name}:addVersion
   {
     "payload": { "data": "<base64(value)>" }
   }
   ```
   Each call to `write` creates a new version. Old versions are not auto-disabled;
   `read` always fetches `versions/latest`.

### `read(name)` sequence

```
GET https://secretmanager.googleapis.com/v1/projects/{project}/secrets/{name}/versions/latest:access
```

Response body: `{ "payload": { "data": "<base64(value)>" } }`.

Return `None` if the secret does not exist (HTTP 404). Propagate other errors as `DriverError`.

### `delete(name)` sequence

```
DELETE https://secretmanager.googleapis.com/v1/projects/{project}/secrets/{name}
```

Returns success if the secret does not exist (idempotent).

### Token rotation on GCP

`--rotate-token` with `--cloud gcp` calls `secret_store.write("nclav-api-token", new_token)`,
which adds a new Secret Manager version. To pick up the new token, the Cloud Run service must
restart. The operator can trigger this with:

```bash
gcloud run services update nclav-api \
  --region us-central1 \
  --project {platform-project} \
  --update-env-vars NCLAV_RESTART=$(date +%s)
```

Or simply wait for the next cold start. The old token is invalid immediately after rotation —
any in-flight CLI commands will receive 401 until restarted against the new token.

### Cloud Run startup read

The GCP-mode API binary reads the token before starting the HTTP server:

```rust
// in the GCP Cloud Run entrypoint (not local bootstrap)
let token = registry
    .secret_store()
    .read("nclav-api-token")
    .await?
    .context("nclav-api-token not found in Secret Manager")?;
let app = nclav_api::build_app(store, registry, Arc::new(token));
```

The `GOOGLE_CLOUD_PROJECT` environment variable (set automatically by Cloud Run) is used
to determine the platform project for Secret Manager lookups — no extra configuration needed.

---

## What's explicitly out of scope for the first GCP driver iteration

- GKE-backed partitions (Cloud Run covers the initial http/tcp surface)
- Cloud VPN / Interconnect for `to: vpn` exports
- Certificate Authority Service for `auth: mtls`
- Shared VPC (each enclave gets its own project + VPC)
- Multi-region partitions
- Cloud Armor / WAF policies on exported HTTP endpoints
- Secret Manager integration for partition inputs
