use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ── Identifiers ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EnclaveId(pub String);

impl EnclaveId {
    pub fn new(s: impl Into<String>) -> Self {
        EnclaveId(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for EnclaveId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PartitionId(pub String);

impl PartitionId {
    pub fn new(s: impl Into<String>) -> Self {
        PartitionId(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PartitionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ── Enums ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CloudTarget {
    Local,
    Gcp,
    Azure,
    Aws,
}

impl std::fmt::Display for CloudTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CloudTarget::Local => write!(f, "local"),
            CloudTarget::Gcp => write!(f, "gcp"),
            CloudTarget::Azure => write!(f, "azure"),
            CloudTarget::Aws => write!(f, "aws"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExportType {
    Http,
    Tcp,
    Queue,
}

impl ExportType {
    /// Returns the set of auth types compatible with this export type.
    pub fn compatible_auth_types(&self) -> &[AuthType] {
        match self {
            ExportType::Http => &[
                AuthType::None,
                AuthType::Token,
                AuthType::Oauth,
                AuthType::Mtls,
            ],
            ExportType::Tcp => &[AuthType::None, AuthType::Mtls, AuthType::Native],
            ExportType::Queue => &[AuthType::None, AuthType::Token, AuthType::Native],
        }
    }

    pub fn is_auth_compatible(&self, auth: &AuthType) -> bool {
        self.compatible_auth_types().contains(auth)
    }
}

impl std::fmt::Display for ExportType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExportType::Http => write!(f, "http"),
            ExportType::Tcp => write!(f, "tcp"),
            ExportType::Queue => write!(f, "queue"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthType {
    None,
    Token,
    Oauth,
    Mtls,
    Native,
}

impl std::fmt::Display for AuthType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthType::None => write!(f, "none"),
            AuthType::Token => write!(f, "token"),
            AuthType::Oauth => write!(f, "oauth"),
            AuthType::Mtls => write!(f, "mtls"),
            AuthType::Native => write!(f, "native"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportTarget {
    Public,
    AnyEnclave,
    Enclave(EnclaveId),
    Vpn,
    Partition(PartitionId),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProducesType {
    Http,
    Tcp,
    Queue,
}

impl ProducesType {
    /// The output keys that a partition with this produces-type must declare.
    pub fn required_outputs(&self) -> &[&'static str] {
        match self {
            ProducesType::Http => &["hostname", "port"],
            ProducesType::Tcp => &["hostname", "port"],
            ProducesType::Queue => &["queue_url"],
        }
    }
}

impl std::fmt::Display for ProducesType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProducesType::Http => write!(f, "http"),
            ProducesType::Tcp => write!(f, "tcp"),
            ProducesType::Queue => write!(f, "queue"),
        }
    }
}

impl From<&ProducesType> for ExportType {
    fn from(p: &ProducesType) -> ExportType {
        match p {
            ProducesType::Http => ExportType::Http,
            ProducesType::Tcp => ExportType::Tcp,
            ProducesType::Queue => ExportType::Queue,
        }
    }
}

// ── Partition backend ─────────────────────────────────────────────────────────

/// How a partition's workload is provisioned.
/// Orthogonal to the enclave's `cloud` field (which controls *where*).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub enum PartitionBackend {
    /// Co-located `.tf` files in the partition directory, run via the `terraform` binary.
    Terraform(TerraformConfig),
    /// Co-located `.tf` files in the partition directory, run via the `tofu` binary.
    OpenTofu(TerraformConfig),
}

impl Default for PartitionBackend {
    fn default() -> Self {
        PartitionBackend::Terraform(TerraformConfig {
            tool: None,
            source: None,
            dir: std::path::PathBuf::new(),
        })
    }
}

// Custom Deserialize that accepts the old `"Managed"` unit-variant string
// (stored before Managed was removed) and silently promotes it to
// `Terraform` with default config, so existing state.redb files remain
// readable across that migration.
impl<'de> Deserialize<'de> for PartitionBackend {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let v = serde_json::Value::deserialize(d)?;
        match &v {
            // Old unit-variant stored as the bare string "Managed".
            serde_json::Value::String(s) if s == "Managed" => {
                Ok(PartitionBackend::Terraform(TerraformConfig {
                    tool: None,
                    source: None,
                    dir: std::path::PathBuf::new(),
                }))
            }
            // Current externally-tagged format: {"Terraform": {...}} or {"OpenTofu": {...}}
            _ => {
                #[derive(Deserialize)]
                enum Inner {
                    Terraform(TerraformConfig),
                    OpenTofu(TerraformConfig),
                }
                match serde_json::from_value::<Inner>(v).map_err(D::Error::custom)? {
                    Inner::Terraform(c) => Ok(PartitionBackend::Terraform(c)),
                    Inner::OpenTofu(c) => Ok(PartitionBackend::OpenTofu(c)),
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerraformConfig {
    /// Binary override. None = auto-detect from PATH (`terraform` first, then `tofu`).
    pub tool: Option<String>,
    /// Module source URL. When present, nclav generates all workspace `.tf` files from
    /// this module and the partition directory must contain no `.tf` files.
    /// Ref pinning and other options are expressed inline using Terraform's native URL
    /// syntax, e.g. `git::https://…//module?ref=v1.2.0`. Passed verbatim to Terraform.
    pub source: Option<String>,
    /// Absolute path to the partition directory containing the `.tf` files.
    /// Set by the config loader; not present in YAML.
    pub dir: std::path::PathBuf,
}

// ── Core structs ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Export {
    pub name: String,
    pub target_partition: PartitionId,
    pub export_type: ExportType,
    pub to: ExportTarget,
    pub auth: AuthType,
    /// Optional hostname override for this export.
    pub hostname: Option<String>,
    /// Optional port override for this export.
    pub port: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Import {
    /// Source enclave id.
    pub from: EnclaveId,
    /// Name of the export on the source enclave.
    pub export_name: String,
    /// Local alias used inside this partition for template substitution.
    pub alias: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Partition {
    pub id: PartitionId,
    pub name: String,
    pub produces: Option<ProducesType>,
    pub imports: Vec<Import>,
    pub exports: Vec<Export>,
    /// Template-able input values resolved before provisioning.
    pub inputs: HashMap<String, String>,
    /// Output keys this partition declares it will produce.
    pub declared_outputs: Vec<String>,
    /// How this partition's workload is provisioned. Defaults to `Terraform`.
    #[serde(default)]
    pub backend: PartitionBackend,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkConfig {
    pub vpc_cidr: Option<String>,
    pub subnets: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DnsConfig {
    pub zone: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Enclave {
    pub id: EnclaveId,
    pub name: String,
    /// Cloud target for this enclave. None means inherit the API's default cloud.
    pub cloud: Option<CloudTarget>,
    pub region: String,
    pub identity: Option<String>,
    pub network: Option<NetworkConfig>,
    pub dns: Option<DnsConfig>,
    /// Cross-enclave imports (entire enclave level).
    pub imports: Vec<Import>,
    /// Exports this enclave exposes to others.
    pub exports: Vec<Export>,
    pub partitions: Vec<Partition>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partition_backend_deserializes_legacy_managed() {
        // Old state.redb stored Managed as the bare string "Managed".
        // It should silently promote to Terraform with default config.
        let b: PartitionBackend = serde_json::from_str("\"Managed\"").unwrap();
        assert!(matches!(b, PartitionBackend::Terraform(_)));
    }

    #[test]
    fn partition_backend_round_trips_terraform() {
        let orig = PartitionBackend::Terraform(TerraformConfig {
            tool: Some("tofu".into()),
            source: None,
            dir: std::path::PathBuf::from("/tmp"),
        });
        let json = serde_json::to_string(&orig).unwrap();
        let back: PartitionBackend = serde_json::from_str(&json).unwrap();
        assert_eq!(orig, back);
    }
}
