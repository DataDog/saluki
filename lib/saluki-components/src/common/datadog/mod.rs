pub mod config;
pub mod endpoints;
pub mod io;
pub mod middleware;
mod proxy;
pub mod request_builder;
mod retry;
pub mod telemetry;
pub mod transaction;

/// Default compressed size limit for intake requests.
pub const DEFAULT_INTAKE_COMPRESSED_SIZE_LIMIT: usize = 3_200_000; // 3 MiB

/// Default uncompressed size limit for intake requests.
pub const DEFAULT_INTAKE_UNCOMPRESSED_SIZE_LIMIT: usize = 62_914_560; // 60 MiB
