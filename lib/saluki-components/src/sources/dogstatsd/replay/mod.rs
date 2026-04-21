//! DogStatsD capture and replay support.
//!
//! Task 1 only establishes the module boundary and core runtime types.
//! Actual file writing and packet tapping are implemented in later tasks.

mod capture;
mod writer;

pub(crate) use self::capture::TrafficCapture;
