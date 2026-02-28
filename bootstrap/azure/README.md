# bootstrap/azure — Deploy nclav to Azure Container Apps

This Terraform module provisions the nclav API server on Azure as a Container
App. Run it once to set up the nclav platform; your enclave workloads are
managed by nclav afterwards.

## What it creates

| Resource | Name | Purpose |
|----------|------|---------|
| Resource Group | `{resource_group_name}` | Container for all platform resources |
| User-Assigned Identity | `nclav-server` | Identity for the Container App |
| Container Registry | `{acr_name}` | Hosts the nclav container image |
| Key Vault | `{key_vault_name}` | Stores the API token for CLI access |
| PostgreSQL Flexible Server | `nclav-state-{suffix}` | Persistent state store |
| Container App Environment | `nclav-env` | Hosting environment for the app |
| Container App | `nclav-api` | The nclav API server |

## Prerequisites

1. An Azure subscription for the nclav platform (not an enclave subscription — a dedicated ops subscription).
2. A management group under which subscription enclaves will be created.
3. An MCA (Microsoft Customer Agreement) billing account with a billing profile and invoice section.
4. Azure CLI installed and authenticated:
   ```sh
   az login
   az account set --subscription YOUR_PLATFORM_SUBSCRIPTION_ID
   ```
5. Terraform or OpenTofu installed.
6. Docker installed (to push the nclav image to ACR).
7. The `Microsoft.App` and `Microsoft.DBforPostgreSQL` resource providers registered on your platform subscription:
   ```sh
   az provider register --namespace Microsoft.App
   az provider register --namespace Microsoft.DBforPostgreSQL
   az provider register --namespace Microsoft.KeyVault
   ```

## One-time setup

Bootstrap happens in two phases because the Container App needs the image to
exist in ACR before it can start.

### Phase 1 — provision the registry

```sh
cd bootstrap/azure

cat > terraform.tfvars <<EOF
platform_subscription_id  = "$(az account show --query id -o tsv)"
azure_tenant_id           = "$(az account show --query tenantId -o tsv)"
azure_management_group_id = "my-management-group"
azure_billing_account_name  = "XXXXXXXX:XXXXXXXX_2019-05-31"
azure_billing_profile_name  = "XXXX-XXXX-XXX-XXX"
azure_invoice_section_name  = "XXXX-XXXX-XXX-XXX-XXX"
acr_name                  = "myorgnclav"
key_vault_name            = "myorg-nclav-kv"
azure_subscription_prefix = "myorg"
EOF

terraform init
# Create the ACR repo first (image must exist before Container App starts)
terraform apply \
  -target=azurerm_resource_group.nclav \
  -target=azurerm_container_registry.nclav
```

### Phase 2 — build and push the image

```sh
# From the repo root:
make push-acr AZURE_ACR=myorgnclav
```

Or manually:
```sh
az acr login --name myorgnclav
docker build --platform linux/amd64 -t myorgnclav.azurecr.io/nclav:latest .
docker push myorgnclav.azurecr.io/nclav:latest
```

### Phase 3 — full apply

```sh
cd bootstrap/azure
terraform apply
```

This creates the PostgreSQL server, Key Vault, and Container App pointing at
the image you just pushed.

## Getting the API token

After `terraform apply`, fetch the token with:

```sh
terraform output -raw token_fetch_command | bash
```

## Connecting the CLI

The Container App exposes a public HTTPS endpoint. One command sets the
required env vars:

```sh
eval $(make connect-azure)
nclav status
nclav apply enclaves/
```

`make connect-azure` fetches the app URL and API token from Terraform outputs
and Key Vault. To set them manually:

```sh
export NCLAV_URL=https://$(cd bootstrap/azure && terraform output -raw app_fqdn)
export NCLAV_TOKEN=$(az keyvault secret show \
  --vault-name $(cd bootstrap/azure && terraform output -raw key_vault_name) \
  --name nclav-api-token --query value -o tsv)
```

## IAM setup (admin required)

After apply, print and run the required `az` commands:

```sh
cd bootstrap/azure && terraform output -raw iam_setup_commands
```

These grant `nclav-server` the roles it needs:

| Role | Scope | Purpose |
|---|---|---|
| `Owner` | Management Group | Create subscriptions, move them, grant RBAC inside |
| `Invoice Section Contributor` | MCA invoice section | Link billing to new subscriptions |

The Management Group Owner assignment can be run by anyone with Owner on the
MG. The billing role requires a billing admin account (`az login
--allow-no-subscriptions` then switch to the billing-admin account).

## Notes

- The Container App scales to zero when idle (`min_replicas = 0`).
- State is persisted in PostgreSQL Flexible Server; no local disk needed.
- Restarting or redeploying the Container App preserves state.
- The API token is generated once by Terraform. Rotate it by:
  ```sh
  terraform apply -replace=random_bytes.api_token
  ```
  Then re-fetch with `make connect-azure`.
- To deploy a new version of nclav: `make push-acr AZURE_ACR=...` then
  `terraform apply` (Container App picks up the new `latest` tag on next revision).
- The PostgreSQL server name includes a random suffix to ensure global uniqueness.
  It is stable across `terraform apply` runs once the suffix is generated.
