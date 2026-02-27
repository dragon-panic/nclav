use async_trait::async_trait;
use nclav_domain::{EnclaveId, PartitionId};
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::StoreError;
use crate::state::{AuditEvent, EnclaveState, IacRun, PartitionState};
use crate::store::StateStore;

// DDL — idempotent; run at every startup via migrate().
const MIGRATIONS: &str = r#"
CREATE TABLE IF NOT EXISTS enclaves (
    id         TEXT PRIMARY KEY,
    state      JSONB NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS audit_events (
    seq         BIGSERIAL PRIMARY KEY,
    enclave_id  TEXT,
    event       JSONB NOT NULL,
    occurred_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_audit_events_enclave
    ON audit_events (enclave_id) WHERE enclave_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS tf_state (
    key   TEXT PRIMARY KEY,
    state BYTEA NOT NULL
);

CREATE TABLE IF NOT EXISTS tf_locks (
    key       TEXT PRIMARY KEY,
    lock_info JSONB NOT NULL
);

CREATE TABLE IF NOT EXISTS iac_runs (
    run_id       UUID PRIMARY KEY,
    enclave_id   TEXT NOT NULL,
    partition_id TEXT NOT NULL,
    started_at   TIMESTAMPTZ NOT NULL,
    run          JSONB NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_iac_runs_partition
    ON iac_runs (enclave_id, partition_id, started_at DESC);
"#;

/// Persistent state store backed by a PostgreSQL database.
///
/// All tables are created automatically on first connect via [`PostgresStore::connect`].
/// Uses JSONB for enclave/event/IaC state and BYTEA for raw Terraform state files.
/// Safe for use from Cloud Run (or any environment where the DB is remote).
#[derive(Clone)]
pub struct PostgresStore {
    pool: PgPool,
}

impl PostgresStore {
    /// Connect to a PostgreSQL database and run schema migrations.
    ///
    /// `url` is a standard libpq-style connection string, e.g.:
    /// - `postgres://user:pass@localhost:5432/nclav`
    /// - `postgres://nclav:pwd@/nclav?host=/cloudsql/project:region:instance`  (Cloud SQL socket)
    pub async fn connect(url: &str) -> Result<Self, StoreError> {
        let pool = PgPool::connect(url)
            .await
            .map_err(|e| StoreError::Internal(format!("postgres connect: {e}")))?;
        let store = Self { pool };
        store.migrate().await?;
        Ok(store)
    }

    /// Run all DDL migrations.  Safe to call on every startup — all statements
    /// use `CREATE TABLE IF NOT EXISTS` / `CREATE INDEX IF NOT EXISTS`.
    async fn migrate(&self) -> Result<(), StoreError> {
        sqlx::query(MIGRATIONS)
            .execute(&self.pool)
            .await
            .map_err(|e| StoreError::Internal(format!("migration: {e}")))?;
        Ok(())
    }
}

// ── Helper conversions ────────────────────────────────────────────────────────

fn to_json<T: serde::Serialize>(v: &T) -> Result<serde_json::Value, StoreError> {
    serde_json::to_value(v).map_err(StoreError::Serialization)
}

fn from_json<T: serde::de::DeserializeOwned>(v: serde_json::Value) -> Result<T, StoreError> {
    serde_json::from_value(v).map_err(StoreError::Serialization)
}

// Extract the `enclave_id` string that should be stored alongside an AuditEvent
// for indexed filtering.
fn event_enclave_id(event: &AuditEvent) -> Option<String> {
    match event {
        AuditEvent::EnclaveProvisioned { enclave_id, .. } => Some(enclave_id.0.clone()),
        AuditEvent::PartitionProvisioned { enclave_id, .. } => Some(enclave_id.0.clone()),
        AuditEvent::ExportWired { enclave_id, .. } => Some(enclave_id.0.clone()),
        AuditEvent::ImportWired { importer_enclave, .. } => Some(importer_enclave.0.clone()),
        AuditEvent::EnclaveError { enclave_id, .. } => Some(enclave_id.0.clone()),
        AuditEvent::PartitionError { enclave_id, .. } => Some(enclave_id.0.clone()),
        AuditEvent::ReconcileStarted { .. } | AuditEvent::ReconcileCompleted { .. } => None,
    }
}

// ── StateStore implementation ─────────────────────────────────────────────────

#[async_trait]
impl StateStore for PostgresStore {
    // ── Enclaves ──────────────────────────────────────────────────────────────

    async fn get_enclave(&self, id: &EnclaveId) -> Result<Option<EnclaveState>, StoreError> {
        let row: Option<(serde_json::Value,)> =
            sqlx::query_as("SELECT state FROM enclaves WHERE id = $1")
                .bind(&id.0)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| StoreError::Internal(e.to_string()))?;
        row.map(|(v,)| from_json(v)).transpose()
    }

    async fn list_enclaves(&self) -> Result<Vec<EnclaveState>, StoreError> {
        let rows: Vec<(serde_json::Value,)> =
            sqlx::query_as("SELECT state FROM enclaves ORDER BY id")
                .fetch_all(&self.pool)
                .await
                .map_err(|e| StoreError::Internal(e.to_string()))?;
        rows.into_iter().map(|(v,)| from_json(v)).collect()
    }

    async fn upsert_enclave(&self, state: &EnclaveState) -> Result<(), StoreError> {
        let json = to_json(state)?;
        sqlx::query(
            "INSERT INTO enclaves (id, state, updated_at)
             VALUES ($1, $2::jsonb, NOW())
             ON CONFLICT (id) DO UPDATE SET state = EXCLUDED.state, updated_at = NOW()",
        )
        .bind(&state.desired.id.0)
        .bind(&json)
        .execute(&self.pool)
        .await
        .map_err(|e| StoreError::Internal(e.to_string()))?;
        Ok(())
    }

    async fn delete_enclave(&self, id: &EnclaveId) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM enclaves WHERE id = $1")
            .bind(&id.0)
            .execute(&self.pool)
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        Ok(())
    }

    // ── Partitions ────────────────────────────────────────────────────────────
    //
    // Partition state is stored nested inside EnclaveState (mirrors redb).
    // These methods load the enclave, mutate the partition map, and re-upsert.

    async fn upsert_partition(
        &self,
        enclave_id: &EnclaveId,
        state: &PartitionState,
    ) -> Result<(), StoreError> {
        let mut enc = self
            .get_enclave(enclave_id)
            .await?
            .ok_or_else(|| StoreError::EnclaveNotFound(enclave_id.0.clone()))?;
        enc.partitions.insert(state.desired.id.clone(), state.clone());
        self.upsert_enclave(&enc).await
    }

    async fn delete_partition(
        &self,
        enclave_id: &EnclaveId,
        partition_id: &PartitionId,
    ) -> Result<(), StoreError> {
        let mut enc = self
            .get_enclave(enclave_id)
            .await?
            .ok_or_else(|| StoreError::EnclaveNotFound(enclave_id.0.clone()))?;
        enc.partitions.remove(partition_id);
        self.upsert_enclave(&enc).await
    }

    // ── Audit events ──────────────────────────────────────────────────────────

    async fn append_event(&self, event: &AuditEvent) -> Result<(), StoreError> {
        let json = to_json(event)?;
        let eid = event_enclave_id(event);
        sqlx::query(
            "INSERT INTO audit_events (enclave_id, event, occurred_at) VALUES ($1, $2::jsonb, NOW())",
        )
        .bind(eid)
        .bind(&json)
        .execute(&self.pool)
        .await
        .map_err(|e| StoreError::Internal(e.to_string()))?;
        Ok(())
    }

    async fn list_events(
        &self,
        enclave_id: Option<&EnclaveId>,
        limit: u32,
    ) -> Result<Vec<AuditEvent>, StoreError> {
        // Fetch the most recent `limit` events (DESC), then reverse so callers
        // get chronological order — consistent with InMemoryStore behaviour.
        let rows: Vec<(serde_json::Value,)> = match enclave_id {
            Some(eid) => sqlx::query_as(
                "SELECT event FROM audit_events WHERE enclave_id = $1
                 ORDER BY seq DESC LIMIT $2",
            )
            .bind(&eid.0)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?,
            None => sqlx::query_as(
                "SELECT event FROM audit_events ORDER BY seq DESC LIMIT $1",
            )
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?,
        };
        let mut events: Vec<AuditEvent> = rows.into_iter().map(|(v,)| from_json(v)).collect::<Result<_, _>>()?;
        events.reverse();
        Ok(events)
    }

    // ── Terraform HTTP state backend ──────────────────────────────────────────

    async fn get_tf_state(&self, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
        let row: Option<(Vec<u8>,)> =
            sqlx::query_as("SELECT state FROM tf_state WHERE key = $1")
                .bind(key)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| StoreError::Internal(e.to_string()))?;
        Ok(row.map(|(b,)| b))
    }

    async fn put_tf_state(&self, key: &str, state: Vec<u8>) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO tf_state (key, state) VALUES ($1, $2)
             ON CONFLICT (key) DO UPDATE SET state = EXCLUDED.state",
        )
        .bind(key)
        .bind(&state)
        .execute(&self.pool)
        .await
        .map_err(|e| StoreError::Internal(e.to_string()))?;
        Ok(())
    }

    async fn delete_tf_state(&self, key: &str) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM tf_state WHERE key = $1")
            .bind(key)
            .execute(&self.pool)
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        Ok(())
    }

    async fn lock_tf_state(
        &self,
        key: &str,
        lock_info: serde_json::Value,
    ) -> Result<(), StoreError> {
        // Atomic insert — if the key already exists the INSERT is a no-op.
        let result = sqlx::query(
            "INSERT INTO tf_locks (key, lock_info) VALUES ($1, $2::jsonb)
             ON CONFLICT (key) DO NOTHING",
        )
        .bind(key)
        .bind(&lock_info)
        .execute(&self.pool)
        .await
        .map_err(|e| StoreError::Internal(e.to_string()))?;

        if result.rows_affected() == 0 {
            // Lock already held — read the current holder.
            let row: (serde_json::Value,) =
                sqlx::query_as("SELECT lock_info FROM tf_locks WHERE key = $1")
                    .bind(key)
                    .fetch_one(&self.pool)
                    .await
                    .map_err(|e| StoreError::Internal(e.to_string()))?;
            let holder = row.0["ID"]
                .as_str()
                .unwrap_or("unknown")
                .to_string();
            return Err(StoreError::LockConflict { holder });
        }
        Ok(())
    }

    async fn unlock_tf_state(&self, key: &str, lock_id: &str) -> Result<(), StoreError> {
        if lock_id.is_empty() {
            // Force-unlock: remove regardless of lock ID (operator override).
            sqlx::query("DELETE FROM tf_locks WHERE key = $1")
                .bind(key)
                .execute(&self.pool)
                .await
                .map_err(|e| StoreError::Internal(e.to_string()))?;
        } else {
            sqlx::query(
                "DELETE FROM tf_locks WHERE key = $1 AND lock_info->>'ID' = $2",
            )
            .bind(key)
            .bind(lock_id)
            .execute(&self.pool)
            .await
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        }
        Ok(())
    }

    // ── IaC run logs ──────────────────────────────────────────────────────────

    async fn upsert_iac_run(&self, run: &IacRun) -> Result<(), StoreError> {
        let json = to_json(run)?;
        sqlx::query(
            "INSERT INTO iac_runs (run_id, enclave_id, partition_id, started_at, run)
             VALUES ($1, $2, $3, $4, $5::jsonb)
             ON CONFLICT (run_id) DO UPDATE SET run = EXCLUDED.run",
        )
        .bind(run.id)
        .bind(&run.enclave_id.0)
        .bind(&run.partition_id.0)
        .bind(run.started_at)
        .bind(&json)
        .execute(&self.pool)
        .await
        .map_err(|e| StoreError::Internal(e.to_string()))?;
        Ok(())
    }

    async fn list_iac_runs(
        &self,
        enclave_id: &EnclaveId,
        partition_id: &PartitionId,
    ) -> Result<Vec<IacRun>, StoreError> {
        let rows: Vec<(serde_json::Value,)> = sqlx::query_as(
            "SELECT run FROM iac_runs
             WHERE enclave_id = $1 AND partition_id = $2
             ORDER BY started_at DESC
             LIMIT 100",
        )
        .bind(&enclave_id.0)
        .bind(&partition_id.0)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| StoreError::Internal(e.to_string()))?;
        rows.into_iter().map(|(v,)| from_json(v)).collect()
    }

    async fn get_iac_run(&self, run_id: Uuid) -> Result<Option<IacRun>, StoreError> {
        let row: Option<(serde_json::Value,)> =
            sqlx::query_as("SELECT run FROM iac_runs WHERE run_id = $1")
                .bind(run_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| StoreError::Internal(e.to_string()))?;
        row.map(|(v,)| from_json(v)).transpose()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────
//
// Gated behind TEST_POSTGRES_URL env var.  Run with:
//   docker run -d --name nclav-pg \
//     -e POSTGRES_PASSWORD=nclav -e POSTGRES_DB=nclav \
//     -p 5432:5432 postgres:16
//   TEST_POSTGRES_URL=postgres://postgres:nclav@localhost:5432/nclav \
//     cargo test -p nclav-store -- --ignored

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{IacOperation, IacRunStatus, ProvisioningStatus, ResourceMeta};
    use chrono::Utc;
    use nclav_domain::{
        CloudTarget, Enclave, EnclaveId, NetworkConfig, Partition, PartitionBackend,
        PartitionId, TerraformConfig,
    };
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn test_url() -> Option<String> {
        std::env::var("TEST_POSTGRES_URL").ok()
    }

    fn dummy_enclave(id: &str) -> EnclaveState {
        EnclaveState {
            desired: Enclave {
                id: EnclaveId(id.into()),
                name: format!("{id} test"),
                cloud: Some(CloudTarget::Local),
                region: "local-1".into(),
                identity: None,
                network: Some(NetworkConfig {
                    vpc_cidr: Some("10.0.0.0/16".into()),
                    subnets: vec!["10.0.1.0/24".into()],
                }),
                dns: None,
                imports: vec![],
                exports: vec![],
                partitions: vec![],
            },
            enclave_handle: None,
            partitions: HashMap::new(),
            export_handles: HashMap::new(),
            import_handles: HashMap::new(),
            meta: ResourceMeta {
                status: ProvisioningStatus::Pending,
                created_at: None,
                updated_at: None,
                last_seen_at: None,
                last_error: None,
                desired_hash: None,
                generation: 0,
            },
            resolved_cloud: None,
        }
    }

    fn dummy_partition(id: &str) -> PartitionState {
        PartitionState {
            desired: Partition {
                id: PartitionId(id.into()),
                name: format!("{id} partition"),
                produces: None,
                imports: vec![],
                exports: vec![],
                inputs: HashMap::new(),
                declared_outputs: vec![],
                backend: PartitionBackend::Terraform(TerraformConfig {
                    tool: None,
                    source: None,
                    dir: PathBuf::from("."),
                }),
            },
            partition_handle: None,
            resolved_outputs: HashMap::new(),
            meta: ResourceMeta {
                status: ProvisioningStatus::Pending,
                created_at: None,
                updated_at: None,
                last_seen_at: None,
                last_error: None,
                desired_hash: None,
                generation: 0,
            },
        }
    }

    #[tokio::test]
    #[ignore = "requires TEST_POSTGRES_URL"]
    async fn upsert_and_get() {
        let url = test_url().unwrap();
        let store = PostgresStore::connect(&url).await.unwrap();

        let enc = dummy_enclave("pg-test-upsert");
        store.upsert_enclave(&enc).await.unwrap();

        let fetched = store.get_enclave(&enc.desired.id).await.unwrap().unwrap();
        assert_eq!(fetched.desired.id, enc.desired.id);

        store.delete_enclave(&enc.desired.id).await.unwrap();
        assert!(store.get_enclave(&enc.desired.id).await.unwrap().is_none());
    }

    #[tokio::test]
    #[ignore = "requires TEST_POSTGRES_URL"]
    async fn list_enclaves() {
        let url = test_url().unwrap();
        let store = PostgresStore::connect(&url).await.unwrap();

        let a = dummy_enclave("pg-test-list-a");
        let b = dummy_enclave("pg-test-list-b");
        store.upsert_enclave(&a).await.unwrap();
        store.upsert_enclave(&b).await.unwrap();

        let all = store.list_enclaves().await.unwrap();
        let ids: Vec<&str> = all.iter().map(|e| e.desired.id.0.as_str()).collect();
        assert!(ids.contains(&"pg-test-list-a"));
        assert!(ids.contains(&"pg-test-list-b"));

        store.delete_enclave(&a.desired.id).await.unwrap();
        store.delete_enclave(&b.desired.id).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires TEST_POSTGRES_URL"]
    async fn upsert_and_delete_partition() {
        let url = test_url().unwrap();
        let store = PostgresStore::connect(&url).await.unwrap();

        let enc = dummy_enclave("pg-test-part-enc");
        store.upsert_enclave(&enc).await.unwrap();

        let part = dummy_partition("pg-test-part");
        store.upsert_partition(&enc.desired.id, &part).await.unwrap();

        let fetched = store.get_enclave(&enc.desired.id).await.unwrap().unwrap();
        assert!(fetched.partitions.contains_key(&part.desired.id));

        store.delete_partition(&enc.desired.id, &part.desired.id).await.unwrap();
        let after = store.get_enclave(&enc.desired.id).await.unwrap().unwrap();
        assert!(!after.partitions.contains_key(&part.desired.id));

        store.delete_enclave(&enc.desired.id).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires TEST_POSTGRES_URL"]
    async fn events_append_and_filter() {
        let url = test_url().unwrap();
        let store = PostgresStore::connect(&url).await.unwrap();

        let eid = EnclaveId("pg-test-events-enc".into());
        let ev1 = AuditEvent::ReconcileStarted {
            id: Uuid::new_v4(),
            at: Utc::now(),
            dry_run: false,
        };
        let ev2 = AuditEvent::EnclaveProvisioned {
            id: Uuid::new_v4(),
            at: Utc::now(),
            enclave_id: eid.clone(),
        };
        store.append_event(&ev1).await.unwrap();
        store.append_event(&ev2).await.unwrap();

        // Enclave-filtered: only ev2
        let filtered = store.list_events(Some(&eid), 10).await.unwrap();
        assert_eq!(filtered.len(), 1);

        // Unfiltered: both (at minimum)
        let all = store.list_events(None, 100).await.unwrap();
        assert!(all.len() >= 2);
    }

    #[tokio::test]
    #[ignore = "requires TEST_POSTGRES_URL"]
    async fn tf_lock_conflict() {
        let url = test_url().unwrap();
        let store = PostgresStore::connect(&url).await.unwrap();

        let key = format!("pg-test-lock/{}", Uuid::new_v4());
        let lock1 = serde_json::json!({ "ID": "lock-aaa", "Operation": "plan" });
        let lock2 = serde_json::json!({ "ID": "lock-bbb", "Operation": "apply" });

        store.lock_tf_state(&key, lock1).await.unwrap();

        let err = store.lock_tf_state(&key, lock2).await.unwrap_err();
        match err {
            StoreError::LockConflict { holder } => assert_eq!(holder, "lock-aaa"),
            other => panic!("expected LockConflict, got {other:?}"),
        }

        store.unlock_tf_state(&key, "lock-aaa").await.unwrap();
        // Should be lockable again
        let lock3 = serde_json::json!({ "ID": "lock-ccc" });
        store.lock_tf_state(&key, lock3).await.unwrap();
        store.unlock_tf_state(&key, "").await.unwrap(); // force-unlock
    }

    #[tokio::test]
    #[ignore = "requires TEST_POSTGRES_URL"]
    async fn iac_run_list() {
        let url = test_url().unwrap();
        let store = PostgresStore::connect(&url).await.unwrap();

        let eid = EnclaveId("pg-test-iac-enc".into());
        let pid = PartitionId("pg-test-iac-part".into());

        let run = IacRun {
            id: Uuid::new_v4(),
            enclave_id: eid.clone(),
            partition_id: pid.clone(),
            operation: IacOperation::Provision,
            started_at: Utc::now(),
            finished_at: None,
            status: IacRunStatus::Succeeded,
            exit_code: Some(0),
            log: "ok".into(),
            reconcile_run_id: None,
        };
        store.upsert_iac_run(&run).await.unwrap();

        let runs = store.list_iac_runs(&eid, &pid).await.unwrap();
        assert!(!runs.is_empty());
        assert!(runs.iter().any(|r| r.id == run.id));

        let fetched = store.get_iac_run(run.id).await.unwrap().unwrap();
        assert_eq!(fetched.id, run.id);
    }
}
