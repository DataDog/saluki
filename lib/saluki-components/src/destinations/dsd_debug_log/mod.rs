use std::{io::Write, path::PathBuf};

use agent_data_plane_config::Live;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use saluki_common::collections::FastHashMap;
use saluki_core::{
    accounting::{MemoryBounds, MemoryBoundsBuilder},
    components::{
        destinations::{Destination, DestinationBuilder, DestinationContext},
        BuildContext,
    },
    data_model::{
        event::{metric::Metric, Event, EventType},
        tags::TagSet,
    },
    topology::interconnect::InputGate,
};
use saluki_error::{generic_error, GenericError};
use stringtheory::MetaString;
use tokio::select;
use tracing::{debug, warn};
use tracing_appender::non_blocking::{NonBlocking, NonBlockingBuilder, WorkerGuard};
use tracing_rolling_file::{RollingConditionBase, RollingFileAppenderBase};

const DEBUG_LOG_WRITER_BUFFER_LINES: usize = 4096;

/// Configuration for the DogStatsD debug log destination.
///
/// The destination is a tap that writes what it receives only while metric-level statistics are enabled. Its input is
/// gated, open only while they are, so that the components feeding it neither clone nor send it anything otherwise,
/// while its received-events series keep counting everything offered to it; see [`InputGate`].
pub struct DogStatsDDebugLogConfiguration {
    /// Whether DogStatsD metric-level statistics are enabled.
    ///
    /// The destination drops metrics while this runtime setting is `false`.
    pub metrics_stats_enabled: Live<bool>,

    /// Path to the DogStatsD debug log file.
    pub log_file: PathBuf,

    /// Maximum size of the active debug log file before rotation, in bytes.
    pub log_file_max_size: u64,

    /// Number of rotated debug log files to keep.
    pub log_file_max_rolls: usize,

    /// The gate on the destination's input, open while metric-level statistics are enabled.
    gate: InputGate,
}

impl DogStatsDDebugLogConfiguration {
    /// Creates a new `DogStatsDDebugLogConfiguration`.
    ///
    /// The destination writes metrics to `log_file`, rotating it once it reaches `log_file_max_size` bytes and keeping
    /// `log_file_max_rolls` rotated files, while `metrics_stats_enabled` is `true`.
    pub fn new(
        metrics_stats_enabled: Live<bool>, log_file: PathBuf, log_file_max_size: u64, log_file_max_rolls: usize,
    ) -> Self {
        Self {
            metrics_stats_enabled,
            log_file,
            log_file_max_size,
            log_file_max_rolls,
            gate: InputGate::closed(),
        }
    }
}

/// DogStatsD destination that writes metric debug lines to a rotating file.
struct DogStatsDDebugLog {
    log_file: PathBuf,
    log_file_max_size: u64,
    log_file_max_rolls: usize,
    writer: Option<DebugLogWriter>,
    metrics_stats_enabled: Live<bool>,
    /// The gate on the destination's input, open while metric-level statistics are enabled.
    gate: InputGate,
    stats: FastHashMap<ContextNoOrigin, MetricSample>,
}

struct DebugLogWriter {
    writer: NonBlocking,
    _guard: WorkerGuard,
}

#[derive(Debug, Default)]
struct MetricSample {
    count: u64,
    last_seen: u64,
}

#[derive(Eq, Hash, PartialEq)]
struct ContextNoOrigin {
    name: MetaString,
    tags: TagSet,
}

impl DogStatsDDebugLog {
    fn new(config: &DogStatsDDebugLogConfiguration) -> Result<Self, GenericError> {
        let mut destination = Self {
            log_file: config.log_file.clone(),
            log_file_max_size: config.log_file_max_size,
            log_file_max_rolls: config.log_file_max_rolls,
            writer: None,
            metrics_stats_enabled: config.metrics_stats_enabled.clone(),
            gate: config.gate.clone(),
            stats: FastHashMap::default(),
        };

        // Built before anything is sent to it, so its input is gated by the setting from the first send on.
        destination.follow_setting();
        if *destination.metrics_stats_enabled {
            destination.ensure_writer()?;
        }

        Ok(destination)
    }

    /// Opens the input gate while metric-level statistics are enabled, and closes it otherwise.
    fn follow_setting(&self) {
        if *self.metrics_stats_enabled {
            self.gate.open();
        } else {
            self.gate.close();
        }
    }

    fn process_metric(&mut self, metric: &Metric) -> Result<(), GenericError> {
        if !*self.metrics_stats_enabled {
            return Ok(());
        }

        self.write_metric(metric)
    }

    fn write_metric(&mut self, metric: &Metric) -> Result<(), GenericError> {
        self.ensure_writer()?;

        let context = metric.context();
        let metric_context = ContextNoOrigin {
            name: context.name().clone(),
            tags: context.tags().clone(),
        };

        let timestamp = saluki_common::time::get_coarse_unix_timestamp();
        let sample = self.stats.entry(metric_context).or_default();
        sample.count += 1;
        sample.last_seen = timestamp;

        let writer = self.writer.as_mut().expect("writer should be initialized");
        writeln!(
            writer.writer,
            "Metric Name: {} | Tags: {{{}}} | Count: {} | Last Seen: {}",
            context.name(),
            format_tags(context.tags()),
            sample.count,
            format_timestamp(sample.last_seen)
        )
        .map_err(|e| {
            generic_error!(
                "Failed to write to DogStatsD debug log file '{}': {}",
                self.log_file.display(),
                e
            )
        })
    }

    fn ensure_writer(&mut self) -> Result<(), GenericError> {
        if self.writer.is_some() {
            return Ok(());
        }

        let appender = RollingFileAppenderBase::new(
            &self.log_file,
            RollingConditionBase::new().max_size(self.log_file_max_size),
            self.log_file_max_rolls,
        )
        .map_err(|e| generic_error!("Failed to open dogstatsd_log_file '{}': {}", self.log_file.display(), e))?;

        let (writer, guard) = NonBlockingBuilder::default()
            .thread_name("dsd-dbg-writer")
            .buffered_lines_limit(DEBUG_LOG_WRITER_BUFFER_LINES)
            // Drop debug log lines rather than slow DogStatsD metric ingestion.
            .lossy(true)
            .finish(appender);

        self.writer = Some(DebugLogWriter { writer, _guard: guard });

        Ok(())
    }
}

#[async_trait]
impl Destination for DogStatsDDebugLog {
    async fn run(mut self: Box<Self>, mut context: DestinationContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();
        health.mark_ready();

        loop {
            select! {
                _ = health.live() => continue,
                maybe_events = context.events().next() => match maybe_events {
                    Some(events) => {
                        for event in events {
                            if let Event::Metric(metric) = event {
                                if let Err(error) = self.process_metric(&metric) {
                                    warn!(error = %error, "Failed to write DogStatsD debug log line; continuing.");
                                }
                            }
                        }
                    },
                    None => break,
                },
                metrics_stats_enabled = self.metrics_stats_enabled.changed() => {
                    self.follow_setting();
                    debug!(metrics_stats_enabled, "Updated DogStatsD metrics stats debug logging gate.");
                },
            }
        }

        self.gate.close();
        Ok(())
    }
}

#[async_trait]
impl DestinationBuilder for DogStatsDDebugLogConfiguration {
    fn input_event_type(&self) -> EventType {
        EventType::Metric
    }

    fn input_gate(&self) -> Option<InputGate> {
        Some(self.gate.clone())
    }

    async fn build(&self, _context: BuildContext) -> Result<Box<dyn Destination + Send>, GenericError> {
        DogStatsDDebugLog::new(self).map(|destination| Box::new(destination) as Box<dyn Destination + Send>)
    }
}

impl MemoryBounds for DogStatsDDebugLogConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            .with_single_value::<DogStatsDDebugLog>("component struct");
    }
}

fn format_tags(tags: &TagSet) -> String {
    let mut formatted = String::new();

    for tag in tags {
        if !formatted.is_empty() {
            formatted.push(' ');
        }
        formatted.push_str(tag.as_str());
    }

    formatted
}

fn format_timestamp(timestamp: u64) -> String {
    i64::try_from(timestamp)
        .ok()
        .and_then(|ts| DateTime::<Utc>::from_timestamp(ts, 0))
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S +0000 UTC").to_string())
        .unwrap_or_else(|| timestamp.to_string())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::{
        fs,
        path::{Path, PathBuf},
    };
    use std::{sync::Arc, time::Duration};

    use agent_data_plane_config::{Live, SalukiConfiguration};
    use async_trait::async_trait;
    use saluki_core::{
        accounting::{ComponentRegistry, MemoryBounds, MemoryBoundsBuilder, MemoryLimiter},
        components::{
            destinations::{DestinationBuilder, DestinationContext},
            sources::{Source, SourceBuilder, SourceContext},
            BuildContext, ComponentContext,
        },
        data_model::event::{
            metric::{context::Context, Metric},
            Event, EventType,
        },
        health::HealthRegistry,
        runtime::{
            state::{DataspaceRegistry, ResourceRegistry},
            Supervisor,
        },
        topology::{interconnect::Consumer, EventsBuffer, OutputDefinition, TopologyBlueprint, TopologyContext},
    };
    use saluki_error::{generic_error, GenericError};
    use saluki_metrics::test::TestRecorder;
    use tempfile::tempdir;
    use tokio::{
        runtime::Handle,
        sync::{mpsc, oneshot},
    };

    use super::{Destination, DogStatsDDebugLog, DogStatsDDebugLogConfiguration};

    fn test_config(log_file: PathBuf, max_size: u64, max_rolls: usize) -> DogStatsDDebugLogConfiguration {
        DogStatsDDebugLogConfiguration::new(Live::new_fixed(true), log_file, max_size, max_rolls)
    }

    fn read_log_files(log_file: &Path, max_rolls: usize) -> String {
        let mut output = String::new();

        for roll in (0..=max_rolls).rev() {
            let path = rolled_path(log_file, roll);
            if path.exists() {
                output.push_str(&fs::read_to_string(&path).expect("debug log file should be readable"));
            }
        }

        output
    }

    fn rolled_path(log_file: &Path, roll: usize) -> PathBuf {
        if roll == 0 {
            log_file.to_path_buf()
        } else {
            PathBuf::from(format!("{}.{}", log_file.display(), roll))
        }
    }

    fn tagged_metric() -> Metric {
        let context = Context::from_static_parts("custom.metric", &["env:prod", "service:web"]);
        Metric::counter(context, 1.0)
    }

    #[tokio::test]
    async fn writes_metric_debug_lines_and_updates_count() {
        let tempdir = tempdir().expect("temporary directory should be created");
        let log_file = tempdir.path().join("dogstatsd-stats.log");
        let config = test_config(log_file.clone(), 64_000, 3);
        let metric = tagged_metric();

        let mut destination = DogStatsDDebugLog::new(&config).expect("debug log destination should be built");
        destination
            .write_metric(&metric)
            .expect("first metric should be written");
        destination
            .write_metric(&metric)
            .expect("second metric should be written");
        drop(destination);

        let output = read_log_files(&log_file, config.log_file_max_rolls);
        let lines = output.lines().collect::<Vec<_>>();

        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("Metric Name: custom.metric"));
        assert!(lines[0].contains("Tags: {env:prod service:web}"));
        assert!(lines[0].contains("Count: 1"));
        assert!(lines[0].contains("Last Seen: "));
        assert!(lines[1].contains("Count: 2"));
    }

    #[tokio::test]
    async fn run_starts_and_stops_logging_with_metrics_stats_setting() {
        let tempdir = tempdir().expect("temporary directory should be created");
        let log_file = tempdir.path().join("dogstatsd-stats.log");
        let cell = Arc::new(arc_swap::ArcSwap::from_pointee(SalukiConfiguration::default()));
        let (tick_tx, tick_rx) = tokio::sync::watch::channel(());
        let mut config = test_config(log_file.clone(), 64_000, 3);
        config.metrics_stats_enabled = Live::new_dynamic(Arc::clone(&cell), tick_rx, |config| {
            &config.domains.dogstatsd.debug_log.metrics_stats_enable
        });
        let destination = DogStatsDDebugLog::new(&config).expect("debug log destination should be built");

        let component_context = ComponentContext::test_destination("test");
        let (events_tx, events_rx) = mpsc::channel::<EventsBuffer>(4);
        let consumer = Consumer::new(component_context.clone(), events_rx);
        let topology_context = TopologyContext::new(
            Arc::from("test"),
            MemoryLimiter::noop(),
            HealthRegistry::new(),
            Handle::current(),
            DataspaceRegistry::new(),
        );
        let health = HealthRegistry::new()
            .register_component(&saluki_core::support::SubsystemIdentifier::from_dotted("test"))
            .expect("component was not previously registered");
        let context = DestinationContext::new(
            &topology_context,
            &component_context,
            ComponentRegistry::default(),
            health,
            consumer,
        );
        let run_handle = tokio::spawn(async move { Box::new(destination).run(context).await });

        let mut events = EventsBuffer::default();
        assert!(events.try_push(Event::Metric(tagged_metric())).is_none());
        events_tx
            .send(events)
            .await
            .expect("disabled metric should be accepted");
        tokio::time::timeout(Duration::from_secs(2), async {
            while events_tx.capacity() != 4 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("disabled metric should be consumed");
        assert!(!log_file.exists());

        let mut updated = (*cell.load_full()).clone();
        updated.domains.dogstatsd.debug_log.metrics_stats_enable = true;
        cell.store(Arc::new(updated));
        tick_tx.send_replace(());

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let mut events = EventsBuffer::default();
                assert!(events.try_push(Event::Metric(tagged_metric())).is_none());
                events_tx.send(events).await.expect("enabled metric should be accepted");
                tokio::time::sleep(Duration::from_millis(10)).await;
                if fs::read_to_string(&log_file).is_ok_and(|output| output.contains("Metric Name: custom.metric")) {
                    break;
                }
            }
        })
        .await
        .expect("metrics should be logged after the runtime setting is enabled");

        let mut updated = (*cell.load_full()).clone();
        updated.domains.dogstatsd.debug_log.metrics_stats_enable = false;
        cell.store(Arc::new(updated));
        tick_tx.send_replace(());

        let line_count_after_disable = tokio::time::timeout(Duration::from_secs(2), async {
            let mut previous_line_count = read_log_files(&log_file, config.log_file_max_rolls).lines().count();
            let mut unchanged_samples = 0;

            loop {
                let mut events = EventsBuffer::default();
                assert!(events.try_push(Event::Metric(tagged_metric())).is_none());
                events_tx
                    .send(events)
                    .await
                    .expect("metric should be accepted while disabling");
                while events_tx.capacity() != 4 {
                    tokio::task::yield_now().await;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;

                let current_line_count = read_log_files(&log_file, config.log_file_max_rolls).lines().count();
                if current_line_count == previous_line_count {
                    unchanged_samples += 1;
                    if unchanged_samples == 5 {
                        break current_line_count;
                    }
                } else {
                    previous_line_count = current_line_count;
                    unchanged_samples = 0;
                }
            }
        })
        .await
        .expect("metrics should stop being logged after the runtime setting is disabled");

        for _ in 0..3 {
            let mut events = EventsBuffer::default();
            assert!(events.try_push(Event::Metric(tagged_metric())).is_none());
            events_tx
                .send(events)
                .await
                .expect("disabled metric should be accepted");
        }
        while events_tx.capacity() != 4 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;

        let output = read_log_files(&log_file, config.log_file_max_rolls);
        assert_eq!(output.lines().count(), line_count_after_disable);

        drop(events_tx);
        run_handle
            .await
            .expect("destination task should not panic")
            .expect("destination should stop cleanly");
    }

    #[tokio::test]
    async fn rotates_log_file_at_configured_size() {
        let tempdir = tempdir().expect("temporary directory should be created");
        let log_file = tempdir.path().join("dogstatsd-stats.log");
        let min_debug_line_len =
            "Metric Name: custom.metric | Tags: {env:prod service:web} | Count: 1 | Last Seen: ".len();
        let config = test_config(log_file.clone(), min_debug_line_len as u64, 2);
        let metric = tagged_metric();

        let mut destination = DogStatsDDebugLog::new(&config).expect("debug log destination should be built");
        for _ in 0..12 {
            destination.write_metric(&metric).expect("metric should be written");
        }
        drop(destination);

        assert!(log_file.exists());
        assert!(rolled_path(&log_file, 1).exists());
        assert!(rolled_path(&log_file, 2).exists());
        assert!(!rolled_path(&log_file, 3).exists());

        let output = read_log_files(&log_file, config.log_file_max_rolls);
        assert!(output.contains("Metric Name: custom.metric"));
    }

    #[tokio::test]
    async fn build_error_mentions_log_file_config_key_and_path() {
        let tempdir = tempdir().expect("temporary directory should be created");
        let blocked_parent = tempdir.path().join("not-a-directory");
        fs::write(&blocked_parent, "not a directory").expect("blocking file should be written");
        let log_file = blocked_parent.join("dogstatsd-stats.log");
        let config = test_config(log_file.clone(), 64_000, 3);

        let err = match DogStatsDDebugLog::new(&config) {
            Ok(_) => panic!("build should fail"),
            Err(err) => err,
        };
        let err = err.to_string();

        assert!(err.contains("dogstatsd_log_file"));
        assert!(err.contains(&log_file.display().to_string()));
    }

    /// A source that dispatches each buffer it is given on its default output, until shutdown.
    struct BufferSource {
        buffers: mpsc::Receiver<EventsBuffer>,
    }

    #[async_trait]
    impl Source for BufferSource {
        async fn run(mut self: Box<Self>, mut context: SourceContext) -> Result<(), GenericError> {
            let shutdown = context.take_shutdown_handle();
            tokio::pin!(shutdown);
            loop {
                tokio::select! {
                    _ = &mut shutdown => break,
                    Some(buffer) = self.buffers.recv() => context.dispatcher().dispatch(buffer).await?,
                }
            }
            Ok(())
        }
    }

    struct BufferSourceBuilder {
        buffers: Mutex<Option<mpsc::Receiver<EventsBuffer>>>,
        outputs: Vec<OutputDefinition<EventType>>,
    }

    #[async_trait]
    impl SourceBuilder for BufferSourceBuilder {
        fn outputs(&self) -> &[OutputDefinition<EventType>] {
            &self.outputs
        }

        async fn build(&self, _context: BuildContext) -> Result<Box<dyn Source + Send>, GenericError> {
            let buffers = self
                .buffers
                .lock()
                .map_err(|_| generic_error!("the buffer source's receiver lock is poisoned"))?
                .take()
                .ok_or_else(|| generic_error!("the buffer source's receiver has already been taken"))?;
            Ok(Box::new(BufferSource { buffers }))
        }
    }

    impl MemoryBounds for BufferSourceBuilder {
        fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
    }

    /// A destination that counts the events it receives.
    struct CountingDestination {
        events: mpsc::UnboundedSender<usize>,
    }

    #[async_trait]
    impl Destination for CountingDestination {
        async fn run(self: Box<Self>, mut context: DestinationContext) -> Result<(), GenericError> {
            let mut health = context.take_health_handle();
            health.mark_ready();
            while let Some(events) = context.events().next().await {
                let _ = self.events.send(events.len());
            }
            Ok(())
        }
    }

    struct CountingDestinationBuilder {
        events: mpsc::UnboundedSender<usize>,
    }

    #[async_trait]
    impl DestinationBuilder for CountingDestinationBuilder {
        fn input_event_type(&self) -> EventType {
            EventType::Metric
        }

        async fn build(&self, _context: BuildContext) -> Result<Box<dyn Destination + Send>, GenericError> {
            Ok(Box::new(CountingDestination {
                events: self.events.clone(),
            }))
        }
    }

    impl MemoryBounds for CountingDestinationBuilder {
        fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
    }

    fn metrics_buffer(metrics: usize) -> EventsBuffer {
        let mut buffer = EventsBuffer::default();
        for _ in 0..metrics {
            assert!(buffer.try_push(Event::Metric(tagged_metric())).is_none());
        }
        buffer
    }

    #[tokio::test]
    async fn the_input_gate_is_open_exactly_while_metric_stats_are_enabled() {
        let tempdir = tempdir().expect("temporary directory should be created");
        let log_file = tempdir.path().join("dogstatsd-stats.log");
        let cell = Arc::new(arc_swap::ArcSwap::from_pointee(SalukiConfiguration::default()));
        let (tick_tx, tick_rx) = tokio::sync::watch::channel(());
        let live = Live::new_dynamic(Arc::clone(&cell), tick_rx, |config| {
            &config.domains.dogstatsd.debug_log.metrics_stats_enable
        });
        let config = DogStatsDDebugLogConfiguration::new(live, log_file, 64_000, 3);
        let gate = config.input_gate().expect("the debug log tap has an input gate");
        let set = |enabled: bool| {
            let mut updated = (*cell.load_full()).clone();
            updated.domains.dogstatsd.debug_log.metrics_stats_enable = enabled;
            cell.store(Arc::new(updated));
            tick_tx.send_replace(());
        };
        let gate_becomes = |open: bool| {
            let gate = gate.clone();
            async move {
                tokio::time::timeout(Duration::from_secs(2), async {
                    while gate.is_open() != open {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await
                .unwrap_or_else(|_| panic!("the gate should be {}", if open { "open" } else { "closed" }));
            }
        };

        // Built while statistics are disabled, the destination closes its input, and opens it once they are enabled.
        let destination = DogStatsDDebugLog::new(&config).expect("debug log destination should be built");
        assert!(!gate.is_open());

        let component_context = ComponentContext::test_destination("test");
        let (events_tx, events_rx) = mpsc::channel::<EventsBuffer>(4);
        let consumer = Consumer::new(component_context.clone(), events_rx);
        let topology_context = TopologyContext::new(
            Arc::from("test"),
            MemoryLimiter::noop(),
            HealthRegistry::new(),
            Handle::current(),
            DataspaceRegistry::new(),
        );
        let health = HealthRegistry::new()
            .register_component(&saluki_core::support::SubsystemIdentifier::from_dotted("test"))
            .expect("component was not previously registered");
        let context = DestinationContext::new(
            &topology_context,
            &component_context,
            ComponentRegistry::default(),
            health,
            consumer,
        );
        let run_handle = tokio::spawn(async move { Box::new(destination).run(context).await });

        set(true);
        gate_becomes(true).await;
        set(false);
        gate_becomes(false).await;
        set(true);
        gate_becomes(true).await;

        // A destination that stops closes its input.
        drop(events_tx);
        run_handle
            .await
            .expect("the destination task should not panic")
            .expect("the destination should stop cleanly");
        assert!(!gate.is_open());

        // Built while statistics are enabled, it opens its input before anything is sent to it.
        let enabled = test_config(tempdir.path().join("enabled.log"), 64_000, 3);
        let gate = enabled.input_gate().expect("the debug log tap has an input gate");
        assert!(!gate.is_open(), "the gate opens when the destination is built");
        let _destination = DogStatsDDebugLog::new(&enabled).expect("debug log destination should be built");
        assert!(gate.is_open());
    }

    #[tokio::test]
    async fn the_tap_is_skipped_while_metric_stats_are_disabled_and_still_counts_what_it_was_offered() {
        // src -> {owner, dsd_debug_log_out}, with metric-level statistics disabled in one run and enabled in the
        // other. The tap writes only while they are enabled, and its received-events series, like the owner's, are
        // the same in both runs: a closed gate counts what it skips as received.
        let received_total = |recorder: &TestRecorder, id: &'static str| {
            recorder.counter((
                "component_events_received_total",
                &[("component_id", id), ("component_type", "destination")][..],
            ))
        };
        let mut series = Vec::new();
        for enabled in [false, true] {
            let tempdir = tempdir().expect("temporary directory should be created");
            let log_file = tempdir.path().join("dogstatsd-stats.log");
            let recorder = TestRecorder::default();
            let _local = metrics::set_default_local_recorder(&recorder);

            let (buffers_tx, buffers_rx) = mpsc::channel(4);
            let (owner_tx, mut owner_rx) = mpsc::unbounded_channel();
            let component_registry = ComponentRegistry::default();
            let mut blueprint = TopologyBlueprint::new("debug_log_gate", &component_registry);
            blueprint
                .add_source(
                    "src",
                    BufferSourceBuilder {
                        buffers: Mutex::new(Some(buffers_rx)),
                        outputs: vec![OutputDefinition::default_output(EventType::Metric)],
                    },
                )
                .unwrap()
                .add_destination("owner", CountingDestinationBuilder { events: owner_tx })
                .unwrap()
                .add_destination(
                    "dsd_debug_log_out",
                    DogStatsDDebugLogConfiguration::new(Live::new_fixed(enabled), log_file.clone(), 64_000, 3),
                )
                .unwrap()
                .connect_components("src", ["owner", "dsd_debug_log_out"])
                .unwrap()
                .with_ambient_worker_pool()
                .with_health_registry(HealthRegistry::new())
                .with_memory_limiter(MemoryLimiter::noop())
                .with_resource_registry(ResourceRegistry::new());
            let mut supervisor = Supervisor::new("debug-log-gate").unwrap();
            supervisor.add_worker(blueprint);
            let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
            let topology = tokio::spawn(async move { supervisor.run_with_shutdown(shutdown_rx).await });

            for metrics in 1..=3 {
                buffers_tx.send(metrics_buffer(metrics)).await.unwrap();
            }
            let mut owned = 0;
            tokio::time::timeout(Duration::from_secs(5), async {
                while owned < 6 {
                    owned += owner_rx.recv().await.expect("the owner receives every buffer");
                }
            })
            .await
            .expect("the owner should receive every metric");
            shutdown_tx.send(()).unwrap();
            let result = tokio::time::timeout(Duration::from_secs(5), topology)
                .await
                .expect("the topology should stop")
                .expect("the topology task should not panic");
            assert!(result.is_ok(), "the topology should stop cleanly: {result:?}");

            // The writer flushes when the destination drops it, at its stop.
            let lines = read_log_files(&log_file, 3).lines().count();
            assert_eq!(lines, if enabled { 6 } else { 0 }, "enabled: {}", enabled);
            series.push((
                received_total(&recorder, "owner"),
                received_total(&recorder, "dsd_debug_log_out"),
            ));
        }
        assert_eq!(series, [(Some(6), Some(6)), (Some(6), Some(6))]);
    }
}
