use std::path::PathBuf;

use nclav_domain::{EnclaveId, PartitionId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconcileRequest {
    pub enclaves_dir: PathBuf,
    pub dry_run: bool,
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
