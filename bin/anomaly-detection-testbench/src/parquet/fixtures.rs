//! In-test Parquet fixture writers.
//!
//! Every fixture writes a small Parquet file with a single row group per row (`max_row_group_size
//! = 1`), which keeps the fixtures tiny while exercising multi-row-group decoding, nulls, and
//! definition levels. The parquet writer dictionary-encodes string columns by default, so these
//! fixtures also cover dictionary-encoded reads.

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{
    ArrayRef, BinaryBuilder, BooleanBuilder, Float64Builder, Int64Builder, ListBuilder, MapBuilder, StringBuilder,
    UInt64Builder,
};
use arrow::datatypes::{DataType, Field, Fields, Schema, SchemaRef};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use parquet::file::properties::WriterProperties;

fn write_parquet(path: &Path, schema: SchemaRef, arrays: Vec<ArrayRef>, row_group: usize) {
    let batch = RecordBatch::try_new(schema.clone(), arrays).unwrap();
    let file = File::create(path).unwrap();
    let properties = WriterProperties::builder()
        .set_max_row_group_row_count(Some(row_group))
        .build();
    let mut writer = ArrowWriter::try_new(file, schema, Some(properties)).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();
}

fn list_type() -> DataType {
    DataType::List(Arc::new(Field::new("item", DataType::Utf8, true)))
}

fn map_type() -> DataType {
    DataType::Map(
        Arc::new(Field::new(
            "entries",
            DataType::Struct(Fields::from(vec![
                Field::new("key", DataType::Utf8, false),
                Field::new("value", DataType::Utf8, true),
            ])),
            false,
        )),
        false,
    )
}

/// Convenience constructor for a tag list with no null elements.
pub fn tag_list(items: &[&str]) -> Option<Vec<Option<String>>> {
    Some(items.iter().map(|item| Some((*item).to_string())).collect())
}

// ---- v1 metrics ----

/// A synthetic v1 metric row.
pub struct V1MetricFixt {
    pub source: String,
    pub time_ms: i64,
    pub name: String,
    pub value: Option<f64>,
    pub tags: Option<Vec<Option<String>>>,
    pub dropped: bool,
    pub labels: Vec<(String, Option<String>)>,
}

/// Builds a v1 metric fixture with empty tags, no labels, and `dropped = false`.
pub fn v1_metric(source: &str, time_ms: i64, name: &str, value: Option<f64>) -> V1MetricFixt {
    V1MetricFixt {
        source: source.to_string(),
        time_ms,
        name: name.to_string(),
        value,
        tags: None,
        dropped: false,
        labels: Vec::new(),
    }
}

/// Writes `observer-metrics-*`-style v1 metric rows, one row group per row.
pub fn write_v1_metrics(dir: &Path, name: &str, rows: &[V1MetricFixt]) {
    write_v1_metrics_rg(dir, name, rows, 1);
}

/// Writes v1 metric rows with an explicit row-group row count.
pub fn write_v1_metrics_rg(dir: &Path, name: &str, rows: &[V1MetricFixt], row_group: usize) {
    let mut label_keys: Vec<String> = Vec::new();
    for row in rows {
        for (key, _) in &row.labels {
            if !label_keys.contains(key) {
                label_keys.push(key.clone());
            }
        }
    }

    let mut fields = vec![
        Field::new("RunID", DataType::Utf8, true),
        Field::new("Time", DataType::Int64, false),
        Field::new("MetricName", DataType::Utf8, true),
        Field::new("ValueFloat", DataType::Float64, true),
        Field::new("Tags", list_type(), true),
        Field::new("Dropped", DataType::Boolean, false),
    ];
    for key in &label_keys {
        fields.push(Field::new(format!("l_{key}"), DataType::Utf8, true));
    }

    let mut run_id = StringBuilder::new();
    let mut time = Int64Builder::new();
    let mut metric_name = StringBuilder::new();
    let mut value = Float64Builder::new();
    let mut tags = ListBuilder::new(StringBuilder::new());
    let mut dropped = BooleanBuilder::new();

    for row in rows {
        run_id.append_value(&row.source);
        time.append_value(row.time_ms);
        metric_name.append_value(&row.name);
        match row.value {
            Some(value_row) => value.append_value(value_row),
            None => value.append_null(),
        }
        match &row.tags {
            Some(list) => {
                for tag in list {
                    match tag {
                        Some(tag) => tags.values().append_value(tag),
                        None => tags.values().append_null(),
                    }
                }
                tags.append(true);
            }
            None => tags.append_null(),
        }
        dropped.append_value(row.dropped);
    }

    let mut arrays: Vec<ArrayRef> = vec![
        Arc::new(run_id.finish()),
        Arc::new(time.finish()),
        Arc::new(metric_name.finish()),
        Arc::new(value.finish()),
        Arc::new(tags.finish()),
        Arc::new(dropped.finish()),
    ];
    for key in &label_keys {
        let mut builder = StringBuilder::new();
        for row in rows {
            match row
                .labels
                .iter()
                .find(|(label, _)| label == key)
                .and_then(|(_, value)| value.as_ref())
            {
                Some(value) => builder.append_value(value),
                None => builder.append_null(),
            }
        }
        arrays.push(Arc::new(builder.finish()));
    }

    write_parquet(&dir.join(name), Arc::new(Schema::new(fields)), arrays, row_group);
}

// ---- v1 logs ----

/// A synthetic v1 log row.
pub struct V1LogFixt {
    pub source: String,
    pub time_ms: Option<i64>,
    pub content: Option<Vec<u8>>,
    pub status: Option<String>,
    pub hostname: Option<String>,
    pub tags: Option<Vec<Option<String>>>,
}

/// Builds a v1 log fixture at `time_ms` with no optional fields.
pub fn v1_log(time_ms: i64) -> V1LogFixt {
    V1LogFixt {
        source: "run".to_string(),
        time_ms: Some(time_ms),
        content: None,
        status: None,
        hostname: None,
        tags: None,
    }
}

/// Writes `observer-logs-*`-style v1 log rows.
pub fn write_v1_logs(dir: &Path, name: &str, rows: &[V1LogFixt]) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("RunID", DataType::Utf8, true),
        Field::new("Time", DataType::Int64, true),
        Field::new("Content", DataType::Binary, true),
        Field::new("Status", DataType::Utf8, true),
        Field::new("Hostname", DataType::Utf8, true),
        Field::new("Tags", list_type(), true),
    ]));

    let mut run_id = StringBuilder::new();
    let mut time = Int64Builder::new();
    let mut content = BinaryBuilder::new();
    let mut status = StringBuilder::new();
    let mut hostname = StringBuilder::new();
    let mut tags = ListBuilder::new(StringBuilder::new());

    for row in rows {
        run_id.append_value(&row.source);
        match row.time_ms {
            Some(value) => time.append_value(value),
            None => time.append_null(),
        }
        match &row.content {
            Some(bytes) => content.append_value(bytes),
            None => content.append_null(),
        }
        match &row.status {
            Some(value) => status.append_value(value),
            None => status.append_null(),
        }
        match &row.hostname {
            Some(value) => hostname.append_value(value),
            None => hostname.append_null(),
        }
        match &row.tags {
            Some(list) => {
                for tag in list {
                    match tag {
                        Some(tag) => tags.values().append_value(tag),
                        None => tags.values().append_null(),
                    }
                }
                tags.append(true);
            }
            None => tags.append_null(),
        }
    }

    write_parquet(
        &dir.join(name),
        schema,
        vec![
            Arc::new(run_id.finish()),
            Arc::new(time.finish()),
            Arc::new(content.finish()),
            Arc::new(status.finish()),
            Arc::new(hostname.finish()),
            Arc::new(tags.finish()),
        ],
        1,
    );
}

// ---- v2 contexts ----

/// A synthetic v2 context entry.
pub struct V2ContextFixt {
    pub key: u64,
    pub name: Option<String>,
    pub host: Option<String>,
    pub source: Option<String>,
    pub tags: Vec<(String, Option<String>)>,
}

/// Builds a v2 context fixture.
pub fn context(key: u64, name: &str, host: Option<&str>, source: Option<&str>) -> V2ContextFixt {
    V2ContextFixt {
        key,
        name: Some(name.to_string()),
        host: host.map(str::to_string),
        source: source.map(str::to_string),
        tags: Vec::new(),
    }
}

/// Writes `contexts.parquet`, optionally using an unsigned context-key column.
pub fn write_contexts_v2(dir: &Path, rows: &[V2ContextFixt], unsigned: bool) {
    let key_type = if unsigned { DataType::UInt64 } else { DataType::Int64 };
    let schema = Arc::new(Schema::new(vec![
        Field::new("context_key", key_type, false),
        Field::new("name", DataType::Utf8, true),
        Field::new("tag_host", DataType::Utf8, true),
        Field::new("tag_source", DataType::Utf8, true),
        Field::new("tags", map_type(), true),
    ]));

    let mut key_i = Int64Builder::new();
    let mut key_u = UInt64Builder::new();
    let mut name = StringBuilder::new();
    let mut host = StringBuilder::new();
    let mut source = StringBuilder::new();
    let mut tags = MapBuilder::new(None, StringBuilder::new(), StringBuilder::new());

    for row in rows {
        if unsigned {
            key_u.append_value(row.key);
        } else {
            key_i.append_value(row.key as i64);
        }
        match &row.name {
            Some(value) => name.append_value(value),
            None => name.append_null(),
        }
        match &row.host {
            Some(value) => host.append_value(value),
            None => host.append_null(),
        }
        match &row.source {
            Some(value) => source.append_value(value),
            None => source.append_null(),
        }
        for (key, value) in &row.tags {
            tags.keys().append_value(key);
            match value {
                Some(value) => tags.values().append_value(value),
                None => tags.values().append_null(),
            }
        }
        tags.append(true).unwrap();
    }

    let key: ArrayRef = if unsigned {
        Arc::new(key_u.finish())
    } else {
        Arc::new(key_i.finish())
    };

    write_parquet(
        &dir.join("contexts.parquet"),
        schema,
        vec![
            key,
            Arc::new(name.finish()),
            Arc::new(host.finish()),
            Arc::new(source.finish()),
            Arc::new(tags.finish()),
        ],
        1,
    );
}

// ---- v2 metric/log rows ----

/// A synthetic v2 metric row.
pub struct V2MetricFixt {
    pub key: i64,
    pub value: Option<f64>,
    pub ts_ns: Option<i64>,
    pub source: Option<String>,
}

/// Builds a v2 metric row.
pub fn v2_metric(key: i64, value: f64, ts_ns: Option<i64>, source: Option<&str>) -> V2MetricFixt {
    V2MetricFixt {
        key,
        value: Some(value),
        ts_ns,
        source: source.map(str::to_string),
    }
}

/// Writes `metrics-*.parquet`.
pub fn write_v2_metrics(dir: &Path, name: &str, rows: &[V2MetricFixt]) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("context_key", DataType::Int64, true),
        Field::new("value", DataType::Float64, true),
        Field::new("timestamp_ns", DataType::Int64, true),
        Field::new("source", DataType::Utf8, true),
    ]));

    let mut key = Int64Builder::new();
    let mut value = Float64Builder::new();
    let mut timestamp = Int64Builder::new();
    let mut source = StringBuilder::new();

    for row in rows {
        key.append_value(row.key);
        match row.value {
            Some(value_row) => value.append_value(value_row),
            None => value.append_null(),
        }
        match row.ts_ns {
            Some(ts) => timestamp.append_value(ts),
            None => timestamp.append_null(),
        }
        match &row.source {
            Some(value) => source.append_value(value),
            None => source.append_null(),
        }
    }

    write_parquet(
        &dir.join(name),
        schema,
        vec![
            Arc::new(key.finish()),
            Arc::new(value.finish()),
            Arc::new(timestamp.finish()),
            Arc::new(source.finish()),
        ],
        1,
    );
}

/// A synthetic v2 log row.
pub struct V2LogFixt {
    pub key: i64,
    pub content: Option<Vec<u8>>,
    pub ts_ns: Option<i64>,
}

/// Builds a v2 log row.
pub fn v2_log(key: i64, content: Option<Vec<u8>>, ts_ns: Option<i64>) -> V2LogFixt {
    V2LogFixt { key, content, ts_ns }
}

/// Writes `logs-*.parquet`.
pub fn write_v2_logs(dir: &Path, name: &str, rows: &[V2LogFixt]) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("context_key", DataType::Int64, true),
        Field::new("content", DataType::Binary, true),
        Field::new("timestamp_ns", DataType::Int64, true),
    ]));

    let mut key = Int64Builder::new();
    let mut content = BinaryBuilder::new();
    let mut timestamp = Int64Builder::new();

    for row in rows {
        key.append_value(row.key);
        match &row.content {
            Some(bytes) => content.append_value(bytes),
            None => content.append_null(),
        }
        match row.ts_ns {
            Some(ts) => timestamp.append_value(ts),
            None => timestamp.append_null(),
        }
    }

    write_parquet(
        &dir.join(name),
        schema,
        vec![
            Arc::new(key.finish()),
            Arc::new(content.finish()),
            Arc::new(timestamp.finish()),
        ],
        1,
    );
}
