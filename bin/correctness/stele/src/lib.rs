//! A common, simplified representation for various types of telemetry data, and how to compare them.
//!
//! Besides the data types, the crate owns their comparison semantics: approximate equality of metric values, the
//! semantic view of a log, and the field-by-field differences between two logs, with the path patterns that select
//! them.

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
