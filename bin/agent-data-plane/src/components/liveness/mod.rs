use std::{
    sync::{Arc, LazyLock},
    time::Duration,
};

use async_trait::async_trait;
use datadog_agent_commons::ipc::{client::RemoteAgentClient, config::RemoteAgentClientConfiguration};
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
    time::{interval, timeout, MissedTickBehavior},
};
use tracing::{debug, warn};

use crate::internal::env::ADPEnvironmentProvider;

const LIVENESS_INTERVAL: Duration = Duration::from_secs(15);
const LIVENESS_IPC_TIMEOUT: Duration = Duration::from_secs(2);
const LIVENESS_DISPATCH_TIMEOUT: Duration = Duration::from_secs(2);
const RUNNING_METRIC_NAME: &str = "datadog.agent_data_plane.running";
const UP_SERVICE_CHECK_NAME: &str = "datadog.agent_data_plane.up";

/// Periodically emits Agent Data Plane liveness signals.
pub struct LivenessConfiguration {
    hostname: MetaString,
    version: MetaString,
    client_config: Option<Arc<RemoteAgentClientConfiguration>>,
}

impl LivenessConfiguration {
    /// Creates a liveness source configuration using the configured environment and Agent IPC client settings.
    pub async fn from_environment_provider(
        config: &saluki_config::GenericConfiguration, env_provider: &ADPEnvironmentProvider,
    ) -> Result<Self, GenericError> {
        let hostname = env_provider
            .host()
            .get_hostname()
            .await
            .error_context("Failed to get hostname for liveness source.")?;
        Self::from_hostname(config, hostname)
    }

    fn from_hostname(config: &saluki_config::GenericConfiguration, hostname: String) -> Result<Self, GenericError> {
        let client_config = RemoteAgentClientConfiguration::from_configuration(config)?;

        Ok(Self {
            hostname: hostname.into(),
            version: saluki_metadata::get_app_details().version().raw().into(),
            client_config: Some(Arc::new(client_config)),
        })
    }

    #[cfg(test)]
    fn for_tests(hostname: &str, version: &str) -> Self {
        Self {
            hostname: hostname.into(),
            version: version.into(),
            client_config: None,
        }
    }
}

#[async_trait]
impl SourceBuilder for LivenessConfiguration {
    async fn build(&self, _context: ComponentContext) -> Result<Box<dyn Source + Send>, GenericError> {
        Ok(Box::new(Liveness {
            hostname: self.hostname.clone(),
            version: self.version.clone(),
            client_config: self.client_config.clone(),
            client: None,
            system_tags: None,
        }))
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
    hostname: MetaString,
    version: MetaString,
    client_config: Option<Arc<RemoteAgentClientConfiguration>>,
    client: Option<RemoteAgentClient>,
    system_tags: Option<TagSet>,
}

impl Liveness {
    async fn system_tags(&mut self) -> TagSet {
        if let Some(system_tags) = &self.system_tags {
            return system_tags.clone();
        }

        if self.client.is_none() {
            let Some(client_config) = &self.client_config else {
                return TagSet::default();
            };

            match timeout(
                LIVENESS_IPC_TIMEOUT,
                RemoteAgentClient::from_client_configuration(client_config),
            )
            .await
            {
                Ok(Ok(client)) => self.client = Some(client),
                Ok(Err(error)) => {
                    warn!(error = %error, "Failed to create Agent IPC client for liveness host tags; continuing without tags.");
                    return TagSet::default();
                }
                Err(_) => {
                    warn!("Timed out creating Agent IPC client for liveness host tags; continuing without tags.");
                    return TagSet::default();
                }
            }
        }

        let client = self.client.as_ref().expect("client is initialized above");
        match timeout(LIVENESS_IPC_TIMEOUT, client.get_host_tags()).await {
            Ok(Ok(response)) => {
                let tags = response.into_inner().system.into_iter().map(Tag::from).collect();
                self.system_tags = Some(tags);
            }
            Ok(Err(error)) => {
                warn!(error = %error, "Failed to fetch liveness host tags; continuing with cached or empty tags.");
            }
            Err(_) => {
                warn!("Timed out fetching liveness host tags; continuing with cached or empty tags.");
            }
        }

        self.system_tags.clone().unwrap_or_default()
    }
}

#[async_trait]
impl Source for Liveness {
    async fn run(mut self: Box<Self>, mut context: SourceContext) -> Result<(), GenericError> {
        let global_shutdown = context.take_shutdown_handle();
        pin!(global_shutdown);

        let mut health = context.take_health_handle();
        let mut tick_interval = interval(LIVENESS_INTERVAL);
        tick_interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

        health.mark_ready();
        debug!("Liveness source started.");

        'run: loop {
            select! {
                _ = &mut global_shutdown => {
                    debug!("Received shutdown signal.");
                    break;
                },
                _ = health.live() => continue,
                _ = tick_interval.tick() => {
                    let system_tags = {
                        let system_tags = self.system_tags();
                        pin!(system_tags);
                        select! {
                            _ = &mut global_shutdown => {
                                debug!("Received shutdown signal.");
                                break 'run;
                            },
                            _ = health.live() => continue 'run,
                            system_tags = &mut system_tags => system_tags,
                        }
                    };
                    let (metric, service_check) =
                        create_liveness_events(self.hostname.clone(), self.version.clone(), &system_tags);

                    let metric_dispatch =
                        timeout(LIVENESS_DISPATCH_TIMEOUT, context.dispatcher().dispatch_one_named("metrics", metric));
                    pin!(metric_dispatch);
                    select! {
                        _ = &mut global_shutdown => {
                            debug!("Received shutdown signal.");
                            break 'run;
                        },
                        _ = health.live() => continue 'run,
                        result = &mut metric_dispatch => match result {
                            Ok(Ok(())) => {},
                            Ok(Err(error)) => warn!(error = %error, "Failed to dispatch liveness metric."),
                            Err(_) => warn!("Timed out dispatching liveness metric."),
                        },
                    }

                    let service_check_dispatch = timeout(
                        LIVENESS_DISPATCH_TIMEOUT,
                        context.dispatcher().dispatch_one_named("service_checks", service_check),
                    );
                    pin!(service_check_dispatch);
                    select! {
                        _ = &mut global_shutdown => {
                            debug!("Received shutdown signal.");
                            break 'run;
                        },
                        _ = health.live() => continue 'run,
                        result = &mut service_check_dispatch => match result {
                            Ok(Ok(())) => {},
                            Ok(Err(error)) => warn!(error = %error, "Failed to dispatch liveness service check."),
                            Err(_) => warn!("Timed out dispatching liveness service check."),
                        },
                    }
                },
            }
        }

        debug!("Liveness source stopped.");
        Ok(())
    }
}

fn create_liveness_events(hostname: MetaString, version: MetaString, system_tags: &TagSet) -> (Event, Event) {
    let mut metric_tags = system_tags.clone();
    metric_tags.insert_tag(format!("version:{version}"));
    let metric_context = Context::from_static_name(RUNNING_METRIC_NAME)
        .with_host(hostname.clone())
        .with_tags(metric_tags);
    let metric = Event::Metric(Metric::gauge(metric_context, 1.0));

    let service_check = Event::ServiceCheck(
        ServiceCheck::new(UP_SERVICE_CHECK_NAME, CheckStatus::Ok)
            .with_hostname(hostname)
            .with_tags(system_tags.clone()),
    );

    (metric, service_check)
}

#[cfg(test)]
mod tests {
    use saluki_config::ConfigurationLoader;
    use saluki_context::tags::TagSet;
    use saluki_core::{
        components::sources::SourceBuilder as _,
        data_model::event::{metric::MetricValues, service_check::CheckStatus, Event, EventType},
    };
    use serde_json::json;

    use super::*;

    #[test]
    fn declares_named_metric_and_service_check_outputs() {
        let configuration = LivenessConfiguration::for_tests("host-a", "1.2.3");
        let outputs = configuration.outputs();

        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0].output_name(), Some("metrics"));
        assert_eq!(outputs[0].data_ty(), EventType::Metric);
        assert_eq!(outputs[1].output_name(), Some("service_checks"));
        assert_eq!(outputs[1].data_ty(), EventType::ServiceCheck);
    }

    #[tokio::test]
    async fn source_configuration_parses_ipc_settings_without_creating_a_client() {
        let (config, _) = ConfigurationLoader::for_tests(
            Some(json!({
                "auth_token_file_path": "/path/that/does/not/exist",
                "cmd_port": 9,
            })),
            None,
            false,
        )
        .await;

        let configuration = LivenessConfiguration::from_hostname(&config, "host-a".to_string())
            .expect("IPC settings should parse without connecting to the Agent");

        assert!(configuration.client_config.is_some());
    }

    #[test]
    fn emits_core_agent_compatible_liveness_events() {
        let system_tags = TagSet::from_iter(["system:linux".into(), "region:us-east-1".into()]);
        let (metric, service_check) = create_liveness_events("host-a".into(), "1.2.3".into(), &system_tags);

        let Event::Metric(metric) = metric else {
            panic!("expected metric event");
        };
        assert_eq!(metric.context().name(), RUNNING_METRIC_NAME);
        assert_eq!(metric.context().host(), Some("host-a"));
        assert!(metric.context().tags().has_tag("system:linux"));
        assert!(metric.context().tags().has_tag("region:us-east-1"));
        assert!(metric.context().tags().has_tag("version:1.2.3"));
        assert_eq!(metric.values(), &MetricValues::gauge((0, 1.0)));

        let Event::ServiceCheck(service_check) = service_check else {
            panic!("expected service check event");
        };
        assert_eq!(service_check.name(), UP_SERVICE_CHECK_NAME);
        assert_eq!(service_check.status(), CheckStatus::Ok);
        assert_eq!(service_check.hostname(), Some("host-a"));
        assert!(service_check.tags().has_tag("system:linux"));
        assert!(service_check.tags().has_tag("region:us-east-1"));
        assert!(!service_check.tags().has_tag("version:1.2.3"));
    }
}
