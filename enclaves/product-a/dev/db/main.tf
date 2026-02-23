# Cloud SQL (PostgreSQL 15) for the product-a-dev db partition.
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

# ── VPC ───────────────────────────────────────────────────────────────────────

# Reference the VPC that the nclav GCP driver created for this enclave.
data "google_compute_network" "nclav" {
  name    = "nclav-vpc"
  project = var.project_id
}

# ── Private services access ───────────────────────────────────────────────────
# Cloud SQL uses a VPC peering connection into Google's managed network.
# servicenetworking.googleapis.com is already enabled by the nclav GCP driver.

resource "google_compute_global_address" "sql_private_range" {
  name          = "cloudsql-private-range"
  purpose       = "VPC_PEERING"
  address_type  = "INTERNAL"
  prefix_length = 20
  network       = data.google_compute_network.nclav.id
}

resource "google_service_networking_connection" "private_vpc" {
  network                 = data.google_compute_network.nclav.id
  service                 = "servicenetworking.googleapis.com"
  reserved_peering_ranges = [google_compute_global_address.sql_private_range.name]
}

# ── Cloud SQL instance ────────────────────────────────────────────────────────

resource "google_sql_database_instance" "db" {
  name             = "db"
  database_version = "POSTGRES_15"
  region           = var.region

  settings {
    tier              = "db-f1-micro"
    availability_type = "ZONAL"

    ip_configuration {
      ipv4_enabled    = false
      private_network = data.google_compute_network.nclav.id
    }

    backup_configuration {
      enabled = false
    }
  }

  deletion_protection = false
  depends_on          = [google_service_networking_connection.private_vpc]
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
