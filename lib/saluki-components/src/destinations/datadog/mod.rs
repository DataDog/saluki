//! Datadog-specific destinations and helper functions.

mod common;
pub use self::common::{io::RB_BUFFER_CHUNK_SIZE, request_builder::RequestBuilder, telemetry::ComponentTelemetry};

mod events;
pub use self::events::{DatadogEventsConfiguration, EventsEndpointEncoder};

mod metrics;
pub use self::metrics::DatadogMetricsConfiguration;

mod service_checks;
pub use self::service_checks::DatadogServiceChecksConfiguration;
