use thiserror::Error;

#[derive(Debug, Error)]
pub enum ReconcileError {
    #[error("config error: {0}")]
    Config(#[from] nclav_config::ConfigError),

    #[error("graph validation error: {0}")]
    Graph(#[from] nclav_graph::GraphError),

    #[error("store error: {0}")]
    Store(#[from] nclav_store::StoreError),

    #[error("driver error: {0}")]
    Driver(#[from] nclav_driver::DriverError),

    #[error("internal error: {0}")]
    Internal(String),
}
