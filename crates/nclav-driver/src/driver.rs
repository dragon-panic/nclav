use std::collections::HashMap;

use async_trait::async_trait;
use nclav_domain::{Enclave, Export, Import, Partition};

use crate::error::DriverError;
use crate::Handle;

/// Result of a successful provision call.
#[derive(Debug, Clone)]
pub struct ProvisionResult {
    /// Opaque handle that the driver uses to reference this resource.
    pub handle: Handle,
    /// Key/value outputs produced by the provisioning (e.g. hostname, port).
    pub outputs: HashMap<String, String>,
}

/// Read-only snapshot of a resource as it exists in the cloud right now.
/// Returned by observe_* methods; never modifies cloud state.
#[derive(Debug, Clone)]
pub struct ObservedState {
    /// Whether the resource exists at all in the cloud.
    pub exists: bool,
    /// Whether the resource is healthy (exists and passing health checks).
    pub healthy: bool,
    /// Current output values read from the cloud (may differ from stored outputs
    /// if cloud drift has occurred).
    pub outputs: HashMap<String, String>,
    /// Full cloud API response, stored opaquely for debugging.
    pub raw: Handle,
}

#[async_trait]
pub trait Driver: Send + Sync + 'static {
    fn name(&self) -> &'static str;

    // ── Mutating ──────────────────────────────────────────────────────────────

    async fn provision_enclave(
        &self,
        enclave: &Enclave,
        existing: Option<&Handle>,
    ) -> Result<ProvisionResult, DriverError>;

    async fn teardown_enclave(
        &self,
        enclave: &Enclave,
        handle: &Handle,
    ) -> Result<(), DriverError>;

    async fn provision_partition(
        &self,
        enclave: &Enclave,
        partition: &Partition,
        resolved_inputs: &HashMap<String, String>,
        existing: Option<&Handle>,
    ) -> Result<ProvisionResult, DriverError>;

    async fn teardown_partition(
        &self,
        enclave: &Enclave,
        partition: &Partition,
        handle: &Handle,
    ) -> Result<(), DriverError>;

    async fn provision_export(
        &self,
        enclave: &Enclave,
        export: &Export,
        partition_outputs: &HashMap<String, String>,
        existing: Option<&Handle>,
    ) -> Result<ProvisionResult, DriverError>;

    async fn provision_import(
        &self,
        importer: &Enclave,
        import: &Import,
        export_handle: &Handle,
        existing: Option<&Handle>,
    ) -> Result<ProvisionResult, DriverError>;

    // ── Read-only (drift detection) ───────────────────────────────────────────

    /// Read the current state of an enclave from the cloud without modifying
    /// anything. Called by the drift detection path.
    async fn observe_enclave(
        &self,
        enclave: &Enclave,
        handle: &Handle,
    ) -> Result<ObservedState, DriverError>;

    /// Read the current state of a partition from the cloud without modifying
    /// anything.
    async fn observe_partition(
        &self,
        enclave: &Enclave,
        partition: &Partition,
        handle: &Handle,
    ) -> Result<ObservedState, DriverError>;

    // ── IaC support ───────────────────────────────────────────────────────────

    /// Cloud-specific Terraform variable values (written to `nclav_context.auto.tfvars`).
    /// Implementations should extract values like `project_id` and `region` from
    /// the enclave handle produced by `provision_enclave`.
    fn context_vars(&self, enclave: &Enclave, handle: &Handle) -> HashMap<String, String>;

    /// Environment variables to set on the Terraform subprocess for cloud
    /// authentication. These are read by the provider SDK automatically and
    /// are never written to disk or tfvars files.
    ///
    /// Example (GCP): `GOOGLE_IMPERSONATE_SERVICE_ACCOUNT`, `GOOGLE_PROJECT`.
    fn auth_env(&self, enclave: &Enclave, handle: &Handle) -> HashMap<String, String>;
}
