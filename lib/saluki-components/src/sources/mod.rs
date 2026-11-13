//! Source implementations.

mod checks_ipc;
pub use self::checks_ipc::ChecksIPCConfiguration;

mod dogstatsd;
pub use self::dogstatsd::{
    CaptureTriggerBody, CaptureTriggerResponseBody, DogStatsDCaptureAPIHandler, DogStatsDCaptureControl,
    DogStatsDConfiguration, DogStatsDReplayAPIHandler, DogStatsDReplayControl, EnablePayloadsConfiguration,
    ListenerRequirement, OriginEnrichmentConfiguration, ReplayControlError, ReplaySession, ReplaySessionResponseBody,
    TimestampResolution, TrafficCaptureReader, CAPTURE_TRIGGER_ROUTE, DEFAULT_REPLAY_LOOPS, REPLAY_CREDENTIALS_GID,
    REPLAY_SESSION_ROUTE,
};

mod heartbeat;
pub use self::heartbeat::HeartbeatConfiguration;

mod otlp;
pub use self::otlp::{
    CumulativeMonotonicMode, HistogramMode, InitialCumulativeMonotonicValue, OtlpConfiguration, OtlpContextSettings,
    OtlpMetricsSettings, SummaryMode,
};
pub use crate::common::otlp::{
    receiver::{GrpcTransport, OtlpGrpcReceiver, OtlpHttpReceiver, OtlpKeepalive, OtlpReceiver, OtlpReceiverTls},
    traces::translator::{ContainerTagPromotion, OtlpTracesSettings},
    CorsConfiguration,
};
