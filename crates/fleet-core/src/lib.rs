//! iDevice Fleet core: device tracking, supervised jobs and persistence.
//! Has no UI code so all of it can be tested headless.

pub mod backup;
pub mod demo;
pub mod devices;
pub mod error;
pub mod firmware;
pub mod fleet;
pub mod jobs;
pub mod model;
pub mod native;
pub mod store;
pub mod usb;

pub use devices::Health;
pub use error::{ErrorClass, FleetError, Result};
pub use fleet::{Config, Fleet, Lookup};
pub use jobs::{JobContext, JobEngine, JobId, JobKind, JobSpec, JobState, JobView, Limits, RetryPolicy};
pub use model::{Device, DeviceKey, DeviceMode, PairState};
