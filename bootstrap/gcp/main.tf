# bootstrap/gcp/main.tf — Deploy nclav to Cloud Run (GCP platform)
#
# This module provisions the nclav API server itself on GCP.  It is a
# one-time setup; your enclave workloads are managed by nclav afterwards.
#
# What it creates:
#   - Service account: nclav-server@ (used by the Cloud Run service)
#   - Cloud SQL (Postgres 16): nclav state store (no GCS mmap issues)
#   - Secret Manager: nclav-api-token, nclav-db-url
#   - Cloud Run service: nclav-api (gen2, connects to Cloud SQL via socket)
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
    "iam.googleapis.com",
    "cloudresourcemanager.googleapis.com",
    "sqladmin.googleapis.com",
    "servicenetworking.googleapis.com",
    "artifactregistry.googleapis.com",
  ])
  service            = each.key
  disable_on_destroy = false
}

# ── Artifact Registry ─────────────────────────────────────────────────────────

resource "google_artifact_registry_repository" "nclav" {
  location      = var.region
  repository_id = "nclav"
  format        = "DOCKER"
  description   = "nclav container images"
  depends_on    = [google_project_service.apis]
}

# ── Service account for the nclav server ──────────────────────────────────────

resource "google_service_account" "nclav" {
  account_id   = "nclav-server"
  display_name = "nclav API Server"
  depends_on   = [google_project_service.apis]
}

# Editor on the platform project (Secret Manager, Cloud SQL, etc.)
resource "google_project_iam_member" "nclav_editor" {
  project = var.project_id
  role    = "roles/editor"
  member  = "serviceAccount:${google_service_account.nclav.email}"
}

# Pull images from the nclav Artifact Registry repository
resource "google_artifact_registry_repository_iam_member" "nclav_ar_reader" {
  project    = var.project_id
  location   = var.region
  repository = google_artifact_registry_repository.nclav.repository_id
  role       = "roles/artifactregistry.reader"
  member     = "serviceAccount:${google_service_account.nclav.email}"
}

# Cloud SQL client — allows the Cloud Run service to connect via socket
resource "google_project_iam_member" "nclav_cloudsql_client" {
  project = var.project_id
  role    = "roles/cloudsql.client"
  member  = "serviceAccount:${google_service_account.nclav.email}"
}

# roles/resourcemanager.projectCreator and roles/billing.user must be granted
# at the folder/org and billing-account level respectively — not the platform
# project.  Run `terraform output iam_setup_commands` after apply and send the
# printed gcloud commands to your GCP admin.

# ── Cloud SQL (Postgres 16) ───────────────────────────────────────────────────

resource "random_password" "db_password" {
  length  = 32
  special = false
}

resource "google_sql_database_instance" "nclav" {
  name             = "nclav-state"
  database_version = "POSTGRES_16"
  region           = var.region

  settings {
    tier              = var.cloud_sql_tier
    availability_type = "ZONAL"

    backup_configuration {
      enabled = true
    }

    ip_configuration {
      # Cloud SQL Auth Proxy requires the instance to have a reachable IP.
      # Public IP is fine — the proxy authenticates via IAM and encrypts
      # the tunnel; postgres is never directly accessible from the internet.
      ipv4_enabled = true
    }

    deletion_protection_enabled = false
  }

  deletion_protection = false
  depends_on          = [google_project_service.apis]
}

resource "google_sql_database" "nclav" {
  name     = "nclav"
  instance = google_sql_database_instance.nclav.name
}

resource "google_sql_user" "nclav" {
  name     = "nclav"
  instance = google_sql_database_instance.nclav.name
  password = random_password.db_password.result
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

# ── Database URL secret ───────────────────────────────────────────────────────

locals {
  # sqlx requires a non-empty host in the URL authority; "localhost" is a
  # placeholder — the "host=" query parameter overrides it with the Cloud SQL
  # Auth Proxy Unix socket path, so no TCP connection to localhost is made.
  db_url          = "postgres://nclav:${random_password.db_password.result}@localhost/nclav?host=/cloudsql/${google_sql_database_instance.nclav.connection_name}"
  gcp_prefix_flag = var.gcp_project_prefix != "" ? ["--gcp-project-prefix", var.gcp_project_prefix] : []

  # Effective container image: use the caller-supplied image if given, otherwise
  # default to the Artifact Registry repo created in this module.
  ar_image       = "${var.region}-docker.pkg.dev/${var.project_id}/nclav/nclav:latest"
  nclav_image    = var.nclav_image != "" ? var.nclav_image : local.ar_image

  # Detect whether gcp_parent is a folder or an organization so we can emit
  # the correct gcloud command family in the iam_setup_commands output.
  _parent_is_folder   = startswith(var.gcp_parent, "folders/")
  _gcloud_parent_bind = local._parent_is_folder ? "gcloud resource-manager folders add-iam-policy-binding ${trimprefix(var.gcp_parent, "folders/")}" : "gcloud organizations add-iam-policy-binding ${trimprefix(var.gcp_parent, "organizations/")}"
}

resource "google_secret_manager_secret" "db_url" {
  secret_id = "nclav-db-url"

  replication {
    auto {}
  }

  depends_on = [google_project_service.apis]
}

resource "google_secret_manager_secret_version" "db_url" {
  secret      = google_secret_manager_secret.db_url.id
  secret_data = local.db_url
}

resource "google_secret_manager_secret_iam_member" "nclav_db_url_access" {
  secret_id = google_secret_manager_secret.db_url.id
  role      = "roles/secretmanager.secretAccessor"
  member    = "serviceAccount:${google_service_account.nclav.email}"
}

# ── Cloud Run service ─────────────────────────────────────────────────────────

resource "google_cloud_run_v2_service" "nclav" {
  name     = "nclav-api"
  location = var.region

  template {
    service_account = google_service_account.nclav.email

    # Mount the Cloud SQL socket so nclav can connect via Unix socket.
    volumes {
      name = "cloudsql"
      cloud_sql_instance {
        instances = [google_sql_database_instance.nclav.connection_name]
      }
    }

    containers {
      image = local.nclav_image

      args = concat(
        [
          "serve",
          "--bind", "0.0.0.0",
          "--cloud", "gcp",
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

      env {
        name = "NCLAV_POSTGRES_URL"
        value_source {
          secret_key_ref {
            secret  = google_secret_manager_secret.db_url.secret_id
            version = "latest"
          }
        }
      }

      volume_mounts {
        name       = "cloudsql"
        mount_path = "/cloudsql"
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
    google_sql_database_instance.nclav,
    google_sql_database.nclav,
    google_sql_user.nclav,
    google_secret_manager_secret_iam_member.nclav_token_access,
    google_secret_manager_secret_iam_member.nclav_db_url_access,
  ]
}

# Grant Cloud Run invoker to each member in var.allowed_invokers.
#
# The default "allUsers" makes the API publicly reachable (application-level
# auth is handled by nclav's own Bearer token middleware).  If your GCP org
# enforces iam.allowedPolicyMemberTypes (Domain Restricted Sharing), allUsers
# will be rejected — set allowed_invokers to your own account instead:
#
#   allowed_invokers = ["user:you@example.com"]
#
# You can still use `gcloud run services proxy` as a zero-config alternative.
resource "google_cloud_run_v2_service_iam_member" "invokers" {
  for_each = toset(var.allowed_invokers)
  project  = var.project_id
  location = google_cloud_run_v2_service.nclav.location
  name     = google_cloud_run_v2_service.nclav.name
  role     = "roles/run.invoker"
  member   = each.value
}
