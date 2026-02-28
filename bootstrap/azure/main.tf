# bootstrap/azure/main.tf — Deploy nclav to Azure Container Apps
#
# This module provisions the nclav API server on Azure as a Container App. It
# is a one-time setup; your enclave workloads are managed by nclav afterwards.
#
# What it creates:
#   - Resource group: {resource_group_name}
#   - User-assigned managed identity: nclav-server (identity for the Container App)
#   - Container Registry: {acr_name} (hosts the nclav container image)
#   - Key Vault: {key_vault_name} (stores the API token for CLI retrieval)
#   - PostgreSQL Flexible Server: nclav-state-{suffix} (persistent state store)
#   - Container App Environment: nclav-env
#   - Container App: nclav-api (the nclav API server)
#
# Prerequisites: see README.md.

terraform {
  required_providers {
    azurerm = {
      source  = "hashicorp/azurerm"
      version = "~> 4.0"
    }
    random = {
      source  = "hashicorp/random"
      version = "~> 3.0"
    }
  }
}

provider "azurerm" {
  subscription_id = var.platform_subscription_id
  features {}
}

# ── Resource group ─────────────────────────────────────────────────────────────

resource "azurerm_resource_group" "nclav" {
  name     = var.resource_group_name
  location = var.location
}

# ── Managed identity for the nclav Container App ───────────────────────────────

resource "azurerm_user_assigned_identity" "nclav" {
  name                = "nclav-server"
  location            = azurerm_resource_group.nclav.location
  resource_group_name = azurerm_resource_group.nclav.name
}

# ── Container Registry ─────────────────────────────────────────────────────────

resource "azurerm_container_registry" "nclav" {
  name                = var.acr_name
  resource_group_name = azurerm_resource_group.nclav.name
  location            = azurerm_resource_group.nclav.location
  sku                 = "Basic"
  admin_enabled       = false
}

# Grant the managed identity AcrPull so the Container App can pull images.
resource "azurerm_role_assignment" "nclav_acr_pull" {
  scope                = azurerm_container_registry.nclav.id
  role_definition_name = "AcrPull"
  principal_id         = azurerm_user_assigned_identity.nclav.principal_id
}

# ── Key Vault (API token storage for CLI access) ───────────────────────────────

data "azurerm_client_config" "current" {}

resource "azurerm_key_vault" "nclav" {
  name                      = var.key_vault_name
  location                  = azurerm_resource_group.nclav.location
  resource_group_name       = azurerm_resource_group.nclav.name
  tenant_id                 = data.azurerm_client_config.current.tenant_id
  sku_name                  = "standard"
  enable_rbac_authorization = true
}

# Allow the Terraform caller to write secrets during provisioning.
resource "azurerm_role_assignment" "terraform_kv_officer" {
  scope                = azurerm_key_vault.nclav.id
  role_definition_name = "Key Vault Secrets Officer"
  principal_id         = data.azurerm_client_config.current.object_id
}

# ── API token ──────────────────────────────────────────────────────────────────

resource "random_bytes" "api_token" {
  length = 32
}

# Store the token in Key Vault so operators can retrieve it without reading
# Terraform state (used by `make connect-azure` and `token_fetch_command`).
resource "azurerm_key_vault_secret" "api_token" {
  name         = "nclav-api-token"
  value        = random_bytes.api_token.hex
  key_vault_id = azurerm_key_vault.nclav.id
  depends_on   = [azurerm_role_assignment.terraform_kv_officer]
}

# ── PostgreSQL Flexible Server ─────────────────────────────────────────────────

resource "random_id" "db_suffix" {
  byte_length = 4
}

resource "random_password" "db_password" {
  length  = 32
  special = false
}

resource "azurerm_postgresql_flexible_server" "nclav" {
  # Server name is globally unique within Azure (appears in DNS).
  name                   = "nclav-state-${random_id.db_suffix.hex}"
  resource_group_name    = azurerm_resource_group.nclav.name
  location               = azurerm_resource_group.nclav.location
  version                = "16"
  administrator_login    = "nclav"
  administrator_password = random_password.db_password.result
  sku_name               = var.postgresql_sku
  storage_mb             = 32768
  backup_retention_days  = 7

  # Public access — the firewall rule below restricts to Azure-hosted services.
  # For private-only access, configure delegated_subnet_id and private_dns_zone_id.
}

resource "azurerm_postgresql_flexible_server_database" "nclav" {
  name      = "nclav"
  server_id = azurerm_postgresql_flexible_server.nclav.id
  charset   = "UTF8"
  collation = "en_US.utf8"
}

# Allow all Azure-hosted services to connect (start/end 0.0.0.0 is a special
# Azure sentinel meaning "allow Azure internal traffic").
resource "azurerm_postgresql_flexible_server_firewall_rule" "azure_services" {
  name             = "AllowAzureServices"
  server_id        = azurerm_postgresql_flexible_server.nclav.id
  start_ip_address = "0.0.0.0"
  end_ip_address   = "0.0.0.0"
}

# ── Locals ─────────────────────────────────────────────────────────────────────

locals {
  db_url = "postgres://nclav:${random_password.db_password.result}@${azurerm_postgresql_flexible_server.nclav.fqdn}/nclav?sslmode=require"

  acr_image   = "${azurerm_container_registry.nclav.login_server}/nclav:latest"
  nclav_image = var.nclav_image != "" ? var.nclav_image : local.acr_image

  subscription_prefix_args = var.azure_subscription_prefix != "" ? [
    "--azure-subscription-prefix", var.azure_subscription_prefix,
  ] : []
}

# ── Container App Environment ──────────────────────────────────────────────────

resource "azurerm_container_app_environment" "nclav" {
  name                = "nclav-env"
  location            = azurerm_resource_group.nclav.location
  resource_group_name = azurerm_resource_group.nclav.name
}

# ── Container App (nclav API server) ──────────────────────────────────────────

resource "azurerm_container_app" "nclav" {
  name                         = "nclav-api"
  container_app_environment_id = azurerm_container_app_environment.nclav.id
  resource_group_name          = azurerm_resource_group.nclav.name
  revision_mode                = "Single"

  identity {
    type         = "UserAssigned"
    identity_ids = [azurerm_user_assigned_identity.nclav.id]
  }

  # Pull images from ACR using the managed identity (no admin credentials needed).
  registry {
    server   = azurerm_container_registry.nclav.login_server
    identity = azurerm_user_assigned_identity.nclav.id
  }

  # Secrets passed directly to the container. The API token is ALSO stored in
  # Key Vault above for human retrieval (make connect-azure / token_fetch_command).
  secret {
    name  = "api-token"
    value = random_bytes.api_token.hex
  }

  secret {
    name  = "db-url"
    value = local.db_url
  }

  template {
    min_replicas = 0
    max_replicas = 1

    container {
      name   = "nclav"
      image  = local.nclav_image
      cpu    = 0.5
      memory = "1Gi"

      # 'command' overrides the container ENTRYPOINT. Our Dockerfile sets
      # ENTRYPOINT ["nclav"], so this effectively calls: nclav serve ...
      command = concat(
        [
          "nclav", "serve",
          "--bind", "0.0.0.0",
          "--cloud", "azure",
          "--azure-tenant-id", var.azure_tenant_id,
          "--azure-management-group-id", var.azure_management_group_id,
          "--azure-billing-account-name", var.azure_billing_account_name,
          "--azure-billing-profile-name", var.azure_billing_profile_name,
          "--azure-invoice-section-name", var.azure_invoice_section_name,
          "--azure-default-location", var.location,
        ],
        local.subscription_prefix_args
      )

      env {
        name        = "NCLAV_TOKEN"
        secret_name = "api-token"
      }

      env {
        name        = "NCLAV_POSTGRES_URL"
        secret_name = "db-url"
      }
    }
  }

  ingress {
    external_enabled = true
    target_port      = 8080

    traffic_weight {
      percentage      = 100
      latest_revision = true
    }
  }

  depends_on = [
    azurerm_role_assignment.nclav_acr_pull,
    azurerm_postgresql_flexible_server_database.nclav,
    azurerm_postgresql_flexible_server_firewall_rule.azure_services,
    azurerm_key_vault_secret.api_token,
  ]
}
