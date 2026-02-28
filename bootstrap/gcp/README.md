# bootstrap/gcp — Deploy nclav to Cloud Run

This Terraform module provisions the nclav API server on GCP as a Cloud Run
service. Run it once to set up the nclav platform; your enclave workloads are
managed by nclav afterwards.

## What it creates

| Resource | Name | Purpose |
|----------|------|---------|
| Service Account | `nclav-server@{project}.iam.gserviceaccount.com` | Identity for the Cloud Run service |
| Artifact Registry repo | `nclav` | Hosts the nclav container image |
| Cloud SQL (Postgres 16) | `nclav-state` | Persistent state store |
| Secret Manager | `nclav-api-token` | Bearer token for CLI authentication |
| Secret Manager | `nclav-db-url` | Postgres connection URL (injected into Cloud Run) |
| Cloud Run service | `nclav-api` | The nclav API server |

## Prerequisites

1. A GCP project to deploy the nclav platform into (the *platform project*).
   ```sh
   gcloud projects create my-platform-project --folder=FOLDER_ID
   gcloud beta billing projects link my-platform-project --billing-account=BILLING_ACCOUNT_ID
   ```
2. A billing account to attach to new enclave projects.
3. A folder or organization under which enclave projects will be created.
4. Application Default Credentials with billing scope:
   ```sh
   gcloud auth application-default login \
     --scopes=https://www.googleapis.com/auth/cloud-platform,https://www.googleapis.com/auth/cloud-billing
   ```
5. Terraform or OpenTofu installed.
6. Docker installed (to push the nclav image to Artifact Registry).

## One-time setup

Bootstrap happens in two phases because the Cloud Run service needs the image
to exist in Artifact Registry before it can start.

### Phase 1 — provision infrastructure

```sh
cd bootstrap/gcp

cat > terraform.tfvars <<EOF
project_id         = "my-platform-project"
region             = "us-central1"
gcp_parent         = "folders/123456789"
billing_account    = "XXXXXX-YYYYYY-ZZZZZZ"
gcp_project_prefix = "myorg"
EOF

terraform init
# Create the AR repo and everything except Cloud Run
terraform apply -target=google_artifact_registry_repository.nclav \
                -target=google_project_service.apis
```

### Phase 2 — build and push the image

```sh
# From the repo root:
make push-ar GCP_PROJECT=my-platform-project AR_REGION=us-central1
```

Or manually:
```sh
gcloud auth configure-docker us-central1-docker.pkg.dev --quiet
docker build --platform linux/amd64 -t us-central1-docker.pkg.dev/my-platform-project/nclav/nclav:latest .
docker push us-central1-docker.pkg.dev/my-platform-project/nclav/nclav:latest
```

### Phase 3 — full apply

```sh
cd bootstrap/gcp
terraform apply
```

This creates Cloud SQL, Secrets, and the Cloud Run service pointing at the
image you just pushed.

## Getting the API token

After `terraform apply`, fetch the token with:

```sh
terraform output -raw token_fetch_command | bash
```

## Connecting the CLI

The Cloud Run service is private by default. Use `gcloud run services proxy`
to open an authenticated local tunnel:

```sh
# Terminal 1 — keep this running:
gcloud run services proxy nclav-api --project=my-platform-project --region=us-central1 --port=8080

# Terminal 2 — use the CLI normally via localhost:
export NCLAV_URL=http://localhost:8080
export NCLAV_TOKEN=$(gcloud secrets versions access latest --secret=nclav-api-token --project=my-platform-project)

nclav status
nclav apply enclaves/
```

To grant direct access to specific users instead of using the proxy, add to
`terraform.tfvars`:

```hcl
allowed_invokers = ["user:you@example.com", "serviceAccount:ci@project.iam.gserviceaccount.com"]
```

## IAM setup (folder/org admin required)

After apply, run the commands printed by:

```sh
terraform output -raw iam_setup_commands
```

These grant the `nclav-server` SA the folder/org-level roles it needs to
create and manage GCP projects for each enclave.

## Notes

- The Cloud Run service scales to zero when idle (`min_instance_count = 0`).
- State is persisted in Cloud SQL (Postgres 16); no local disk needed.
- Restarting or redeploying the Cloud Run service preserves state.
- The API token is generated once by Terraform. Rotate it by:
  ```sh
  terraform taint random_bytes.api_token
  terraform apply
  ```
- To deploy a new version of nclav: `make push-ar GCP_PROJECT=...` then
  `terraform apply` (Cloud Run picks up the new `latest` tag on next revision).
