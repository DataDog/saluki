use std::{
    collections::HashMap,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use async_trait::async_trait;
use bytesize::ByteSize;
use chrono::{DateTime, Utc};
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_config::GenericConfiguration;
use saluki_context::tags::TagSet;
use saluki_core::{
    components::{
        destinations::{Destination, DestinationBuilder, DestinationContext},
        ComponentContext,
    },
    data_model::event::{metric::Metric, Event, EventType},
};
use saluki_error::{generic_error, GenericError};
use serde::Deserialize;
use stringtheory::MetaString;
use tokio::select;
use tracing_appender::non_blocking::{NonBlocking, NonBlockingBuilder, WorkerGuard};
use tracing_rolling_file::{RollingConditionBase, RollingFileAppenderBase};

const DEFAULT_DOGSTATSD_LOG_FILE_MAX_SIZE: ByteSize = ByteSize::mb(10);
const DEFAULT_DOGSTATSD_LOG_FILE_MAX_ROLLS: usize = 3;
const DEBUG_LOG_WRITER_BUFFER_LINES: usize = 4096;
const DOGSTATSD_METRICS_STATS_ENABLE_KEY: &str = "dogstatsd_metrics_stats_enable";

const fn default_true() -> bool {
    true
}

const fn default_log_file_max_size() -> ByteSize {
    DEFAULT_DOGSTATSD_LOG_FILE_MAX_SIZE
}

const fn default_log_file_max_rolls() -> usize {
    DEFAULT_DOGSTATSD_LOG_FILE_MAX_ROLLS
}

/// Configuration for the DogStatsD debug log destination.
#[derive(Clone, Debug, Deserialize)]
#[cfg_attr(test, derive(PartialEq, serde::Serialize))]
pub struct DogStatsDDebugLogConfiguration {
    /// Whether DogStatsD metric-level statistics are enabled.
    ///
    /// This defaults to `false`. The debug log destination is wired when `dogstatsd_logging_enabled` is `true`, but
    /// it drops metrics until this runtime flag becomes `true`.
    #[serde(rename = "dogstatsd_metrics_stats_enable", default)]
    metrics_stats_enabled: bool,

    #[serde(skip)]
    metrics_stats_enabled_state: RuntimeBool,

    /// Whether DogStatsD metric-level statistics should also be written to a log file.
    ///
    /// This defaults to `true`, matching the core Agent. This controls whether the destination is added to the
    /// topology.
    #[serde(rename = "dogstatsd_logging_enabled", default = "default_true")]
    logging_enabled: bool,

    /// Path to the DogStatsD debug log file.
    ///
    /// This defaults to the platform-specific core Agent DogStatsD stats log path when the configured value is empty.
    #[serde(rename = "dogstatsd_log_file", default)]
    log_file: PathBuf,

    /// Maximum size of the active debug log file before rotation.
    ///
    /// This defaults to `10Mb`.
    #[serde(rename = "dogstatsd_log_file_max_size", default = "default_log_file_max_size")]
    log_file_max_size: ByteSize,

    /// Number of rotated debug log files to keep.
    ///
    /// This defaults to `3`.
    #[serde(rename = "dogstatsd_log_file_max_rolls", default = "default_log_file_max_rolls")]
    log_file_max_rolls: usize,
}

/// DogStatsD destination that writes metric debug lines to a rotating file.
struct DogStatsDDebugLog {
    log_file: PathBuf,
    log_file_max_size: ByteSize,
    log_file_max_rolls: usize,
    writer: Option<DebugLogWriter>,
    metrics_stats_enabled: RuntimeBool,
    stats: HashMap<ContextNoOrigin, MetricSample>,
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

#[derive(Clone, Debug)]
struct RuntimeBool {
    value: Arc<AtomicBool>,
}

impl RuntimeBool {
    fn new(value: bool) -> Self {
        Self {
            value: Arc::new(AtomicBool::new(value)),
        }
    }

    fn load(&self) -> bool {
        self.value.load(Ordering::Relaxed)
    }

    fn store(&self, value: bool) {
        self.value.store(value, Ordering::Relaxed);
    }
}

impl Default for RuntimeBool {
    fn default() -> Self {
        Self::new(false)
    }
}

impl PartialEq for RuntimeBool {
    fn eq(&self, other: &Self) -> bool {
        self.load() == other.load()
    }
}

impl DogStatsDDebugLogConfiguration {
    /// Creates a new `DogStatsDDebugLogConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let mut cfg: Self = config.as_typed()?;

        if cfg.log_file.as_os_str().is_empty() {
            cfg.log_file = Self::default_log_file_path();
        }

        cfg.metrics_stats_enabled_state = RuntimeBool::new(cfg.metrics_stats_enabled);
        if cfg.logging_enabled {
            cfg.watch_metrics_stats_enabled(config);
        }

        Ok(cfg)
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

    /// Returns the default DogStatsD debug log file path for the current platform.
    pub fn default_log_file_path() -> PathBuf {
        default_dogstatsd_log_file_path()
    }

    fn watch_metrics_stats_enabled(&self, config: &GenericConfiguration) {
        let Some(mut updates) = config.subscribe_for_updates() else {
            return;
        };

        let metrics_stats_enabled = self.metrics_stats_enabled_state.clone();
        tokio::spawn(async move {
            loop {
                match updates.recv().await {
                    Ok(event) if event.key == DOGSTATSD_METRICS_STATS_ENABLE_KEY => {
                        match event
                            .new_value
                            .as_ref()
                            .and_then(|value| serde_json::from_value::<bool>(value.clone()).ok())
                        {
                            Some(enabled) => metrics_stats_enabled.store(enabled),
                            None => tracing::warn!(
                                key = DOGSTATSD_METRICS_STATS_ENABLE_KEY,
                                value = ?event.new_value,
                                "Ignoring invalid dynamic DogStatsD metrics stats update."
                            ),
                        }
                    }
                    Ok(_) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        tracing::warn!(
                            key = DOGSTATSD_METRICS_STATS_ENABLE_KEY,
                            "Dropped dynamic configuration update events while watching DogStatsD metrics stats."
                        );
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }
}

impl DogStatsDDebugLog {
    fn from_configuration(config: &DogStatsDDebugLogConfiguration) -> Result<Self, GenericError> {
        let mut destination = Self {
            log_file: config.log_file.clone(),
            log_file_max_size: config.log_file_max_size,
            log_file_max_rolls: config.log_file_max_rolls,
            writer: None,
            metrics_stats_enabled: config.metrics_stats_enabled_state.clone(),
            stats: HashMap::new(),
        };

        if destination.metrics_stats_enabled.load() {
            destination.ensure_writer()?;
        }

        Ok(destination)
    }

    fn process_metric(&mut self, metric: &Metric) -> Result<(), GenericError> {
        if !self.metrics_stats_enabled.load() {
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
                "Failed to write DogStatsD debug log line to dogstatsd_log_file '{}': {}",
                self.log_file.display(),
                e
            )
        })
    }

    fn ensure_writer(&mut self) -> Result<(), GenericError> {
        if self.writer.is_some() {
            return Ok(());
        }

        self.writer = Some(build_debug_log_writer(
            &self.log_file,
            self.log_file_max_size,
            self.log_file_max_rolls,
        )?);

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
                                    tracing::warn!(error = %error, "Failed to write DogStatsD debug log line; continuing.");
                                }
                            }
                        }
                    },
                    None => break,
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

fn build_debug_log_writer(
    log_file: &Path, log_file_max_size: ByteSize, log_file_max_rolls: usize,
) -> Result<DebugLogWriter, GenericError> {
    if log_file.to_str().is_none() {
        return Err(generic_error!(
            "dogstatsd_log_file must be valid UTF-8, got '{}'",
            log_file.display()
        ));
    }

    let appender = RollingFileAppenderBase::new(
        log_file,
        RollingConditionBase::new().max_size(log_file_max_size.as_u64()),
        log_file_max_rolls,
    )
    .map_err(|e| generic_error!("Failed to open dogstatsd_log_file '{}': {}", log_file.display(), e))?;

    let (writer, guard) = NonBlockingBuilder::default()
        .thread_name("dogstatsd-debug-log-writer")
        .buffered_lines_limit(DEBUG_LOG_WRITER_BUFFER_LINES)
        // Drop debug log lines rather than slow DogStatsD metric ingestion.
        .lossy(true)
        .finish(appender);

    Ok(DebugLogWriter { writer, _guard: guard })
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

#[cfg(target_os = "macos")]
fn default_dogstatsd_log_file_path() -> PathBuf {
    PathBuf::from("/opt/datadog-agent/logs/dogstatsd_info/dogstatsd-stats.log")
}

#[cfg(target_os = "windows")]
fn default_dogstatsd_log_file_path() -> PathBuf {
    std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"c:\programdata"))
        .join("datadog")
        .join("logs")
        .join("dogstatsd_info")
        .join("dogstatsd-stats.log")
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn default_dogstatsd_log_file_path() -> PathBuf {
    PathBuf::from("/var/log/datadog/dogstatsd_info/dogstatsd-stats.log")
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    use bytesize::ByteSize;
    use saluki_context::Context;
    use saluki_core::data_model::event::metric::Metric;
    use serde_json::json;
    use tempfile::tempdir;

    use super::{DogStatsDDebugLog, DogStatsDDebugLogConfiguration};
    use crate::config_registry::structs;
    use crate::config_registry::test_support::run_config_smoke_tests;

    async fn deser_config(raw_json: &str) -> DogStatsDDebugLogConfiguration {
        let value = serde_json::from_str(raw_json).expect("test config should be valid JSON");
        let (config, _) = saluki_config::ConfigurationLoader::for_tests(Some(value), None, false).await;

        DogStatsDDebugLogConfiguration::from_configuration(&config)
            .expect("DogStatsDDebugLogConfiguration should deserialize")
    }

    fn metrics_stats_enabled(config: &DogStatsDDebugLogConfiguration) -> bool {
        config.metrics_stats_enabled_state.load()
    }

    #[tokio::test]
    async fn defaults_match_core_agent() {
        let config = deser_config("{}").await;

        assert!(config.enabled());
        assert!(!config.metrics_stats_enabled);
        assert!(config.logging_enabled);
        assert_eq!(
            config.log_file(),
            DogStatsDDebugLogConfiguration::default_log_file_path()
        );
        assert_eq!(config.log_file_max_size(), ByteSize::mb(10));
        assert_eq!(config.log_file_max_rolls(), 3);
    }

    #[tokio::test]
    async fn logging_enabled_controls_topology_wiring() {
        let config = deser_config(r#"{ "dogstatsd_metrics_stats_enable": true }"#).await;
        assert!(config.enabled());
        assert!(metrics_stats_enabled(&config));

        let config = deser_config(
            r#"{
                "dogstatsd_metrics_stats_enable": true,
                "dogstatsd_logging_enabled": true
            }"#,
        )
        .await;
        assert!(config.enabled());
        assert!(metrics_stats_enabled(&config));

        let config = deser_config(
            r#"{
                "dogstatsd_metrics_stats_enable": true,
                "dogstatsd_logging_enabled": false
            }"#,
        )
        .await;
        assert!(!config.enabled());
        assert!(metrics_stats_enabled(&config));

        let config = deser_config(
            r#"{
                "dogstatsd_metrics_stats_enable": false,
                "dogstatsd_logging_enabled": true
            }"#,
        )
        .await;
        assert!(config.enabled());
        assert!(!metrics_stats_enabled(&config));
    }

    #[tokio::test]
    async fn explicit_log_file_path_is_preserved() {
        let config = deser_config(r#"{ "dogstatsd_log_file": "/tmp/dsd-debug.log" }"#).await;

        assert_eq!(config.log_file(), std::path::Path::new("/tmp/dsd-debug.log"));
    }

    #[tokio::test]
    async fn negative_log_file_max_rolls_is_rejected() {
        let value = json!({ "dogstatsd_log_file_max_rolls": -1 });
        let (config, _) = saluki_config::ConfigurationLoader::for_tests(Some(value), None, false).await;

        let result = DogStatsDDebugLogConfiguration::from_configuration(&config);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn smoke_test() {
        run_config_smoke_tests(structs::DOGSTATSD_DEBUG_LOG_CONFIGURATION, &[], json!({}), |cfg| {
            DogStatsDDebugLogConfiguration::from_configuration(&cfg)
                .expect("DogStatsDDebugLogConfiguration should deserialize")
        })
        .await
    }

    fn test_config(log_file: PathBuf, max_size: ByteSize, max_rolls: usize) -> DogStatsDDebugLogConfiguration {
        let metrics_stats_enabled = true;
        DogStatsDDebugLogConfiguration {
            metrics_stats_enabled,
            metrics_stats_enabled_state: super::RuntimeBool::new(metrics_stats_enabled),
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

    #[test]
    fn writes_metric_debug_lines_and_updates_count() {
        let tempdir = tempdir().expect("temporary directory should be created");
        let log_file = tempdir.path().join("dogstatsd-stats.log");
        let config = test_config(log_file.clone(), ByteSize::kb(64), 3);
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

    #[tokio::test]
    async fn dynamically_drops_until_metrics_stats_are_enabled() {
        use std::time::Duration;

        use saluki_config::dynamic::ConfigUpdate;
        use tokio::time::sleep;

        let tempdir = tempdir().expect("temporary directory should be created");
        let log_file = tempdir.path().join("dogstatsd-stats.log");
        let (config, sender) = saluki_config::ConfigurationLoader::for_tests(
            Some(json!({
                "dogstatsd_log_file": log_file.display().to_string(),
                "dogstatsd_log_file_max_size": "64kb",
                "dogstatsd_log_file_max_rolls": 3,
                "dogstatsd_logging_enabled": true
            })),
            None,
            true,
        )
        .await;
        let sender = sender.expect("dynamic sender should be present");
        sender
            .send(ConfigUpdate::Snapshot(json!({})))
            .await
            .expect("initial dynamic snapshot should be sent");
        config.ready().await;

        let dsd_config = DogStatsDDebugLogConfiguration::from_configuration(&config)
            .expect("DogStatsDDebugLogConfiguration should deserialize");
        assert!(dsd_config.enabled());
        assert!(!metrics_stats_enabled(&dsd_config));

        let metric = tagged_metric();
        let mut destination =
            DogStatsDDebugLog::from_configuration(&dsd_config).expect("debug log destination should be built");
        destination
            .process_metric(&metric)
            .expect("disabled metric should be dropped cleanly");
        assert!(!log_file.exists());

        sender
            .send(ConfigUpdate::Partial {
                key: "dogstatsd_metrics_stats_enable".to_string(),
                value: json!(true),
            })
            .await
            .expect("dynamic update should be sent");

        tokio::time::timeout(Duration::from_secs(2), async {
            while !metrics_stats_enabled(&dsd_config) {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("metrics stats flag should become enabled");

        destination
            .process_metric(&metric)
            .expect("enabled metric should be written");

        sender
            .send(ConfigUpdate::Partial {
                key: "dogstatsd_metrics_stats_enable".to_string(),
                value: json!(false),
            })
            .await
            .expect("dynamic update should be sent");

        tokio::time::timeout(Duration::from_secs(2), async {
            while metrics_stats_enabled(&dsd_config) {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("metrics stats flag should become disabled");

        destination
            .process_metric(&metric)
            .expect("disabled metric should be dropped cleanly");
        drop(destination);

        let output = read_log_files(&log_file, dsd_config.log_file_max_rolls());
        let lines = output.lines().collect::<Vec<_>>();

        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("Metric Name: custom.metric"));
        assert!(lines[0].contains("Count: 1"));
    }

    #[test]
    fn rotates_log_file_at_configured_size() {
        let tempdir = tempdir().expect("temporary directory should be created");
        let log_file = tempdir.path().join("dogstatsd-stats.log");
        let min_debug_line_len =
            "Metric Name: custom.metric | Tags: {env:prod service:web} | Count: 1 | Last Seen: ".len();
        let config = test_config(log_file.clone(), ByteSize::b(min_debug_line_len as u64), 2);
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

    #[test]
    fn build_error_mentions_log_file_config_key_and_path() {
        let tempdir = tempdir().expect("temporary directory should be created");
        let blocked_parent = tempdir.path().join("not-a-directory");
        fs::write(&blocked_parent, "not a directory").expect("blocking file should be written");
        let log_file = blocked_parent.join("dogstatsd-stats.log");
        let config = test_config(log_file.clone(), ByteSize::kb(64), 3);

        let err = match DogStatsDDebugLog::from_configuration(&config) {
            Ok(_) => panic!("build should fail"),
            Err(err) => err,
        };
        let err = err.to_string();

        assert!(err.contains("dogstatsd_log_file"));
        assert!(err.contains(&log_file.display().to_string()));
    }
}
