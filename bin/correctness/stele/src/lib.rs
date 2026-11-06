//! A common, simplified representation for various types of telemetry data.

#![deny(warnings)]
#![deny(missing_docs)]

mod events;
pub use self::events::*;

mod logs;
pub use self::logs::*;

mod logs_diff;
pub use self::logs_diff::*;

mod metrics;
pub use self::metrics::*;

mod service_checks;
pub use self::service_checks::*;

mod traces;
pub use self::traces::*;
