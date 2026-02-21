use std::sync::Arc;
use nclav_driver::DriverRegistry;
use nclav_store::StateStore;

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<dyn StateStore>,
    pub registry: Arc<DriverRegistry>,
    /// Bearer token required on every request.
    pub auth_token: Arc<String>,
    /// Base URL of this API server (e.g. "http://127.0.0.1:8080").
    /// Passed to the reconciler so IaC partitions can configure their TF HTTP backend.
    pub api_base: Arc<String>,
}
