
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use nclav_domain::{EnclaveId, PartitionId};
use nclav_driver::TerraformBackend;
use nclav_reconciler::{reconcile, ReconcileRequest};
use nclav_store::StoreError;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use tracing::warn;
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::AppState;

// ── Health ────────────────────────────────────────────────────────────────────

pub async fn health() -> StatusCode {
    StatusCode::OK
}

pub async fn ready(State(state): State<AppState>) -> Result<StatusCode, ApiError> {
    state.store.list_enclaves().await?;
    Ok(StatusCode::OK)
}

// ── Reconcile ─────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ReconcileBody {
    pub enclaves_dir: String,
    #[serde(default)]
    pub resources_only: bool,
}

pub async fn post_reconcile(
    State(state): State<AppState>,
    Json(body): Json<ReconcileBody>,
) -> Result<Json<Value>, ApiError> {
    let req = ReconcileRequest {
        enclaves_dir: body.enclaves_dir.into(),
        dry_run: false,
        api_base: (*state.api_base).clone(),
        auth_token: state.auth_token.clone(),
        test_mode: false,
        resources_only: body.resources_only,
    };
    let report = reconcile(req, state.store, state.registry).await?;
    Ok(Json(json!(report)))
}

pub async fn post_reconcile_dry_run(
    State(state): State<AppState>,
    Json(body): Json<ReconcileBody>,
) -> Result<Json<Value>, ApiError> {
    let req = ReconcileRequest {
        enclaves_dir: body.enclaves_dir.into(),
        dry_run: true,
        api_base: (*state.api_base).clone(),
        auth_token: state.auth_token.clone(),
        test_mode: false,
        resources_only: body.resources_only,
    };
    let report = reconcile(req, state.store, state.registry).await?;
    Ok(Json(json!(report)))
}

// ── Enclaves ──────────────────────────────────────────────────────────────────

pub async fn list_enclaves(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let enclaves = state.store.list_enclaves().await?;
    Ok(Json(json!(enclaves)))
}

pub async fn get_enclave(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let eid = EnclaveId::new(id);
    let enclave = state
        .store
        .get_enclave(&eid)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("enclave '{}' not found", eid)))?;
    Ok(Json(json!(enclave)))
}

#[derive(Debug, Deserialize, Default)]
pub struct DeleteEnclaveQuery {
    #[serde(default)]
    pub resources_only: bool,
}

pub async fn delete_enclave(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<DeleteEnclaveQuery>,
) -> Result<Json<Value>, ApiError> {
    let eid = EnclaveId::new(&id);
    let existing = state
        .store
        .get_enclave(&eid)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("enclave '{}' not found", id)))?;

    let cloud = existing
        .resolved_cloud
        .clone()
        .unwrap_or_else(|| state.registry.default_cloud.clone());

    let driver = state
        .registry
        .for_cloud(cloud)
        .map_err(|e| ApiError::internal(e.to_string()))?;

    let tf_backend = TerraformBackend {
        api_base: (*state.api_base).clone(),
        auth_token: state.auth_token.clone(),
        store: state.store.clone(),
        test_mode: false,
    };

    let mut errors: Vec<String> = Vec::new();

    if let Some(enc_handle) = &existing.enclave_handle {
        // Teardown IaC partitions first
        let auth_env = driver.auth_env(&existing.desired, enc_handle);
        for (part_id, part_state) in &existing.partitions {
            if let Err(e) = tf_backend
                .teardown(&existing.desired, &part_state.desired, &auth_env, None)
                .await
            {
                warn!(enclave_id = %id, partition_id = %part_id, error = %e, "IaC teardown failed");
                errors.push(format!("teardown {}/{}: {}", id, part_id, e));
            }
            // Clean up partition SA after IaC teardown
            if let Some(handle) = &part_state.partition_handle {
                if let Err(e) = driver
                    .teardown_partition(&existing.desired, &part_state.desired, handle)
                    .await
                {
                    warn!(enclave_id = %id, partition_id = %part_id, error = %e, "partition SA cleanup failed");
                    errors.push(format!("SA cleanup {}/{}: {}", id, part_id, e));
                }
            }
        }

        // Teardown the enclave itself (skip project deletion if resources_only)
        if query.resources_only {
            warn!(enclave_id = %id, "resources_only: skipping project deletion");
        } else if let Err(e) = driver.teardown_enclave(&existing.desired, enc_handle).await {
            warn!(enclave_id = %id, error = %e, "enclave teardown failed");
            errors.push(format!("enclave teardown {}: {}", id, e));
        }
    }

    state.store.delete_enclave(&eid).await?;

    Ok(Json(json!({ "destroyed": id, "errors": errors })))
}

// ── delete_partition ──────────────────────────────────────────────────────────

pub async fn delete_partition(
    State(state): State<AppState>,
    Path((enc_id, part_id)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let eid = EnclaveId::new(&enc_id);
    let pid = PartitionId::new(&part_id);

    let existing = state
        .store
        .get_enclave(&eid)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("enclave '{}' not found", enc_id)))?;

    let part_state = existing
        .partitions
        .get(&pid)
        .cloned()
        .ok_or_else(|| ApiError::not_found(
            format!("partition '{}' not found in enclave '{}'", part_id, enc_id)
        ))?;

    let cloud = existing
        .resolved_cloud
        .clone()
        .unwrap_or_else(|| state.registry.default_cloud.clone());

    let driver = state
        .registry
        .for_cloud(cloud)
        .map_err(|e| ApiError::internal(e.to_string()))?;

    let mut errors: Vec<String> = Vec::new();

    let tf_backend = TerraformBackend {
        api_base:   (*state.api_base).clone(),
        auth_token: state.auth_token.clone(),
        store:      state.store.clone(),
        test_mode:  false,
    };
    let auth_env = existing
        .enclave_handle
        .as_ref()
        .map(|h| driver.auth_env(&existing.desired, h))
        .unwrap_or_default();
    if let Err(e) = tf_backend
        .teardown(&existing.desired, &part_state.desired, &auth_env, None)
        .await
    {
        warn!(enclave_id = %enc_id, partition_id = %part_id, error = %e, "IaC partition teardown failed");
        errors.push(format!("{}", e));
    }
    // Clean up partition SA
    if let Some(handle) = &part_state.partition_handle {
        if let Err(e) = driver
            .teardown_partition(&existing.desired, &part_state.desired, handle)
            .await
        {
            warn!(enclave_id = %enc_id, partition_id = %part_id, error = %e, "partition SA cleanup failed");
            errors.push(format!("SA cleanup: {}", e));
        }
    }

    // Remove partition from store regardless of teardown errors so the operator
    // can fix things manually and re-apply.
    state.store.delete_partition(&eid, &pid).await?;

    // Post-destroy CAI check: list any resources still labeled to this partition.
    let remaining_resources: Vec<String> = if let Some(enc_handle) = &existing.enclave_handle {
        driver
            .list_partition_resources(&existing.desired, enc_handle, &part_state.desired)
            .await
            .unwrap_or_default()
    } else {
        vec![]
    };

    Ok(Json(json!({
        "destroyed":           format!("{}/{}", enc_id, part_id),
        "errors":              errors,
        "remaining_resources": remaining_resources,
    })))
}

pub async fn get_enclave_graph(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let eid = EnclaveId::new(&id);
    let enc_state = state
        .store
        .get_enclave(&eid)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("enclave '{}' not found", eid)))?;

    let enc = &enc_state.desired;
    let nodes: Vec<Value> = enc
        .partitions
        .iter()
        .map(|p| {
            let part_status = enc_state
                .partitions
                .get(&p.id)
                .map(|ps| ps.meta.status.to_string())
                .unwrap_or_else(|| "pending".to_string());
            json!({
                "id": p.id,
                "name": p.name,
                "produces": p.produces,
                "status": part_status,
            })
        })
        .collect();

    let edges: Vec<Value> = enc
        .exports
        .iter()
        .map(|e| {
            json!({
                "from": e.target_partition,
                "export_name": e.name,
                "type": e.export_type,
            })
        })
        .collect();

    Ok(Json(json!({
        "enclave": id,
        "status": enc_state.meta.status,
        "nodes": nodes,
        "edges": edges,
    })))
}

pub async fn get_system_graph(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let all = state.store.list_enclaves().await?;

    let nodes: Vec<Value> = all
        .iter()
        .map(|s| {
            let partitions: Vec<Value> = s.desired.partitions.iter().map(|p| {
                let part_status = s.partitions.get(&p.id)
                    .map(|ps| ps.meta.status.to_string())
                    .unwrap_or_else(|| "pending".to_string());
                json!({
                    "id": p.id,
                    "name": p.name,
                    "produces": p.produces,
                    "status": part_status,
                })
            }).collect();

            json!({
                "id": s.desired.id,
                "name": s.desired.name,
                "cloud": s.desired.cloud,
                "status": s.meta.status,
                "created_at": s.meta.created_at,
                "updated_at": s.meta.updated_at,
                "last_error": s.meta.last_error,
                "partitions": partitions,
            })
        })
        .collect();

    let mut edges: Vec<Value> = Vec::new();
    for s in &all {
        for import in &s.desired.imports {
            edges.push(json!({
                "from": import.from,
                "to": s.desired.id,
                "export": import.export_name,
                "alias": import.alias,
            }));
        }
        for part in &s.desired.partitions {
            for import in &part.imports {
                edges.push(json!({
                    "from": import.from,
                    "to": s.desired.id,
                    "partition": part.id,
                    "export": import.export_name,
                    "alias": import.alias,
                }));
            }
        }
    }

    Ok(Json(json!({ "nodes": nodes, "edges": edges })))
}

// ── Events ────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct EventsQuery {
    pub enclave_id: Option<String>,
    pub limit: Option<u32>,
}

pub async fn list_events(
    State(state): State<AppState>,
    Query(q): Query<EventsQuery>,
) -> Result<Json<Value>, ApiError> {
    let eid = q.enclave_id.as_deref().map(EnclaveId::new);
    let events = state.store.list_events(eid.as_ref(), q.limit.unwrap_or(100)).await?;
    Ok(Json(json!(events)))
}

// ── Terraform HTTP state backend ──────────────────────────────────────────────

pub async fn get_tf_state(
    State(state): State<AppState>,
    Path((enc, part)): Path<(String, String)>,
) -> impl IntoResponse {
    let key = format!("{}/{}", enc, part);
    match state.store.get_tf_state(&key).await {
        Ok(Some(bytes)) => (StatusCode::OK, bytes).into_response(),
        Ok(None) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => ApiError::internal(e.to_string()).into_response(),
    }
}

pub async fn put_tf_state(
    State(state): State<AppState>,
    Path((enc, part)): Path<(String, String)>,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    let key = format!("{}/{}", enc, part);
    state.store.put_tf_state(&key, body.to_vec()).await?;
    Ok(StatusCode::OK)
}

pub async fn delete_tf_state(
    State(state): State<AppState>,
    Path((enc, part)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let key = format!("{}/{}", enc, part);
    state.store.delete_tf_state(&key).await?;
    Ok(StatusCode::OK)
}

pub async fn lock_tf_state(
    State(state): State<AppState>,
    Path((enc, part)): Path<(String, String)>,
    Json(lock_info): Json<Value>,
) -> impl IntoResponse {
    let key = format!("{}/{}", enc, part);
    match state.store.lock_tf_state(&key, lock_info).await {
        Ok(()) => StatusCode::OK.into_response(),
        // Terraform's HTTP backend expects the response body to be the existing
        // lock info JSON directly (not wrapped) — it uses it to show the lock owner.
        Err(StoreError::LockConflict { holder }) => (
            StatusCode::CONFLICT,
            Json(holder),
        )
            .into_response(),
        Err(e) => ApiError::internal(e.to_string()).into_response(),
    }
}

pub async fn unlock_tf_state(
    State(state): State<AppState>,
    Path((enc, part)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> Result<StatusCode, ApiError> {
    let key = format!("{}/{}", enc, part);
    // Parse the lock ID from the body if present (Terraform's normal unlock path).
    // An absent or empty body means force-unlock — clear the lock regardless of ID.
    let lock_id = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|v| v.get("ID").and_then(|v| v.as_str()).map(String::from))
        .unwrap_or_default();
    state.store.unlock_tf_state(&key, &lock_id).await?;
    Ok(StatusCode::OK)
}

// ── IaC run logs ──────────────────────────────────────────────────────────────

pub async fn list_iac_runs(
    State(state): State<AppState>,
    Path((id, part)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let eid = EnclaveId::new(&id);
    let pid = PartitionId::new(&part);
    let runs = state.store.list_iac_runs(&eid, &pid).await?;
    Ok(Json(json!(runs)))
}

pub async fn get_latest_iac_run(
    State(state): State<AppState>,
    Path((id, part)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let eid = EnclaveId::new(&id);
    let pid = PartitionId::new(&part);
    let runs = state.store.list_iac_runs(&eid, &pid).await?;
    let latest = runs
        .into_iter()
        .max_by_key(|r| r.started_at)
        .ok_or_else(|| ApiError::not_found("no IaC runs found for this partition"))?;
    Ok(Json(json!(latest)))
}

pub async fn get_iac_run(
    State(state): State<AppState>,
    Path((_id, _part, run_id)): Path<(String, String, String)>,
) -> Result<Json<Value>, ApiError> {
    let run_uuid = Uuid::parse_str(&run_id)
        .map_err(|_| ApiError::bad_request(format!("invalid run ID: {}", run_id)))?;
    let run = state
        .store
        .get_iac_run(run_uuid)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("IaC run '{}' not found", run_id)))?;
    Ok(Json(json!(run)))
}

// ── Status ────────────────────────────────────────────────────────────────────

pub async fn status(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let enclaves = state.store.list_enclaves().await?;

    let mut by_status: HashMap<String, usize> = HashMap::new();
    let mut errors: Vec<Value> = Vec::new();

    for s in &enclaves {
        *by_status.entry(s.meta.status.to_string()).or_default() += 1;

        if let Some(err) = &s.meta.last_error {
            errors.push(json!({
                "enclave_id": s.desired.id,
                "message": err.message,
                "occurred_at": err.occurred_at,
            }));
        }
        for (pid, ps) in &s.partitions {
            if let Some(err) = &ps.meta.last_error {
                errors.push(json!({
                    "enclave_id": s.desired.id,
                    "partition_id": pid,
                    "message": err.message,
                    "occurred_at": err.occurred_at,
                }));
            }
        }
    }

    let last_reconciled_at = enclaves.iter().filter_map(|s| s.meta.updated_at).max();
    let default_cloud = &state.registry.default_cloud;
    let active_drivers: Vec<String> = {
        let mut clouds = state.registry.active_clouds();
        clouds.sort_by_key(|c| c.to_string());
        clouds.iter().map(|c| c.to_string()).collect()
    };

    Ok(Json(json!({
        "enclave_count": enclaves.len(),
        "by_status": by_status,
        "last_reconciled_at": last_reconciled_at,
        "errors": errors,
        "default_cloud": default_cloud,
        "active_drivers": active_drivers,
    })))
}

// ── Orphans ────────────────────────────────────────────────────────────────────

pub async fn list_orphans(
    State(state): State<AppState>,
) -> Result<Json<Value>, ApiError> {
    let enclaves = state.store.list_enclaves().await?;
    let mut all_orphans: Vec<Value> = Vec::new();

    for enc_state in &enclaves {
        let Some(enc_handle) = &enc_state.enclave_handle else { continue };
        let cloud = enc_state.resolved_cloud.clone()
            .unwrap_or_else(|| state.registry.default_cloud.clone());
        let Ok(driver) = state.registry.for_cloud(cloud) else { continue };

        let known: Vec<&str> = enc_state.partitions.keys()
            .map(|id| id.as_str())
            .collect();

        if let Ok(orphans) = driver
            .list_orphaned_resources(&enc_state.desired, enc_handle, &known)
            .await
        {
            for o in orphans {
                all_orphans.push(json!({
                    "enclave":         enc_state.desired.id.as_str(),
                    "nclav_partition": o.nclav_partition,
                    "resource_type":   o.resource_type,
                    "resource_name":   o.resource_name,
                }));
            }
        }
    }

    Ok(Json(json!({ "orphans": all_orphans })))
}
