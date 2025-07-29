//! Destination implementations.

mod blackhole;
pub use self::blackhole::BlackholeConfiguration;

mod prometheus;
pub use self::prometheus::PrometheusConfiguration;
