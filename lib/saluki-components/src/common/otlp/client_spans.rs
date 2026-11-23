//! Per-client counters for OTLP spans received.
//!
//! Tags the counter with the client dimensions an operator needs to attribute traffic: the sending SDK's language, its
//! version, and the ingest endpoint version.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

use saluki_common::strings::StringBuilder;
use saluki_metrics::{Counter, MetricsBuilder};

use crate::common::otlp::traces::normalize::{is_normalized_tag_value, normalize_tag_value_into_unchecked};

/// Prefix on the SDK version in `tracer_version`, to tell OTLP clients from Datadog tracers.
const TRACER_VERSION_PREFIX: &str = "otlp-";

/// Value of the `endpoint_version` tag.
const ENDPOINT_VERSION: &str = "opentelemetry_grpc_v1";

/// Value of the `lang` and `tracer_version` tags on the shared series that absorbs clients past the cap.
const OVERFLOW_TAG_VALUE: &str = "overflow";

/// Maximum number of distinct (`lang`, `tracer_version`) series registered per component.
///
/// SDK language and version come from the sender, so without a cap a misbehaving client could create series without
/// limit. Clients past the cap are counted on a single shared overflow series.
pub(super) const MAX_CLIENT_SERIES: usize = 1000;

/// The registered client series, keyed by normalized language and then by normalized SDK version.
///
/// Nesting the maps lets a lookup borrow both keys as `&str`, so finding an existing series never allocates.
#[derive(Default)]
struct ClientSeries {
    by_language: HashMap<Box<str>, HashMap<Box<str>, Counter>>,
    len: usize,
}

impl ClientSeries {
    fn get(&self, language: &str, version: &str) -> Option<&Counter> {
        self.by_language.get(language)?.get(version)
    }
}

/// Counters for received OTLP spans, resolved by the sending client's identity.
///
/// Clones share one set of counters.
#[derive(Clone)]
pub struct ClientSpanCounters {
    builder: MetricsBuilder,
    max_series: usize,
    series: Arc<RwLock<ClientSeries>>,
    overflow: Arc<OnceLock<Counter>>,
}

impl ClientSpanCounters {
    /// Creates counters that register through `builder`, capped at [`MAX_CLIENT_SERIES`] client series.
    pub(super) fn new(builder: MetricsBuilder) -> Self {
        Self::with_max_series(builder, MAX_CLIENT_SERIES)
    }

    fn with_max_series(builder: MetricsBuilder, max_series: usize) -> Self {
        Self {
            builder,
            max_series,
            series: Arc::new(RwLock::new(ClientSeries::default())),
            overflow: Arc::new(OnceLock::new()),
        }
    }

    /// Returns the counter for spans sent by the client identified by the raw `telemetry.sdk.language` and
    /// `telemetry.sdk.version` resource attributes, registering it on first use.
    ///
    /// Both values are normalized as tag values. An empty language is left off the series, while an empty version
    /// still yields `tracer_version:otlp-`. Once [`MAX_CLIENT_SERIES`] series exist, new clients share one overflow
    /// series.
    pub fn for_client(&self, sdk_language: &str, sdk_version: &str) -> Counter {
        let language = normalized(sdk_language);
        let version = normalized(sdk_version);

        // Hits, and clients past the cap, are served under the read lock. Once the series are full they never grow
        // again, so the overflow check stays valid and those clients never contend for the write lock.
        {
            let series = self.series.read().unwrap();
            if let Some(counter) = series.get(&language, &version) {
                return counter.clone();
            }
            if series.len >= self.max_series {
                return self.overflow_counter();
            }
        }

        let mut series = self.series.write().unwrap();
        // Another thread may have registered the series, or filled the last slot, since the read lock was released.
        if let Some(counter) = series.get(&language, &version) {
            return counter.clone();
        }
        if series.len >= self.max_series {
            return self.overflow_counter();
        }

        let counter = register(&self.builder, (!language.is_empty()).then_some(&*language), &version);
        series
            .by_language
            .entry(language.into_owned().into_boxed_str())
            .or_default()
            .insert(version.into_owned().into_boxed_str(), counter.clone());
        series.len += 1;
        counter
    }

    fn overflow_counter(&self) -> Counter {
        self.overflow
            .get_or_init(|| register(&self.builder, Some(OVERFLOW_TAG_VALUE), OVERFLOW_TAG_VALUE))
            .clone()
    }
}

/// Registers the span counter for a client. `version` is the normalized SDK version, without its `otlp-` prefix.
fn register(builder: &MetricsBuilder, lang: Option<&str>, version: &str) -> Counter {
    let mut tags = vec![("message_type", "otlp_spans".to_string())];
    if let Some(lang) = lang {
        tags.push(("lang", lang.to_string()));
    }
    tags.push(("tracer_version", format!("{TRACER_VERSION_PREFIX}{version}")));
    tags.push(("endpoint_version", ENDPOINT_VERSION.to_string()));

    builder.register_counter_with_tags("component_events_received_total", tags)
}

/// Returns `value` normalized as a tag value, borrowing it when it's already normalized.
fn normalized(value: &str) -> Cow<'_, str> {
    if value.is_empty() || is_normalized_tag_value(value) {
        return Cow::Borrowed(value);
    }

    let mut normalized = StringBuilder::new();
    normalize_tag_value_into_unchecked(value, &mut normalized);
    Cow::Owned(normalized.as_str().to_string())
}

#[cfg(test)]
mod tests {
    use saluki_metrics::test::TestRecorder;

    use super::*;

    fn counters(max_series: usize) -> ClientSpanCounters {
        let builder = MetricsBuilder::default().add_default_tag("component_id:test");
        ClientSpanCounters::with_max_series(builder, max_series)
    }

    fn spans(recorder: &TestRecorder, tags: &'static [(&'static str, &'static str)]) -> Option<u64> {
        recorder.counter(("component_events_received_total", tags))
    }

    #[test]
    fn tags_the_series_with_the_client_identity() {
        let recorder = TestRecorder::default();
        let _guard = metrics::set_default_local_recorder(&recorder);

        counters(10).for_client("go", "1.2.3").increment(4);

        assert_eq!(
            spans(
                &recorder,
                &[
                    ("component_id", "test"),
                    ("message_type", "otlp_spans"),
                    ("lang", "go"),
                    ("tracer_version", "otlp-1.2.3"),
                    ("endpoint_version", "opentelemetry_grpc_v1"),
                ]
            ),
            Some(4)
        );
    }

    #[test]
    fn omits_lang_when_the_language_is_missing() {
        let recorder = TestRecorder::default();
        let _guard = metrics::set_default_local_recorder(&recorder);

        counters(10).for_client("", "1.2.3").increment(2);

        assert_eq!(
            spans(
                &recorder,
                &[
                    ("component_id", "test"),
                    ("message_type", "otlp_spans"),
                    ("tracer_version", "otlp-1.2.3"),
                    ("endpoint_version", "opentelemetry_grpc_v1"),
                ]
            ),
            Some(2)
        );
    }

    #[test]
    fn keeps_the_prefixed_tracer_version_when_the_version_is_missing() {
        let recorder = TestRecorder::default();
        let _guard = metrics::set_default_local_recorder(&recorder);

        counters(10).for_client("rust", "").increment(3);

        assert_eq!(
            spans(
                &recorder,
                &[
                    ("component_id", "test"),
                    ("message_type", "otlp_spans"),
                    ("lang", "rust"),
                    ("tracer_version", "otlp-"),
                    ("endpoint_version", "opentelemetry_grpc_v1"),
                ]
            ),
            Some(3)
        );
    }

    #[test]
    fn normalizes_the_language_and_version_as_tag_values() {
        let recorder = TestRecorder::default();
        let _guard = metrics::set_default_local_recorder(&recorder);

        counters(10).for_client("Node JS", "1.0 Beta/2").increment(1);

        assert_eq!(
            spans(
                &recorder,
                &[
                    ("component_id", "test"),
                    ("message_type", "otlp_spans"),
                    ("lang", "node_js"),
                    ("tracer_version", "otlp-1.0_beta/2"),
                    ("endpoint_version", "opentelemetry_grpc_v1"),
                ]
            ),
            Some(1)
        );
    }

    #[test]
    fn reuses_the_series_for_spellings_that_normalize_alike() {
        let counters = counters(10);

        counters.for_client("Go", "1.0").increment(1);
        counters.for_client("go", "1.0").increment(1);

        assert_eq!(counters.series.read().unwrap().len, 1);
    }

    #[test]
    fn counts_clients_past_the_cap_on_one_overflow_series() {
        let recorder = TestRecorder::default();
        let _guard = metrics::set_default_local_recorder(&recorder);

        let counters = counters(2);
        counters.for_client("go", "1").increment(1);
        counters.for_client("go", "2").increment(1);
        counters.for_client("go", "3").increment(5);
        counters.for_client("java", "9").increment(7);
        // A client registered before the cap is reached keeps its own series.
        counters.for_client("go", "1").increment(1);

        assert_eq!(counters.series.read().unwrap().len, 2);
        assert_eq!(
            spans(
                &recorder,
                &[
                    ("component_id", "test"),
                    ("message_type", "otlp_spans"),
                    ("lang", "overflow"),
                    ("tracer_version", "otlp-overflow"),
                    ("endpoint_version", "opentelemetry_grpc_v1"),
                ]
            ),
            Some(12)
        );
        assert_eq!(
            spans(
                &recorder,
                &[
                    ("component_id", "test"),
                    ("message_type", "otlp_spans"),
                    ("lang", "go"),
                    ("tracer_version", "otlp-1"),
                    ("endpoint_version", "opentelemetry_grpc_v1"),
                ]
            ),
            Some(2)
        );
    }
}
