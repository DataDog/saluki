use std::{
    io::Write,
    path::{Path, PathBuf},
};

use agent_data_plane_config::{domains::dogstatsd::DebugLog, Live};
use async_trait::async_trait;
use bytesize::ByteSize;
use chrono::{DateTime, Utc};
use resource_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_common::collections::FastHashMap;
use saluki_context::tags::TagSet;
use saluki_core::{
    components::{
        destinations::{Destination, DestinationBuilder, DestinationContext},
        ComponentContext,
    },
    data_model::event::{metric::Metric, Event, EventType},
};
use saluki_error::{generic_error, GenericError};
use stringtheory::MetaString;
use tokio::select;
use tracing::{debug, warn};
use tracing_appender::non_blocking::{NonBlocking, NonBlockingBuilder, WorkerGuard};
use tracing_rolling_file::{RollingConditionBase, RollingFileAppenderBase};

const DEBUG_LOG_WRITER_BUFFER_LINES: usize = 4096;

/// Configuration for the DogStatsD debug log destination.
#[derive(Clone, Debug)]
pub struct DogStatsDDebugLogConfiguration {
    /// Whether DogStatsD metric-level statistics are enabled.
    ///
    /// The debug log destination is always present when logging is enabled, but it drops metrics
    /// until this runtime flag becomes `true`, and only during that period.
    metrics_stats_enabled: bool,

    /// Whether DogStatsD metric-level statistics should also be written to a log file.
    ///
    /// This controls whether the destination is added to the topology.
    logging_enabled: bool,

    /// Path to the DogStatsD debug log file.
    log_file: PathBuf,

    /// Maximum size of the active debug log file before rotation.
    log_file_max_size: ByteSize,

    /// Number of rotated debug log files to keep.
    log_file_max_rolls: usize,

    /// Live view of the debug log settings, used to react to runtime changes.
    live: Live<DebugLog>,
}

/// DogStatsD destination that writes metric debug lines to a rotating file.
struct DogStatsDDebugLog {
    log_file: PathBuf,
    log_file_max_size: ByteSize,
    log_file_max_rolls: usize,
    writer: Option<DebugLogWriter>,
    metrics_stats_enabled: bool,
    live: Live<DebugLog>,
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

impl DogStatsDDebugLogConfiguration {
    /// Builds a debug log configuration from the DogStatsD debug log settings.
    ///
    /// If the configured log file path is empty, `default_log_file_path` is used. The `live` view
    /// drives runtime changes to the metric-statistics gate.
    pub fn from_configuration(
        debug_log: &DebugLog, default_log_file_path: PathBuf, live: Live<DebugLog>,
    ) -> Result<Self, GenericError> {
        let log_file = if debug_log.log_file.as_os_str().is_empty() {
            default_log_file_path
        } else {
            debug_log.log_file.clone()
        };

        if log_file.to_str().is_none() {
            return Err(generic_error!(
                "DogStatsD debug log file path must be valid UTF-8, got '{}'",
                log_file.display()
            ));
        }

        Ok(Self {
            metrics_stats_enabled: debug_log.metrics_stats_enable,
            logging_enabled: debug_log.logging_enabled,
            log_file,
            log_file_max_size: ByteSize::b(debug_log.log_file_max_size),
            log_file_max_rolls: debug_log.log_file_max_rolls,
            live,
        })
    }

    /// Returns `true` if the debug log destination should be added to the topology.
    pub const fn enabled(&self) -> bool {
        self.logging_enabled
    }

    /// Returns the DogStatsD debug log file path.
    pub fn log_file(&self) -> &Path {
        &self.log_file
    }

    /// Returns the maximum size of the active debug log file before rotation.
    pub const fn log_file_max_size(&self) -> ByteSize {
        self.log_file_max_size
    }

    /// Returns the number of rotated debug log files to keep.
    pub const fn log_file_max_rolls(&self) -> usize {
        self.log_file_max_rolls
    }
}

impl DogStatsDDebugLog {
    fn from_configuration(config: &DogStatsDDebugLogConfiguration) -> Result<Self, GenericError> {
        let mut destination = Self {
            log_file: config.log_file.clone(),
            log_file_max_size: config.log_file_max_size,
            log_file_max_rolls: config.log_file_max_rolls,
            writer: None,
            metrics_stats_enabled: config.metrics_stats_enabled,
            live: config.live.clone(),
            stats: FastHashMap::default(),
        };

        if destination.metrics_stats_enabled {
            destination.ensure_writer()?;
        }

        Ok(destination)
    }

    fn process_metric(&mut self, metric: &Metric) -> Result<(), GenericError> {
        if !self.metrics_stats_enabled {
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
            RollingConditionBase::new().max_size(self.log_file_max_size.as_u64()),
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

        let mut live = self.live.clone();

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
                _ = live.changed() => {
                    let metrics_stats_enabled = live.current().metrics_stats_enable;
                    self.metrics_stats_enabled = metrics_stats_enabled;
                    debug!(metrics_stats_enabled, "Updated DogStatsD metrics stats debug logging gate.");
                },
            }
        }

        Ok(())
    }
}

#[async_trait]
impl DestinationBuilder for DogStatsDDebugLogConfiguration {
    fn input_event_type(&self) -> EventType {
        EventType::Metric
    }

    async fn build(&self, _context: ComponentContext) -> Result<Box<dyn Destination + Send>, GenericError> {
        DogStatsDDebugLog::from_configuration(self)
            .map(|destination| Box::new(destination) as Box<dyn Destination + Send>)
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
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::Arc,
        time::Duration,
    };

    use agent_data_plane_config::{domains::dogstatsd::DebugLog, Live, SalukiConfiguration};
    use arc_swap::ArcSwap;
    use bytesize::ByteSize;
    use saluki_context::Context;
    use saluki_core::data_model::event::metric::Metric;
    use tempfile::tempdir;
    use tokio::sync::watch;

    use super::{DogStatsDDebugLog, DogStatsDDebugLogConfiguration};

    fn test_default_log_file_path() -> PathBuf {
        PathBuf::from("/tmp/default-dogstatsd-stats.log")
    }

    /// A debug log slice that mirrors what the translator produces for the core Agent defaults.
    fn debug_log(log_file: PathBuf) -> DebugLog {
        DebugLog {
            logging_enabled: true,
            log_file,
            log_file_max_rolls: 3,
            log_file_max_size: 10_000_000,
            metrics_stats_enable: false,
            disable_verbose_logs: false,
        }
    }

    fn config_from(debug_log: DebugLog) -> DogStatsDDebugLogConfiguration {
        let live = Live::fixed(debug_log.clone());
        DogStatsDDebugLogConfiguration::from_configuration(&debug_log, test_default_log_file_path(), live)
            .expect("configuration should build")
    }

    #[test]
    fn maps_model_fields_onto_the_component_config() {
        let config = config_from(DebugLog {
            log_file_max_size: 65_536,
            ..debug_log(PathBuf::from("/tmp/dsd-debug.log"))
        });

        assert!(config.enabled());
        assert!(!config.metrics_stats_enabled);
        assert_eq!(config.log_file(), Path::new("/tmp/dsd-debug.log"));
        assert_eq!(config.log_file_max_size(), ByteSize::b(65_536));
        assert_eq!(config.log_file_max_rolls(), 3);
    }

    #[test]
    fn empty_log_file_path_falls_back_to_default() {
        let config = config_from(debug_log(PathBuf::new()));

        assert_eq!(config.log_file(), test_default_log_file_path());
    }

    #[test]
    fn explicit_log_file_path_is_preserved() {
        let config = config_from(debug_log(PathBuf::from("/tmp/dsd-debug.log")));

        assert_eq!(config.log_file(), Path::new("/tmp/dsd-debug.log"));
    }

    #[test]
    fn logging_enabled_gates_topology_wiring() {
        let disabled = config_from(DebugLog {
            logging_enabled: false,
            ..debug_log(PathBuf::from("/tmp/dsd-debug.log"))
        });
        assert!(!disabled.enabled());

        let enabled = config_from(debug_log(PathBuf::from("/tmp/dsd-debug.log")));
        assert!(enabled.enabled());
    }

    fn writable_config(log_file: PathBuf, max_size: ByteSize, max_rolls: usize) -> DogStatsDDebugLogConfiguration {
        config_from(DebugLog {
            log_file,
            log_file_max_size: max_size.as_u64(),
            log_file_max_rolls: max_rolls,
            metrics_stats_enable: true,
            ..DebugLog::default()
        })
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
        let config = writable_config(log_file.clone(), ByteSize::kb(64), 3);
        let metric = tagged_metric();

        let mut destination =
            DogStatsDDebugLog::from_configuration(&config).expect("debug log destination should be built");
        destination
            .write_metric(&metric)
            .expect("first metric should be written");
        destination
            .write_metric(&metric)
            .expect("second metric should be written");
        drop(destination);

        let output = read_log_files(&log_file, config.log_file_max_rolls());
        let lines = output.lines().collect::<Vec<_>>();

        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("Metric Name: custom.metric"));
        assert!(lines[0].contains("Tags: {env:prod service:web}"));
        assert!(lines[0].contains("Count: 1"));
        assert!(lines[0].contains("Last Seen: "));
        assert!(lines[1].contains("Count: 2"));
    }

    /// A [`Live`] view backed by a cell the test can flip, mirroring how the config system drives
    /// runtime updates.
    fn drivable_live(debug_log: DebugLog) -> (Arc<ArcSwap<SalukiConfiguration>>, watch::Sender<()>, Live<DebugLog>) {
        let mut config = SalukiConfiguration::default();
        config.domains.dogstatsd.debug_log = debug_log;

        let cell = Arc::new(ArcSwap::from_pointee(config));
        let (tx, rx) = watch::channel(());
        let live = Live::dynamic(Arc::clone(&cell), rx, |c| &c.domains.dogstatsd.debug_log);
        (cell, tx, live)
    }

    fn set_metrics_stats(cell: &ArcSwap<SalukiConfiguration>, tx: &watch::Sender<()>, base: &DebugLog, enabled: bool) {
        let mut config = SalukiConfiguration::default();
        config.domains.dogstatsd.debug_log = DebugLog {
            metrics_stats_enable: enabled,
            ..base.clone()
        };
        cell.store(Arc::new(config));
        tx.send(()).expect("live cell should have a receiver");
    }

    #[tokio::test]
    async fn dynamically_drops_until_metrics_stats_are_enabled() {
        let tempdir = tempdir().expect("temporary directory should be created");
        let log_file = tempdir.path().join("dogstatsd-stats.log");
        let base = DebugLog {
            log_file: log_file.clone(),
            log_file_max_size: ByteSize::kb(64).as_u64(),
            log_file_max_rolls: 3,
            metrics_stats_enable: false,
            ..DebugLog::default()
        };
        let (cell, tx, live) = drivable_live(base.clone());

        let config = DogStatsDDebugLogConfiguration::from_configuration(&base, test_default_log_file_path(), live)
            .expect("configuration should build");
        let mut destination =
            DogStatsDDebugLog::from_configuration(&config).expect("debug log destination should be built");

        let metric = tagged_metric();

        // Statistics start disabled, so metrics are dropped and no file is created.
        destination
            .process_metric(&metric)
            .expect("disabled metric should be dropped cleanly");
        assert!(!log_file.exists());

        // Enabling the runtime flag through the live view flips the gate on.
        set_metrics_stats(&cell, &tx, &base, true);
        tokio::time::timeout(Duration::from_secs(2), destination.live.changed())
            .await
            .expect("live view should observe the enabled update");
        destination.metrics_stats_enabled = destination.live.current().metrics_stats_enable;
        assert!(destination.metrics_stats_enabled);
        destination
            .process_metric(&metric)
            .expect("enabled metric should be written");

        // Disabling it again stops collection.
        set_metrics_stats(&cell, &tx, &base, false);
        tokio::time::timeout(Duration::from_secs(2), destination.live.changed())
            .await
            .expect("live view should observe the disabled update");
        destination.metrics_stats_enabled = destination.live.current().metrics_stats_enable;
        assert!(!destination.metrics_stats_enabled);
        destination
            .process_metric(&metric)
            .expect("disabled metric should be dropped cleanly");
        drop(destination);

        let output = read_log_files(&log_file, config.log_file_max_rolls());
        let lines = output.lines().collect::<Vec<_>>();

        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("Metric Name: custom.metric"));
        assert!(lines[0].contains("Count: 1"));
    }

    #[tokio::test]
    async fn rotates_log_file_at_configured_size() {
        let tempdir = tempdir().expect("temporary directory should be created");
        let log_file = tempdir.path().join("dogstatsd-stats.log");
        let min_debug_line_len =
            "Metric Name: custom.metric | Tags: {env:prod service:web} | Count: 1 | Last Seen: ".len();
        let config = writable_config(log_file.clone(), ByteSize::b(min_debug_line_len as u64), 2);
        let metric = tagged_metric();

        let mut destination =
            DogStatsDDebugLog::from_configuration(&config).expect("debug log destination should be built");
        for _ in 0..12 {
            destination.write_metric(&metric).expect("metric should be written");
        }
        drop(destination);

        assert!(log_file.exists());
        assert!(rolled_path(&log_file, 1).exists());
        assert!(rolled_path(&log_file, 2).exists());
        assert!(!rolled_path(&log_file, 3).exists());

        let output = read_log_files(&log_file, config.log_file_max_rolls());
        assert!(output.contains("Metric Name: custom.metric"));
    }

    #[tokio::test]
    async fn build_error_mentions_log_file_config_key_and_path() {
        let tempdir = tempdir().expect("temporary directory should be created");
        let blocked_parent = tempdir.path().join("not-a-directory");
        fs::write(&blocked_parent, "not a directory").expect("blocking file should be written");
        let log_file = blocked_parent.join("dogstatsd-stats.log");
        let config = writable_config(log_file.clone(), ByteSize::kb(64), 3);

        let err = match DogStatsDDebugLog::from_configuration(&config) {
            Ok(_) => panic!("build should fail"),
            Err(err) => err,
        };
        let err = err.to_string();

        assert!(err.contains("dogstatsd_log_file"));
        assert!(err.contains(&log_file.display().to_string()));
    }
}
