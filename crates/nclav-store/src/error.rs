use thiserror::Error;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("enclave not found: {0}")]
    EnclaveNotFound(String),

    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("internal store error: {0}")]
    Internal(String),

    /// Returned when a TF state lock is already held by another holder.
    /// `holder` is the full lock-info JSON as stored (Terraform displays it on conflict).
    #[error("state lock conflict")]
    LockConflict { holder: serde_json::Value },
}
