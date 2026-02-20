use thiserror::Error;
use nclav_domain::{EnclaveId, PartitionId};

#[derive(Debug, Error)]
pub enum GraphError {
    #[error("dangling import: enclave '{importer}' imports from unknown enclave '{from}'")]
    DanglingImportEnclave {
        importer: EnclaveId,
        from: EnclaveId,
    },

    #[error("dangling import: enclave '{importer}' imports export '{export_name}' which does not exist on '{from}'")]
    DanglingImportExport {
        importer: EnclaveId,
        from: EnclaveId,
        export_name: String,
    },

    #[error("access denied: enclave '{importer}' is not permitted to import '{export_name}' from '{from}'")]
    AccessDenied {
        importer: EnclaveId,
        from: EnclaveId,
        export_name: String,
    },

    #[error("type mismatch: enclave '{importer}' imports '{export_name}' as {import_type} but it is {export_type}")]
    TypeMismatch {
        importer: EnclaveId,
        export_name: String,
        import_type: String,
        export_type: String,
    },

    #[error("produces/export mismatch: partition '{partition}' produces {produces_type} but is targeted by export '{export_name}' of type {export_type}")]
    ProducesExportMismatch {
        partition: PartitionId,
        produces_type: String,
        export_name: String,
        export_type: String,
    },

    #[error("missing required output: partition '{partition}' produces {produces_type} but does not declare output '{key}'")]
    MissingRequiredOutput {
        partition: PartitionId,
        produces_type: String,
        key: String,
    },

    #[error("cycle detected in enclave dependency graph")]
    CycleDetected,

    #[error("multiple errors")]
    Multiple(Vec<GraphError>),
}
