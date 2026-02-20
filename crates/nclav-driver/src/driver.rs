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

#[async_trait]
pub trait Driver: Send + Sync + 'static {
    fn name(&self) -> &'static str;

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
}
