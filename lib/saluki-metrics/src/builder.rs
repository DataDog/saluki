use metrics::{counter, gauge, histogram, Counter, Gauge, Histogram, Label, Level, SharedString};

mod private {
    use metrics::SharedString;

    pub trait Sealed {}

    impl Sealed for &'static str {}
    impl Sealed for String {}
    impl<T> Sealed for (&'static str, T) where T: Into<SharedString> {}
}

/// A metric tag.
///
/// Marker trait for types which can support being used as a metric tag, in order to optimize their internal
/// representation to avoid unnecessary allocations.
///
/// This trait is sealed and cannot be implemented outside of this crate.
pub trait MetricTag: private::Sealed {
    /// Consumes `self` and converts it to a tag.
    ///
    /// Under the hood, the [`metrics`][metrics] crate is used, which calls tags "labels" instead, which is where the
    /// return type naming comes from.
    ///
    /// [metrics]: https://docs.rs/metrics
    fn into_label(self) -> Label;
}

impl MetricTag for &'static str {
    fn into_label(self) -> Label {
        match self.split_once(':') {
            Some((key, value)) => Label::from_static_parts(key, value),
            None => Label::from_static_parts(self, ""),
        }
    }
}

impl MetricTag for String {
    fn into_label(self) -> Label {
        match self.split_once(':') {
            Some((key, value)) => Label::new(key.to_string(), value.to_string()),
            None => Label::new(self, ""),
        }
    }
}

impl<T> MetricTag for (&'static str, T)
where
    T: Into<SharedString>,
{
    fn into_label(self) -> Label {
        Label::new(SharedString::const_str(self.0), self.1.into())
    }
}

/// Builder for constructing metrics.
///
/// This builder is simplistic, but supports constructing metrics with default tags, and in an API-driven way to help
/// ensure consistent tagging across the board.
#[derive(Clone, Default)]
pub struct MetricsBuilder {
    default_tags: Vec<Label>,
}

impl MetricsBuilder {
    /// Adds an additional default tag to use when constructing metrics.
    ///
    /// These tags will be included along with any existing default tags configured in the builder.
    ///
    /// Tags can be provided in numerous forms:
    /// - individual tags (`"tag_name"` or `"tag_name:tag_value"`, either as `&'static str` or `String`)
    /// - key/value tuples (`("tag_name", "tag_value")`, with the name as `&'static str` and the value as either
    ///   `&'static str` or `String`)
    pub fn add_default_tag<T>(mut self, tag: T) -> Self
    where
        T: MetricTag,
    {
        self.default_tags.push(tag.into_label());
        self
    }

    /// Registers a counter at debug verbosity.
    ///
    /// The counter will include the configured default tags for this builder.
    pub fn register_debug_counter(&self, metric_name: &'static str) -> Counter {
        let tags = self.default_tags.clone();
        counter!(level: Level::DEBUG, metric_name, tags)
    }

    /// Registers a counter at debug verbosity with additional tags.
    ///
    /// The counter will include the configured default tags for this builder, in addition to the additional tags provided.
    ///
    /// See [`add_default_tags`](MetricsBuilder::add_default_tags) for information on the supported tag formats.
    pub fn register_debug_counter_with_tags<I, T>(&self, metric_name: &'static str, additional_tags: I) -> Counter
    where
        I: IntoIterator<Item = T>,
        T: MetricTag,
    {
        let mut tags = self.default_tags.clone();
        tags.extend(additional_tags.into_iter().map(MetricTag::into_label));

        counter!(level: Level::DEBUG, metric_name, tags)
    }

    /// Registers a gauge at debug verbosity.
    ///
    /// The gauge will include the configured default tags for this builder.
    pub fn register_debug_gauge(&self, metric_name: &'static str) -> Gauge {
        let tags = self.default_tags.clone();
        gauge!(level: Level::DEBUG, metric_name, tags)
    }

    /// Registers a gauge at debug verbosity with additional tags.
    ///
    /// The gauge will include the configured default tags for this builder, in addition to the additional tags provided.
    ///
    /// See [`add_default_tags`](MetricsBuilder::add_default_tags) for information on the supported tag formats.
    pub fn register_debug_gauge_with_tags<I, T>(&self, metric_name: &'static str, additional_tags: I) -> Gauge
    where
        I: IntoIterator<Item = T>,
        T: MetricTag,
    {
        let mut tags = self.default_tags.clone();
        tags.extend(additional_tags.into_iter().map(MetricTag::into_label));

        gauge!(level: Level::DEBUG, metric_name, tags)
    }

    /// Registers a histogram at debug verbosity.
    ///
    /// The histogram will include the configured default tags for this builder.
    pub fn register_debug_histogram(&self, metric_name: &'static str) -> Histogram {
        let tags = self.default_tags.clone();
        histogram!(level: Level::DEBUG, metric_name, tags)
    }

    /// Registers a histogram at debug verbosity with additional tags.
    ///
    /// The histogram will include the configured default tags for this builder, in addition to the additional tags provided.
    ///
    /// See [`add_default_tags`](MetricsBuilder::add_default_tags) for information on the supported tag formats.
    pub fn register_debug_histogram_with_tags<I, T>(&self, metric_name: &'static str, additional_tags: I) -> Histogram
    where
        I: IntoIterator<Item = T>,
        T: MetricTag,
    {
        let mut tags = self.default_tags.clone();
        tags.extend(additional_tags.into_iter().map(MetricTag::into_label));

        histogram!(level: Level::DEBUG, metric_name, tags)
    }
}
