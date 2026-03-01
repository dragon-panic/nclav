# Hosted deployment — GCP

Run the nclav API as a Cloud Run service backed by Cloud SQL. No local process required after setup. The Terraform bootstrap lives in `bootstrap/gcp/`.

**What it creates:**

| Resource | Name | Purpose |
|---|---|---|
| Service Account | `nclav-server@{project}.iam.gserviceaccount.com` | Identity for the Cloud Run service |
| Cloud SQL (Postgres 16) | `nclav-state` | Persistent state store |
| Secret Manager secret | `nclav-api-token` | Bearer token for CLI authentication |
| Secret Manager secret | `nclav-db-url` | Postgres connection URL for the Cloud Run service |
| Cloud Run service | `nclav-api` | The nclav API server |

## Prerequisites

- A GCP folder or organization under which enclave projects will be created
- A billing account for enclave projects
- A GCP project for the nclav platform (not an enclave project — a dedicated ops project).
  Create one if you don't have it yet:
  ```bash
  gcloud projects create myorg-nclav --organization=ORGANIZATION_ID
  gcloud billing projects link myorg-nclav --billing-account=XXXXXX-YYYYYY-ZZZZZZ
  ```
- Terraform or OpenTofu installed locally
- ADC with billing scope:
  ```bash
  gcloud auth application-default login \
    --scopes=https://www.googleapis.com/auth/cloud-platform,https://www.googleapis.com/auth/cloud-billing
  ```

## Step 1 — Configure and deploy

Copy the vars template and fill in your values:

```bash
cp bootstrap/gcp/terraform.tfvars.example bootstrap/gcp/terraform.tfvars
# edit bootstrap/gcp/terraform.tfvars
```

Then run the full bootstrap in one command:

```bash
make bootstrap-gcp GCP_PROJECT=my-platform-project
```

This handles the two-phase Terraform apply (APIs + AR repo first, then Cloud Run + Cloud SQL) and builds and pushes the container image in between. Terraform will prompt for plan approval twice.

**Doing it manually** (if you prefer to see each step):

```bash
cd bootstrap/gcp
terraform init
# Phase 1: AR repo + APIs
terraform apply -target=google_artifact_registry_repository.nclav \
                -target=google_project_service.apis
# Build and push the image
cd ../..
make push-ar GCP_PROJECT=my-platform-project AR_REGION=us-central1
# Phase 2: everything else
cd bootstrap/gcp
terraform apply
```

## Step 2 — Grant folder-level IAM

Terraform cannot grant folder/org-level IAM without admin credentials. After `terraform apply`, print the required `gcloud` commands:

```bash
cd bootstrap/gcp && terraform output -raw iam_setup_commands
```

These grant `nclav-server` the roles it needs (`projectCreator`, `iam.serviceAccountAdmin`, `compute.networkAdmin`, etc.) at the folder or org level, plus `billing.user` on the billing account. **If you are the org admin, run them yourself.** Otherwise send them to whoever manages your GCP org. These are one-time grants and do not need to be repeated per enclave.

## Step 3 — Connect the CLI

The Cloud Run service is private by default. One command starts the proxy and sets the required env vars:

```bash
eval $(make connect GCP_PROJECT=my-platform-project)
nclav status
nclav apply enclaves/
```

`make connect` runs `gcloud run services proxy` in the background (logs to `/tmp/nclav-proxy.log`) and fetches the API token from Secret Manager. Add the exported vars to your shell profile to avoid re-running after the proxy is already up:

```bash
export NCLAV_URL=http://localhost:8080
export NCLAV_TOKEN=$(gcloud secrets versions access latest \
  --secret=nclav-api-token --project=my-platform-project)
```

To stop the proxy: `pkill -f 'gcloud run services proxy'`

## Token rotation

```bash
cd bootstrap/gcp

# Generate a new token and update the Secret Manager version
terraform apply -replace=random_bytes.api_token

# Cloud Run reads secrets at revision creation time, so force a new revision.
# Updating an env var is the standard way to trigger this without redeploying.
gcloud run services update nclav-api \
  --project=my-platform-project \
  --region=us-central1 \
  --update-env-vars=_RESTART=$(date +%s)

# Fetch the new token
export NCLAV_TOKEN=$(gcloud secrets versions access latest \
  --secret=nclav-api-token --project=my-platform-project)
```
