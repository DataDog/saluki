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

mod histogram;
pub use self::histogram::{HistogramAggregate, HistogramParams, MAX_PERCENTILE};

mod mapper;
pub use self::mapper::{MapperProfile, MatchType, MetricMapping};

mod prefix;
pub use self::prefix::NamespacePrefixParams;

mod tags;
pub use self::tags::{
    validate_tag_value_rules, InvalidTagValueRules, TagFilterAction, TagFilterRule, TagValueMismatch, TagValueRule,
};
