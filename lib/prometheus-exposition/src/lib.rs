//! Prometheus exposition format rendering.
//!
//! Provides a reusable [`PrometheusRenderer`] type that renders metrics in the [Prometheus text exposition
//! format][prom-text]. The renderer owns internal buffers that are reused across calls, amortizing allocation costs.
//!
//! [prom-text]: https://prometheus.io/docs/instrumenting/exposition_formats/#text-based-format

use std::fmt::Write as _;

/// Prometheus metric type, used in the `# TYPE` header.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum MetricType {
    /// A counter metric.
    Counter,

    /// A gauge metric.
    Gauge,

    /// A histogram metric.
    Histogram,

    /// A summary metric.
    Summary,
}

impl MetricType {
    /// Returns the Prometheus type string.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::Gauge => "gauge",
            Self::Histogram => "histogram",
            Self::Summary => "summary",
        }
    }
}

/// A renderer for the Prometheus text exposition format.
///
/// The renderer owns internal buffers that are reused across calls, amortizing allocation costs when rendering
/// multiple payloads over time.
///
/// # Usage
///
/// There are two ways to use the renderer:
///
/// **High-level**: Use [`render_group`][Self::render_group] for counter/gauge groups where all series are simple
/// label/value pairs. This handles the TYPE/HELP headers and all series lines in one call.
///
/// **Low-level**: Use [`begin_group`][Self::begin_group] to start a new metric group (writes TYPE/HELP headers), then
/// call individual series writing methods like [`write_gauge_or_counter_series`][Self::write_gauge_or_counter_series],
/// [`write_histogram_series`][Self::write_histogram_series], or
/// [`write_summary_series`][Self::write_summary_series] for each tag combination. Finally, call
/// [`finish_group`][Self::finish_group] to append the group to the output. This is useful when you have multiple series
/// with complex types (histograms/summaries) sharing a single TYPE header.
pub struct PrometheusRenderer {
    output: String,
    metric_buffer: String,
    labels_buffer: String,
    name_buffer: String,
    groups_written: usize,
}

impl PrometheusRenderer {
    /// Creates a new `PrometheusRenderer`.
    pub fn new() -> Self {
        Self {
            output: String::new(),
            metric_buffer: String::new(),
            labels_buffer: String::new(),
            name_buffer: String::new(),
            groups_written: 0,
        }
    }

    /// Clears the output buffer to start a fresh payload, retaining allocated capacity.
    pub fn clear(&mut self) {
        self.output.clear();
        self.groups_written = 0;
    }

    /// Returns the accumulated output as a string slice.
    pub fn output(&self) -> &str {
        &self.output
    }

    /// Normalizes a metric name to a valid Prometheus metric name, returning a reference to the
    /// internal name buffer.
    ///
    /// Periods (`.`) are converted to double underscores (`__`). Other invalid characters are
    /// converted to a single underscore (`_`).
    pub fn normalize_metric_name<'a>(&'a mut self, name: &str) -> &'a str {
        self.normalize_name_internal(name);
        &self.name_buffer
    }

    /// Renders a counter or gauge metric group: `# TYPE` header, optional `# HELP` header, and all series lines.
    ///
    /// The metric name is automatically normalized to a valid Prometheus name.
    pub fn render_scalar_group<S, L, K, V>(
        &mut self, metric_name: &str, metric_type: MetricType, help_text: Option<&str>, series: S,
    ) where
        S: IntoIterator<Item = (L, f64)>,
        L: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        self.begin_group(metric_name, metric_type, help_text);
        for (labels, value) in series {
            self.write_gauge_or_counter_series(labels, value);
        }
        self.finish_group();
    }

    // --- Low-level API ---

    /// Begins a new metric group by writing the `# TYPE` and optional `# HELP` headers.
    ///
    /// The metric name is automatically normalized to a valid Prometheus name and stored internally
    /// for use by subsequent series writing methods.
    ///
    /// After calling this, write individual series using [`write_gauge_or_counter_series`][Self::write_gauge_or_counter_series],
    /// [`write_histogram_series`][Self::write_histogram_series], or
    /// [`write_summary_series`][Self::write_summary_series]. When done, call [`finish_group`][Self::finish_group].
    pub fn begin_group(&mut self, metric_name: &str, metric_type: MetricType, help_text: Option<&str>) {
        self.normalize_name_internal(metric_name);
        self.metric_buffer.clear();

        if let Some(help) = help_text {
            let _ = writeln!(self.metric_buffer, "# HELP {} {}", self.name_buffer, help);
        }
        let _ = writeln!(
            self.metric_buffer,
            "# TYPE {} {}",
            self.name_buffer,
            metric_type.as_str()
        );
    }

    /// Writes a single counter or gauge series line to the current metric group.
    ///
    /// Uses the metric name set by the most recent [`begin_group`][Self::begin_group] call.
    pub fn write_gauge_or_counter_series<L, K, V>(&mut self, labels: L, value: f64)
    where
        L: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let has_labels = self.format_labels(labels);

        // SAFETY: We need to index into `name_buffer` while pushing to `metric_buffer`. Since these
        // are separate fields, this is fine — we just need to avoid re-borrowing `self`.
        let name_len = self.name_buffer.len();
        self.metric_buffer.push_str(&self.name_buffer[..name_len]);
        if has_labels {
            self.metric_buffer.push('{');
            self.metric_buffer.push_str(&self.labels_buffer);
            self.metric_buffer.push('}');
        }
        let _ = writeln!(self.metric_buffer, " {}", value);
    }

    /// Writes a single histogram series (one set of labels) to the current metric group.
    ///
    /// Uses the metric name set by the most recent [`begin_group`][Self::begin_group] call.
    ///
    /// Each bucket is `(upper_bound_str, cumulative_count)`. A `+Inf` bucket is automatically
    /// appended using the total `count`.
    pub fn write_histogram_series<L, K, V, B>(&mut self, labels: L, buckets: B, sum: f64, count: u64)
    where
        L: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
        B: IntoIterator<Item = (&'static str, u64)>,
    {
        let has_labels = self.format_labels(labels);

        // Write bucket lines.
        for (upper_bound_str, cumulative_count) in buckets {
            let _ = write!(
                self.metric_buffer,
                "{}_bucket{{{}",
                self.name_buffer, self.labels_buffer
            );
            if has_labels {
                self.metric_buffer.push(',');
            }
            let _ = writeln!(self.metric_buffer, "le=\"{}\"}} {}", upper_bound_str, cumulative_count);
        }

        // Write +Inf bucket.
        let _ = write!(
            self.metric_buffer,
            "{}_bucket{{{}",
            self.name_buffer, self.labels_buffer
        );
        if has_labels {
            self.metric_buffer.push(',');
        }
        let _ = writeln!(self.metric_buffer, "le=\"+Inf\"}} {}", count);

        // Write sum and count.
        self.write_suffixed_value("_sum", has_labels, sum);
        self.write_suffixed_value("_count", has_labels, count as f64);
    }

    /// Writes a single summary series (one set of labels) to the current metric group.
    ///
    /// Uses the metric name set by the most recent [`begin_group`][Self::begin_group] call.
    ///
    /// Each quantile is `(quantile_value, observed_value)`.
    pub fn write_summary_series<L, K, V, Q>(&mut self, labels: L, quantiles: Q, sum: f64, count: u64)
    where
        L: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
        Q: IntoIterator<Item = (f64, f64)>,
    {
        let has_labels = self.format_labels(labels);

        // Write quantile lines.
        for (quantile, value) in quantiles {
            let _ = write!(self.metric_buffer, "{}{{{}", self.name_buffer, self.labels_buffer);
            if has_labels {
                self.metric_buffer.push(',');
            }
            let _ = writeln!(self.metric_buffer, "quantile=\"{}\"}} {}", quantile, value);
        }

        // Write sum and count.
        self.write_suffixed_value("_sum", has_labels, sum);
        self.write_suffixed_value("_count", has_labels, count as f64);
    }

    /// Finishes the current metric group, appending the metric buffer to the output.
    pub fn finish_group(&mut self) {
        if self.groups_written > 0 {
            self.output.push('\n');
        }
        self.output.push_str(&self.metric_buffer);
        self.groups_written += 1;
    }

    /// Formats labels into the labels buffer, returning whether any labels were written.
    fn format_labels<L, K, V>(&mut self, labels: L) -> bool
    where
        L: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        self.labels_buffer.clear();
        let mut has_labels = false;

        for (key, val) in labels {
            if has_labels {
                self.labels_buffer.push(',');
            }
            let _ = write!(self.labels_buffer, "{}=\"{}\"", key.as_ref(), val.as_ref());
            has_labels = true;
        }

        has_labels
    }

    /// Writes a `{name_buffer}{suffix}{labels} {value}` line.
    fn write_suffixed_value(&mut self, suffix: &str, has_labels: bool, value: f64) {
        let _ = write!(self.metric_buffer, "{}{}", self.name_buffer, suffix);
        if has_labels {
            let _ = write!(self.metric_buffer, "{{{}}}", self.labels_buffer);
        }
        let _ = writeln!(self.metric_buffer, " {}", value);
    }

    /// Normalizes a metric name into the internal name buffer.
    fn normalize_name_internal(&mut self, name: &str) {
        self.name_buffer.clear();

        for (i, c) in name.chars().enumerate() {
            if i == 0 && is_valid_name_start_char(c) || i != 0 && is_valid_name_char(c) {
                self.name_buffer.push(c);
            } else {
                // Convert periods to a set of two underscores, and anything else to a single
                // underscore. This lets us ensure that the normal separators we use in metrics
                // (periods) are converted in a way where they can be distinguished on the collector
                // side to potentially reconstitute them back to their original form.
                self.name_buffer.push_str(if c == '.' { "__" } else { "_" });
            }
        }
    }
}

impl Default for PrometheusRenderer {
    fn default() -> Self {
        Self::new()
    }
}

#[inline]
const fn is_valid_name_start_char(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || c == ':'
}

#[inline]
const fn is_valid_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == ':'
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn empty_tags() -> [(&'static str, &'static str); 0] {
        []
    }

    #[test]
    fn normalize_metric_name_basic() {
        let mut renderer = PrometheusRenderer::new();
        assert_eq!(renderer.normalize_metric_name("foo.bar.baz"), "foo__bar__baz");
        assert_eq!(renderer.normalize_metric_name("valid_name"), "valid_name");
        assert_eq!(renderer.normalize_metric_name("has-dash"), "has_dash");
        assert_eq!(renderer.normalize_metric_name("a.b-c"), "a__b_c");
    }

    #[test]
    fn render_counter_group() {
        let mut renderer = PrometheusRenderer::new();
        renderer.render_scalar_group(
            "requests_total",
            MetricType::Counter,
            Some("Total requests"),
            vec![([("method", "GET")], 42.0), ([("method", "POST")], 17.0)],
        );

        let output = renderer.output();
        assert!(output.contains("# HELP requests_total Total requests\n"));
        assert!(output.contains("# TYPE requests_total counter\n"));
        assert!(output.contains("requests_total{method=\"GET\"} 42\n"));
        assert!(output.contains("requests_total{method=\"POST\"} 17\n"));
    }

    #[test]
    fn render_gauge_no_labels() {
        let mut renderer = PrometheusRenderer::new();
        renderer.render_scalar_group("temperature", MetricType::Gauge, None, vec![(empty_tags(), 23.5)]);

        let output = renderer.output();
        assert!(!output.contains("# HELP"));
        assert!(output.contains("# TYPE temperature gauge\n"));
        assert!(output.contains("temperature 23.5\n"));
    }

    #[test]
    fn render_multiple_groups_separated() {
        let mut renderer = PrometheusRenderer::new();
        renderer.render_scalar_group("metric_a", MetricType::Counter, None, vec![(empty_tags(), 1.0)]);
        renderer.render_scalar_group("metric_b", MetricType::Gauge, None, vec![(empty_tags(), 2.0)]);

        let output = renderer.output();
        assert!(output.contains("metric_a 1\n\n# TYPE metric_b"));
    }

    #[test]
    fn clear_retains_capacity() {
        let mut renderer = PrometheusRenderer::new();
        renderer.render_scalar_group(
            "big_metric",
            MetricType::Counter,
            Some("A metric with help text"),
            vec![([("tag", "value")], 100.0)],
        );
        let cap_before = renderer.output.capacity();
        renderer.clear();
        assert!(renderer.output().is_empty());
        assert_eq!(renderer.output.capacity(), cap_before);
    }

    #[test]
    fn render_histogram_low_level() {
        let mut renderer = PrometheusRenderer::new();
        renderer.begin_group(
            "request_duration_seconds",
            MetricType::Histogram,
            Some("Request duration"),
        );
        renderer.write_histogram_series([("method", "GET")], vec![("0.1", 10), ("0.5", 25), ("1", 30)], 45.0, 30);
        renderer.finish_group();

        let output = renderer.output();
        assert!(output.contains("# HELP request_duration_seconds Request duration\n"));
        assert!(output.contains("# TYPE request_duration_seconds histogram\n"));
        assert!(output.contains("request_duration_seconds_bucket{method=\"GET\",le=\"0.1\"} 10\n"));
        assert!(output.contains("request_duration_seconds_bucket{method=\"GET\",le=\"+Inf\"} 30\n"));
        assert!(output.contains("request_duration_seconds_sum{method=\"GET\"} 45\n"));
        assert!(output.contains("request_duration_seconds_count{method=\"GET\"} 30\n"));
    }

    #[test]
    fn render_histogram_multiple_series() {
        let mut renderer = PrometheusRenderer::new();
        renderer.begin_group("http_duration_seconds", MetricType::Histogram, None);
        renderer.write_histogram_series([("method", "GET")], vec![("1", 5)], 10.0, 5);
        renderer.write_histogram_series([("method", "POST")], vec![("1", 3)], 6.0, 3);
        renderer.finish_group();

        let output = renderer.output();
        // One TYPE header, two sets of series.
        assert_eq!(output.matches("# TYPE").count(), 1);
        assert!(output.contains("http_duration_seconds_bucket{method=\"GET\",le=\"1\"} 5\n"));
        assert!(output.contains("http_duration_seconds_bucket{method=\"POST\",le=\"1\"} 3\n"));
    }

    #[test]
    fn render_summary_low_level() {
        let mut renderer = PrometheusRenderer::new();
        renderer.begin_group("rpc_duration_seconds", MetricType::Summary, None);
        renderer.write_summary_series(empty_tags(), vec![(0.5, 0.05), (0.99, 0.1)], 100.0, 1000);
        renderer.finish_group();

        let output = renderer.output();
        assert!(output.contains("# TYPE rpc_duration_seconds summary\n"));
        assert!(output.contains("rpc_duration_seconds{quantile=\"0.5\"} 0.05\n"));
        assert!(output.contains("rpc_duration_seconds{quantile=\"0.99\"} 0.1\n"));
        assert!(output.contains("rpc_duration_seconds_sum 100\n"));
        assert!(output.contains("rpc_duration_seconds_count 1000\n"));
    }
}
