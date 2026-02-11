use async_trait::async_trait;

pub use super::{event, event_platform, histogram, log, metric, service_check};
pub mod console;
pub mod cshlib;

// TODO Can we take Metric/ServiceCheck/... as ref to avoid alloc?
// TODO Should Sink provide the configuration, to avoid copy?
#[async_trait]
pub trait Sink: Send + Sync {
    async fn submit_metric(&self, metric: metric::Metric, flush_first: bool); // flush arg really usefull?
    async fn submit_service_check(&self, service_check: service_check::ServiceCheck);
    async fn submit_event(&self, event: event::Event);
    async fn submit_histogram(&self, histogram: histogram::Histogram, flush_first: bool);
    async fn submit_event_platform_event(&self, event: event_platform::Event);

    // FIXME Accept any kind of string
    async fn log(&self, level: log::Level, message: String);
}
