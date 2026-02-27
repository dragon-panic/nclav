variable "project_id" {
  description = "GCP project ID in which to deploy nclav (the platform project, not an enclave project)."
  type        = string
}

variable "region" {
  description = "GCP region for Cloud Run, GCS bucket, and other regional resources."
  type        = string
  default     = "us-central1"
}

variable "nclav_image" {
  description = "Container image for the nclav API server, e.g. 'ghcr.io/your-org/nclav:latest'."
  type        = string
}

variable "gcp_parent" {
  description = "GCP resource parent for enclave project creation: 'folders/NUMERIC_ID' or 'organizations/NUMERIC_ID'."
  type        = string
}

variable "billing_account" {
  description = "GCP billing account to attach to every new enclave project: 'XXXXXX-YYYYYY-ZZZZZZ' (without the 'billingAccounts/' prefix)."
  type        = string
}

variable "gcp_project_prefix" {
  description = "Optional prefix prepended to every GCP project ID derived from an enclave ID. Scopes project IDs to your organisation."
  type        = string
  default     = ""
}

variable "cloud_sql_tier" {
  description = "Cloud SQL machine tier for the nclav state database."
  type        = string
  default     = "db-f1-micro"
}
