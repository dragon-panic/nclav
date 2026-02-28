# nclav GCP and Azure deployment helpers.
#
# Quick start (GCP):
#   cp bootstrap/gcp/terraform.tfvars.example bootstrap/gcp/terraform.tfvars
#   # edit terraform.tfvars with your project/org/billing values
#   make bootstrap-gcp GCP_PROJECT=my-platform-project
#   eval $(make connect GCP_PROJECT=my-platform-project)
#   nclav status
#
# Quick start (Azure):
#   cp bootstrap/azure/terraform.tfvars.example bootstrap/azure/terraform.tfvars
#   # edit terraform.tfvars with your subscription, MG, and billing values
#   make bootstrap-azure AZURE_ACR=myorgnclav
#   eval $(make connect-azure)
#   nclav status
#
# Override the image tag or bundled IaC versions:
#   make push-ar   GCP_PROJECT=... TAG=v0.1.0
#   make push-acr  AZURE_ACR=...   TAG=v0.1.0
#   make push-ar   GCP_PROJECT=... TERRAFORM_VERSION=1.10.0 TOFU_VERSION=1.9.0

GCP_PROJECT ?=
AR_REGION   ?= us-central1
AZURE_ACR   ?=
TAG         ?= latest

TERRAFORM_VERSION ?= 1.9.8
TOFU_VERSION      ?= 1.8.3

AR_IMAGE  := $(AR_REGION)-docker.pkg.dev/$(GCP_PROJECT)/nclav/nclav
ACR_IMAGE := $(AZURE_ACR).azurecr.io/nclav

.PHONY: push-ar bootstrap-gcp connect _check-gcp-project \
        push-acr bootstrap-azure connect-azure _check-azure-acr

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

# ── Azure targets ──────────────────────────────────────────────────────────────

_check-azure-acr:
	@test -n "$(AZURE_ACR)" || \
	  (echo "ERROR: AZURE_ACR is not set.  Run: make <target> AZURE_ACR=your-acr-name" && exit 1)

## Build and push to Azure Container Registry (az login must be active).
## Usage: make push-acr AZURE_ACR=your-acr-name [TAG=latest]
push-acr: _check-azure-acr
	az acr login --name $(AZURE_ACR)
	docker build \
	  --platform linux/amd64 \
	  --build-arg TERRAFORM_VERSION=$(TERRAFORM_VERSION) \
	  --build-arg TOFU_VERSION=$(TOFU_VERSION) \
	  -t $(ACR_IMAGE):$(TAG) \
	  .
	docker push $(ACR_IMAGE):$(TAG)

## Full Azure bootstrap: create ACR → build+push image → deploy Container App + PostgreSQL.
## Requires bootstrap/azure/terraform.tfvars (copy from terraform.tfvars.example and fill in values).
## AZURE_ACR must match acr_name in terraform.tfvars. Terraform will prompt for approval twice.
## Usage: make bootstrap-azure AZURE_ACR=your-acr-name
bootstrap-azure: _check-azure-acr
	cd bootstrap/azure && terraform init
	cd bootstrap/azure && terraform apply \
	  -target=azurerm_resource_group.nclav \
	  -target=azurerm_container_registry.nclav
	$(MAKE) push-acr AZURE_ACR=$(AZURE_ACR)
	cd bootstrap/azure && terraform apply

## Print env vars for the nclav CLI (fetches token from Key Vault).
## Usage: eval $(make connect-azure)
## Then run: nclav status
connect-azure:
	@VAULT_NAME=$$(cd bootstrap/azure && terraform output -raw key_vault_name) && \
	APP_FQDN=$$(cd bootstrap/azure && terraform output -raw app_fqdn) && \
	echo "export NCLAV_URL=https://$$APP_FQDN" && \
	echo "export NCLAV_TOKEN=$$(az keyvault secret show --vault-name $$VAULT_NAME --name nclav-api-token --query value -o tsv)"
