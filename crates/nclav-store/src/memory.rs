use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use nclav_domain::{EnclaveId, PartitionId};
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::error::StoreError;
use crate::state::{AuditEvent, EnclaveState, IacRun, PartitionState};
use crate::store::StateStore;

#[derive(Debug, Default)]
struct Inner {
    enclaves: HashMap<EnclaveId, EnclaveState>,
    events: Vec<AuditEvent>,
    tf_state: HashMap<String, Vec<u8>>,
    tf_locks: HashMap<String, serde_json::Value>,
    iac_runs: HashMap<Uuid, IacRun>,
}

/// In-memory implementation of [`StateStore`].
///
/// All data is lost on process exit. Suitable for tests and the local driver.
#[derive(Debug, Clone, Default)]
pub struct InMemoryStore {
    inner: Arc<RwLock<Inner>>,
}

impl InMemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl StateStore for InMemoryStore {
    async fn get_enclave(&self, id: &EnclaveId) -> Result<Option<EnclaveState>, StoreError> {
        let guard = self.inner.read().await;
        Ok(guard.enclaves.get(id).cloned())
    }

    async fn list_enclaves(&self) -> Result<Vec<EnclaveState>, StoreError> {
        let guard = self.inner.read().await;
        Ok(guard.enclaves.values().cloned().collect())
    }

    async fn upsert_enclave(&self, state: &EnclaveState) -> Result<(), StoreError> {
        let mut guard = self.inner.write().await;
        guard.enclaves.insert(state.desired.id.clone(), state.clone());
        Ok(())
    }

    async fn delete_enclave(&self, id: &EnclaveId) -> Result<(), StoreError> {
        let mut guard = self.inner.write().await;
        guard.enclaves.remove(id);
        Ok(())
    }

    async fn upsert_partition(
        &self,
        enclave_id: &EnclaveId,
        state: &PartitionState,
    ) -> Result<(), StoreError> {
        let mut guard = self.inner.write().await;
        let enclave = guard
            .enclaves
            .get_mut(enclave_id)
            .ok_or_else(|| StoreError::EnclaveNotFound(enclave_id.to_string()))?;
        enclave
            .partitions
            .insert(state.desired.id.clone(), state.clone());
        Ok(())
    }

    async fn delete_partition(
        &self,
        enclave_id: &EnclaveId,
        partition_id: &PartitionId,
    ) -> Result<(), StoreError> {
        let mut guard = self.inner.write().await;
        if let Some(enclave) = guard.enclaves.get_mut(enclave_id) {
            enclave.partitions.remove(partition_id);
        }
        Ok(())
    }

    async fn append_event(&self, event: &AuditEvent) -> Result<(), StoreError> {
        let mut guard = self.inner.write().await;
        guard.events.push(event.clone());
        Ok(())
    }

    async fn list_events(
        &self,
        enclave_id: Option<&EnclaveId>,
        limit: u32,
    ) -> Result<Vec<AuditEvent>, StoreError> {
        let guard = self.inner.read().await;
        let filtered: Vec<AuditEvent> = guard
            .events
            .iter()
            .filter(|ev| {
                if let Some(eid) = enclave_id {
                    ev.enclave_id().map_or(false, |id| id == eid)
                } else {
                    true
                }
            })
            .cloned()
            .collect();

        let start = filtered.len().saturating_sub(limit as usize);
        Ok(filtered[start..].to_vec())
    }

    // ── Terraform HTTP state backend ──────────────────────────────────────────

    async fn get_tf_state(&self, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
        let guard = self.inner.read().await;
        Ok(guard.tf_state.get(key).cloned())
    }

    async fn put_tf_state(&self, key: &str, state: Vec<u8>) -> Result<(), StoreError> {
        let mut guard = self.inner.write().await;
        guard.tf_state.insert(key.to_string(), state);
        Ok(())
    }

    async fn delete_tf_state(&self, key: &str) -> Result<(), StoreError> {
        let mut guard = self.inner.write().await;
        guard.tf_state.remove(key);
        guard.tf_locks.remove(key);
        Ok(())
    }

    async fn lock_tf_state(
        &self,
        key: &str,
        lock_info: serde_json::Value,
    ) -> Result<(), StoreError> {
        let mut guard = self.inner.write().await;
        if let Some(existing) = guard.tf_locks.get(key) {
            let holder = existing["ID"]
                .as_str()
                .unwrap_or("unknown")
                .to_string();
            return Err(StoreError::LockConflict { holder });
        }
        guard.tf_locks.insert(key.to_string(), lock_info);
        Ok(())
    }

    async fn unlock_tf_state(&self, key: &str, lock_id: &str) -> Result<(), StoreError> {
        let mut guard = self.inner.write().await;
        if let Some(existing) = guard.tf_locks.get(key) {
            // Empty lock_id = force-unlock (no ID check).
            if lock_id.is_empty() || existing["ID"].as_str().unwrap_or("") == lock_id {
                guard.tf_locks.remove(key);
            }
        }
        Ok(())
    }

    // ── IaC run log ───────────────────────────────────────────────────────────

    async fn upsert_iac_run(&self, run: &IacRun) -> Result<(), StoreError> {
        let mut guard = self.inner.write().await;
        guard.iac_runs.insert(run.id, run.clone());
        Ok(())
    }

    async fn list_iac_runs(
        &self,
        enclave_id: &EnclaveId,
        partition_id: &PartitionId,
    ) -> Result<Vec<IacRun>, StoreError> {
        let guard = self.inner.read().await;
        let mut runs: Vec<IacRun> = guard
            .iac_runs
            .values()
            .filter(|r| &r.enclave_id == enclave_id && &r.partition_id == partition_id)
            .cloned()
            .collect();
        runs.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        runs.truncate(100);
        Ok(runs)
    }

    async fn get_iac_run(&self, run_id: Uuid) -> Result<Option<IacRun>, StoreError> {
        let guard = self.inner.read().await;
        Ok(guard.iac_runs.get(&run_id).cloned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nclav_domain::*;

    fn dummy_enclave(id: &str) -> EnclaveState {
        EnclaveState::new(Enclave {
            id: EnclaveId::new(id),
            name: id.to_string(),
            cloud: None,
            region: "local".to_string(),
            identity: None,
            network: None,
            dns: None,
            imports: vec![],
            exports: vec![],
            partitions: vec![],
        })
    }

    #[tokio::test]
    async fn upsert_and_get() {
        let store = InMemoryStore::new();
        let state = dummy_enclave("test");
        store.upsert_enclave(&state).await.unwrap();

        let got = store.get_enclave(&EnclaveId::new("test")).await.unwrap();
        assert!(got.is_some());
        assert_eq!(got.unwrap().desired.id.as_str(), "test");
    }

    #[tokio::test]
    async fn list_enclaves() {
        let store = InMemoryStore::new();
        store.upsert_enclave(&dummy_enclave("a")).await.unwrap();
        store.upsert_enclave(&dummy_enclave("b")).await.unwrap();

        let list = store.list_enclaves().await.unwrap();
        assert_eq!(list.len(), 2);
    }

    #[tokio::test]
    async fn delete_enclave() {
        let store = InMemoryStore::new();
        store.upsert_enclave(&dummy_enclave("del")).await.unwrap();
        store.delete_enclave(&EnclaveId::new("del")).await.unwrap();
        assert!(store.get_enclave(&EnclaveId::new("del")).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn events_filtered_by_enclave() {
        use uuid::Uuid;
        use chrono::Utc;

        let store = InMemoryStore::new();
        store
            .append_event(&AuditEvent::EnclaveProvisioned {
                id: Uuid::new_v4(),
                at: Utc::now(),
                enclave_id: EnclaveId::new("a"),
            })
            .await
            .unwrap();
        store
            .append_event(&AuditEvent::EnclaveProvisioned {
                id: Uuid::new_v4(),
                at: Utc::now(),
                enclave_id: EnclaveId::new("b"),
            })
            .await
            .unwrap();

        let all = store.list_events(None, 100).await.unwrap();
        assert_eq!(all.len(), 2);

        let for_a = store
            .list_events(Some(&EnclaveId::new("a")), 100)
            .await
            .unwrap();
        assert_eq!(for_a.len(), 1);
    }
}
