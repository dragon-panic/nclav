use std::collections::HashMap;

use chrono::{DateTime, Utc};
use nclav_domain::{Enclave, EnclaveId, Partition, PartitionId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Opaque driver handle — anything the driver returned from provision.
pub type Handle = Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartitionState {
    /// The desired partition as parsed from YAML.
    pub desired: Partition,
    /// Handle returned by the driver for this partition.
    pub partition_handle: Option<Handle>,
    /// Resolved key→value outputs produced by the driver.
    pub resolved_outputs: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnclaveState {
    /// The desired enclave as parsed from YAML.
    pub desired: Enclave,
    /// Handle returned by the driver for the enclave itself.
    pub enclave_handle: Option<Handle>,
    /// Partition states, keyed by partition id.
    pub partitions: HashMap<PartitionId, PartitionState>,
    /// Export handles keyed by export name.
    pub export_handles: HashMap<String, Handle>,
    /// Import handles keyed by alias.
    pub import_handles: HashMap<String, Handle>,
    /// Wall-clock time of the last successful reconcile.
    pub last_reconciled_at: Option<DateTime<Utc>>,
}

impl EnclaveState {
    pub fn new(desired: Enclave) -> Self {
        Self {
            desired,
            enclave_handle: None,
            partitions: HashMap::new(),
            export_handles: HashMap::new(),
            import_handles: HashMap::new(),
            last_reconciled_at: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum AuditEvent {
    ReconcileStarted {
        id: Uuid,
        at: DateTime<Utc>,
        dry_run: bool,
    },
    ReconcileCompleted {
        id: Uuid,
        at: DateTime<Utc>,
        changes: usize,
        dry_run: bool,
    },
    EnclaveProvisioned {
        id: Uuid,
        at: DateTime<Utc>,
        enclave_id: EnclaveId,
    },
    PartitionProvisioned {
        id: Uuid,
        at: DateTime<Utc>,
        enclave_id: EnclaveId,
        partition_id: PartitionId,
    },
    ExportWired {
        id: Uuid,
        at: DateTime<Utc>,
        enclave_id: EnclaveId,
        export_name: String,
    },
    ImportWired {
        id: Uuid,
        at: DateTime<Utc>,
        importer_enclave: EnclaveId,
        export_name: String,
    },
}

impl AuditEvent {
    pub fn enclave_id(&self) -> Option<&EnclaveId> {
        match self {
            AuditEvent::EnclaveProvisioned { enclave_id, .. } => Some(enclave_id),
            AuditEvent::PartitionProvisioned { enclave_id, .. } => Some(enclave_id),
            AuditEvent::ExportWired { enclave_id, .. } => Some(enclave_id),
            AuditEvent::ImportWired { importer_enclave, .. } => Some(importer_enclave),
            _ => None,
        }
    }
}
