use std::sync::Arc;
use std::time::Duration;

use saluki_core::data_model::event::Event;
use saluki_env::autodiscovery::{Data, Instance, RawData};
use saluki_error::GenericError;
use stringtheory::MetaString;

use async_trait::async_trait;
use tokio::sync::mpsc::Sender;
use tracing::{debug, error, info, trace, warn};

use integration_check::check::Check as RustCheck;
use integration_check::sink::{event, event_platform, histogram, log, metric, service_check, Sink};

use crate::sources::checks::builder::native::metric::{
    event_to_eventd, histogram_to_event, metric_to_event, service_check_to_event,
};
use crate::sources::checks::check::Check;
use crate::sources::checks::execution_context::ExecutionContext;

use serde_yaml::Mapping;

pub struct NativeSink {
    events: Sender<Event>,
    execution_context: Arc<ExecutionContext>,
}

// TODO MemoryBound
pub struct NativeCheck {
    check: Arc<dyn RustCheck<Snk = NativeSink> + Send + Sync>,
    instance: Instance,
    init_config: Data,
    name: MetaString,
    source: MetaString,
}

impl NativeCheck {
    pub async fn build<C>(
        events: Sender<Event>, name: MetaString, init_config: Data, instance: Instance, source: MetaString,
        execution_context: Arc<ExecutionContext>,
    ) -> Option<Arc<dyn Check + Send + Sync>>
    where
        C: RustCheck<Snk = NativeSink> + Send + Sync + 'static,
    {
        let sink = NativeSink {
            events,
            execution_context,
        };

        let check_init_config = data_to_mapping(init_config.get_value());
        let check_instance = data_to_mapping(instance.get_value());
        let check = C::build(sink, check_init_config, check_instance)
            .inspect_err(|err| error!(check.name = %name, error = %err, "Error creating check."))
            .ok()?;
        let check = Arc::new(check);

        let native = NativeCheck {
            check,
            init_config,
            instance,
            name,
            source,
        };
        let native = Arc::new(native);
        Some(native)
    }
}

#[async_trait]
impl Check for NativeCheck {
    async fn run(&self) -> Result<(), GenericError> {
        let _ = self.init_config; // FIXME

        trace!(check.name = %self.name, id = self.id(), "Running.");
        self.check.run().await
    }

    fn interval(&self) -> Duration {
        let min_interval = self
            .instance
            .get("min_collection_interval")
            .and_then(|value| value.as_u64())
            .unwrap_or(5); // FIXME check default from core agent
        Duration::from_secs(min_interval)
    }

    fn id(&self) -> &str {
        self.instance.id()
    }

    fn version(&self) -> &str {
        // FIXME
        // self.check.version()
        "0.0.01"
    }

    fn source(&self) -> &str {
        self.source.as_ref()
    }
}

#[async_trait]
impl Sink for NativeSink {
    async fn submit_metric(&self, metric: metric::Metric, _flush_first: bool) {
        let event = metric_to_event(metric, &self.execution_context);
        if let Err(err) = self.events.send(event).await {
            error!(error = %err, "Unable to send metric.")
        }
    }

    async fn submit_service_check(&self, service_check: service_check::ServiceCheck) {
        let event = service_check_to_event(service_check, &self.execution_context);
        if let Err(err) = self.events.send(event).await {
            error!(error = %err, "Unable to send service check.")
        }
    }

    async fn submit_event(&self, event: event::Event) {
        let eventd = event_to_eventd(event, &self.execution_context);
        if let Err(err) = self.events.send(eventd).await {
            error!(error = %err, "Unable to send event.")
        }
    }

    async fn submit_histogram(&self, histogram: histogram::Histogram, _flush_first: bool) {
        let event = histogram_to_event(histogram, &self.execution_context);
        if let Err(err) = self.events.send(event).await {
            error!(error = %err, "Unable to send histogram.")
        }
    }

    async fn submit_event_platform_event(&self, event: event_platform::Event) {
        // TODO: Implement event platform events conversion
        // For now, log that this is not yet implemented
        error!(event = ?event, "Event platform events are not yet implemented.");
    }

    async fn log(&self, level: log::Level, message: String) {
        let id = "fixme_check_id"; // FIXME
                                   // FIXME use tracing::event!
        match level {
            log::Level::Critical => error!(check.id = id, "{message}"),
            log::Level::Error => error!(check.id = id, "{message}"),
            log::Level::Warning => warn!(check.id = id, "{message}"),
            log::Level::Info => info!(check.id = id, "{message}"),
            log::Level::Debug => debug!(check.id = id, "{message}"),
            log::Level::Trace => trace!(check.id = id, "{message}"),
        }
    }
}

// FIXME Use underlying type of Data/Instance
fn data_to_mapping(data: &std::collections::BTreeMap<stringtheory::MetaString, serde_yaml::Value>) -> Mapping {
    let mut mapping = Mapping::new();
    for (key, value) in data.iter() {
        let key_value = serde_yaml::Value::String(key.to_string());
        mapping.insert(key_value, value.clone());
    }
    mapping
}
