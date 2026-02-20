use thiserror::Error;

#[derive(Debug, Error)]
pub enum DriverError {
    #[error("provision failed: {0}")]
    ProvisionFailed(String),

    #[error("teardown failed: {0}")]
    TeardownFailed(String),

    #[error("internal driver error: {0}")]
    Internal(String),
}
