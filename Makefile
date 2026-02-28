# Build and publish the nclav container image to GHCR.
#
# Quick start:
#   export GITHUB_ORG=your-github-org-or-username
#   export GITHUB_TOKEN=ghp_...          # PAT with write:packages scope
#   make login-ghcr
#   make push
#
# Override the tag:
#   make push TAG=v0.1.0
#
# Override IaC tool versions:
#   make push TERRAFORM_VERSION=1.10.0 TOFU_VERSION=1.9.0

GITHUB_ORG  ?=
GCP_PROJECT ?=
AR_REGION   ?= us-central1
TAG         ?= latest

TERRAFORM_VERSION ?= 1.9.8
TOFU_VERSION      ?= 1.8.3

IMAGE    := ghcr.io/$(GITHUB_ORG)/nclav
AR_IMAGE := $(AR_REGION)-docker.pkg.dev/$(GCP_PROJECT)/nclav/nclav

.PHONY: build push push-ar login-ghcr _check-org _check-gcp-project

_check-org:
	@test -n "$(GITHUB_ORG)" || \
	  (echo "ERROR: GITHUB_ORG is not set.  Run: make <target> GITHUB_ORG=your-github-org" && exit 1)

## Log in to GHCR using GITHUB_TOKEN env var.
## Requires: export GITHUB_TOKEN=ghp_... (PAT with write:packages scope)
login-ghcr: _check-org
	@test -n "$${GITHUB_TOKEN}" || \
	  (echo "ERROR: GITHUB_TOKEN is not set." && exit 1)
	@echo "$${GITHUB_TOKEN}" | docker login ghcr.io -u "$(GITHUB_ORG)" --password-stdin

## Build the nclav image for linux/amd64 (Cloud Run target platform).
build: _check-org
	docker build \
	  --platform linux/amd64 \
	  --build-arg TERRAFORM_VERSION=$(TERRAFORM_VERSION) \
	  --build-arg TOFU_VERSION=$(TOFU_VERSION) \
	  -t $(IMAGE):$(TAG) \
	  .

## Build and push to GHCR (run login-ghcr first).
push: build
	docker push $(IMAGE):$(TAG)

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
