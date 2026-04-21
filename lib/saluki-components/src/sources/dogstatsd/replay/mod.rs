//! DogStatsD capture and replay support.
//!
//! Task 1 only establishes the module boundary and core runtime types.
//! Actual file writing and packet tapping are implemented in later tasks.
#![allow(dead_code)]

mod capture;
mod file;
mod writer;

pub(crate) use self::capture::TrafficCapture;
