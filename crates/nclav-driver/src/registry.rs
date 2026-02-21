use std::collections::HashMap;
use std::sync::Arc;

use nclav_domain::{CloudTarget, Enclave};

use crate::driver::Driver;
use crate::error::DriverError;

/// Dispatches driver calls to the correct cloud-specific [`Driver`] implementation.
///
/// Each enclave's `cloud:` field selects its driver. When `cloud:` is absent the
/// enclave inherits `default_cloud`. The [`LocalDriver`](crate::local::LocalDriver)
/// should always be registered.
pub struct DriverRegistry {
    /// Default cloud used when an enclave's `cloud:` field is absent.
    pub default_cloud: CloudTarget,
    drivers: HashMap<CloudTarget, Arc<dyn Driver>>,
}

impl DriverRegistry {
    pub fn new(default_cloud: CloudTarget) -> Self {
        Self { default_cloud, drivers: HashMap::new() }
    }

    /// Register a driver for a cloud target. Returns `&mut self` for chaining.
    pub fn register(&mut self, cloud: CloudTarget, driver: Arc<dyn Driver>) -> &mut Self {
        self.drivers.insert(cloud, driver);
        self
    }

    /// Resolve the driver for the given enclave.
    ///
    /// Uses `enc.cloud` if set, otherwise falls back to `default_cloud`.
    /// Returns `DriverNotConfigured` if no driver is registered for the resolved cloud.
    pub fn for_enclave(&self, enc: &Enclave) -> Result<Arc<dyn Driver>, DriverError> {
        let cloud = self.resolved_cloud(enc);
        self.for_cloud(cloud)
    }

    /// Resolve the driver for the given cloud target directly.
    pub fn for_cloud(&self, cloud: CloudTarget) -> Result<Arc<dyn Driver>, DriverError> {
        self.drivers
            .get(&cloud)
            .cloned()
            .ok_or(DriverError::DriverNotConfigured(cloud))
    }

    /// Return the cloud that will be used for this enclave (enc.cloud or default).
    pub fn resolved_cloud(&self, enc: &Enclave) -> CloudTarget {
        enc.cloud.clone().unwrap_or_else(|| self.default_cloud.clone())
    }

    /// Return all cloud targets that have a registered driver.
    pub fn active_clouds(&self) -> Vec<CloudTarget> {
        self.drivers.keys().cloned().collect()
    }
}
