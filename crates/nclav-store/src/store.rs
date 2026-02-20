use async_trait::async_trait;
use nclav_domain::{EnclaveId, PartitionId};

use crate::error::StoreError;
use crate::state::{AuditEvent, EnclaveState, PartitionState};

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
}
