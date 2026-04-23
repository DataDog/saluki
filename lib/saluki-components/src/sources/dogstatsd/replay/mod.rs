//! DogStatsD capture and replay support.
//!
//! This module contains the capture writer, capture reader, replay-state store, and replay injector used by the ADP
//! operator commands.
#![allow(dead_code)]

mod capture;
mod file;
mod injector;
mod reader;
mod state;
mod writer;

/// Magic Unix group ID used to identify replayed DogStatsD traffic.
///
/// This mirrors the Go Agent replay constant so replayed UDS credentials can be distinguished from live traffic.
pub const REPLAY_CREDENTIALS_GID: u32 = 999_888_777;

pub use self::capture::DogStatsDCaptureControl;
pub(crate) use self::capture::TrafficCapture;
pub use self::injector::DogStatsDReplayInjector;
pub use self::reader::TrafficCaptureReader;
pub use self::state::DogStatsDReplayState;
pub(crate) use self::writer::CaptureRecord;
