# bootstrap/azure/variables.tf

# ── Platform subscription ──────────────────────────────────────────────────────

variable "platform_subscription_id" {
  description = "Azure subscription ID in which to deploy the nclav platform (not an enclave subscription — a dedicated ops subscription). Find it with: az account show --query id -o tsv"
  type        = string
}

variable "resource_group_name" {
  description = "Name of the Azure resource group to create for the nclav platform."
  type        = string
  default     = "nclav-platform"
}

variable "location" {
  description = "Azure region for all platform resources. Must match --azure-default-location when running nclav serve."
  type        = string
  default     = "eastus2"
}

# ── Registry and Key Vault (globally unique names) ─────────────────────────────

variable "acr_name" {
  description = "Globally unique name for the Azure Container Registry (alphanumeric only, 5-50 chars, no hyphens). Example: 'myorgnclav'. Used as the push target: {acr_name}.azurecr.io/nclav."
  type        = string
}

variable "key_vault_name" {
  description = "Globally unique name for the Azure Key Vault (3-24 chars, alphanumeric + hyphens). Example: 'myorg-nclav-kv'. Stores the nclav API token for CLI access."
  type        = string
}

# ── nclav Azure driver configuration ──────────────────────────────────────────

variable "azure_tenant_id" {
  description = "Azure tenant ID (GUID). Passed to nclav serve as --azure-tenant-id."
  type        = string
}

variable "azure_management_group_id" {
  description = "Management group ID where subscription enclaves will be placed. Passed to nclav serve as --azure-management-group-id."
  type        = string
}

variable "azure_billing_account_name" {
  description = "MCA billing account name (long GUID form). Passed to nclav serve as --azure-billing-account-name."
  type        = string
}

variable "azure_billing_profile_name" {
  description = "MCA billing profile name. Passed to nclav serve as --azure-billing-profile-name."
  type        = string
}

variable "azure_invoice_section_name" {
  description = "MCA invoice section name. Passed to nclav serve as --azure-invoice-section-name."
  type        = string
}

variable "azure_subscription_prefix" {
  description = "Optional prefix prepended to every subscription alias. Avoids alias collisions: 'myorg' + 'product-a-dev' → 'myorg-product-a-dev'. Passed to nclav serve as --azure-subscription-prefix."
  type        = string
  default     = ""
}

# ── Infrastructure sizing ──────────────────────────────────────────────────────

variable "postgresql_sku" {
  description = "SKU for the PostgreSQL Flexible Server. B_Standard_B1ms is the cheapest burstable option."
  type        = string
  default     = "B_Standard_B1ms"
}

variable "nclav_image" {
  description = "Container image for the nclav API server. Defaults to the ACR repo created by this module ({acr_name}.azurecr.io/nclav:latest). Override to use a pre-built image."
  type        = string
  default     = ""
}
