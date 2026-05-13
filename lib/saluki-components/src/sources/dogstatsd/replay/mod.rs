//! DogStatsD capture support.
//!
//! This module contains the capture lifecycle, writer, and file header helpers used by the ADP DogStatsD capture API.

mod capture;
mod capture_api;
mod file;
mod writer;

pub use self::capture::DogStatsDCaptureControl;
pub(crate) use self::capture::TrafficCapture;
pub use self::capture_api::DogStatsDCaptureAPIHandler;
pub(crate) use self::writer::CaptureRecord;
