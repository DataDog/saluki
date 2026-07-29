use std::{
    sync::{Arc, LazyLock},
    time::Duration,
};

use async_trait::async_trait;
use saluki_common::time::get_unix_timestamp;
use saluki_context::{
    tags::{SharedTagSet, Tag, TagSet},
    Context,
};
use saluki_core::{
    accounting::{MemoryBounds, MemoryBoundsBuilder},
    components::{sources::*, ComponentContext},
    data_model::event::{
        metric::Metric,
        service_check::{CheckStatus, ServiceCheck},
        Event, EventType,
    },
    topology::OutputDefinition,
};
use saluki_env::{EnvironmentProvider as _, HostProvider as _, WorkloadProvider};
use saluki_error::{ErrorContext as _, GenericError};
use stringtheory::MetaString;
use tokio::{
    pin, select,
    time::{interval, MissedTickBehavior},
};
use tracing::{debug, warn};

use crate::internal::env::ADPEnvironmentProvider;

const LIVENESS_INTERVAL: Duration = Duration::from_secs(15);
const RUNNING_METRIC_NAME: &str = "datadog.agent_data_plane.running";
const UP_SERVICE_CHECK_NAME: &str = "datadog.agent_data_plane.up";

/// Periodically emits Agent Data Plane liveness signals.
pub struct LivenessConfiguration {
    hostname: MetaString,
    version: MetaString,
    add_container_tags: bool,
    workload_provider: Option<Arc<dyn WorkloadProvider + Send + Sync>>,
}

impl LivenessConfiguration {
    /// Creates a liveness source configuration using the configured environment.
    pub async fn from_environment_provider(
        env_provider: &ADPEnvironmentProvider, add_container_tags: bool,
    ) -> Result<Self, GenericError> {
        let hostname = env_provider
            .host()
            .get_hostname()
            .await
            .error_context("Failed to get hostname for liveness source.")?;
        Ok(Self::from_hostname(hostname).with_container_tags(add_container_tags, env_provider.workload().clone()))
    }

    fn from_hostname(hostname: String) -> Self {
        Self {
            hostname: hostname.into(),
            version: saluki_metadata::get_app_details().version().raw().into(),
            add_container_tags: false,
            workload_provider: None,
        }
    }

    fn with_container_tags<W>(mut self, add_container_tags: bool, workload_provider: W) -> Self
    where
        W: WorkloadProvider + Send + Sync + 'static,
    {
        self.add_container_tags = add_container_tags;
        self.workload_provider = Some(Arc::new(workload_provider));
        self
    }
}

#[async_trait]
impl SourceBuilder for LivenessConfiguration {
    async fn build(&self, _context: ComponentContext) -> Result<Box<dyn Source + Send>, GenericError> {
        Ok(Box::new(Liveness::new(
            self.hostname.clone(),
            self.version.clone(),
            self.add_container_tags,
            self.workload_provider.clone(),
        )))
    }

    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        static OUTPUTS: LazyLock<Vec<OutputDefinition<EventType>>> = LazyLock::new(|| {
            vec![
                OutputDefinition::named_output("metrics", EventType::Metric),
                OutputDefinition::named_output("service_checks", EventType::ServiceCheck),
            ]
        });
        &OUTPUTS
    }
}

impl MemoryBounds for LivenessConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder.minimum().with_single_value::<Liveness>("liveness source");
    }
}

struct Liveness {
    metric_context: Context,
    service_check: ServiceCheck,
    add_container_tags: bool,
    workload_provider: Option<Arc<dyn WorkloadProvider + Send + Sync>>,
}

impl Liveness {
    fn new(
        hostname: MetaString, version: MetaString, add_container_tags: bool,
        workload_provider: Option<Arc<dyn WorkloadProvider + Send + Sync>>,
    ) -> Self {
        let (metric_context, service_check) = create_liveness_payloads(hostname, version);
        Self {
            metric_context,
            service_check,
            add_container_tags,
            workload_provider,
        }
    }

    fn signals_at(&self, timestamp: u64) -> (Event, Event) {
        let container_tags = self.container_tags();
        (
            self.metric_at_with_container_tags(timestamp, container_tags.as_ref()),
            self.service_check_with_container_tags(container_tags.as_ref()),
        )
    }

    fn metric_at_with_container_tags(&self, timestamp: u64, container_tags: Option<&SharedTagSet>) -> Event {
        let context = match container_tags {
            Some(container_tags) => {
                let mut tags = self.metric_context.tags().clone();
                tags.merge_shared(container_tags);
                self.metric_context.with_tags(tags)
            }
            None => self.metric_context.clone(),
        };
        Event::Metric(Metric::gauge(context, (timestamp, 1.0)))
    }

    fn service_check_with_container_tags(&self, container_tags: Option<&SharedTagSet>) -> Event {
        let service_check = match container_tags {
            Some(container_tags) => self.service_check.clone().with_tags(container_tags.clone()),
            None => self.service_check.clone(),
        };
        Event::ServiceCheck(service_check)
    }

    fn container_tags(&self) -> Option<SharedTagSet> {
        self.add_container_tags.then(|| {
            self.workload_provider
                .as_ref()
                .and_then(|workload_provider| workload_provider.get_self_container_tags())
        })?
    }
}

#[async_trait]
impl Source for Liveness {
    async fn run(self: Box<Self>, mut context: SourceContext) -> Result<(), GenericError> {
        let global_shutdown = context.take_shutdown_handle();
        pin!(global_shutdown);

        let mut health = context.take_health_handle();
        let mut tick_interval = interval(LIVENESS_INTERVAL);
        tick_interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

        health.mark_ready();
        debug!("Liveness source started.");

        loop {
            select! {
                _ = &mut global_shutdown => {
                    debug!("Received shutdown signal.");
                    break;
                },
                _ = health.live() => continue,
                _ = tick_interval.tick() => {
                    let (metric, service_check) = self.signals_at(get_unix_timestamp());

                    if let Err(error) = context.dispatcher().dispatch_one_named("metrics", metric).await {
                        warn!(error = %error, "Failed to dispatch liveness metric.");
                    }

                    if let Err(error) = context.dispatcher().dispatch_one_named("service_checks", service_check).await {
                        warn!(error = %error, "Failed to dispatch liveness service check.");
                    }
                },
            }
        }

        debug!("Liveness source stopped.");
        Ok(())
    }
}

fn create_liveness_payloads(hostname: MetaString, version: MetaString) -> (Context, ServiceCheck) {
    let metric_context = Context::from_static_name(RUNNING_METRIC_NAME)
        .with_host(hostname.clone())
        .with_tags(TagSet::from_iter([Tag::from(format!("version:{version}"))]));

    let service_check = ServiceCheck::new(UP_SERVICE_CHECK_NAME, CheckStatus::Ok).with_hostname(hostname);

    (metric_context, service_check)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use saluki_context::{
        origin::{OriginTagCardinality, RawOrigin},
        tags::SharedTagSet,
    };
    use saluki_core::{
        components::sources::SourceBuilder as _,
        data_model::event::{metric::MetricValues, service_check::CheckStatus, Event, EventType},
    };
    use saluki_env::{
        workload::{origin::ResolvedOrigin, EntityId},
        WorkloadProvider,
    };

    use super::*;

    #[derive(Clone)]
    struct ContainerTagProvider {
        self_container_entity: EntityId,
        tags: SharedTagSet,
    }

    impl WorkloadProvider for ContainerTagProvider {
        fn get_tags_for_entity(&self, entity_id: &EntityId, cardinality: OriginTagCardinality) -> Option<SharedTagSet> {
            (entity_id == &self.self_container_entity && cardinality == OriginTagCardinality::Low)
                .then(|| self.tags.clone())
        }

        fn get_self_container_tags(&self) -> Option<SharedTagSet> {
            self.get_tags_for_entity(&self.self_container_entity, OriginTagCardinality::Low)
        }

        fn get_resolved_origin(&self, _: RawOrigin<'_>) -> Option<ResolvedOrigin> {
            None
        }
    }

    fn resolved_self_container_entity() -> EntityId {
        EntityId::Container("06d914d2013e51a777feead523895935e33d8ad725b3251ac74c491b3d55d8fe".into())
    }

    fn container_tags() -> SharedTagSet {
        [Tag::from("container_id:adp-container")].into_iter().collect()
    }

    #[test]
    fn declares_named_metric_and_service_check_outputs() {
        let configuration = LivenessConfiguration::from_hostname("host-a".to_string());
        let outputs = configuration.outputs();

        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0].output_name(), Some("metrics"));
        assert_eq!(outputs[0].data_ty(), EventType::Metric);
        assert_eq!(outputs[1].output_name(), Some("service_checks"));
        assert_eq!(outputs[1].data_ty(), EventType::ServiceCheck);
    }

    #[test]
    fn source_configuration_requires_only_a_hostname_and_version() {
        let configuration = LivenessConfiguration::from_hostname("host-a".to_string());

        assert_eq!(configuration.hostname, "host-a");
        assert_eq!(
            configuration.version,
            saluki_metadata::get_app_details().version().raw()
        );
    }

    #[test]
    fn enabled_container_tags_are_added_to_both_liveness_signals() {
        let liveness = Liveness::new(
            "host-a".into(),
            "1.2.3".into(),
            true,
            Some(Arc::new(ContainerTagProvider {
                self_container_entity: resolved_self_container_entity(),
                tags: container_tags(),
            })),
        );
        let (metric, service_check) = liveness.signals_at(0);

        let Event::Metric(metric) = metric else {
            panic!("expected metric event");
        };
        assert!(metric.context().tags().has_tag("version:1.2.3"));
        assert!(metric.context().tags().has_tag("container_id:adp-container"));

        let Event::ServiceCheck(service_check) = service_check else {
            panic!("expected service check event");
        };
        assert!(service_check.tags().has_tag("container_id:adp-container"));
    }

    #[test]
    fn disabled_container_tags_are_absent() {
        let liveness = Liveness::new(
            "host-a".into(),
            "1.2.3".into(),
            false,
            Some(Arc::new(ContainerTagProvider {
                self_container_entity: resolved_self_container_entity(),
                tags: container_tags(),
            })),
        );
        let (metric, service_check) = liveness.signals_at(0);

        let Event::Metric(metric) = metric else {
            panic!("expected metric event");
        };
        assert!(!metric.context().tags().has_tag("container_id:adp-container"));

        let Event::ServiceCheck(service_check) = service_check else {
            panic!("expected service check event");
        };
        assert!(!service_check.tags().has_tag("container_id:adp-container"));
    }

    #[test]
    fn unavailable_container_tags_are_absent() {
        let liveness = Liveness::new("host-a".into(), "1.2.3".into(), true, None);
        let (metric, service_check) = liveness.signals_at(0);

        let Event::Metric(metric) = metric else {
            panic!("expected metric event");
        };
        assert!(!metric.context().tags().has_tag("container_id:adp-container"));

        let Event::ServiceCheck(service_check) = service_check else {
            panic!("expected service check event");
        };
        assert!(!service_check.tags().has_tag("container_id:adp-container"));
    }

    #[test]
    fn metric_payload_has_required_contract() {
        let liveness = Liveness::new("host-a".into(), "1.2.3".into(), false, None);
        let (metric, _) = liveness.signals_at(0);

        let Event::Metric(metric) = metric else {
            panic!("expected metric event");
        };
        assert_eq!(metric.context().name(), RUNNING_METRIC_NAME);
        assert_eq!(metric.context().host(), Some("host-a"));
        assert!(metric.context().tags().has_tag("version:1.2.3"));
        assert_eq!(metric.values(), &MetricValues::gauge((0, 1.0)));
    }

    #[test]
    fn metric_payload_uses_emission_timestamp() {
        let liveness = Liveness::new("host-a".into(), "1.2.3".into(), false, None);
        let emission_timestamp = 1_700_000_000;
        let (metric, _) = liveness.signals_at(emission_timestamp);
        let Event::Metric(metric) = metric else {
            panic!("expected metric event");
        };

        assert_eq!(metric.values(), &MetricValues::gauge((emission_timestamp, 1.0)));
    }

    #[test]
    fn prebuilt_service_check_payload_has_required_contract() {
        let liveness = Liveness::new("host-a".into(), "1.2.3".into(), false, None);
        let (_, service_check) = liveness.signals_at(0);

        let Event::ServiceCheck(service_check) = service_check else {
            panic!("expected service check event");
        };
        assert_eq!(service_check.name(), UP_SERVICE_CHECK_NAME);
        assert_eq!(service_check.status(), CheckStatus::Ok);
        assert_eq!(service_check.hostname(), Some("host-a"));
    }
}
