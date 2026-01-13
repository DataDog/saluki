//! A DDSketch implementation based on the Datadog Agent's DDSketch implementation.
#![deny(warnings)]
#![deny(missing_docs)]

mod ddsketch;
pub use self::ddsketch::DDSketch;

mod params;
pub use self::params::*;
