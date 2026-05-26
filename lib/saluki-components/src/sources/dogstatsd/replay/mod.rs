//! DogStatsD capture and replay support.
//!
//! This module contains the capture lifecycle, writer, capture file reader, and file header helpers used by the ADP
//! DogStatsD capture and replay APIs.

mod capture;
mod capture_api;
mod captured_tagger;
mod file_header;
mod reader;
mod replay_api;
mod replay_control;
pub(super) mod writer;

pub use self::capture::DogStatsDCaptureControl;
pub(crate) use self::capture::TrafficCapture;
pub use self::capture_api::DogStatsDCaptureAPIHandler;
pub(crate) use self::captured_tagger::{CapturedTaggerHandle, CapturedTaggerStore};
pub use self::reader::{TimestampResolution, TrafficCaptureReader};
pub use self::replay_api::DogStatsDReplayAPIHandler;
pub use self::replay_control::{DogStatsDReplayControl, ReplaySession, DEFAULT_REPLAY_LOOPS};
pub(crate) use self::writer::CaptureRecord;

/// GID stamped onto replay-injected DogStatsD packets via `SCM_CREDENTIALS`.
///
/// The replay sender stamps this value as the `gid` field of the ancillary credentials block and packs the original
/// captured PID into the `uid` field. The DogStatsD UDS listener uses this GID as the signal to recover the original
/// PID from `uid` and route origin resolution against the captured tagger state instead of the live workload provider.
pub const REPLAY_CREDENTIALS_GID: u32 = 999_888_777;
