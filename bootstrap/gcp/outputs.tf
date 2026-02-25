output "api_url" {
  description = "The Cloud Run service URL for the nclav API."
  value       = google_cloud_run_v2_service.nclav.uri
}

output "token_fetch_command" {
  description = "Shell command to retrieve the nclav API token from Secret Manager."
  value       = "gcloud secrets versions access latest --secret=nclav-api-token --project=${var.project_id}"
}

output "state_bucket" {
  description = "GCS bucket storing the nclav redb state file."
  value       = google_storage_bucket.state.name
}

output "service_account" {
  description = "Service account email used by the nclav Cloud Run service."
  value       = google_service_account.nclav.email
}
