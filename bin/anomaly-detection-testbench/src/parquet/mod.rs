//! Parquet scenario loading and normalization.
//!
//! The testbench replays recorded Observer scenarios. Recordings come in two layouts:
//!
//! - **v1**: `observer-metrics-*.parquet` / `observer-logs-*.parquet` with inline tags.
//! - **v2**: a shared `contexts.parquet` plus `metrics-*.parquet` / `logs-*.parquet` rows that
//!   reference contexts by key.
//!
//! Both layouts are decoded into the same normalized observation types. Decoding follows the Go
//! reference testbench so that replayed inputs match: unit conversions, tag normalization, the
//! metric reorder window, the log ordering tolerance, and the metric-over-log merge precedence all
//! mirror `internal/qbranch/anomalydetection-testbench/bench`.
//!
//! Decoding always copies out of the Arrow buffers (strings and byte arrays are cloned), so no
//! reused Parquet page buffer is ever retained by an observation.

mod v1;
mod v2;

#[cfg(test)]
mod fixtures;

use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;
use std::fmt;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{Array, LargeStringArray, StringArray};
use arrow::datatypes::Schema;
use arrow::error::ArrowError;
use arrow::record_batch::RecordBatch;
use parquet::arrow::arrow_reader::{ParquetRecordBatchReader, ParquetRecordBatchReaderBuilder};
use parquet::errors::ParquetError;

/// Bounded metric look-ahead, in seconds.
///
/// GenSim recordings can contain a small amount of metric timestamp disorder from concurrent
/// collection. Instead of retaining and sorting a whole scenario, the streaming loader holds at
/// most this many seconds of metrics before emitting them in timestamp order.
pub const METRIC_REORDER_WINDOW_SECONDS: i64 = 5;

/// Parquet files smaller than this many bytes are not valid recordings and are ignored.
pub const MIN_PARQUET_FILE_SIZE: u64 = 20;

/// Ingestion source recorded for every replayed observation, regardless of any per-row source.
pub const REPLAY_INGESTION_SOURCE: &str = "parquet";

/// Parquet layout selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParquetFormat {
    /// Inline-tag layout: `observer-metrics-*.parquet` / `observer-logs-*.parquet`.
    V1,
    /// Shared-context layout: `contexts.parquet` plus `metrics-*.parquet` / `logs-*.parquet`.
    V2,
}

impl ParquetFormat {
    /// Auto-detects the layout of `dir`: v2 when `contexts.parquet` is present, v1 otherwise.
    pub fn detect(dir: &Path) -> Self {
        if dir.join("contexts.parquet").exists() {
            Self::V2
        } else {
            Self::V1
        }
    }
}

/// Errors raised while discovering, decoding, or ordering scenario data.
#[derive(Debug)]
pub enum LoadError {
    /// Filesystem access failed.
    Io(std::io::Error),
    /// The Parquet reader rejected a file.
    Parquet(ParquetError),
    /// Arrow rejected a file or batch.
    Arrow(ArrowError),
    /// A decoded row violated the loader's structural expectations.
    Decode(String),
    /// Rows were observed out of the order tolerated by the streaming loader.
    Disorder(String),
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "{err}"),
            Self::Parquet(err) => write!(f, "{err}"),
            Self::Arrow(err) => write!(f, "{err}"),
            Self::Decode(message) | Self::Disorder(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for LoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::Parquet(err) => Some(err),
            Self::Arrow(err) => Some(err),
            Self::Decode(_) | Self::Disorder(_) => None,
        }
    }
}

impl From<std::io::Error> for LoadError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<ParquetError> for LoadError {
    fn from(err: ParquetError) -> Self {
        Self::Parquet(err)
    }
}

impl From<ArrowError> for LoadError {
    fn from(err: ArrowError) -> Self {
        Self::Arrow(err)
    }
}

/// Convenience result alias for loader operations.
pub type Result<T> = std::result::Result<T, LoadError>;

/// A normalized metric observation.
#[derive(Debug, Clone, PartialEq)]
pub struct MetricObservation {
    /// Metric name.
    pub name: String,
    /// Metric value; `0.0` when the recorded value was null or unreadable.
    pub value: f64,
    /// Fully normalized tags, with any lifted `host:` tags removed.
    pub tags: Arc<[String]>,
    /// Host lifted from the first `host:` tag, or empty when none was present.
    pub host: String,
    /// Unix timestamp in whole seconds.
    pub timestamp_sec: i64,
    /// Recorded source (the v1 `RunID` column or the v2 `source` column).
    pub source: String,
}

/// A normalized log observation.
#[derive(Debug, Clone, PartialEq)]
pub struct LogObservation {
    /// Raw message bytes.
    pub message: Vec<u8>,
    /// Recorded status, when present.
    pub status: String,
    /// Normalized tags.
    pub tags: Arc<[String]>,
    /// Hostname, taken from the recorded `hostname` column (v1) or the last `host:` tag (v2).
    pub hostname: String,
    /// Unix timestamp in milliseconds.
    pub timestamp_ms: i64,
}

/// A single item of the globally ordered replay stream.
#[derive(Debug, Clone, PartialEq)]
pub enum Observation {
    /// A metric sample.
    Metric(MetricObservation),
    /// A log entry.
    Log(LogObservation),
}

impl Observation {
    /// The observation timestamp truncated to whole seconds.
    pub fn timestamp_sec(&self) -> i64 {
        match self {
            Self::Metric(metric) => metric.timestamp_sec,
            Self::Log(log) => log.timestamp_ms / 1000,
        }
    }
}

/// Replay input-selection options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadOptions {
    /// Skip metric rows recorded as dropped (default `true`, matching the Go `--skip-dropped`).
    pub skip_dropped_metrics: bool,
    /// Read only logs and ignore metric files entirely.
    pub logs_only: bool,
}

impl Default for LoadOptions {
    fn default() -> Self {
        Self {
            skip_dropped_metrics: true,
            logs_only: false,
        }
    }
}

/// Extracts the host from the first `host:` tag and removes all `host:` tags.
///
/// When the first `host:` tag has a nonempty value, that value becomes the host and every `host:`
/// tag is removed from the returned tags. When the value is empty (a bare `host:` tag) the original
/// tags are preserved unchanged, matching the Go reference edge case.
pub fn resolve_metric_host_and_tags(tags: &[String]) -> (String, Vec<String>) {
    match first_host(tags) {
        Some(host) if !host.is_empty() => {
            let filtered = tags.iter().filter(|tag| !tag.starts_with("host:")).cloned().collect();
            (host.to_string(), filtered)
        }
        _ => (String::new(), tags.to_vec()),
    }
}

fn first_host(tags: &[String]) -> Option<&str> {
    tags.iter().find_map(|tag| tag.strip_prefix("host:"))
}

/// Finds a column by name, normalizing case and underscores.
pub(crate) fn find_col(schema: &Schema, name: &str) -> Option<usize> {
    let target = normalize(name);
    schema
        .fields()
        .iter()
        .position(|field| normalize(field.name()) == target)
}

fn normalize(name: &str) -> String {
    name.chars()
        .filter(|character| *character != '_')
        .flat_map(char::to_lowercase)
        .collect()
}

/// Reads a UTF-8 string value at `index`, returning `None` for nulls and unsupported types.
pub(crate) fn string_value(values: &dyn Array, index: usize) -> Option<String> {
    if values.is_null(index) {
        return None;
    }
    if let Some(array) = values.as_any().downcast_ref::<StringArray>() {
        return Some(array.value(index).to_string());
    }
    if let Some(array) = values.as_any().downcast_ref::<LargeStringArray>() {
        return Some(array.value(index).to_string());
    }
    None
}

fn resolve_metric_host_and_tags_arc(tags: Arc<[String]>) -> (String, Arc<[String]>) {
    match first_host(&tags) {
        Some(host) if !host.is_empty() => {
            let filtered: Vec<String> = tags.iter().filter(|tag| !tag.starts_with("host:")).cloned().collect();
            (host.to_string(), Arc::from(filtered))
        }
        _ => (String::new(), tags),
    }
}

/// A decoded metric row before input selection and host resolution.
#[derive(Debug, Clone)]
struct DecodedMetric {
    name: String,
    value: f64,
    tags: Arc<[String]>,
    timestamp_sec: i64,
    source: String,
    dropped: bool,
}

impl DecodedMetric {
    fn into_observation(self) -> MetricObservation {
        let (host, tags) = resolve_metric_host_and_tags_arc(self.tags);
        MetricObservation {
            name: self.name,
            value: self.value,
            tags,
            host,
            timestamp_sec: self.timestamp_sec,
            source: self.source,
        }
    }
}

impl From<v1::FgmMetric> for DecodedMetric {
    fn from(metric: v1::FgmMetric) -> Self {
        Self {
            name: metric.name,
            value: metric.value,
            tags: Arc::from(metric.tags),
            timestamp_sec: metric.time_ms / 1000,
            source: metric.source,
            dropped: metric.dropped,
        }
    }
}

impl From<v2::V2Metric> for DecodedMetric {
    fn from(metric: v2::V2Metric) -> Self {
        Self {
            name: metric.name,
            value: metric.value,
            tags: metric.tags,
            timestamp_sec: metric.timestamp_sec,
            source: metric.source,
            dropped: false,
        }
    }
}

fn metric_selected(metric: &DecodedMetric, options: &LoadOptions) -> bool {
    !(metric.name.starts_with("datadog.") || (options.skip_dropped_metrics && metric.dropped))
}

// ---- File/batch plumbing ----

fn open_reader(path: &Path) -> Result<ParquetRecordBatchReader> {
    let metadata = std::fs::metadata(path)?;
    if metadata.len() < MIN_PARQUET_FILE_SIZE {
        return Err(LoadError::Decode(format!(
            "{} is too small to be parquet ({} bytes)",
            path.display(),
            metadata.len()
        )));
    }
    let file = File::open(path)?;
    Ok(ParquetRecordBatchReaderBuilder::try_new(file)?
        .with_batch_size(1024)
        .build()?)
}

fn collect_file<T>(path: &Path, decode: &mut dyn FnMut(&RecordBatch) -> Result<Vec<T>>) -> Result<Vec<T>> {
    let mut reader = open_reader(path)?;
    let mut rows = Vec::new();
    for batch in &mut reader {
        rows.extend(decode(&batch?)?);
    }
    Ok(rows)
}

/// Decodes one Arrow batch of an input file into owned rows.
type BatchDecoder<T> = Box<dyn FnMut(&RecordBatch) -> Result<Vec<T>>>;

/// Lazily yields decoded rows across a sequence of Parquet files, one Arrow batch at a time.
struct ParquetRows<T> {
    files: std::vec::IntoIter<PathBuf>,
    reader: Option<ParquetRecordBatchReader>,
    batch: Option<RecordBatch>,
    queued: std::vec::IntoIter<T>,
    current_name: String,
    decode: BatchDecoder<T>,
}

impl<T> ParquetRows<T> {
    fn new(files: Vec<PathBuf>, decode: impl FnMut(&RecordBatch) -> Result<Vec<T>> + 'static) -> Self {
        Self {
            files: files.into_iter(),
            reader: None,
            batch: None,
            queued: Vec::new().into_iter(),
            current_name: String::new(),
            decode: Box::new(decode),
        }
    }

    /// Basename of the file the most recently yielded row came from.
    fn file_name(&self) -> &str {
        &self.current_name
    }

    fn next_row(&mut self) -> Result<Option<T>> {
        loop {
            if let Some(row) = self.queued.next() {
                return Ok(Some(row));
            }
            if let Some(batch) = self.batch.take() {
                self.queued = (self.decode)(&batch)?.into_iter();
                continue;
            }
            if self.reader.is_none() {
                match self.files.next() {
                    Some(path) => {
                        self.current_name = path
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        self.reader = Some(open_reader(&path)?);
                    }
                    None => return Ok(None),
                }
            }

            match self.reader.as_mut().expect("reader initialized").next() {
                Some(Ok(batch)) => self.batch = Some(batch),
                Some(Err(err)) => return Err(err.into()),
                None => self.reader = None,
            }
        }
    }
}

// ---- Streaming cursors ----

struct QueuedMetric {
    timestamp_sec: i64,
    sequence: u64,
    metric: DecodedMetric,
}

impl Ord for QueuedMetric {
    fn cmp(&self, other: &Self) -> Ordering {
        self.timestamp_sec
            .cmp(&other.timestamp_sec)
            .then_with(|| self.sequence.cmp(&other.sequence))
    }
}

impl PartialOrd for QueuedMetric {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for QueuedMetric {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for QueuedMetric {}

/// Emits metrics in timestamp order using the bounded reorder window.
struct MetricCursor {
    rows: ParquetRows<DecodedMetric>,
    pending: BinaryHeap<Reverse<QueuedMetric>>,
    sequence: u64,
    max_seen: Option<i64>,
    rows_read: usize,
}

impl MetricCursor {
    fn new(rows: ParquetRows<DecodedMetric>) -> Self {
        Self {
            rows,
            pending: BinaryHeap::new(),
            sequence: 0,
            max_seen: None,
            rows_read: 0,
        }
    }

    fn next_row(&mut self) -> Result<Option<DecodedMetric>> {
        loop {
            if let Some(watermark) = self.max_seen.map(|max| max - METRIC_REORDER_WINDOW_SECONDS) {
                if let Some(Reverse(top)) = self.pending.peek() {
                    if top.timestamp_sec <= watermark {
                        return Ok(Some(self.pending.pop().expect("peeked").0.metric));
                    }
                }
            }

            match self.rows.next_row()? {
                Some(metric) => {
                    let timestamp_sec = metric.timestamp_sec;
                    if let Some(max_seen) = self.max_seen {
                        if timestamp_sec < max_seen - METRIC_REORDER_WINDOW_SECONDS {
                            return Err(LoadError::Disorder(format!(
                                "metric timestamp disorder exceeds {METRIC_REORDER_WINDOW_SECONDS}s: {} contains {} after {}",
                                self.rows.file_name(),
                                timestamp_sec,
                                max_seen
                            )));
                        }
                    }
                    if self.max_seen.is_none_or(|max| timestamp_sec > max) {
                        self.max_seen = Some(timestamp_sec);
                    }
                    self.pending.push(Reverse(QueuedMetric {
                        timestamp_sec,
                        sequence: self.sequence,
                        metric,
                    }));
                    self.sequence += 1;
                    self.rows_read += 1;
                }
                None => {
                    return Ok(self.pending.pop().map(|entry| entry.0.metric));
                }
            }
        }
    }
}

/// Emits logs, enforcing ordering at the Observer's one-second scheduling resolution.
struct LogCursor {
    rows: ParquetRows<LogObservation>,
    previous_ms: Option<i64>,
    rows_read: usize,
}

impl LogCursor {
    fn new(rows: ParquetRows<LogObservation>) -> Self {
        Self {
            rows,
            previous_ms: None,
            rows_read: 0,
        }
    }

    fn next_row(&mut self) -> Result<Option<LogObservation>> {
        let Some(entry) = self.rows.next_row()? else {
            return Ok(None);
        };
        if let Some(previous) = self.previous_ms {
            if entry.timestamp_ms / 1000 < previous / 1000 {
                return Err(LoadError::Disorder(format!(
                    "log timestamps are not globally ordered: {} contains {} after {}",
                    self.rows.file_name(),
                    entry.timestamp_ms,
                    previous
                )));
            }
        }
        self.previous_ms = Some(entry.timestamp_ms);
        self.rows_read += 1;
        Ok(Some(entry))
    }
}

// ---- Public entry points ----

/// A lazily ordered merge of the metric and log streams of a scenario.
pub struct ObservationStream {
    metric: Option<MetricCursor>,
    log: LogCursor,
    options: LoadOptions,
    metric_head: Option<DecodedMetric>,
    log_head: Option<LogObservation>,
    metric_done: bool,
    log_done: bool,
    finished: bool,
    error: Option<LoadError>,
}

impl ObservationStream {
    /// Number of metric rows decoded from the scenario so far, before input selection.
    pub fn metric_rows_read(&self) -> usize {
        self.metric.as_ref().map_or(0, |cursor| cursor.rows_read)
    }

    /// Number of log rows decoded from the scenario so far.
    pub fn log_rows_read(&self) -> usize {
        self.log.rows_read
    }
}

impl Iterator for ObservationStream {
    type Item = Result<Observation>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(err) = self.error.take() {
            self.finished = true;
            return Some(Err(err));
        }
        if self.finished {
            return None;
        }

        loop {
            if self.metric_head.is_none() && !self.metric_done {
                match self.metric.as_mut() {
                    Some(cursor) => match cursor.next_row() {
                        Ok(Some(metric)) => self.metric_head = Some(metric),
                        Ok(None) => self.metric_done = true,
                        Err(err) => {
                            self.error = Some(err);
                            return self.next();
                        }
                    },
                    None => self.metric_done = true,
                }
            }
            if self.log_head.is_none() && !self.log_done {
                match self.log.next_row() {
                    Ok(Some(log)) => self.log_head = Some(log),
                    Ok(None) => self.log_done = true,
                    Err(err) => {
                        self.error = Some(err);
                        return self.next();
                    }
                }
            }

            if self.metric_head.is_none() && self.log_head.is_none() {
                if self.metric_done && self.log_done {
                    self.finished = true;
                    return None;
                }
                continue;
            }

            let take_metric = match (&self.metric_head, &self.log_head) {
                (Some(metric), Some(log)) => metric.timestamp_sec <= log.timestamp_ms / 1000,
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (None, None) => unreachable!("at least one head is present"),
            };

            if take_metric {
                let metric = self.metric_head.take().expect("metric head present");
                if !metric_selected(&metric, &self.options) {
                    continue;
                }
                return Some(Ok(Observation::Metric(metric.into_observation())));
            }

            let log = self.log_head.take().expect("log head present");
            return Some(Ok(Observation::Log(log)));
        }
    }
}

/// Opens a lazily ordered observation stream for a scenario data directory.
///
/// `format` should normally come from [`ParquetFormat::detect`]. In v2 the shared contexts file is
/// loaded once and its failures fail the whole load; per-row decode errors propagate as stream
/// errors.
pub fn open_stream(dir: &Path, format: ParquetFormat, options: &LoadOptions) -> Result<ObservationStream> {
    let contexts = load_contexts(dir, format)?;
    let metric = if options.logs_only {
        None
    } else {
        Some(MetricCursor::new(metric_rows(dir, format, contexts.clone())?))
    };
    let log = LogCursor::new(log_rows(dir, format, contexts)?);

    Ok(ObservationStream {
        metric,
        log,
        options: options.clone(),
        metric_head: None,
        log_head: None,
        metric_done: options.logs_only,
        log_done: false,
        finished: false,
        error: None,
    })
}

/// A fully retained, sorted scenario load.
pub struct LoadedScenario {
    /// Metrics sorted by recorded timestamp, after input selection and host resolution.
    pub metrics: Vec<MetricObservation>,
    /// Logs sorted by timestamp.
    pub logs: Vec<LogObservation>,
    /// Files that were skipped because they could not be decoded.
    pub skipped_files: Vec<PathBuf>,
}

/// Loads and normalizes a whole scenario, retaining every observation.
///
/// Metrics are stably sorted by their recorded timestamp key (raw milliseconds for v1, seconds for
/// v2) before input selection, matching the Go retained reader's sub-second ordering. Files that
/// fail to decode are skipped and recorded in [`LoadedScenario::skipped_files`]; a broken v2
/// `contexts.parquet` fails the whole load.
pub fn load_all(dir: &Path, format: ParquetFormat, options: &LoadOptions) -> Result<LoadedScenario> {
    let contexts = load_contexts(dir, format)?;
    let mut skipped_files = Vec::new();

    let mut metrics: Vec<(i64, DecodedMetric)> = Vec::new();
    if !options.logs_only {
        match format {
            ParquetFormat::V1 => {
                for path in v1::find_metric_files(dir)? {
                    match collect_file(&path, &mut |batch| v1::decode_metric_batch(batch)) {
                        Ok(rows) => metrics.extend(rows.into_iter().map(|m| (m.time_ms, DecodedMetric::from(m)))),
                        Err(_) => skipped_files.push(path),
                    }
                }
            }
            ParquetFormat::V2 => {
                let contexts = contexts.as_ref().expect("v2 contexts loaded");
                for path in v2::find_metric_files(dir)? {
                    match collect_file(&path, &mut |batch| v2::decode_metric_batch(batch, contexts)) {
                        Ok(rows) => metrics.extend(rows.into_iter().map(|m| (m.timestamp_sec, DecodedMetric::from(m)))),
                        Err(_) => skipped_files.push(path),
                    }
                }
            }
        }
    }
    // Stable sort keeps sub-second row order within a second.
    metrics.sort_by_key(|(key, _)| *key);
    let metrics = metrics
        .into_iter()
        .map(|(_, metric)| metric)
        .filter(|metric| metric_selected(metric, options))
        .map(DecodedMetric::into_observation)
        .collect();

    let mut logs: Vec<LogObservation> = Vec::new();
    match format {
        ParquetFormat::V1 => {
            for path in v1::find_log_files(dir)? {
                match collect_file(&path, &mut |batch| v1::decode_log_batch(batch)) {
                    Ok(rows) => logs.extend(rows),
                    Err(_) => skipped_files.push(path),
                }
            }
        }
        ParquetFormat::V2 => {
            let contexts = contexts.as_ref().expect("v2 contexts loaded");
            for path in v2::find_log_files(dir)? {
                match collect_file(&path, &mut |batch| v2::decode_log_batch(batch, contexts)) {
                    Ok(rows) => logs.extend(rows),
                    Err(_) => skipped_files.push(path),
                }
            }
        }
    }
    logs.sort_by_key(|log| log.timestamp_ms);

    Ok(LoadedScenario {
        metrics,
        logs,
        skipped_files,
    })
}

fn load_contexts(dir: &Path, format: ParquetFormat) -> Result<Option<Arc<v2::Contexts>>> {
    match format {
        ParquetFormat::V1 => Ok(None),
        ParquetFormat::V2 => {
            let contexts = v2::read_contexts(&dir.join("contexts.parquet"))
                .map_err(|err| LoadError::Decode(format!("reading contexts: {err}")))?;
            Ok(Some(Arc::new(contexts)))
        }
    }
}

fn metric_rows(
    dir: &Path, format: ParquetFormat, contexts: Option<Arc<v2::Contexts>>,
) -> Result<ParquetRows<DecodedMetric>> {
    match format {
        ParquetFormat::V1 => {
            let files = v1::find_metric_files(dir)?;
            Ok(ParquetRows::new(files, |batch| {
                Ok(v1::decode_metric_batch(batch)?
                    .into_iter()
                    .map(DecodedMetric::from)
                    .collect())
            }))
        }
        ParquetFormat::V2 => {
            let contexts = contexts.expect("v2 contexts loaded");
            let files = v2::find_metric_files(dir)?;
            Ok(ParquetRows::new(files, move |batch| {
                Ok(v2::decode_metric_batch(batch, &contexts)?
                    .into_iter()
                    .map(DecodedMetric::from)
                    .collect())
            }))
        }
    }
}

fn log_rows(
    dir: &Path, format: ParquetFormat, contexts: Option<Arc<v2::Contexts>>,
) -> Result<ParquetRows<LogObservation>> {
    match format {
        ParquetFormat::V1 => {
            let files = v1::find_log_files(dir)?;
            Ok(ParquetRows::new(files, v1::decode_log_batch))
        }
        ParquetFormat::V2 => {
            let contexts = contexts.expect("v2 contexts loaded");
            let files = v2::find_log_files(dir)?;
            Ok(ParquetRows::new(files, move |batch| {
                v2::decode_log_batch(batch, &contexts)
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parquet::fixtures::*;

    fn collect_stream(dir: &Path, format: ParquetFormat, options: &LoadOptions) -> (Vec<Observation>, Result<()>) {
        let mut stream = open_stream(dir, format, options).unwrap();
        let mut observations = Vec::new();
        let result = loop {
            match stream.next() {
                Some(Ok(observation)) => observations.push(observation),
                Some(Err(err)) => break Err(err),
                None => break Ok(()),
            }
        };
        (observations, result)
    }

    #[test]
    fn v2_is_detected_by_contexts_file() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        write_contexts_v2(dir, &[context(1, "system.cpu", Some("host-a"), None)], false);
        write_v2_metrics(
            dir,
            "metrics-0.parquet",
            &[v2_metric(1, 1.5, Some(10_000_000_000), Some("check"))],
        );

        assert_eq!(ParquetFormat::detect(dir), ParquetFormat::V2);

        let scenario = load_all(dir, ParquetFormat::detect(dir), &LoadOptions::default()).unwrap();
        assert_eq!(scenario.metrics.len(), 1);
        assert_eq!(scenario.metrics[0].name, "system.cpu");
        assert!(scenario.logs.is_empty());
    }

    #[test]
    fn v2_unit_conversions_use_seconds_for_metrics_and_millis_for_logs() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        write_contexts_v2(
            dir,
            &[
                context(1, "system.cpu", Some("host-a"), None),
                context(2, "app", Some("host-b"), None),
            ],
            false,
        );
        write_v2_metrics(
            dir,
            "metrics-0.parquet",
            &[v2_metric(1, 1.5, Some(10_000_000_000), Some("check"))],
        );
        write_v2_logs(
            dir,
            "logs-0.parquet",
            &[v2_log(2, Some(b"hello".to_vec()), Some(12_345_000_000))],
        );

        let scenario = load_all(dir, ParquetFormat::V2, &LoadOptions::default()).unwrap();
        assert_eq!(scenario.metrics[0].timestamp_sec, 10);
        assert_eq!(scenario.logs[0].timestamp_ms, 12_345);
    }

    #[test]
    fn datadog_names_are_skipped_in_both_paths() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        write_v1_metrics(
            dir,
            "observer-metrics-0.parquet",
            &[
                v1_metric("run", 1_000, "system.cpu", Some(1.0)),
                v1_metric("run", 1_000, "datadog.agent.running", Some(1.0)),
            ],
        );

        let scenario = load_all(dir, ParquetFormat::V1, &LoadOptions::default()).unwrap();
        assert_eq!(scenario.metrics.len(), 1);
        assert_eq!(scenario.metrics[0].name, "system.cpu");

        let (observations, result) = collect_stream(dir, ParquetFormat::V1, &LoadOptions::default());
        result.unwrap();
        assert_eq!(observations.len(), 1);
    }

    #[test]
    fn dropped_rows_are_skipped_by_default_and_kept_when_requested() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        let mut dropped = v1_metric("run", 1_000, "system.cpu", Some(1.0));
        dropped.dropped = true;
        write_v1_metrics(
            dir,
            "observer-metrics-0.parquet",
            &[dropped, v1_metric("run", 2_000, "system.mem", Some(2.0))],
        );

        let scenario = load_all(dir, ParquetFormat::V1, &LoadOptions::default()).unwrap();
        assert_eq!(scenario.metrics.len(), 1);
        assert_eq!(scenario.metrics[0].name, "system.mem");

        let keep = LoadOptions {
            skip_dropped_metrics: false,
            logs_only: false,
        };
        let scenario = load_all(dir, ParquetFormat::V1, &keep).unwrap();
        assert_eq!(scenario.metrics.len(), 2);
    }

    #[test]
    fn host_tag_is_lifted_and_removed_from_tags() {
        let (host, tags) =
            resolve_metric_host_and_tags(&["env:prod".into(), "host:web-1".into(), "service:api".into()]);
        assert_eq!(host, "web-1");
        assert_eq!(tags, vec!["env:prod".to_string(), "service:api".to_string()]);

        let (host, tags) = resolve_metric_host_and_tags(&["host:web-1".into(), "host:web-2".into()]);
        assert_eq!(host, "web-1", "first host tag wins");
        assert!(tags.is_empty(), "all host tags are removed");
    }

    #[test]
    fn empty_host_value_preserves_original_tags() {
        let (host, tags) = resolve_metric_host_and_tags(&["host:".into(), "env:prod".into()]);
        assert_eq!(host, "");
        assert_eq!(tags, vec!["host:".to_string(), "env:prod".to_string()]);
    }

    #[test]
    fn v1_host_tag_is_lifted_end_to_end_and_bare_host_is_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        let mut lifted = v1_metric("run", 1_000, "system.cpu", Some(1.0));
        lifted.tags = tag_list(&["host:web-1", "env:prod"]);
        let mut bare = v1_metric("run", 2_000, "system.mem", Some(2.0));
        bare.tags = tag_list(&["host:"]);
        write_v1_metrics(dir, "observer-metrics-0.parquet", &[lifted, bare]);

        let scenario = load_all(dir, ParquetFormat::V1, &LoadOptions::default()).unwrap();
        assert_eq!(scenario.metrics[0].host, "web-1");
        assert_eq!(scenario.metrics[0].tags.as_ref(), ["env:prod"]);
        assert_eq!(scenario.metrics[1].host, "", "empty host value does not lift");
        assert_eq!(
            scenario.metrics[1].tags.as_ref(),
            ["host"],
            "bare tag survives, colon dropped"
        );
    }

    #[test]
    fn signed_and_unsigned_context_keys_resolve_identically() {
        for unsigned in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let dir = dir.path();
            write_contexts_v2(
                dir,
                &[
                    context(1, "a", Some("host-a"), None),
                    context(u64::MAX, "wrapped", Some("host-z"), None),
                ],
                unsigned,
            );
            write_v2_metrics(
                dir,
                "metrics-0.parquet",
                &[
                    v2_metric(1, 1.0, Some(1_000_000_000), None),
                    v2_metric(-1, 2.0, Some(2_000_000_000), None),
                ],
            );

            let scenario = load_all(dir, ParquetFormat::V2, &LoadOptions::default()).unwrap();
            assert_eq!(scenario.metrics.len(), 2, "unsigned = {unsigned}");
            assert_eq!(scenario.metrics[0].name, "a");
            assert_eq!(scenario.metrics[1].name, "wrapped", "i64 -1 maps to u64::MAX");
        }
    }

    #[test]
    fn unknown_context_rows_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        write_contexts_v2(dir, &[context(1, "known", None, None)], false);
        write_v2_metrics(
            dir,
            "metrics-0.parquet",
            &[
                v2_metric(1, 1.0, Some(1_000_000_000), None),
                v2_metric(99, 2.0, Some(2_000_000_000), None),
            ],
        );

        let scenario = load_all(dir, ParquetFormat::V2, &LoadOptions::default()).unwrap();
        assert_eq!(scenario.metrics.len(), 1);
        assert_eq!(scenario.metrics[0].name, "known");
    }

    #[test]
    fn metric_reorder_window_orders_within_bounds() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        write_v1_metrics(
            dir,
            "observer-metrics-0.parquet",
            &[v1_metric("run", 1_001_000, "a", Some(1.0))],
        );
        write_v1_metrics(
            dir,
            "observer-metrics-1.parquet",
            &[
                v1_metric("run", 1_000_000, "a", Some(2.0)),
                v1_metric("run", 1_007_000, "a", Some(3.0)),
            ],
        );

        let (observations, result) = collect_stream(dir, ParquetFormat::V1, &LoadOptions::default());
        result.unwrap();
        let stamps: Vec<i64> = observations.iter().map(Observation::timestamp_sec).collect();
        assert_eq!(stamps, [1000, 1001, 1007]);
    }

    #[test]
    fn metric_reorder_window_rejects_disorder_beyond_five_seconds() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        write_v1_metrics(
            dir,
            "observer-metrics-0.parquet",
            &[
                v1_metric("run", 1_006_000, "a", Some(1.0)),
                v1_metric("run", 1_000_000, "a", Some(2.0)),
            ],
        );

        let mut stream = open_stream(dir, ParquetFormat::V1, &LoadOptions::default()).unwrap();
        let err = stream.find_map(|item| item.err()).expect("disorder error");
        let message = err.to_string();
        assert!(message.contains("metric timestamp disorder exceeds 5s"), "{message}");
        assert!(message.contains("observer-metrics-0.parquet"), "{message}");
    }

    #[test]
    fn equal_timestamp_metrics_preserve_sequence_order() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        // `first` and `second` share a timestamp but arrive from different files, so only the
        // recorded sequence (not file or heap order) can decide their relative order.
        write_v1_metrics(
            dir,
            "observer-metrics-0.parquet",
            &[v1_metric("run", 1_000_000, "first", Some(1.0))],
        );
        write_v1_metrics(
            dir,
            "observer-metrics-1.parquet",
            &[
                v1_metric("run", 1_000_000, "second", Some(2.0)),
                v1_metric("run", 1_008_000, "later", Some(9.0)),
            ],
        );

        let (observations, result) = collect_stream(dir, ParquetFormat::V1, &LoadOptions::default());
        result.unwrap();
        let names: Vec<&str> = observations
            .iter()
            .filter_map(|observation| match observation {
                Observation::Metric(metric) => Some(metric.name.as_str()),
                Observation::Log(_) => None,
            })
            .collect();
        assert_eq!(
            names,
            ["first", "second", "later"],
            "equal timestamps keep sequence order"
        );
    }

    #[test]
    fn logs_allow_sub_second_disorder_but_reject_cross_second_disorder() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        write_v1_logs(
            dir,
            "observer-logs-0.parquet",
            &[v1_log(1_999), v1_log(1_000), v1_log(1_500), v1_log(2_000)],
        );

        let (observations, result) = collect_stream(dir, ParquetFormat::V1, &LoadOptions::default());
        result.unwrap();
        assert_eq!(observations.len(), 4);

        let bad = tempfile::tempdir().unwrap();
        let bad = bad.path();
        write_v1_logs(bad, "observer-logs-0.parquet", &[v1_log(2_001), v1_log(1_000)]);
        let mut stream = open_stream(bad, ParquetFormat::V1, &LoadOptions::default()).unwrap();
        let err = stream.find_map(|item| item.err()).expect("log disorder error");
        let message = err.to_string();
        assert!(message.contains("log timestamps are not globally ordered"), "{message}");
        assert!(
            message.contains("observer-logs-0.parquet contains 1000 after 2001"),
            "{message}"
        );
    }

    #[test]
    fn metrics_take_precedence_over_logs_at_equal_seconds() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        write_v1_metrics(
            dir,
            "observer-metrics-0.parquet",
            &[
                v1_metric("run", 10_000, "cpu", Some(1.0)),
                v1_metric("run", 12_000, "cpu", Some(2.0)),
            ],
        );
        write_v1_logs(
            dir,
            "observer-logs-0.parquet",
            &[v1_log(11_500), v1_log(12_500), v1_log(13_000)],
        );

        let (observations, result) = collect_stream(dir, ParquetFormat::V1, &LoadOptions::default());
        result.unwrap();
        let kinds: Vec<&str> = observations
            .iter()
            .map(|observation| match observation {
                Observation::Metric(_) => "metric",
                Observation::Log(_) => "log",
            })
            .collect();
        let stamps: Vec<i64> = observations.iter().map(Observation::timestamp_sec).collect();
        assert_eq!(stamps, [10, 11, 12, 12, 13]);
        assert_eq!(kinds, ["metric", "log", "metric", "log", "log"]);
    }

    #[test]
    fn tiny_metric_files_are_filtered_out() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        write_v1_metrics(
            dir,
            "observer-metrics-0.parquet",
            &[v1_metric("run", 1_000, "system.cpu", Some(1.0))],
        );
        std::fs::write(dir.join("observer-metrics-1.parquet"), b"tiny").unwrap();

        let (observations, result) = collect_stream(dir, ParquetFormat::V1, &LoadOptions::default());
        result.unwrap();
        assert_eq!(observations.len(), 1);

        let scenario = load_all(dir, ParquetFormat::V1, &LoadOptions::default()).unwrap();
        assert_eq!(scenario.metrics.len(), 1);
    }

    #[test]
    fn logs_only_ignores_metrics() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        write_v1_metrics(
            dir,
            "observer-metrics-0.parquet",
            &[v1_metric("run", 1_000, "system.cpu", Some(1.0))],
        );
        write_v1_logs(dir, "observer-logs-0.parquet", &[v1_log(11_000)]);

        let options = LoadOptions {
            skip_dropped_metrics: true,
            logs_only: true,
        };
        let (observations, result) = collect_stream(dir, ParquetFormat::V1, &options);
        result.unwrap();
        assert_eq!(observations.len(), 1);
        assert!(matches!(observations[0], Observation::Log(_)));

        let scenario = load_all(dir, ParquetFormat::V1, &options).unwrap();
        assert!(scenario.metrics.is_empty());
        assert_eq!(scenario.logs.len(), 1);
    }

    #[test]
    fn streaming_and_retained_paths_agree_on_v1_and_v2() {
        for v2 in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let dir = dir.path();
            if v2 {
                write_contexts_v2(
                    dir,
                    &[
                        context(1, "system.cpu", Some("host-a"), None),
                        context(2, "app", Some("host-b"), None),
                    ],
                    false,
                );
                write_v2_metrics(
                    dir,
                    "metrics-0.parquet",
                    &[
                        v2_metric(1, 1.0, Some(10_000_000_000), Some("check")),
                        v2_metric(1, 2.0, Some(11_000_000_000), Some("check")),
                    ],
                );
                write_v2_logs(
                    dir,
                    "logs-0.parquet",
                    &[v2_log(2, Some(b"x".to_vec()), Some(10_500_000_000))],
                );
            } else {
                write_v1_metrics(
                    dir,
                    "observer-metrics-0.parquet",
                    &[
                        v1_metric("run", 10_000, "system.cpu", Some(1.0)),
                        v1_metric("run", 11_000, "system.cpu", Some(2.0)),
                    ],
                );
                write_v1_logs(dir, "observer-logs-0.parquet", &[v1_log(10_500)]);
            }

            let format = ParquetFormat::detect(dir);
            let scenario = load_all(dir, format, &LoadOptions::default()).unwrap();
            let (observations, result) = collect_stream(dir, format, &LoadOptions::default());
            result.unwrap();

            let expected: Vec<Observation> = scenario
                .metrics
                .iter()
                .cloned()
                .map(Observation::Metric)
                .chain(scenario.logs.iter().cloned().map(Observation::Log))
                .collect();
            let ordered = {
                let mut merged = expected;
                merged.sort_by_key(Observation::timestamp_sec); // stable; metrics precede equal-second logs
                merged
            };
            assert_eq!(observations, ordered, "v2 = {v2}");
        }
    }

    #[test]
    fn string_columns_are_dictionary_encoded_and_nulls_default() {
        use std::fs::File;

        use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
        use parquet::basic::Encoding;

        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        let rows: Vec<V1MetricFixt> = (0..16)
            .map(|_| {
                let mut row = v1_metric("run", 1_000_000, "system.cpu", None);
                row.tags = tag_list(&["env:prod"]);
                row
            })
            .collect();
        write_v1_metrics_rg(dir, "observer-metrics-0.parquet", &rows, 1024);

        let file = File::open(dir.join("observer-metrics-0.parquet")).unwrap();
        let builder = ParquetRecordBatchReaderBuilder::try_new(file).unwrap();
        let metadata = builder.metadata().clone();
        assert_eq!(metadata.num_row_groups(), 1);
        let dictionary_encoded = metadata
            .row_group(0)
            .columns()
            .iter()
            .any(|column| column.encodings().any(|encoding| encoding == Encoding::RLE_DICTIONARY));
        assert!(dictionary_encoded, "string columns should be dictionary encoded");

        let scenario = load_all(dir, ParquetFormat::V1, &LoadOptions::default()).unwrap();
        assert_eq!(scenario.metrics.len(), 16);
        assert_eq!(scenario.metrics[0].value, 0.0, "null value defaults to 0");
        assert_eq!(scenario.metrics[0].tags.as_ref(), ["env:prod"]);
    }

    #[test]
    fn multiple_row_groups_decode_in_row_order() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        let rows: Vec<V1MetricFixt> = (0..6)
            .map(|i| v1_metric("run", 1_000_000 + i * 1_000, "system.cpu", Some(i as f64)))
            .collect();
        write_v1_metrics_rg(dir, "observer-metrics-0.parquet", &rows, 2);

        let file = std::fs::File::open(dir.join("observer-metrics-0.parquet")).unwrap();
        let builder = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(file).unwrap();
        assert!(
            builder.metadata().num_row_groups() > 1,
            "fixture spans multiple row groups"
        );

        let scenario = load_all(dir, ParquetFormat::V1, &LoadOptions::default()).unwrap();
        let values: Vec<f64> = scenario.metrics.iter().map(|metric| metric.value).collect();
        assert_eq!(values, (0..6).map(|i| i as f64).collect::<Vec<_>>());
    }

    #[test]
    fn v2_context_tags_follow_go_normalization() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        let mut ctx = context(1, "system.cpu", Some("host-a"), Some("check"));
        ctx.tags = vec![
            ("region".to_string(), Some("us-east".to_string())),
            ("bare".to_string(), None),
            ("empty".to_string(), Some(String::new())),
        ];
        write_contexts_v2(dir, &[ctx], false);
        write_v2_metrics(
            dir,
            "metrics-0.parquet",
            &[v2_metric(1, 1.0, Some(1_000_000_000), Some("check"))],
        );

        let scenario = load_all(dir, ParquetFormat::V2, &LoadOptions::default()).unwrap();
        assert_eq!(scenario.metrics[0].host, "host-a");
        assert_eq!(
            scenario.metrics[0].tags.as_ref(),
            ["source:check", "region:us-east", "bare", "empty"],
            "fixed tag columns first, then map entries; null/empty values become bare keys"
        );
    }

    #[test]
    fn v2_log_uses_last_host_tag_and_keeps_source_tag() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        let mut ctx = context(2, "app", Some("host-a"), Some("check"));
        ctx.tags = vec![("host".to_string(), Some("host-b".to_string()))];
        write_contexts_v2(dir, &[ctx], false);
        write_v2_logs(
            dir,
            "logs-0.parquet",
            &[v2_log(2, Some(b"hello".to_vec()), Some(1_500_000))],
        );

        let scenario = load_all(dir, ParquetFormat::V2, &LoadOptions::default()).unwrap();
        assert_eq!(scenario.logs[0].hostname, "host-b", "last host tag wins");
        assert_eq!(scenario.logs[0].message, b"hello");
        assert_eq!(
            scenario.logs[0].tags.as_ref(),
            ["host:host-a", "source:check", "host:host-b"]
        );
    }

    #[test]
    fn v2_null_row_fields_default_safely() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        write_contexts_v2(dir, &[context(1, "system.cpu", None, None)], false);
        write_v2_metrics(
            dir,
            "metrics-0.parquet",
            &[V2MetricFixt {
                key: 1,
                value: None,
                ts_ns: None,
                source: None,
            }],
        );

        let scenario = load_all(dir, ParquetFormat::V2, &LoadOptions::default()).unwrap();
        assert_eq!(scenario.metrics[0].value, 0.0);
        assert_eq!(scenario.metrics[0].timestamp_sec, 0);
        assert_eq!(scenario.metrics[0].source, "");
    }

    #[test]
    fn v1_log_null_time_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        let mut row = v1_log(1_000);
        row.time_ms = None;
        write_v1_logs(dir, "observer-logs-0.parquet", &[row]);

        let mut stream = open_stream(dir, ParquetFormat::V1, &LoadOptions::default()).unwrap();
        let err = stream.find_map(|item| item.err()).expect("null time error");
        assert!(err.to_string().contains("Time is null at row 0"), "{err}");
    }
}
