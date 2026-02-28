use std::collections::{BTreeMap, HashMap};
use std::process::Command as StdCommand;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use hmac::{Hmac, Mac};
use nclav_domain::{Enclave, Export, ExportType, Import, Partition};
use quick_xml::{events::Event as XmlEvent, Reader as XmlReader};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tracing::{debug, info, warn};

use crate::driver::{Driver, ObservedState, OrphanedResource, ProvisionResult};
use crate::error::DriverError;
use crate::Handle;

type HmacSha256 = Hmac<Sha256>;

// ── Configuration ─────────────────────────────────────────────────────────────

/// Static configuration for the AWS driver, injected at startup.
#[derive(Clone)]
pub struct AwsDriverConfig {
    /// AWS Organizations OU ID where new accounts will be placed.
    /// Format: "ou-xxxx-yyyyyyyy"
    pub org_unit_id: String,
    /// Email domain for new account emails.
    /// New accounts get address: aws+{account-name}@{email_domain}
    pub email_domain: String,
    /// Default AWS region for new resources. e.g. "us-east-1"
    pub default_region: String,
    /// Optional prefix prepended to every account name.
    pub account_prefix: Option<String>,
    /// IAM role name that nclav assumes in each enclave account.
    /// Default: "OrganizationAccountAccessRole"
    pub cross_account_role: String,
    /// Optional: assume this role ARN for management API calls.
    pub role_arn: Option<String>,
}

// ── Base URLs (overridden in tests) ───────────────────────────────────────────

#[derive(Clone)]
pub(crate) struct BaseUrls {
    pub(crate) organizations: String,
    pub(crate) sts:           String,
    pub(crate) ec2:           String,
    pub(crate) iam:           String,
    pub(crate) route53:       String,
    pub(crate) tagging:       String,
}

impl BaseUrls {
    fn for_region(region: &str) -> Self {
        Self {
            organizations: "https://organizations.us-east-1.amazonaws.com".into(),
            sts:           "https://sts.amazonaws.com".into(),
            ec2:           format!("https://ec2.{}.amazonaws.com", region),
            iam:           "https://iam.amazonaws.com".into(),
            route53:       "https://route53.amazonaws.com".into(),
            tagging:       format!("https://tagging.{}.amazonaws.com", region),
        }
    }
}

// ── Credentials ───────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct AwsCredentials {
    access_key_id:     String,
    secret_access_key: String,
    session_token:     Option<String>,
}

#[async_trait]
trait CredentialsProvider: Send + Sync {
    async fn credentials(&self) -> Result<AwsCredentials, DriverError>;
}

// ── Static credentials (env vars / config) ────────────────────────────────────

struct StaticCredentialsProvider {
    access_key_id:     String,
    secret_access_key: String,
    session_token:     Option<String>,
}

#[async_trait]
impl CredentialsProvider for StaticCredentialsProvider {
    async fn credentials(&self) -> Result<AwsCredentials, DriverError> {
        Ok(AwsCredentials {
            access_key_id:     self.access_key_id.clone(),
            secret_access_key: self.secret_access_key.clone(),
            session_token:     self.session_token.clone(),
        })
    }
}

// ── IMDS / ECS credentials ────────────────────────────────────────────────────

struct ImdsCredentialsProvider {
    client: reqwest::Client,
    ecs_uri: Option<String>, // set when AWS_CONTAINER_CREDENTIALS_RELATIVE_URI is present
    cache:  tokio::sync::Mutex<Option<(AwsCredentials, Instant)>>,
}

#[async_trait]
impl CredentialsProvider for ImdsCredentialsProvider {
    async fn credentials(&self) -> Result<AwsCredentials, DriverError> {
        {
            let guard = self.cache.lock().await;
            if let Some((creds, expiry)) = guard.as_ref() {
                if Instant::now() < *expiry {
                    return Ok(creds.clone());
                }
            }
        }

        let creds = if let Some(ref uri) = self.ecs_uri {
            // ECS task metadata credentials
            let url = format!("http://169.254.170.2{}", uri);
            let resp: Value = self
                .client
                .get(&url)
                .send()
                .await
                .map_err(|e| DriverError::Internal(format!("ECS IMDS request: {}", e)))?
                .json()
                .await
                .map_err(|e| DriverError::Internal(format!("ECS IMDS decode: {}", e)))?;

            AwsCredentials {
                access_key_id:     resp["AccessKeyId"].as_str().unwrap_or("").to_string(),
                secret_access_key: resp["SecretAccessKey"].as_str().unwrap_or("").to_string(),
                session_token:     resp["Token"].as_str().map(str::to_string),
            }
        } else {
            // EC2 IMDSv2
            let token_resp = self
                .client
                .put("http://169.254.169.254/latest/api/token")
                .header("X-aws-ec2-metadata-token-ttl-seconds", "21600")
                .send()
                .await
                .map_err(|e| DriverError::Internal(format!("IMDSv2 token request: {}", e)))?;
            let imds_token = token_resp
                .text()
                .await
                .map_err(|e| DriverError::Internal(format!("IMDSv2 token decode: {}", e)))?;

            // Get role name
            let roles_resp = self
                .client
                .get("http://169.254.169.254/latest/meta-data/iam/security-credentials/")
                .header("X-aws-ec2-metadata-token", &imds_token)
                .send()
                .await
                .map_err(|e| DriverError::Internal(format!("IMDS roles request: {}", e)))?;
            let roles_text = roles_resp
                .text()
                .await
                .unwrap_or_default();
            let role_name = roles_text.lines().next().unwrap_or("").to_string();
            if role_name.is_empty() {
                return Err(DriverError::Internal("IMDS: no IAM role found".into()));
            }

            // Get credentials for the role
            let creds_url = format!(
                "http://169.254.169.254/latest/meta-data/iam/security-credentials/{}",
                role_name
            );
            let resp: Value = self
                .client
                .get(&creds_url)
                .header("X-aws-ec2-metadata-token", &imds_token)
                .send()
                .await
                .map_err(|e| DriverError::Internal(format!("IMDS creds request: {}", e)))?
                .json()
                .await
                .map_err(|e| DriverError::Internal(format!("IMDS creds decode: {}", e)))?;

            AwsCredentials {
                access_key_id:     resp["AccessKeyId"].as_str().unwrap_or("").to_string(),
                secret_access_key: resp["SecretAccessKey"].as_str().unwrap_or("").to_string(),
                session_token:     resp["Token"].as_str().map(str::to_string),
            }
        };

        // Cache for 10 minutes (credentials expire after 6 hours typically)
        let expiry = Instant::now() + Duration::from_secs(600);
        *self.cache.lock().await = Some((creds.clone(), expiry));
        Ok(creds)
    }
}

// ── AWS CLI credentials ───────────────────────────────────────────────────────

struct AwsCliCredentialsProvider;

#[async_trait]
impl CredentialsProvider for AwsCliCredentialsProvider {
    async fn credentials(&self) -> Result<AwsCredentials, DriverError> {
        let output = StdCommand::new("aws")
            .args(["sts", "get-session-token", "--duration-seconds", "3600", "--output", "json"])
            .output()
            .map_err(|e| DriverError::Internal(format!(
                "aws CLI not found: {}. Install AWS CLI or configure credentials via env vars.",
                e
            )))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(DriverError::Internal(format!(
                "aws sts get-session-token failed: {}. Run 'aws configure' first.",
                stderr.trim()
            )));
        }

        let resp: Value = serde_json::from_slice(&output.stdout)
            .map_err(|e| DriverError::Internal(format!("aws CLI output parse: {}", e)))?;

        let creds = &resp["Credentials"];
        Ok(AwsCredentials {
            access_key_id:     creds["AccessKeyId"].as_str().unwrap_or("").to_string(),
            secret_access_key: creds["SecretAccessKey"].as_str().unwrap_or("").to_string(),
            session_token:     creds["SessionToken"].as_str().map(str::to_string),
        })
    }
}

// ── Static credentials (test-only) ───────────────────────────────────────────

#[cfg(test)]
pub struct StaticCredentials {
    pub access_key_id:     String,
    pub secret_access_key: String,
    pub session_token:     Option<String>,
}

#[cfg(test)]
#[async_trait]
impl CredentialsProvider for StaticCredentials {
    async fn credentials(&self) -> Result<AwsCredentials, DriverError> {
        Ok(AwsCredentials {
            access_key_id:     self.access_key_id.clone(),
            secret_access_key: self.secret_access_key.clone(),
            session_token:     self.session_token.clone(),
        })
    }
}

// ── SigV4 signing ─────────────────────────────────────────────────────────────

fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect()
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn derive_signing_key(secret: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    let k_date    = hmac_sha256(format!("AWS4{}", secret).as_bytes(), date.as_bytes());
    let k_region  = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    hmac_sha256(&k_service, b"aws4_request")
}

/// Extract the hostname from a URL (scheme://host/path → host).
fn url_host(url: &str) -> &str {
    let without_scheme = url
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    match without_scheme.find('/') {
        Some(pos) => &without_scheme[..pos],
        None      => without_scheme,
    }
}

/// Build SigV4 request headers for an AWS API call.
///
/// Returns a `BTreeMap` of headers to add to the request.
/// Caller must also set `Content-Type` and `Host`.
fn sigv4_headers(
    method:       &str,
    uri_path:     &str,
    query_string: &str,
    content_type: &str,
    body:         &[u8],
    creds:        &AwsCredentials,
    region:       &str,
    service:      &str,
    host:         &str,
) -> BTreeMap<String, String> {
    let now       = chrono::Utc::now();
    let timestamp = now.format("%Y%m%dT%H%M%SZ").to_string();
    let date      = now.format("%Y%m%d").to_string();

    let payload_hash = sha256_hex(body);

    // Canonical headers (must be sorted and lowercased)
    let mut canon_hdrs: BTreeMap<String, String> = BTreeMap::new();
    canon_hdrs.insert("content-type".into(), content_type.into());
    canon_hdrs.insert("host".into(), host.into());
    canon_hdrs.insert("x-amz-content-sha256".into(), payload_hash.clone());
    canon_hdrs.insert("x-amz-date".into(), timestamp.clone());
    if let Some(ref token) = creds.session_token {
        canon_hdrs.insert("x-amz-security-token".into(), token.clone());
    }

    let signed_headers: String = canon_hdrs.keys().cloned().collect::<Vec<_>>().join(";");
    let canonical_headers: String = canon_hdrs
        .iter()
        .map(|(k, v)| format!("{}:{}\n", k, v.trim()))
        .collect();

    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        method, uri_path, query_string,
        canonical_headers, signed_headers, payload_hash
    );

    let scope = format!("{}/{}/{}/aws4_request", date, region, service);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{}\n{}",
        timestamp, scope, sha256_hex(canonical_request.as_bytes())
    );

    let signing_key = derive_signing_key(&creds.secret_access_key, &date, region, service);
    let signature   = hmac_sha256(&signing_key, string_to_sign.as_bytes())
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<String>();

    let auth = format!(
        "AWS4-HMAC-SHA256 Credential={}/{},SignedHeaders={},Signature={}",
        creds.access_key_id, scope, signed_headers, signature
    );

    let mut out = BTreeMap::new();
    out.insert("Authorization".into(), auth);
    out.insert("x-amz-date".into(), timestamp);
    out.insert("x-amz-content-sha256".into(), payload_hash);
    if let Some(ref token) = creds.session_token {
        out.insert("x-amz-security-token".into(), token.clone());
    }
    out
}

// ── XML helpers ───────────────────────────────────────────────────────────────

/// Find the text content of the first `<tag>…</tag>` element in XML.
/// Skips over nested elements; returns `None` if not found or empty.
fn xml_text(xml: &str, tag: &str) -> Option<String> {
    let tag_bytes   = tag.as_bytes();
    let mut reader  = XmlReader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut in_tag  = false;
    let mut depth:  usize = 0;

    loop {
        match reader.read_event() {
            Ok(XmlEvent::Start(e)) => {
                if !in_tag && e.local_name().as_ref() == tag_bytes {
                    in_tag = true;
                    depth  = 0;
                } else if in_tag {
                    depth += 1;
                }
            }
            Ok(XmlEvent::End(_)) => {
                if in_tag {
                    if depth == 0 { return None; }
                    depth -= 1;
                }
            }
            Ok(XmlEvent::Text(e)) if in_tag && depth == 0 => {
                return e.unescape().ok().map(|s| s.into_owned());
            }
            Ok(XmlEvent::Eof) | Err(_) => break,
            _ => {}
        }
    }
    None
}

/// Collect text content of every `<tag>…</tag>` element in XML.
fn xml_all_texts(xml: &str, tag: &str) -> Vec<String> {
    let tag_bytes  = tag.as_bytes();
    let mut reader = XmlReader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut result = Vec::new();
    let mut depth: usize = 0; // 0 = not in tag

    loop {
        match reader.read_event() {
            Ok(XmlEvent::Start(e)) => {
                if depth == 0 && e.local_name().as_ref() == tag_bytes {
                    depth = 1;
                } else if depth > 0 {
                    depth += 1;
                }
            }
            Ok(XmlEvent::End(_)) => {
                if depth > 0 { depth -= 1; }
            }
            Ok(XmlEvent::Text(e)) if depth == 1 => {
                if let Ok(s) = e.unescape() {
                    result.push(s.into_owned());
                }
            }
            Ok(XmlEvent::Eof) | Err(_) => break,
            _ => {}
        }
    }
    result
}

/// Parse the AWS error code from an XML error response.
fn xml_error_code(xml: &str) -> String {
    xml_text(xml, "Code")
        .or_else(|| xml_text(xml, "code"))
        .unwrap_or_else(|| "Unknown".into())
}

/// Parse the AWS error message from an XML error response.
fn xml_error_message(xml: &str) -> String {
    xml_text(xml, "Message")
        .or_else(|| xml_text(xml, "message"))
        .unwrap_or_else(|| "unknown error".into())
}

// ── Name helpers ──────────────────────────────────────────────────────────────

/// Sanitize a string into a valid AWS account name (max 50 chars, alphanumeric + space + hyphen).
fn sanitize_account_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_alphanumeric() || c == ' ' || c == '-' { c } else { '-' })
        .collect();
    let trimmed: String = cleaned
        .trim_matches(|c: char| !c.is_alphanumeric())
        .to_string();
    if trimmed.len() > 50 {
        trimmed[..50].to_string()
    } else {
        trimmed
    }
}

/// Derive the IAM role name for a partition (max 64 chars).
/// Format: "nclav-partition-{id}" truncated + hex hash if needed.
fn partition_role_name(partition_id: &str) -> String {
    let prefix = "nclav-partition-";
    let base   = format!("{}{}", prefix, partition_id);
    if base.len() <= 64 {
        return base;
    }
    // Truncate and append 8-char hex hash of the partition ID
    let mut hasher = Sha256::new();
    hasher.update(partition_id.as_bytes());
    let hash: String = hasher.finalize()[..4]
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect();
    let max_id_len = 64 - prefix.len() - 1 - hash.len();
    format!("{}{}-{}", prefix, &partition_id[..max_id_len], hash)
}

// ── AwsDriver ─────────────────────────────────────────────────────────────────

pub struct AwsDriver {
    config: AwsDriverConfig,
    client: reqwest::Client,
    creds:  Box<dyn CredentialsProvider>,
    base:   BaseUrls,
}

impl AwsDriver {
    /// Create an `AwsDriver`, auto-selecting the credentials provider:
    /// 1. `role_arn` in config → assume role with ambient creds
    /// 2. Env vars `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY`
    /// 3. `AWS_CONTAINER_CREDENTIALS_RELATIVE_URI` (ECS task)
    /// 4. EC2 IMDSv2
    /// 5. AWS CLI fallback
    pub async fn new(config: AwsDriverConfig) -> Result<Self, DriverError> {
        let client = reqwest::Client::new();
        let base   = BaseUrls::for_region(&config.default_region);

        let creds: Box<dyn CredentialsProvider> = if let (Ok(key), Ok(secret)) = (
            std::env::var("AWS_ACCESS_KEY_ID"),
            std::env::var("AWS_SECRET_ACCESS_KEY"),
        ) {
            Box::new(StaticCredentialsProvider {
                access_key_id:     key,
                secret_access_key: secret,
                session_token:     std::env::var("AWS_SESSION_TOKEN").ok(),
            })
        } else if let Ok(uri) = std::env::var("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI") {
            Box::new(ImdsCredentialsProvider {
                client: client.clone(),
                ecs_uri: Some(uri),
                cache:   tokio::sync::Mutex::new(None),
            })
        } else {
            // Try IMDS; fall back to CLI. We probe IMDS with a short timeout.
            let imds_probe = client
                .get("http://169.254.169.254/latest/api/token")
                .header("X-aws-ec2-metadata-token-ttl-seconds", "10")
                .timeout(Duration::from_secs(2))
                .send()
                .await;
            if imds_probe.is_ok() {
                Box::new(ImdsCredentialsProvider {
                    client: client.clone(),
                    ecs_uri: None,
                    cache:   tokio::sync::Mutex::new(None),
                })
            } else {
                Box::new(AwsCliCredentialsProvider)
            }
        };

        Ok(Self { config, client, creds, base })
    }

    /// Create an `AwsDriver` with injected credentials and base URLs.
    /// Used exclusively in tests.
    #[cfg(test)]
    pub(crate) fn with_test_config(
        config: AwsDriverConfig,
        base: BaseUrls,
        creds: impl CredentialsProvider + 'static,
    ) -> Self {
        Self {
            config,
            client: reqwest::Client::new(),
            creds:  Box::new(creds),
            base,
        }
    }

    async fn get_creds(&self) -> Result<AwsCredentials, DriverError> {
        self.creds.credentials().await
    }

    // ── AWS Query API (EC2, IAM, STS, Route53-Query) ──────────────────────────

    /// POST an AWS Query-protocol request, return the raw XML response text.
    async fn query_api(
        &self,
        base_url:  &str,
        region:    &str,
        service:   &str,
        creds:     &AwsCredentials,
        params:    &[(&str, &str)],
    ) -> Result<String, DriverError> {
        let host = url_host(base_url).to_string();
        let url  = format!("{}/", base_url.trim_end_matches('/'));

        // Build form-encoded body
        let body_str = params
            .iter()
            .map(|(k, v)| format!("{}={}", k, urlencoding::encode(v)))
            .collect::<Vec<_>>()
            .join("&");
        let body_bytes = body_str.as_bytes();

        let ct = "application/x-www-form-urlencoded; charset=utf-8";
        let sig_headers = sigv4_headers(
            "POST", "/", "", ct, body_bytes, creds, region, service, &host,
        );

        let mut req = self
            .client
            .post(&url)
            .header("Content-Type", ct)
            .body(body_bytes.to_vec());
        for (k, v) in &sig_headers {
            req = req.header(k, v);
        }

        let resp = req
            .send()
            .await
            .map_err(|e| DriverError::Internal(format!("POST {} failed: {}", url, e)))?;

        let status = resp.status().as_u16();
        let text   = resp.text().await.unwrap_or_default();

        if status >= 400 {
            let code = xml_error_code(&text);
            let msg  = xml_error_message(&text);
            return Err(DriverError::ProvisionFailed(format!(
                "{}: {} — {}", base_url, code, msg
            )));
        }
        Ok(text)
    }

    /// POST an AWS query request using the enclave's assumed-role credentials.
    async fn query_api_with(
        &self,
        base_url: &str,
        region:   &str,
        service:  &str,
        creds:    &AwsCredentials,
        params:   &[(&str, &str)],
    ) -> Result<String, DriverError> {
        self.query_api(base_url, region, service, creds, params).await
    }

    // ── AWS JSON/Target API (Organizations, ResourceGroupsTagging) ────────────

    /// POST an AWS JSON-protocol request, return the parsed JSON response.
    async fn json_api(
        &self,
        base_url: &str,
        region:   &str,
        service:  &str,
        target:   &str,
        creds:    &AwsCredentials,
        body:     &Value,
    ) -> Result<Value, DriverError> {
        let host      = url_host(base_url).to_string();
        let url       = format!("{}/", base_url.trim_end_matches('/'));
        let body_str  = serde_json::to_string(body).unwrap_or_default();
        let body_bytes = body_str.as_bytes();
        let ct        = "application/x-amz-json-1.1";

        let mut sig_headers = sigv4_headers(
            "POST", "/", "", ct, body_bytes, creds, region, service, &host,
        );
        sig_headers.insert("X-Amz-Target".into(), target.into());

        let mut req = self
            .client
            .post(&url)
            .header("Content-Type", ct)
            .header("X-Amz-Target", target)
            .body(body_bytes.to_vec());
        for (k, v) in &sig_headers {
            req = req.header(k, v);
        }

        let resp = req
            .send()
            .await
            .map_err(|e| DriverError::Internal(format!("POST {} failed: {}", url, e)))?;

        let status    = resp.status().as_u16();
        let resp_body: Value = resp.json().await.unwrap_or(Value::Null);

        if status >= 400 {
            let error_type = resp_body["__type"]
                .as_str()
                .unwrap_or("Unknown");
            let msg = resp_body["message"]
                .as_str()
                .or_else(|| resp_body["Message"].as_str())
                .unwrap_or("unknown error");
            return Err(DriverError::ProvisionFailed(format!(
                "{} [{}]: {} — {}", base_url, target, error_type, msg
            )));
        }
        Ok(resp_body)
    }

    // ── Route53 (REST XML) ────────────────────────────────────────────────────

    /// POST to Route53 REST XML API, returning the response body.
    async fn route53_post(
        &self,
        path:  &str,
        creds: &AwsCredentials,
        xml:   &str,
    ) -> Result<String, DriverError> {
        let base = &self.base.route53;
        let host = url_host(base).to_string();
        let url  = format!("{}{}", base.trim_end_matches('/'), path);
        let ct   = "text/xml; charset=utf-8";
        let body = xml.as_bytes();

        let sig_headers = sigv4_headers(
            "POST", path, "", ct, body, creds, "us-east-1", "route53", &host,
        );

        let mut req = self
            .client
            .post(&url)
            .header("Content-Type", ct)
            .body(body.to_vec());
        for (k, v) in &sig_headers {
            req = req.header(k, v);
        }

        let resp   = req.send().await
            .map_err(|e| DriverError::Internal(format!("Route53 POST {}: {}", path, e)))?;
        let status = resp.status().as_u16();
        let text   = resp.text().await.unwrap_or_default();

        if status >= 400 {
            let code = xml_error_code(&text);
            let msg  = xml_error_message(&text);
            return Err(DriverError::ProvisionFailed(format!(
                "Route53 {}: {} — {}", path, code, msg
            )));
        }
        Ok(text)
    }

    // ── STS AssumeRole ────────────────────────────────────────────────────────

    /// Assume an IAM role via STS, return temporary credentials.
    async fn sts_assume_role(
        &self,
        creds:        &AwsCredentials,
        role_arn:     &str,
        session_name: &str,
    ) -> Result<AwsCredentials, DriverError> {
        debug!(role_arn, session_name, "STS AssumeRole");
        let xml = self.query_api(
            &self.base.sts,
            "us-east-1",
            "sts",
            creds,
            &[
                ("Action",          "AssumeRole"),
                ("Version",         "2011-06-15"),
                ("RoleArn",         role_arn),
                ("RoleSessionName", session_name),
                ("DurationSeconds", "3600"),
            ],
        ).await?;

        let key_id = xml_text(&xml, "AccessKeyId")
            .ok_or_else(|| DriverError::Internal("STS AssumeRole: no AccessKeyId".into()))?;
        let secret = xml_text(&xml, "SecretAccessKey")
            .ok_or_else(|| DriverError::Internal("STS AssumeRole: no SecretAccessKey".into()))?;
        let token  = xml_text(&xml, "SessionToken");

        Ok(AwsCredentials {
            access_key_id:     key_id,
            secret_access_key: secret,
            session_token:     token,
        })
    }

    /// Get credentials for the cross-account role in an enclave account.
    async fn enclave_creds(&self, account_id: &str) -> Result<AwsCredentials, DriverError> {
        let base_creds = self.get_creds().await?;
        let role_arn   = format!(
            "arn:aws:iam::{}:role/{}",
            account_id, self.config.cross_account_role
        );
        self.sts_assume_role(&base_creds, &role_arn, "nclav-session").await
    }

    // ── Account naming ────────────────────────────────────────────────────────

    fn account_name(&self, enclave_id: &str) -> String {
        let raw = match &self.config.account_prefix {
            Some(p) if !p.is_empty() => format!("{}-{}", p, enclave_id),
            _                        => enclave_id.to_string(),
        };
        sanitize_account_name(&raw)
    }

    fn account_email(&self, account_name: &str) -> String {
        let clean = account_name.replace(' ', "").to_lowercase();
        format!("aws+{}@{}", clean, self.config.email_domain)
    }

    // ── Organizations helpers ─────────────────────────────────────────────────

    async fn org_create_account(
        &self,
        creds:        &AwsCredentials,
        account_name: &str,
        email:        &str,
    ) -> Result<String, DriverError> {
        info!(account_name, email, "Organizations: CreateAccount");
        let resp = self.json_api(
            &self.base.organizations,
            "us-east-1",
            "organizations",
            "AmazonOrganizationsV20161128.CreateAccount",
            creds,
            &json!({ "AccountName": account_name, "Email": email }),
        ).await;

        match resp {
            Ok(v) => {
                let req_id = v["CreateAccountStatus"]["Id"]
                    .as_str()
                    .ok_or_else(|| DriverError::ProvisionFailed(
                        "CreateAccount: no CreateAccountStatus.Id in response".into()
                    ))?
                    .to_string();
                Ok(req_id)
            }
            Err(e) if e.to_string().contains("DuplicateAccountException") => {
                // Account already exists — look it up by email
                Err(DriverError::ProvisionFailed(format!(
                    "Account '{}' already exists but no account ID in state. \
                     Set provisioning_complete in the enclave handle to recover. \
                     Original error: {}", account_name, e
                )))
            }
            Err(e) => Err(e),
        }
    }

    /// Poll DescribeCreateAccountStatus until Succeeded or error.
    /// Returns the new account ID.
    async fn org_wait_for_account(
        &self,
        creds:  &AwsCredentials,
        req_id: &str,
    ) -> Result<String, DriverError> {
        let backoff = [1u64, 2, 4, 8, 16, 30];
        let max_polls = 120;

        for (i, &delay) in backoff.iter().cycle().take(max_polls).enumerate() {
            let resp = self.json_api(
                &self.base.organizations,
                "us-east-1",
                "organizations",
                "AmazonOrganizationsV20161128.DescribeCreateAccountStatus",
                creds,
                &json!({ "CreateAccountRequestId": req_id }),
            ).await?;

            let status = resp["CreateAccountStatus"]["State"]
                .as_str()
                .unwrap_or("UNKNOWN");

            match status {
                "SUCCEEDED" => {
                    return resp["CreateAccountStatus"]["AccountId"]
                        .as_str()
                        .ok_or_else(|| DriverError::ProvisionFailed(
                            "DescribeCreateAccountStatus: no AccountId in Succeeded response".into()
                        ))
                        .map(str::to_string);
                }
                "FAILED" => {
                    let reason = resp["CreateAccountStatus"]["FailureReason"]
                        .as_str()
                        .unwrap_or("unknown");
                    return Err(DriverError::ProvisionFailed(format!(
                        "CreateAccount failed: {}", reason
                    )));
                }
                _ => {} // IN_PROGRESS or other
            }

            let poll = i + 1;
            if poll % 10 == 0 {
                info!(poll, req_id, "still waiting for AWS account creation");
            } else {
                debug!(poll, req_id, delay, "AWS account creation pending, waiting");
            }
            tokio::time::sleep(Duration::from_secs(delay)).await;
        }

        Err(DriverError::ProvisionFailed(format!(
            "AWS account creation timed out after {} polls (request: {})",
            max_polls, req_id
        )))
    }

    async fn org_list_parents(
        &self,
        creds:      &AwsCredentials,
        account_id: &str,
    ) -> Result<String, DriverError> {
        let resp = self.json_api(
            &self.base.organizations,
            "us-east-1",
            "organizations",
            "AmazonOrganizationsV20161128.ListParents",
            creds,
            &json!({ "ChildId": account_id }),
        ).await?;

        resp["Parents"]
            .as_array()
            .and_then(|a| a.first())
            .and_then(|p| p["Id"].as_str())
            .map(str::to_string)
            .ok_or_else(|| DriverError::ProvisionFailed(
                format!("ListParents for {}: no parent found", account_id)
            ))
    }

    async fn org_move_account(
        &self,
        creds:            &AwsCredentials,
        account_id:       &str,
        source_parent_id: &str,
        dest_parent_id:   &str,
    ) -> Result<(), DriverError> {
        let result = self.json_api(
            &self.base.organizations,
            "us-east-1",
            "organizations",
            "AmazonOrganizationsV20161128.MoveAccount",
            creds,
            &json!({
                "AccountId":          account_id,
                "SourceParentId":     source_parent_id,
                "DestinationParentId": dest_parent_id,
            }),
        ).await;

        match result {
            Ok(_) => Ok(()),
            Err(e) if e.to_string().contains("DuplicateAccountException")
                   || e.to_string().contains("AccountAlreadyInOrganizationException") => {
                info!(account_id, dest_parent_id, "Account already in target OU");
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    // ── EC2 helpers ───────────────────────────────────────────────────────────

    async fn ec2_create_vpc(
        &self,
        creds:    &AwsCredentials,
        region:   &str,
        cidr:     &str,
        enc_id:   &str,
    ) -> Result<String, DriverError> {
        info!(cidr, region, "EC2: CreateVpc");
        let xml = self.query_api_with(
            &self.base.ec2,
            region,
            "ec2",
            creds,
            &[
                ("Action", "CreateVpc"),
                ("Version", "2016-11-15"),
                ("CidrBlock", cidr),
                ("TagSpecification.1.ResourceType", "vpc"),
                ("TagSpecification.1.Tag.1.Key", "Name"),
                ("TagSpecification.1.Tag.1.Value", &format!("nclav-{}", enc_id)),
                ("TagSpecification.1.Tag.2.Key", "nclav-managed"),
                ("TagSpecification.1.Tag.2.Value", "true"),
                ("TagSpecification.1.Tag.3.Key", "nclav-enclave"),
                ("TagSpecification.1.Tag.3.Value", enc_id),
            ],
        ).await?;

        xml_text(&xml, "vpcId")
            .ok_or_else(|| DriverError::ProvisionFailed("EC2 CreateVpc: no vpcId in response".into()))
    }

    async fn ec2_modify_vpc_attribute(
        &self,
        creds:  &AwsCredentials,
        region: &str,
        vpc_id: &str,
        attr:   &str,
        value:  &str,
    ) -> Result<(), DriverError> {
        self.query_api_with(
            &self.base.ec2,
            region,
            "ec2",
            creds,
            &[
                ("Action", "ModifyVpcAttribute"),
                ("Version", "2016-11-15"),
                ("VpcId", vpc_id),
                (attr, value),
            ],
        ).await.map(|_| ())
    }

    async fn ec2_create_subnet(
        &self,
        creds:   &AwsCredentials,
        region:  &str,
        vpc_id:  &str,
        cidr:    &str,
        enc_id:  &str,
        idx:     usize,
    ) -> Result<String, DriverError> {
        info!(cidr, vpc_id, "EC2: CreateSubnet");
        let xml = self.query_api_with(
            &self.base.ec2,
            region,
            "ec2",
            creds,
            &[
                ("Action", "CreateSubnet"),
                ("Version", "2016-11-15"),
                ("VpcId", vpc_id),
                ("CidrBlock", cidr),
                ("TagSpecification.1.ResourceType", "subnet"),
                ("TagSpecification.1.Tag.1.Key", "Name"),
                ("TagSpecification.1.Tag.1.Value", &format!("nclav-{}-subnet-{}", enc_id, idx)),
                ("TagSpecification.1.Tag.2.Key", "nclav-managed"),
                ("TagSpecification.1.Tag.2.Value", "true"),
                ("TagSpecification.1.Tag.3.Key", "nclav-enclave"),
                ("TagSpecification.1.Tag.3.Value", enc_id),
            ],
        ).await?;

        xml_text(&xml, "subnetId")
            .ok_or_else(|| DriverError::ProvisionFailed("EC2 CreateSubnet: no subnetId".into()))
    }

    // ── Route53 helpers ───────────────────────────────────────────────────────

    async fn route53_create_hosted_zone(
        &self,
        creds:   &AwsCredentials,
        name:    &str,
        vpc_id:  &str,
        region:  &str,
    ) -> Result<String, DriverError> {
        info!(name, vpc_id, "Route53: CreateHostedZone");
        let caller_ref = format!("nclav-{}-{}", name, chrono::Utc::now().timestamp_millis());
        let xml_body = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<CreateHostedZoneRequest xmlns="https://route53.amazonaws.com/doc/2013-04-01/">
  <Name>{}</Name>
  <CallerReference>{}</CallerReference>
  <HostedZoneConfig>
    <Comment>Managed by nclav</Comment>
    <PrivateZone>true</PrivateZone>
  </HostedZoneConfig>
  <VPC>
    <VPCRegion>{}</VPCRegion>
    <VPCId>{}</VPCId>
  </VPC>
</CreateHostedZoneRequest>"#,
            name, caller_ref, region, vpc_id
        );

        let resp_xml = self.route53_post("/2013-04-01/hostedzone", creds, &xml_body).await?;

        // Extract zone ID: /hostedzone/Z1234567890ABC → Z1234567890ABC
        xml_text(&resp_xml, "Id")
            .map(|id| id.trim_start_matches("/hostedzone/").to_string())
            .ok_or_else(|| DriverError::ProvisionFailed("Route53 CreateHostedZone: no Id".into()))
    }

    // ── IAM helpers ───────────────────────────────────────────────────────────

    async fn iam_create_role(
        &self,
        creds:         &AwsCredentials,
        role_name:     &str,
        trust_policy:  &str,
        enc_id:        &str,
        part_id:       Option<&str>,
    ) -> Result<String, DriverError> {
        info!(role_name, "IAM: CreateRole");
        let mut params = vec![
            ("Action", "CreateRole"),
            ("Version", "2010-05-08"),
            ("RoleName", role_name),
            ("AssumeRolePolicyDocument", trust_policy),
            ("Tags.member.1.Key", "nclav-managed"),
            ("Tags.member.1.Value", "true"),
            ("Tags.member.2.Key", "nclav-enclave"),
            ("Tags.member.2.Value", enc_id),
        ];
        let part_tag_key   = "Tags.member.3.Key".to_string();
        let part_tag_val   = "nclav-partition".to_string();
        let part_tag_keyv  = "Tags.member.3.Value".to_string();
        if let Some(pid) = part_id {
            params.push((&part_tag_key, &part_tag_val));
            params.push((&part_tag_keyv, pid));
        }

        let xml = self.query_api_with(
            &self.base.iam, "us-east-1", "iam", creds, &params,
        ).await;

        match xml {
            Ok(xml) => {
                xml_text(&xml, "Arn")
                    .ok_or_else(|| DriverError::ProvisionFailed("IAM CreateRole: no Arn".into()))
            }
            Err(e) if e.to_string().contains("EntityAlreadyExists") => {
                info!(role_name, "IAM role already exists, retrieving ARN");
                self.iam_get_role_arn(creds, role_name).await
            }
            Err(e) => Err(e),
        }
    }

    async fn iam_get_role_arn(
        &self,
        creds:     &AwsCredentials,
        role_name: &str,
    ) -> Result<String, DriverError> {
        let xml = self.query_api_with(
            &self.base.iam,
            "us-east-1",
            "iam",
            creds,
            &[
                ("Action", "GetRole"),
                ("Version", "2010-05-08"),
                ("RoleName", role_name),
            ],
        ).await?;

        xml_text(&xml, "Arn")
            .ok_or_else(|| DriverError::ProvisionFailed(format!("IAM GetRole {}: no Arn", role_name)))
    }

    async fn iam_attach_role_policy(
        &self,
        creds:      &AwsCredentials,
        role_name:  &str,
        policy_arn: &str,
    ) -> Result<(), DriverError> {
        self.query_api_with(
            &self.base.iam,
            "us-east-1",
            "iam",
            creds,
            &[
                ("Action", "AttachRolePolicy"),
                ("Version", "2010-05-08"),
                ("RoleName", role_name),
                ("PolicyArn", policy_arn),
            ],
        ).await.map(|_| ())
    }

    async fn iam_detach_all_policies(
        &self,
        creds:     &AwsCredentials,
        role_name: &str,
    ) -> Result<(), DriverError> {
        let xml = self.query_api_with(
            &self.base.iam,
            "us-east-1",
            "iam",
            creds,
            &[
                ("Action", "ListAttachedRolePolicies"),
                ("Version", "2010-05-08"),
                ("RoleName", role_name),
            ],
        ).await?;

        let arns = xml_all_texts(&xml, "PolicyArn");
        for arn in &arns {
            debug!(role_name, arn, "IAM: DetachRolePolicy");
            let _ = self.query_api_with(
                &self.base.iam,
                "us-east-1",
                "iam",
                creds,
                &[
                    ("Action", "DetachRolePolicy"),
                    ("Version", "2010-05-08"),
                    ("RoleName", role_name),
                    ("PolicyArn", arn.as_str()),
                ],
            ).await;
        }
        Ok(())
    }

    async fn iam_delete_inline_policies(
        &self,
        creds:     &AwsCredentials,
        role_name: &str,
    ) -> Result<(), DriverError> {
        let xml = self.query_api_with(
            &self.base.iam,
            "us-east-1",
            "iam",
            creds,
            &[
                ("Action", "ListRolePolicies"),
                ("Version", "2010-05-08"),
                ("RoleName", role_name),
            ],
        ).await?;

        let names = xml_all_texts(&xml, "member");
        for name in &names {
            let _ = self.query_api_with(
                &self.base.iam,
                "us-east-1",
                "iam",
                creds,
                &[
                    ("Action", "DeleteRolePolicy"),
                    ("Version", "2010-05-08"),
                    ("RoleName", role_name),
                    ("PolicyName", name.as_str()),
                ],
            ).await;
        }
        Ok(())
    }

    async fn iam_delete_role(
        &self,
        creds:     &AwsCredentials,
        role_name: &str,
    ) -> Result<(), DriverError> {
        let result = self.query_api_with(
            &self.base.iam,
            "us-east-1",
            "iam",
            creds,
            &[
                ("Action", "DeleteRole"),
                ("Version", "2010-05-08"),
                ("RoleName", role_name),
            ],
        ).await;
        match result {
            Ok(_) => Ok(()),
            Err(e) if e.to_string().contains("NoSuchEntity") => {
                warn!(role_name, "IAM role not found during teardown, skipping");
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    // ── Tagging API (orphan detection) ────────────────────────────────────────

    async fn tagging_get_resources(
        &self,
        creds:       &AwsCredentials,
        region:      &str,
        tag_filters: &Value,
    ) -> Result<Vec<(String, String, HashMap<String, String>)>, DriverError> {
        let resp = self.json_api(
            &self.base.tagging,
            region,
            "tagging",
            "ResourceGroupsTaggingAPI_20170126.GetResources",
            creds,
            &json!({ "TagFilters": tag_filters }),
        ).await?;

        let empty = vec![];
        let list  = resp["ResourceTagMappingList"].as_array().unwrap_or(&empty);
        let result = list.iter().map(|item| {
            let arn  = item["ResourceARN"].as_str().unwrap_or("").to_string();
            let rtype = arn.split(':').nth(2).unwrap_or("").to_string();
            let tags: HashMap<String, String> = item["Tags"]
                .as_array()
                .unwrap_or(&vec![])
                .iter()
                .filter_map(|t| {
                    let k = t["Key"].as_str()?;
                    let v = t["Value"].as_str()?;
                    Some((k.to_string(), v.to_string()))
                })
                .collect();
            (arn, rtype, tags)
        }).collect();
        Ok(result)
    }
}

#[async_trait]
impl Driver for AwsDriver {
    fn name(&self) -> &'static str { "aws" }

    // ── provision_enclave ─────────────────────────────────────────────────────

    async fn provision_enclave(
        &self,
        enclave:  &Enclave,
        existing: Option<&Handle>,
    ) -> Result<ProvisionResult, DriverError> {
        let enc_id  = enclave.id.as_str();
        let region  = enclave.region.as_str();

        // Idempotency: if already fully provisioned, return the stored handle.
        if let Some(h) = existing {
            if h["provisioning_complete"].as_bool() == Some(true) {
                return Ok(ProvisionResult {
                    handle:  h.clone(),
                    outputs: HashMap::new(),
                });
            }
        }

        let base_creds = self.get_creds().await?;

        // ── Step 1: Create AWS account ────────────────────────────────────────
        let account_name = self.account_name(enc_id);
        let email        = self.account_email(&account_name);
        info!(enc_id, account_name, email, "Provisioning AWS account");

        let req_id = self.org_create_account(&base_creds, &account_name, &email).await?;
        info!(enc_id, req_id, "Account creation request submitted, polling…");

        let account_id = self.org_wait_for_account(&base_creds, &req_id).await?;
        info!(enc_id, account_id, "AWS account created");

        // ── Step 2: Move account to configured OU ────────────────────────────
        let root_id = self.org_list_parents(&base_creds, &account_id).await?;
        self.org_move_account(
            &base_creds,
            &account_id,
            &root_id,
            &self.config.org_unit_id,
        ).await?;
        info!(enc_id, account_id, ou = %self.config.org_unit_id, "Moved account to OU");

        // ── Step 3: Assume role in the new account ────────────────────────────
        let enc_creds = self.enclave_creds(&account_id).await?;

        // ── Step 4: Create VPC ────────────────────────────────────────────────
        let cidr = enclave
            .network
            .as_ref()
            .and_then(|n| n.vpc_cidr.as_deref())
            .unwrap_or("10.0.0.0/16");

        let vpc_id = self.ec2_create_vpc(&enc_creds, region, cidr, enc_id).await?;

        // Enable DNS support and hostnames
        let _ = self.ec2_modify_vpc_attribute(
            &enc_creds, region, &vpc_id,
            "EnableDnsSupport.Value", "true",
        ).await;
        let _ = self.ec2_modify_vpc_attribute(
            &enc_creds, region, &vpc_id,
            "EnableDnsHostnames.Value", "true",
        ).await;
        info!(enc_id, vpc_id, "VPC created with DNS support");

        // ── Step 5: Create subnets ────────────────────────────────────────────
        let mut subnet_ids = Vec::new();
        let subnets = enclave
            .network
            .as_ref()
            .map(|n| n.subnets.as_slice())
            .unwrap_or(&[]);

        for (i, cidr) in subnets.iter().enumerate() {
            let subnet_id = self.ec2_create_subnet(
                &enc_creds, region, &vpc_id, cidr, enc_id, i,
            ).await?;
            subnet_ids.push(subnet_id);
        }

        // ── Step 6: Create Route53 private hosted zone (if dns.zone set) ──────
        let zone_id = if let Some(zone) = enclave.dns.as_ref().and_then(|d| d.zone.as_deref()) {
            let id = self.route53_create_hosted_zone(
                &enc_creds, zone, &vpc_id, region,
            ).await?;
            info!(enc_id, zone, zone_id = id, "Route53 private hosted zone created");
            Some(id)
        } else {
            None
        };

        // ── Step 7: Create identity IAM role (if identity set) ────────────────
        let identity_role_arn = if let Some(identity) = &enclave.identity {
            // Trust policy allows the nclav server role (or the management role) to assume this
            let server_role_arn = self.config.role_arn
                .as_deref()
                .unwrap_or("arn:aws:iam::*:root");
            let trust = serde_json::to_string(&json!({
                "Version": "2012-10-17",
                "Statement": [{
                    "Effect": "Allow",
                    "Principal": { "AWS": server_role_arn },
                    "Action": "sts:AssumeRole"
                }]
            })).unwrap();
            let arn = self.iam_create_role(
                &enc_creds, identity, &trust, enc_id, None,
            ).await?;
            info!(enc_id, identity, arn, "Identity IAM role created");
            Some(arn)
        } else {
            None
        };

        // ── Step 8: Stamp handle ──────────────────────────────────────────────
        let mut handle = json!({
            "driver":               "aws",
            "kind":                 "enclave",
            "account_id":           account_id,
            "account_name":         account_name,
            "region":               region,
            "vpc_id":               vpc_id,
            "subnet_ids":           subnet_ids,
            "provisioning_complete": true,
        });
        if let Some(ref zid) = zone_id {
            handle["route53_zone_id"] = json!(zid);
        }
        if let Some(ref arn) = identity_role_arn {
            handle["identity_role_arn"] = json!(arn);
        }

        Ok(ProvisionResult { handle, outputs: HashMap::new() })
    }

    // ── teardown_enclave ──────────────────────────────────────────────────────

    async fn teardown_enclave(
        &self,
        enclave: &Enclave,
        handle:  &Handle,
    ) -> Result<(), DriverError> {
        let enc_id     = enclave.id.as_str();
        let account_id = handle["account_id"].as_str().unwrap_or("");
        if account_id.is_empty() {
            warn!(enc_id, "teardown_enclave: no account_id in handle, skipping");
            return Ok(());
        }

        let base_creds = self.get_creds().await?;
        warn!(
            enc_id, account_id,
            "Closing AWS account (90-day hold; account will be deactivated)"
        );

        let result = self.json_api(
            &self.base.organizations,
            "us-east-1",
            "organizations",
            "AmazonOrganizationsV20161128.CloseAccount",
            &base_creds,
            &json!({ "AccountId": account_id }),
        ).await;

        match result {
            Ok(_) => {
                info!(enc_id, account_id, "AWS account closure initiated (90-day hold)");
                Ok(())
            }
            Err(e) if e.to_string().contains("AccountNotFoundException") => {
                warn!(enc_id, account_id, "AWS account not found during teardown, skipping");
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    // ── provision_partition ───────────────────────────────────────────────────

    async fn provision_partition(
        &self,
        enclave:         &Enclave,
        partition:       &Partition,
        _resolved_inputs: &HashMap<String, String>,
        existing:        Option<&Handle>,
    ) -> Result<ProvisionResult, DriverError> {
        let enc_id  = enclave.id.as_str();
        let part_id = partition.id.as_str();

        // Idempotency: if already provisioned, return stored handle.
        if let Some(h) = existing {
            if h["driver"].as_str() == Some("aws") && h["kind"].as_str() == Some("partition") {
                return Ok(ProvisionResult {
                    handle:  h.clone(),
                    outputs: HashMap::new(),
                });
            }
        }

        // Get enclave account ID from enclave handle (via resolved_inputs injected by reconciler)
        let enc_handle_str = _resolved_inputs.get("nclav_account_id")
            .cloned()
            .unwrap_or_default();
        let account_id = if enc_handle_str.is_empty() {
            return Err(DriverError::ProvisionFailed(format!(
                "provision_partition for enclave '{}': cannot determine AWS account ID. \
                 Ensure provision_enclave has run first (account_id is injected via \
                 context_vars → nclav_account_id).",
                enc_id
            )));
        } else {
            enc_handle_str
        };

        // Assume the cross-account role in the enclave account
        let enc_creds = self.enclave_creds(&account_id).await?;

        // Create the partition IAM role
        let role_name  = partition_role_name(part_id);
        let server_arn = self.config.role_arn
            .as_deref()
            .unwrap_or("arn:aws:iam::*:root");
        let trust = serde_json::to_string(&json!({
            "Version": "2012-10-17",
            "Statement": [{
                "Effect": "Allow",
                "Principal": { "AWS": server_arn },
                "Action": "sts:AssumeRole"
            }]
        })).unwrap();

        let role_arn = self.iam_create_role(
            &enc_creds, &role_name, &trust, enc_id, Some(part_id),
        ).await?;
        info!(enc_id, part_id, role_arn, "Partition IAM role created");

        // Attach AdministratorAccess managed policy
        self.iam_attach_role_policy(
            &enc_creds,
            &role_name,
            "arn:aws:iam::aws:policy/AdministratorAccess",
        ).await?;

        let handle = json!({
            "driver":           "aws",
            "kind":             "partition",
            "type":             "iac",
            "account_id":       account_id,
            "partition_role_arn": role_arn,
        });

        Ok(ProvisionResult { handle, outputs: HashMap::new() })
    }

    // ── teardown_partition ────────────────────────────────────────────────────

    async fn teardown_partition(
        &self,
        enclave:   &Enclave,
        partition: &Partition,
        handle:    &Handle,
    ) -> Result<(), DriverError> {
        let enc_id     = enclave.id.as_str();
        let part_id    = partition.id.as_str();
        let account_id = handle["account_id"].as_str().unwrap_or("");

        if account_id.is_empty() {
            warn!(enc_id, part_id, "teardown_partition: no account_id in handle, skipping");
            return Ok(());
        }

        let enc_creds = match self.enclave_creds(account_id).await {
            Ok(c) => c,
            Err(e) => {
                warn!(enc_id, part_id, ?e, "teardown_partition: could not assume enclave role, skipping");
                return Ok(());
            }
        };

        let role_name = partition_role_name(part_id);
        self.iam_detach_all_policies(&enc_creds, &role_name).await?;
        self.iam_delete_inline_policies(&enc_creds, &role_name).await?;
        self.iam_delete_role(&enc_creds, &role_name).await?;
        info!(enc_id, part_id, role_name, "Partition IAM role deleted");
        Ok(())
    }

    // ── provision_export ──────────────────────────────────────────────────────

    async fn provision_export(
        &self,
        _enclave:          &Enclave,
        export:            &Export,
        partition_outputs: &HashMap<String, String>,
        existing:          Option<&Handle>,
    ) -> Result<ProvisionResult, DriverError> {
        if let Some(h) = existing {
            if h.get("driver").and_then(|v| v.as_str()) == Some("aws") {
                return Ok(ProvisionResult { handle: h.clone(), outputs: HashMap::new() });
            }
        }

        let export_name = &export.name;
        let handle = match &export.export_type {
            ExportType::Http | ExportType::Tcp => {
                let endpoint_url = partition_outputs
                    .get("endpoint_url")
                    .cloned()
                    .unwrap_or_default();
                let port: u16 = partition_outputs
                    .get("port")
                    .and_then(|p| p.parse().ok())
                    .unwrap_or(if export.export_type == ExportType::Http { 443 } else { 0 });
                json!({
                    "driver":       "aws",
                    "kind":         "export",
                    "type":         export.export_type.to_string(),
                    "export_name":  export_name,
                    "endpoint_url": endpoint_url,
                    "port":         port,
                })
            }
            ExportType::Queue => {
                let queue_url = partition_outputs
                    .get("queue_url")
                    .cloned()
                    .unwrap_or_default();
                json!({
                    "driver":     "aws",
                    "kind":       "export",
                    "type":       "queue",
                    "export_name": export_name,
                    "queue_url":  queue_url,
                })
            }
        };

        let mut outputs = HashMap::new();
        if let Some(url) = partition_outputs.get("endpoint_url") {
            outputs.insert("endpoint_url".into(), url.clone());
        }
        if let Some(url) = partition_outputs.get("queue_url") {
            outputs.insert("queue_url".into(), url.clone());
        }

        Ok(ProvisionResult { handle, outputs })
    }

    // ── provision_import ──────────────────────────────────────────────────────

    async fn provision_import(
        &self,
        _importer:    &Enclave,
        import:       &Import,
        export_handle: &Handle,
        existing:     Option<&Handle>,
    ) -> Result<ProvisionResult, DriverError> {
        if let Some(h) = existing {
            if h.get("driver").and_then(|v| v.as_str()) == Some("aws") {
                return Ok(ProvisionResult { handle: h.clone(), outputs: HashMap::new() });
            }
        }

        let alias       = &import.alias;
        let export_type = export_handle["type"].as_str().unwrap_or("http");

        let handle = match export_type {
            "http" | "tcp" => {
                let endpoint_url = export_handle["endpoint_url"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();
                let port = export_handle["port"].as_u64().unwrap_or(443) as u16;
                json!({
                    "driver":       "aws",
                    "kind":         "import",
                    "alias":        alias,
                    "endpoint_url": endpoint_url,
                    "port":         port,
                })
            }
            "queue" => {
                let queue_url = export_handle["queue_url"].as_str().unwrap_or("").to_string();
                json!({
                    "driver":    "aws",
                    "kind":      "import",
                    "alias":     alias,
                    "queue_url": queue_url,
                })
            }
            other => {
                return Err(DriverError::ProvisionFailed(format!(
                    "provision_import '{}': unknown export type '{}'", alias, other
                )));
            }
        };

        let mut outputs = HashMap::new();
        if let Some(url) = export_handle["endpoint_url"].as_str() {
            outputs.insert("endpoint_url".into(), url.to_string());
        }
        if let Some(url) = export_handle["queue_url"].as_str() {
            outputs.insert("queue_url".into(), url.to_string());
        }

        Ok(ProvisionResult { handle, outputs })
    }

    // ── observe_enclave ───────────────────────────────────────────────────────

    async fn observe_enclave(
        &self,
        _enclave: &Enclave,
        handle:   &Handle,
    ) -> Result<ObservedState, DriverError> {
        let account_id = handle["account_id"].as_str().unwrap_or("");
        if account_id.is_empty() {
            return Ok(ObservedState {
                exists:  false,
                healthy: false,
                outputs: HashMap::new(),
                raw:     handle.clone(),
            });
        }

        let base_creds = self.get_creds().await?;
        let resp = self.json_api(
            &self.base.organizations,
            "us-east-1",
            "organizations",
            "AmazonOrganizationsV20161128.DescribeAccount",
            &base_creds,
            &json!({ "AccountId": account_id }),
        ).await;

        match resp {
            Ok(v) => {
                let status  = v["Account"]["Status"].as_str().unwrap_or("UNKNOWN");
                let exists  = status != "SUSPENDED";
                let healthy = status == "ACTIVE";
                Ok(ObservedState {
                    exists,
                    healthy,
                    outputs: HashMap::new(),
                    raw:     v,
                })
            }
            Err(e) if e.to_string().contains("AccountNotFoundException") => {
                Ok(ObservedState {
                    exists:  false,
                    healthy: false,
                    outputs: HashMap::new(),
                    raw:     handle.clone(),
                })
            }
            Err(e) => Err(e),
        }
    }

    // ── observe_partition ─────────────────────────────────────────────────────

    async fn observe_partition(
        &self,
        _enclave:   &Enclave,
        _partition: &Partition,
        handle:     &Handle,
    ) -> Result<ObservedState, DriverError> {
        let exists = handle["driver"].as_str() == Some("aws")
            && handle["kind"].as_str() == Some("partition");
        Ok(ObservedState {
            exists,
            healthy: exists,
            outputs: HashMap::new(),
            raw:     handle.clone(),
        })
    }

    // ── context_vars ──────────────────────────────────────────────────────────

    fn context_vars(&self, enclave: &Enclave, handle: &Handle) -> HashMap<String, String> {
        let account_id = handle["account_id"].as_str().unwrap_or("").to_string();
        let region     = handle["region"].as_str().unwrap_or(&self.config.default_region).to_string();
        let role_arn   = handle["partition_role_arn"].as_str().unwrap_or("").to_string();

        let mut vars = HashMap::new();
        // GCP-compat alias
        vars.insert("nclav_project_id".into(),     account_id.clone());
        vars.insert("nclav_region".into(),          region);
        vars.insert("nclav_account_id".into(),      account_id);
        vars.insert("nclav_role_arn".into(),         role_arn);
        vars.insert("nclav_enclave".into(),          enclave.id.as_str().to_string());
        vars
    }

    // ── auth_env ──────────────────────────────────────────────────────────────

    fn auth_env(&self, _enclave: &Enclave, handle: &Handle) -> HashMap<String, String> {
        let region   = handle["region"].as_str().unwrap_or(&self.config.default_region).to_string();
        let role_arn = handle["partition_role_arn"].as_str().unwrap_or("").to_string();

        let mut env = HashMap::new();
        env.insert("AWS_DEFAULT_REGION".into(), region);
        if !role_arn.is_empty() {
            env.insert("AWS_ROLE_ARN".into(), role_arn);
        }
        env
    }

    // ── list_partition_resources ──────────────────────────────────────────────

    async fn list_partition_resources(
        &self,
        enclave:   &Enclave,
        enc_handle: &Handle,
        partition: &Partition,
    ) -> Result<Vec<String>, DriverError> {
        let account_id = enc_handle["account_id"].as_str().unwrap_or("");
        if account_id.is_empty() { return Ok(vec![]); }

        let region = enc_handle["region"].as_str().unwrap_or(&self.config.default_region);
        let enc_creds = self.enclave_creds(account_id).await?;
        let part_id   = partition.id.as_str();
        let enc_id    = enclave.id.as_str();

        let resources = self.tagging_get_resources(
            &enc_creds,
            region,
            &json!([
                { "Key": "nclav-managed",   "Values": ["true"] },
                { "Key": "nclav-partition", "Values": [part_id] },
                { "Key": "nclav-enclave",   "Values": [enc_id] },
            ]),
        ).await?;

        Ok(resources.into_iter().map(|(arn, _, _)| arn).collect())
    }

    // ── list_orphaned_resources ───────────────────────────────────────────────

    async fn list_orphaned_resources(
        &self,
        enclave:             &Enclave,
        enc_handle:          &Handle,
        known_partition_ids: &[&str],
    ) -> Result<Vec<OrphanedResource>, DriverError> {
        let account_id = enc_handle["account_id"].as_str().unwrap_or("");
        if account_id.is_empty() { return Ok(vec![]); }

        let region    = enc_handle["region"].as_str().unwrap_or(&self.config.default_region);
        let enc_creds = self.enclave_creds(account_id).await?;
        let enc_id    = enclave.id.as_str();

        let resources = self.tagging_get_resources(
            &enc_creds,
            region,
            &json!([
                { "Key": "nclav-managed", "Values": ["true"] },
                { "Key": "nclav-enclave", "Values": [enc_id] },
            ]),
        ).await?;

        let orphans = resources
            .into_iter()
            .filter_map(|(arn, rtype, tags)| {
                let part = tags.get("nclav-partition")?.to_string();
                let enc  = tags.get("nclav-enclave").cloned().unwrap_or_default();
                if known_partition_ids.contains(&part.as_str()) {
                    return None;
                }
                Some(OrphanedResource {
                    resource_name:   arn,
                    resource_type:   rtype,
                    nclav_partition: part,
                    nclav_enclave:   enc,
                })
            })
            .collect();

        Ok(orphans)
    }
}

// ── URL encoding helper (no extra dep needed) ─────────────────────────────────

mod urlencoding {
    pub fn encode(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for byte in s.bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9'
                | b'-' | b'_' | b'.' | b'~' => out.push(byte as char),
                b' ' => out.push('+'),
                b => out.push_str(&format!("%{:02X}", b)),
            }
        }
        out
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use nclav_domain::{EnclaveId, NetworkConfig, PartitionId};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_config() -> AwsDriverConfig {
        AwsDriverConfig {
            org_unit_id:        "ou-test-12345678".into(),
            email_domain:       "example.com".into(),
            default_region:     "us-east-1".into(),
            account_prefix:     Some("test".into()),
            cross_account_role: "OrganizationAccountAccessRole".into(),
            role_arn:           Some("arn:aws:iam::111111111111:role/nclav-server".into()),
        }
    }

    fn test_creds() -> StaticCredentials {
        StaticCredentials {
            access_key_id:     "AKIAIOSFODNN7EXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
            session_token:     None,
        }
    }

    fn test_base_urls(server: &MockServer) -> BaseUrls {
        let base = server.uri();
        BaseUrls {
            organizations: format!("{}/orgs", base),
            sts:           format!("{}/sts", base),
            ec2:           format!("{}/ec2", base),
            iam:           format!("{}/iam", base),
            route53:       format!("{}/route53", base),
            tagging:       format!("{}/tagging", base),
        }
    }

    fn dummy_enclave() -> Enclave {
        Enclave {
            id:         EnclaveId::new("product-a-dev"),
            name:       "Product A Dev".into(),
            cloud:      Some(nclav_domain::CloudTarget::Aws),
            region:     "us-east-1".into(),
            identity:   None,
            network:    Some(NetworkConfig {
                vpc_cidr: Some("10.0.0.0/16".into()),
                subnets:  vec!["10.0.1.0/24".into()],
            }),
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
            backend:          nclav_domain::PartitionBackend::default(),
        }
    }

    // ── STS AssumeRole ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn sts_assume_role_parses_credentials() {
        let server = MockServer::start().await;

        let xml_resp = r#"<AssumeRoleResponse>
          <AssumeRoleResult>
            <Credentials>
              <AccessKeyId>ASIAIOSFODNN7EXAMPLE</AccessKeyId>
              <SecretAccessKey>wJalrXUtnFEMI/K7MDENG</SecretAccessKey>
              <SessionToken>AQoXnyc4lcK4w</SessionToken>
            </Credentials>
          </AssumeRoleResult>
        </AssumeRoleResponse>"#;

        Mock::given(method("POST"))
            .and(path("/sts/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(xml_resp))
            .mount(&server)
            .await;

        let d = AwsDriver::with_test_config(test_config(), test_base_urls(&server), test_creds());
        let base_creds = AwsCredentials {
            access_key_id:     "AKID".into(),
            secret_access_key: "SECRET".into(),
            session_token:     None,
        };

        let creds = d.sts_assume_role(
            &base_creds,
            "arn:aws:iam::123456789012:role/TestRole",
            "test-session",
        ).await.unwrap();

        assert_eq!(creds.access_key_id, "ASIAIOSFODNN7EXAMPLE");
        assert_eq!(creds.session_token.as_deref(), Some("AQoXnyc4lcK4w"));
    }

    // ── account naming ────────────────────────────────────────────────────────

    #[test]
    fn account_name_with_prefix() {
        let config = test_config();
        let d = AwsDriver {
            config,
            client: reqwest::Client::new(),
            creds:  Box::new(test_creds()),
            base:   BaseUrls::for_region("us-east-1"),
        };
        let name = d.account_name("product-a-dev");
        assert_eq!(name, "test-product-a-dev");
    }

    #[test]
    fn account_email_replaces_spaces() {
        let config = test_config();
        let d = AwsDriver {
            config,
            client: reqwest::Client::new(),
            creds:  Box::new(test_creds()),
            base:   BaseUrls::for_region("us-east-1"),
        };
        let email = d.account_email("test-product-a-dev");
        assert_eq!(email, "aws+test-product-a-dev@example.com");
    }

    // ── partition role name ───────────────────────────────────────────────────

    #[test]
    fn partition_role_name_short() {
        let name = partition_role_name("api");
        assert_eq!(name, "nclav-partition-api");
    }

    #[test]
    fn partition_role_name_long_truncates() {
        let long_id = "a".repeat(60);
        let name    = partition_role_name(&long_id);
        assert!(name.len() <= 64, "role name must be <= 64 chars: {}", name.len());
        assert!(name.starts_with("nclav-partition-"));
    }

    // ── xml_text ──────────────────────────────────────────────────────────────

    #[test]
    fn xml_text_finds_simple_element() {
        let xml = "<CreateVpcResponse><vpc><vpcId>vpc-abc123</vpcId></vpc></CreateVpcResponse>";
        assert_eq!(xml_text(xml, "vpcId"), Some("vpc-abc123".into()));
    }

    #[test]
    fn xml_text_returns_none_for_missing() {
        let xml = "<Foo><Bar>baz</Bar></Foo>";
        assert_eq!(xml_text(xml, "Missing"), None);
    }

    #[test]
    fn xml_all_texts_collects_multiple() {
        let xml = r#"<Result>
            <Policies>
              <PolicyArn>arn:aws:iam::aws:policy/Foo</PolicyArn>
              <PolicyArn>arn:aws:iam::aws:policy/Bar</PolicyArn>
            </Policies>
          </Result>"#;
        let texts = xml_all_texts(xml, "PolicyArn");
        assert_eq!(texts.len(), 2);
        assert!(texts[0].contains("Foo"));
        assert!(texts[1].contains("Bar"));
    }

    // ── provision_partition ───────────────────────────────────────────────────

    #[tokio::test]
    async fn provision_partition_creates_role() {
        let server = MockServer::start().await;

        // Mock STS AssumeRole (for enclave_creds)
        let sts_xml = r#"<AssumeRoleResponse><AssumeRoleResult><Credentials>
          <AccessKeyId>ASIA-ENC</AccessKeyId>
          <SecretAccessKey>ENC-SECRET</SecretAccessKey>
          <SessionToken>ENC-TOKEN</SessionToken>
        </Credentials></AssumeRoleResult></AssumeRoleResponse>"#;
        Mock::given(method("POST"))
            .and(path("/sts/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sts_xml))
            .mount(&server)
            .await;

        // Mock IAM CreateRole
        let create_role_xml = r#"<CreateRoleResponse><CreateRoleResult><Role>
          <Arn>arn:aws:iam::123456789012:role/nclav-partition-api</Arn>
          <RoleName>nclav-partition-api</RoleName>
        </Role></CreateRoleResult></CreateRoleResponse>"#;
        Mock::given(method("POST"))
            .and(path("/iam/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(create_role_xml))
            .mount(&server)
            .await;

        let d    = AwsDriver::with_test_config(test_config(), test_base_urls(&server), test_creds());
        let enc  = dummy_enclave();
        let part = dummy_partition();

        let mut inputs = HashMap::new();
        inputs.insert("nclav_account_id".into(), "123456789012".into());

        let result = d.provision_partition(&enc, &part, &inputs, None).await.unwrap();

        assert_eq!(result.handle["driver"].as_str(), Some("aws"));
        assert_eq!(result.handle["kind"].as_str(), Some("partition"));
        assert_eq!(result.handle["account_id"].as_str(), Some("123456789012"));
        assert!(result.handle["partition_role_arn"].as_str().unwrap_or("").contains("nclav-partition-api"));
    }

    // ── observe_partition ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn observe_partition_returns_healthy_for_valid_handle() {
        let server = MockServer::start().await;
        let d      = AwsDriver::with_test_config(test_config(), test_base_urls(&server), test_creds());
        let enc    = dummy_enclave();
        let part   = dummy_partition();
        let handle = json!({ "driver": "aws", "kind": "partition", "type": "iac" });

        let state = d.observe_partition(&enc, &part, &handle).await.unwrap();
        assert!(state.exists);
        assert!(state.healthy);
    }

    // ── context_vars ──────────────────────────────────────────────────────────

    #[test]
    fn context_vars_returns_expected_keys() {
        let config = test_config();
        let d = AwsDriver {
            config,
            client: reqwest::Client::new(),
            creds:  Box::new(test_creds()),
            base:   BaseUrls::for_region("us-east-1"),
        };
        let enc    = dummy_enclave();
        let handle = json!({
            "account_id":         "123456789012",
            "region":             "us-east-1",
            "partition_role_arn": "arn:aws:iam::123456789012:role/nclav-partition-api",
        });
        let vars = d.context_vars(&enc, &handle);
        assert_eq!(vars.get("nclav_project_id").map(String::as_str),  Some("123456789012"));
        assert_eq!(vars.get("nclav_account_id").map(String::as_str),  Some("123456789012"));
        assert_eq!(vars.get("nclav_region").map(String::as_str),       Some("us-east-1"));
        assert_eq!(vars.get("nclav_enclave").map(String::as_str),      Some("product-a-dev"));
        assert!(vars.get("nclav_role_arn").map(String::as_str).unwrap_or("").contains("nclav-partition"));
    }

    // ── auth_env ──────────────────────────────────────────────────────────────

    #[test]
    fn auth_env_sets_region_and_role() {
        let config = test_config();
        let d = AwsDriver {
            config,
            client: reqwest::Client::new(),
            creds:  Box::new(test_creds()),
            base:   BaseUrls::for_region("us-east-1"),
        };
        let enc    = dummy_enclave();
        let handle = json!({
            "region":             "eu-west-1",
            "partition_role_arn": "arn:aws:iam::123456789012:role/nclav-partition-api",
        });
        let env = d.auth_env(&enc, &handle);
        assert_eq!(env.get("AWS_DEFAULT_REGION").map(String::as_str), Some("eu-west-1"));
        assert_eq!(
            env.get("AWS_ROLE_ARN").map(String::as_str),
            Some("arn:aws:iam::123456789012:role/nclav-partition-api")
        );
    }
}
