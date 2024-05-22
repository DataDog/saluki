use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_core::{components::transforms::*, topology::interconnect::EventBuffer};
use saluki_env::{EnvironmentProvider, HostProvider};
use saluki_error::GenericError;
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
    <E::Host as HostProvider>::Error: Into<GenericError>,
{
    async fn build(&self) -> Result<Box<dyn SynchronousTransform + Send>, GenericError> {
        Ok(Box::new(
            HostEnrichment::from_environment_provider(&self.env_provider).await?,
        ))
    }
}

impl<E> MemoryBounds for HostEnrichmentConfiguration<E> {
    fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
}

pub struct HostEnrichment {
    hostname: String,
}

impl HostEnrichment {
    pub async fn from_environment_provider<E>(env_provider: &E) -> Result<Self, GenericError>
    where
        E: EnvironmentProvider + Send + Sync + 'static,
        <E::Host as HostProvider>::Error: Into<GenericError>,
    {
        Ok(Self {
            hostname: env_provider.host().get_hostname().await.map_err(Into::into)?,
        })
    }

    fn enrich_metric(&self, _metric: &mut Metric) {
        // TODO: This code below worked before when we could just mutate our context willy-nilly, but not so much when
        // we're using a resolved handle.
        //
        // As mentioned in some other comments, we likely want to move things like origin PID, hostname, container ID,
        // etc... into something like `MetricMetadata` as they're specific to internal processing, rather than the
        // context itself, which is used in a certain way internally but generally represents the name/tags a user is
        // going to see... so if we're always just passing around this internal stuff using tags, it sort of speaks to
        // situating these bits of information in a more permanent and structured way.
        //
        // if !metric.context.contains_tag(HOST_TAG) {
        //     metric.context.insert_tag((HOST_TAG.to_string(), self.hostname.clone()));
        // }
        todo!()
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
