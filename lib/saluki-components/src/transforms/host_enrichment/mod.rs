use async_trait::async_trait;
use saluki_env::{EnvironmentProvider, HostProvider};

use saluki_core::{components::transforms::*, topology::interconnect::EventBuffer};
use saluki_event::{metric::Metric, Event};

const HOST_TAG: &str = "host";

/// Host Enrichment synchronous transform.
///
/// Enriches metrics with a `host` tag if one is not already present. Calculates the hostname to use based on the
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
    <E::Host as HostProvider>::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    async fn build(&self) -> Result<Box<dyn SynchronousTransform + Send>, Box<dyn std::error::Error + Send + Sync>> {
        Ok(Box::new(
            HostEnrichment::from_environment_provider(&self.env_provider).await?,
        ))
    }
}

pub struct HostEnrichment {
    hostname: String,
}

impl HostEnrichment {
    pub async fn from_environment_provider<E>(
        env_provider: &E,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>>
    where
        E: EnvironmentProvider + Send + Sync + 'static,
        <E::Host as HostProvider>::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        Ok(Self {
            hostname: env_provider.host().get_hostname().await.map_err(Into::into)?,
        })
    }

    fn enrich_metric(&self, metric: &mut Metric) {
        if !metric.context.contains_tag(HOST_TAG) {
            metric.context.insert_tag((HOST_TAG.to_string(), self.hostname.clone()));
        }
    }
}

impl SynchronousTransform for HostEnrichment {
    fn transform_buffer(&self, event_buffer: &mut EventBuffer) {
        for event in event_buffer {
            match event {
                Event::Metric(metric) => self.enrich_metric(metric),
            }
        }
    }
}
