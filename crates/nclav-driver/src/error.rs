use nclav_domain::CloudTarget;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DriverError {
    #[error("provision failed: {0}")]
    ProvisionFailed(String),

    #[error("teardown failed: {0}")]
    TeardownFailed(String),

    #[error("internal driver error: {0}")]
    Internal(String),

    #[error("driver not configured for cloud: {0}")]
    DriverNotConfigured(CloudTarget),

    #[error(".tf file '{file}' found in partition at {path} which uses terraform.source; remove the .tf file or remove terraform.source")]
    TfFilesWithModuleSource { path: String, file: String },
}
