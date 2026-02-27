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


# roles/resourcemanager.projectCreator and roles/billing.user must be granted
# at the folder/org and billing-account level respectively — not the platform
# project.  Run `terraform output iam_setup_commands` after apply and send the
# printed gcloud commands to your GCP admin.

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

  # Detect whether gcp_parent is a folder or an organization so we can emit
  # the correct gcloud command family in the iam_setup_commands output.
  _parent_is_folder   = startswith(var.gcp_parent, "folders/")
  _gcloud_parent_bind = local._parent_is_folder ? "gcloud resource-manager folders add-iam-policy-binding ${trimprefix(var.gcp_parent, "folders/")}" : "gcloud organizations add-iam-policy-binding ${trimprefix(var.gcp_parent, "organizations/")}"
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

# Allow unauthenticated invocations so the nclav CLI (running locally) can
# reach the Cloud Run service.  Application-level authentication is handled by
# nclav's own Bearer token middleware — GCP IAM auth is not used here.
resource "google_cloud_run_v2_service_iam_member" "public_invoker" {
  project  = var.project_id
  location = google_cloud_run_v2_service.nclav.location
  name     = google_cloud_run_v2_service.nclav.name
  role     = "roles/run.invoker"
  member   = "allUsers"
}
