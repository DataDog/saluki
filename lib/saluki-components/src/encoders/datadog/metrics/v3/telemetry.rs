use std::sync::Arc;

use datadog_metrics_v3::V3ValueEncodingStats;
use metrics::Counter;
use saluki_metrics::MetricsBuilder;

use super::constants::COLUMN_NAMES;

#[derive(Clone, Copy, Debug)]
pub(crate) enum V3PayloadSplitReason {
    MaxPoints,
    PayloadFull,
    ItemTooBig,
}

struct V3ColumnSizeTelemetry {
    compressed: Counter,
    uncompressed: Counter,
}

struct V3SerializerTelemetryInner {
    values_zero: Counter,
    values_sint64: Counter,
    values_float32: Counter,
    values_float64: Counter,
    item_too_big: Counter,
    split_max_points: Counter,
    split_payload_full: Counter,
    split_item_too_big: Counter,
    column_sizes: Vec<Option<V3ColumnSizeTelemetry>>,
}

#[derive(Clone)]
pub(crate) struct V3SerializerTelemetry {
    inner: Arc<V3SerializerTelemetryInner>,
}

impl V3SerializerTelemetry {
    pub(crate) fn from_builder(builder: &MetricsBuilder) -> Self {
        let mut column_sizes = Vec::with_capacity(COLUMN_NAMES.len());
        column_sizes.push(None);
        for column_name in COLUMN_NAMES.iter().skip(1) {
            column_sizes.push(Some(V3ColumnSizeTelemetry {
                compressed: builder.register_counter_with_tags(
                    "serializer.v3_column_size",
                    [("column", *column_name), ("compressed", "compressed")],
                ),
                uncompressed: builder.register_counter_with_tags(
                    "serializer.v3_column_size",
                    [("column", *column_name), ("compressed", "uncompressed")],
                ),
            }));
        }

        Self {
            inner: Arc::new(V3SerializerTelemetryInner {
                values_zero: builder.register_counter_with_tags("serializer.v3_values_count", ["type:zero"]),
                values_sint64: builder.register_counter_with_tags("serializer.v3_values_count", ["type:sint64"]),
                values_float32: builder.register_counter_with_tags("serializer.v3_values_count", ["type:float32"]),
                values_float64: builder.register_counter_with_tags("serializer.v3_values_count", ["type:float64"]),
                item_too_big: builder.register_counter("serializer.v3_item_too_big"),
                split_max_points: builder
                    .register_counter_with_tags("serializer.v3_payload_split_reason", ["reason:max_points"]),
                split_payload_full: builder
                    .register_counter_with_tags("serializer.v3_payload_split_reason", ["reason:payload_full"]),
                split_item_too_big: builder
                    .register_counter_with_tags("serializer.v3_payload_split_reason", ["reason:item_too_big"]),
                column_sizes,
            }),
        }
    }

    pub(crate) fn record_values_count(&self, stats: V3ValueEncodingStats) {
        self.inner.values_zero.increment(stats.zero);
        self.inner.values_sint64.increment(stats.sint64);
        self.inner.values_float32.increment(stats.float32);
        self.inner.values_float64.increment(stats.float64);
    }

    pub(crate) fn record_column_size(&self, field_number: u32, uncompressed_size: u64, compressed_size: u64) {
        let Some(Some(column)) = self.inner.column_sizes.get(field_number as usize) else {
            return;
        };

        column.uncompressed.increment(uncompressed_size);
        column.compressed.increment(compressed_size);
    }

    pub(crate) fn record_item_too_big(&self) {
        self.inner.item_too_big.increment(1);
    }

    pub(crate) fn record_split_reason(&self, reason: V3PayloadSplitReason) {
        match reason {
            V3PayloadSplitReason::MaxPoints => self.inner.split_max_points.increment(1),
            V3PayloadSplitReason::PayloadFull => self.inner.split_payload_full.increment(1),
            V3PayloadSplitReason::ItemTooBig => self.inner.split_item_too_big.increment(1),
        }
    }
}
