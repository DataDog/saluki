use std::{io::Write, path::PathBuf};

use async_trait::async_trait;
use bytesize::ByteSize;
use chrono::{DateTime, Utc};
use resource_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_common::collections::FastHashMap;
use saluki_component_config::dogstatsd::DogStatsDDebugLogConfig;
use saluki_component_config::ScopedConfig;
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
///
/// This is a dynamic-capable component: it consumes a [`ScopedConfig<DogStatsDDebugLogConfig>`] and
/// rebuilds its local state whenever a new configuration value is published (notably the
/// `metrics_stats_enabled` runtime gate).
pub struct DogStatsDDebugLogConfiguration {
    config: ScopedConfig<DogStatsDDebugLogConfig>,
}

impl DogStatsDDebugLogConfiguration {
    /// Creates a new `DogStatsDDebugLogConfiguration` from the given native configuration handle.
    pub fn from_native(config: ScopedConfig<DogStatsDDebugLogConfig>) -> Self {
        Self { config }
    }

    /// Returns `true` if the debug log destination should be added to the topology.
    pub fn enabled(&self) -> bool {
        self.config.current().logging_enabled
    }
}

/// DogStatsD destination that writes metric debug lines to a rotating file.
struct DogStatsDDebugLog {
    log_file: PathBuf,
    log_file_max_size: ByteSize,
    log_file_max_rolls: usize,
    writer: Option<DebugLogWriter>,
    metrics_stats_enabled: bool,
    config: ScopedConfig<DogStatsDDebugLogConfig>,
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
    /// Builds the runtime destination from a configuration handle.
    ///
    /// Local state (log file path, rotation settings, and the metrics-stats gate) is derived from
    /// the current configuration value; it is rebuilt from `config.current()` whenever the handle
    /// publishes an update.
    fn from_config(config: ScopedConfig<DogStatsDDebugLogConfig>) -> Result<Self, GenericError> {
        let current = config.current();

        if current.log_file.to_str().is_none() {
            return Err(generic_error!(
                "dogstatsd_log_file must be valid UTF-8, got '{}'",
                current.log_file.display()
            ));
        }

        let mut destination = Self {
            log_file: current.log_file.clone(),
            log_file_max_size: current.log_file_max_size,
            log_file_max_rolls: current.log_file_max_rolls,
            writer: None,
            metrics_stats_enabled: current.metrics_stats_enabled,
            config,
            stats: FastHashMap::default(),
        };

        if destination.metrics_stats_enabled {
            destination.ensure_writer()?;
        }

        Ok(destination)
    }

    /// Rebuilds local state from the latest configuration value.
    ///
    /// The metrics-stats gate is the dynamic field of interest, but the log file path and rotation
    /// settings are refreshed as well so a published update is fully reflected. A new writer is
    /// opened lazily on the next write when stats are enabled.
    fn rebuild_state(&mut self) {
        let current = self.config.current();

        if self.log_file != current.log_file
            || self.log_file_max_size != current.log_file_max_size
            || self.log_file_max_rolls != current.log_file_max_rolls
        {
            self.log_file = current.log_file.clone();
            self.log_file_max_size = current.log_file_max_size;
            self.log_file_max_rolls = current.log_file_max_rolls;
            self.writer = None;
        }

        self.metrics_stats_enabled = current.metrics_stats_enabled;
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
                _ = self.config.changed() => {
                    self.rebuild_state();
                    debug!(
                        metrics_stats_enabled = self.metrics_stats_enabled,
                        "Rebuilt DogStatsD debug log state from updated configuration."
                    );
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
        DogStatsDDebugLog::from_config(self.config.clone())
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
    };

    use bytesize::ByteSize;
    use saluki_component_config::ScopedConfig;
    use saluki_context::Context;
    use saluki_core::data_model::event::metric::Metric;
    use tempfile::tempdir;
    use tokio::sync::watch;

    use super::{DogStatsDDebugLog, DogStatsDDebugLogConfig as LeafConfig};

    fn leaf_config(log_file: PathBuf, max_size: ByteSize, max_rolls: usize) -> LeafConfig {
        LeafConfig {
            metrics_stats_enabled: true,
            logging_enabled: true,
            log_file,
            log_file_max_size: max_size,
            log_file_max_rolls: max_rolls,
        }
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
        let config = leaf_config(log_file.clone(), ByteSize::kb(64), 3);
        let max_rolls = config.log_file_max_rolls;
        let metric = tagged_metric();

        let mut destination =
            DogStatsDDebugLog::from_config(ScopedConfig::fixed(config)).expect("debug log destination should be built");
        destination
            .write_metric(&metric)
            .expect("first metric should be written");
        destination
            .write_metric(&metric)
            .expect("second metric should be written");
        drop(destination);

        let output = read_log_files(&log_file, max_rolls);
        let lines = output.lines().collect::<Vec<_>>();

        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("Metric Name: custom.metric"));
        assert!(lines[0].contains("Tags: {env:prod service:web}"));
        assert!(lines[0].contains("Count: 1"));
        assert!(lines[0].contains("Last Seen: "));
        assert!(lines[1].contains("Count: 2"));
    }

    #[tokio::test]
    async fn dynamically_drops_until_metrics_stats_are_enabled() {
        let tempdir = tempdir().expect("temporary directory should be created");
        let log_file = tempdir.path().join("dogstatsd-stats.log");

        // Start with the metrics-stats gate disabled and drive it through a live update channel.
        let initial = LeafConfig {
            metrics_stats_enabled: false,
            logging_enabled: true,
            log_file: log_file.clone(),
            log_file_max_size: ByteSize::kb(64),
            log_file_max_rolls: 3,
        };
        let (tx, rx) = watch::channel(initial.clone());
        let handle = ScopedConfig::live(initial, rx);
        let max_rolls = handle.current().log_file_max_rolls;

        let metric = tagged_metric();
        let mut destination = DogStatsDDebugLog::from_config(handle).expect("debug log destination should be built");

        // Disabled gate: metric is dropped and no file is created.
        destination
            .process_metric(&metric)
            .expect("disabled metric should be dropped cleanly");
        assert!(!log_file.exists());

        // Publish an update enabling the gate, then mirror the reactive rebuild from run().
        tx.send(LeafConfig {
            metrics_stats_enabled: true,
            ..destination.config.current()
        })
        .expect("receiver is alive");
        destination.config.changed().await;
        destination.rebuild_state();
        assert!(destination.metrics_stats_enabled);

        destination
            .process_metric(&metric)
            .expect("enabled metric should be written");

        // Publish an update disabling the gate again.
        tx.send(LeafConfig {
            metrics_stats_enabled: false,
            ..destination.config.current()
        })
        .expect("receiver is alive");
        destination.config.changed().await;
        destination.rebuild_state();
        assert!(!destination.metrics_stats_enabled);

        destination
            .process_metric(&metric)
            .expect("disabled metric should be dropped cleanly");
        drop(destination);

        let output = read_log_files(&log_file, max_rolls);
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
        let config = leaf_config(log_file.clone(), ByteSize::b(min_debug_line_len as u64), 2);
        let max_rolls = config.log_file_max_rolls;
        let metric = tagged_metric();

        let mut destination =
            DogStatsDDebugLog::from_config(ScopedConfig::fixed(config)).expect("debug log destination should be built");
        for _ in 0..12 {
            destination.write_metric(&metric).expect("metric should be written");
        }
        drop(destination);

        assert!(log_file.exists());
        assert!(rolled_path(&log_file, 1).exists());
        assert!(rolled_path(&log_file, 2).exists());
        assert!(!rolled_path(&log_file, 3).exists());

        let output = read_log_files(&log_file, max_rolls);
        assert!(output.contains("Metric Name: custom.metric"));
    }

    #[tokio::test]
    async fn build_error_mentions_log_file_config_key_and_path() {
        let tempdir = tempdir().expect("temporary directory should be created");
        let blocked_parent = tempdir.path().join("not-a-directory");
        fs::write(&blocked_parent, "not a directory").expect("blocking file should be written");
        let log_file = blocked_parent.join("dogstatsd-stats.log");
        let config = leaf_config(log_file.clone(), ByteSize::kb(64), 3);

        let err = match DogStatsDDebugLog::from_config(ScopedConfig::fixed(config)) {
            Ok(_) => panic!("build should fail"),
            Err(err) => err,
        };
        let err = err.to_string();

        assert!(err.contains("dogstatsd_log_file"));
        assert!(err.contains(&log_file.display().to_string()));
    }
}
