use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::Utc;
use nclav_domain::{Enclave, EnclaveId, PartitionBackend};
use nclav_store::{
    AuditEvent, EnclaveState, PartitionState, ProvisioningStatus, StateStore,
    compute_desired_hash,
};
use nclav_driver::{DriverRegistry, TerraformBackend};
use nclav_graph::validate;
use uuid::Uuid;
use tracing::{debug, info, warn};

use crate::error::ReconcileError;
use crate::report::{Change, ReconcileReport, ReconcileRequest};

pub async fn reconcile(
    req: ReconcileRequest,
    store: Arc<dyn StateStore>,
    registry: Arc<DriverRegistry>,
) -> Result<ReconcileReport, ReconcileError> {
    let tf_backend = Arc::new(TerraformBackend {
        api_base: req.api_base.clone(),
        auth_token: req.auth_token.clone(),
        store: store.clone(),
    });
    let mut report = ReconcileReport::new(req.dry_run);

    // 1. Load YAML
    info!("Loading enclaves from {:?}", req.enclaves_dir);
    let desired_enclaves = nclav_config::load_enclaves(&req.enclaves_dir)?;
    debug!("Loaded {} enclaves", desired_enclaves.len());

    // 2. Validate graph — abort entire reconcile on structural errors
    info!("Validating enclave graph");
    let resolved = validate(&desired_enclaves)?;
    debug!(
        "Graph valid. Topo order: {:?}",
        resolved.topo_order.iter().map(|n| &n.0).collect::<Vec<_>>()
    );

    // 3. Load actual state
    let actual_states: HashMap<EnclaveId, EnclaveState> = store
        .list_enclaves()
        .await?
        .into_iter()
        .map(|s| (s.desired.id.clone(), s))
        .collect();

    // 4. Diff: compute desired vs actual and collect changes
    let desired_ids: HashSet<EnclaveId> =
        desired_enclaves.iter().map(|e| e.id.clone()).collect();
    let actual_ids: HashSet<EnclaveId> = actual_states.keys().cloned().collect();

    // Removals
    for id in actual_ids.difference(&desired_ids) {
        report.changes.push(Change::EnclaveDeleted { id: id.clone() });
    }

    // Build ordered list of desired enclaves (topo order first)
    let topo_ids: Vec<EnclaveId> = resolved
        .topo_order
        .iter()
        .map(|n| EnclaveId::new(&n.0))
        .collect();
    let desired_map: HashMap<&EnclaveId, &Enclave> =
        desired_enclaves.iter().map(|e| (&e.id, e)).collect();

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
        let enc_hash = compute_desired_hash(enc);
        let hash_unchanged = existing
            .and_then(|s| s.meta.desired_hash.as_deref())
            .map_or(false, |h| h == enc_hash);

        if existing.is_none() {
            report.changes.push(Change::EnclaveCreated { id: enc.id.clone() });
        } else if !hash_unchanged {
            report.changes.push(Change::EnclaveUpdated { id: enc.id.clone() });
        }

        for part in &enc.partitions {
            let part_hash = compute_desired_hash(part);
            let part_existing = existing.and_then(|s| s.partitions.get(&part.id));
            let part_hash_unchanged = part_existing
                .and_then(|ps| ps.meta.desired_hash.as_deref())
                .map_or(false, |h| h == part_hash);

            if part_existing.is_none() {
                report.changes.push(Change::PartitionCreated {
                    enclave_id: enc.id.clone(),
                    partition_id: part.id.clone(),
                });
            } else if !part_hash_unchanged {
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

    // Cross-enclave import changes
    for enc in &ordered_desired {
        let existing = actual_states.get(&enc.id);
        for import in &enc.imports {
            if existing.and_then(|s| s.import_handles.get(&import.alias)).is_none() {
                report.changes.push(Change::ImportWired {
                    importer_enclave: enc.id.clone(),
                    alias: import.alias.clone(),
                });
            }
        }
        for part in &enc.partitions {
            for import in &part.imports {
                if existing.and_then(|s| s.import_handles.get(&import.alias)).is_none() {
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

    let run_id = Uuid::new_v4();
    store
        .append_event(&AuditEvent::ReconcileStarted {
            id: run_id,
            at: Utc::now(),
            dry_run: false,
        })
        .await?;

    // 6. Teardowns for removed enclaves
    for id in actual_ids.difference(&desired_ids) {
        if let Some(existing) = actual_states.get(id) {
            // Use resolved_cloud from persisted state so teardown works after YAML removal
            let cloud = existing.resolved_cloud.clone().unwrap_or_else(|| registry.default_cloud.clone());
            if let Ok(driver) = registry.for_cloud(cloud) {
                if let Some(handle) = &existing.enclave_handle {
                    // Teardown IaC partitions before tearing down the enclave itself
                    let auth_env = driver.auth_env(&existing.desired, handle);
                    for (part_id, part_state) in &existing.partitions {
                        match &part_state.desired.backend {
                            PartitionBackend::Terraform(_) | PartitionBackend::OpenTofu(_) => {
                                if let Err(e) = tf_backend
                                    .teardown(&existing.desired, &part_state.desired, &auth_env, Some(run_id))
                                    .await
                                {
                                    warn!(
                                        enclave_id = %id,
                                        partition_id = %part_id,
                                        error = %e,
                                        "IaC partition teardown failed during enclave removal"
                                    );
                                    report.errors.push(format!(
                                        "teardown {}/{}: {}", id, part_id, e
                                    ));
                                }
                                // Clean up the partition SA after terraform destroy
                                if let Some(handle) = &part_state.partition_handle {
                                    if let Err(e) = driver
                                        .teardown_partition(&existing.desired, &part_state.desired, handle)
                                        .await
                                    {
                                        warn!(
                                            enclave_id = %id,
                                            partition_id = %part_id,
                                            error = %e,
                                            "Partition SA cleanup failed during enclave removal"
                                        );
                                    }
                                }
                            }
                            PartitionBackend::Managed => {}
                        }
                    }

                    driver.teardown_enclave(&existing.desired, handle).await?;
                }
            }
            store.delete_enclave(id).await?;
        }
    }

    // 7. Provision / update in topo order
    for enc in &ordered_desired {
        // Resolve the driver for this enclave — per-enclave error, not global abort
        let driver = match registry.for_enclave(enc) {
            Ok(d) => d,
            Err(e) => {
                let msg = e.to_string();
                warn!(enclave_id = %enc.id, error = %msg, "no driver for enclave cloud");
                report.errors.push(format!("enclave {}: {}", enc.id, msg));
                continue;
            }
        };

        let existing = actual_states.get(&enc.id);
        let enc_hash = compute_desired_hash(enc);
        let _hash_unchanged = existing
            .and_then(|s| s.meta.desired_hash.as_deref())
            .map_or(false, |h| h == enc_hash);

        // Initialise or clone state
        let mut enc_state = existing
            .cloned()
            .unwrap_or_else(|| EnclaveState::new((*enc).clone()));
        enc_state.desired = (*enc).clone();

        // Stamp resolved cloud before the first upsert so teardown always knows which driver to use
        enc_state.resolved_cloud = Some(registry.resolved_cloud(enc));

        // Mark in-flight status before driver call
        enc_state.meta.status = if existing.is_some() {
            ProvisioningStatus::Updating
        } else {
            ProvisioningStatus::Provisioning
        };
        store.upsert_enclave(&enc_state).await?;

        // Provision enclave
        match driver
            .provision_enclave(enc, existing.and_then(|s| s.enclave_handle.as_ref()))
            .await
        {
            Ok(result) => {
                let now = Utc::now();
                enc_state.enclave_handle = Some(result.handle);
                enc_state.meta.mark_active(now, enc_hash);
            }
            Err(e) => {
                let msg = e.to_string();
                warn!(enclave_id = %enc.id, error = %msg, "enclave provision failed");
                enc_state.meta.mark_error(Utc::now(), msg.clone());
                store.upsert_enclave(&enc_state).await?;
                store
                    .append_event(&AuditEvent::EnclaveError {
                        id: Uuid::new_v4(),
                        at: Utc::now(),
                        enclave_id: enc.id.clone(),
                        message: msg.clone(),
                    })
                    .await?;
                report.errors.push(format!("enclave {}: {}", enc.id, msg));
                continue; // skip partitions for this enclave
            }
        }

        // Provision partitions
        for part in &enc.partitions {
            let part_hash = compute_desired_hash(part);
            let part_existing = enc_state.partitions.get(&part.id).cloned();
            let part_hash_unchanged = part_existing
                .as_ref()
                .and_then(|ps| ps.meta.desired_hash.as_deref())
                .map_or(false, |h| h == part_hash);

            if part_hash_unchanged {
                debug!(partition_id = %part.id, "skipping unchanged partition");
                continue;
            }

            // context_vars powers {{ nclav_* }} template substitution for all backends
            let context_vars = enc_state
                .enclave_handle
                .as_ref()
                .map(|h| driver.context_vars(enc, h))
                .unwrap_or_default();
            let resolved_inputs = resolve_inputs(&part.inputs, &enc_state, &context_vars);

            let mut part_state = part_existing
                .unwrap_or_else(|| PartitionState::new(part.clone()));
            part_state.desired = part.clone();
            part_state.meta.status = if part_state.partition_handle.is_some() {
                ProvisioningStatus::Updating
            } else {
                ProvisioningStatus::Provisioning
            };
            enc_state.partitions.insert(part.id.clone(), part_state.clone());
            store.upsert_enclave(&enc_state).await?;

            let provision_result = match &part.backend {
                PartitionBackend::Managed => {
                    driver
                        .provision_partition(enc, part, &resolved_inputs, part_state.partition_handle.as_ref())
                        .await
                        .map_err(|e| e.to_string())
                }
                PartitionBackend::Terraform(_) | PartitionBackend::OpenTofu(_) => {
                    // 1. Create partition SA (returns a handle containing "partition_sa").
                    let sa_result = driver
                        .provision_partition(enc, part, &resolved_inputs, part_state.partition_handle.as_ref())
                        .await
                        .map_err(|e| e.to_string());

                    match sa_result {
                        Err(e) => Err(e),
                        Ok(sa_provision) => {
                            // Persist the SA handle immediately so partition_sa survives
                            // the next reconcile even if Terraform subsequently fails.
                            {
                                let ps = enc_state.partitions
                                    .entry(part.id.clone())
                                    .or_insert_with(|| PartitionState::new(part.clone()));
                                ps.partition_handle = Some(sa_provision.handle.clone());
                            }
                            store.upsert_enclave(&enc_state).await.ok();

                            // 2. Build auth_env, override GOOGLE_IMPERSONATE_SERVICE_ACCOUNT
                            //    with the partition SA so Terraform runs under it.
                            //    Only in SA-key mode (GOOGLE_APPLICATION_CREDENTIALS present);
                            //    in ADC mode the operator's credentials run Terraform directly.
                            let mut auth_env = enc_state
                                .enclave_handle
                                .as_ref()
                                .map(|h| driver.auth_env(enc, h))
                                .unwrap_or_default();
                            if auth_env.contains_key("GOOGLE_APPLICATION_CREDENTIALS") {
                                if let Some(sa) = sa_provision.handle["partition_sa"].as_str() {
                                    auth_env.insert(
                                        "GOOGLE_IMPERSONATE_SERVICE_ACCOUNT".into(),
                                        sa.to_string(),
                                    );
                                }
                            }

                            // 3. Run Terraform under the partition SA identity.
                            tf_backend
                                .provision(enc, part, &resolved_inputs, &auth_env, Some(run_id))
                                .await
                                .map_err(|e| e.to_string())
                                // Merge the SA handle fields into the Terraform handle for storage.
                                .map(|mut tf_result| {
                                    if let Some(sa) = sa_provision.handle["partition_sa"].as_str() {
                                        tf_result.handle["partition_sa"] = serde_json::json!(sa);
                                    }
                                    tf_result
                                })
                        }
                    }
                }
            };

            match provision_result {
                Ok(result) => {
                    let now = Utc::now();
                    let ps = enc_state.partitions.entry(part.id.clone()).or_insert_with(|| PartitionState::new(part.clone()));
                    ps.partition_handle = Some(result.handle);
                    ps.resolved_outputs = result.outputs;
                    ps.meta.mark_active(now, part_hash);

                    store
                        .append_event(&AuditEvent::PartitionProvisioned {
                            id: Uuid::new_v4(),
                            at: Utc::now(),
                            enclave_id: enc.id.clone(),
                            partition_id: part.id.clone(),
                        })
                        .await?;
                }
                Err(msg) => {
                    warn!(partition_id = %part.id, error = %msg, "partition provision failed");
                    let ps = enc_state.partitions.entry(part.id.clone()).or_insert_with(|| PartitionState::new(part.clone()));
                    ps.meta.mark_error(Utc::now(), msg.clone());

                    store
                        .append_event(&AuditEvent::PartitionError {
                            id: Uuid::new_v4(),
                            at: Utc::now(),
                            enclave_id: enc.id.clone(),
                            partition_id: part.id.clone(),
                            message: msg.clone(),
                        })
                        .await?;
                    report.errors.push(format!(
                        "partition {}/{}: {}", enc.id, part.id, msg
                    ));
                    // Continue with remaining partitions
                }
            }
        }

        // Provision exports
        for export in &enc.exports {
            let part_outputs = enc_state
                .partitions
                .get(&export.target_partition)
                .map(|ps| ps.resolved_outputs.clone())
                .unwrap_or_default();

            match driver
                .provision_export(
                    enc,
                    export,
                    &part_outputs,
                    enc_state.export_handles.get(&export.name),
                )
                .await
            {
                Ok(result) => {
                    enc_state.export_handles.insert(export.name.clone(), result.handle);
                    store
                        .append_event(&AuditEvent::ExportWired {
                            id: Uuid::new_v4(),
                            at: Utc::now(),
                            enclave_id: enc.id.clone(),
                            export_name: export.name.clone(),
                        })
                        .await?;
                }
                Err(e) => {
                    let msg = e.to_string();
                    warn!(export = %export.name, error = %msg, "export provision failed");
                    report.errors.push(format!("export {}/{}: {}", enc.id, export.name, msg));
                }
            }
        }

        store
            .append_event(&AuditEvent::EnclaveProvisioned {
                id: Uuid::new_v4(),
                at: Utc::now(),
                enclave_id: enc.id.clone(),
            })
            .await?;

        store.upsert_enclave(&enc_state).await?;
    }

    // 8. Wire cross-enclave imports (second pass, after all enclaves provisioned)
    for enc in &ordered_desired {
        // Use the importer's driver for import wiring
        let driver = match registry.for_enclave(enc) {
            Ok(d) => d,
            Err(_) => continue, // already logged in step 7
        };

        let mut enc_state = match store.get_enclave(&enc.id).await? {
            Some(s) => s,
            None => continue,
        };
        let mut changed = false;

        for import in enc.imports.iter().chain(
            enc.partitions.iter().flat_map(|p| p.imports.iter())
        ) {
            if enc_state.import_handles.contains_key(&import.alias) {
                continue; // already wired
            }
            let exporter_state = store.get_enclave(&import.from).await?;
            if let Some(exporter) = exporter_state {
                if let Some(export_handle) = exporter.export_handles.get(&import.export_name) {
                    match driver
                        .provision_import(
                            enc,
                            import,
                            export_handle,
                            enc_state.import_handles.get(&import.alias),
                        )
                        .await
                    {
                        Ok(result) => {
                            enc_state.import_handles.insert(import.alias.clone(), result.handle);
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
                        Err(e) => {
                            let msg = e.to_string();
                            warn!(alias = %import.alias, error = %msg, "import wiring failed");
                            report.errors.push(format!(
                                "import {}/{}: {}", enc.id, import.alias, msg
                            ));
                        }
                    }
                }
            }
        }

        if changed {
            store.upsert_enclave(&enc_state).await?;
        }
    }

    // 9. Final audit event
    store
        .append_event(&AuditEvent::ReconcileCompleted {
            id: run_id,
            at: Utc::now(),
            changes: report.changes.len(),
            dry_run: false,
        })
        .await?;

    info!(
        changes = report.changes.len(),
        errors = report.errors.len(),
        "Reconcile complete"
    );
    Ok(report)
}

/// Resolve template variables in `inputs:` values.
///
/// Two forms are supported:
/// - `{{ alias.key }}` — resolved from cross-partition import handles
/// - `{{ nclav_token }}` (no dot) — resolved from `context_vars` (e.g. `nclav_project_id`)
fn resolve_inputs(
    inputs: &HashMap<String, String>,
    enc_state: &EnclaveState,
    context_vars: &HashMap<String, String>,
) -> HashMap<String, String> {
    inputs
        .iter()
        .map(|(k, v)| (k.clone(), resolve_template(v, enc_state, context_vars)))
        .collect()
}

fn resolve_template(
    template: &str,
    enc_state: &EnclaveState,
    context_vars: &HashMap<String, String>,
) -> String {
    let mut result = template.to_string();
    let mut search_start = 0;
    loop {
        let Some(start) = result[search_start..].find("{{") else { break };
        let abs_start = search_start + start;
        let Some(end) = result[abs_start..].find("}}") else { break };
        let abs_end = abs_start + end + 2;

        let inner = result[abs_start + 2..abs_end - 2].trim();
        let parts: Vec<&str> = inner.splitn(2, '.').collect();
        if parts.len() == 2 {
            // {{ alias.key }} — cross-partition import
            let alias = parts[0];
            let key = parts[1];
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
        } else {
            // {{ token }} — single-token lookup in context_vars (e.g. {{ nclav_project_id }})
            if let Some(val) = context_vars.get(inner) {
                let val = val.clone();
                result = format!("{}{}{}", &result[..abs_start], val, &result[abs_end..]);
                search_start = abs_start + val.len();
                continue;
            }
        }
        search_start = abs_end;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use nclav_domain::CloudTarget;
    use nclav_driver::{DriverRegistry, LocalDriver};
    use nclav_store::{InMemoryStore, ProvisioningStatus};
    use std::path::Path;

    fn test_registry() -> Arc<DriverRegistry> {
        // Register LocalDriver for both Local and Gcp so fixture YAMLs with
        // cloud: gcp work in tests without real GCP credentials.
        let driver = Arc::new(LocalDriver::new());
        let mut registry = DriverRegistry::new(CloudTarget::Local);
        registry.register(CloudTarget::Local, driver.clone());
        registry.register(CloudTarget::Gcp, driver);
        Arc::new(registry)
    }

    #[tokio::test]
    async fn dry_run_returns_changes_without_persisting() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/enclaves");
        if !dir.exists() { return; }

        let store = Arc::new(InMemoryStore::new());
        let registry = test_registry();
        let req = ReconcileRequest { enclaves_dir: dir, dry_run: true, ..Default::default() };

        let report = reconcile(req, store.clone(), registry).await.unwrap();
        assert!(report.dry_run);
        assert!(!report.changes.is_empty());
        assert!(store.list_enclaves().await.unwrap().is_empty(), "dry run must not persist");
    }

    #[tokio::test]
    async fn apply_sets_active_status() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/enclaves");
        if !dir.exists() { return; }

        let store = Arc::new(InMemoryStore::new());
        let registry = test_registry();
        let req = ReconcileRequest { enclaves_dir: dir, dry_run: false, ..Default::default() };

        let report = reconcile(req, store.clone(), registry).await.unwrap();
        assert!(report.errors.is_empty(), "expected no errors: {:?}", report.errors);

        for enc_state in store.list_enclaves().await.unwrap() {
            assert_eq!(
                enc_state.meta.status,
                ProvisioningStatus::Active,
                "enclave {} should be Active",
                enc_state.desired.id
            );
            assert!(enc_state.meta.created_at.is_some());
            assert!(enc_state.meta.desired_hash.is_some());
            assert!(enc_state.resolved_cloud.is_some(), "resolved_cloud should be set");
            for (pid, ps) in &enc_state.partitions {
                assert_eq!(
                    ps.meta.status,
                    ProvisioningStatus::Active,
                    "partition {} should be Active",
                    pid
                );
                assert!(ps.meta.created_at.is_some());
            }
        }
    }

    #[tokio::test]
    async fn idempotent_apply_skips_unchanged() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/enclaves");
        if !dir.exists() { return; }

        let store = Arc::new(InMemoryStore::new());
        let registry = test_registry();
        let req = ReconcileRequest { enclaves_dir: dir.clone(), dry_run: false, ..Default::default() };

        reconcile(req.clone(), store.clone(), registry.clone()).await.unwrap();
        let report2 = reconcile(req, store.clone(), registry).await.unwrap();

        // No creates on second run — hash-matched resources are skipped
        let creates: Vec<_> = report2.changes.iter()
            .filter(|c| matches!(c, Change::EnclaveCreated { .. }))
            .collect();
        assert!(creates.is_empty(), "second apply should not create enclaves again");
    }
}
