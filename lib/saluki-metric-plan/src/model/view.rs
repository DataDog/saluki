use saluki_core::data_model::{
    event::metric::{context::Context, Metric, MetricMetadata},
    tags::TagSet,
};
use saluki_metric_kernels::intake_name::{is_normalized, normalize_into, NameBuf};

use super::Kind;

/// The state of a view's intake name, which is computed at most once.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum IntakeName {
    /// Not computed yet.
    Unknown,

    /// The intake rejects the name, so it has no intake name.
    Missing,

    /// The name is already normalized, so the intake name is the name itself.
    AsGiven,

    /// The intake name is the normalized name in the view's buffer.
    Normalized,
}

/// A read-only view of one metric, as predicates see it.
///
/// The view exposes the metric's stored fields directly, and computes its derived fields lazily: the
/// [intake name][Self::intake_name] is computed the first time it is read, at most once per view, and never allocates.
/// Build one view per metric and pass it to everything that evaluates against that metric, so that derived fields are
/// shared between them.
///
/// A view holds only what predicates can read of a metric: its context, kind, and metadata, never its values. So it can
/// also be built from those parts ([`from_parts`][Self::from_parts]) for a record that does not exist yet, such as one
/// an aggregation flush is about to emit, which a filter pushed into the flush judges before the record is built.
pub struct EventView<'a> {
    context: &'a Context,
    kind: Kind,
    metadata: &'a MetricMetadata,

    /// The raw name, read once: every name and intake name leaf reads it.
    name: &'a str,
    intake_name: IntakeName,

    /// The buffer a name that needs normalizing is normalized into. It is only initialized when it is needed, since
    /// almost every name is already normalized.
    buf: Option<NameBuf>,
}

impl<'a> EventView<'a> {
    /// Creates a view of `metric`.
    pub fn new(metric: &'a Metric) -> Self {
        Self::from_parts(metric.context(), Kind::of(metric.values()), metric.metadata())
    }

    /// Creates a view of a record with this context, kind, and metadata, whatever its values: what the view of every
    /// metric with these parts reads.
    pub fn from_parts(context: &'a Context, kind: Kind, metadata: &'a MetricMetadata) -> Self {
        Self {
            context,
            kind,
            metadata,
            name: context.name(),
            intake_name: IntakeName::Unknown,
            buf: None,
        }
    }

    /// Returns the context: the name, tags, origin tags, and host.
    pub fn context(&self) -> &'a Context {
        self.context
    }

    /// Returns the metadata.
    pub fn metadata(&self) -> &'a MetricMetadata {
        self.metadata
    }

    /// Returns the raw name, exactly as it is forwarded.
    pub fn name(&self) -> &'a str {
        self.name
    }

    /// Returns the host, or `None` if it is unset.
    ///
    /// An unset host is distinct from an empty one, which is `Some("")`.
    pub fn host(&self) -> Option<&'a str> {
        self.context.host()
    }

    /// Returns the instrumented tags.
    pub fn tags(&self) -> &'a TagSet {
        self.context.tags()
    }

    /// Returns the origin tags.
    pub fn origin_tags(&self) -> &'a TagSet {
        self.context.origin_tags()
    }

    /// Returns the metric's kind.
    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// Returns the intake name: the name as the intake stores it, or `None` if the intake rejects the name.
    ///
    /// The intake name is computed on the first call and remembered for later ones. An already-normalized name is
    /// returned as given; any other name is normalized into a buffer held inside the view, so this never allocates.
    pub fn intake_name(&mut self) -> Option<&[u8]> {
        if self.intake_name == IntakeName::Unknown {
            let name = self.name();
            self.intake_name = if is_normalized(name) {
                IntakeName::AsGiven
            } else {
                let buf = self.buf.get_or_insert_with(NameBuf::new);
                match normalize_into(buf, name) {
                    Some(_) => IntakeName::Normalized,
                    None => IntakeName::Missing,
                }
            };
        }

        match (self.intake_name, &self.buf) {
            (IntakeName::AsGiven, _) => Some(self.name.as_bytes()),
            (IntakeName::Normalized, Some(buf)) => Some(buf.as_bytes()),
            _ => None,
        }
    }
}

impl EventView<'_> {
    /// Returns `true` once the intake name has been computed.
    #[cfg(test)]
    pub(crate) fn has_computed_intake_name(&self) -> bool {
        self.intake_name != IntakeName::Unknown
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use saluki_core::data_model::{
        event::metric::{context::Context, Metric, MetricMetadata, MetricValues},
        tags::Tag,
    };
    use saluki_metric_kernels::intake_name::MAX_LENGTH;
    use stringtheory::MetaString;

    use super::*;

    fn metric_named(name: &str) -> Metric {
        Metric::gauge(Context::from_parts(name.to_string(), TagSet::default()), 1.0)
    }

    #[test]
    fn intake_name_is_derived_from_the_raw_name() {
        let over_limit = "a".repeat(MAX_LENGTH + 1);
        // (raw name, expected intake name)
        let cases: &[(&str, Option<&str>)] = &[
            ("already.normalized", Some("already.normalized")),
            ("my metric-name", Some("my_metric_name")),
            ("1app.requests", Some("app.requests")),
            ("café.requests", Some("caf.requests")),
            ("", None),
            ("123", None),
            ("🍣", None),
            (&over_limit, None),
        ];

        for (name, expected) in cases {
            let metric = metric_named(name);
            let mut view = EventView::new(&metric);
            assert_eq!(view.intake_name(), expected.map(str::as_bytes), "name: {:?}", name);
            // The raw name is never rewritten.
            assert_eq!(view.name(), *name);
        }
    }

    #[test]
    fn intake_name_is_computed_lazily_and_once() {
        let metric = metric_named("needs normalizing");
        let mut view = EventView::new(&metric);
        assert_eq!(view.intake_name, IntakeName::Unknown, "nothing is computed up front");
        assert!(view.buf.is_none());

        assert_eq!(view.intake_name(), Some(&b"needs_normalizing"[..]));
        assert_eq!(view.intake_name, IntakeName::Normalized);

        // A later read returns the remembered value. Replacing the remembered state proves the second read does not
        // recompute it.
        view.intake_name = IntakeName::Missing;
        assert_eq!(view.intake_name(), None);

        // An already-normalized name is remembered without copying it.
        let metric = metric_named("plain.name");
        let mut view = EventView::new(&metric);
        let intake_name = view.intake_name().expect("name is storable").as_ptr();
        assert_eq!(intake_name, metric.context().name().as_ptr());
        assert_eq!(view.intake_name, IntakeName::AsGiven);
        assert!(view.buf.is_none(), "an already-normalized name needs no buffer");
    }

    #[test]
    fn stored_fields_come_from_the_metric() {
        let context = Context::from_parts("requests", TagSet::from(Tag::from("env:prod")))
            .with_origin_tags(TagSet::from(Tag::from("team:a")))
            .with_host(Some(MetaString::from("")));
        let metric = Metric::counter(context, 1.0);
        let view = EventView::new(&metric);

        assert_eq!(view.name(), "requests");
        assert_eq!(view.host(), Some(""), "an empty host is not an unset host");
        assert!(view.tags().has_tag("env:prod"));
        assert!(!view.tags().has_tag("team:a"));
        assert!(view.origin_tags().has_tag("team:a"));
        assert_eq!(view.kind(), Kind::Counter);
        assert!(std::ptr::eq(view.context(), metric.context()));
        assert!(std::ptr::eq(view.metadata(), metric.metadata()));

        let metric = metric_named("unset.host");
        assert_eq!(EventView::new(&metric).host(), None);
    }

    #[test]
    fn a_view_from_parts_reads_what_the_view_of_a_metric_with_those_parts_reads() {
        // A filter pushed into an aggregation flush judges the record the flush is about to emit by its parts: the
        // context the flush holds, the kind it emits, a rate for a counter, and the metadata it kept.
        let context = Context::from_parts("needs normalizing", TagSet::from(Tag::from("env:prod")))
            .with_origin_tags(TagSet::from(Tag::from("team:a")))
            .with_host(Some(MetaString::from("h")));
        let metadata = MetricMetadata::default().with_unit(MetaString::from("second"));
        let metric = Metric::from_parts(
            context.clone(),
            MetricValues::rate(1.0, Duration::from_secs(10)),
            metadata.clone(),
        );

        let mut parts = EventView::from_parts(&context, Kind::Rate, &metadata);
        let mut whole = EventView::new(&metric);
        assert_eq!(parts.name(), whole.name());
        assert_eq!(parts.intake_name(), Some(&b"needs_normalizing"[..]));
        assert_eq!(parts.intake_name(), whole.intake_name());
        assert_eq!(parts.host(), whole.host());
        assert_eq!(parts.tags(), whole.tags());
        assert_eq!(parts.origin_tags(), whole.origin_tags());
        assert_eq!(parts.kind(), whole.kind());
        assert_eq!(parts.metadata(), whole.metadata());
        assert!(std::ptr::eq(parts.context(), &context));
    }
}
