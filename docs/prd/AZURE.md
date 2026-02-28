# Azure Driver

This document is the reference specification for the nclav Azure driver.

---

## Concept → Azure Primitive Mapping

| nclav concept | Azure primitive |
|---|---|
| `Enclave` | **Subscription** (created via MCA alias API, placed under a management group) |
| `Enclave.identity` | **User-Assigned Managed Identity** (`nclav-identity` in the `nclav-rg` resource group) |
| `Partition` (identity) | **User-Assigned Managed Identity** (`partition-{id}` in `nclav-rg`) |
| `Enclave.network` | **Virtual Network** (`nclav-vnet`) + subnets in `nclav-rg` |
| `Enclave.dns.zone` | **Private DNS Zone** linked to `nclav-vnet` |
| `Export` (http) | Private Link Service resource ID read from Terraform output `pls_id`; endpoint URL from `endpoint_url` |
| `Export` (tcp) | Private Link Service resource ID from Terraform output `pls_id`; port from `port` |
| `Export` (queue) | Service Bus namespace + topic read from Terraform outputs |
| `Import` (http/tcp) | **Private Endpoint** in importer VNet + DNS A record |
| `Import` (queue) | RBAC grant on Service Bus topic + Private Endpoint to Service Bus namespace |

---

## Authentication

The Azure driver uses a `TokenProvider` abstraction to support multiple auth modes.
Selection is automatic at startup:

| Priority | Mode | Trigger |
|---|---|---|
| 1 | **Service Principal** | `client_id` + `client_secret` set in config |
| 2 | **Service Principal (env)** | `AZURE_CLIENT_ID` + `AZURE_CLIENT_SECRET` env vars |
| 3 | **Managed Identity (IMDS)** | `IDENTITY_ENDPOINT` env var present (Azure-hosted environments) |
| 4 | **Azure CLI** | Fallback — runs `az account get-access-token` |

SP and IMDS tokens are cached until 60 seconds before expiry. CLI tokens are re-fetched on every use.

---

## `AzureDriverConfig` Reference

| Field | Type | Required | Description |
|---|---|---|---|
| `tenant_id` | `String` | Yes | Azure tenant ID (GUID) |
| `management_group_id` | `String` | Yes | Management group where subscription enclaves are placed |
| `billing_account_name` | `String` | Yes | MCA billing account name (long GUID form) |
| `billing_profile_name` | `String` | Yes | MCA billing profile name |
| `invoice_section_name` | `String` | Yes | MCA invoice section name |
| `default_location` | `String` | No | Default Azure region (default: `eastus2`) |
| `subscription_prefix` | `Option<String>` | No | Prefix prepended to every subscription alias |
| `client_id` | `Option<String>` | No | SP client ID (falls back to IMDS/CLI if absent) |
| `client_secret` | `Option<String>` | No | SP client secret (falls back to IMDS/CLI if absent) |

### CLI flags and environment variables

```
--azure-tenant-id                   NCLAV_AZURE_TENANT_ID
--azure-management-group-id         NCLAV_AZURE_MANAGEMENT_GROUP_ID
--azure-billing-account-name        NCLAV_AZURE_BILLING_ACCOUNT_NAME
--azure-billing-profile-name        NCLAV_AZURE_BILLING_PROFILE_NAME
--azure-invoice-section-name        NCLAV_AZURE_INVOICE_SECTION_NAME
--azure-default-location            NCLAV_AZURE_DEFAULT_LOCATION     (default: eastus2)
--azure-subscription-prefix         NCLAV_AZURE_SUBSCRIPTION_PREFIX
--azure-client-id                   NCLAV_AZURE_CLIENT_ID
--azure-client-secret               NCLAV_AZURE_CLIENT_SECRET
```

---

## Required RBAC for nclav Runner

The service principal or managed identity running nclav needs these roles:

| Role | Scope | Purpose |
|---|---|---|
| `Owner` | Management Group | Create subscriptions, move them into the MG, grant RBAC in new subscriptions |
| `Microsoft.Subscription/aliases/write` | Tenant root (`/`) | Create one subscription per enclave via MCA alias API |
| `Invoice Section Contributor` | MCA invoice section | Link billing to new subscriptions |

---

## Driver Method Specifications

### `provision_enclave`

Creates one Azure Subscription per enclave. Idempotent via `provisioning_complete` flag.

**Sequence:**
1. Check `provisioning_complete` in existing handle → early return if true.
2. Derive subscription alias: `{prefix}-{enclave-id}` (1–63 chars, alphanumeric + `-_\.`).
3. `PUT /providers/Microsoft.Subscription/aliases/{alias}?api-version=2021-10-01`
   Body: `{ displayName, billingScope (MCA path), workload: "Production" }`.
   Poll `Azure-AsyncOperation` header URL until `{ "status": "Succeeded" }`.
   409 → GET existing alias to retrieve `subscriptionId`.
4. `PUT /providers/Microsoft.Management/managementGroups/{mgId}/subscriptions/{subId}?api-version=2020-05-01` → move to MG. 409 = already in MG → success.
5. `PUT /subscriptions/{sub}/resourcegroups/nclav-rg?api-version=2021-04-01`
   Tags: `nclav-managed=true`, `nclav-enclave={enclave_id}`.
6. `PUT /subscriptions/{sub}/resourceGroups/nclav-rg/providers/Microsoft.ManagedIdentity/userAssignedIdentities/nclav-identity?api-version=2023-01-31`
   Returns `principalId`, `clientId` synchronously.
7. If `enclave.network` set: `PUT .../virtualNetworks/nclav-vnet?api-version=2023-11-01`.
   Poll async op.
8. If `enclave.dns.zone` set:
   a. `PUT .../privateDnsZones/{zone}?api-version=2020-06-01` `{ location: "global" }`.
   b. `PUT .../virtualNetworkLinks/nclav-link` with VNet resource ID.
9. Stamp `provisioning_complete: true` in handle.

**Enclave handle shape:**
```json
{
  "driver": "azure", "kind": "enclave",
  "subscription_id": "12345678-...",
  "subscription_alias": "prefix-product-a-dev",
  "resource_group": "nclav-rg",
  "location": "eastus2",
  "identity_resource_id": "/subscriptions/.../userAssignedIdentities/nclav-identity",
  "identity_principal_id": "uuid",
  "identity_client_id": "uuid",
  "vnet_resource_id": "/subscriptions/.../virtualNetworks/nclav-vnet",
  "dns_zone_name": "product-a.dev.local",
  "provisioning_complete": true
}
```

### `teardown_enclave`

```
POST /subscriptions/{sub}/providers/Microsoft.Subscription/cancel?api-version=2021-10-01
```

Azure soft-deletes subscriptions (90-day hold). Resources persist for ~90 days.
A warning is logged; operators must permanently delete via Azure portal after the hold period.

---

### `provision_partition`

Creates a per-partition managed identity with Contributor access on the subscription.

**Sequence:**
1. Derive partition MI name: `partition-{id}` (truncated+hex-hash if >64 chars).
2. `PUT .../userAssignedIdentities/{name}` → returns `principalId`, `clientId`.
3. `PUT /subscriptions/{sub}/providers/Microsoft.Authorization/roleAssignments/{uuid}?api-version=2022-04-01`
   Assigns `Contributor` (role ID `b24988ac-6180-42a0-ab88-20f7382dd24c`) to partition MI.
   409 `RoleAssignmentExists` → success. RBAC failure is non-fatal (logged as warning).

**Subscription ID resolution:**
The driver reads `nclav_subscription_id` from `resolved_inputs` (injected by the reconciler from the enclave handle's `context_vars`). Falls back to the existing partition handle's `subscription_id`, then to `enclave.identity`.

**Partition handle shape:**
```json
{
  "driver": "azure", "kind": "partition", "type": "iac",
  "subscription_id": "12345678-...",
  "resource_group": "nclav-rg",
  "partition_identity_resource_id": "/subscriptions/.../userAssignedIdentities/partition-api",
  "partition_identity_principal_id": "uuid",
  "partition_identity_client_id": "uuid"
}
```

### `teardown_partition`

`DELETE .../userAssignedIdentities/{name}` — non-fatal 404.

---

### `provision_export`

Reads Terraform outputs from `partition_outputs` and constructs the export handle.
No Azure API calls are made — the actual resources are Terraform-managed.

**`type: http`**

Required TF outputs: `endpoint_url` (required), `pls_id` (optional), `port` (optional, default 443).
Outputs forwarded: `hostname`, `port`.

**`type: tcp`**

Required TF outputs: `pls_id` (required), `port` (optional, default 0).
Outputs forwarded: `pls_resource_id`, `port`.

**`type: queue`**

Required TF outputs: `service_bus_namespace_name`, `topic_name`, `service_bus_resource_id`.
Outputs forwarded: `queue_url` = `{namespace}.servicebus.windows.net/{topic}`.

---

### `provision_import`

**`type: http` / `type: tcp`** — Private Endpoint wiring:

1. Read `pls_resource_id` from export handle.
2. Derive PE name: `{alias}-pe` in importer's `nclav-rg`.
3. `PUT /subscriptions/{importer_sub}/resourceGroups/nclav-rg/providers/Microsoft.Network/privateEndpoints/{name}?api-version=2023-11-01`
   Properties: subnet (first subnet of `nclav-vnet`), `privateLinkServiceConnections` with `pls_resource_id`.
   Poll async op.
4. GET the PE's NIC to extract `privateIPAddress`.
5. If importer has `dns.zone`: create DNS A record in Private DNS Zone.
6. Outputs: `hostname = {alias}.{dns_zone}` (or raw IP if no DNS zone), `port`.

**`type: queue`** — Service Bus wiring:

1. Grant `Azure Service Bus Data Receiver` RBAC (role `4f6d3b9b-027b-4f4c-9142-0e5a2a2247e0`) on Service Bus namespace to importer partition MI.
2. Create Private Endpoint in importer VNet pointing at Service Bus namespace (groupId: `namespace`).
3. Outputs: `queue_url`.

**Importer subscription ID resolution:**
Read from existing import handle → `enclave.identity` fallback. Returns `ProvisionFailed` if neither is available (provision_enclave must run first).

---

### `observe_enclave`

```
GET /subscriptions/{sub}?api-version=2022-12-01
```

- 200 + `"state": "Enabled"` → `exists: true, healthy: true`
- 200 + other state → `exists: true, healthy: false`
- 404 → `exists: false`

Additionally checks VNet and MI presence; sets `healthy: false` if either is missing (drift detection).

### `observe_partition`

Returns `exists: true, healthy: true` if the handle has `driver: azure` and `kind: partition`.

---

### `list_partition_resources` / `list_orphaned_resources`

Uses Azure Resource Graph API:
```
POST https://management.azure.com/providers/Microsoft.ResourceGraph/resources?api-version=2021-03-01
{
  "subscriptions": ["{sub}"],
  "query": "Resources | where tags['nclav-managed'] == 'true' and tags['nclav-partition'] == '{id}'"
}
```

`list_orphaned_resources` queries for all `nclav-managed` resources and filters where `nclav-partition` is not in `known_partition_ids`.

---

## `context_vars` Reference

Injected into partition Terraform as `nclav_context.auto.tfvars`:

| Variable | Value | Notes |
|---|---|---|
| `nclav_project_id` | subscription_id | GCP-compat alias (used by shared modules) |
| `nclav_region` | location | GCP-compat alias |
| `nclav_subscription_id` | subscription_id | Azure-specific |
| `nclav_resource_group` | `nclav-rg` | Always `nclav-rg` |
| `nclav_location` | location | e.g. `eastus2` |
| `nclav_identity_client_id` | enclave MI client ID | For workload identity assignment in Terraform |
| `nclav_enclave` | enclave ID | |

---

## `auth_env` Reference

Environment variables passed to Terraform subprocesses:

| Variable | Source |
|---|---|
| `ARM_TENANT_ID` | `config.tenant_id` |
| `ARM_SUBSCRIPTION_ID` | `handle["subscription_id"]` |
| `ARM_CLIENT_ID` | `config.client_id` (SP mode only) |
| `ARM_CLIENT_SECRET` | `config.client_secret` (SP mode only) |
| `ARM_USE_MSI` | `"true"` (MSI mode only) |

The Terraform `azurerm` provider reads these automatically.

---

## ARM Async Operation Polling

Many Azure operations return `202` with `Azure-AsyncOperation` or `Location` header.

Poll pattern:
- Read `Azure-AsyncOperation` header → GET until `{ "status": "Succeeded" | "Failed" | "Canceled" }`.
- Backoff: `[1, 2, 4, 8, 16, 30]` seconds, cycling. Max 120 polls (~58 minutes).
- INFO log every 10 polls.
- On `"Failed"` or `"Canceled"`: extract `error.code: message` from body, return `DriverError::ProvisionFailed`.

---

## Idempotency Strategy

| Operation | Idempotency mechanism |
|---|---|
| `provision_enclave` | `provisioning_complete: true` flag in handle — early return if set |
| `create_subscription` | 409 → GET alias to retrieve existing subscription ID |
| `move_to_management_group` | 409 → treated as success (already in MG) |
| `create_resource_group` | Conflict error → treated as success |
| `provision_partition` | Handle with `kind: partition` and `driver: azure` → early return |
| `assign_role` | 409 `RoleAssignmentExists` → success |
| `teardown_partition` (MI delete) | 404 → non-fatal warning |
| `teardown_enclave` (cancel sub) | `SubscriptionNotFound` → non-fatal warning |

---

## Tagging Strategy

All nclav-managed resources are tagged for drift detection and orphan scanning:

| Tag | Value | Applied to |
|---|---|---|
| `nclav-managed` | `"true"` | All resources |
| `nclav-enclave` | enclave ID | Resource groups, VNet, MI, DNS zones, PEs |
| `nclav-partition` | partition ID | Partition MIs, TF-managed resources |

---

## Out of Scope for v1

- AKS / Container Apps integration (partitions are pure Terraform)
- Hub-spoke networking (each enclave has its own VNet)
- Certificate-based (mTLS) auth for service principals
- Custom Private Link approval workflows (auto-approve assumed)
- Azure Policy enforcement on new subscriptions
- Partition-level Terraform auth isolation (nclav server SP runs all Terraform; partition MI is for workloads only)
- Azure Lighthouse for cross-tenant management
- Budget alerts on new subscriptions

---

## Example: End-to-End Azure Enclave

### 1. Start the server

```bash
nclav serve --cloud azure \
  --azure-tenant-id $TENANT_ID \
  --azure-management-group-id $MG_ID \
  --azure-billing-account-name $BILLING_ACCOUNT \
  --azure-billing-profile-name $BILLING_PROFILE \
  --azure-invoice-section-name $INVOICE_SECTION \
  --azure-client-id $CLIENT_ID \
  --azure-client-secret $CLIENT_SECRET
```

### 2. Enclave YAML

```yaml
id: product-a-dev
name: Product A Development
cloud: azure
region: eastus2

network:
  vpc_cidr: 10.0.0.0/16
  subnets:
    - 10.0.1.0/24
    - 10.0.2.0/24

dns:
  zone: product-a.dev.internal

partitions:
  - id: api
    name: API
    backend:
      Terraform:
        dir: ./api/
```

### 3. Partition Terraform must declare these outputs

```hcl
# For http exports:
output "endpoint_url" { value = azurerm_container_app.api.latest_revision_fqdn }
output "pls_id"       { value = azurerm_private_link_service.api.id }

# Context vars available as auto.tfvars:
# nclav_subscription_id, nclav_resource_group, nclav_location, nclav_identity_client_id
```

### 4. Apply

```bash
nclav apply enclaves/
nclav status
nclav destroy product-a-dev
```
