output "image_push_command" {
  description = "Commands to build and push the nclav image to the Artifact Registry repo created by this module."
  value       = <<-EOT
    # Authenticate Docker to Artifact Registry (once per session):
    gcloud auth configure-docker ${var.region}-docker.pkg.dev --quiet

    # Build and push from the repo root:
    make push-ar GCP_PROJECT=${var.project_id} AR_REGION=${var.region}

    # Or manually:
    docker build --platform linux/amd64 -t ${local.ar_image} .
    docker push ${local.ar_image}
  EOT
}

output "proxy_command" {
  description = "Run this in a background terminal to reach the private Cloud Run service locally."
  value       = "gcloud run services proxy nclav-api --project=${var.project_id} --region=${var.region} --port=8080"
}

output "api_url" {
  description = "The Cloud Run service URL for the nclav API."
  value       = google_cloud_run_v2_service.nclav.uri
}

output "token_fetch_command" {
  description = "Shell command to retrieve the nclav API token from Secret Manager."
  value       = "gcloud secrets versions access latest --secret=nclav-api-token --project=${var.project_id}"
}

output "service_account" {
  description = "Service account email used by the nclav Cloud Run service."
  value       = google_service_account.nclav.email
}

output "db_instance" {
  description = "Cloud SQL instance connection name (project:region:instance)."
  value       = google_sql_database_instance.nclav.connection_name
}

output "iam_setup_commands" {
  description = "Send these gcloud commands to your GCP folder/org admin. They grant nclav-server the roles it needs to create and manage enclave projects. Run once after terraform apply."
  value       = <<-EOT

    # ── Send to your GCP admin — run once after terraform apply ──────────────
    # These bind nclav-server to folder/org-level roles so it can create GCP
    # projects for each enclave and manage resources within them.

    SA="${google_service_account.nclav.email}"

    # Folder / org level:
    ${local._gcloud_parent_bind} --member="serviceAccount:${google_service_account.nclav.email}" --role="roles/resourcemanager.projectCreator"
    ${local._gcloud_parent_bind} --member="serviceAccount:${google_service_account.nclav.email}" --role="roles/iam.serviceAccountAdmin"
    ${local._gcloud_parent_bind} --member="serviceAccount:${google_service_account.nclav.email}" --role="roles/iam.serviceAccountTokenCreator"
    ${local._gcloud_parent_bind} --member="serviceAccount:${google_service_account.nclav.email}" --role="roles/compute.networkAdmin"
    ${local._gcloud_parent_bind} --member="serviceAccount:${google_service_account.nclav.email}" --role="roles/dns.admin"
    ${local._gcloud_parent_bind} --member="serviceAccount:${google_service_account.nclav.email}" --role="roles/run.admin"
    ${local._gcloud_parent_bind} --member="serviceAccount:${google_service_account.nclav.email}" --role="roles/pubsub.admin"
    ${local._gcloud_parent_bind} --member="serviceAccount:${google_service_account.nclav.email}" --role="roles/cloudasset.viewer"

    # Billing account (attach billing to new enclave projects):
    gcloud billing accounts add-iam-policy-binding ${var.billing_account} --member="serviceAccount:${google_service_account.nclav.email}" --role="roles/billing.user"

  EOT
}
