//! Parquet v1 ("FGM") decoding: inline-tag metrics and logs.
//!
//! This mirrors `bench/parquet.go`. Metric columns are decoded leniently (missing or wrong-typed
//! optional columns are ignored, matching the Go reader); the log decoder is strict and reports the
//! first structural problem it finds.

use std::path::{Path, PathBuf};

use arrow::array::{
    Array, ArrayRef, BinaryArray, BooleanArray, Float64Array, Int64Array, LargeStringArray, ListArray, StringArray,
    TimestampMicrosecondArray, TimestampMillisecondArray, TimestampNanosecondArray, TimestampSecondArray,
};
use arrow::datatypes::{DataType, Schema};
use arrow::record_batch::RecordBatch;

use super::{find_col, string_value, LoadError, LogObservation, Result, MIN_PARQUET_FILE_SIZE};

/// A decoded v1 metric row, before normalization.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FgmMetric {
    pub(crate) source: String,
    pub(crate) time_ms: i64,
    pub(crate) name: String,
    pub(crate) value: f64,
    pub(crate) tags: Vec<String>,
    pub(crate) dropped: bool,
}

/// Finds `observer-metrics-*.parquet` files recursively under `dir`, sorted by path.
///
/// Files smaller than [`MIN_PARQUET_FILE_SIZE`] are excluded at discovery, matching the Go finder.
pub(crate) fn find_metric_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    walk_metrics(dir, &mut files)?;
    files.sort();
    Ok(files)
}

fn walk_metrics(dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            walk_metrics(&path, files)?;
            continue;
        }
        if !is_named(&path, "observer-metrics-") {
            continue;
        }
        if entry.metadata()?.len() >= MIN_PARQUET_FILE_SIZE {
            files.push(path);
        }
    }
    Ok(())
}

/// Finds `observer-logs-*.parquet` files directly under `dir`, sorted by path.
///
/// Unlike the metric finder, the log finder does not filter on size; the size is checked when the
/// file is opened.
pub(crate) fn find_log_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            continue;
        }
        if is_named(&entry.path(), "observer-logs-") {
            files.push(entry.path());
        }
    }
    files.sort();
    Ok(files)
}

fn is_named(path: &Path, prefix: &str) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(prefix) && name.ends_with(".parquet"))
}

/// Decodes a v1 metric batch into raw rows.
pub(crate) fn decode_metric_batch(batch: &RecordBatch) -> Result<Vec<FgmMetric>> {
    let rows = batch.num_rows();
    if rows == 0 {
        return Ok(Vec::new());
    }
    let schema = batch.schema();

    let run_id = optional_string(batch, &schema, "runid");
    let metric_name = optional_string(batch, &schema, "metricname");
    let value_float =
        find_col(&schema, "valuefloat").and_then(|idx| batch.column(idx).as_any().downcast_ref::<Float64Array>());
    let tags = find_col(&schema, "tags").and_then(|idx| batch.column(idx).as_any().downcast_ref::<ListArray>());
    let dropped =
        find_col(&schema, "dropped").and_then(|idx| batch.column(idx).as_any().downcast_ref::<BooleanArray>());
    let time = find_col(&schema, "time").map(|idx| batch.column(idx));

    // Optional `l_*` label columns override tag-list values for the matching key.
    let labels: Vec<(String, &StringArray)> = schema
        .fields()
        .iter()
        .enumerate()
        .filter_map(|(idx, field)| {
            let key = field.name().strip_prefix("l_")?;
            let column = batch.column(idx).as_any().downcast_ref::<StringArray>()?;
            Some((key.to_string(), column))
        })
        .collect();

    let mut metrics = Vec::with_capacity(rows);
    for i in 0..rows {
        let mut ordered: Vec<(String, String)> = Vec::new();
        if let Some(list) = tags {
            if !list.is_null(i) {
                let offsets = list.value_offsets();
                let (start, end) = (offsets[i] as usize, offsets[i + 1] as usize);
                let values = list.values();
                for j in start..end {
                    if let Some(tag) = string_value(values, j) {
                        insert_tag(&mut ordered, &tag);
                    }
                }
            }
        }
        for (key, column) in &labels {
            if !column.is_null(i) {
                let value = column.value(i);
                if !value.is_empty() {
                    upsert_tag(&mut ordered, key, value);
                }
            }
        }

        metrics.push(FgmMetric {
            source: run_id.map_or_else(String::new, |col| string_at(col, i)),
            time_ms: read_time(time, i),
            name: metric_name.map_or_else(String::new, |col| string_at(col, i)),
            value: value_float.map_or(0.0, |col| if col.is_null(i) { 0.0 } else { col.value(i) }),
            tags: ordered
                .into_iter()
                .map(|(key, value)| {
                    if value.is_empty() {
                        key
                    } else {
                        format!("{key}:{value}")
                    }
                })
                .collect(),
            dropped: dropped.is_some_and(|col| !col.is_null(i) && col.value(i)),
        });
    }
    Ok(metrics)
}

/// Decodes a v1 log batch into normalized observations.
///
/// The `Time` column is required and must be an Int64; a null time is an error. Wrong-typed
/// optional columns are errors too, matching the Go streaming reader.
pub(crate) fn decode_log_batch(batch: &RecordBatch) -> Result<Vec<LogObservation>> {
    let rows = batch.num_rows();
    if rows == 0 {
        return Ok(Vec::new());
    }
    let schema = batch.schema();

    let time_idx = find_col(&schema, "time").ok_or_else(|| LoadError::Decode("missing required Time column".into()))?;
    let time_col = batch.column(time_idx);
    let time = time_col.as_any().downcast_ref::<Int64Array>().ok_or_else(|| {
        LoadError::Decode(format!(
            "Time column has type {:?}, expected int64",
            time_col.data_type()
        ))
    })?;

    let source = required_string(batch, &schema, "runid")?;
    let status = required_string(batch, &schema, "status")?;
    let hostname = required_string(batch, &schema, "hostname")?;

    let content = match find_col(&schema, "content") {
        Some(idx) => {
            let column = batch.column(idx);
            Some(column.as_any().downcast_ref::<BinaryArray>().ok_or_else(|| {
                LoadError::Decode(format!(
                    "Content column has type {:?}, expected binary",
                    column.data_type()
                ))
            })?)
        }
        None => None,
    };

    let tags = match find_col(&schema, "tags") {
        Some(idx) => {
            let column = batch.column(idx);
            let list = column.as_any().downcast_ref::<ListArray>().ok_or_else(|| {
                LoadError::Decode(format!("Tags column has type {:?}, expected list", column.data_type()))
            })?;
            let values = list.values();
            if values.as_any().downcast_ref::<StringArray>().is_none()
                && values.as_any().downcast_ref::<LargeStringArray>().is_none()
            {
                return Err(LoadError::Decode(format!(
                    "Tags values have type {:?}, expected string",
                    values.data_type()
                )));
            }
            Some(list)
        }
        None => None,
    };

    let mut logs = Vec::with_capacity(rows);
    for i in 0..rows {
        if time.is_null(i) {
            return Err(LoadError::Decode(format!("Time is null at row {i}")));
        }

        let message = match content {
            Some(col) if !col.is_null(i) => col.value(i).to_vec(),
            _ => Vec::new(),
        };
        let tags = match tags {
            Some(list) => read_log_tags(list, i),
            None => Vec::new(),
        };

        // The v1 `RunID` column is validated but not part of the normalized log observation.
        let _ = source.map_or_else(String::new, |col| string_at(col, i));

        logs.push(LogObservation {
            message,
            status: status.map_or_else(String::new, |col| string_at(col, i)),
            tags: tags.into(),
            hostname: hostname.map_or_else(String::new, |col| string_at(col, i)),
            timestamp_ms: time.value(i),
        });
    }
    Ok(logs)
}

fn read_log_tags(list: &ListArray, row: usize) -> Vec<String> {
    if list.is_null(row) {
        return Vec::new();
    }
    let offsets = list.value_offsets();
    let (start, end) = (offsets[row] as usize, offsets[row + 1] as usize);
    let values = list.values();
    (start..end)
        .map(|index| string_value(values, index).unwrap_or_default())
        .collect()
}

fn string_at(column: &StringArray, row: usize) -> String {
    if column.is_null(row) {
        String::new()
    } else {
        column.value(row).to_string()
    }
}

fn optional_string<'a>(batch: &'a RecordBatch, schema: &Schema, name: &str) -> Option<&'a StringArray> {
    let idx = find_col(schema, name)?;
    batch.column(idx).as_any().downcast_ref::<StringArray>()
}

fn required_string<'a>(batch: &'a RecordBatch, schema: &Schema, name: &str) -> Result<Option<&'a StringArray>> {
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

fn read_time(column: Option<&ArrayRef>, row: usize) -> i64 {
    let Some(column) = column else {
        return 0;
    };
    if column.is_null(row) {
        return 0;
    }
    match column.data_type() {
        DataType::Int64 => column
            .as_any()
            .downcast_ref::<Int64Array>()
            .map_or(0, |array| array.value(row)),
        DataType::Timestamp(_, _) => read_timestamp_raw(column, row),
        _ => 0,
    }
}

fn read_timestamp_raw(column: &ArrayRef, row: usize) -> i64 {
    let any = column.as_any();
    if let Some(array) = any.downcast_ref::<TimestampSecondArray>() {
        return array.value(row);
    }
    if let Some(array) = any.downcast_ref::<TimestampMillisecondArray>() {
        return array.value(row);
    }
    if let Some(array) = any.downcast_ref::<TimestampMicrosecondArray>() {
        return array.value(row);
    }
    if let Some(array) = any.downcast_ref::<TimestampNanosecondArray>() {
        return array.value(row);
    }
    0
}

fn insert_tag(ordered: &mut Vec<(String, String)>, tag: &str) {
    match tag.split_once(':') {
        Some((key, value)) => upsert_tag(ordered, key, value),
        None => {
            if !tag.is_empty() {
                upsert_tag(ordered, tag, "");
            }
        }
    }
}

fn upsert_tag(ordered: &mut Vec<(String, String)>, key: &str, value: &str) {
    match ordered.iter_mut().find(|(existing, _)| existing == key) {
        Some((_, slot)) => *slot = value.to_string(),
        None => ordered.push((key.to_string(), value.to_string())),
    }
}
