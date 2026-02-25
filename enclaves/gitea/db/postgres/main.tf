# Cloud SQL (PostgreSQL 15) for the gitea-db enclave.
#
# This partition provisions a PSC-enabled Cloud SQL instance.
# Cross-enclave connectivity uses Private Service Connect (PSC):
#
#   1. Cloud SQL exposes a native PSC service attachment URI.
#   2. nclav reads `psc_service_attachment` from this partition's outputs.
#   3. In the importing enclave (gitea-app), nclav creates a PSC endpoint
#      (forwarding rule) and registers a DNS A record in gitea-app's private
#      Cloud DNS zone that resolves to the endpoint IP.
#
# PSC Cloud SQL does NOT use service-networking VPC peering — no
# google_compute_global_address or google_service_networking_connection needed.
#
# Variables are supplied by nclav via nclav_context.auto.tfvars.
# Declare only the ones you use; see inputs: in config.yml.

terraform {
  required_providers {
    google = {
      source  = "hashicorp/google"
      version = "~> 5.0"
    }
    random = {
      source  = "hashicorp/random"
      version = "~> 3.6"
    }
  }
}

variable "project_id" {}
variable "region" {}

provider "google" {
  project = var.project_id
  region  = var.region
}

# ── Database credentials ──────────────────────────────────────────────────────

resource "random_password" "gitea_db" {
  length  = 32
  special = false
}

# ── Cloud SQL instance (Enterprise, PSC enabled) ──────────────────────────────
#
# PSC requires Enterprise edition. PostgreSQL uses db-custom-{vcpu}-{memory_mb};
# db-n1-standard-* are MySQL-only names. db-custom-2-7680 = 2 vCPU / 7.5 GB.

resource "google_sql_database_instance" "gitea" {
  name             = "gitea"
  database_version = "POSTGRES_15"
  region           = var.region

  settings {
    tier              = "db-custom-2-7680"
    availability_type = "ZONAL"

    ip_configuration {
      ipv4_enabled = false
      psc_config {
        psc_enabled = true
      }
    }

    backup_configuration {
      enabled = false
    }
  }

  deletion_protection = false
}

resource "google_sql_database" "gitea" {
  name     = "gitea"
  instance = google_sql_database_instance.gitea.name
}

resource "google_sql_user" "gitea" {
  name     = "gitea"
  instance = google_sql_database_instance.gitea.name
  password = random_password.gitea_db.result
}

# ── Outputs (must match declared_outputs in config.yml) ───────────────────────
# nclav reads psc_service_attachment to wire the PSC endpoint in the importer.
# hostname is kept as a reference; the importer overrides it with a DNS name.

output "hostname" {
  description = "Private IP of the Cloud SQL instance (reference only; importer uses PSC DNS name)."
  value       = google_sql_database_instance.gitea.private_ip_address
}

output "port" {
  description = "PostgreSQL port."
  value       = "5432"
}

output "db_name" {
  description = "Database name."
  value       = google_sql_database.gitea.name
}

output "db_user" {
  description = "Database user."
  value       = google_sql_user.gitea.name
}

output "db_password" {
  description = "Database password."
  value       = random_password.gitea_db.result
  sensitive   = true
}

output "psc_service_attachment" {
  description = "PSC service attachment URI for cross-enclave connectivity."
  value       = google_sql_database_instance.gitea.psc_service_attachment_link
}
