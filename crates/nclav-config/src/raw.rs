use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Raw YAML representation of an enclave config file (enclave/config.yml)
#[derive(Debug, Deserialize, Serialize)]
pub struct RawEnclave {
    pub id: String,
    pub name: String,
    /// Optional cloud target; absent means inherit the API's default cloud.
    pub cloud: Option<String>,
    pub region: String,
    pub identity: Option<String>,
    pub network: Option<RawNetwork>,
    pub dns: Option<RawDns>,
    #[serde(default)]
    pub imports: Vec<RawImport>,
    #[serde(default)]
    pub exports: Vec<RawExport>,
    #[serde(default)]
    pub partitions: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct RawNetwork {
    pub vpc_cidr: Option<String>,
    #[serde(default)]
    pub subnets: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct RawDns {
    pub zone: Option<String>,
}

/// Raw YAML representation of a partition config file (partition/config.yml)
#[derive(Debug, Deserialize, Serialize)]
pub struct RawPartition {
    pub id: String,
    pub name: String,
    pub produces: Option<String>,
    #[serde(default)]
    pub imports: Vec<RawImport>,
    #[serde(default)]
    pub exports: Vec<RawExport>,
    #[serde(default)]
    pub inputs: HashMap<String, String>,
    #[serde(default)]
    pub declared_outputs: Vec<String>,
    /// "managed" (default), "terraform", or "opentofu".
    #[serde(default)]
    pub backend: String,
    /// Present when `backend` is "terraform" or "opentofu".
    pub terraform: Option<RawTerraformConfig>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct RawTerraformConfig {
    /// Override the IaC binary. Absent = auto-detect from PATH.
    pub tool: Option<String>,
    /// Module source URL. When present, nclav generates all `.tf` files.
    pub source: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct RawExport {
    pub name: String,
    pub target_partition: String,
    #[serde(rename = "type")]
    pub export_type: String,
    pub to: RawExportTarget,
    #[serde(default = "default_auth")]
    pub auth: String,
    pub hostname: Option<String>,
    pub port: Option<u16>,
}

fn default_auth() -> String {
    "none".to_string()
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum RawExportTarget {
    Simple(String),
    Enclave { enclave: String },
    Partition { partition: String },
}

#[derive(Debug, Deserialize, Serialize)]
pub struct RawImport {
    pub from: String,
    pub export_name: String,
    pub alias: String,
}
