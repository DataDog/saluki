use std::sync::Arc;

use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_core::{components::transforms::*, topology::interconnect::EventBuffer};
use saluki_env::{EnvironmentProvider, HostProvider};
use saluki_error::GenericError;
use saluki_event::metric::Metric;

/// Host Enrichment synchronous transform.
///
/// Enriches metrics with a hostname if one is not already present. Calculates the hostname to use based on the
/// configured environment provider, allowing for a high degree of accuracy around what qualifies as a hostname, and how
/// to query it.
pub struct HostEnrichmentConfiguration<E> {
    env_provider: E,
}

impl<E> HostEnrichmentConfiguration<E> {
    /// Creates a new `HostEnrichmentConfiguration` with the given environment provider.
    pub fn from_environment_provider(env_provider: E) -> Self {
        Self { env_provider }
    }
}

#[async_trait]
impl<E> SynchronousTransformBuilder for HostEnrichmentConfiguration<E>
where
    E: EnvironmentProvider + Send + Sync + 'static,
    <E::Host as HostProvider>::Error: Into<GenericError>,
{
    async fn build(&self) -> Result<Box<dyn SynchronousTransform + Send>, GenericError> {
        Ok(Box::new(
            HostEnrichment::from_environment_provider(&self.env_provider).await?,
        ))
    }
}

impl<E> MemoryBounds for HostEnrichmentConfiguration<E> {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        // TODO: We don't account for the size of the hostname since we only query it when we go to actually build the
        // transform. We could move the querying to the point where we create `HostEnrichmentConfiguration` itself but
        // that would mean it couldn't be updated dynamically.
        //
        // Not a relevant problem _right now_, but a _potential_ problem in the future. :shrug:

        // Capture the size of the heap allocation when the component is built.
        builder.minimum().with_single_value::<HostEnrichment>();
    }
}

pub struct HostEnrichment {
    hostname: Arc<str>,
}

impl HostEnrichment {
    pub async fn from_environment_provider<E>(env_provider: &E) -> Result<Self, GenericError>
    where
        E: EnvironmentProvider + Send + Sync + 'static,
        <E::Host as HostProvider>::Error: Into<GenericError>,
    {
        Ok(Self {
            hostname: env_provider
                .host()
                .get_hostname()
                .await
                .map(Arc::from)
                .map_err(Into::into)?,
        })
    }

    fn enrich_metric(&self, metric: &mut Metric) {
        // Only add the hostname if it's not already present.
        if metric.metadata().hostname().is_none() {
            metric.metadata_mut().set_hostname(self.hostname.clone());
        }
    }
}

impl SynchronousTransform for HostEnrichment {
    fn transform_buffer(&self, event_buffer: &mut EventBuffer) {
        for event in event_buffer {
            if let Some(metric) = event.try_as_metric_mut() {
                self.enrich_metric(metric)
            }
        }
    }
}
