# bootstrap/gcp — Deploy nclav to Cloud Run

This Terraform module provisions the nclav API server on GCP as a Cloud Run
service. Run it once to set up the nclav platform; your enclave workloads are
managed by nclav afterwards.

## What it creates

| Resource | Name | Purpose |
|----------|------|---------|
| Service Account | `nclav-server@{project}.iam.gserviceaccount.com` | Identity for the Cloud Run service |
| GCS Bucket | `{project}-nclav-state` | Persistent storage for the redb state file (GCS volume mount) |
| Secret Manager | `nclav-api-token` | Bearer token for CLI authentication |
| Cloud Run service | `nclav-api` | The nclav API server |

## Prerequisites

1. A GCP project to deploy the nclav platform into (`project_id`).
2. A billing account to attach to new enclave projects.
3. A folder or organization under which enclave projects will be created.
4. Application Default Credentials with billing scope:
   ```sh
   gcloud auth application-default login \
     --scopes=https://www.googleapis.com/auth/cloud-platform,https://www.googleapis.com/auth/cloud-billing
   ```
5. Terraform or OpenTofu installed.

## One-time setup

```sh
cd bootstrap/gcp

# Create a terraform.tfvars file:
cat > terraform.tfvars <<EOF
project_id         = "my-platform-project"
region             = "us-central1"
nclav_image        = "ghcr.io/your-org/nclav:latest"
gcp_parent         = "folders/123456789"
billing_account    = "XXXXXX-YYYYYY-ZZZZZZ"
gcp_project_prefix = "myorg"
EOF

terraform init
terraform apply
```

## Getting the API token

After `terraform apply`, fetch the token with:

```sh
gcloud secrets versions access latest \
  --secret=nclav-api-token \
  --project=<your-platform-project>
```

Or use the Terraform output:

```sh
terraform output -raw token_fetch_command | bash
```

## Connecting the CLI

```sh
# Point the CLI at the Cloud Run API:
export NCLAV_REMOTE=$(terraform output -raw api_url)
export NCLAV_TOKEN=$(terraform output -raw token_fetch_command | bash)

nclav status
nclav apply enclaves/
```

## Notes

- The Cloud Run service scales to zero when idle (min instances = 0).
- State is persisted in the GCS bucket; the redb file is mounted at `/mnt/state/`.
- Restarting or redeploying the Cloud Run service preserves state.
- The API token is generated once by Terraform. Rotate it by:
  ```sh
  terraform taint random_bytes.api_token
  terraform apply
  ```
