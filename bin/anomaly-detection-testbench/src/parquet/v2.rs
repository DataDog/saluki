//! Parquet v2 decoding: a shared `contexts.parquet` plus context-keyed metric and log rows.
//!
//! This mirrors `bench/parquet_v2.go`. Context keys are file-local lookup identities (never Saluki
//! context keys); both signed and unsigned 64-bit representations are accepted. Row files store
//! signed 64-bit keys, which are cast to `u64` for lookup.
//!
//! Difference from the Go reader: when a required column is present but has an unexpected type, or
//! when a per-row group column reader fails, the Go v2 reader silently skips the rest of the row
//! group. Here that is reported as a [`LoadError`] instead, so malformed input is never silently
//! dropped while the replay still appears complete.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{
    Array, ArrayRef, BinaryArray, Float64Array, Int64Array, LargeBinaryArray, MapArray, StringArray, UInt64Array,
};
use arrow::record_batch::RecordBatch;

use super::{find_col, string_value, LoadError, LogObservation, Result};

/// A shared context: a metric/log name plus its pre-built `key:value` tags.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ContextEntry {
    pub(crate) name: String,
    pub(crate) tags: Arc<[String]>,
}

/// Context table keyed by the file-local context key.
pub(crate) type Contexts = HashMap<u64, ContextEntry>;

/// A decoded v2 metric row, before normalization and host resolution.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct V2Metric {
    pub(crate) name: String,
    pub(crate) value: f64,
    pub(crate) tags: Arc<[String]>,
    pub(crate) timestamp_sec: i64,
    pub(crate) source: String,
}

/// The fixed context tag columns, in the Go reader's emission order.
const FIXED_TAG_COLUMNS: [(&str, &str); 7] = [
    ("tag_host", "host"),
    ("tag_device", "device"),
    ("tag_source", "source"),
    ("tag_service", "service"),
    ("tag_env", "env"),
    ("tag_version", "version"),
    ("tag_team", "team"),
];

/// Loads `contexts.parquet` into a keyed map.
pub(crate) fn read_contexts(path: &Path) -> Result<Contexts> {
    let mut reader = super::open_reader(path)?;
    let mut contexts = Contexts::new();

    for batch in &mut reader {
        let batch = batch?;
        let rows = batch.num_rows();
        if rows == 0 {
            continue;
        }
        let schema = batch.schema();

        let key_idx = find_col(&schema, "context_key")
            .ok_or_else(|| LoadError::Decode("contexts.parquet missing required columns context_key or name".into()))?;
        let name_idx = find_col(&schema, "name")
            .ok_or_else(|| LoadError::Decode("contexts.parquet missing required columns context_key or name".into()))?;

        let key_column = batch.column(key_idx);
        let keys = KeyColumn::new(key_column).ok_or_else(|| {
            LoadError::Decode("contexts.parquet: context_key column is neither uint64 nor int64".into())
        })?;
        let names = batch.column(name_idx).as_any().downcast_ref::<StringArray>();

        let fixed: Vec<Option<&StringArray>> = FIXED_TAG_COLUMNS
            .iter()
            .map(|(column, _)| {
                find_col(&schema, column).and_then(|idx| batch.column(idx).as_any().downcast_ref::<StringArray>())
            })
            .collect();
        let tags_map = find_col(&schema, "tags").and_then(|idx| batch.column(idx).as_any().downcast_ref::<MapArray>());

        for row in 0..rows {
            let Some(key) = keys.get(row) else {
                continue;
            };

            let name = names.map_or_else(String::new, |column| {
                if column.is_null(row) {
                    String::new()
                } else {
                    column.value(row).to_string()
                }
            });

            let mut tags: Vec<String> = Vec::with_capacity(8);
            for (slot, (_, tag_key)) in fixed.iter().zip(FIXED_TAG_COLUMNS) {
                if let Some(column) = slot {
                    if !column.is_null(row) {
                        let value = column.value(row);
                        if !value.is_empty() {
                            tags.push(format!("{tag_key}:{value}"));
                        }
                    }
                }
            }
            if let Some(map) = tags_map {
                if !map.is_null(row) {
                    let offsets = map.value_offsets();
                    let (start, end) = (offsets[row] as usize, offsets[row + 1] as usize);
                    let keys = map.keys();
                    let values = map.values();
                    for index in start..end {
                        let Some(map_key) = string_value(keys, index) else {
                            continue;
                        };
                        if map_key.is_empty() {
                            continue;
                        }
                        match string_value(values, index) {
                            Some(value) if !value.is_empty() => tags.push(format!("{map_key}:{value}")),
                            _ => tags.push(map_key),
                        }
                    }
                }
            }

            contexts.insert(
                key,
                ContextEntry {
                    name,
                    tags: tags.into(),
                },
            );
        }
    }

    Ok(contexts)
}

/// Finds `metrics-*.parquet` files directly under `dir`, sorted by path.
pub(crate) fn find_metric_files(dir: &Path) -> Result<Vec<PathBuf>> {
    find_prefixed(dir, "metrics-")
}

/// Finds `logs-*.parquet` files directly under `dir`, sorted by path.
pub(crate) fn find_log_files(dir: &Path) -> Result<Vec<PathBuf>> {
    find_prefixed(dir, "logs-")
}

fn find_prefixed(dir: &Path, prefix: &str) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.starts_with(prefix) && name.ends_with(".parquet") {
            files.push(entry.path());
        }
    }
    files.sort();
    Ok(files)
}

/// Decodes a v2 metric batch, resolving each row against the shared context table.
pub(crate) fn decode_metric_batch(batch: &RecordBatch, contexts: &Contexts) -> Result<Vec<V2Metric>> {
    let rows = batch.num_rows();
    if rows == 0 {
        return Ok(Vec::new());
    }
    let schema = batch.schema();

    let key_idx = super::find_col(&schema, "context_key").ok_or_else(|| {
        LoadError::Decode("missing one or more required columns: context_key, value, timestamp_ns".into())
    })?;
    let value_idx = find_col(&schema, "value").ok_or_else(|| {
        LoadError::Decode("missing one or more required columns: context_key, value, timestamp_ns".into())
    })?;
    let ts_idx = find_col(&schema, "timestamp_ns").ok_or_else(|| {
        LoadError::Decode("missing one or more required columns: context_key, value, timestamp_ns".into())
    })?;

    let keys = KeyColumn::new(batch.column(key_idx))
        .ok_or_else(|| LoadError::Decode("metric context_key column is neither uint64 nor int64".into()))?;
    let value_column = batch.column(value_idx);
    let values = value_column.as_any().downcast_ref::<Float64Array>().ok_or_else(|| {
        LoadError::Decode(format!(
            "value column has type {:?}, expected float64",
            value_column.data_type()
        ))
    })?;
    let ts_column = batch.column(ts_idx);
    let timestamps = ts_column.as_any().downcast_ref::<Int64Array>().ok_or_else(|| {
        LoadError::Decode(format!(
            "timestamp_ns column has type {:?}, expected int64",
            ts_column.data_type()
        ))
    })?;
    let source = optional_string(batch, &schema, "source")?;

    let mut metrics = Vec::new();
    for row in 0..rows {
        let Some(key) = keys.get(row) else {
            continue;
        };
        let Some(context) = contexts.get(&key) else {
            continue;
        };

        metrics.push(V2Metric {
            name: context.name.clone(),
            value: if values.is_null(row) { 0.0 } else { values.value(row) },
            tags: context.tags.clone(),
            timestamp_sec: if timestamps.is_null(row) {
                0
            } else {
                timestamps.value(row)
            } / 1_000_000_000,
            source: source.map_or_else(String::new, |column| string_at(column, row)),
        });
    }
    Ok(metrics)
}

/// Decodes a v2 log batch, resolving each row against the shared context table.
///
/// The hostname and source come from the last `host:`/`source:` tags in the context. The tags
/// themselves are kept as-is (including `host:`/`source:`) and shared across rows.
pub(crate) fn decode_log_batch(batch: &RecordBatch, contexts: &Contexts) -> Result<Vec<LogObservation>> {
    let rows = batch.num_rows();
    if rows == 0 {
        return Ok(Vec::new());
    }
    let schema = batch.schema();

    let key_idx = super::find_col(&schema, "context_key").ok_or_else(|| {
        LoadError::Decode("missing one or more required columns: context_key, content, timestamp_ns".into())
    })?;
    let content_idx = find_col(&schema, "content").ok_or_else(|| {
        LoadError::Decode("missing one or more required columns: context_key, content, timestamp_ns".into())
    })?;
    let ts_idx = find_col(&schema, "timestamp_ns").ok_or_else(|| {
        LoadError::Decode("missing one or more required columns: context_key, content, timestamp_ns".into())
    })?;

    let keys = KeyColumn::new(batch.column(key_idx))
        .ok_or_else(|| LoadError::Decode("log context_key column is neither uint64 nor int64".into()))?;
    let content_column = batch.column(content_idx);
    if !is_binary(content_column.data_type()) {
        return Err(LoadError::Decode(format!(
            "content column has type {:?}, expected binary",
            content_column.data_type()
        )));
    }
    let ts_column = batch.column(ts_idx);
    let timestamps = ts_column.as_any().downcast_ref::<Int64Array>().ok_or_else(|| {
        LoadError::Decode(format!(
            "timestamp_ns column has type {:?}, expected int64",
            ts_column.data_type()
        ))
    })?;

    let mut logs = Vec::new();
    for row in 0..rows {
        let Some(key) = keys.get(row) else {
            continue;
        };
        let Some(context) = contexts.get(&key) else {
            continue;
        };

        let mut hostname = String::new();
        let mut _source = String::new();
        for tag in context.tags.iter() {
            if let Some(value) = tag.strip_prefix("host:") {
                hostname = value.to_string();
            } else if let Some(value) = tag.strip_prefix("source:") {
                _source = value.to_string();
            }
        }

        logs.push(LogObservation {
            message: binary_at(content_column, row),
            status: String::new(),
            tags: context.tags.clone(),
            hostname,
            timestamp_ms: if timestamps.is_null(row) {
                0
            } else {
                timestamps.value(row)
            } / 1_000_000,
        });
    }
    Ok(logs)
}

fn is_binary(data_type: &arrow::datatypes::DataType) -> bool {
    use arrow::datatypes::DataType;
    matches!(
        data_type,
        DataType::Binary | DataType::LargeBinary | DataType::Utf8 | DataType::LargeUtf8
    )
}

fn binary_at(column: &ArrayRef, row: usize) -> Vec<u8> {
    if column.is_null(row) {
        return Vec::new();
    }
    let any = column.as_any();
    if let Some(array) = any.downcast_ref::<BinaryArray>() {
        return array.value(row).to_vec();
    }
    if let Some(array) = any.downcast_ref::<LargeBinaryArray>() {
        return array.value(row).to_vec();
    }
    if let Some(array) = any.downcast_ref::<StringArray>() {
        return array.value(row).as_bytes().to_vec();
    }
    if let Some(array) = any.downcast_ref::<arrow::array::LargeStringArray>() {
        return array.value(row).as_bytes().to_vec();
    }
    Vec::new()
}

fn optional_string<'a>(
    batch: &'a RecordBatch, schema: &arrow::datatypes::Schema, name: &str,
) -> Result<Option<&'a StringArray>> {
    let Some(idx) = find_col(schema, name) else {
        return Ok(None);
    };
    let column = batch.column(idx);
    column.as_any().downcast_ref::<StringArray>().map(Some).ok_or_else(|| {
        LoadError::Decode(format!(
            "{name} column has type {:?}, expected string",
            column.data_type()
        ))
    })
}

fn string_at(column: &StringArray, row: usize) -> String {
    if column.is_null(row) {
        String::new()
    } else {
        column.value(row).to_string()
    }
}

/// A context-key column that may be stored as either an unsigned or a signed 64-bit integer.
enum KeyColumn<'a> {
    Unsigned(&'a UInt64Array),
    Signed(&'a Int64Array),
}

impl<'a> KeyColumn<'a> {
    fn new(column: &'a ArrayRef) -> Option<Self> {
        if let Some(array) = column.as_any().downcast_ref::<UInt64Array>() {
            return Some(Self::Unsigned(array));
        }
        column.as_any().downcast_ref::<Int64Array>().map(Self::Signed)
    }

    fn get(&self, row: usize) -> Option<u64> {
        match self {
            Self::Unsigned(array) => (!array.is_null(row)).then(|| array.value(row)),
            Self::Signed(array) => (!array.is_null(row)).then(|| array.value(row) as u64),
        }
    }
}
