pub mod driver;
pub mod error;
pub mod gcp;
pub mod local;
pub mod registry;
pub mod terraform;

pub use driver::{Driver, ObservedState, OrphanedResource, ProvisionResult};
pub use error::DriverError;
pub use gcp::{GcpDriver, GcpDriverConfig};
pub use local::LocalDriver;
pub use registry::DriverRegistry;
pub use terraform::TerraformBackend;

/// Opaque driver handle — any JSON value.
pub type Handle = serde_json::Value;
