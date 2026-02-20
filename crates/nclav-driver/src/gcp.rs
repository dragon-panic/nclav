use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use nclav_domain::{AuthType, Enclave, Export, ExportType, Import, Partition, ProducesType};
use serde_json::{json, Value};
use tracing::{debug, info, warn};

use crate::driver::{Driver, ObservedState, ProvisionResult};
use crate::error::DriverError;
use crate::Handle;

// ── Configuration ─────────────────────────────────────────────────────────────

/// Static configuration for the GCP driver, injected at startup.
/// Not stored in per-enclave YAML — these are operator-level settings.
#[derive(Clone)]
pub struct GcpDriverConfig {
    /// GCP resource parent for project creation: "folders/123" or "organizations/456".
    pub parent: String,
    /// Billing account to attach to every new project: "billingAccounts/XXXXXX-YYYYYY-ZZZZZZ".
    pub billing_account: String,
    /// Default region used when `enclave.region` is not otherwise specified.
    pub default_region: String,
}

// ── Base URLs (overridden in tests to point at a mock server) ─────────────────

#[derive(Clone)]
struct BaseUrls {
    resourcemanager: String,
    compute:         String,
    run:             String,
    iam:             String,
    pubsub:          String,
    sqladmin:        String,
    serviceusage:    String,
    cloudbilling:    String,
}

impl Default for BaseUrls {
    fn default() -> Self {
        Self {
            resourcemanager: "https://cloudresourcemanager.googleapis.com".into(),
            compute:         "https://compute.googleapis.com".into(),
            run:             "https://run.googleapis.com".into(),
            iam:             "https://iam.googleapis.com".into(),
            pubsub:          "https://pubsub.googleapis.com".into(),
            sqladmin:        "https://sqladmin.googleapis.com".into(),
            serviceusage:    "https://serviceusage.googleapis.com".into(),
            cloudbilling:    "https://cloudbilling.googleapis.com".into(),
        }
    }
}

// ── Token provider ────────────────────────────────────────────────────────────

/// Abstraction over GCP token acquisition — enables test injection.
#[async_trait]
trait TokenProvider: Send + Sync {
    async fn token(&self) -> Result<String, DriverError>;
}

/// Production token provider backed by Application Default Credentials.
struct AdcTokenProvider {
    inner: std::sync::Arc<dyn gcp_auth::TokenProvider>,
}

#[async_trait]
impl TokenProvider for AdcTokenProvider {
    async fn token(&self) -> Result<String, DriverError> {
        let token = self
            .inner
            .token(&["https://www.googleapis.com/auth/cloud-platform"])
            .await
            .map_err(|e| DriverError::Internal(format!("GCP auth failed: {}", e)))?;
        Ok(token.as_str().to_string())
    }
}

/// Test token provider — returns a fixed string without any network call.
pub struct StaticToken(pub String);

#[async_trait]
impl TokenProvider for StaticToken {
    async fn token(&self) -> Result<String, DriverError> {
        Ok(self.0.clone())
    }
}

// ── GCP APIs to enable on every new project ───────────────────────────────────

const REQUIRED_APIS: &[&str] = &[
    "compute.googleapis.com",
    "run.googleapis.com",
    "iam.googleapis.com",
    "cloudresourcemanager.googleapis.com",
    "dns.googleapis.com",
    "pubsub.googleapis.com",
    "sqladmin.googleapis.com",
    "servicenetworking.googleapis.com",
    "cloudbilling.googleapis.com",
];

// ── GcpDriver ─────────────────────────────────────────────────────────────────

pub struct GcpDriver {
    config: GcpDriverConfig,
    client: reqwest::Client,
    token:  Box<dyn TokenProvider>,
    base:   BaseUrls,
}

impl GcpDriver {
    /// Create a `GcpDriver` using Application Default Credentials.
    ///
    /// ADC resolution order:
    /// 1. `GOOGLE_APPLICATION_CREDENTIALS` env var (service account JSON key)
    /// 2. Workload Identity (when running on GCP)
    /// 3. `gcloud auth application-default login` for local dev
    pub async fn from_adc(config: GcpDriverConfig) -> Result<Self, DriverError> {
        let inner = gcp_auth::provider()
            .await
            .map_err(|e| DriverError::Internal(format!("Failed to initialise GCP ADC: {}", e)))?;
        Ok(Self {
            config,
            client: reqwest::Client::new(),
            token:  Box::new(AdcTokenProvider { inner }),
            base:   BaseUrls::default(),
        })
    }

    /// Create a `GcpDriver` with a static bearer token and custom base URLs.
    /// Used exclusively in tests — not exposed in the public API.
    #[cfg(test)]
    fn with_static_token(config: GcpDriverConfig, token: &str, base: BaseUrls) -> Self {
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

    fn region<'a>(&'a self, enclave: &'a Enclave) -> &'a str {
        &enclave.region
    }

    // ── GCP error parsing ─────────────────────────────────────────────────────

    /// Convert a GCP REST error envelope into a human-readable message:
    ///   `"PERMISSION_DENIED: The caller does not have permission [IAM_PERMISSION_DENIED — compute.networks.create]"`
    fn extract_gcp_error(body: &Value) -> String {
        let err = &body["error"];
        let status  = err["status"].as_str().unwrap_or("UNKNOWN");
        let message = err["message"].as_str().unwrap_or("unknown error");

        let detail_suffix = err["details"]
            .as_array()
            .and_then(|arr| arr.first())
            .and_then(|d| {
                let reason = d["reason"].as_str()?;
                let meta_vals: Vec<&str> = d["metadata"]
                    .as_object()
                    .map(|m| m.values().filter_map(|v| v.as_str()).collect())
                    .unwrap_or_default();
                Some(format!(" [{} — {}]", reason, meta_vals.join(", ")))
            })
            .unwrap_or_default();

        format!("{}: {}{}", status, message, detail_suffix)
    }

    // ── Long-running operation polling ────────────────────────────────────────

    /// Poll a GCP long-running operation URL until it completes or times out.
    ///
    /// Backoff: 1 s, 2 s, 4 s, 8 s, 16 s, 30 s, 30 s, … (max 30 polls ≈ ~10 min).
    async fn wait_for_operation(&self, op_url: &str) -> Result<Value, DriverError> {
        let token = self.bearer().await?;
        let delays = [1u64, 2, 4, 8, 16, 30];

        for &delay in delays.iter().cycle().take(30) {
            let resp: Value = self
                .client
                .get(op_url)
                .bearer_auth(&token)
                .send()
                .await
                .map_err(|e| DriverError::Internal(format!("poll {}: {}", op_url, e)))?
                .json()
                .await
                .map_err(|e| DriverError::Internal(format!("poll decode: {}", e)))?;

            if resp["done"].as_bool().unwrap_or(false) {
                if resp.get("error").is_some() {
                    let msg = Self::extract_gcp_error(&json!({ "error": resp["error"] }));
                    return Err(DriverError::ProvisionFailed(
                        format!("operation failed: {}", msg),
                    ));
                }
                return Ok(resp["response"].clone());
            }

            tokio::time::sleep(Duration::from_secs(delay)).await;
        }

        Err(DriverError::ProvisionFailed(
            "GCP operation timed out after 30 polls".into(),
        ))
    }

    // ── JSON helper ───────────────────────────────────────────────────────────

    async fn post_json(
        &self,
        url: &str,
        token: &str,
        body: &Value,
    ) -> Result<Value, DriverError> {
        let resp: Value = self
            .client
            .post(url)
            .bearer_auth(token)
            .json(body)
            .send()
            .await
            .map_err(|e| DriverError::ProvisionFailed(e.to_string()))?
            .json()
            .await
            .map_err(|e| DriverError::Internal(e.to_string()))?;
        if resp.get("error").is_some() {
            return Err(DriverError::ProvisionFailed(Self::extract_gcp_error(&resp)));
        }
        Ok(resp)
    }
}

// ── Driver impl ───────────────────────────────────────────────────────────────

#[async_trait]
impl Driver for GcpDriver {
    fn name(&self) -> &'static str {
        "gcp"
    }

    // ── provision_enclave ─────────────────────────────────────────────────────

    async fn provision_enclave(
        &self,
        enclave: &Enclave,
        existing: Option<&Handle>,
    ) -> Result<ProvisionResult, DriverError> {
        let token      = self.bearer().await?;
        let project_id = enclave.id.as_str();
        let region     = self.region(enclave);

        // Idempotency: if we already have a handle with a project_id, check if
        // it still exists and return early rather than trying to re-create.
        if let Some(handle) = existing {
            if let Some(pid) = handle["project_id"].as_str() {
                let url = format!("{}/v3/projects/{}", self.base.resourcemanager, pid);
                let resp = self
                    .client
                    .get(&url)
                    .bearer_auth(&token)
                    .send()
                    .await
                    .map_err(|e| DriverError::Internal(e.to_string()))?;
                if resp.status().is_success() {
                    debug!(project_id = pid, "GCP project already exists, skipping creation");
                    return Ok(ProvisionResult {
                        handle:  handle.clone(),
                        outputs: HashMap::new(),
                    });
                }
            }
        }

        // 1. Create project → returns a long-running operation
        info!(project_id, "Creating GCP project");
        let create_url = format!("{}/v3/projects", self.base.resourcemanager);
        let op = self
            .post_json(
                &create_url,
                &token,
                &json!({
                    "projectId":   project_id,
                    "displayName": enclave.name,
                    "parent":      self.config.parent,
                }),
            )
            .await?;

        let op_name = op["name"]
            .as_str()
            .ok_or_else(|| DriverError::ProvisionFailed("create project: no operation name".into()))?;
        let op_url = format!("{}/v3/{}", self.base.resourcemanager, op_name);
        let project_resp = self.wait_for_operation(&op_url).await?;
        let project_number = project_resp["projectNumber"]
            .as_str()
            .unwrap_or("")
            .to_string();

        // 2. Link billing account
        let billing_url = format!(
            "{}/v1/projects/{}/billingInfo",
            self.base.cloudbilling, project_id
        );
        self.client
            .put(&billing_url)
            .bearer_auth(&token)
            .json(&json!({ "billingAccountName": self.config.billing_account }))
            .send()
            .await
            .map_err(|e| DriverError::ProvisionFailed(format!("billing link: {}", e)))?;

        // 3. Enable required APIs
        let enable_url = format!(
            "{}/v1/projects/{}/services:batchEnable",
            self.base.serviceusage, project_id
        );
        let enable_op = self
            .post_json(&enable_url, &token, &json!({ "serviceIds": REQUIRED_APIS }))
            .await?;
        if let Some(op_name) = enable_op["name"].as_str() {
            let op_url = format!("{}/v1/{}", self.base.serviceusage, op_name);
            self.wait_for_operation(&op_url).await?;
        }

        // 4. Create enclave service account
        let sa_id  = enclave.identity.as_deref().unwrap_or(project_id);
        let sa_url = format!("{}/v1/projects/{}/serviceAccounts", self.base.iam, project_id);
        let sa_resp = self
            .post_json(
                &sa_url,
                &token,
                &json!({
                    "accountId":      sa_id,
                    "serviceAccount": { "displayName": enclave.name },
                }),
            )
            .await?;
        let sa_email = sa_resp["email"]
            .as_str()
            .unwrap_or(&format!("{}@{}.iam.gserviceaccount.com", sa_id, project_id))
            .to_string();

        // 5. Create VPC network (if network config is present)
        let mut vpc_self_link = String::new();
        if enclave.network.is_some() {
            let vpc_url = format!(
                "{}/compute/v1/projects/{}/global/networks",
                self.base.compute, project_id
            );
            let vpc_op = self
                .post_json(
                    &vpc_url,
                    &token,
                    &json!({ "name": "nclav-vpc", "autoCreateSubnetworks": false }),
                )
                .await?;
            if let Some(op_name) = vpc_op["name"].as_str() {
                // Compute operation URLs are project-scoped
                let op_url = format!(
                    "{}/compute/v1/projects/{}/global/operations/{}",
                    self.base.compute, project_id, op_name
                );
                self.wait_for_operation(&op_url).await?;
            }
            vpc_self_link = format!(
                "https://www.googleapis.com/compute/v1/projects/{}/global/networks/nclav-vpc",
                project_id
            );
        }

        let handle = json!({
            "driver":                "gcp",
            "kind":                  "enclave",
            "project_id":            project_id,
            "project_number":        project_number,
            "service_account_email": sa_email,
            "vpc_self_link":         vpc_self_link,
            "region":                region,
        });

        Ok(ProvisionResult { handle, outputs: HashMap::new() })
    }

    // ── teardown_enclave ──────────────────────────────────────────────────────

    async fn teardown_enclave(
        &self,
        enclave: &Enclave,
        _handle: &Handle,
    ) -> Result<(), DriverError> {
        let token      = self.bearer().await?;
        let project_id = enclave.id.as_str();
        let url        = format!("{}/v3/projects/{}", self.base.resourcemanager, project_id);

        let resp = self
            .client
            .delete(&url)
            .bearer_auth(&token)
            .send()
            .await
            .map_err(|e| DriverError::TeardownFailed(e.to_string()))?;

        let status = resp.status();
        if !status.is_success() && status.as_u16() != 404 {
            let body: Value = resp.json().await.unwrap_or_default();
            return Err(DriverError::TeardownFailed(Self::extract_gcp_error(&body)));
        }

        info!(project_id, "GCP project delete requested (30-day hold)");
        Ok(())
    }

    // ── provision_partition ───────────────────────────────────────────────────

    async fn provision_partition(
        &self,
        enclave: &Enclave,
        partition: &Partition,
        resolved_inputs: &HashMap<String, String>,
        _existing: Option<&Handle>,
    ) -> Result<ProvisionResult, DriverError> {
        let token        = self.bearer().await?;
        let project_id   = enclave.id.as_str();
        let region       = self.region(enclave);
        let partition_id = partition.id.as_str();

        match &partition.produces {
            // ── Cloud Run (http) ─────────────────────────────────────────────
            Some(ProducesType::Http) => {
                let image = resolved_inputs
                    .get("image")
                    .cloned()
                    .unwrap_or_else(|| "gcr.io/cloudrun/hello".into());
                let sa_email = format!("{}@{}.iam.gserviceaccount.com", project_id, project_id);
                let env: Vec<Value> = resolved_inputs
                    .iter()
                    .filter(|(k, _)| *k != "image")
                    .map(|(k, v)| json!({ "name": k, "value": v }))
                    .collect();

                let service_name = format!(
                    "projects/{}/locations/{}/services/{}",
                    project_id, region, partition_id
                );
                let url = format!(
                    "{}/v2/projects/{}/locations/{}/services",
                    self.base.run, project_id, region
                );
                let op = self
                    .post_json(
                        &url,
                        &token,
                        &json!({
                            "name": service_name,
                            "template": {
                                "serviceAccount": sa_email,
                                "containers": [{ "image": image, "env": env }],
                            },
                            "ingress": "INGRESS_TRAFFIC_INTERNAL_ONLY",
                        }),
                    )
                    .await?;

                // Poll the operation if it isn't immediately done
                if op.get("done").is_some() && !op["done"].as_bool().unwrap_or(true) {
                    let op_name = op["name"]
                        .as_str()
                        .ok_or_else(|| DriverError::ProvisionFailed("Cloud Run op: no name".into()))?;
                    let op_url = format!("{}/v2/{}", self.base.run, op_name);
                    self.wait_for_operation(&op_url).await?;
                }

                // Fetch the service to read the generated URL
                let get_url = format!(
                    "{}/v2/projects/{}/locations/{}/services/{}",
                    self.base.run, project_id, region, partition_id
                );
                let svc: Value = self
                    .client
                    .get(&get_url)
                    .bearer_auth(&token)
                    .send()
                    .await
                    .map_err(|e| DriverError::Internal(e.to_string()))?
                    .json()
                    .await
                    .map_err(|e| DriverError::Internal(e.to_string()))?;

                let service_url = svc["uri"].as_str().unwrap_or("").to_string();
                let hostname    = service_url.trim_start_matches("https://").to_string();

                let handle = json!({
                    "driver":       "gcp",
                    "kind":         "partition",
                    "type":         "cloud_run",
                    "project_id":   project_id,
                    "region":       region,
                    "service_name": service_name,
                    "service_url":  service_url,
                });
                let mut outputs = HashMap::new();
                outputs.insert("hostname".into(), hostname);
                outputs.insert("port".into(), "443".into());

                Ok(ProvisionResult { handle, outputs })
            }

            // ── Cloud SQL (tcp) ──────────────────────────────────────────────
            Some(ProducesType::Tcp) => {
                let url = format!(
                    "{}/v1/projects/{}/instances",
                    self.base.sqladmin, project_id
                );
                let vpc_link = format!("projects/{}/global/networks/nclav-vpc", project_id);
                let op = self
                    .post_json(
                        &url,
                        &token,
                        &json!({
                            "name":            partition_id,
                            "databaseVersion": "POSTGRES_16",
                            "region":          region,
                            "settings": {
                                "tier": "db-f1-micro",
                                "ipConfiguration": {
                                    "ipv4Enabled":    false,
                                    "privateNetwork": vpc_link,
                                },
                            },
                        }),
                    )
                    .await?;

                if let Some(op_name) = op["name"].as_str() {
                    let op_url = format!(
                        "{}/v1/projects/{}/operations/{}",
                        self.base.sqladmin, project_id, op_name
                    );
                    self.wait_for_operation(&op_url).await?;
                }

                // Fetch instance to read private IP
                let get_url = format!(
                    "{}/v1/projects/{}/instances/{}",
                    self.base.sqladmin, project_id, partition_id
                );
                let instance: Value = self
                    .client
                    .get(&get_url)
                    .bearer_auth(&token)
                    .send()
                    .await
                    .map_err(|e| DriverError::Internal(e.to_string()))?
                    .json()
                    .await
                    .map_err(|e| DriverError::Internal(e.to_string()))?;

                let hostname = instance["ipAddresses"]
                    .as_array()
                    .and_then(|arr| {
                        arr.iter()
                            .find(|ip| ip["type"].as_str() == Some("PRIVATE"))
                            .and_then(|ip| ip["ipAddress"].as_str())
                    })
                    .unwrap_or("127.0.0.1")
                    .to_string();

                let handle = json!({
                    "driver":        "gcp",
                    "kind":          "partition",
                    "type":          "cloud_sql",
                    "project_id":    project_id,
                    "instance_name": partition_id,
                    "region":        region,
                });
                let mut outputs = HashMap::new();
                outputs.insert("hostname".into(), hostname);
                outputs.insert("port".into(), "5432".into());

                Ok(ProvisionResult { handle, outputs })
            }

            // ── Pub/Sub topic (queue) ────────────────────────────────────────
            Some(ProducesType::Queue) => {
                let url = format!(
                    "{}/v1/projects/{}/topics/{}",
                    self.base.pubsub, project_id, partition_id
                );
                let resp = self
                    .client
                    .put(&url)
                    .bearer_auth(&token)
                    .json(&json!({}))
                    .send()
                    .await
                    .map_err(|e| DriverError::ProvisionFailed(e.to_string()))?;

                let status = resp.status();
                if !status.is_success() && status.as_u16() != 409 {
                    // 409 ALREADY_EXISTS is idempotent success
                    let body: Value = resp.json().await.unwrap_or_default();
                    return Err(DriverError::ProvisionFailed(Self::extract_gcp_error(&body)));
                }

                let queue_url = format!("projects/{}/topics/{}", project_id, partition_id);
                let handle = json!({
                    "driver":     "gcp",
                    "kind":       "partition",
                    "type":       "pubsub_topic",
                    "project_id": project_id,
                    "topic_name": queue_url,
                });
                let mut outputs = HashMap::new();
                outputs.insert("queue_url".into(), queue_url);

                Ok(ProvisionResult { handle, outputs })
            }

            None => Err(DriverError::ProvisionFailed(format!(
                "partition '{}' has no produces type; GCP driver requires one",
                partition.id
            ))),
        }
    }

    // ── teardown_partition ────────────────────────────────────────────────────

    async fn teardown_partition(
        &self,
        enclave: &Enclave,
        partition: &Partition,
        handle: &Handle,
    ) -> Result<(), DriverError> {
        let token        = self.bearer().await?;
        let project_id   = enclave.id.as_str();
        let partition_id = partition.id.as_str();
        let region       = self.region(enclave);

        let url = match handle["type"].as_str().unwrap_or("") {
            "cloud_run"    => format!(
                "{}/v2/projects/{}/locations/{}/services/{}",
                self.base.run, project_id, region, partition_id
            ),
            "cloud_sql"    => format!(
                "{}/v1/projects/{}/instances/{}",
                self.base.sqladmin, project_id, partition_id
            ),
            "pubsub_topic" => format!(
                "{}/v1/projects/{}/topics/{}",
                self.base.pubsub, project_id, partition_id
            ),
            other => {
                warn!(kind = other, "teardown_partition: unknown partition type, skipping");
                return Ok(());
            }
        };

        let resp = self
            .client
            .delete(&url)
            .bearer_auth(&token)
            .send()
            .await
            .map_err(|e| DriverError::TeardownFailed(e.to_string()))?;

        let status = resp.status();
        if !status.is_success() && status.as_u16() != 404 {
            let body: Value = resp.json().await.unwrap_or_default();
            return Err(DriverError::TeardownFailed(Self::extract_gcp_error(&body)));
        }
        Ok(())
    }

    // ── provision_export ──────────────────────────────────────────────────────

    async fn provision_export(
        &self,
        enclave: &Enclave,
        export: &Export,
        partition_outputs: &HashMap<String, String>,
        _existing: Option<&Handle>,
    ) -> Result<ProvisionResult, DriverError> {
        let token      = self.bearer().await?;
        let project_id = enclave.id.as_str();
        let region     = self.region(enclave);

        match export.export_type {
            ExportType::Http => {
                let service_name = format!(
                    "projects/{}/locations/{}/services/{}",
                    project_id, region, export.target_partition.as_str()
                );
                // For auth:none we grant allUsers run.invoker immediately.
                // For other auth types the IAM binding is added at import time.
                if matches!(export.auth, AuthType::None) {
                    let iam_url = format!("{}/v2/{}:setIamPolicy", self.base.run, service_name);
                    self.post_json(
                        &iam_url,
                        &token,
                        &json!({
                            "policy": {
                                "bindings": [{
                                    "role":    "roles/run.invoker",
                                    "members": ["allUsers"],
                                }],
                            },
                        }),
                    )
                    .await?;
                }

                let handle = json!({
                    "driver":               "gcp",
                    "kind":                 "export",
                    "type":                 "http",
                    "project_id":           project_id,
                    "export_name":          export.name,
                    "cloud_run_service":    service_name,
                    "iam_bindings_applied": if matches!(export.auth, AuthType::None) {
                        json!(["allUsers:roles/run.invoker"])
                    } else {
                        json!([])
                    },
                    "outputs": partition_outputs,
                });
                Ok(ProvisionResult { handle, outputs: partition_outputs.clone() })
            }

            ExportType::Tcp => {
                // PSC attachment is complex; record the region/project for import wiring.
                let handle = json!({
                    "driver":      "gcp",
                    "kind":        "export",
                    "type":        "tcp",
                    "project_id":  project_id,
                    "export_name": export.name,
                    "region":      region,
                    "outputs":     partition_outputs,
                });
                Ok(ProvisionResult { handle, outputs: partition_outputs.clone() })
            }

            ExportType::Queue => {
                let handle = json!({
                    "driver":      "gcp",
                    "kind":        "export",
                    "type":        "queue",
                    "project_id":  project_id,
                    "export_name": export.name,
                    "topic": partition_outputs.get("queue_url").cloned().unwrap_or_default(),
                    "outputs":     partition_outputs,
                });
                Ok(ProvisionResult { handle, outputs: partition_outputs.clone() })
            }
        }
    }

    // ── provision_import ──────────────────────────────────────────────────────

    async fn provision_import(
        &self,
        importer: &Enclave,
        import: &Import,
        export_handle: &Handle,
        _existing: Option<&Handle>,
    ) -> Result<ProvisionResult, DriverError> {
        let token            = self.bearer().await?;
        let importer_project = importer.id.as_str();
        let export_type      = export_handle["type"].as_str().unwrap_or("");
        let mut outputs      = HashMap::new();

        match export_type {
            "http" => {
                // Inject resolved outputs from the export handle.
                if let Some(obj) = export_handle["outputs"].as_object() {
                    for (k, v) in obj {
                        if let Some(s) = v.as_str() {
                            outputs.insert(k.clone(), s.to_string());
                        }
                    }
                }

                let handle = json!({
                    "driver":           "gcp",
                    "kind":             "import",
                    "type":             "http",
                    "importer_project": importer_project,
                    "alias":            import.alias,
                    "export_handle":    export_handle,
                    "outputs":          outputs,
                });
                Ok(ProvisionResult { handle, outputs })
            }

            "tcp" => {
                // Propagate connection details (PSC wiring would go here).
                if let Some(obj) = export_handle["outputs"].as_object() {
                    for (k, v) in obj {
                        if let Some(s) = v.as_str() {
                            outputs.insert(k.clone(), s.to_string());
                        }
                    }
                }

                let handle = json!({
                    "driver":           "gcp",
                    "kind":             "import",
                    "type":             "tcp",
                    "importer_project": importer_project,
                    "alias":            import.alias,
                    "outputs":          outputs,
                });
                Ok(ProvisionResult { handle, outputs })
            }

            "queue" => {
                // Create cross-project Pub/Sub subscription in the importer's project.
                let exporter_topic = export_handle["topic"].as_str().unwrap_or("");
                let sub_url = format!(
                    "{}/v1/projects/{}/subscriptions/{}",
                    self.base.pubsub, importer_project, import.alias
                );
                let resp = self
                    .client
                    .put(&sub_url)
                    .bearer_auth(&token)
                    .json(&json!({
                        "topic":              exporter_topic,
                        "ackDeadlineSeconds": 60,
                    }))
                    .send()
                    .await
                    .map_err(|e| DriverError::ProvisionFailed(e.to_string()))?;

                let status = resp.status();
                if !status.is_success() && status.as_u16() != 409 {
                    let body: Value = resp.json().await.unwrap_or_default();
                    return Err(DriverError::ProvisionFailed(Self::extract_gcp_error(&body)));
                }

                let queue_url = format!(
                    "projects/{}/subscriptions/{}",
                    importer_project, import.alias
                );
                outputs.insert("queue_url".into(), queue_url.clone());

                let handle = json!({
                    "driver":           "gcp",
                    "kind":             "import",
                    "type":             "queue",
                    "importer_project": importer_project,
                    "alias":            import.alias,
                    "subscription":     queue_url,
                    "outputs":          outputs,
                });
                Ok(ProvisionResult { handle, outputs })
            }

            other => Err(DriverError::ProvisionFailed(format!(
                "provision_import: unknown export type '{}' in export handle",
                other
            ))),
        }
    }

    // ── observe_enclave ───────────────────────────────────────────────────────

    async fn observe_enclave(
        &self,
        enclave: &Enclave,
        handle: &Handle,
    ) -> Result<ObservedState, DriverError> {
        let token      = self.bearer().await?;
        let project_id = handle["project_id"]
            .as_str()
            .unwrap_or(enclave.id.as_str());

        let url = format!("{}/v3/projects/{}", self.base.resourcemanager, project_id);
        let resp = self
            .client
            .get(&url)
            .bearer_auth(&token)
            .send()
            .await
            .map_err(|e| DriverError::Internal(e.to_string()))?;

        if resp.status().as_u16() == 404 {
            return Ok(ObservedState {
                exists:  false,
                healthy: false,
                outputs: HashMap::new(),
                raw:     json!({}),
            });
        }
        if !resp.status().is_success() {
            let body: Value = resp.json().await.unwrap_or_default();
            return Err(DriverError::Internal(Self::extract_gcp_error(&body)));
        }

        let project: Value = resp
            .json()
            .await
            .map_err(|e| DriverError::Internal(e.to_string()))?;

        let lifecycle = project["lifecycleState"].as_str().unwrap_or("");
        let healthy   = lifecycle == "ACTIVE";

        Ok(ObservedState {
            exists:  true,
            healthy,
            outputs: HashMap::new(),
            raw:     project,
        })
    }

    // ── observe_partition ─────────────────────────────────────────────────────

    async fn observe_partition(
        &self,
        enclave: &Enclave,
        partition: &Partition,
        handle: &Handle,
    ) -> Result<ObservedState, DriverError> {
        let token        = self.bearer().await?;
        let project_id   = handle["project_id"].as_str().unwrap_or(enclave.id.as_str());
        let region       = self.region(enclave);
        let partition_id = partition.id.as_str();

        match handle["type"].as_str().unwrap_or("") {
            // ── Cloud Run ────────────────────────────────────────────────────
            "cloud_run" => {
                let url = format!(
                    "{}/v2/projects/{}/locations/{}/services/{}",
                    self.base.run, project_id, region, partition_id
                );
                let resp = self
                    .client
                    .get(&url)
                    .bearer_auth(&token)
                    .send()
                    .await
                    .map_err(|e| DriverError::Internal(e.to_string()))?;

                if resp.status().as_u16() == 404 {
                    return Ok(ObservedState {
                        exists: false, healthy: false,
                        outputs: HashMap::new(), raw: json!({}),
                    });
                }

                let svc: Value = resp
                    .json()
                    .await
                    .map_err(|e| DriverError::Internal(e.to_string()))?;

                // "Ready" condition: True → healthy, False → unhealthy, Unknown → in-progress
                let ready_status = svc["conditions"]
                    .as_array()
                    .and_then(|arr| arr.iter().find(|c| c["type"] == "Ready"))
                    .and_then(|c| c["status"].as_str());
                let healthy = ready_status == Some("True");

                let service_url = svc["uri"].as_str().unwrap_or("").to_string();
                let hostname    = service_url.trim_start_matches("https://").to_string();
                let mut outputs = HashMap::new();
                if !hostname.is_empty() {
                    outputs.insert("hostname".into(), hostname);
                    outputs.insert("port".into(), "443".into());
                }

                Ok(ObservedState { exists: true, healthy, outputs, raw: svc })
            }

            // ── Cloud SQL ────────────────────────────────────────────────────
            "cloud_sql" => {
                let url = format!(
                    "{}/v1/projects/{}/instances/{}",
                    self.base.sqladmin, project_id, partition_id
                );
                let resp = self
                    .client
                    .get(&url)
                    .bearer_auth(&token)
                    .send()
                    .await
                    .map_err(|e| DriverError::Internal(e.to_string()))?;

                if resp.status().as_u16() == 404 {
                    return Ok(ObservedState {
                        exists: false, healthy: false,
                        outputs: HashMap::new(), raw: json!({}),
                    });
                }

                let instance: Value = resp
                    .json()
                    .await
                    .map_err(|e| DriverError::Internal(e.to_string()))?;

                let state   = instance["state"].as_str().unwrap_or("");
                let healthy = state == "RUNNABLE";

                let hostname = instance["ipAddresses"]
                    .as_array()
                    .and_then(|arr| {
                        arr.iter()
                            .find(|ip| ip["type"].as_str() == Some("PRIVATE"))
                            .and_then(|ip| ip["ipAddress"].as_str())
                    })
                    .unwrap_or("")
                    .to_string();

                let mut outputs = HashMap::new();
                if !hostname.is_empty() {
                    outputs.insert("hostname".into(), hostname);
                    outputs.insert("port".into(), "5432".into());
                }

                Ok(ObservedState { exists: true, healthy, outputs, raw: instance })
            }

            // ── Pub/Sub topic ────────────────────────────────────────────────
            "pubsub_topic" => {
                let fallback = format!("projects/{}/topics/{}", project_id, partition_id);
                let topic    = handle["topic_name"].as_str().unwrap_or(&fallback);
                let url      = format!("{}/v1/{}", self.base.pubsub, topic);
                let resp = self
                    .client
                    .get(&url)
                    .bearer_auth(&token)
                    .send()
                    .await
                    .map_err(|e| DriverError::Internal(e.to_string()))?;

                if resp.status().as_u16() == 404 {
                    return Ok(ObservedState {
                        exists: false, healthy: false,
                        outputs: HashMap::new(), raw: json!({}),
                    });
                }

                let topic_resp: Value = resp
                    .json()
                    .await
                    .map_err(|e| DriverError::Internal(e.to_string()))?;

                let queue_url = topic_resp["name"]
                    .as_str()
                    .unwrap_or(topic)
                    .to_string();
                let mut outputs = HashMap::new();
                outputs.insert("queue_url".into(), queue_url);

                Ok(ObservedState { exists: true, healthy: true, outputs, raw: topic_resp })
            }

            other => {
                warn!(kind = other, "observe_partition: unknown partition type");
                Ok(ObservedState {
                    exists: false, healthy: false,
                    outputs: HashMap::new(), raw: json!({}),
                })
            }
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use nclav_domain::{CloudTarget, EnclaveId, PartitionId};
    use wiremock::{
        matchers::{method, path},
        Mock, MockServer, ResponseTemplate,
    };

    // ── Test helpers ──────────────────────────────────────────────────────────

    fn test_config() -> GcpDriverConfig {
        GcpDriverConfig {
            parent:         "folders/123456".into(),
            billing_account: "billingAccounts/AAAAAA-BBBBBB-CCCCCC".into(),
            default_region:  "us-central1".into(),
        }
    }

    /// All base URLs point at the same mock server — the paths distinguish them.
    fn test_base(url: &str) -> BaseUrls {
        BaseUrls {
            resourcemanager: url.to_string(),
            compute:         url.to_string(),
            run:             url.to_string(),
            iam:             url.to_string(),
            pubsub:          url.to_string(),
            sqladmin:        url.to_string(),
            serviceusage:    url.to_string(),
            cloudbilling:    url.to_string(),
        }
    }

    fn driver(server: &MockServer) -> GcpDriver {
        GcpDriver::with_static_token(test_config(), "fake-token", test_base(&server.uri()))
    }

    fn dummy_enclave() -> Enclave {
        Enclave {
            id:         EnclaveId::new("test-proj"),
            name:       "Test Project".into(),
            cloud:      CloudTarget::Local,
            region:     "us-central1".into(),
            identity:   None,
            network:    None,
            dns:        None,
            imports:    vec![],
            exports:    vec![],
            partitions: vec![],
        }
    }

    fn http_partition() -> Partition {
        Partition {
            id:               PartitionId::new("api"),
            name:             "API".into(),
            produces:         Some(ProducesType::Http),
            imports:          vec![],
            exports:          vec![],
            inputs:           HashMap::new(),
            declared_outputs: vec!["hostname".into(), "port".into()],
        }
    }

    fn tcp_partition() -> Partition {
        Partition {
            id:               PartitionId::new("db"),
            name:             "DB".into(),
            produces:         Some(ProducesType::Tcp),
            imports:          vec![],
            exports:          vec![],
            inputs:           HashMap::new(),
            declared_outputs: vec!["hostname".into(), "port".into()],
        }
    }

    fn queue_partition() -> Partition {
        Partition {
            id:               PartitionId::new("queue"),
            name:             "Queue".into(),
            produces:         Some(ProducesType::Queue),
            imports:          vec![],
            exports:          vec![],
            inputs:           HashMap::new(),
            declared_outputs: vec!["queue_url".into()],
        }
    }

    // ── GCP error parsing (pure, no mocking) ──────────────────────────────────

    #[test]
    fn parse_gcp_error_simple() {
        let body = json!({
            "error": {
                "code":    403,
                "status":  "PERMISSION_DENIED",
                "message": "The caller does not have permission",
            }
        });
        let msg = GcpDriver::extract_gcp_error(&body);
        assert_eq!(msg, "PERMISSION_DENIED: The caller does not have permission");
    }

    #[test]
    fn parse_gcp_error_with_error_info_details() {
        let body = json!({
            "error": {
                "code":    403,
                "status":  "PERMISSION_DENIED",
                "message": "The caller does not have permission",
                "details": [{
                    "@type":   "type.googleapis.com/google.rpc.ErrorInfo",
                    "reason":  "IAM_PERMISSION_DENIED",
                    "domain":  "iam.googleapis.com",
                    "metadata": { "permission": "compute.networks.create" },
                }],
            }
        });
        let msg = GcpDriver::extract_gcp_error(&body);
        assert!(msg.contains("PERMISSION_DENIED"), "status not in message");
        assert!(msg.contains("IAM_PERMISSION_DENIED"), "reason not in message");
        assert!(msg.contains("compute.networks.create"), "metadata not in message");
    }

    #[test]
    fn parse_gcp_error_missing_fields_gives_fallback() {
        let body = json!({ "error": {} });
        let msg = GcpDriver::extract_gcp_error(&body);
        assert_eq!(msg, "UNKNOWN: unknown error");
    }

    // ── wait_for_operation ────────────────────────────────────────────────────

    #[tokio::test]
    async fn wait_for_operation_returns_response_on_done() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v3/operations/op-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "name":     "operations/op-1",
                "done":     true,
                "response": { "projectNumber": "999" },
            })))
            .mount(&server)
            .await;

        let d    = driver(&server);
        let url  = format!("{}/v3/operations/op-1", server.uri());
        let resp = d.wait_for_operation(&url).await.unwrap();
        assert_eq!(resp["projectNumber"], "999");
    }

    #[tokio::test]
    async fn wait_for_operation_errors_on_failed_op() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v3/operations/op-fail"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "name": "operations/op-fail",
                "done": true,
                "error": {
                    "code":    403,
                    "status":  "PERMISSION_DENIED",
                    "message": "Permission denied",
                },
            })))
            .mount(&server)
            .await;

        let d   = driver(&server);
        let url = format!("{}/v3/operations/op-fail", server.uri());
        let err = d.wait_for_operation(&url).await.unwrap_err();
        assert!(matches!(err, DriverError::ProvisionFailed(_)));
        assert!(err.to_string().contains("PERMISSION_DENIED"));
    }

    // ── observe_enclave ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn observe_enclave_active() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v3/projects/test-proj"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "projectId":      "test-proj",
                "lifecycleState": "ACTIVE",
            })))
            .mount(&server)
            .await;

        let obs = driver(&server)
            .observe_enclave(&dummy_enclave(), &json!({ "project_id": "test-proj" }))
            .await
            .unwrap();

        assert!(obs.exists);
        assert!(obs.healthy);
    }

    #[tokio::test]
    async fn observe_enclave_delete_requested_is_unhealthy() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v3/projects/test-proj"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "projectId":      "test-proj",
                "lifecycleState": "DELETE_REQUESTED",
            })))
            .mount(&server)
            .await;

        let obs = driver(&server)
            .observe_enclave(&dummy_enclave(), &json!({ "project_id": "test-proj" }))
            .await
            .unwrap();

        assert!(obs.exists);
        assert!(!obs.healthy);
    }

    #[tokio::test]
    async fn observe_enclave_not_found_returns_exists_false() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v3/projects/test-proj"))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({
                "error": { "code": 404, "status": "NOT_FOUND", "message": "not found" },
            })))
            .mount(&server)
            .await;

        let obs = driver(&server)
            .observe_enclave(&dummy_enclave(), &json!({ "project_id": "test-proj" }))
            .await
            .unwrap();

        assert!(!obs.exists);
        assert!(!obs.healthy);
    }

    // ── observe_partition: Cloud Run ──────────────────────────────────────────

    #[tokio::test]
    async fn observe_partition_cloud_run_ready() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v2/projects/test-proj/locations/us-central1/services/api"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "uri":        "https://api-abc123-uc.a.run.app",
                "conditions": [{ "type": "Ready", "status": "True" }],
            })))
            .mount(&server)
            .await;

        let obs = driver(&server)
            .observe_partition(
                &dummy_enclave(),
                &http_partition(),
                &json!({ "type": "cloud_run", "project_id": "test-proj" }),
            )
            .await
            .unwrap();

        assert!(obs.exists);
        assert!(obs.healthy);
        assert_eq!(obs.outputs["hostname"], "api-abc123-uc.a.run.app");
        assert_eq!(obs.outputs["port"], "443");
    }

    #[tokio::test]
    async fn observe_partition_cloud_run_condition_false_is_unhealthy() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v2/projects/test-proj/locations/us-central1/services/api"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "uri":        "https://api-abc123-uc.a.run.app",
                "conditions": [{ "type": "Ready", "status": "False", "message": "OOM" }],
            })))
            .mount(&server)
            .await;

        let obs = driver(&server)
            .observe_partition(
                &dummy_enclave(),
                &http_partition(),
                &json!({ "type": "cloud_run", "project_id": "test-proj" }),
            )
            .await
            .unwrap();

        assert!(obs.exists);
        assert!(!obs.healthy);
    }

    #[tokio::test]
    async fn observe_partition_cloud_run_not_found() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v2/projects/test-proj/locations/us-central1/services/api"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let obs = driver(&server)
            .observe_partition(
                &dummy_enclave(),
                &http_partition(),
                &json!({ "type": "cloud_run", "project_id": "test-proj" }),
            )
            .await
            .unwrap();

        assert!(!obs.exists);
    }

    // ── observe_partition: Cloud SQL ──────────────────────────────────────────

    #[tokio::test]
    async fn observe_partition_cloud_sql_runnable() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/projects/test-proj/instances/db"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "state": "RUNNABLE",
                "ipAddresses": [{ "type": "PRIVATE", "ipAddress": "10.0.0.5" }],
            })))
            .mount(&server)
            .await;

        let obs = driver(&server)
            .observe_partition(
                &dummy_enclave(),
                &tcp_partition(),
                &json!({ "type": "cloud_sql", "project_id": "test-proj" }),
            )
            .await
            .unwrap();

        assert!(obs.exists);
        assert!(obs.healthy);
        assert_eq!(obs.outputs["hostname"], "10.0.0.5");
        assert_eq!(obs.outputs["port"], "5432");
    }

    #[tokio::test]
    async fn observe_partition_cloud_sql_failed_is_unhealthy() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/projects/test-proj/instances/db"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "state": "FAILED",
                "ipAddresses": [],
            })))
            .mount(&server)
            .await;

        let obs = driver(&server)
            .observe_partition(
                &dummy_enclave(),
                &tcp_partition(),
                &json!({ "type": "cloud_sql", "project_id": "test-proj" }),
            )
            .await
            .unwrap();

        assert!(obs.exists);
        assert!(!obs.healthy);
    }

    #[tokio::test]
    async fn observe_partition_cloud_sql_not_found() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/projects/test-proj/instances/db"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let obs = driver(&server)
            .observe_partition(
                &dummy_enclave(),
                &tcp_partition(),
                &json!({ "type": "cloud_sql", "project_id": "test-proj" }),
            )
            .await
            .unwrap();

        assert!(!obs.exists);
    }

    // ── observe_partition: Pub/Sub ────────────────────────────────────────────

    #[tokio::test]
    async fn observe_partition_pubsub_exists() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/projects/test-proj/topics/queue"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "name": "projects/test-proj/topics/queue",
            })))
            .mount(&server)
            .await;

        let obs = driver(&server)
            .observe_partition(
                &dummy_enclave(),
                &queue_partition(),
                &json!({
                    "type":       "pubsub_topic",
                    "project_id": "test-proj",
                    "topic_name": "projects/test-proj/topics/queue",
                }),
            )
            .await
            .unwrap();

        assert!(obs.exists);
        assert!(obs.healthy);
        assert_eq!(obs.outputs["queue_url"], "projects/test-proj/topics/queue");
    }

    #[tokio::test]
    async fn observe_partition_pubsub_not_found() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/projects/test-proj/topics/queue"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let obs = driver(&server)
            .observe_partition(
                &dummy_enclave(),
                &queue_partition(),
                &json!({
                    "type":       "pubsub_topic",
                    "project_id": "test-proj",
                    "topic_name": "projects/test-proj/topics/queue",
                }),
            )
            .await
            .unwrap();

        assert!(!obs.exists);
    }

    // ── provision_partition: Pub/Sub topic ────────────────────────────────────

    #[tokio::test]
    async fn provision_partition_queue_creates_topic() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/v1/projects/test-proj/topics/queue"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "name": "projects/test-proj/topics/queue",
            })))
            .mount(&server)
            .await;

        let result = driver(&server)
            .provision_partition(&dummy_enclave(), &queue_partition(), &HashMap::new(), None)
            .await
            .unwrap();

        assert_eq!(result.handle["type"], "pubsub_topic");
        assert_eq!(result.outputs["queue_url"], "projects/test-proj/topics/queue");
    }

    #[tokio::test]
    async fn provision_partition_queue_409_is_idempotent_success() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/v1/projects/test-proj/topics/queue"))
            .respond_with(ResponseTemplate::new(409).set_body_json(json!({
                "error": { "code": 409, "status": "ALREADY_EXISTS", "message": "Already exists" },
            })))
            .mount(&server)
            .await;

        let result = driver(&server)
            .provision_partition(&dummy_enclave(), &queue_partition(), &HashMap::new(), None)
            .await
            .unwrap();

        // 409 is treated as success; the known queue_url is still returned.
        assert_eq!(result.outputs["queue_url"], "projects/test-proj/topics/queue");
    }

    // ── provision_partition: Cloud Run ────────────────────────────────────────

    #[tokio::test]
    async fn provision_partition_http_returns_hostname_and_port() {
        let server = MockServer::start().await;

        // POST /services → operation already done
        Mock::given(method("POST"))
            .and(path("/v2/projects/test-proj/locations/us-central1/services"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "name": "operations/cloud-run-create",
                "done": true,
                "response": {},
            })))
            .mount(&server)
            .await;

        // GET service (for URL)
        Mock::given(method("GET"))
            .and(path("/v2/projects/test-proj/locations/us-central1/services/api"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "uri":        "https://api-hash-uc.a.run.app",
                "conditions": [{ "type": "Ready", "status": "True" }],
            })))
            .mount(&server)
            .await;

        let result = driver(&server)
            .provision_partition(&dummy_enclave(), &http_partition(), &HashMap::new(), None)
            .await
            .unwrap();

        assert_eq!(result.handle["type"], "cloud_run");
        assert_eq!(result.outputs["hostname"], "api-hash-uc.a.run.app");
        assert_eq!(result.outputs["port"], "443");
    }

    #[tokio::test]
    async fn provision_partition_http_polls_operation_when_not_done() {
        let server = MockServer::start().await;

        // POST → in-progress operation
        Mock::given(method("POST"))
            .and(path("/v2/projects/test-proj/locations/us-central1/services"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "name": "operations/create-op",
                "done": false,
            })))
            .mount(&server)
            .await;

        // Operation poll → done
        Mock::given(method("GET"))
            .and(path("/v2/operations/create-op"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "name":     "operations/create-op",
                "done":     true,
                "response": {},
            })))
            .mount(&server)
            .await;

        // GET service
        Mock::given(method("GET"))
            .and(path("/v2/projects/test-proj/locations/us-central1/services/api"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "uri":        "https://api-hash-uc.a.run.app",
                "conditions": [{ "type": "Ready", "status": "True" }],
            })))
            .mount(&server)
            .await;

        let result = driver(&server)
            .provision_partition(&dummy_enclave(), &http_partition(), &HashMap::new(), None)
            .await
            .unwrap();

        assert_eq!(result.outputs["hostname"], "api-hash-uc.a.run.app");
    }

    // ── provision_partition: Cloud SQL ────────────────────────────────────────

    #[tokio::test]
    async fn provision_partition_tcp_returns_hostname_and_port() {
        let server = MockServer::start().await;

        // POST /instances → operation
        Mock::given(method("POST"))
            .and(path("/v1/projects/test-proj/instances"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "name": "sql-create-op-001",
                "done": true,
                "response": {},
            })))
            .mount(&server)
            .await;

        // Operation poll
        Mock::given(method("GET"))
            .and(path("/v1/projects/test-proj/operations/sql-create-op-001"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "done": true,
                "response": {},
            })))
            .mount(&server)
            .await;

        // GET instance for private IP
        Mock::given(method("GET"))
            .and(path("/v1/projects/test-proj/instances/db"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "state":       "RUNNABLE",
                "ipAddresses": [{ "type": "PRIVATE", "ipAddress": "10.0.1.10" }],
            })))
            .mount(&server)
            .await;

        let result = driver(&server)
            .provision_partition(&dummy_enclave(), &tcp_partition(), &HashMap::new(), None)
            .await
            .unwrap();

        assert_eq!(result.handle["type"], "cloud_sql");
        assert_eq!(result.outputs["hostname"], "10.0.1.10");
        assert_eq!(result.outputs["port"], "5432");
    }

    // ── provision_import: queue subscription ──────────────────────────────────

    #[tokio::test]
    async fn provision_import_queue_creates_subscription() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/v1/projects/importer-proj/subscriptions/my-alias"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "name": "projects/importer-proj/subscriptions/my-alias",
            })))
            .mount(&server)
            .await;

        let importer = Enclave {
            id:         EnclaveId::new("importer-proj"),
            name:       "Importer".into(),
            cloud:      CloudTarget::Local,
            region:     "us-central1".into(),
            identity:   None,
            network:    None,
            dns:        None,
            imports:    vec![],
            exports:    vec![],
            partitions: vec![],
        };
        let import = Import {
            from:        EnclaveId::new("exporter-proj"),
            export_name: "events".into(),
            alias:       "my-alias".into(),
        };
        let export_handle = json!({
            "type":    "queue",
            "topic":   "projects/exporter-proj/topics/events",
            "outputs": { "queue_url": "projects/exporter-proj/topics/events" },
        });

        let d      = GcpDriver::with_static_token(test_config(), "fake", test_base(&server.uri()));
        let result = d.provision_import(&importer, &import, &export_handle, None).await.unwrap();

        assert_eq!(result.handle["type"], "queue");
        assert_eq!(
            result.outputs["queue_url"],
            "projects/importer-proj/subscriptions/my-alias"
        );
    }

    // ── provision_enclave (full sequence) ─────────────────────────────────────

    #[tokio::test]
    async fn provision_enclave_full_sequence() {
        let server = MockServer::start().await;

        // 1. POST /v3/projects → operation
        Mock::given(method("POST"))
            .and(path("/v3/projects"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "name": "operations/proj-create-op",
                "done": false,
            })))
            .mount(&server)
            .await;

        // Poll project operation
        Mock::given(method("GET"))
            .and(path("/v3/operations/proj-create-op"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "done":     true,
                "response": { "projectNumber": "123456789012" },
            })))
            .mount(&server)
            .await;

        // 2. PUT billing
        Mock::given(method("PUT"))
            .and(path("/v1/projects/test-proj/billingInfo"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&server)
            .await;

        // 3. POST serviceusage batchEnable → operation already done
        Mock::given(method("POST"))
            .and(path("/v1/projects/test-proj/services:batchEnable"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "name": "operations/api-enable-op",
                "done": true,
                "response": {},
            })))
            .mount(&server)
            .await;

        // serviceusage operation poll (hit if done=false, but won't be called here)
        Mock::given(method("GET"))
            .and(path("/v1/operations/api-enable-op"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "done": true, "response": {},
            })))
            .mount(&server)
            .await;

        // 4. POST /v1/projects/test-proj/serviceAccounts
        Mock::given(method("POST"))
            .and(path("/v1/projects/test-proj/serviceAccounts"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "email": "test-proj@test-proj.iam.gserviceaccount.com",
                "name":  "projects/test-proj/serviceAccounts/test-proj",
            })))
            .mount(&server)
            .await;

        let result = driver(&server)
            .provision_enclave(&dummy_enclave(), None)
            .await
            .unwrap();

        assert_eq!(result.handle["driver"],       "gcp");
        assert_eq!(result.handle["kind"],         "enclave");
        assert_eq!(result.handle["project_id"],   "test-proj");
        assert_eq!(result.handle["project_number"], "123456789012");
        assert_eq!(
            result.handle["service_account_email"],
            "test-proj@test-proj.iam.gserviceaccount.com"
        );
    }

    #[tokio::test]
    async fn provision_enclave_idempotent_when_existing_handle_project_exists() {
        let server = MockServer::start().await;

        // GET existing project → 200 (still active)
        Mock::given(method("GET"))
            .and(path("/v3/projects/test-proj"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "projectId":      "test-proj",
                "lifecycleState": "ACTIVE",
            })))
            .mount(&server)
            .await;

        let existing_handle = json!({
            "driver":     "gcp",
            "kind":       "enclave",
            "project_id": "test-proj",
        });

        let result = driver(&server)
            .provision_enclave(&dummy_enclave(), Some(&existing_handle))
            .await
            .unwrap();

        // Should return the same handle without creating anything new
        assert_eq!(result.handle["project_id"], "test-proj");
    }
}
