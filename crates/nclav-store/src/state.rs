use std::collections::HashMap;

use chrono::{DateTime, Utc};
use nclav_domain::{CloudTarget, Enclave, EnclaveId, Partition, PartitionId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Opaque driver handle — anything the driver returned from provision.
pub type Handle = Value;

// ── Lifecycle state machine ───────────────────────────────────────────────────

/// The lifecycle state of a provisioned resource.
///
/// Transitions:
///   Pending → Provisioning → Active ↔ Updating
///   Provisioning | Updating → Error
///   Active → Deleting → Deleted
///   Active → Degraded (from observe())
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProvisioningStatus {
    /// Known but not yet provisioned.
    #[default]
    Pending,
    /// Driver call in-flight for initial creation.
    Provisioning,
    /// Last provision/update succeeded; resource should exist.
    Active,
    /// Driver call in-flight for an update.
    Updating,
    /// observe() returned success but resource reported unhealthy.
    Degraded,
    /// Last driver call failed; `last_error` is populated.
    Error,
    /// Driver teardown in-flight.
    Deleting,
    /// Teardown confirmed; record retained briefly for audit.
    Deleted,
}

impl std::fmt::Display for ProvisioningStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            ProvisioningStatus::Pending => "pending",
            ProvisioningStatus::Provisioning => "provisioning",
            ProvisioningStatus::Active => "active",
            ProvisioningStatus::Updating => "updating",
            ProvisioningStatus::Degraded => "degraded",
            ProvisioningStatus::Error => "error",
            ProvisioningStatus::Deleting => "deleting",
            ProvisioningStatus::Deleted => "deleted",
        };
        write!(f, "{}", s)
    }
}

// ── ResourceError ─────────────────────────────────────────────────────────────

/// A persisted record of the most recent provisioning failure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceError {
    pub message: String,
    pub occurred_at: DateTime<Utc>,
}

// ── ResourceMeta ──────────────────────────────────────────────────────────────

/// Lifecycle and health metadata attached to every enclave and partition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceMeta {
    /// Current lifecycle state.
    pub status: ProvisioningStatus,
    /// When this resource was first successfully provisioned.
    pub created_at: Option<DateTime<Utc>>,
    /// When this resource was last successfully updated.
    pub updated_at: Option<DateTime<Utc>>,
    /// When Driver::observe() last confirmed the resource exists in the cloud.
    pub last_seen_at: Option<DateTime<Utc>>,
    /// Most recent provisioning failure, if any.
    pub last_error: Option<ResourceError>,
    /// SHA-256 of the canonical JSON of the desired config at last successful
    /// apply. Used to detect config drift cheaply without diffing the full struct.
    pub desired_hash: Option<String>,
    /// Monotonically increasing on every successful state write.
    /// Future: used for optimistic concurrency control in the store.
    pub generation: u64,
}

impl Default for ResourceMeta {
    fn default() -> Self {
        Self {
            status: ProvisioningStatus::Pending,
            created_at: None,
            updated_at: None,
            last_seen_at: None,
            last_error: None,
            desired_hash: None,
            generation: 0,
        }
    }
}

impl ResourceMeta {
    /// Transition to Active after a successful provision/update.
    pub fn mark_active(&mut self, now: DateTime<Utc>, hash: String) {
        if self.created_at.is_none() {
            self.created_at = Some(now);
        }
        self.updated_at = Some(now);
        self.status = ProvisioningStatus::Active;
        self.last_error = None;
        self.desired_hash = Some(hash);
        self.generation += 1;
    }

    /// Transition to Error after a failed provision/update.
    pub fn mark_error(&mut self, now: DateTime<Utc>, message: String) {
        self.status = ProvisioningStatus::Error;
        self.last_error = Some(ResourceError { message, occurred_at: now });
        self.generation += 1;
    }

    /// Record a successful observe() call.
    pub fn mark_seen(&mut self, now: DateTime<Utc>, healthy: bool) {
        self.last_seen_at = Some(now);
        if self.status == ProvisioningStatus::Active && !healthy {
            self.status = ProvisioningStatus::Degraded;
        } else if self.status == ProvisioningStatus::Degraded && healthy {
            self.status = ProvisioningStatus::Active;
        }
    }
}

// ── Compute a canonical desired-state hash ────────────────────────────────────

/// Serialize `value` to canonical JSON (object keys sorted) and return its
/// SHA-256 hex digest. Used to detect config drift cheaply.
pub fn compute_desired_hash<T: Serialize>(value: &T) -> String {
    let v = serde_json::to_value(value).unwrap_or(serde_json::Value::Null);
    let canonical = sort_json_keys(v);
    let bytes = serde_json::to_vec(&canonical).unwrap_or_default();
    let digest = Sha256::digest(&bytes);
    format!("{:x}", digest)
}

/// Recursively sort JSON object keys so HashMap field ordering doesn't affect
/// the hash.
fn sort_json_keys(v: serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::Object(map) => {
            let sorted: std::collections::BTreeMap<String, serde_json::Value> = map
                .into_iter()
                .map(|(k, v)| (k, sort_json_keys(v)))
                .collect();
            serde_json::Value::Object(sorted.into_iter().collect())
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.into_iter().map(sort_json_keys).collect())
        }
        other => other,
    }
}

// ── PartitionState ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartitionState {
    /// The desired partition as parsed from YAML.
    pub desired: Partition,
    /// Handle returned by the driver for this partition.
    pub partition_handle: Option<Handle>,
    /// Resolved key→value outputs produced by the driver.
    pub resolved_outputs: HashMap<String, String>,
    /// Lifecycle and health metadata.
    pub meta: ResourceMeta,
}

impl PartitionState {
    pub fn new(desired: Partition) -> Self {
        Self {
            desired,
            partition_handle: None,
            resolved_outputs: HashMap::new(),
            meta: ResourceMeta::default(),
        }
    }
}

// ── EnclaveState ──────────────────────────────────────────────────────────────

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
    /// Lifecycle and health metadata.
    pub meta: ResourceMeta,
    /// The cloud target resolved at reconcile time (desired.cloud or the API default).
    /// Stored so teardown knows which driver to use even after YAML removal.
    #[serde(default)]
    pub resolved_cloud: Option<CloudTarget>,
}

impl EnclaveState {
    pub fn new(desired: Enclave) -> Self {
        Self {
            desired,
            enclave_handle: None,
            partitions: HashMap::new(),
            export_handles: HashMap::new(),
            import_handles: HashMap::new(),
            meta: ResourceMeta::default(),
            resolved_cloud: None,
        }
    }
}

// ── IaC run log ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IacOperation {
    Provision,
    Update,
    Teardown,
}

impl std::fmt::Display for IacOperation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IacOperation::Provision => write!(f, "provision"),
            IacOperation::Update => write!(f, "update"),
            IacOperation::Teardown => write!(f, "teardown"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IacRunStatus {
    Running,
    Succeeded,
    Failed,
}

impl std::fmt::Display for IacRunStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IacRunStatus::Running => write!(f, "running"),
            IacRunStatus::Succeeded => write!(f, "succeeded"),
            IacRunStatus::Failed => write!(f, "failed"),
        }
    }
}

/// A record of a single IaC tool invocation (init + apply/destroy) for a partition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IacRun {
    pub id: Uuid,
    pub enclave_id: EnclaveId,
    pub partition_id: PartitionId,
    pub operation: IacOperation,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub status: IacRunStatus,
    /// Exit code of the IaC tool process (None while still Running).
    pub exit_code: Option<i32>,
    /// Combined stdout+stderr in arrival order.
    pub log: String,
    /// The reconcile run that triggered this IaC run, if any.
    pub reconcile_run_id: Option<Uuid>,
}

// ── AuditEvent ────────────────────────────────────────────────────────────────

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
    EnclaveError {
        id: Uuid,
        at: DateTime<Utc>,
        enclave_id: EnclaveId,
        message: String,
    },
    PartitionError {
        id: Uuid,
        at: DateTime<Utc>,
        enclave_id: EnclaveId,
        partition_id: PartitionId,
        message: String,
    },
}

impl AuditEvent {
    pub fn enclave_id(&self) -> Option<&EnclaveId> {
        match self {
            AuditEvent::EnclaveProvisioned { enclave_id, .. } => Some(enclave_id),
            AuditEvent::PartitionProvisioned { enclave_id, .. } => Some(enclave_id),
            AuditEvent::ExportWired { enclave_id, .. } => Some(enclave_id),
            AuditEvent::ImportWired { importer_enclave, .. } => Some(importer_enclave),
            AuditEvent::EnclaveError { enclave_id, .. } => Some(enclave_id),
            AuditEvent::PartitionError { enclave_id, .. } => Some(enclave_id),
            _ => None,
        }
    }
}
