use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use nclav_domain::{Enclave, Partition, PartitionBackend};
use nclav_store::{IacOperation, IacRun, IacRunStatus, StateStore};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::driver::{ObservedState, ProvisionResult};
use crate::error::DriverError;
use crate::Handle;

// ── TerraformBackend ──────────────────────────────────────────────────────────

/// Executes IaC-backed partitions by invoking the `terraform` or `tofu` binary.
///
/// Responsibilities:
/// - Maintain a workspace under `~/.nclav/workspaces/{enclave_id}/{partition_id}/`
/// - Symlink the partition's `.tf` files into the workspace
/// - Generate `nclav_backend.tf` and `nclav_context.auto.tfvars`
/// - Run `terraform init` + `terraform apply` (or `destroy`)
/// - Capture combined stdout+stderr into an [`IacRun`] log record
/// - Extract declared outputs from `terraform output -json`
pub struct TerraformBackend {
    /// nclav API base URL, used to configure the Terraform HTTP state backend.
    pub api_base: String,
    /// nclav auth token, passed as `TF_HTTP_PASSWORD` to the subprocess.
    pub auth_token: Arc<String>,
    /// Store for persisting [`IacRun`] log records.
    pub store: Arc<dyn StateStore>,
}

impl TerraformBackend {
    /// Provision (create or update) a terraform-backed partition.
    pub async fn provision(
        &self,
        enclave: &Enclave,
        partition: &Partition,
        resolved_inputs: &HashMap<String, String>,
        context_vars: &HashMap<String, String>,
        auth_env: &HashMap<String, String>,
        reconcile_run_id: Option<Uuid>,
    ) -> Result<ProvisionResult, DriverError> {
        let (binary, tf_config) = extract_tf_config(partition)?;
        let binary = binary.as_str();
        let workspace = self.workspace_dir(&enclave.id.0, &partition.id.0);

        self.setup_workspace(&workspace, &tf_config.dir).await?;
        self.write_backend_tf(&workspace, &enclave.id.0, &partition.id.0)?;
        self.write_tfvars(&workspace, resolved_inputs, context_vars, enclave, partition)?;

        let operation = IacOperation::Provision;
        let mut log = String::new();

        // terraform init
        let init_log = self
            .run_tf(
                binary,
                &workspace,
                &[
                    "init",
                    "-reconfigure",
                    "-no-color",
                    &format!(
                        "-backend-config=address={}/terraform/state/{}/{}",
                        self.api_base.trim_end_matches('/'),
                        enclave.id.0,
                        partition.id.0
                    ),
                    &format!(
                        "-backend-config=lock_address={}/terraform/state/{}/{}/lock",
                        self.api_base.trim_end_matches('/'),
                        enclave.id.0,
                        partition.id.0
                    ),
                    &format!(
                        "-backend-config=unlock_address={}/terraform/state/{}/{}/lock",
                        self.api_base.trim_end_matches('/'),
                        enclave.id.0,
                        partition.id.0
                    ),
                    "-backend-config=lock_method=POST",
                    "-backend-config=unlock_method=DELETE",
                    "-backend-config=username=nclav",
                ],
                auth_env,
            )
            .await;

        let (init_exit, init_output) = match init_log {
            Ok(out) => out,
            Err(e) => {
                let msg = e.to_string();
                self.write_run(
                    enclave, partition, operation, reconcile_run_id,
                    msg.clone(), Some(1),
                )
                .await;
                return Err(DriverError::ProvisionFailed(format!("terraform init: {}", msg)));
            }
        };

        log.push_str("=== terraform init ===\n");
        log.push_str(&init_output);

        if init_exit != 0 {
            self.write_run(
                enclave, partition, operation, reconcile_run_id,
                log.clone(), Some(init_exit),
            )
            .await;
            return Err(DriverError::ProvisionFailed(format!(
                "terraform init exited with code {}", init_exit
            )));
        }

        // terraform apply
        let apply_log = self
            .run_tf(binary, &workspace, &["apply", "-auto-approve", "-no-color"], auth_env)
            .await;

        let (apply_exit, apply_output) = match apply_log {
            Ok(out) => out,
            Err(e) => {
                let msg = e.to_string();
                log.push_str("\n=== terraform apply ===\n");
                log.push_str(&msg);
                self.write_run(
                    enclave, partition, operation, reconcile_run_id,
                    log, Some(1),
                )
                .await;
                return Err(DriverError::ProvisionFailed(format!("terraform apply: {}", msg)));
            }
        };

        log.push_str("\n=== terraform apply ===\n");
        log.push_str(&apply_output);

        if apply_exit != 0 {
            self.write_run(
                enclave, partition, operation, reconcile_run_id,
                log, Some(apply_exit),
            )
            .await;
            return Err(DriverError::ProvisionFailed(format!(
                "terraform apply exited with code {}", apply_exit
            )));
        }

        // Read outputs
        let outputs = self.read_outputs(binary, &workspace, &partition.declared_outputs, auth_env).await?;

        self.write_run(
            enclave, partition, operation, reconcile_run_id,
            log, Some(0),
        )
        .await;

        let handle = serde_json::json!({
            "backend": binary.to_string(),
            "workspace": workspace.display().to_string(),
            "enclave_id": enclave.id.0,
            "partition_id": partition.id.0,
        });

        Ok(ProvisionResult { handle, outputs })
    }

    /// Tear down a terraform-backed partition via `terraform destroy`.
    pub async fn teardown(
        &self,
        enclave: &Enclave,
        partition: &Partition,
        auth_env: &HashMap<String, String>,
        reconcile_run_id: Option<Uuid>,
    ) -> Result<(), DriverError> {
        let (binary, _) = extract_tf_config(partition)?;
        let binary = binary.as_str();
        let workspace = self.workspace_dir(&enclave.id.0, &partition.id.0);

        if !workspace.exists() {
            debug!(
                enclave_id = %enclave.id, partition_id = %partition.id,
                "no workspace found; nothing to destroy"
            );
            return Ok(());
        }

        let mut log = String::new();

        let destroy_log = self
            .run_tf(binary, &workspace, &["destroy", "-auto-approve", "-no-color"], auth_env)
            .await;

        let (exit_code, output) = match destroy_log {
            Ok(out) => out,
            Err(e) => {
                let msg = e.to_string();
                log.push_str(&msg);
                self.write_run(
                    enclave, partition, IacOperation::Teardown, reconcile_run_id,
                    log, Some(1),
                )
                .await;
                return Err(DriverError::TeardownFailed(format!("terraform destroy: {}", msg)));
            }
        };

        log.push_str("=== terraform destroy ===\n");
        log.push_str(&output);

        if exit_code != 0 {
            self.write_run(
                enclave, partition, IacOperation::Teardown, reconcile_run_id,
                log, Some(exit_code),
            )
            .await;
            return Err(DriverError::TeardownFailed(format!(
                "terraform destroy exited with code {}", exit_code
            )));
        }

        self.write_run(
            enclave, partition, IacOperation::Teardown, reconcile_run_id,
            log, Some(0),
        )
        .await;

        Ok(())
    }

    /// Observe an IaC-backed partition by reading its current outputs.
    pub async fn observe(
        &self,
        enclave: &Enclave,
        partition: &Partition,
        auth_env: &HashMap<String, String>,
        handle: &Handle,
    ) -> Result<ObservedState, DriverError> {
        let (binary, _) = extract_tf_config(partition)?;
        let binary = binary.as_str();
        let workspace = self.workspace_dir(&enclave.id.0, &partition.id.0);

        if !workspace.exists() {
            return Ok(ObservedState {
                exists: false,
                healthy: false,
                outputs: HashMap::new(),
                raw: handle.clone(),
            });
        }

        match self.read_outputs(binary, &workspace, &partition.declared_outputs, auth_env).await {
            Ok(outputs) => Ok(ObservedState {
                exists: true,
                healthy: true,
                outputs,
                raw: handle.clone(),
            }),
            Err(_) => Ok(ObservedState {
                exists: false,
                healthy: false,
                outputs: HashMap::new(),
                raw: handle.clone(),
            }),
        }
    }

    // ── Workspace helpers ─────────────────────────────────────────────────────

    fn workspace_dir(&self, enclave_id: &str, partition_id: &str) -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        PathBuf::from(home)
            .join(".nclav")
            .join("workspaces")
            .join(enclave_id)
            .join(partition_id)
    }

    async fn setup_workspace(&self, workspace: &Path, source_dir: &Path) -> Result<(), DriverError> {
        tokio::fs::create_dir_all(workspace)
            .await
            .map_err(|e| DriverError::Internal(format!("create workspace dir: {}", e)))?;

        // Symlink all .tf files from the source directory into the workspace.
        // Existing symlinks pointing to the same target are left as-is.
        let mut read_dir = tokio::fs::read_dir(source_dir)
            .await
            .map_err(|e| DriverError::Internal(format!("read source dir {:?}: {}", source_dir, e)))?;

        while let Some(entry) = read_dir.next_entry().await
            .map_err(|e| DriverError::Internal(e.to_string()))?
        {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if !name_str.ends_with(".tf") {
                continue;
            }
            let link = workspace.join(&name);
            let target = entry.path();

            // Remove stale link before re-creating.
            if link.exists() || link.symlink_metadata().is_ok() {
                tokio::fs::remove_file(&link)
                    .await
                    .map_err(|e| DriverError::Internal(format!("remove stale symlink: {}", e)))?;
            }

            #[cfg(unix)]
            tokio::fs::symlink(&target, &link)
                .await
                .map_err(|e| DriverError::Internal(format!("symlink {:?} → {:?}: {}", link, target, e)))?;

            #[cfg(not(unix))]
            tokio::fs::copy(&target, &link)
                .await
                .map_err(|e| DriverError::Internal(format!("copy {:?} → {:?}: {}", target, link, e)))?;
        }

        Ok(())
    }

    fn write_backend_tf(
        &self,
        workspace: &Path,
        enclave_id: &str,
        partition_id: &str,
    ) -> Result<(), DriverError> {
        let content = format!(
            "# Generated by nclav — do not edit\n\
             terraform {{\n\
               backend \"http\" {{}}\n\
             }}\n\
             # Variables declared here so nclav_context.auto.tfvars can set them\n\
             variable \"nclav_enclave_id\"   {{ default = \"{}\" }}\n\
             variable \"nclav_partition_id\" {{ default = \"{}\" }}\n\
             variable \"nclav_region\"       {{ default = \"\" }}\n\
             variable \"nclav_project_id\"   {{ default = \"\" }}\n",
            enclave_id, partition_id,
        );
        std::fs::write(workspace.join("nclav_backend.tf"), content)
            .map_err(|e| DriverError::Internal(format!("write nclav_backend.tf: {}", e)))?;
        Ok(())
    }

    fn write_tfvars(
        &self,
        workspace: &Path,
        resolved_inputs: &HashMap<String, String>,
        context_vars: &HashMap<String, String>,
        enclave: &Enclave,
        partition: &Partition,
    ) -> Result<(), DriverError> {
        let mut content = String::from("# Generated by nclav — do not edit\n");

        // Standard nclav context
        content.push_str(&tfvar("nclav_enclave_id", enclave.id.as_str()));
        content.push_str(&tfvar("nclav_partition_id", partition.id.as_str()));
        content.push_str(&tfvar(
            "nclav_region",
            context_vars.get("nclav_region").map(String::as_str).unwrap_or(""),
        ));
        content.push_str(&tfvar(
            "nclav_project_id",
            context_vars.get("nclav_project_id").map(String::as_str).unwrap_or(""),
        ));

        // Any extra cloud-specific context vars not already emitted above
        for (k, v) in context_vars {
            if k != "nclav_region" && k != "nclav_project_id" {
                content.push_str(&tfvar(k, v));
            }
        }

        // Resolved partition inputs (may include values from cross-partition imports)
        if !resolved_inputs.is_empty() {
            content.push_str("\n# resolved partition inputs\n");
            for (k, v) in resolved_inputs {
                content.push_str(&tfvar(k, v));
            }
        }

        std::fs::write(workspace.join("nclav_context.auto.tfvars"), content)
            .map_err(|e| DriverError::Internal(format!("write nclav_context.auto.tfvars: {}", e)))?;
        Ok(())
    }

    // ── Process execution ─────────────────────────────────────────────────────

    /// Run a terraform sub-command, capturing combined stdout+stderr.
    /// Returns (exit_code, combined_log).
    async fn run_tf(
        &self,
        binary: &str,
        workspace: &Path,
        args: &[&str],
        auth_env: &HashMap<String, String>,
    ) -> Result<(i32, String), DriverError> {
        info!(binary, ?args, workspace = %workspace.display(), "running IaC command");

        let mut cmd = Command::new(binary);
        cmd.args(args)
            .current_dir(workspace)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // State backend auth
            .env("TF_HTTP_PASSWORD", self.auth_token.as_str())
            // Disable interactive prompts
            .env("TF_IN_AUTOMATION", "1")
            // Cloud-specific auth
            .envs(auth_env);

        let mut child = cmd.spawn()
            .map_err(|e| DriverError::Internal(format!("spawn {}: {}", binary, e)))?;

        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");

        // Merge stdout and stderr by reading them concurrently into a shared log buffer.
        // Each line is also mirrored to tracing so it appears in nclav's own log output.
        let mut log = String::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();

        let tx1 = tx.clone();
        let stdout_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let _ = tx1.send(line);
            }
        });

        let tx2 = tx.clone();
        let stderr_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let _ = tx2.send(line);
            }
        });

        drop(tx); // close our own sender so rx finishes when both tasks finish

        // Collect lines from both streams as they arrive.
        while let Some(line) = rx.recv().await {
            debug!(target: "nclav::iac", "{}", line);
            log.push_str(&line);
            log.push('\n');
        }

        stdout_task.await.ok();
        stderr_task.await.ok();

        let status = child.wait().await
            .map_err(|e| DriverError::Internal(format!("wait {}: {}", binary, e)))?;

        let code = status.code().unwrap_or(-1);
        if code != 0 {
            warn!(binary, code, "IaC command exited non-zero");
        }
        Ok((code, log))
    }

    /// Run `terraform output -json` and extract `declared_outputs` keys.
    async fn read_outputs(
        &self,
        binary: &str,
        workspace: &Path,
        declared_outputs: &[String],
        auth_env: &HashMap<String, String>,
    ) -> Result<HashMap<String, String>, DriverError> {
        let (exit, out_json) = self
            .run_tf(binary, workspace, &["output", "-json", "-no-color"], auth_env)
            .await?;

        if exit != 0 {
            return Err(DriverError::ProvisionFailed(format!(
                "terraform output exited with code {}", exit
            )));
        }

        let map: serde_json::Value = serde_json::from_str(out_json.trim())
            .map_err(|e| DriverError::ProvisionFailed(format!("parse terraform output: {}", e)))?;

        let mut outputs = HashMap::new();
        for key in declared_outputs {
            match map.get(key).and_then(|v| v.get("value")).and_then(|v| v.as_str()) {
                Some(val) => { outputs.insert(key.clone(), val.to_string()); }
                None => {
                    return Err(DriverError::ProvisionFailed(format!(
                        "declared output '{}' missing from terraform output", key
                    )));
                }
            }
        }
        Ok(outputs)
    }

    // ── IaC run logging ───────────────────────────────────────────────────────

    async fn write_run(
        &self,
        enclave: &Enclave,
        partition: &Partition,
        operation: IacOperation,
        reconcile_run_id: Option<Uuid>,
        log: String,
        exit_code: Option<i32>,
    ) {
        let status = match exit_code {
            Some(0) => IacRunStatus::Succeeded,
            _ => IacRunStatus::Failed,
        };

        let run = IacRun {
            id: Uuid::new_v4(),
            enclave_id: enclave.id.clone(),
            partition_id: partition.id.clone(),
            operation,
            started_at: Utc::now(),
            finished_at: Some(Utc::now()),
            status,
            exit_code,
            log,
            reconcile_run_id,
        };

        if let Err(e) = self.store.upsert_iac_run(&run).await {
            warn!(error = %e, "failed to persist IaC run log");
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Extract the binary name and a clone of `TerraformConfig` from a partition's backend.
/// Returns `DriverError::Internal` if called on a non-IaC partition.
fn extract_tf_config(partition: &Partition) -> Result<(String, nclav_domain::TerraformConfig), DriverError> {
    match &partition.backend {
        PartitionBackend::Terraform(cfg) => {
            let binary = cfg.tool.clone().unwrap_or_else(|| "terraform".into());
            Ok((binary, cfg.clone()))
        }
        PartitionBackend::OpenTofu(cfg) => {
            let binary = cfg.tool.clone().unwrap_or_else(|| "tofu".into());
            Ok((binary, cfg.clone()))
        }
        PartitionBackend::Managed => Err(DriverError::Internal(
            "extract_tf_config called on a Managed partition".into(),
        )),
    }
}

/// Format a single HCL string variable assignment.
fn tfvar(key: &str, value: &str) -> String {
    // Escape backslashes and double-quotes inside the value.
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("{} = \"{}\"\n", key, escaped)
}
