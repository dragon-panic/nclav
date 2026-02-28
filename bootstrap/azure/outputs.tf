output "acr_login_server" {
  description = "ACR login server — the registry hostname for pushing and pulling the nclav image."
  value       = azurerm_container_registry.nclav.login_server
}

output "key_vault_name" {
  description = "Key Vault name — used by 'make connect-azure' and 'token_fetch_command' to retrieve the API token."
  value       = azurerm_key_vault.nclav.name
}

output "app_fqdn" {
  description = "Public FQDN of the nclav Container App. Set NCLAV_URL=https://{fqdn}."
  value       = azurerm_container_app.nclav.ingress[0].fqdn
}

output "token_fetch_command" {
  description = "Shell command to retrieve the nclav API token from Key Vault."
  value       = "az keyvault secret show --vault-name ${azurerm_key_vault.nclav.name} --name nclav-api-token --query value -o tsv"
}

output "managed_identity_object_id" {
  description = "Object ID of the nclav-server managed identity. Used in iam_setup_commands."
  value       = azurerm_user_assigned_identity.nclav.principal_id
}

output "image_push_command" {
  description = "Commands to build and push the nclav image to the ACR repo created by this module."
  value       = <<-EOT
    # Authenticate Docker to ACR (once per session):
    az acr login --name ${azurerm_container_registry.nclav.name}

    # Build and push from the repo root:
    make push-acr AZURE_ACR=${azurerm_container_registry.nclav.name}

    # Or manually:
    docker build --platform linux/amd64 -t ${local.acr_image} .
    docker push ${local.acr_image}
  EOT
}

output "iam_setup_commands" {
  description = "Send these az commands to your Azure admin. They grant nclav-server the roles it needs to create and manage subscription enclaves. Run once after terraform apply."
  value       = <<-EOT

    # ── Send to your Azure admin — run once after terraform apply ─────────────
    # These grant nclav-server the roles it needs to create Azure Subscription
    # enclaves, move them into the management group, and link MCA billing.

    MI_PRINCIPAL="${azurerm_user_assigned_identity.nclav.principal_id}"
    TENANT_ID="${var.azure_tenant_id}"

    # Owner on the Management Group — create subscriptions, move them into the
    # MG, and grant RBAC inside new subscription enclaves:
    az role assignment create \
      --assignee "$MI_PRINCIPAL" \
      --role "Owner" \
      --scope "/providers/Microsoft.Management/managementGroups/${var.azure_management_group_id}"

    # Invoice Section Contributor — links MCA billing to new subscriptions.
    # Requires a billing-admin account. Run with: az login --allow-no-subscriptions
    az billing role-assignment create \
      --account-name "${var.azure_billing_account_name}" \
      --profile-name "${var.azure_billing_profile_name}" \
      --invoice-section-name "${var.azure_invoice_section_name}" \
      --billing-role-definition-name "Invoice Section Contributor" \
      --principal-id "$MI_PRINCIPAL" \
      --principal-tenant-id "$TENANT_ID"

  EOT
}
