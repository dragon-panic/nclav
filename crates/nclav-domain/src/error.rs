use thiserror::Error;

#[derive(Debug, Error)]
pub enum DomainError {
    #[error("invalid enclave id: {0}")]
    InvalidEnclaveId(String),

    #[error("invalid partition id: {0}")]
    InvalidPartitionId(String),

    #[error("invalid export name: {0}")]
    InvalidExportName(String),

    #[error("incompatible auth type {auth:?} for export type {export_type:?}")]
    IncompatibleAuthType {
        auth: String,
        export_type: String,
    },

    #[error("missing required output '{key}' for produces type {produces:?}")]
    MissingRequiredOutput { key: String, produces: String },

    #[error("invalid configuration: {0}")]
    InvalidConfig(String),
}
