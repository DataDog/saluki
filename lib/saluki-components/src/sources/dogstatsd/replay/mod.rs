//! DogStatsD capture and replay support.
//!
//! Capture support lives here now.
//! Replay readback and injection will be added in later tasks.
#![allow(dead_code)]

mod capture;
mod file;
mod reader;
mod state;
mod writer;

pub use self::capture::DogStatsDCaptureControl;
pub(crate) use self::capture::TrafficCapture;
pub use self::state::DogStatsDReplayState;
pub(crate) use self::writer::CaptureRecord;
