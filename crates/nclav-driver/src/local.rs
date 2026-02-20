use std::collections::HashMap;

use async_trait::async_trait;
use nclav_domain::{Enclave, Export, Import, Partition};
use serde_json::json;
use tracing::debug;

use crate::driver::{Driver, ProvisionResult};
use crate::error::DriverError;
use crate::Handle;

/// A stub driver that simulates infrastructure locally.
///
/// - Produces synthetic handles (JSON objects describing what would be created).
/// - Stubs required outputs with `local://<partition>/<key>` values.
/// - Performs no actual I/O.
#[derive(Debug, Default, Clone)]
pub struct LocalDriver;

impl LocalDriver {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Driver for LocalDriver {
    fn name(&self) -> &'static str {
        "local"
    }

    async fn provision_enclave(
        &self,
        enclave: &Enclave,
        _existing: Option<&Handle>,
    ) -> Result<ProvisionResult, DriverError> {
        debug!(enclave_id = %enclave.id, "LocalDriver: provision_enclave");
        let handle = json!({
            "driver": "local",
            "kind": "enclave",
            "id": enclave.id.as_str(),
            "cloud": "local",
        });
        Ok(ProvisionResult {
            handle,
            outputs: HashMap::new(),
        })
    }

    async fn teardown_enclave(
        &self,
        enclave: &Enclave,
        _handle: &Handle,
    ) -> Result<(), DriverError> {
        debug!(enclave_id = %enclave.id, "LocalDriver: teardown_enclave");
        Ok(())
    }

    async fn provision_partition(
        &self,
        enclave: &Enclave,
        partition: &Partition,
        _resolved_inputs: &HashMap<String, String>,
        _existing: Option<&Handle>,
    ) -> Result<ProvisionResult, DriverError> {
        debug!(
            enclave_id = %enclave.id,
            partition_id = %partition.id,
            "LocalDriver: provision_partition"
        );

        let handle = json!({
            "driver": "local",
            "kind": "partition",
            "enclave_id": enclave.id.as_str(),
            "partition_id": partition.id.as_str(),
        });

        // Stub required outputs
        let mut outputs = HashMap::new();
        if let Some(produces) = &partition.produces {
            for key in produces.required_outputs() {
                let val = format!("local://{}/{}", partition.id.as_str(), key);
                outputs.insert(key.to_string(), val);
            }
        }

        Ok(ProvisionResult { handle, outputs })
    }

    async fn teardown_partition(
        &self,
        _enclave: &Enclave,
        partition: &Partition,
        _handle: &Handle,
    ) -> Result<(), DriverError> {
        debug!(partition_id = %partition.id, "LocalDriver: teardown_partition");
        Ok(())
    }

    async fn provision_export(
        &self,
        enclave: &Enclave,
        export: &Export,
        partition_outputs: &HashMap<String, String>,
        _existing: Option<&Handle>,
    ) -> Result<ProvisionResult, DriverError> {
        debug!(
            enclave_id = %enclave.id,
            export = %export.name,
            "LocalDriver: provision_export"
        );

        let handle = json!({
            "driver": "local",
            "kind": "export",
            "enclave_id": enclave.id.as_str(),
            "export_name": export.name,
            "outputs": partition_outputs,
        });

        Ok(ProvisionResult {
            handle,
            outputs: partition_outputs.clone(),
        })
    }

    async fn provision_import(
        &self,
        importer: &Enclave,
        import: &Import,
        export_handle: &Handle,
        _existing: Option<&Handle>,
    ) -> Result<ProvisionResult, DriverError> {
        debug!(
            importer = %importer.id,
            alias = %import.alias,
            "LocalDriver: provision_import"
        );

        let handle = json!({
            "driver": "local",
            "kind": "import",
            "importer_enclave": importer.id.as_str(),
            "alias": import.alias,
            "export_handle": export_handle,
        });

        // Outputs are whatever the export handle carries
        let outputs = if let Some(obj) = export_handle.get("outputs") {
            obj.as_object()
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect()
                })
                .unwrap_or_default()
        } else {
            HashMap::new()
        };

        Ok(ProvisionResult { handle, outputs })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nclav_domain::*;

    fn dummy_enclave() -> Enclave {
        Enclave {
            id: EnclaveId::new("test"),
            name: "test".to_string(),
            cloud: CloudTarget::Local,
            region: "local".to_string(),
            identity: None,
            network: None,
            dns: None,
            imports: vec![],
            exports: vec![],
            partitions: vec![],
        }
    }

    fn dummy_partition(produces: Option<ProducesType>) -> Partition {
        Partition {
            id: PartitionId::new("svc"),
            name: "svc".to_string(),
            produces,
            imports: vec![],
            exports: vec![],
            inputs: HashMap::new(),
            declared_outputs: vec!["hostname".into(), "port".into()],
        }
    }

    #[tokio::test]
    async fn provision_enclave_returns_handle() {
        let driver = LocalDriver::new();
        let enc = dummy_enclave();
        let result = driver.provision_enclave(&enc, None).await.unwrap();
        assert_eq!(result.handle["kind"], "enclave");
    }

    #[tokio::test]
    async fn provision_http_partition_stubs_outputs() {
        let driver = LocalDriver::new();
        let enc = dummy_enclave();
        let part = dummy_partition(Some(ProducesType::Http));
        let result = driver
            .provision_partition(&enc, &part, &HashMap::new(), None)
            .await
            .unwrap();
        assert!(result.outputs.contains_key("hostname"));
        assert!(result.outputs.contains_key("port"));
    }

    #[tokio::test]
    async fn provision_queue_partition_stubs_outputs() {
        let driver = LocalDriver::new();
        let enc = dummy_enclave();
        let part = dummy_partition(Some(ProducesType::Queue));
        let result = driver
            .provision_partition(&enc, &part, &HashMap::new(), None)
            .await
            .unwrap();
        assert!(result.outputs.contains_key("queue_url"));
    }
}
