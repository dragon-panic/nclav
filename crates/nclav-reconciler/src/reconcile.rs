use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::Utc;
use nclav_domain::{Enclave, EnclaveId};
use nclav_store::{AuditEvent, EnclaveState, PartitionState, StateStore};
use nclav_driver::Driver;
use nclav_graph::validate;
use uuid::Uuid;
use tracing::{debug, info};

use crate::error::ReconcileError;
use crate::report::{Change, ReconcileReport, ReconcileRequest};

pub async fn reconcile(
    req: ReconcileRequest,
    store: Arc<dyn StateStore>,
    driver: Arc<dyn Driver>,
) -> Result<ReconcileReport, ReconcileError> {
    let mut report = ReconcileReport::new(req.dry_run);

    // 1. Load YAML
    info!("Loading enclaves from {:?}", req.enclaves_dir);
    let desired_enclaves = nclav_config::load_enclaves(&req.enclaves_dir)?;
    debug!("Loaded {} enclaves", desired_enclaves.len());

    // 2. Validate graph
    info!("Validating enclave graph");
    let resolved = validate(&desired_enclaves)?;
    debug!(
        "Graph valid. Topo order: {:?}",
        resolved.topo_order.iter().map(|n| &n.0).collect::<Vec<_>>()
    );

    // 3. Load actual state
    let actual_states: HashMap<EnclaveId, nclav_store::EnclaveState> = store
        .list_enclaves()
        .await?
        .into_iter()
        .map(|s| (s.desired.id.clone(), s))
        .collect();

    // 4. Diff: detect creates, updates, deletes
    let desired_ids: HashSet<EnclaveId> =
        desired_enclaves.iter().map(|e| e.id.clone()).collect();
    let actual_ids: HashSet<EnclaveId> = actual_states.keys().cloned().collect();

    // Deletes
    for id in actual_ids.difference(&desired_ids) {
        report.changes.push(Change::EnclaveDeleted { id: id.clone() });
    }

    // Creates and updates (in topo order)
    let topo_ids: Vec<EnclaveId> = resolved
        .topo_order
        .iter()
        .map(|n| EnclaveId::new(&n.0))
        .collect();

    // Build a map for quick lookup
    let desired_map: HashMap<&EnclaveId, &Enclave> =
        desired_enclaves.iter().map(|e| (&e.id, e)).collect();

    // Add creates/updates in topo order, then any remaining in arbitrary order
    let mut ordered_desired: Vec<&Enclave> = Vec::new();
    for id in &topo_ids {
        if let Some(enc) = desired_map.get(id) {
            ordered_desired.push(enc);
        }
    }
    for enc in &desired_enclaves {
        if !topo_ids.contains(&enc.id) {
            ordered_desired.push(enc);
        }
    }

    for enc in &ordered_desired {
        let existing = actual_states.get(&enc.id);
        let is_new = existing.is_none();
        let is_changed = existing.map_or(true, |s| s.desired != **enc);

        if is_new {
            report.changes.push(Change::EnclaveCreated { id: enc.id.clone() });
        } else if is_changed {
            report.changes.push(Change::EnclaveUpdated { id: enc.id.clone() });
        }

        for part in &enc.partitions {
            let part_existing = existing.and_then(|s| s.partitions.get(&part.id));
            let part_is_new = part_existing.is_none();
            let part_is_changed = part_existing.map_or(true, |ps| ps.desired != *part);

            if part_is_new {
                report.changes.push(Change::PartitionCreated {
                    enclave_id: enc.id.clone(),
                    partition_id: part.id.clone(),
                });
            } else if part_is_changed {
                report.changes.push(Change::PartitionUpdated {
                    enclave_id: enc.id.clone(),
                    partition_id: part.id.clone(),
                });
            }
        }

        for export in &enc.exports {
            let already_wired = existing
                .and_then(|s| s.export_handles.get(&export.name))
                .is_some();
            if !already_wired {
                report.changes.push(Change::ExportWired {
                    enclave_id: enc.id.clone(),
                    export_name: export.name.clone(),
                });
            }
        }
    }

    // Cross-enclave imports
    for enc in &ordered_desired {
        for import in &enc.imports {
            let existing = actual_states.get(&enc.id);
            let already_wired = existing
                .and_then(|s| s.import_handles.get(&import.alias))
                .is_some();
            if !already_wired {
                report.changes.push(Change::ImportWired {
                    importer_enclave: enc.id.clone(),
                    alias: import.alias.clone(),
                });
            }
        }
        for part in &enc.partitions {
            let existing = actual_states.get(&enc.id);
            for import in &part.imports {
                let already_wired = existing
                    .and_then(|s| s.import_handles.get(&import.alias))
                    .is_some();
                if !already_wired {
                    report.changes.push(Change::ImportWired {
                        importer_enclave: enc.id.clone(),
                        alias: import.alias.clone(),
                    });
                }
            }
        }
    }

    // 5. Dry-run gate
    if req.dry_run {
        info!("Dry run — skipping provisioning");
        return Ok(report);
    }

    // 6. Provision in topo order
    let run_id = Uuid::new_v4();

    store
        .append_event(&AuditEvent::ReconcileStarted {
            id: run_id,
            at: Utc::now(),
            dry_run: false,
        })
        .await?;

    // Handle deletes
    for id in actual_ids.difference(&desired_ids) {
        if let Some(existing) = actual_states.get(id) {
            if let Some(handle) = &existing.enclave_handle {
                let enclave = &existing.desired;
                driver.teardown_enclave(enclave, handle).await?;
            }
            store.delete_enclave(id).await?;
        }
    }

    // Provision creates/updates in topo order
    for enc in &ordered_desired {
        let existing = actual_states.get(&enc.id);
        let _is_changed = existing.map_or(true, |s| s.desired != **enc);

        // Provision enclave
        let enc_result = driver
            .provision_enclave(enc, existing.and_then(|s| s.enclave_handle.as_ref()))
            .await?;

        let mut enc_state = existing.cloned().unwrap_or_else(|| EnclaveState::new((*enc).clone()));
        enc_state.desired = (*enc).clone();
        enc_state.enclave_handle = Some(enc_result.handle.clone());

        // Provision partitions
        for part in &enc.partitions {
            let part_existing = existing.and_then(|s| s.partitions.get(&part.id));
            let _part_is_changed = part_existing.map_or(true, |ps| ps.desired != *part);

            // Resolve template inputs
            let resolved_inputs = resolve_inputs(&part.inputs, &enc_state);

            let part_result = driver
                .provision_partition(
                    enc,
                    part,
                    &resolved_inputs,
                    part_existing.and_then(|ps| ps.partition_handle.as_ref()),
                )
                .await?;

            let part_state = PartitionState {
                desired: part.clone(),
                partition_handle: Some(part_result.handle),
                resolved_outputs: part_result.outputs.clone(),
            };

            enc_state.partitions.insert(part.id.clone(), part_state);

            store
                .append_event(&AuditEvent::PartitionProvisioned {
                    id: Uuid::new_v4(),
                    at: Utc::now(),
                    enclave_id: enc.id.clone(),
                    partition_id: part.id.clone(),
                })
                .await?;
        }

        // Provision exports
        for export in &enc.exports {
            let _already_wired = existing
                .and_then(|s| s.export_handles.get(&export.name))
                .is_some();

            // Gather partition outputs for this export's target partition
            let part_outputs = enc_state
                .partitions
                .get(&export.target_partition)
                .map(|ps| ps.resolved_outputs.clone())
                .unwrap_or_default();

            let export_result = driver
                .provision_export(
                    enc,
                    export,
                    &part_outputs,
                    existing.and_then(|s| s.export_handles.get(&export.name)),
                )
                .await?;

            enc_state
                .export_handles
                .insert(export.name.clone(), export_result.handle);

            store
                .append_event(&AuditEvent::ExportWired {
                    id: Uuid::new_v4(),
                    at: Utc::now(),
                    enclave_id: enc.id.clone(),
                    export_name: export.name.clone(),
                })
                .await?;
        }

        enc_state.last_reconciled_at = Some(Utc::now());

        // Persist enclave state
        store.upsert_enclave(&enc_state).await?;

        store
            .append_event(&AuditEvent::EnclaveProvisioned {
                id: Uuid::new_v4(),
                at: Utc::now(),
                enclave_id: enc.id.clone(),
            })
            .await?;
    }

    // 7. Wire cross-enclave imports (second pass, after all enclaves provisioned)
    for enc in &ordered_desired {
        let mut enc_state = store
            .get_enclave(&enc.id)
            .await?
            .unwrap_or_else(|| EnclaveState::new((*enc).clone()));

        let mut changed = false;

        // Enclave-level imports
        for import in &enc.imports {
            // Get the exporter's export handle
            let exporter_state = store.get_enclave(&import.from).await?;
            if let Some(exporter) = exporter_state {
                if let Some(export_handle) = exporter.export_handles.get(&import.export_name) {
                    let import_result = driver
                        .provision_import(
                            enc,
                            import,
                            export_handle,
                            enc_state.import_handles.get(&import.alias),
                        )
                        .await?;

                    enc_state
                        .import_handles
                        .insert(import.alias.clone(), import_result.handle);

                    store
                        .append_event(&AuditEvent::ImportWired {
                            id: Uuid::new_v4(),
                            at: Utc::now(),
                            importer_enclave: enc.id.clone(),
                            export_name: import.export_name.clone(),
                        })
                        .await?;

                    changed = true;
                }
            }
        }

        // Partition-level imports
        for part in &enc.partitions {
            for import in &part.imports {
                let exporter_state = store.get_enclave(&import.from).await?;
                if let Some(exporter) = exporter_state {
                    if let Some(export_handle) = exporter.export_handles.get(&import.export_name) {
                        let import_result = driver
                            .provision_import(
                                enc,
                                import,
                                export_handle,
                                enc_state.import_handles.get(&import.alias),
                            )
                            .await?;

                        enc_state
                            .import_handles
                            .insert(import.alias.clone(), import_result.handle);

                        store
                            .append_event(&AuditEvent::ImportWired {
                                id: Uuid::new_v4(),
                                at: Utc::now(),
                                importer_enclave: enc.id.clone(),
                                export_name: import.export_name.clone(),
                            })
                            .await?;

                        changed = true;
                    }
                }
            }
        }

        if changed {
            store.upsert_enclave(&enc_state).await?;
        }
    }

    // 8. Final audit event
    store
        .append_event(&AuditEvent::ReconcileCompleted {
            id: run_id,
            at: Utc::now(),
            changes: report.changes.len(),
            dry_run: false,
        })
        .await?;

    info!("Reconcile complete: {} changes", report.changes.len());
    Ok(report)
}

/// Resolve template variables of the form `{{ alias.key }}` in input values.
fn resolve_inputs(
    inputs: &HashMap<String, String>,
    enc_state: &EnclaveState,
) -> HashMap<String, String> {
    inputs
        .iter()
        .map(|(k, v)| {
            let resolved = resolve_template(v, enc_state);
            (k.clone(), resolved)
        })
        .collect()
}

fn resolve_template(template: &str, enc_state: &EnclaveState) -> String {
    // Replace {{ alias.key }} patterns
    let mut result = template.to_string();
    let _re_pattern = "{{ ";

    let mut search_start = 0;
    loop {
        if let Some(start) = result[search_start..].find("{{") {
            let abs_start = search_start + start;
            if let Some(end) = result[abs_start..].find("}}") {
                let abs_end = abs_start + end + 2;
                let placeholder = &result[abs_start..abs_end];
                let inner = placeholder
                    .trim_start_matches("{{")
                    .trim_end_matches("}}")
                    .trim();

                // inner is "alias.key"
                let parts: Vec<&str> = inner.splitn(2, '.').collect();
                if parts.len() == 2 {
                    let alias = parts[0];
                    let key = parts[1];

                    // Look in import_handles (outputs)
                    let resolved_val = enc_state
                        .import_handles
                        .get(alias)
                        .and_then(|h| h.get("outputs"))
                        .and_then(|o| o.get(key))
                        .and_then(|v| v.as_str())
                        .map(String::from);

                    if let Some(val) = resolved_val {
                        result = format!("{}{}{}", &result[..abs_start], val, &result[abs_end..]);
                        search_start = abs_start + val.len();
                        continue;
                    }
                }

                search_start = abs_end;
            } else {
                break;
            }
        } else {
            break;
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use nclav_driver::LocalDriver;
    use nclav_store::InMemoryStore;
    use std::path::Path;

    #[tokio::test]
    async fn dry_run_returns_changes_without_persisting() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../enclaves");
        if !dir.exists() {
            return;
        }

        let store = Arc::new(InMemoryStore::new());
        let driver = Arc::new(LocalDriver::new());

        let req = ReconcileRequest {
            enclaves_dir: dir,
            dry_run: true,
        };

        let report = reconcile(req, store.clone(), driver).await.unwrap();
        assert!(report.dry_run);
        assert!(!report.changes.is_empty());

        // State must not be modified
        let enclaves = store.list_enclaves().await.unwrap();
        assert!(enclaves.is_empty(), "dry run should not persist state");
    }

    #[tokio::test]
    async fn apply_persists_state() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../enclaves");
        if !dir.exists() {
            return;
        }

        let store = Arc::new(InMemoryStore::new());
        let driver = Arc::new(LocalDriver::new());

        let req = ReconcileRequest {
            enclaves_dir: dir.clone(),
            dry_run: false,
        };

        let report = reconcile(req, store.clone(), driver.clone()).await.unwrap();
        assert!(!report.dry_run);
        assert!(!report.changes.is_empty());

        // State must have been persisted
        let enclaves = store.list_enclaves().await.unwrap();
        assert!(!enclaves.is_empty(), "apply should persist state");
    }

    #[tokio::test]
    async fn idempotent_apply() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../enclaves");
        if !dir.exists() {
            return;
        }

        let store = Arc::new(InMemoryStore::new());
        let driver = Arc::new(LocalDriver::new());

        let req = ReconcileRequest {
            enclaves_dir: dir.clone(),
            dry_run: false,
        };

        // First apply
        reconcile(req.clone(), store.clone(), driver.clone()).await.unwrap();

        // Second apply — should find fewer (or zero) changes
        let report2 = reconcile(req, store.clone(), driver).await.unwrap();
        // Changes related to already-provisioned items should be empty/minimal
        let creates: Vec<_> = report2
            .changes
            .iter()
            .filter(|c| matches!(c, Change::EnclaveCreated { .. }))
            .collect();
        assert!(
            creates.is_empty(),
            "second apply should not create enclaves again"
        );
    }
}
