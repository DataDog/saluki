use std::time::Duration;

use saluki_core::data_model::event::Event;
use saluki_env::autodiscovery::{Data, Instance, RawData};
use saluki_error::GenericError;
use stringtheory::MetaString;

use async_trait::async_trait;
use tokio::sync::mpsc::Sender;
use tracing::{debug, error, info, trace, warn};

use serde_yaml::Mapping;

use dd_rs_checks::check::Check as RustCheck;
use dd_rs_checks::sink::{event, event_platform, histogram, log, metric, service_check, Sink};

use crate::sources::checks::check::Check;

// TODO MemoryBound
pub struct NativeCheck<'a, C>
where
    C: RustCheck<'a> + 'a + Send + Sync,
{
    events: Sender<Event>,
    check: Option<C>,
    instance: Instance,
    init_config: Data,
    name: MetaString,
    source: MetaString,
}

impl<'a, C> NativeCheck<'a, C>
where
    C: RustCheck<'a> + 'a + Send + Sync {
    pub fn new(
        events: Sender<Event>, name: MetaString, init_config: Data, instance: Instance, source: MetaString,
    ) -> Self {
        let check_init_config = Mapping::new(); // FIXME
        let check_instance = Mapping::new(); // FIXME
        let check = None;

        let mut native = NativeCheck::<'a, C> {
            events,
            check,
            init_config,
            instance,
            name,
            source,
        };

        let check = C::build(&native, &check_init_config, &check_instance);
        native.check = Some(check);

        native
    }
}

#[async_trait]
impl<'a, C> Check for NativeCheck<'a, C>
where
C: RustCheck<'a> + 'a + Send + Sync {
    async fn run(&mut self) -> Result<(), GenericError> {
        trace!("NativeCheck::run of {} for {}", self.name, self.id());
        self.check.as_mut().unwrap().run().await
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

impl<'a, C> Sink for NativeCheck<'a, C> 
where
C: RustCheck<'a> + 'a + Send + Sync {
    fn submit_metric(&self, metric: metric::Metric, _flush_first: bool) {
        println!("submit_metric: {:#?}", metric);
    }

    fn submit_service_check(&self, service_check: service_check::ServiceCheck) {
        println!("submit_service_check: {:#?}", service_check);
    }

    fn submit_event(&self, event: event::Event) {
        println!("submit_event: {:#?}", event);
    }

    fn submit_histogram(&self, histogram: histogram::Histrogram, _flush_first: bool) {
        println!("submit_histogram: {:#?}", histogram);
    }

    fn submit_event_platform_event(&self, event: event_platform::Event) {
        println!("submit_event_platform_event: {:#?}", event);
    }

    fn log(&self, level: log::Level, message: String) {
        let id = "fixme_check_id"; // FIXME
                                   // FIXME use tracing::event!
        match level {
            log::Level::Critical => error!("[check: {}] {message}", id),
            log::Level::Error => error!("[check: {}] {message}", id),
            log::Level::Warning => warn!("[check: {}] {message}", id),
            log::Level::Info => info!("[check: {}] {message}", id),
            log::Level::Debug => debug!("[check: {}] {message}", id),
            log::Level::Trace => trace!("[check: {}] {message}", id),
        }
    }
}
