use std::{sync::LazyLock, time::Duration};

use async_trait::async_trait;
use saluki_common::time::get_unix_timestamp;
use saluki_context::{
    tags::{Tag, TagSet},
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
use saluki_env::{EnvironmentProvider as _, HostProvider as _};
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
}

impl LivenessConfiguration {
    /// Creates a liveness source configuration using the configured environment.
    pub async fn from_environment_provider(env_provider: &ADPEnvironmentProvider) -> Result<Self, GenericError> {
        let hostname = env_provider
            .host()
            .get_hostname()
            .await
            .error_context("Failed to get hostname for liveness source.")?;
        Ok(Self::from_hostname(hostname))
    }

    fn from_hostname(hostname: String) -> Self {
        Self {
            hostname: hostname.into(),
            version: saluki_metadata::get_app_details().version().raw().into(),
        }
    }
}

#[async_trait]
impl SourceBuilder for LivenessConfiguration {
    async fn build(&self, _context: ComponentContext) -> Result<Box<dyn Source + Send>, GenericError> {
        Ok(Box::new(Liveness::new(self.hostname.clone(), self.version.clone())))
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
    service_check: Event,
}

impl Liveness {
    fn new(hostname: MetaString, version: MetaString) -> Self {
        let (metric_context, service_check) = create_liveness_payloads(hostname, version);
        Self {
            metric_context,
            service_check,
        }
    }

    fn metric_at(&self, timestamp: u64) -> Event {
        Event::Metric(Metric::gauge(self.metric_context.clone(), (timestamp, 1.0)))
    }

    fn service_check(&self) -> Event {
        self.service_check.clone()
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
                    let metric = self.metric_at(get_unix_timestamp());
                    let service_check = self.service_check();

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

fn create_liveness_payloads(hostname: MetaString, version: MetaString) -> (Context, Event) {
    let metric_context = Context::from_static_name(RUNNING_METRIC_NAME)
        .with_host(hostname.clone())
        .with_tags(TagSet::from_iter([Tag::from(format!("version:{version}"))]));

    let service_check =
        Event::ServiceCheck(ServiceCheck::new(UP_SERVICE_CHECK_NAME, CheckStatus::Ok).with_hostname(hostname));

    (metric_context, service_check)
}

#[cfg(test)]
mod tests {
    use saluki_core::{
        components::sources::SourceBuilder as _,
        data_model::event::{metric::MetricValues, service_check::CheckStatus, Event, EventType},
    };

    use super::*;

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
    fn metric_payload_has_required_contract() {
        let liveness = Liveness::new("host-a".into(), "1.2.3".into());
        let metric = liveness.metric_at(0);

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
        let liveness = Liveness::new("host-a".into(), "1.2.3".into());
        let emission_timestamp = 1_700_000_000;
        let Event::Metric(metric) = liveness.metric_at(emission_timestamp) else {
            panic!("expected metric event");
        };

        assert_eq!(metric.values(), &MetricValues::gauge((emission_timestamp, 1.0)));
    }

    #[test]
    fn prebuilt_service_check_payload_has_required_contract() {
        let liveness = Liveness::new("host-a".into(), "1.2.3".into());
        let service_check = liveness.service_check();

        let Event::ServiceCheck(service_check) = service_check else {
            panic!("expected service check event");
        };
        assert_eq!(service_check.name(), UP_SERVICE_CHECK_NAME);
        assert_eq!(service_check.status(), CheckStatus::Ok);
        assert_eq!(service_check.hostname(), Some("host-a"));
    }
}
