use std::sync::Arc;
use nclav_driver::Driver;
use nclav_store::StateStore;

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<dyn StateStore>,
    pub driver: Arc<dyn Driver>,
}
