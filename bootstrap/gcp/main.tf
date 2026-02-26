# bootstrap/gcp/main.tf — Deploy nclav to Cloud Run (GCP platform)
#
# This module provisions the nclav API server itself on GCP.  It is a
# one-time setup; your enclave workloads are managed by nclav afterwards.
#
# What it creates:
#   - Service account: nclav-server@ (used by the Cloud Run service)
#   - GCS bucket: {project_id}-nclav-state (GCS volume mount for redb state)
#   - Secret Manager secret: nclav-api-token (random 32-byte hex bearer token)
#   - Cloud Run service: nclav-api (gen2, GCS volume at /mnt/state/)
#
# Prerequisites: see README.md.

terraform {
  required_providers {
    google = {
      source  = "hashicorp/google"
      version = "~> 5.0"
    }
    random = {
      source  = "hashicorp/random"
      version = "~> 3.0"
    }
  }
}

provider "google" {
  project = var.project_id
  region  = var.region
}

# ── Enable required APIs ──────────────────────────────────────────────────────

resource "google_project_service" "apis" {
  for_each = toset([
    "run.googleapis.com",
    "secretmanager.googleapis.com",
    "storage.googleapis.com",
    "iam.googleapis.com",
    "cloudresourcemanager.googleapis.com",
  ])
  service            = each.key
  disable_on_destroy = false
}

# ── Service account for the nclav server ──────────────────────────────────────

resource "google_service_account" "nclav" {
  account_id   = "nclav-server"
  display_name = "nclav API Server"
  depends_on   = [google_project_service.apis]
}

# nclav needs project-level permissions to create enclave projects,
# manage IAM, enable APIs, and access Cloud Asset Inventory.
resource "google_project_iam_member" "nclav_editor" {
  project = var.project_id
  role    = "roles/editor"
  member  = "serviceAccount:${google_service_account.nclav.email}"
}

resource "google_project_iam_member" "nclav_resourcemanager" {
  project = var.project_id
  role    = "roles/resourcemanager.projectCreator"
  member  = "serviceAccount:${google_service_account.nclav.email}"
}

resource "google_project_iam_member" "nclav_billing_user" {
  project = var.project_id
  role    = "roles/billing.user"
  member  = "serviceAccount:${google_service_account.nclav.email}"
}

# ── GCS bucket for redb state ─────────────────────────────────────────────────

resource "google_storage_bucket" "state" {
  name                        = "${var.project_id}-nclav-state"
  location                    = var.region
  uniform_bucket_level_access = true
  force_destroy               = false

  versioning {
    enabled = true
  }

  depends_on = [google_project_service.apis]
}

resource "google_storage_bucket_iam_member" "nclav_state_rw" {
  bucket = google_storage_bucket.state.name
  role   = "roles/storage.objectAdmin"
  member = "serviceAccount:${google_service_account.nclav.email}"
}

# ── API token (Secret Manager) ────────────────────────────────────────────────

resource "random_bytes" "api_token" {
  length = 32
}

resource "google_secret_manager_secret" "api_token" {
  secret_id = "nclav-api-token"

  replication {
    auto {}
  }

  depends_on = [google_project_service.apis]
}

resource "google_secret_manager_secret_version" "api_token" {
  secret      = google_secret_manager_secret.api_token.id
  secret_data = random_bytes.api_token.hex
}

resource "google_secret_manager_secret_iam_member" "nclav_token_access" {
  secret_id = google_secret_manager_secret.api_token.id
  role      = "roles/secretmanager.secretAccessor"
  member    = "serviceAccount:${google_service_account.nclav.email}"
}

# ── Cloud Run service ─────────────────────────────────────────────────────────

locals {
  gcp_prefix_flag = var.gcp_project_prefix != "" ? ["--gcp-project-prefix", var.gcp_project_prefix] : []
}

resource "google_cloud_run_v2_service" "nclav" {
  name     = "nclav-api"
  location = var.region

  template {
    service_account = google_service_account.nclav.email

    # Mount the GCS bucket at /mnt/state/ for the redb state file.
    volumes {
      name = "state"
      gcs {
        bucket    = google_storage_bucket.state.name
        read_only = false
      }
    }

    containers {
      image = var.nclav_image

      args = concat(
        [
          "serve",
          "--bind", "0.0.0.0",
          "--cloud", "gcp",
          "--store-path", "/mnt/state/state.redb",
          "--gcp-parent", var.gcp_parent,
          "--gcp-billing-account", var.billing_account,
        ],
        local.gcp_prefix_flag
      )

      env {
        name = "NCLAV_TOKEN"
        value_source {
          secret_key_ref {
            secret  = google_secret_manager_secret.api_token.secret_id
            version = "latest"
          }
        }
      }

      volume_mounts {
        name       = "state"
        mount_path = "/mnt/state"
      }

      ports {
        container_port = 8080
      }

      resources {
        limits = {
          cpu    = "1"
          memory = "512Mi"
        }
        startup_cpu_boost = true
      }
    }

    scaling {
      min_instance_count = 0
      max_instance_count = 1
    }
  }

  depends_on = [
    google_project_service.apis,
    google_storage_bucket_iam_member.nclav_state_rw,
    google_secret_manager_secret_iam_member.nclav_token_access,
  ]
}

# Allow the Cloud Run service to be invoked (requires auth by default).
# Operators connect via the nclav CLI which sends the Bearer token.
resource "google_cloud_run_service_iam_member" "invoker" {
  location = google_cloud_run_v2_service.nclav.location
  service  = google_cloud_run_v2_service.nclav.name
  role     = "roles/run.invoker"
  member   = "serviceAccount:${google_service_account.nclav.email}"
}
