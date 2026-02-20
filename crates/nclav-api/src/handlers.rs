
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use nclav_domain::EnclaveId;
use nclav_reconciler::{reconcile, ReconcileRequest};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;

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
}

pub async fn post_reconcile(
    State(state): State<AppState>,
    Json(body): Json<ReconcileBody>,
) -> Result<Json<Value>, ApiError> {
    let req = ReconcileRequest { enclaves_dir: body.enclaves_dir.into(), dry_run: false };
    let report = reconcile(req, state.store, state.driver).await?;
    Ok(Json(json!(report)))
}

pub async fn post_reconcile_dry_run(
    State(state): State<AppState>,
    Json(body): Json<ReconcileBody>,
) -> Result<Json<Value>, ApiError> {
    let req = ReconcileRequest { enclaves_dir: body.enclaves_dir.into(), dry_run: true };
    let report = reconcile(req, state.store, state.driver).await?;
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

    Ok(Json(json!({
        "enclave_count": enclaves.len(),
        "by_status": by_status,
        "last_reconciled_at": last_reconciled_at,
        "errors": errors,
    })))
}
