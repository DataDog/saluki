//! A common, simplified representation for various types of telemetry data.

#![deny(warnings)]
#![deny(missing_docs)]

mod metrics;
pub use self::metrics::*;

mod traces;
pub use self::traces::*;
