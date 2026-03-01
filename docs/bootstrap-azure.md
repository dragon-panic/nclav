# Hosted deployment — Azure

Run the nclav API as a Container App backed by PostgreSQL Flexible Server. No local process required after setup. The Terraform bootstrap lives in `bootstrap/azure/`.

**What it creates:**

| Resource | Name | Purpose |
|---|---|---|
| User-Assigned Identity | `nclav-server` | Identity for the Container App |
| Container Registry | `{acr_name}` | Hosts the nclav container image |
| Key Vault | `{key_vault_name}` | Bearer token for CLI authentication |
| PostgreSQL Flexible Server | `nclav-state-{suffix}` | Persistent state store |
| Container App | `nclav-api` | The nclav API server |

## Prerequisites

- An Azure subscription for the nclav platform (dedicated ops subscription)
- A management group under which subscription enclaves will be created
- An MCA billing account with a billing profile and invoice section
- Azure CLI installed and authenticated (`az login`)
- Terraform or OpenTofu installed locally
- Docker installed (to push the nclav image to ACR)
- Required resource providers registered:
  ```bash
  az provider register --namespace Microsoft.App
  az provider register --namespace Microsoft.DBforPostgreSQL
  az provider register --namespace Microsoft.KeyVault
  ```

## Step 1 — Configure and deploy

Copy the vars template and fill in your values:

```bash
cp bootstrap/azure/terraform.tfvars.example bootstrap/azure/terraform.tfvars
# edit bootstrap/azure/terraform.tfvars
```

Then run the full bootstrap in one command:

```bash
make bootstrap-azure AZURE_ACR=myorgnclav
```

This creates the ACR repo, builds and pushes the container image, then deploys the PostgreSQL server, Key Vault, and Container App. Terraform will prompt for plan approval twice.

**Doing it manually** (if you prefer to see each step):

```bash
cd bootstrap/azure
terraform init
# Phase 1: ACR repo (image must exist before Container App starts)
terraform apply -target=azurerm_resource_group.nclav \
                -target=azurerm_container_registry.nclav
# Build and push the image
cd ../..
make push-acr AZURE_ACR=myorgnclav
# Phase 2: everything else
cd bootstrap/azure
terraform apply
```

## Step 2 — Grant management group IAM

Terraform cannot grant management group or billing roles without admin credentials. After `terraform apply`, print the required `az` commands:

```bash
cd bootstrap/azure && terraform output -raw iam_setup_commands
```

These grant `nclav-server` the roles it needs (`Owner` on the management group and `Invoice Section Contributor` on the MCA invoice section). **If you are the Azure admin, run them yourself.** Otherwise send them to whoever manages your Azure tenant. These are one-time grants and do not need to be repeated per enclave.

## Step 3 — Connect the CLI

The Container App exposes a public HTTPS endpoint secured by the nclav Bearer token. One command sets the required env vars:

```bash
eval $(make connect-azure)
nclav status
nclav apply enclaves/
```

`make connect-azure` fetches the app URL and API token from Terraform outputs and Key Vault. Add the exported vars to your shell profile:

```bash
export NCLAV_URL=https://nclav-api.{region}.azurecontainerapps.io
export NCLAV_TOKEN=$(az keyvault secret show \
  --vault-name myorg-nclav-kv --name nclav-api-token --query value -o tsv)
```

## Token rotation

```bash
cd bootstrap/azure

# Generate a new token (updates Key Vault secret and ACA secret)
terraform apply -replace=random_bytes.api_token

# Fetch the new token
eval $(make connect-azure)
```
