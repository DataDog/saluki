//! Source implementations.

mod checks_ipc;
pub use self::checks_ipc::ChecksIPCConfiguration;

mod dogstatsd;
/// Entry points into the DogStatsD source for measurements, with the `measurement` feature.
#[cfg(feature = "measurement")]
pub use self::dogstatsd::measurement as dogstatsd_measurement;
pub use self::dogstatsd::{
    decoder_workers as dogstatsd_decoder_workers, metric_lane_output as dogstatsd_metric_lane_output,
    DogStatsDCaptureAPIHandler, DogStatsDCaptureControl, DogStatsDConfiguration, DogStatsDReplayAPIHandler,
    DogStatsDReplayControl, EnablePayloadsConfiguration, OriginEnrichmentConfiguration, ReplaySession,
    TimestampResolution, TrafficCaptureReader, DEFAULT_REPLAY_LOOPS, METRIC_LANES_MAX as DOGSTATSD_METRIC_LANES_MAX,
    REPLAY_CREDENTIALS_GID,
};

mod heartbeat;
pub use self::heartbeat::HeartbeatConfiguration;

mod otlp;
pub use self::otlp::OtlpConfiguration;
