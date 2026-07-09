use async_trait::async_trait;
use saluki_core::accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_core::{components::transforms::*, topology::EventsBuffer};
use saluki_core::{
    components::ComponentContext,
    data_model::event::{eventd::EventD, service_check::ServiceCheck},
};
use saluki_env::{EnvironmentProvider, HostProvider};
use saluki_error::GenericError;
use stringtheory::MetaString;

/// Host enrichment synchronous transform.
///
/// Enriches events and service checks with a hostname if one isn't already present. Metrics must carry their hostname
/// in their context before this transform so metric identity is stable before fanout/encoding.
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
    async fn build(&self, _context: ComponentContext) -> Result<Box<dyn SynchronousTransform + Send>, GenericError> {
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
        builder
            .minimum()
            .with_single_value::<HostEnrichment>("component struct");
    }
}

pub struct HostEnrichment {
    hostname: MetaString,
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
                .map(MetaString::from)
                .map_err(Into::into)?,
        })
    }

    fn enrich_eventd(&self, eventd: &mut EventD) {
        // Only add the hostname if it's not already present.
        if eventd.hostname().is_none() {
            eventd.set_hostname(Some(self.hostname.clone()));
        }
    }

    fn enrich_service_check(&self, service_check: &mut ServiceCheck) {
        // Only add the hostname if it's not already present.
        if service_check.hostname().is_none() {
            service_check.set_hostname(Some(self.hostname.clone()));
        }
    }
}

impl SynchronousTransform for HostEnrichment {
    fn transform_buffer(&mut self, event_buffer: &mut EventsBuffer) {
        for event in event_buffer {
            if let Some(eventd) = event.try_as_eventd_mut() {
                self.enrich_eventd(eventd);
            } else if let Some(service_check) = event.try_as_service_check_mut() {
                self.enrich_service_check(service_check);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use saluki_context::Context;
    use saluki_core::components::transforms::SynchronousTransform;
    use saluki_core::data_model::event::{
        eventd::EventD,
        metric::Metric,
        service_check::{CheckStatus, ServiceCheck},
        Event,
    };
    use saluki_core::topology::EventsBuffer;
    use stringtheory::MetaString;

    use super::HostEnrichment;

    fn host_enrichment() -> HostEnrichment {
        HostEnrichment {
            hostname: MetaString::from_static("default-host"),
        }
    }

    fn eventd_hostnames(events: EventsBuffer) -> Vec<Option<String>> {
        events
            .into_iter()
            .map(|event| match event {
                Event::EventD(eventd) => eventd.hostname().map(str::to_string),
                other => panic!("expected eventd event, got {other:?}"),
            })
            .collect()
    }

    fn service_check_hostnames(events: EventsBuffer) -> Vec<Option<String>> {
        events
            .into_iter()
            .map(|event| match event {
                Event::ServiceCheck(service_check) => service_check.hostname().map(str::to_string),
                other => panic!("expected service check event, got {other:?}"),
            })
            .collect()
    }

    #[test]
    fn transform_leaves_metric_context_host_unchanged() {
        let cases = [
            None,
            Some(MetaString::empty()),
            Some(MetaString::from_static("custom-host")),
        ];

        for host in cases {
            let context = Context::from_static_name("metric").with_host(host.clone());
            let metric = Metric::gauge(context, 1.0);
            let mut events = EventsBuffer::default();
            assert!(events.try_push(Event::Metric(metric)).is_none());

            host_enrichment().transform_buffer(&mut events);

            let Event::Metric(metric) = events.into_iter().next().expect("metric event") else {
                panic!("expected metric event");
            };
            assert_eq!(metric.context().host(), host.as_deref());
        }
    }

    #[test]
    fn transform_enriches_eventd_hostname_only_when_absent() {
        let mut events = EventsBuffer::default();
        assert!(events.try_push(Event::EventD(EventD::new("title", "text"))).is_none());
        assert!(events
            .try_push(Event::EventD(
                EventD::new("title", "text").with_hostname(MetaString::from_static("existing-host")),
            ))
            .is_none());

        host_enrichment().transform_buffer(&mut events);

        // The hostname-less event is filled with the transform's default hostname; the event that already carries a
        // hostname is left untouched.
        assert_eq!(
            eventd_hostnames(events),
            vec![Some("default-host".to_string()), Some("existing-host".to_string())],
        );
    }

    #[test]
    fn transform_enriches_service_check_hostname_only_when_absent() {
        let mut events = EventsBuffer::default();
        assert!(events
            .try_push(Event::ServiceCheck(ServiceCheck::new("svc", CheckStatus::Ok)))
            .is_none());
        assert!(events
            .try_push(Event::ServiceCheck(
                ServiceCheck::new("svc", CheckStatus::Ok).with_hostname(MetaString::from_static("existing-host")),
            ))
            .is_none());

        host_enrichment().transform_buffer(&mut events);

        // The hostname-less service check is filled with the transform's default hostname; the one that already carries
        // a hostname is left untouched.
        assert_eq!(
            service_check_hostnames(events),
            vec![Some("default-host".to_string()), Some("existing-host".to_string())],
        );
    }
}
