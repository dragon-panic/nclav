use async_trait::async_trait;
use nclav_domain::{EnclaveId, PartitionId};
use uuid::Uuid;

use crate::error::StoreError;
use crate::state::{AuditEvent, EnclaveState, IacRun, PartitionState};

#[async_trait]
pub trait StateStore: Send + Sync + 'static {
    async fn get_enclave(&self, id: &EnclaveId) -> Result<Option<EnclaveState>, StoreError>;
    async fn list_enclaves(&self) -> Result<Vec<EnclaveState>, StoreError>;
    async fn upsert_enclave(&self, state: &EnclaveState) -> Result<(), StoreError>;
    async fn delete_enclave(&self, id: &EnclaveId) -> Result<(), StoreError>;

    async fn upsert_partition(
        &self,
        enclave_id: &EnclaveId,
        state: &PartitionState,
    ) -> Result<(), StoreError>;

    async fn delete_partition(
        &self,
        enclave_id: &EnclaveId,
        partition_id: &PartitionId,
    ) -> Result<(), StoreError>;

    async fn append_event(&self, event: &AuditEvent) -> Result<(), StoreError>;

    async fn list_events(
        &self,
        enclave_id: Option<&EnclaveId>,
        limit: u32,
    ) -> Result<Vec<AuditEvent>, StoreError>;

    // ── Terraform HTTP state backend ──────────────────────────────────────────

    /// Fetch the raw Terraform state blob. Returns `None` if no state exists yet.
    async fn get_tf_state(&self, key: &str) -> Result<Option<Vec<u8>>, StoreError>;

    /// Persist the raw Terraform state blob (overwrites any existing state).
    async fn put_tf_state(&self, key: &str, state: Vec<u8>) -> Result<(), StoreError>;

    /// Delete the Terraform state blob entirely (called after a successful destroy).
    async fn delete_tf_state(&self, key: &str) -> Result<(), StoreError>;

    /// Acquire an advisory lock on the Terraform state.
    /// Returns `Err(StoreError::LockConflict)` if already locked by a different holder.
    /// `lock_info` is the JSON body sent by Terraform's lock protocol.
    async fn lock_tf_state(
        &self,
        key: &str,
        lock_info: serde_json::Value,
    ) -> Result<(), StoreError>;

    /// Release the advisory lock. No-op if not locked or locked by a different ID.
    async fn unlock_tf_state(&self, key: &str, lock_id: &str) -> Result<(), StoreError>;

    // ── IaC run log ───────────────────────────────────────────────────────────

    /// Persist an IaC run record (insert or update by `run.id`).
    async fn upsert_iac_run(&self, run: &IacRun) -> Result<(), StoreError>;

    /// List IaC runs for a partition, newest first, capped at 100.
    async fn list_iac_runs(
        &self,
        enclave_id: &EnclaveId,
        partition_id: &PartitionId,
    ) -> Result<Vec<IacRun>, StoreError>;

    /// Fetch a single IaC run by its UUID.
    async fn get_iac_run(&self, run_id: Uuid) -> Result<Option<IacRun>, StoreError>;
}
