use async_trait::async_trait;
use saluki_event::metric::MetricTags;

#[async_trait]
pub trait WorkloadProvider {
    type Error;

    async fn get_workload_tags(&self) -> Result<MetricTags, Self::Error>;
}

pub struct NoopWorkloadProvider;

#[async_trait]
impl WorkloadProvider for NoopWorkloadProvider {
    type Error = std::convert::Infallible;

    async fn get_workload_tags(&self) -> Result<MetricTags, Self::Error> {
        Ok(MetricTags::default())
    }
}
