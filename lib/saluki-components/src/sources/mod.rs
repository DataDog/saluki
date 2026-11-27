//! Source implementations.

mod checks_ipc;
pub use self::checks_ipc::ChecksIPCConfiguration;

mod dogstatsd;
pub use self::dogstatsd::{
    DogStatsDConfiguration, EnablePayloadsConfiguration, ListenerRequirement, OriginEnrichmentConfiguration,
};
pub use crate::common::dogstatsd_capture::{
    CaptureTriggerBody, CaptureTriggerResponseBody, DogStatsDCaptureAPIHandler, DogStatsDCaptureControl,
    DogStatsDReplayAPIHandler, DogStatsDReplayControl, ReplayControlError, ReplaySession, ReplaySessionResponseBody,
    TimestampResolution, TrafficCaptureReader, CAPTURE_TRIGGER_ROUTE, DEFAULT_REPLAY_LOOPS, REPLAY_CREDENTIALS_GID,
    REPLAY_SESSION_ROUTE,
};

mod heartbeat;
pub use self::heartbeat::HeartbeatConfiguration;

mod otlp;
pub use self::otlp::{
    CumulativeMonotonicMode, HistogramMode, InitialCumulativeMonotonicValue, OtlpConfiguration, OtlpContextSettings,
    OtlpDebugVerbosity, OtlpMetricsSettings, SummaryMode,
};
pub use crate::common::otlp::{
    proxy::OtlpProxyDestinations,
    receiver::{GrpcTransport, OtlpGrpcReceiver, OtlpHttpReceiver, OtlpKeepalive, OtlpReceiver, OtlpReceiverTls},
    traces::translator::{ContainerTagPromotion, OtlpTracesSettings},
    CorsConfiguration, OtlpRoute, OtlpRoutes,
};
pub use crate::common::proxy::{listener_reached_by, Destination as ProxyDestination};
