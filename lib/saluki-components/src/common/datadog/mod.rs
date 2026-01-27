pub mod apm;
pub mod config;
pub mod endpoints;
pub mod io;
pub mod middleware;
pub mod obfuscation;
mod proxy;
pub mod request_builder;
mod retry;
pub mod telemetry;
pub mod transaction;

/// Metric key used to store Datadog sampling priority (`_sampling_priority_v1`).
pub const SAMPLING_PRIORITY_METRIC_KEY: &str = "_sampling_priority_v1";

/// Default compressed size limit for intake requests.
pub const DEFAULT_INTAKE_COMPRESSED_SIZE_LIMIT: usize = 3_200_000; // 3 MiB

/// Default uncompressed size limit for intake requests.
pub const DEFAULT_INTAKE_UNCOMPRESSED_SIZE_LIMIT: usize = 62_914_560; // 60 MiB
