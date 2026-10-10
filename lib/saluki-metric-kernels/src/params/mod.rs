//! Parameter types for kernels.
//!
//! Kernels never read configuration. Instead, each kernel takes its resolved parameters as plain types defined in this
//! module, so that the same kernel can be driven by a legacy component, by a plan whose parameters were lowered from
//! configuration, or by a test, and so that this crate never depends on any configuration crate.
//!
//! - [`MapperProfile`]: name mapping profiles, which rewrite matching names and add tags.
//! - [`NamespacePrefixParams`]: the namespace prepended to names, and the names it is not prepended to.
//! - [`TagFilterRule`] and [`TagValueRule`]: tag projection rules, which remove tags and restrict tag values.
//! - [`HistogramParams`]: the statistics that aggregation computes for each histogram, and its distribution copies.
//! - [`ALLOWED_BUCKET_WIDTHS_SECS`]: the bucket widths aggregation allows, and [`is_allowed_bucket_width`], which a
//!   host validates a configured width with.

mod bucket_width;
pub use self::bucket_width::{
    describe_allowed_widths, is_allowed_bucket_width, ALLOWED_BUCKET_WIDTHS_SECS, MAX_TRANSITION_LCM_SECS,
};

mod histogram;
pub use self::histogram::{HistogramAggregate, HistogramParams, InvalidHistogramStatistic, MAX_PERCENTILE};

mod mapper;
pub use self::mapper::{MapperProfile, MatchType, MetricMapping};

mod prefix;
pub use self::prefix::NamespacePrefixParams;

mod tags;
pub use self::tags::{
    validate_tag_value_rule_keys, validate_tag_value_rules, InvalidTagValueRules, TagFilterAction, TagFilterRule,
    TagValueMismatch, TagValueRule,
};
