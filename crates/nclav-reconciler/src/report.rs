use std::path::PathBuf;
use std::sync::Arc;

use nclav_domain::{EnclaveId, PartitionId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconcileRequest {
    pub enclaves_dir: PathBuf,
    pub dry_run: bool,
    /// Base URL of the nclav API server (e.g. "http://127.0.0.1:8080").
    /// Used to configure the Terraform HTTP state backend.
    #[serde(default = "default_api_base")]
    pub api_base: String,
    /// nclav bearer token. Passed as TF_HTTP_PASSWORD to IaC subprocesses.
    /// Not serialized — callers must supply it directly.
    #[serde(skip, default)]
    pub auth_token: Arc<String>,
    /// When true, the TerraformBackend skips subprocess invocations and returns stubbed outputs.
    /// Use in tests to avoid requiring a terraform binary.
    #[serde(default)]
    pub test_mode: bool,
}

fn default_api_base() -> String {
    "http://127.0.0.1:8080".into()
}

impl Default for ReconcileRequest {
    fn default() -> Self {
        Self {
            enclaves_dir: std::path::PathBuf::new(),
            dry_run: false,
            api_base: default_api_base(),
            auth_token: Arc::new(String::new()),
            test_mode: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum Change {
    EnclaveCreated { id: EnclaveId },
    EnclaveUpdated { id: EnclaveId },
    EnclaveDeleted { id: EnclaveId },
    PartitionCreated { enclave_id: EnclaveId, partition_id: PartitionId },
    PartitionUpdated { enclave_id: EnclaveId, partition_id: PartitionId },
    PartitionDeleted { enclave_id: EnclaveId, partition_id: PartitionId },
    ExportWired { enclave_id: EnclaveId, export_name: String },
    ImportWired { importer_enclave: EnclaveId, alias: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconcileReport {
    pub dry_run: bool,
    pub changes: Vec<Change>,
    pub errors: Vec<String>,
}

impl ReconcileReport {
    pub fn new(dry_run: bool) -> Self {
        Self {
            dry_run,
            changes: Vec::new(),
            errors: Vec::new(),
        }
    }
}
