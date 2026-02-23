use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use nclav_domain::{EnclaveId, PartitionId};
use redb::{Database, ReadableTable, TableDefinition};
use uuid::Uuid;

use crate::error::StoreError;
use crate::state::{AuditEvent, EnclaveState, IacRun, PartitionState};
use crate::store::StateStore;

const ENCLAVES: TableDefinition<&str, &[u8]>  = TableDefinition::new("enclaves");
const EVENTS:   TableDefinition<u64, &[u8]>   = TableDefinition::new("events");
const META:     TableDefinition<&str, u64>     = TableDefinition::new("meta");
// Terraform state backend
const TF_STATE: TableDefinition<&str, &[u8]>  = TableDefinition::new("tf_state");
const TF_LOCKS: TableDefinition<&str, &[u8]>  = TableDefinition::new("tf_locks");
// IaC run log — keyed by "{enclave_id}/{partition_id}/{started_at_rfc3339}/{run_id}"
// for efficient partition-scoped queries in chronological order.
const IAC_RUNS:         TableDefinition<&str, &[u8]> = TableDefinition::new("iac_runs");
const IAC_RUNS_BY_PART: TableDefinition<&str, &str>  = TableDefinition::new("iac_runs_by_part");

/// Persistent state store backed by a redb database file.
///
/// All enclave state survives process restarts. Suitable for local production use.
#[derive(Clone)]
pub struct RedbStore {
    db: Arc<Database>,
}

impl RedbStore {
    /// Open (or create) a redb database at `path`.
    ///
    /// Parent directories are created automatically.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| StoreError::Internal(e.to_string()))?;
        }
        let db = Database::create(path)
            .map_err(|e| StoreError::Internal(e.to_string()))?;

        // Ensure tables exist
        {
            let wtxn = db.begin_write().map_err(|e| StoreError::Internal(e.to_string()))?;
            wtxn.open_table(ENCLAVES).map_err(|e| StoreError::Internal(e.to_string()))?;
            wtxn.open_table(EVENTS).map_err(|e| StoreError::Internal(e.to_string()))?;
            wtxn.open_table(META).map_err(|e| StoreError::Internal(e.to_string()))?;
            wtxn.open_table(TF_STATE).map_err(|e| StoreError::Internal(e.to_string()))?;
            wtxn.open_table(TF_LOCKS).map_err(|e| StoreError::Internal(e.to_string()))?;
            wtxn.open_table(IAC_RUNS).map_err(|e| StoreError::Internal(e.to_string()))?;
            wtxn.open_table(IAC_RUNS_BY_PART).map_err(|e| StoreError::Internal(e.to_string()))?;
            wtxn.commit().map_err(|e| StoreError::Internal(e.to_string()))?;
        }

        Ok(Self { db: Arc::new(db) })
    }
}

#[async_trait]
impl StateStore for RedbStore {
    async fn get_enclave(&self, id: &EnclaveId) -> Result<Option<EnclaveState>, StoreError> {
        let rtxn = self.db.begin_read().map_err(|e| StoreError::Internal(e.to_string()))?;
        let table = rtxn.open_table(ENCLAVES).map_err(|e| StoreError::Internal(e.to_string()))?;
        match table.get(id.as_str()).map_err(|e| StoreError::Internal(e.to_string()))? {
            Some(guard) => {
                let state: EnclaveState = serde_json::from_slice(guard.value())?;
                Ok(Some(state))
            }
            None => Ok(None),
        }
    }

    async fn list_enclaves(&self) -> Result<Vec<EnclaveState>, StoreError> {
        let rtxn = self.db.begin_read().map_err(|e| StoreError::Internal(e.to_string()))?;
        let table = rtxn.open_table(ENCLAVES).map_err(|e| StoreError::Internal(e.to_string()))?;
        let mut results = Vec::new();
        for entry in table.iter().map_err(|e| StoreError::Internal(e.to_string()))? {
            let (_k, v) = entry.map_err(|e| StoreError::Internal(e.to_string()))?;
            let state: EnclaveState = serde_json::from_slice(v.value())?;
            results.push(state);
        }
        Ok(results)
    }

    async fn upsert_enclave(&self, state: &EnclaveState) -> Result<(), StoreError> {
        let bytes = serde_json::to_vec(state)?;
        let key = state.desired.id.0.clone();
        let wtxn = self.db.begin_write().map_err(|e| StoreError::Internal(e.to_string()))?;
        {
            let mut table = wtxn.open_table(ENCLAVES).map_err(|e| StoreError::Internal(e.to_string()))?;
            table.insert(key.as_str(), bytes.as_slice()).map_err(|e| StoreError::Internal(e.to_string()))?;
        }
        wtxn.commit().map_err(|e| StoreError::Internal(e.to_string()))?;
        Ok(())
    }

    async fn delete_enclave(&self, id: &EnclaveId) -> Result<(), StoreError> {
        let wtxn = self.db.begin_write().map_err(|e| StoreError::Internal(e.to_string()))?;
        {
            let mut table = wtxn.open_table(ENCLAVES).map_err(|e| StoreError::Internal(e.to_string()))?;
            table.remove(id.as_str()).map_err(|e| StoreError::Internal(e.to_string()))?;
        }
        wtxn.commit().map_err(|e| StoreError::Internal(e.to_string()))?;
        Ok(())
    }

    async fn upsert_partition(
        &self,
        enclave_id: &EnclaveId,
        state: &PartitionState,
    ) -> Result<(), StoreError> {
        let mut enc_state = self
            .get_enclave(enclave_id)
            .await?
            .ok_or_else(|| StoreError::EnclaveNotFound(enclave_id.to_string()))?;
        enc_state.partitions.insert(state.desired.id.clone(), state.clone());
        self.upsert_enclave(&enc_state).await
    }

    async fn delete_partition(
        &self,
        enclave_id: &EnclaveId,
        partition_id: &PartitionId,
    ) -> Result<(), StoreError> {
        if let Some(mut enc_state) = self.get_enclave(enclave_id).await? {
            enc_state.partitions.remove(partition_id);
            self.upsert_enclave(&enc_state).await?;
        }
        Ok(())
    }

    async fn append_event(&self, event: &AuditEvent) -> Result<(), StoreError> {
        let bytes = serde_json::to_vec(event)?;
        let wtxn = self.db.begin_write().map_err(|e| StoreError::Internal(e.to_string()))?;
        {
            let mut meta = wtxn.open_table(META).map_err(|e| StoreError::Internal(e.to_string()))?;
            let seq = meta
                .get("event_seq")
                .map_err(|e| StoreError::Internal(e.to_string()))?
                .map(|g| g.value())
                .unwrap_or(0);
            let new_seq = seq + 1;
            meta.insert("event_seq", new_seq).map_err(|e| StoreError::Internal(e.to_string()))?;

            let mut events = wtxn.open_table(EVENTS).map_err(|e| StoreError::Internal(e.to_string()))?;
            events.insert(new_seq, bytes.as_slice()).map_err(|e| StoreError::Internal(e.to_string()))?;
        }
        wtxn.commit().map_err(|e| StoreError::Internal(e.to_string()))?;
        Ok(())
    }

    async fn list_events(
        &self,
        enclave_id: Option<&EnclaveId>,
        limit: u32,
    ) -> Result<Vec<AuditEvent>, StoreError> {
        let rtxn = self.db.begin_read().map_err(|e| StoreError::Internal(e.to_string()))?;
        let table = rtxn.open_table(EVENTS).map_err(|e| StoreError::Internal(e.to_string()))?;
        let mut all: Vec<AuditEvent> = Vec::new();
        for entry in table.iter().map_err(|e| StoreError::Internal(e.to_string()))? {
            let (_k, v) = entry.map_err(|e| StoreError::Internal(e.to_string()))?;
            let event: AuditEvent = serde_json::from_slice(v.value())?;
            if let Some(eid) = enclave_id {
                if event.enclave_id().map_or(false, |id| id == eid) {
                    all.push(event);
                }
            } else {
                all.push(event);
            }
        }
        let start = all.len().saturating_sub(limit as usize);
        Ok(all[start..].to_vec())
    }

    // ── Terraform HTTP state backend ──────────────────────────────────────────

    async fn get_tf_state(&self, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
        let rtxn = self.db.begin_read().map_err(|e| StoreError::Internal(e.to_string()))?;
        let table = rtxn.open_table(TF_STATE).map_err(|e| StoreError::Internal(e.to_string()))?;
        Ok(table
            .get(key)
            .map_err(|e| StoreError::Internal(e.to_string()))?
            .map(|g| g.value().to_vec()))
    }

    async fn put_tf_state(&self, key: &str, state: Vec<u8>) -> Result<(), StoreError> {
        let wtxn = self.db.begin_write().map_err(|e| StoreError::Internal(e.to_string()))?;
        {
            let mut table = wtxn.open_table(TF_STATE).map_err(|e| StoreError::Internal(e.to_string()))?;
            table.insert(key, state.as_slice()).map_err(|e| StoreError::Internal(e.to_string()))?;
        }
        wtxn.commit().map_err(|e| StoreError::Internal(e.to_string()))?;
        Ok(())
    }

    async fn delete_tf_state(&self, key: &str) -> Result<(), StoreError> {
        let wtxn = self.db.begin_write().map_err(|e| StoreError::Internal(e.to_string()))?;
        {
            let mut state_table = wtxn.open_table(TF_STATE).map_err(|e| StoreError::Internal(e.to_string()))?;
            state_table.remove(key).map_err(|e| StoreError::Internal(e.to_string()))?;
            let mut lock_table = wtxn.open_table(TF_LOCKS).map_err(|e| StoreError::Internal(e.to_string()))?;
            lock_table.remove(key).map_err(|e| StoreError::Internal(e.to_string()))?;
        }
        wtxn.commit().map_err(|e| StoreError::Internal(e.to_string()))?;
        Ok(())
    }

    async fn lock_tf_state(
        &self,
        key: &str,
        lock_info: serde_json::Value,
    ) -> Result<(), StoreError> {
        let wtxn = self.db.begin_write().map_err(|e| StoreError::Internal(e.to_string()))?;
        {
            let mut table = wtxn.open_table(TF_LOCKS).map_err(|e| StoreError::Internal(e.to_string()))?;
            // Copy bytes out of the guard immediately so the immutable borrow is
            // released before we attempt any mutable operation on `table`.
            let existing_bytes: Option<Vec<u8>> = table
                .get(key)
                .map_err(|e| StoreError::Internal(e.to_string()))?
                .map(|g| g.value().to_vec());
            if let Some(bytes) = existing_bytes {
                let existing: serde_json::Value = serde_json::from_slice(&bytes)?;
                let holder = existing["ID"].as_str().unwrap_or("unknown").to_string();
                return Err(StoreError::LockConflict { holder });
            }
            let bytes = serde_json::to_vec(&lock_info)?;
            table.insert(key, bytes.as_slice()).map_err(|e| StoreError::Internal(e.to_string()))?;
        }
        wtxn.commit().map_err(|e| StoreError::Internal(e.to_string()))?;
        Ok(())
    }

    async fn unlock_tf_state(&self, key: &str, lock_id: &str) -> Result<(), StoreError> {
        let wtxn = self.db.begin_write().map_err(|e| StoreError::Internal(e.to_string()))?;
        {
            let mut table = wtxn.open_table(TF_LOCKS).map_err(|e| StoreError::Internal(e.to_string()))?;
            let existing_bytes: Option<Vec<u8>> = table
                .get(key)
                .map_err(|e| StoreError::Internal(e.to_string()))?
                .map(|g| g.value().to_vec());
            if let Some(bytes) = existing_bytes {
                let existing: serde_json::Value = serde_json::from_slice(&bytes)?;
                // Empty lock_id = force-unlock (no ID check).
                if lock_id.is_empty() || existing["ID"].as_str().unwrap_or("") == lock_id {
                    table.remove(key).map_err(|e| StoreError::Internal(e.to_string()))?;
                }
            }
        }
        wtxn.commit().map_err(|e| StoreError::Internal(e.to_string()))?;
        Ok(())
    }

    // ── IaC run log ───────────────────────────────────────────────────────────

    async fn upsert_iac_run(&self, run: &IacRun) -> Result<(), StoreError> {
        let bytes = serde_json::to_vec(run)?;
        let run_id = run.id.to_string();
        // Secondary index key: "{enclave_id}/{partition_id}/{started_at}/{run_id}"
        // Lexicographic iteration gives chronological order; reverse for newest-first.
        let index_key = format!(
            "{}/{}/{}/{}",
            run.enclave_id.as_str(),
            run.partition_id.as_str(),
            run.started_at.to_rfc3339(),
            run_id,
        );

        let wtxn = self.db.begin_write().map_err(|e| StoreError::Internal(e.to_string()))?;
        {
            let mut runs = wtxn.open_table(IAC_RUNS).map_err(|e| StoreError::Internal(e.to_string()))?;
            runs.insert(run_id.as_str(), bytes.as_slice()).map_err(|e| StoreError::Internal(e.to_string()))?;
            let mut idx = wtxn.open_table(IAC_RUNS_BY_PART).map_err(|e| StoreError::Internal(e.to_string()))?;
            idx.insert(index_key.as_str(), run_id.as_str()).map_err(|e| StoreError::Internal(e.to_string()))?;
        }
        wtxn.commit().map_err(|e| StoreError::Internal(e.to_string()))?;
        Ok(())
    }

    async fn list_iac_runs(
        &self,
        enclave_id: &EnclaveId,
        partition_id: &PartitionId,
    ) -> Result<Vec<IacRun>, StoreError> {
        let prefix = format!("{}/{}/", enclave_id.as_str(), partition_id.as_str());
        let rtxn = self.db.begin_read().map_err(|e| StoreError::Internal(e.to_string()))?;
        let idx = rtxn.open_table(IAC_RUNS_BY_PART).map_err(|e| StoreError::Internal(e.to_string()))?;
        let runs_table = rtxn.open_table(IAC_RUNS).map_err(|e| StoreError::Internal(e.to_string()))?;

        // Collect run IDs from the index in chronological order, then reverse.
        let mut run_ids: Vec<String> = idx
            .range(prefix.as_str()..)
            .map_err(|e| StoreError::Internal(e.to_string()))?
            .filter_map(|entry| {
                let (k, v) = entry.ok()?;
                if k.value().starts_with(prefix.as_str()) {
                    Some(v.value().to_string())
                } else {
                    None
                }
            })
            .collect();
        run_ids.reverse(); // newest first
        run_ids.truncate(100);

        let mut runs = Vec::with_capacity(run_ids.len());
        for id in run_ids {
            if let Some(g) = runs_table.get(id.as_str()).map_err(|e| StoreError::Internal(e.to_string()))? {
                let run: IacRun = serde_json::from_slice(g.value())?;
                runs.push(run);
            }
        }
        Ok(runs)
    }

    async fn get_iac_run(&self, run_id: Uuid) -> Result<Option<IacRun>, StoreError> {
        let id = run_id.to_string();
        let rtxn = self.db.begin_read().map_err(|e| StoreError::Internal(e.to_string()))?;
        let table = rtxn.open_table(IAC_RUNS).map_err(|e| StoreError::Internal(e.to_string()))?;
        match table.get(id.as_str()).map_err(|e| StoreError::Internal(e.to_string()))? {
            Some(g) => Ok(Some(serde_json::from_slice(g.value())?)),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nclav_domain::*;
    use tempfile::TempDir;

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

    fn open_store(dir: &TempDir) -> RedbStore {
        RedbStore::open(&dir.path().join("state.redb")).unwrap()
    }

    #[tokio::test]
    async fn upsert_and_get() {
        let dir = TempDir::new().unwrap();
        let store = open_store(&dir);
        let state = dummy_enclave("test");
        store.upsert_enclave(&state).await.unwrap();
        let got = store.get_enclave(&EnclaveId::new("test")).await.unwrap();
        assert!(got.is_some());
        assert_eq!(got.unwrap().desired.id.as_str(), "test");
    }

    #[tokio::test]
    async fn persistence_survives_reopen() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("state.redb");

        // Write
        {
            let store = RedbStore::open(&path).unwrap();
            store.upsert_enclave(&dummy_enclave("persistent")).await.unwrap();
        }

        // Re-open and verify
        {
            let store = RedbStore::open(&path).unwrap();
            let got = store.get_enclave(&EnclaveId::new("persistent")).await.unwrap();
            assert!(got.is_some(), "data should survive store reopen");
            assert_eq!(got.unwrap().desired.id.as_str(), "persistent");
        }
    }

    #[tokio::test]
    async fn delete_enclave() {
        let dir = TempDir::new().unwrap();
        let store = open_store(&dir);
        store.upsert_enclave(&dummy_enclave("del")).await.unwrap();
        store.delete_enclave(&EnclaveId::new("del")).await.unwrap();
        assert!(store.get_enclave(&EnclaveId::new("del")).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn list_enclaves() {
        let dir = TempDir::new().unwrap();
        let store = open_store(&dir);
        store.upsert_enclave(&dummy_enclave("a")).await.unwrap();
        store.upsert_enclave(&dummy_enclave("b")).await.unwrap();
        let list = store.list_enclaves().await.unwrap();
        assert_eq!(list.len(), 2);
    }

    #[tokio::test]
    async fn events_append_and_list() {
        use chrono::Utc;
        use uuid::Uuid;

        let dir = TempDir::new().unwrap();
        let store = open_store(&dir);
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

        let for_a = store.list_events(Some(&EnclaveId::new("a")), 100).await.unwrap();
        assert_eq!(for_a.len(), 1);
    }
}
