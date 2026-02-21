use std::sync::Arc;
use nclav_driver::DriverRegistry;
use nclav_store::StateStore;

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<dyn StateStore>,
    pub registry: Arc<DriverRegistry>,
    /// Bearer token required on every request.
    pub auth_token: Arc<String>,
}
