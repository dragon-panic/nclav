# Cloud Run (API) for the product-a-dev enclave.
#
# Connects to the db partition via the PSC endpoint that nclav provisions in
# product-a-dev's VPC. The `db_host` variable is a DNS name registered by nclav
# in this enclave's Cloud DNS — it resolves to the PSC endpoint IP and is only
# reachable from within this enclave's VPC.
#
# Variables are supplied by nclav via nclav_context.auto.tfvars.

terraform {
  required_providers {
    google = {
      source  = "hashicorp/google"
      version = "~> 5.0"
    }
  }
}

variable "project_id" {}
variable "region" {}
variable "db_host" {}   # nclav-provisioned DNS name for the db PSC endpoint
variable "db_port" {}

provider "google" {
  project = var.project_id
  region  = var.region
}

# ── VPC and subnet (created by nclav GCP driver) ─────────────────────────────

data "google_compute_network" "nclav" {
  name    = "nclav-vpc"
  project = var.project_id
}

data "google_compute_subnetwork" "nclav" {
  name    = "subnet-0"
  region  = var.region
  project = var.project_id
}

# ── Cloud Run service ─────────────────────────────────────────────────────────

resource "google_cloud_run_v2_service" "api" {
  name     = "api"
  location = var.region

  template {
    # Route outbound traffic through the enclave VPC so the PSC endpoint and
    # its DNS name are reachable. PRIVATE_RANGES_ONLY keeps public egress
    # (e.g. container pulls) on the default internet path.
    vpc_access {
      network_interfaces {
        network    = data.google_compute_network.nclav.name
        subnetwork = data.google_compute_subnetwork.nclav.name
      }
      egress = "PRIVATE_RANGES_ONLY"
    }

    containers {
      image = "gcr.io/cloudrun/hello"

      env {
        name  = "DB_HOST"
        value = var.db_host
      }
      env {
        name  = "DB_PORT"
        value = var.db_port
      }

      ports {
        container_port = 8080
      }

      resources {
        limits = {
          cpu    = "1"
          memory = "512Mi"
        }
      }
    }
  }
}

# ── Outputs (must match declared_outputs in config.yml) ───────────────────────

output "hostname" {
  description = "Cloud Run service hostname (no scheme)."
  value       = trimprefix(google_cloud_run_v2_service.api.uri, "https://")
}

output "port" {
  description = "HTTPS port."
  value       = "443"
}
