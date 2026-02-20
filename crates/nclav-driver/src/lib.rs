pub mod driver;
pub mod local;
pub mod error;

pub use driver::{Driver, ObservedState, ProvisionResult};
pub use local::LocalDriver;
pub use error::DriverError;

/// Opaque driver handle — any JSON value.
pub type Handle = serde_json::Value;
