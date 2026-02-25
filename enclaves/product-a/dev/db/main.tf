# Cloud SQL (PostgreSQL 15) for the product-a-dev db partition.
#
# PSC-enabled — no VPC peering / service networking required.
# Cross-enclave connectivity uses the native PSC service attachment.
#
# Variables are supplied by nclav via nclav_context.auto.tfvars.
# Declare only the ones you use; see inputs: in config.yml.

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

provider "google" {
  project = var.project_id
  region  = var.region
}

# ── Cloud SQL instance (Enterprise, PSC enabled) ──────────────────────────────

resource "google_sql_database_instance" "db" {
  name             = "db"
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

resource "google_sql_database" "app" {
  name     = "app"
  instance = google_sql_database_instance.db.name
}

# ── Outputs (must match declared_outputs in config.yml) ───────────────────────

output "hostname" {
  description = "Private IP address of the Cloud SQL instance."
  value       = google_sql_database_instance.db.private_ip_address
}

output "port" {
  description = "PostgreSQL port."
  value       = "5432"
}

output "psc_service_attachment" {
  description = "PSC service attachment URI for cross-enclave connectivity."
  value       = google_sql_database_instance.db.psc_service_attachment_link
}
