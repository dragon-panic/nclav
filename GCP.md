# GCP Driver Implementation Plan

This document maps every nclav concept to a concrete GCP primitive, specifies
the exact API calls needed at each driver method, and defines what the opaque
`Handle` and output map look like for GCP resources.

---

## Concept → GCP primitive mapping

| nclav concept | GCP primitive | Notes |
|---|---|---|
| `Enclave` | **Project** | Billing, IAM, API enablement, and network are all project-scoped |
| `Enclave.identity` | **Service Account** | One SA per enclave; name taken from `identity` field |
| `Enclave.network` | **VPC network + subnets** | Custom-mode VPC; one subnet per entry in `subnets` list |
| `Enclave.dns.zone` | **Cloud DNS managed zone** | Private zone, visible only within the VPC |
| `Partition` (http) | **Cloud Run service** | Serverless; region from enclave |
| `Partition` (tcp) | **Cloud SQL instance** or **Cloud Run** | Cloud SQL for databases; Cloud Run for custom TCP |
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
sqladmin.googleapis.com         # Cloud SQL (tcp partitions backed by SQL)
servicenetworking.googleapis.com # Private service access / PSC
cloudbilling.googleapis.com     # Billing account linkage
```

---

## Required IAM roles for the nclav runner

The service account or user running nclav needs these at the **organization or folder level**:

| Role | Why |
|---|---|
| `roles/resourcemanager.projectCreator` | Create one project per enclave |
| `roles/billing.user` | Link billing account to new projects |
| `roles/iam.serviceAccountAdmin` | Create per-enclave service accounts |
| `roles/iam.serviceAccountTokenCreator` | Impersonate enclave SAs for provisioning |
| `roles/compute.networkAdmin` | Create VPCs, subnets, firewall rules |
| `roles/dns.admin` | Create Cloud DNS zones and records |
| `roles/run.admin` | Deploy and configure Cloud Run services |
| `roles/pubsub.admin` | Create topics and subscriptions |
| `roles/servicenetworking.networksAdmin` | Configure Private Service Connect |

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
  "project_id": "product-a-dev",
  "project_number": "123456789012",
  "service_account_email": "product-a-dev@product-a-dev.iam.gserviceaccount.com",
  "vpc_self_link": "https://www.googleapis.com/compute/v1/projects/.../networks/nclav-vpc",
  "dns_zone_name": "nclav-zone",
  "region": "us-central1"
}
```

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
POST https://run.googleapis.com/v2/projects/<project-id>/locations/<region>/services
{
  "name": "projects/<p>/locations/<r>/services/<partition-id>",
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

**Outputs:**
```
hostname  →  <service-hash>-<project-hash>.<region>.run.app
port      →  443
```

#### `produces: tcp` → Cloud SQL (Postgres default)

```
POST https://sqladmin.googleapis.com/v1/projects/<project-id>/instances
{
  "name": "<partition-id>",
  "databaseVersion": "POSTGRES_16",
  "region": "<region>",
  "settings": {
    "tier": "db-f1-micro",
    "ipConfiguration": {
      "ipv4Enabled": false,
      "privateNetwork": "projects/<p>/global/networks/nclav-vpc"
    }
  }
}
```

→ Poll `operations/{name}` until `status == DONE`.

**Outputs:**
```
hostname  →  <private-ip>     (from instance.ipAddresses where type=PRIVATE)
port      →  5432
```

#### `produces: queue` → Pub/Sub topic

```
PUT https://pubsub.googleapis.com/v1/projects/<project-id>/topics/<partition-id>
{}
```

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
  "project_id": "product-a-dev",
  "region": "us-central1",
  "service_name": "projects/product-a-dev/locations/us-central1/services/api",
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
    /// Default region when enclave.region is "gcp-default".
    pub default_region: String,   // "us-central1"
    /// Path to service account JSON key, or None to use ADC.
    pub credentials: Option<PathBuf>,
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
# HTTP client (already in workspace)
reqwest = { version = "0.12", features = ["json"] }

# GCP auth — obtains access tokens from ADC or a service account key
google-cloud-auth = "0.17"

# Optional: strongly-typed GCP API clients (generated from discovery docs)
# google-cloud-run  = "0.4"
# google-cloud-pubsub = "0.27"
# google-cloud-storage = "0.24"
# Or use raw REST via reqwest + google-cloud-auth for full control.

# Retry with exponential backoff for long-running operations
tokio-retry = "0.3"
```

The simplest approach for the initial implementation: raw REST with `reqwest`
and `google-cloud-auth` for token acquisition. The typed clients can be adopted
later per-service once the API surface is stable.

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
    let backoff = [1, 2, 4, 8, 16, 30]; // seconds
    for &delay in backoff.iter().cycle().take(30) {
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
        tokio::time::sleep(Duration::from_secs(delay)).await;
    }
    Err(DriverError::ProvisionFailed("operation timed out".into()))
}
```

Operation endpoint patterns by API:
- Resource Manager: `https://cloudresourcemanager.googleapis.com/v3/{operation}`
- Compute: `https://compute.googleapis.com/compute/v1/projects/{p}/global/operations/{op}`
  (or regional: `.../regions/{r}/operations/{op}`)
- Cloud Run: `https://run.googleapis.com/v2/{operation}`
- Cloud SQL: `https://sqladmin.googleapis.com/v1/projects/{p}/operations/{op}`

---

## Idempotency strategy

Every GCP create call should be checked against `existing: Option<&Handle>` before issuing:

- If `handle` is `Some` and contains a `project_id` / `service_name` / etc.,
  issue a `GET` first and only `PATCH`/`PUT` if state has drifted.
- Cloud Run `services.patch` with `updateMask` is safe to call repeatedly.
- Pub/Sub `topics.create` returns `ALREADY_EXISTS` (409); treat as success.
- Project creation: check `projects.get` first; if it exists and is active, skip.
- PSC attachments and forwarding rules: check by name before creating.

---

## What's explicitly out of scope for the first GCP driver iteration

- GKE-backed partitions (Cloud Run covers the initial http/tcp surface)
- Cloud VPN / Interconnect for `to: vpn` exports
- Certificate Authority Service for `auth: mtls`
- Shared VPC (each enclave gets its own project + VPC)
- Multi-region partitions
- Cloud Armor / WAF policies on exported HTTP endpoints
- Secret Manager integration for partition inputs
