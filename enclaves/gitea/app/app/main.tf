# Cloud Run (Gitea) for the gitea-app enclave.
#
# Connects to PostgreSQL in gitea-db via a Private Service Connect endpoint
# that nclav provisions in gitea-app's VPC. The `db_host` variable is a DNS
# name registered by nclav in gitea-app's Cloud DNS — it resolves to the PSC
# endpoint IP and is only reachable from within this enclave's VPC.
#
# Cloud Run uses Direct VPC Egress so that DNS lookups and TCP connections
# to the PSC endpoint are routed through the nclav-vpc in this project.
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
variable "db_host" {}     # nclav-provisioned DNS name for the PSC endpoint
variable "db_port" {}
variable "db_name" {}
variable "db_user" {}
variable "db_password" { sensitive = true }

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

resource "google_cloud_run_v2_service" "gitea" {
  name     = "gitea"
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
      image = "gitea/gitea:latest"

      # ── Database connection ────────────────────────────────────────────────
      # db_host resolves within this VPC to the PSC endpoint for gitea-db's
      # postgres partition. nclav provisions the endpoint and DNS record;
      # this Terraform has no visibility into how that wiring works.
      env {
        name  = "GITEA__database__DB_TYPE"
        value = "postgres"
      }
      env {
        name  = "GITEA__database__HOST"
        value = "${var.db_host}:${var.db_port}"
      }
      env {
        name  = "GITEA__database__NAME"
        value = var.db_name
      }
      env {
        name  = "GITEA__database__USER"
        value = var.db_user
      }
      env {
        name  = "GITEA__database__PASSWD"
        value = var.db_password
      }
      env {
        name  = "GITEA__database__SSL_MODE"
        value = "disable"
      }

      # ── Server config ──────────────────────────────────────────────────────
      env {
        name  = "GITEA__server__APP_URL"
        value = "https://${var.region}-${var.project_id}.a.run.app/"
      }
      env {
        name  = "GITEA__server__ROOT_URL"
        value = "https://${var.region}-${var.project_id}.a.run.app/"
      }
      # Cloud Run is stateless; repos live in /tmp for this fixture.
      # For a persistent deployment, mount Cloud Filestore or GCS FUSE here.
      env {
        name  = "GITEA__repository__ROOT"
        value = "/tmp/gitea/repos"
      }

      ports {
        container_port = 3000
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

# Allow unauthenticated invocations — nclav's token auth layer (from the
# app-http export) handles access control at the application boundary.
resource "google_cloud_run_v2_service_iam_member" "public" {
  location = google_cloud_run_v2_service.gitea.location
  name     = google_cloud_run_v2_service.gitea.name
  role     = "roles/run.invoker"
  member   = "allUsers"
}

# ── Outputs (must match declared_outputs in config.yml) ───────────────────────

output "hostname" {
  description = "Cloud Run service hostname (no scheme)."
  value       = trimprefix(google_cloud_run_v2_service.gitea.uri, "https://")
}

output "port" {
  description = "HTTPS port."
  value       = "443"
}
