pub mod driver;
pub mod error;
pub mod gcp;
pub mod local;

pub use driver::{Driver, ObservedState, ProvisionResult};
pub use error::DriverError;
pub use gcp::{GcpDriver, GcpDriverConfig};
pub use local::LocalDriver;

/// Opaque driver handle — any JSON value.
pub type Handle = serde_json::Value;
