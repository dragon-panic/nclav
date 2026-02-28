use std::collections::HashMap;
use std::process::Command as StdCommand;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use nclav_domain::{Enclave, Export, ExportType, Import, Partition};
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::driver::{Driver, ObservedState, OrphanedResource, ProvisionResult};
use crate::error::DriverError;
use crate::Handle;

// ── Configuration ─────────────────────────────────────────────────────────────

/// Static configuration for the Azure driver, injected at startup.
/// Not stored in per-enclave YAML — these are operator-level settings.
#[derive(Clone)]
pub struct AzureDriverConfig {
    /// Azure tenant ID (GUID).
    pub tenant_id: String,
    /// Management group ID where new subscription enclaves will be placed.
    /// e.g. "myorg" → `/providers/Microsoft.Management/managementGroups/myorg`
    pub management_group_id: String,
    /// MCA billing account name (long GUID form).
    pub billing_account_name: String,
    /// MCA billing profile name.
    pub billing_profile_name: String,
    /// MCA invoice section name.
    pub invoice_section_name: String,
    /// Default Azure region for new resources. e.g. "eastus2"
    pub default_location: String,
    /// Optional prefix prepended to every subscription alias.
    pub subscription_prefix: Option<String>,
    /// Service principal client ID (optional; falls back to MSI/CLI).
    pub client_id: Option<String>,
    /// Service principal client secret (optional; falls back to MSI/CLI).
    pub client_secret: Option<String>,
}

// ── Base URLs (overridden in tests) ───────────────────────────────────────────

#[derive(Clone)]
pub(crate) struct BaseUrls {
    management: String,
    login:      String,
    graph:      String,
}

impl Default for BaseUrls {
    fn default() -> Self {
        Self {
            management: "https://management.azure.com".into(),
            login:      "https://login.microsoftonline.com".into(),
            graph:      "https://management.azure.com".into(),
        }
    }
}

// ── Token provider ────────────────────────────────────────────────────────────

/// Abstraction over Azure token acquisition — enables test injection.
#[async_trait]
trait TokenProvider: Send + Sync {
    async fn token(&self) -> Result<String, DriverError>;
}

// ── Service Principal ─────────────────────────────────────────────────────────

struct ServicePrincipalTokenProvider {
    tenant_id:     String,
    client_id:     String,
    client_secret: String,
    login_base:    String,
    client:        reqwest::Client,
    cache:         Mutex<Option<(String, Instant)>>,
}

#[async_trait]
impl TokenProvider for ServicePrincipalTokenProvider {
    async fn token(&self) -> Result<String, DriverError> {
        {
            let guard = self.cache.lock().await;
            if let Some((tok, expiry)) = guard.as_ref() {
                if Instant::now() < *expiry {
                    return Ok(tok.clone());
                }
            }
        }

        let url = format!("{}/{}/oauth2/v2.0/token", self.login_base, self.tenant_id);
        let params = [
            ("grant_type", "client_credentials"),
            ("client_id", &self.client_id),
            ("client_secret", &self.client_secret),
            ("scope", "https://management.azure.com/.default"),
        ];
        let resp: Value = self
            .client
            .post(&url)
            .form(&params)
            .send()
            .await
            .map_err(|e| DriverError::Internal(format!("SP token request: {}", e)))?
            .json()
            .await
            .map_err(|e| DriverError::Internal(format!("SP token decode: {}", e)))?;

        let tok = resp["access_token"]
            .as_str()
            .ok_or_else(|| DriverError::Internal(format!("SP token: no access_token in response: {}", resp)))?
            .to_string();
        let expires_in = resp["expires_in"].as_u64().unwrap_or(3600);
        let expiry = Instant::now() + Duration::from_secs(expires_in.saturating_sub(60));

        *self.cache.lock().await = Some((tok.clone(), expiry));
        Ok(tok)
    }
}

// ── Managed Identity (IMDS) ───────────────────────────────────────────────────

struct ManagedIdentityTokenProvider {
    client: reqwest::Client,
    cache:  Mutex<Option<(String, Instant)>>,
}

#[async_trait]
impl TokenProvider for ManagedIdentityTokenProvider {
    async fn token(&self) -> Result<String, DriverError> {
        {
            let guard = self.cache.lock().await;
            if let Some((tok, expiry)) = guard.as_ref() {
                if Instant::now() < *expiry {
                    return Ok(tok.clone());
                }
            }
        }

        let resp: Value = self
            .client
            .get("http://169.254.169.254/metadata/identity/oauth2/token")
            .header("Metadata", "true")
            .query(&[
                ("api-version", "2018-02-01"),
                ("resource", "https://management.azure.com/"),
            ])
            .send()
            .await
            .map_err(|e| DriverError::Internal(format!("IMDS token request: {}", e)))?
            .json()
            .await
            .map_err(|e| DriverError::Internal(format!("IMDS token decode: {}", e)))?;

        let tok = resp["access_token"]
            .as_str()
            .ok_or_else(|| DriverError::Internal(format!("IMDS token: no access_token: {}", resp)))?
            .to_string();
        let expires_in = resp["expires_in"]
            .as_str()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(3600);
        let expiry = Instant::now() + Duration::from_secs(expires_in.saturating_sub(60));

        *self.cache.lock().await = Some((tok.clone(), expiry));
        Ok(tok)
    }
}

// ── Azure CLI ─────────────────────────────────────────────────────────────────

struct AzureCliTokenProvider {
    tenant_id: String,
}

#[async_trait]
impl TokenProvider for AzureCliTokenProvider {
    async fn token(&self) -> Result<String, DriverError> {
        let output = StdCommand::new("az")
            .args([
                "account",
                "get-access-token",
                "--resource",
                "https://management.azure.com",
                "--tenant",
                &self.tenant_id,
                "--output",
                "json",
            ])
            .output()
            .map_err(|e| DriverError::Internal(format!("az CLI not found: {}. Install Azure CLI or configure service principal credentials.", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(DriverError::Internal(format!(
                "az account get-access-token failed: {}. Run 'az login' first.",
                stderr.trim()
            )));
        }

        let resp: Value = serde_json::from_slice(&output.stdout)
            .map_err(|e| DriverError::Internal(format!("az CLI output parse: {}", e)))?;
        let tok = resp["accessToken"]
            .as_str()
            .ok_or_else(|| DriverError::Internal("az CLI: no accessToken in output".into()))?
            .to_string();
        Ok(tok)
    }
}

// ── Static (tests) ────────────────────────────────────────────────────────────

pub struct StaticToken(pub String);

#[async_trait]
impl TokenProvider for StaticToken {
    async fn token(&self) -> Result<String, DriverError> {
        Ok(self.0.clone())
    }
}

// ── AzureDriver ───────────────────────────────────────────────────────────────

pub struct AzureDriver {
    config: AzureDriverConfig,
    client: reqwest::Client,
    token:  Box<dyn TokenProvider>,
    base:   BaseUrls,
}

impl AzureDriver {
    /// Create an `AzureDriver`, auto-selecting the token provider:
    /// 1. `client_id` + `client_secret` in config → Service Principal
    /// 2. `AZURE_CLIENT_ID` + `AZURE_CLIENT_SECRET` env vars → Service Principal
    /// 3. `IDENTITY_ENDPOINT` env var → Managed Identity (IMDS)
    /// 4. Otherwise → Azure CLI (`az account get-access-token`)
    pub fn new(config: AzureDriverConfig) -> Result<Self, DriverError> {
        let client = reqwest::Client::new();
        let base   = BaseUrls::default();

        let token: Box<dyn TokenProvider> = if let (Some(cid), Some(cs)) = (
            config.client_id.as_deref(),
            config.client_secret.as_deref(),
        ) {
            Box::new(ServicePrincipalTokenProvider {
                tenant_id:     config.tenant_id.clone(),
                client_id:     cid.to_string(),
                client_secret: cs.to_string(),
                login_base:    base.login.clone(),
                client:        client.clone(),
                cache:         Mutex::new(None),
            })
        } else if let (Ok(cid), Ok(cs)) = (
            std::env::var("AZURE_CLIENT_ID"),
            std::env::var("AZURE_CLIENT_SECRET"),
        ) {
            Box::new(ServicePrincipalTokenProvider {
                tenant_id:     config.tenant_id.clone(),
                client_id:     cid,
                client_secret: cs,
                login_base:    base.login.clone(),
                client:        client.clone(),
                cache:         Mutex::new(None),
            })
        } else if std::env::var("IDENTITY_ENDPOINT").is_ok() {
            Box::new(ManagedIdentityTokenProvider {
                client: client.clone(),
                cache:  Mutex::new(None),
            })
        } else {
            Box::new(AzureCliTokenProvider {
                tenant_id: config.tenant_id.clone(),
            })
        };

        Ok(Self { config, client, token, base })
    }

    /// Create an `AzureDriver` with a static bearer token and custom base URLs.
    /// Used exclusively in tests.
    #[cfg(test)]
    pub(crate) fn with_static_token(config: AzureDriverConfig, token: &str, base: BaseUrls) -> Self {
        Self {
            config,
            client: reqwest::Client::new(),
            token:  Box::new(StaticToken(token.to_string())),
            base,
        }
    }

    async fn bearer(&self) -> Result<String, DriverError> {
        self.token.token().await
    }

    fn location<'a>(&'a self, enclave: &'a Enclave) -> &'a str {
        &enclave.region
    }

    /// Derive the subscription alias for an enclave.
    ///
    /// Rules: 1–63 chars, alphanumeric + hyphens, starts with letter or digit.
    fn subscription_alias(&self, enclave_id: &str) -> String {
        let raw = match &self.config.subscription_prefix {
            Some(p) if !p.is_empty() => format!("{}-{}", p, enclave_id),
            _                        => enclave_id.to_string(),
        };
        sanitize_subscription_alias(&raw)
    }

    /// Build the MCA billing scope string for subscription creation.
    fn billing_scope(&self) -> String {
        format!(
            "/providers/Microsoft.Billing/billingAccounts/{}/billingProfiles/{}/invoiceSections/{}",
            self.config.billing_account_name,
            self.config.billing_profile_name,
            self.config.invoice_section_name,
        )
    }

    // ── ARM error parsing ─────────────────────────────────────────────────────

    fn parse_arm_error(body: &Value) -> String {
        let err = body
            .get("error")
            .or_else(|| body.get("Error"))
            .unwrap_or(body);
        let code    = err["code"].as_str().unwrap_or("Unknown");
        let message = err["message"].as_str().unwrap_or("unknown error");
        format!("{}: {}", code, message)
    }

    // ── ARM async polling ─────────────────────────────────────────────────────

    /// Poll an ARM async operation URL until it completes or times out.
    ///
    /// Azure 202 responses carry `Azure-AsyncOperation` or `Location` header.
    /// This method accepts either and polls until `status == "Succeeded"`.
    /// Backoff: `[1, 2, 4, 8, 16, 30]` cycling, max 120 polls.
    async fn wait_for_operation(&self, op_url: &str) -> Result<Value, DriverError> {
        let token  = self.bearer().await?;
        let delays = [1u64, 2, 4, 8, 16, 30];
        let max_polls = 120;

        for (i, &delay) in delays.iter().cycle().take(max_polls).enumerate() {
            let resp = self
                .client
                .get(op_url)
                .bearer_auth(&token)
                .send()
                .await
                .map_err(|e| DriverError::Internal(format!("poll {}: {}", op_url, e)))?;

            let body: Value = resp
                .json()
                .await
                .map_err(|e| DriverError::Internal(format!("poll decode {}: {}", op_url, e)))?;

            let status = body["status"].as_str().unwrap_or("Unknown");
            match status {
                "Succeeded" => return Ok(body),
                "Failed" | "Canceled" => {
                    let msg = Self::parse_arm_error(&body);
                    return Err(DriverError::ProvisionFailed(
                        format!("ARM operation failed ({}): {}", status, msg),
                    ));
                }
                _ => {}
            }

            let poll = i + 1;
            if poll % 10 == 0 {
                info!(poll, op_url, "still waiting for Azure ARM operation");
            } else {
                debug!(poll, op_url, delay, "Azure ARM operation pending, waiting");
            }
            tokio::time::sleep(Duration::from_secs(delay)).await;
        }

        Err(DriverError::ProvisionFailed(format!(
            "Azure ARM operation timed out after {} polls: {}",
            max_polls, op_url
        )))
    }

    // ── ARM HTTP verbs ────────────────────────────────────────────────────────

    async fn arm_put(&self, url: &str, body: &Value) -> Result<(u16, Value, Option<String>), DriverError> {
        let token = self.bearer().await?;
        debug!(url, "Azure ARM PUT");
        let resp = self
            .client
            .put(url)
            .bearer_auth(&token)
            .json(body)
            .send()
            .await
            .map_err(|e| DriverError::ProvisionFailed(format!("PUT {}: {}", url, e)))?;

        let status = resp.status().as_u16();
        let async_op = resp
            .headers()
            .get("Azure-AsyncOperation")
            .or_else(|| resp.headers().get("Location"))
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let body_val: Value = resp
            .json()
            .await
            .unwrap_or(Value::Null);
        Ok((status, body_val, async_op))
    }

    async fn arm_get(&self, url: &str) -> Result<(u16, Value), DriverError> {
        let token = self.bearer().await?;
        debug!(url, "Azure ARM GET");
        let resp = self
            .client
            .get(url)
            .bearer_auth(&token)
            .send()
            .await
            .map_err(|e| DriverError::Internal(format!("GET {}: {}", url, e)))?;

        let status = resp.status().as_u16();
        let body: Value = resp
            .json()
            .await
            .unwrap_or(Value::Null);
        Ok((status, body))
    }

    async fn arm_delete(&self, url: &str) -> Result<(), DriverError> {
        let token = self.bearer().await?;
        debug!(url, "Azure ARM DELETE");
        let resp = self
            .client
            .delete(url)
            .bearer_auth(&token)
            .send()
            .await
            .map_err(|e| DriverError::TeardownFailed(format!("DELETE {}: {}", url, e)))?;

        let status = resp.status().as_u16();
        if status == 404 || status == 204 || (200..300).contains(&status) {
            return Ok(());
        }

        // Handle async delete (202)
        if status == 202 {
            if let Some(op_url) = resp
                .headers()
                .get("Azure-AsyncOperation")
                .or_else(|| resp.headers().get("Location"))
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
            {
                self.wait_for_operation(&op_url).await?;
                return Ok(());
            }
            return Ok(());
        }

        let body: Value = resp.json().await.unwrap_or(Value::Null);
        Err(DriverError::TeardownFailed(format!(
            "DELETE {}: status {} — {}",
            url,
            status,
            Self::parse_arm_error(&body)
        )))
    }

    async fn arm_post(&self, url: &str, body: &Value) -> Result<Value, DriverError> {
        let token = self.bearer().await?;
        debug!(url, "Azure ARM POST");
        let resp = self
            .client
            .post(url)
            .bearer_auth(&token)
            .json(body)
            .send()
            .await
            .map_err(|e| DriverError::ProvisionFailed(format!("POST {}: {}", url, e)))?;

        let status = resp.status().as_u16();
        let body_val: Value = resp.json().await.unwrap_or(Value::Null);

        if !(200..300).contains(&status as &u16) && status != 202 {
            return Err(DriverError::ProvisionFailed(format!(
                "POST {}: status {} — {}",
                url,
                status,
                Self::parse_arm_error(&body_val)
            )));
        }
        Ok(body_val)
    }

    /// Wait for an async PUT to complete if it returned 202.
    async fn arm_put_and_wait(&self, url: &str, body: &Value) -> Result<Value, DriverError> {
        let (status, body_val, async_op) = self.arm_put(url, body).await?;

        // 200/201 means synchronously complete
        if status == 200 || status == 201 {
            if body_val.get("error").is_some() {
                return Err(DriverError::ProvisionFailed(format!(
                    "PUT {}: {}",
                    url,
                    Self::parse_arm_error(&body_val)
                )));
            }
            return Ok(body_val);
        }

        // 202 — poll the async operation URL
        if status == 202 {
            if let Some(op_url) = async_op {
                return self.wait_for_operation(&op_url).await;
            }
            // No async op URL — treat as success
            return Ok(body_val);
        }

        // 409 Conflict — caller handles idempotency
        if status == 409 {
            return Err(DriverError::ProvisionFailed(format!(
                "PUT {} conflict (409): {}",
                url,
                Self::parse_arm_error(&body_val)
            )));
        }

        Err(DriverError::ProvisionFailed(format!(
            "PUT {}: status {} — {}",
            url,
            status,
            Self::parse_arm_error(&body_val)
        )))
    }

    // ── Subscription alias provisioning ──────────────────────────────────────

    async fn create_subscription(&self, alias: &str, display_name: &str) -> Result<String, DriverError> {
        let url = format!(
            "{}/providers/Microsoft.Subscription/aliases/{}?api-version=2021-10-01",
            self.base.management, alias,
        );
        let billing_scope = self.billing_scope();
        let body = json!({
            "properties": {
                "displayName": display_name,
                "billingScope": billing_scope,
                "workload": "Production"
            }
        });

        let (status, body_val, async_op) = self.arm_put(&url, &body).await?;

        // Already exists — GET to retrieve existing subscription ID
        if status == 200 || status == 201 {
            if let Some(sub_id) = body_val["properties"]["subscriptionId"].as_str() {
                return Ok(sub_id.to_string());
            }
        }

        if status == 202 {
            let op_url = async_op.ok_or_else(|| {
                DriverError::ProvisionFailed("subscription alias PUT 202: no Azure-AsyncOperation header".into())
            })?;
            let result = self.wait_for_operation(&op_url).await?;
            // After polling, GET the alias to retrieve subscription ID
            let (_, alias_body) = self.arm_get(&url).await?;
            if let Some(sub_id) = alias_body["properties"]["subscriptionId"].as_str() {
                return Ok(sub_id.to_string());
            }
            // Also check the polling result itself
            if let Some(sub_id) = result["subscriptionId"].as_str()
                .or_else(|| result["properties"]["subscriptionId"].as_str())
            {
                return Ok(sub_id.to_string());
            }
            return Err(DriverError::ProvisionFailed(
                "subscription alias: no subscriptionId in operation result".into(),
            ));
        }

        // 409 = alias already exists; GET to retrieve subscription ID
        if status == 409 {
            info!(alias, "Subscription alias already exists, retrieving subscription ID");
            let (get_status, get_body) = self.arm_get(&url).await?;
            if get_status == 200 {
                if let Some(sub_id) = get_body["properties"]["subscriptionId"].as_str() {
                    return Ok(sub_id.to_string());
                }
            }
            return Err(DriverError::ProvisionFailed(format!(
                "subscription alias 409 and GET returned {}: {}",
                get_status,
                Self::parse_arm_error(&get_body)
            )));
        }

        Err(DriverError::ProvisionFailed(format!(
            "create subscription alias '{}': status {} — {}",
            alias,
            status,
            Self::parse_arm_error(&body_val)
        )))
    }

    /// Move a subscription into the configured management group.
    async fn move_to_management_group(&self, sub_id: &str) -> Result<(), DriverError> {
        let url = format!(
            "{}/providers/Microsoft.Management/managementGroups/{}/subscriptions/{}?api-version=2020-05-01",
            self.base.management,
            self.config.management_group_id,
            sub_id,
        );
        let (status, body_val, _) = self.arm_put(&url, &json!({})).await?;
        if (200..300).contains(&status) || status == 204 {
            return Ok(());
        }
        // 409 = already in MG
        if status == 409 {
            info!(sub_id, mg = %self.config.management_group_id, "Subscription already in management group");
            return Ok(());
        }
        Err(DriverError::ProvisionFailed(format!(
            "move subscription {} to MG {}: status {} — {}",
            sub_id,
            self.config.management_group_id,
            status,
            Self::parse_arm_error(&body_val)
        )))
    }

    /// Create the `nclav-rg` resource group in a subscription.
    async fn create_resource_group(
        &self,
        sub_id: &str,
        location: &str,
        enclave_id: &str,
    ) -> Result<(), DriverError> {
        let url = format!(
            "{}/subscriptions/{}/resourcegroups/nclav-rg?api-version=2021-04-01",
            self.base.management, sub_id,
        );
        let body = json!({
            "location": location,
            "tags": {
                "nclav-managed": "true",
                "nclav-enclave": enclave_id,
            }
        });
        let result = self.arm_put_and_wait(&url, &body).await;
        match result {
            Ok(_) => Ok(()),
            Err(e) if e.to_string().to_lowercase().contains("conflict") => {
                info!(sub_id, "Resource group nclav-rg already exists");
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Create a user-assigned managed identity in `nclav-rg`.
    async fn create_managed_identity(
        &self,
        sub_id: &str,
        name: &str,
        location: &str,
        enclave_id: &str,
        partition_id: Option<&str>,
    ) -> Result<(String, String, String), DriverError> {
        let url = format!(
            "{}/subscriptions/{}/resourceGroups/nclav-rg/providers/Microsoft.ManagedIdentity/userAssignedIdentities/{}?api-version=2023-01-31",
            self.base.management, sub_id, name,
        );
        let mut tags = json!({
            "nclav-managed": "true",
            "nclav-enclave": enclave_id,
        });
        if let Some(part_id) = partition_id {
            tags["nclav-partition"] = json!(part_id);
        }
        let body = json!({
            "location": location,
            "tags": tags,
        });
        let (status, body_val, _) = self.arm_put(&url, &body).await?;
        if !(200..300).contains(&status) {
            return Err(DriverError::ProvisionFailed(format!(
                "create managed identity '{}': status {} — {}",
                name,
                status,
                Self::parse_arm_error(&body_val)
            )));
        }
        let resource_id    = body_val["id"].as_str().unwrap_or("").to_string();
        let principal_id   = body_val["properties"]["principalId"].as_str().unwrap_or("").to_string();
        let client_id      = body_val["properties"]["clientId"].as_str().unwrap_or("").to_string();
        Ok((resource_id, principal_id, client_id))
    }

    /// Grant a role to a principal (by principal_id) on a scope.
    /// Idempotent — 409 with RoleAssignmentExists → success.
    async fn assign_role(
        &self,
        scope: &str,
        role_definition_id: &str,
        principal_id: &str,
    ) -> Result<(), DriverError> {
        let assignment_id = Uuid::new_v4();
        let url = format!(
            "{}{}/providers/Microsoft.Authorization/roleAssignments/{}?api-version=2022-04-01",
            self.base.management, scope, assignment_id,
        );
        let body = json!({
            "properties": {
                "roleDefinitionId": role_definition_id,
                "principalId": principal_id,
                "principalType": "ServicePrincipal",
            }
        });
        let (status, body_val, _) = self.arm_put(&url, &body).await?;
        if (200..300).contains(&status) {
            return Ok(());
        }
        // 409 = RoleAssignmentExists
        if status == 409 {
            debug!(scope, role_definition_id, principal_id, "RBAC role assignment already exists");
            return Ok(());
        }
        Err(DriverError::ProvisionFailed(format!(
            "assign role on {}: status {} — {}",
            scope,
            status,
            Self::parse_arm_error(&body_val)
        )))
    }
}

// ── Subscription alias sanitization ──────────────────────────────────────────

/// Sanitize a raw string into a valid Azure subscription alias.
///
/// Rules: 1–63 chars, letters/digits/hyphens/underscores/periods, starts with letter or digit.
fn sanitize_subscription_alias(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len().min(63));
    for c in raw.chars() {
        if out.len() == 63 {
            break;
        }
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
            out.push(c);
        } else {
            if !out.is_empty() && !out.ends_with('-') {
                out.push('-');
            }
        }
    }
    // Trim trailing non-alphanumeric
    while out.ends_with(|c: char| !c.is_ascii_alphanumeric()) {
        out.pop();
    }
    out
}

/// Derive a partition managed identity name.
///
/// Azure MI name limit: 128 chars. We keep it short for usability.
fn partition_mi_name(partition_id: &str) -> String {
    let candidate = format!("partition-{}", partition_id);
    if candidate.len() <= 64 {
        return candidate;
    }
    // "pt-" + up to 19 chars + "-" + 6-char hex hash
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    partition_id.hash(&mut hasher);
    let hash = format!("{:016x}", hasher.finish());
    let short_hash = &hash[..6];
    let truncated = &partition_id[..19.min(partition_id.len())];
    format!("pt-{}-{}", truncated, short_hash)
}

// ── Driver impl ───────────────────────────────────────────────────────────────

#[async_trait]
impl Driver for AzureDriver {
    fn name(&self) -> &'static str {
        "azure"
    }

    // ── provision_enclave ─────────────────────────────────────────────────────

    async fn provision_enclave(
        &self,
        enclave: &Enclave,
        existing: Option<&Handle>,
    ) -> Result<ProvisionResult, DriverError> {
        // Idempotency: if already complete, return stored handle
        if let Some(h) = existing {
            if h["provisioning_complete"].as_bool().unwrap_or(false) {
                info!(enclave_id = %enclave.id, "Azure enclave already provisioned, skipping");
                return Ok(ProvisionResult { handle: h.clone(), outputs: HashMap::new() });
            }
        }

        let enclave_id = enclave.id.as_str();
        let alias      = self.subscription_alias(enclave_id);
        let location   = self.location(enclave).to_string();

        info!(enclave_id, alias, "Provisioning Azure subscription enclave");

        // Step 1: Create subscription via MCA alias API
        let sub_id = self.create_subscription(&alias, &enclave.name).await?;
        info!(enclave_id, sub_id, "Subscription created/found");

        // Step 2: Move subscription to management group
        self.move_to_management_group(&sub_id).await?;
        info!(enclave_id, sub_id, mg = %self.config.management_group_id, "Subscription in management group");

        // Step 3: Create resource group
        self.create_resource_group(&sub_id, &location, enclave_id).await?;
        info!(enclave_id, sub_id, "Resource group nclav-rg created");

        // Step 4: Create enclave managed identity
        let mi_name = enclave.identity.as_deref().unwrap_or("nclav-identity");
        let (identity_resource_id, identity_principal_id, identity_client_id) =
            self.create_managed_identity(&sub_id, mi_name, &location, enclave_id, None).await?;
        info!(enclave_id, sub_id, mi = mi_name, "Enclave managed identity created");

        // Step 5: Create VNet if network config is present
        let mut vnet_resource_id = String::new();
        if let Some(network) = &enclave.network {
            let address_prefixes: Vec<&str> = network.subnets.iter().map(|s| s.as_str()).collect();
            let cidr = network.vpc_cidr.as_deref().unwrap_or("10.0.0.0/16");

            let subnets: Vec<Value> = network.subnets.iter().enumerate().map(|(i, prefix)| {
                json!({
                    "name": format!("subnet-{}", i),
                    "properties": { "addressPrefix": prefix }
                })
            }).collect();

            let vnet_url = format!(
                "{}/subscriptions/{}/resourceGroups/nclav-rg/providers/Microsoft.Network/virtualNetworks/nclav-vnet?api-version=2023-11-01",
                self.base.management, sub_id,
            );
            let vnet_body = json!({
                "location": location,
                "tags": { "nclav-managed": "true", "nclav-enclave": enclave_id },
                "properties": {
                    "addressSpace": {
                        "addressPrefixes": [cidr]
                    },
                    "subnets": subnets,
                }
            });
            let vnet_result = self.arm_put_and_wait(&vnet_url, &vnet_body).await
                .map_err(|e| DriverError::ProvisionFailed(format!("create VNet: {}", e)))?;
            vnet_resource_id = vnet_result["id"].as_str().unwrap_or("").to_string();
            if vnet_resource_id.is_empty() {
                // GET to retrieve
                let (_, vnet_get) = self.arm_get(&vnet_url).await?;
                vnet_resource_id = vnet_get["id"].as_str().unwrap_or("").to_string();
            }
            info!(enclave_id, sub_id, "VNet nclav-vnet created");
            let _ = address_prefixes; // silence unused warning
        }

        // Step 6: Create Private DNS zone if dns config is present
        let mut dns_zone_name = String::new();
        if let Some(dns) = &enclave.dns {
            if let Some(zone) = &dns.zone {
                dns_zone_name = zone.clone();

                // Create private DNS zone (location must be "global")
                let zone_url = format!(
                    "{}/subscriptions/{}/resourceGroups/nclav-rg/providers/Microsoft.Network/privateDnsZones/{}?api-version=2020-06-01",
                    self.base.management, sub_id, zone,
                );
                let zone_body = json!({
                    "location": "global",
                    "tags": { "nclav-managed": "true", "nclav-enclave": enclave_id },
                });
                self.arm_put_and_wait(&zone_url, &zone_body).await
                    .map_err(|e| DriverError::ProvisionFailed(format!("create DNS zone: {}", e)))?;
                info!(enclave_id, zone, "Private DNS zone created");

                // Create VNet link if we have a VNet
                if !vnet_resource_id.is_empty() {
                    let link_url = format!(
                        "{}/subscriptions/{}/resourceGroups/nclav-rg/providers/Microsoft.Network/privateDnsZones/{}/virtualNetworkLinks/nclav-link?api-version=2020-06-01",
                        self.base.management, sub_id, zone,
                    );
                    let link_body = json!({
                        "location": "global",
                        "properties": {
                            "virtualNetwork": { "id": vnet_resource_id },
                            "registrationEnabled": false,
                        }
                    });
                    self.arm_put_and_wait(&link_url, &link_body).await
                        .map_err(|e| DriverError::ProvisionFailed(format!("create DNS VNet link: {}", e)))?;
                    info!(enclave_id, zone, "Private DNS zone VNet link created");
                }
            }
        }

        // Step 7: Stamp provisioning_complete
        let handle = json!({
            "driver":                    "azure",
            "kind":                      "enclave",
            "subscription_id":           sub_id,
            "subscription_alias":        alias,
            "resource_group":            "nclav-rg",
            "location":                  location,
            "identity_resource_id":      identity_resource_id,
            "identity_principal_id":     identity_principal_id,
            "identity_client_id":        identity_client_id,
            "vnet_resource_id":          vnet_resource_id,
            "dns_zone_name":             dns_zone_name,
            "provisioning_complete":     true,
        });

        info!(enclave_id, sub_id, "Azure enclave provisioning complete");
        Ok(ProvisionResult { handle, outputs: HashMap::new() })
    }

    // ── teardown_enclave ──────────────────────────────────────────────────────

    async fn teardown_enclave(
        &self,
        enclave: &Enclave,
        handle: &Handle,
    ) -> Result<(), DriverError> {
        let sub_id = handle["subscription_id"].as_str().unwrap_or("");
        if sub_id.is_empty() {
            warn!(enclave_id = %enclave.id, "teardown_enclave: no subscription_id in handle, nothing to cancel");
            return Ok(());
        }

        info!(enclave_id = %enclave.id, sub_id, "Cancelling Azure subscription (90-day hold applies)");
        let url = format!(
            "{}/subscriptions/{}/providers/Microsoft.Subscription/cancel?api-version=2021-10-01",
            self.base.management, sub_id,
        );
        let result = self.arm_post(&url, &json!({})).await;
        match result {
            Ok(_) => {
                warn!(
                    sub_id,
                    "Azure subscription cancelled. Resources persist for ~90 days (Azure soft-delete policy). \
                     To permanently delete, cancel via Azure portal after the hold period."
                );
                Ok(())
            }
            Err(e) if e.to_string().to_lowercase().contains("subscriptionnotfound")
                || e.to_string().to_lowercase().contains("not found") =>
            {
                warn!(sub_id, "Subscription not found during teardown, treating as already gone");
                Ok(())
            }
            Err(e) => Err(DriverError::TeardownFailed(format!(
                "cancel subscription {}: {}",
                sub_id, e
            ))),
        }
    }

    // ── provision_partition ───────────────────────────────────────────────────

    async fn provision_partition(
        &self,
        enclave: &Enclave,
        partition: &Partition,
        resolved_inputs: &HashMap<String, String>,
        existing: Option<&Handle>,
    ) -> Result<ProvisionResult, DriverError> {
        // Re-use existing partition handle if already provisioned (idempotency)
        if let Some(h) = existing {
            if h["kind"].as_str() == Some("partition") && h["driver"].as_str() == Some("azure") {
                info!(
                    enclave_id = %enclave.id, partition_id = %partition.id,
                    "Azure partition already provisioned, skipping"
                );
                return Ok(ProvisionResult { handle: h.clone(), outputs: HashMap::new() });
            }
        }

        // Subscription ID comes from context_vars injected by the reconciler into resolved_inputs.
        // Falls back to the existing partition handle's subscription_id, then the enclave identity field.
        let sub_id = resolved_inputs
            .get("nclav_subscription_id")
            .map(|s| s.as_str())
            .or_else(|| existing.and_then(|h| h["subscription_id"].as_str()))
            .or_else(|| enclave.identity.as_deref())
            .unwrap_or("")
            .to_string();

        if sub_id.is_empty() {
            return Err(DriverError::ProvisionFailed(format!(
                "provision_partition for enclave '{}': cannot determine Azure subscription ID. \
                 Ensure provision_enclave has run first (subscription_id is injected via context_vars → nclav_subscription_id).",
                enclave.id,
            )));
        }

        let location   = resolved_inputs
            .get("nclav_location")
            .map(|s| s.as_str())
            .unwrap_or_else(|| self.location(enclave))
            .to_string();
        let enclave_id = enclave.id.as_str();
        let part_id    = partition.id.as_str();
        let mi_name    = partition_mi_name(part_id);

        info!(enclave_id, part_id, mi_name, "Provisioning Azure partition managed identity");

        // Step 1: Create partition managed identity
        let (identity_resource_id, identity_principal_id, identity_client_id) =
            self.create_managed_identity(&sub_id, &mi_name, &location, enclave_id, Some(part_id)).await?;
        info!(enclave_id, part_id, mi_name, "Partition managed identity created");

        // Step 2: Grant Contributor on the subscription to partition MI
        // Contributor role definition ID (Azure built-in, same across all tenants)
        let contributor_role = format!(
            "/subscriptions/{}/providers/Microsoft.Authorization/roleDefinitions/b24988ac-6180-42a0-ab88-20f7382dd24c",
            sub_id,
        );
        let scope = format!("/subscriptions/{}", sub_id);
        match self.assign_role(&scope, &contributor_role, &identity_principal_id).await {
            Ok(()) => info!(enclave_id, part_id, "Contributor RBAC granted to partition MI"),
            Err(e) => warn!(
                enclave_id, part_id,
                "Could not grant Contributor RBAC to partition MI (non-fatal): {}. \
                 Grant manually if needed.", e
            ),
        }

        let handle = json!({
            "driver":                           "azure",
            "kind":                             "partition",
            "type":                             "iac",
            "subscription_id":                  sub_id,
            "resource_group":                   "nclav-rg",
            "partition_identity_resource_id":   identity_resource_id,
            "partition_identity_principal_id":  identity_principal_id,
            "partition_identity_client_id":     identity_client_id,
        });

        Ok(ProvisionResult { handle, outputs: HashMap::new() })
    }

    // ── teardown_partition ────────────────────────────────────────────────────

    async fn teardown_partition(
        &self,
        enclave: &Enclave,
        partition: &Partition,
        handle: &Handle,
    ) -> Result<(), DriverError> {
        let sub_id  = handle["subscription_id"].as_str().unwrap_or("");
        if sub_id.is_empty() {
            warn!(
                enclave_id = %enclave.id, partition_id = %partition.id,
                "teardown_partition: no subscription_id in handle, skipping"
            );
            return Ok(());
        }
        let part_id = partition.id.as_str();
        let mi_name = partition_mi_name(part_id);

        info!(
            enclave_id = %enclave.id, partition_id = part_id, mi_name,
            "Tearing down Azure partition managed identity"
        );

        let url = format!(
            "{}/subscriptions/{}/resourceGroups/nclav-rg/providers/Microsoft.ManagedIdentity/userAssignedIdentities/{}?api-version=2023-01-31",
            self.base.management, sub_id, mi_name,
        );
        match self.arm_delete(&url).await {
            Ok(()) => info!(enclave_id = %enclave.id, partition_id = part_id, mi_name, "Partition MI deleted"),
            Err(e) => warn!(
                enclave_id = %enclave.id, partition_id = part_id,
                "Partition MI deletion failed (non-fatal): {}", e
            ),
        }
        Ok(())
    }

    // ── provision_export ──────────────────────────────────────────────────────

    async fn provision_export(
        &self,
        enclave: &Enclave,
        export: &Export,
        partition_outputs: &HashMap<String, String>,
        existing: Option<&Handle>,
    ) -> Result<ProvisionResult, DriverError> {
        if let Some(h) = existing {
            if h["driver"].as_str() == Some("azure") && h["kind"].as_str() == Some("export") {
                return Ok(ProvisionResult {
                    handle: h.clone(),
                    outputs: export_outputs_from_handle(h),
                });
            }
        }

        let enclave_id = enclave.id.as_str();
        let export_name = &export.name;

        match &export.export_type {
            ExportType::Http => {
                let pls_resource_id = partition_outputs.get("pls_id").cloned().unwrap_or_default();
                let endpoint_url = partition_outputs.get("endpoint_url")
                    .ok_or_else(|| DriverError::ProvisionFailed(
                        format!("provision_export '{export_name}': missing Terraform output 'endpoint_url' — \
                                 your .tf must declare output \"endpoint_url\"")
                    ))?
                    .clone();
                let port: u16 = partition_outputs.get("port")
                    .and_then(|p| p.parse().ok())
                    .unwrap_or(443);

                let handle = json!({
                    "driver":           "azure",
                    "kind":             "export",
                    "type":             "http",
                    "subscription_id":  enclave.id.as_str(),
                    "resource_group":   "nclav-rg",
                    "export_name":      export_name,
                    "pls_resource_id":  pls_resource_id,
                    "endpoint_url":     endpoint_url,
                    "port":             port,
                });

                let mut outputs = HashMap::new();
                outputs.insert("hostname".into(), extract_url_hostname(&endpoint_url));
                outputs.insert("port".into(), port.to_string());

                info!(enclave_id, export_name, "Azure HTTP export provisioned");
                Ok(ProvisionResult { handle, outputs })
            }

            ExportType::Tcp => {
                let pls_resource_id = partition_outputs.get("pls_id")
                    .ok_or_else(|| DriverError::ProvisionFailed(
                        format!("provision_export '{export_name}': missing Terraform output 'pls_id' — \
                                 your .tf must declare output \"pls_id\"")
                    ))?
                    .clone();
                let port: u16 = partition_outputs.get("port")
                    .and_then(|p| p.parse().ok())
                    .unwrap_or(0);

                let handle = json!({
                    "driver":           "azure",
                    "kind":             "export",
                    "type":             "tcp",
                    "subscription_id":  enclave.id.as_str(),
                    "resource_group":   "nclav-rg",
                    "export_name":      export_name,
                    "pls_resource_id":  pls_resource_id,
                    "port":             port,
                });

                let mut outputs = HashMap::new();
                outputs.insert("pls_resource_id".into(), pls_resource_id);
                outputs.insert("port".into(), port.to_string());

                info!(enclave_id, export_name, "Azure TCP export provisioned");
                Ok(ProvisionResult { handle, outputs })
            }

            ExportType::Queue => {
                let ns_name = partition_outputs.get("service_bus_namespace_name")
                    .ok_or_else(|| DriverError::ProvisionFailed(
                        format!("provision_export '{export_name}': missing Terraform output \
                                 'service_bus_namespace_name'")
                    ))?
                    .clone();
                let topic_name = partition_outputs.get("topic_name")
                    .ok_or_else(|| DriverError::ProvisionFailed(
                        format!("provision_export '{export_name}': missing Terraform output 'topic_name'")
                    ))?
                    .clone();
                let sb_resource_id = partition_outputs.get("service_bus_resource_id")
                    .ok_or_else(|| DriverError::ProvisionFailed(
                        format!("provision_export '{export_name}': missing Terraform output \
                                 'service_bus_resource_id'")
                    ))?
                    .clone();

                let queue_url = format!("{}.servicebus.windows.net/{}", ns_name, topic_name);

                let handle = json!({
                    "driver":                       "azure",
                    "kind":                         "export",
                    "type":                         "queue",
                    "subscription_id":              enclave.id.as_str(),
                    "resource_group":               "nclav-rg",
                    "export_name":                  export_name,
                    "service_bus_namespace_name":   ns_name,
                    "topic_name":                   topic_name,
                    "service_bus_resource_id":      sb_resource_id,
                });

                let mut outputs = HashMap::new();
                outputs.insert("queue_url".into(), queue_url);

                info!(enclave_id, export_name, "Azure queue export provisioned");
                Ok(ProvisionResult { handle, outputs })
            }
        }
    }

    // ── provision_import ──────────────────────────────────────────────────────

    async fn provision_import(
        &self,
        importer: &Enclave,
        import: &Import,
        export_handle: &Handle,
        existing: Option<&Handle>,
    ) -> Result<ProvisionResult, DriverError> {
        if let Some(h) = existing {
            if h["driver"].as_str() == Some("azure") && h["kind"].as_str() == Some("import") {
                return Ok(ProvisionResult {
                    handle:  h.clone(),
                    outputs: import_outputs_from_handle(h),
                });
            }
        }

        let importer_id = importer.id.as_str();
        let alias       = &import.alias;
        let export_type = export_handle["type"].as_str().unwrap_or("http");

        // Retrieve importer subscription ID from:
        // 1. The existing import handle (re-provisioning path)
        // 2. The importer enclave's identity field (if set to subscription_id by convention)
        // Note: The Driver trait does not pass the importer's enclave handle to provision_import.
        // The subscription_id must be available through one of the above paths.
        let importer_sub_id: String = existing
            .and_then(|h| h["subscription_id"].as_str())
            .map(|s| s.to_string())
            .or_else(|| {
                // Fallback: if identity contains a subscription_id (UUID format or otherwise)
                importer.identity.as_deref().map(|s| s.to_string())
            })
            .unwrap_or_default();
        let location       = self.location(importer).to_string();

        match export_type {
            "http" | "tcp" => {
                let pls_resource_id = export_handle["pls_resource_id"]
                    .as_str()
                    .ok_or_else(|| DriverError::ProvisionFailed(format!(
                        "provision_import '{}': export handle missing 'pls_resource_id'. \
                         Ensure the exporter's Terraform declares output \"pls_id\".",
                        alias
                    )))?;
                let port: u16 = export_handle["port"].as_u64().unwrap_or(443) as u16;

                let pe_name = format!("{}-pe", alias);

                // We need the importer VNet and subnet IDs.
                // These come from the importer enclave's provisioned state.
                // In the reconciler, when provision_import is called the importer's handle
                // should be available. We retrieve VNet info from the importer enclave's handle.
                // Since we only have the importer Enclave struct here (not its handle),
                // we construct the expected VNet resource ID from what we know.
                let importer_sub = if importer_sub_id.is_empty() {
                    return Err(DriverError::ProvisionFailed(format!(
                        "provision_import '{}': cannot determine importer subscription ID. \
                         Ensure the importer enclave '{}' is provisioned first.",
                        alias, importer_id
                    )));
                } else {
                    importer_sub_id.as_str()
                };

                // Construct the subnet ID (first subnet in nclav-vnet)
                let subnet_id = format!(
                    "/subscriptions/{}/resourceGroups/nclav-rg/providers/Microsoft.Network/virtualNetworks/nclav-vnet/subnets/subnet-0",
                    importer_sub,
                );

                // Create Private Endpoint
                let pe_url = format!(
                    "{}/subscriptions/{}/resourceGroups/nclav-rg/providers/Microsoft.Network/privateEndpoints/{}?api-version=2023-11-01",
                    self.base.management, importer_sub, pe_name,
                );
                let pe_body = json!({
                    "location": location,
                    "tags": {
                        "nclav-managed": "true",
                        "nclav-enclave": importer_id,
                    },
                    "properties": {
                        "subnet": { "id": subnet_id },
                        "privateLinkServiceConnections": [{
                            "name": format!("{}-connection", alias),
                            "properties": {
                                "privateLinkServiceId": pls_resource_id,
                                "requestMessage": format!("nclav import {}", alias),
                            }
                        }]
                    }
                });
                let pe_result = self.arm_put_and_wait(&pe_url, &pe_body).await
                    .map_err(|e| DriverError::ProvisionFailed(format!("create private endpoint: {}", e)))?;

                // Extract NIC resource ID from PE result and get private IP
                let nic_resource_id = pe_result["properties"]["networkInterfaces"]
                    .as_array()
                    .and_then(|nics| nics.first())
                    .and_then(|nic| nic["id"].as_str())
                    .unwrap_or("");

                let private_ip = if !nic_resource_id.is_empty() {
                    let nic_url = format!(
                        "{}{}?api-version=2023-11-01",
                        self.base.management, nic_resource_id,
                    );
                    let (_, nic_body) = self.arm_get(&nic_url).await?;
                    nic_body["properties"]["ipConfigurations"]
                        .as_array()
                        .and_then(|cfgs| cfgs.first())
                        .and_then(|cfg| cfg["properties"]["privateIPAddress"].as_str())
                        .unwrap_or("10.0.0.0")
                        .to_string()
                } else {
                    "10.0.0.0".to_string()
                };

                // Create DNS A record if importer has a DNS zone
                if let Some(dns) = &importer.dns {
                    if let Some(zone) = &dns.zone {
                        let dns_url = format!(
                            "{}/subscriptions/{}/resourceGroups/nclav-rg/providers/Microsoft.Network/privateDnsZones/{}/A/{}?api-version=2020-06-01",
                            self.base.management, importer_sub, zone, alias,
                        );
                        let dns_body = json!({
                            "properties": {
                                "ttl": 300,
                                "aRecords": [{ "ipv4Address": private_ip }],
                            }
                        });
                        self.arm_put_and_wait(&dns_url, &dns_body).await
                            .map_err(|e| DriverError::ProvisionFailed(format!("create DNS A record: {}", e)))?;

                        let hostname = format!("{}.{}", alias, zone);
                        let pe_resource_id = pe_result["id"].as_str().unwrap_or("").to_string();

                        let handle = json!({
                            "driver":                   "azure",
                            "kind":                     "import",
                            "type":                     export_type,
                            "subscription_id":          importer_sub,
                            "resource_group":           "nclav-rg",
                            "private_endpoint_resource_id": pe_resource_id,
                            "private_ip":               private_ip,
                            "dns_record_name":          alias,
                        });
                        let mut outputs = HashMap::new();
                        outputs.insert("hostname".into(), hostname);
                        outputs.insert("port".into(), port.to_string());
                        return Ok(ProvisionResult { handle, outputs });
                    }
                }

                // No DNS zone — return IP directly
                let pe_resource_id = pe_result["id"].as_str().unwrap_or("").to_string();
                let handle = json!({
                    "driver":                   "azure",
                    "kind":                     "import",
                    "type":                     export_type,
                    "subscription_id":          importer_sub,
                    "resource_group":           "nclav-rg",
                    "private_endpoint_resource_id": pe_resource_id,
                    "private_ip":               private_ip,
                    "dns_record_name":          "",
                });
                let mut outputs = HashMap::new();
                outputs.insert("hostname".into(), private_ip);
                outputs.insert("port".into(), port.to_string());
                Ok(ProvisionResult { handle, outputs })
            }

            "queue" => {
                let sb_resource_id = export_handle["service_bus_resource_id"]
                    .as_str()
                    .ok_or_else(|| DriverError::ProvisionFailed(format!(
                        "provision_import '{}': export handle missing 'service_bus_resource_id'",
                        alias
                    )))?;
                let ns_name    = export_handle["service_bus_namespace_name"].as_str().unwrap_or("");
                let topic_name = export_handle["topic_name"].as_str().unwrap_or("");

                let importer_sub = if importer_sub_id.is_empty() {
                    return Err(DriverError::ProvisionFailed(format!(
                        "provision_import '{}': cannot determine importer subscription ID",
                        alias
                    )));
                } else {
                    importer_sub_id.as_str()
                };

                // Get importer partition MI principal ID (best-effort from enclave identity)
                let importer_principal_id = ""; // Not easily accessible without the enclave handle

                // Grant Azure Service Bus Data Receiver to importer partition MI
                // Role: 4f6d3b9b-027b-4f4c-9142-0e5a2a2247e0
                let sb_receiver_role = format!(
                    "{}/providers/Microsoft.Authorization/roleDefinitions/4f6d3b9b-027b-4f4c-9142-0e5a2a2247e0",
                    sb_resource_id
                );
                if !importer_principal_id.is_empty() {
                    let sb_scope = sb_resource_id;
                    match self.assign_role(sb_scope, &sb_receiver_role, importer_principal_id).await {
                        Ok(()) => info!(importer_id, alias, "Service Bus Data Receiver RBAC granted"),
                        Err(e) => warn!(importer_id, alias, "Service Bus RBAC grant failed (non-fatal): {}", e),
                    }
                }

                // Create Private Endpoint to Service Bus namespace
                let pe_name   = format!("{}-sb-pe", alias);
                let subnet_id = format!(
                    "/subscriptions/{}/resourceGroups/nclav-rg/providers/Microsoft.Network/virtualNetworks/nclav-vnet/subnets/subnet-0",
                    importer_sub,
                );
                let pe_url = format!(
                    "{}/subscriptions/{}/resourceGroups/nclav-rg/providers/Microsoft.Network/privateEndpoints/{}?api-version=2023-11-01",
                    self.base.management, importer_sub, pe_name,
                );
                let pe_body = json!({
                    "location": location,
                    "tags": { "nclav-managed": "true", "nclav-enclave": importer_id },
                    "properties": {
                        "subnet": { "id": subnet_id },
                        "privateLinkServiceConnections": [{
                            "name": format!("{}-sb-connection", alias),
                            "properties": {
                                "privateLinkServiceId": sb_resource_id,
                                "groupIds": ["namespace"],
                                "requestMessage": format!("nclav import {}", alias),
                            }
                        }]
                    }
                });
                self.arm_put_and_wait(&pe_url, &pe_body).await
                    .map_err(|e| DriverError::ProvisionFailed(format!("create SB private endpoint: {}", e)))?;

                let queue_url = format!("{}.servicebus.windows.net/{}", ns_name, topic_name);
                let handle = json!({
                    "driver":           "azure",
                    "kind":             "import",
                    "type":             "queue",
                    "subscription_id":  importer_sub,
                    "resource_group":   "nclav-rg",
                    "alias":            alias,
                });
                let mut outputs = HashMap::new();
                outputs.insert("queue_url".into(), queue_url);
                Ok(ProvisionResult { handle, outputs })
            }

            _ => Err(DriverError::ProvisionFailed(format!(
                "provision_import '{}': unknown export type '{}'",
                alias, export_type
            ))),
        }
    }

    // ── observe_enclave ───────────────────────────────────────────────────────

    async fn observe_enclave(
        &self,
        enclave: &Enclave,
        handle: &Handle,
    ) -> Result<ObservedState, DriverError> {
        let sub_id = handle["subscription_id"].as_str().unwrap_or("");
        if sub_id.is_empty() {
            return Ok(ObservedState {
                exists:  false,
                healthy: false,
                outputs: HashMap::new(),
                raw:     handle.clone(),
            });
        }

        let url = format!(
            "{}/subscriptions/{}?api-version=2022-12-01",
            self.base.management, sub_id,
        );
        let (status, body) = self.arm_get(&url).await?;

        if status == 404 {
            return Ok(ObservedState {
                exists:  false,
                healthy: false,
                outputs: HashMap::new(),
                raw:     body,
            });
        }

        let state   = body["state"].as_str().unwrap_or("Unknown");
        let exists  = (200..300).contains(&status);
        let healthy = exists && state == "Enabled";

        // Check VNet presence in parallel if we expect one
        let vnet_resource_id = handle["vnet_resource_id"].as_str().unwrap_or("");
        let vnet_healthy = if !vnet_resource_id.is_empty() {
            let vnet_url = format!(
                "{}{}?api-version=2023-11-01",
                self.base.management, vnet_resource_id,
            );
            let (vnet_status, _) = self.arm_get(&vnet_url).await.unwrap_or((404, Value::Null));
            (200..300).contains(&vnet_status)
        } else {
            true // no VNet expected → healthy
        };

        // Check MI presence
        let mi_resource_id = handle["identity_resource_id"].as_str().unwrap_or("");
        let mi_healthy = if !mi_resource_id.is_empty() {
            let mi_url = format!(
                "{}{}?api-version=2023-01-31",
                self.base.management, mi_resource_id,
            );
            let (mi_status, _) = self.arm_get(&mi_url).await.unwrap_or((404, Value::Null));
            (200..300).contains(&mi_status)
        } else {
            true
        };

        let enclave_id = enclave.id.as_str();
        if !vnet_healthy { warn!(enclave_id, sub_id, "VNet nclav-vnet not found — drift detected"); }
        if !mi_healthy   { warn!(enclave_id, sub_id, "Enclave MI not found — drift detected"); }

        Ok(ObservedState {
            exists,
            healthy: healthy && vnet_healthy && mi_healthy,
            outputs: HashMap::new(),
            raw: body,
        })
    }

    // ── observe_partition ─────────────────────────────────────────────────────

    async fn observe_partition(
        &self,
        _enclave: &Enclave,
        _partition: &Partition,
        handle: &Handle,
    ) -> Result<ObservedState, DriverError> {
        let exists  = handle["kind"].as_str() == Some("partition")
            && handle["driver"].as_str() == Some("azure");
        Ok(ObservedState {
            exists,
            healthy: exists,
            outputs: HashMap::new(),
            raw:     handle.clone(),
        })
    }

    // ── context_vars ──────────────────────────────────────────────────────────

    fn context_vars(&self, enclave: &Enclave, handle: &Handle) -> HashMap<String, String> {
        let sub_id   = handle["subscription_id"].as_str().unwrap_or("").to_string();
        let location = handle["location"].as_str().unwrap_or(&self.config.default_location).to_string();
        let mi_client_id = handle["identity_client_id"].as_str().unwrap_or("").to_string();
        let region   = location.clone();

        let mut vars = HashMap::new();
        // GCP-compatible alias (used by shared module code)
        vars.insert("nclav_project_id".into(),         sub_id.clone());
        vars.insert("nclav_region".into(),             region);
        // Azure-specific
        vars.insert("nclav_subscription_id".into(),    sub_id);
        vars.insert("nclav_resource_group".into(),     "nclav-rg".into());
        vars.insert("nclav_location".into(),           location);
        vars.insert("nclav_identity_client_id".into(), mi_client_id);
        vars.insert("nclav_enclave".into(),            enclave.id.as_str().to_string());
        vars
    }

    // ── auth_env ──────────────────────────────────────────────────────────────

    fn auth_env(&self, _enclave: &Enclave, handle: &Handle) -> HashMap<String, String> {
        let sub_id = handle["subscription_id"].as_str().unwrap_or("").to_string();
        let mut env = HashMap::new();
        env.insert("ARM_TENANT_ID".into(), self.config.tenant_id.clone());
        env.insert("ARM_SUBSCRIPTION_ID".into(), sub_id);
        if let Some(cid) = &self.config.client_id {
            env.insert("ARM_CLIENT_ID".into(), cid.clone());
        }
        if let Some(cs) = &self.config.client_secret {
            env.insert("ARM_CLIENT_SECRET".into(), cs.clone());
        }
        // If no SP credentials, signal Terraform to use MSI/CLI auth
        if self.config.client_id.is_none() || self.config.client_secret.is_none() {
            if std::env::var("IDENTITY_ENDPOINT").is_ok() {
                env.insert("ARM_USE_MSI".into(), "true".into());
            }
        }
        env
    }

    // ── list_partition_resources ──────────────────────────────────────────────

    async fn list_partition_resources(
        &self,
        _enclave: &Enclave,
        enc_handle: &Handle,
        partition: &Partition,
    ) -> Result<Vec<String>, DriverError> {
        let sub_id   = enc_handle["subscription_id"].as_str().unwrap_or("");
        let part_id  = partition.id.as_str();
        if sub_id.is_empty() { return Ok(vec![]); }

        let url = format!(
            "{}/providers/Microsoft.ResourceGraph/resources?api-version=2021-03-01",
            self.base.graph,
        );
        let body = json!({
            "subscriptions": [sub_id],
            "query": format!(
                "Resources | where tags['nclav-managed'] == 'true' and tags['nclav-partition'] == '{}'",
                part_id
            ),
        });
        let (status, resp) = self.arm_get(&url).await.unwrap_or((500, Value::Null));
        if status != 200 {
            // Try POST instead (Resource Graph requires POST)
            return match self.arm_post(&url, &body).await {
                Ok(resp) => Ok(extract_resource_names(&resp)),
                Err(_)   => Ok(vec![]),
            };
        }
        let _ = resp;
        match self.arm_post(&url, &body).await {
            Ok(resp) => Ok(extract_resource_names(&resp)),
            Err(_)   => Ok(vec![]),
        }
    }

    // ── list_orphaned_resources ───────────────────────────────────────────────

    async fn list_orphaned_resources(
        &self,
        _enclave: &Enclave,
        enc_handle: &Handle,
        known_partition_ids: &[&str],
    ) -> Result<Vec<OrphanedResource>, DriverError> {
        let sub_id = enc_handle["subscription_id"].as_str().unwrap_or("");
        if sub_id.is_empty() { return Ok(vec![]); }

        let url = format!(
            "{}/providers/Microsoft.ResourceGraph/resources?api-version=2021-03-01",
            self.base.graph,
        );
        let body = json!({
            "subscriptions": [sub_id],
            "query": "Resources | where tags['nclav-managed'] == 'true' and isnotempty(tags['nclav-partition'])",
        });

        let resp = match self.arm_post(&url, &body).await {
            Ok(r)  => r,
            Err(_) => return Ok(vec![]),
        };

        let mut orphans = Vec::new();
        if let Some(rows) = resp["data"]["rows"].as_array() {
            let cols = resp["data"]["columns"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let col_idx = |name: &str| {
                cols.iter().position(|c| c["name"].as_str() == Some(name))
            };
            let idx_id         = col_idx("id");
            let idx_type       = col_idx("type");
            let idx_name       = col_idx("name");
            let idx_tags       = col_idx("tags");

            for row in rows {
                let tags = row.as_array()
                    .and_then(|r| idx_tags.and_then(|i| r.get(i)))
                    .cloned()
                    .unwrap_or(Value::Null);
                let part_label = tags["nclav-partition"].as_str().unwrap_or("");
                let enc_label  = tags["nclav-enclave"].as_str().unwrap_or("");
                if part_label.is_empty() { continue; }
                if known_partition_ids.contains(&part_label) { continue; }

                let resource_name = row.as_array()
                    .and_then(|r| idx_id.and_then(|i| r.get(i)).or_else(|| idx_name.and_then(|i| r.get(i))))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let resource_type = row.as_array()
                    .and_then(|r| idx_type.and_then(|i| r.get(i)))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

                orphans.push(OrphanedResource {
                    resource_name,
                    resource_type,
                    nclav_partition: part_label.to_string(),
                    nclav_enclave:   enc_label.to_string(),
                });
            }
        }
        Ok(orphans)
    }
}

// ── Helper functions ──────────────────────────────────────────────────────────

fn export_outputs_from_handle(h: &Handle) -> HashMap<String, String> {
    let mut outputs = HashMap::new();
    match h["type"].as_str() {
        Some("http") => {
            if let Some(url) = h["endpoint_url"].as_str() {
                outputs.insert("hostname".into(), extract_url_hostname(url));
            }
            if let Some(port) = h["port"].as_u64() {
                outputs.insert("port".into(), port.to_string());
            }
        }
        Some("tcp") => {
            if let Some(id) = h["pls_resource_id"].as_str() {
                outputs.insert("pls_resource_id".into(), id.to_string());
            }
            if let Some(port) = h["port"].as_u64() {
                outputs.insert("port".into(), port.to_string());
            }
        }
        Some("queue") => {
            if let (Some(ns), Some(topic)) = (
                h["service_bus_namespace_name"].as_str(),
                h["topic_name"].as_str(),
            ) {
                outputs.insert("queue_url".into(), format!("{}.servicebus.windows.net/{}", ns, topic));
            }
        }
        _ => {}
    }
    outputs
}

fn import_outputs_from_handle(h: &Handle) -> HashMap<String, String> {
    let mut outputs = HashMap::new();
    match h["type"].as_str() {
        Some("http") | Some("tcp") => {
            // Re-derive hostname from dns_record_name + enclave dns zone (not stored)
            // Return stored private_ip as fallback
            if let Some(ip) = h["private_ip"].as_str() {
                outputs.insert("hostname".into(), ip.to_string());
            }
        }
        Some("queue") => {
            if let Some(url) = h["queue_url"].as_str() {
                outputs.insert("queue_url".into(), url.to_string());
            }
        }
        _ => {}
    }
    outputs
}

/// Extract the hostname from a URL string without requiring the `url` crate.
///
/// Strips `https://` or `http://` prefix, then takes the portion before the first `/` or `:`.
fn extract_url_hostname(url: &str) -> String {
    let without_proto = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let without_path = without_proto.splitn(2, '/').next().unwrap_or(without_proto);
    let without_port = without_path.splitn(2, ':').next().unwrap_or(without_path);
    without_port.to_string()
}

fn extract_resource_names(resp: &Value) -> Vec<String> {
    let mut names = Vec::new();
    if let Some(rows) = resp["data"]["rows"].as_array() {
        let cols = resp["data"]["columns"].as_array().cloned().unwrap_or_default();
        let id_idx = cols.iter().position(|c| c["name"].as_str() == Some("id"))
            .or_else(|| cols.iter().position(|c| c["name"].as_str() == Some("name")));
        if let Some(idx) = id_idx {
            for row in rows {
                if let Some(name) = row.as_array().and_then(|r| r.get(idx)).and_then(|v| v.as_str()) {
                    names.push(name.to_string());
                }
            }
        }
    }
    names
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use nclav_domain::{CloudTarget, EnclaveId, PartitionId};
    use wiremock::{
        matchers::{method, path, path_regex},
        Mock, MockServer, ResponseTemplate,
    };

    fn test_config() -> AzureDriverConfig {
        AzureDriverConfig {
            tenant_id:             "test-tenant-id".into(),
            management_group_id:   "test-mg".into(),
            billing_account_name:  "12345678-XXXX-XXXX-XXXX-XXXXXXXXXXXX:YYYYYY".into(),
            billing_profile_name:  "ABCD-EFGH".into(),
            invoice_section_name:  "IJKL-MNOP".into(),
            default_location:      "eastus2".into(),
            subscription_prefix:   None,
            client_id:             None,
            client_secret:         None,
        }
    }

    fn test_base(url: &str) -> BaseUrls {
        BaseUrls {
            management: url.to_string(),
            login:      url.to_string(),
            graph:      url.to_string(),
        }
    }

    fn driver(server: &MockServer) -> AzureDriver {
        AzureDriver::with_static_token(test_config(), "fake-token", test_base(&server.uri()))
    }

    fn dummy_enclave() -> Enclave {
        Enclave {
            id:         EnclaveId::new("product-a-dev"),
            name:       "Product A Dev".into(),
            cloud:      Some(CloudTarget::Azure),
            region:     "eastus2".into(),
            identity:   None,
            network:    None,
            dns:        None,
            imports:    vec![],
            exports:    vec![],
            partitions: vec![],
        }
    }

    fn dummy_partition() -> Partition {
        Partition {
            id:               PartitionId::new("api"),
            name:             "API".into(),
            produces:         None,
            imports:          vec![],
            exports:          vec![],
            inputs:           HashMap::new(),
            declared_outputs: vec![],
            backend:          Default::default(),
        }
    }

    /// Mount mocks for subscription alias create (PUT + GET for sub ID retrieval).
    #[allow(dead_code)]
    async fn mock_subscription_create(server: &MockServer, alias: &str, sub_id: &str) {
        // PUT alias → 202 with async op location
        let op_path = format!("/providers/Microsoft.Subscription/operationresults/op-{}", alias);
        Mock::given(method("PUT"))
            .and(path_regex(r"^/providers/Microsoft\.Subscription/aliases/.*"))
            .respond_with(ResponseTemplate::new(202)
                .append_header("Azure-AsyncOperation", format!("{}{}", "{url}", op_path))
                .set_body_json(json!({})))
            .mount(server)
            .await;

        // Poll op → Succeeded with subscription ID
        Mock::given(method("GET"))
            .and(path(op_path.as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status": "Succeeded",
                "properties": { "subscriptionId": sub_id }
            })))
            .mount(server)
            .await;

        // GET alias to retrieve subscription ID
        Mock::given(method("GET"))
            .and(path_regex(r"^/providers/Microsoft\.Subscription/aliases/.*"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "properties": { "subscriptionId": sub_id }
            })))
            .mount(server)
            .await;
    }

    #[allow(dead_code)]
    async fn mock_resource_group_create(server: &MockServer, sub_id: &str) {
        Mock::given(method("PUT"))
            .and(path(format!("/subscriptions/{}/resourcegroups/nclav-rg", sub_id).as_str()))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "id":   format!("/subscriptions/{}/resourceGroups/nclav-rg", sub_id),
                "name": "nclav-rg",
                "location": "eastus2",
            })))
            .mount(server)
            .await;
    }

    async fn mock_identity_create(server: &MockServer, sub_id: &str, name: &str) {
        Mock::given(method("PUT"))
            .and(path(format!(
                "/subscriptions/{}/resourceGroups/nclav-rg/providers/Microsoft.ManagedIdentity/userAssignedIdentities/{}",
                sub_id, name,
            ).as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": format!(
                    "/subscriptions/{}/resourceGroups/nclav-rg/providers/Microsoft.ManagedIdentity/userAssignedIdentities/{}",
                    sub_id, name,
                ),
                "properties": {
                    "principalId": "aaaa-1111-bbbb-2222",
                    "clientId":    "cccc-3333-dddd-4444",
                }
            })))
            .mount(server)
            .await;
    }

    #[allow(dead_code)]
    async fn mock_move_to_mg(server: &MockServer, mg_id: &str, sub_id: &str) {
        Mock::given(method("PUT"))
            .and(path(format!(
                "/providers/Microsoft.Management/managementGroups/{}/subscriptions/{}",
                mg_id, sub_id,
            ).as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(server)
            .await;
    }

    #[allow(dead_code)]
    async fn mock_teardown(server: &MockServer, sub_id: &str) {
        Mock::given(method("POST"))
            .and(path(format!(
                "/subscriptions/{}/providers/Microsoft.Subscription/cancel",
                sub_id
            ).as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "subscriptionId": sub_id
            })))
            .mount(server)
            .await;
    }

    async fn mock_partition_sa_creation(server: &MockServer, sub_id: &str, partition_id: &str) {
        let mi_name = partition_mi_name(partition_id);
        mock_identity_create(server, sub_id, &mi_name).await;

        // RBAC assignment
        Mock::given(method("PUT"))
            .and(path_regex(r".*Microsoft\.Authorization/roleAssignments/.*"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "id": "/subscriptions/test-sub/providers/Microsoft.Authorization/roleAssignments/some-uuid",
                "properties": {
                    "roleDefinitionId": "/subscriptions/test-sub/providers/Microsoft.Authorization/roleDefinitions/b24988ac-6180-42a0-ab88-20f7382dd24c",
                    "principalId": "aaaa-1111-bbbb-2222",
                }
            })))
            .mount(server)
            .await;
    }

    // ── sanitize_subscription_alias (pure) ────────────────────────────────────

    #[test]
    fn alias_passthrough() {
        assert_eq!(sanitize_subscription_alias("product-a-dev"), "product-a-dev");
    }

    #[test]
    fn alias_with_prefix() {
        assert_eq!(sanitize_subscription_alias("acme-product-a-dev"), "acme-product-a-dev");
    }

    #[test]
    fn alias_invalid_chars_replaced() {
        assert_eq!(sanitize_subscription_alias("my org/product"), "my-org-product");
    }

    #[test]
    fn alias_truncates_at_63() {
        let long = "a".repeat(80);
        let result = sanitize_subscription_alias(&long);
        assert!(result.len() <= 63, "len={}", result.len());
    }

    // ── partition_mi_name (pure) ──────────────────────────────────────────────

    #[test]
    fn partition_mi_name_short() {
        assert_eq!(partition_mi_name("api"), "partition-api");
    }

    #[test]
    fn partition_mi_name_long_is_shortened() {
        let long_id = "a".repeat(60);
        let name = partition_mi_name(&long_id);
        assert!(name.len() <= 64, "len={}", name.len());
    }

    // ── wait_for_operation ────────────────────────────────────────────────────

    #[tokio::test]
    async fn wait_for_operation_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/operations/test-op"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status": "Succeeded",
                "properties": { "subscriptionId": "test-sub-123" }
            })))
            .mount(&server)
            .await;

        let d   = driver(&server);
        let url = format!("{}/operations/test-op", server.uri());
        let res = d.wait_for_operation(&url).await.unwrap();
        assert_eq!(res["status"].as_str(), Some("Succeeded"));
    }

    #[tokio::test]
    async fn wait_for_operation_fails_on_failed_status() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/operations/op-fail"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status": "Failed",
                "error": { "code": "InternalError", "message": "Something went wrong" }
            })))
            .mount(&server)
            .await;

        let d   = driver(&server);
        let url = format!("{}/operations/op-fail", server.uri());
        let err = d.wait_for_operation(&url).await.unwrap_err();
        assert!(err.to_string().contains("Failed"), "got: {}", err);
    }

    // ── parse_arm_error (pure) ────────────────────────────────────────────────

    #[test]
    fn parse_arm_error_standard() {
        let body = json!({
            "error": { "code": "ResourceNotFound", "message": "The resource was not found" }
        });
        let msg = AzureDriver::parse_arm_error(&body);
        assert!(msg.contains("ResourceNotFound"), "got: {}", msg);
        assert!(msg.contains("not found"), "got: {}", msg);
    }

    #[test]
    fn parse_arm_error_missing_fields_gives_fallback() {
        let body = json!({ "error": {} });
        let msg = AzureDriver::parse_arm_error(&body);
        assert_eq!(msg, "Unknown: unknown error");
    }

    // ── provision_enclave idempotency ─────────────────────────────────────────

    #[tokio::test]
    async fn provision_enclave_idempotent_when_complete() {
        // When provisioning_complete is true in the existing handle, NO API calls should be made.
        let server = MockServer::start().await;
        let d      = driver(&server);
        let enc    = dummy_enclave();

        let existing = json!({
            "driver":                "azure",
            "kind":                  "enclave",
            "subscription_id":       "existing-sub-id",
            "subscription_alias":    "product-a-dev",
            "resource_group":        "nclav-rg",
            "location":              "eastus2",
            "identity_resource_id":  "/subscriptions/existing-sub-id/...",
            "identity_principal_id": "pppp-1234",
            "identity_client_id":    "cccc-5678",
            "vnet_resource_id":      "",
            "dns_zone_name":         "",
            "provisioning_complete": true,
        });

        let result = d.provision_enclave(&enc, Some(&existing)).await.unwrap();
        assert_eq!(result.handle["subscription_id"].as_str(), Some("existing-sub-id"));

        // wiremock will fail the test if any unexpected request was made
        let received = wiremock::MockServer::received_requests(&server).await;
        assert!(received.is_none() || received.unwrap().is_empty(),
            "Expected no API calls for idempotent enclave, but got requests");
    }

    // ── provision_partition creates MI ────────────────────────────────────────

    #[tokio::test]
    async fn provision_partition_creates_mi() {
        let server   = MockServer::start().await;
        let sub_id   = "test-sub-abc";
        let part_id  = "api";

        mock_partition_sa_creation(&server, sub_id, part_id).await;

        let d    = driver(&server);
        let enc  = dummy_enclave();
        let part = dummy_partition();

        // Simulate what the reconciler does: inject context_vars (from the enclave handle)
        // into resolved_inputs. The driver reads nclav_subscription_id from there.
        let mut resolved_inputs = HashMap::new();
        resolved_inputs.insert("nclav_subscription_id".into(), sub_id.to_string());
        resolved_inputs.insert("nclav_location".into(), "eastus2".to_string());

        let result = d.provision_partition(&enc, &part, &resolved_inputs, None).await.unwrap();
        assert_eq!(result.handle["kind"].as_str(), Some("partition"));
        assert_eq!(result.handle["driver"].as_str(), Some("azure"));
        assert_eq!(result.handle["subscription_id"].as_str(), Some(sub_id));
    }

    // ── observe_enclave ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn observe_enclave_enabled() {
        let server = MockServer::start().await;
        let sub_id = "test-sub-observe";

        Mock::given(method("GET"))
            .and(path(format!("/subscriptions/{}", sub_id).as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "subscriptionId": sub_id,
                "state":          "Enabled",
                "displayName":    "Test Enclave",
            })))
            .mount(&server)
            .await;

        let d       = driver(&server);
        let enc     = dummy_enclave();
        let handle  = json!({ "subscription_id": sub_id, "vnet_resource_id": "", "identity_resource_id": "" });
        let state   = d.observe_enclave(&enc, &handle).await.unwrap();

        assert!(state.exists,  "expected exists=true");
        assert!(state.healthy, "expected healthy=true");
    }

    #[tokio::test]
    async fn observe_enclave_not_found() {
        let server = MockServer::start().await;
        let sub_id = "nonexistent-sub";

        Mock::given(method("GET"))
            .and(path(format!("/subscriptions/{}", sub_id).as_str()))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({
                "error": { "code": "SubscriptionNotFound", "message": "Subscription not found" }
            })))
            .mount(&server)
            .await;

        let d      = driver(&server);
        let enc    = dummy_enclave();
        let handle = json!({ "subscription_id": sub_id, "vnet_resource_id": "", "identity_resource_id": "" });
        let state  = d.observe_enclave(&enc, &handle).await.unwrap();

        assert!(!state.exists,  "expected exists=false");
        assert!(!state.healthy, "expected healthy=false");
    }

    // ── observe_partition ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn observe_partition_iac_returns_healthy() {
        let server = MockServer::start().await;
        let d      = driver(&server);
        let enc    = dummy_enclave();
        let part   = dummy_partition();
        let handle = json!({ "driver": "azure", "kind": "partition", "type": "iac" });

        let state = d.observe_partition(&enc, &part, &handle).await.unwrap();
        assert!(state.exists);
        assert!(state.healthy);
    }

    // ── context_vars ──────────────────────────────────────────────────────────

    #[test]
    fn context_vars_returns_expected_keys() {
        let config = test_config();
        let base   = BaseUrls::default();
        let d      = AzureDriver { config, client: reqwest::Client::new(), token: Box::new(StaticToken("t".into())), base };
        let enc    = dummy_enclave();
        let handle = json!({
            "subscription_id":   "my-sub-id",
            "location":          "westus",
            "identity_client_id": "mi-client-id",
        });
        let vars = d.context_vars(&enc, &handle);
        assert_eq!(vars.get("nclav_subscription_id").map(|s| s.as_str()), Some("my-sub-id"));
        assert_eq!(vars.get("nclav_resource_group").map(|s| s.as_str()), Some("nclav-rg"));
        assert_eq!(vars.get("nclav_location").map(|s| s.as_str()), Some("westus"));
        assert_eq!(vars.get("nclav_identity_client_id").map(|s| s.as_str()), Some("mi-client-id"));
        // GCP-compat alias
        assert_eq!(vars.get("nclav_project_id").map(|s| s.as_str()), Some("my-sub-id"));
    }

    // ── auth_env ──────────────────────────────────────────────────────────────

    #[test]
    fn auth_env_sp_mode() {
        let mut config = test_config();
        config.client_id     = Some("my-client-id".into());
        config.client_secret = Some("my-secret".into());
        let base = BaseUrls::default();
        let d    = AzureDriver { config, client: reqwest::Client::new(), token: Box::new(StaticToken("t".into())), base };
        let enc  = dummy_enclave();
        let handle = json!({ "subscription_id": "sub-xyz" });
        let env  = d.auth_env(&enc, &handle);
        assert_eq!(env.get("ARM_TENANT_ID").map(|s| s.as_str()), Some("test-tenant-id"));
        assert_eq!(env.get("ARM_SUBSCRIPTION_ID").map(|s| s.as_str()), Some("sub-xyz"));
        assert_eq!(env.get("ARM_CLIENT_ID").map(|s| s.as_str()), Some("my-client-id"));
        assert_eq!(env.get("ARM_CLIENT_SECRET").map(|s| s.as_str()), Some("my-secret"));
    }
}
