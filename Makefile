# nclav GCP deployment helpers.
#
# Quick start:
#   cp bootstrap/gcp/terraform.tfvars.example bootstrap/gcp/terraform.tfvars
#   # edit terraform.tfvars with your project/org/billing values
#   make bootstrap-gcp GCP_PROJECT=my-platform-project
#   eval $(make connect GCP_PROJECT=my-platform-project)
#   nclav status
#
# Override the image tag or bundled IaC versions:
#   make push-ar GCP_PROJECT=... TAG=v0.1.0
#   make push-ar GCP_PROJECT=... TERRAFORM_VERSION=1.10.0 TOFU_VERSION=1.9.0

GCP_PROJECT ?=
AR_REGION   ?= us-central1
TAG         ?= latest

TERRAFORM_VERSION ?= 1.9.8
TOFU_VERSION      ?= 1.8.3

AR_IMAGE := $(AR_REGION)-docker.pkg.dev/$(GCP_PROJECT)/nclav/nclav

.PHONY: push-ar bootstrap-gcp connect _check-gcp-project

_check-gcp-project:
	@test -n "$(GCP_PROJECT)" || \
	  (echo "ERROR: GCP_PROJECT is not set.  Run: make <target> GCP_PROJECT=your-gcp-project-id" && exit 1)

## Build and push to GCP Artifact Registry (gcloud auth must be active).
## Usage: make push-ar GCP_PROJECT=your-project-id [AR_REGION=us-central1] [TAG=latest]
push-ar: _check-gcp-project
	gcloud auth configure-docker $(AR_REGION)-docker.pkg.dev --quiet
	docker build \
	  --platform linux/amd64 \
	  --build-arg TERRAFORM_VERSION=$(TERRAFORM_VERSION) \
	  --build-arg TOFU_VERSION=$(TOFU_VERSION) \
	  -t $(AR_IMAGE):$(TAG) \
	  .
	docker push $(AR_IMAGE):$(TAG)

## Full GCP bootstrap: enable APIs → create AR repo → build+push image → deploy Cloud Run + Cloud SQL.
## Requires bootstrap/gcp/terraform.tfvars (copy from terraform.tfvars.example and fill in values).
## GCP_PROJECT must match project_id in terraform.tfvars. Terraform will prompt for approval twice.
## Usage: make bootstrap-gcp GCP_PROJECT=my-project [AR_REGION=us-central1]
bootstrap-gcp: _check-gcp-project
	cd bootstrap/gcp && terraform init
	cd bootstrap/gcp && terraform apply \
	  -target=google_artifact_registry_repository.nclav \
	  -target=google_project_service.apis
	$(MAKE) push-ar GCP_PROJECT=$(GCP_PROJECT) AR_REGION=$(AR_REGION)
	cd bootstrap/gcp && terraform apply

## Start the gcloud proxy in the background and print env vars for the nclav CLI.
## Usage: eval $(make connect GCP_PROJECT=my-project [AR_REGION=us-central1])
## Then run: nclav status
connect: _check-gcp-project
	@nohup gcloud run services proxy nclav-api \
	  --project=$(GCP_PROJECT) --region=$(AR_REGION) --port=8080 \
	  >> /tmp/nclav-proxy.log 2>&1 &
	@echo "export NCLAV_URL=http://localhost:8080"
	@echo "export NCLAV_TOKEN=$$(gcloud secrets versions access latest --secret=nclav-api-token --project=$(GCP_PROJECT))"
	@echo "# Proxy running in background. Logs: /tmp/nclav-proxy.log" >&2
	@echo "# To stop: pkill -f 'gcloud run services proxy'" >&2
