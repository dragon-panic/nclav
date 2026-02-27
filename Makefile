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

GITHUB_ORG ?=
TAG        ?= latest

TERRAFORM_VERSION ?= 1.9.8
TOFU_VERSION      ?= 1.8.3

IMAGE := ghcr.io/$(GITHUB_ORG)/nclav

.PHONY: build push login-ghcr _check-org

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
