//! A common, simplified representation for various types of telemetry data.

#![deny(warnings)]
#![deny(missing_docs)]

mod events;
pub use self::events::*;

mod metrics;
pub use self::metrics::*;

mod service_checks;
pub use self::service_checks::*;

mod logs;
pub use self::logs::*;

mod traces;
pub use self::traces::*;
