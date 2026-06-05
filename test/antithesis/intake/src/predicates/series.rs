//! Series-level predicates from the `invariant-jig` `README.md`
//! §Properties.Payloads for checks that read a whole `MetricSeries` rather than
//! a single point.

use datadog_protos::metrics::metric_payload::{MetricSeries, Resource};
use datadog_protos::metrics::MetricType;

use crate::predicates::constants::{
    MAX_HOST_NAME_BYTES, MAX_METRIC_NAME_BYTES, ORIGIN_CATEGORY_MAX, ORIGIN_CATEGORY_RESERVED, ORIGIN_PRODUCT_MAX,
    ORIGIN_SERVICE_MAX, ORIGIN_SERVICE_RESERVED,
};

/// Intake resolves a series' host with `series.Host()`, which scans the
/// resources and returns the first whose `type` is `"host"`. It ignores
/// position and ignores any later host-typed resource. A series with no
/// host-typed resource resolves to no host. The scan mirrors dd-source
/// `apiv2/custom.go MetricSeries.Host`. W17 and W19 both read the host through
/// this one helper, so the rig resolves the host exactly as intake does.
#[must_use]
pub fn host_resource(series: &MetricSeries) -> Option<&Resource> {
    series.resources.iter().find(|r| r.type_() == "host")
}

/// W19 -- Resource, Host Name Length. Specification §Properties.Payloads W19:
/// the host name is at most 255 bytes. Intake reads the host via
/// [`host_resource`], the first resource whose `type` is `"host"`. A series
/// with no host-typed resource carries no host name, so the check does not
/// apply. Returns the over-cap host name when it exceeds the bound, or `None`
/// otherwise.
#[must_use]
pub fn w19_host_name_too_long(series: &MetricSeries) -> Option<&str> {
    let host = host_resource(series)?;
    (host.name().len() > MAX_HOST_NAME_BYTES).then_some(host.name())
}

/// W13 -- `MetricSeries`, Tag Count. Specification §Properties.Payloads W13: a
/// series carries at most `MaxTags(orgID)` tags. The bound is inclusive, so
/// intake rejects a series strictly over the cap. An Agent reports to one org,
/// so the caller passes that single org's `max_tags` cap. Returns the series
/// tag count when it exceeds the cap, or `None` otherwise. The W13 asymmetry
/// records the Agent not capping tag count, so this is an intake-only check.
#[must_use]
pub fn w13_tag_count(series: &MetricSeries, max_tags: usize) -> Option<usize> {
    let count = series.tags.len();
    (count > max_tags).then_some(count)
}

/// W18 -- `MetricSeries`, Resource Count. Specification §Properties.Payloads
/// W18: a series carries at most `MaxResources(orgID)` resources. The bound is
/// inclusive, so intake rejects a series strictly over the cap. An Agent
/// reports to one org, so the caller passes that single org's `max_resources`
/// cap. Returns the series resource count when it exceeds the cap, or `None`
/// otherwise. The Agent does not cap resource count, so this is an intake-only
/// check.
#[must_use]
pub fn w18_resource_count(series: &MetricSeries, max_resources: usize) -> Option<usize> {
    let count = series.resources.len();
    (count > max_resources).then_some(count)
}

/// W9 -- `MetricSeries`, Metric Non-Empty. Specification §Properties.Payloads
/// W9: a series carries a non-empty `metric`. Whitespace-only names are
/// non-empty and pass this step. The Agent does not validate the metric name,
/// so this is an intake-only check.
#[must_use]
pub fn w9_metric_non_empty(series: &MetricSeries) -> bool {
    !series.metric().is_empty()
}

/// W10 -- `MetricSeries`, Metric Length. Specification §Properties.Payloads
/// W10: the metric name is at most 350 bytes. Intake measures the byte length
/// inside `ValidateMetricName`, after the empty check and before the
/// alphabetic check. Returns the over-cap byte length when it exceeds the
/// bound, or `None` otherwise. The Agent does not validate the metric name, so
/// this is intake-only.
#[must_use]
pub fn w10_metric_name_too_long(series: &MetricSeries) -> Option<usize> {
    let len = series.metric().len();
    (len > MAX_METRIC_NAME_BYTES).then_some(len)
}

/// W11 -- `MetricSeries`, Metric Alphabetic. Specification §Properties.Payloads
/// W11: a metric name contains at least one ASCII alphabetic character.
/// Intake's `ValidateMetricName` scans the name byte by byte with `isAlpha`
/// ([A-Za-z]) and rejects a name with no alphabetic byte, the last of its
/// three metric-name checks. The scan is byte-wise, so a name of only digits,
/// punctuation, or multibyte UTF-8 carries no alphabetic byte. Returns whether
/// the name lacks an alphabetic byte. The empty name is rejected upstream by
/// W9, so the pipeline reaches this check only with a non-empty name.
#[must_use]
pub fn w11_metric_name_no_alpha(series: &MetricSeries) -> bool {
    !series.metric().bytes().any(|b| b.is_ascii_alphabetic())
}

/// W12 -- `MetricSeries`, Type Enum. Specification §Properties.Payloads W12:
/// the metric type is one of `COUNT`, `RATE`, or `GAUGE`. The Agent's
/// compile-time enum admits only those three, so any payload it sends carries
/// an in-domain type. Intake unmarshals the wire `int32` with no enum check
/// and admits any value, so W12 is observation-only. The proto3 wire default 0
/// is `UNSPECIFIED`, itself outside the accepted set, so a series that never
/// set the field fires. Returns the out-of-domain wire value, or `None` when
/// the type is one of the three.
#[must_use]
pub fn w12_type_out_of_domain(series: &MetricSeries) -> Option<i32> {
    let in_domain = matches!(
        series.type_.enum_value(),
        Ok(MetricType::COUNT | MetricType::RATE | MetricType::GAUGE)
    );
    (!in_domain).then_some(series.type_.value())
}

/// The two tag prefixes the Agent reserves for resource promotion. A `device:`
/// tag promotes to a `(type='device', name=value)` resource and a
/// `dd.internal.resource:type:name` tag promotes to a `(type, name)` resource.
/// The Agent strips both before sending, so a survivor on the wire is the W14
/// observation.
const RESERVED_TAG_PREFIXES: [&str; 2] = ["device:", "dd.internal.resource:"];

/// W14 -- `MetricSeries`, Tag Prefix Reserved. Specification §Properties.Payloads
/// W14: no tag starts with `device:` or `dd.internal.resource:`. The Agent
/// strips both prefixes pre-send, promoting the tag to a resource, so any
/// payload it sends carries none. Intake parses survivors as routing metadata
/// with no rejection, so W14 is observation-only. Returns the first
/// reserved-prefix tag, or `None` when no tag carries a reserved prefix.
#[must_use]
pub fn w14_reserved_tag_prefix(series: &MetricSeries) -> Option<&str> {
    series
        .tags
        .iter()
        .find(|tag| RESERVED_TAG_PREFIXES.iter().any(|prefix| tag.starts_with(prefix)))
        .map(String::as_str)
}

/// W15 -- `MetricSeries`, Per-Series Point Count. Specification
/// §Properties.Payloads W15: a series carries at most
/// `serializer_max_series_points_per_payload` points, the same cap W8 totals
/// across the whole payload, default 10 000. The Agent drops a series whose
/// point count exceeds the cap, so any payload it sends carries none over.
/// Intake does not re-validate, so W15 is observation-only. Returns the
/// over-cap point count, or `None` when the count is at or below the cap.
#[must_use]
pub fn w15_point_count(series: &MetricSeries, max_points: usize) -> Option<usize> {
    let count = series.points.len();
    (count > max_points).then_some(count)
}

/// The origin field W16 found out of domain. The handler names it in the
/// triage detail so a reader sees which of the three fields the Agent put out
/// of domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OriginField {
    /// `MetricSeries.metadata.origin.origin_product`.
    Product,
    /// `MetricSeries.metadata.origin.origin_category`.
    Category,
    /// `MetricSeries.metadata.origin.origin_service`.
    Service,
}

impl OriginField {
    /// The proto field name, for the triage detail.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            OriginField::Product => "origin_product",
            OriginField::Category => "origin_category",
            OriginField::Service => "origin_service",
        }
    }
}

/// A field value is in domain when it is at or below its enum maximum and the
/// proto does not mark it `reserved`. Value `0` is each enum's default
/// no-origin member and sits below the maximum, so it is in domain.
fn origin_field_in_domain(value: u32, max: u32, reserved: &[u32]) -> bool {
    value <= max && !reserved.contains(&value)
}

/// W16 -- `MetricSeries`, Origin Populated. Specification §Properties.Payloads
/// W16: each populated `Origin` field is a defined enum member. The Agent
/// derives all three from the metric source through a fixed map over the
/// `OriginProduct`, `OriginSubProduct`, and `OriginProductDetail` members, so a
/// series it built carries only defined values, and intake backfills an absent
/// `Origin` without validating the domain. W16 is observation-only. A series
/// with no `Metadata` or no `Origin` holds. Returns the first out-of-domain
/// field and its wire value, checking product then category then service, or
/// `None` when every populated field is in domain.
#[must_use]
pub fn w16_origin_out_of_domain(series: &MetricSeries) -> Option<(OriginField, u32)> {
    let origin = series.metadata.as_ref()?.origin.as_ref()?;
    if !origin_field_in_domain(origin.origin_product, ORIGIN_PRODUCT_MAX, &[]) {
        return Some((OriginField::Product, origin.origin_product));
    }
    if !origin_field_in_domain(origin.origin_category, ORIGIN_CATEGORY_MAX, &ORIGIN_CATEGORY_RESERVED) {
        return Some((OriginField::Category, origin.origin_category));
    }
    if !origin_field_in_domain(origin.origin_service, ORIGIN_SERVICE_MAX, &ORIGIN_SERVICE_RESERVED) {
        return Some((OriginField::Service, origin.origin_service));
    }
    None
}

/// Why W17 found the host intake resolves is not the Agent hostname. The
/// handler names it in the triage detail so a reader sees whether the host
/// resource was absent or merely misnamed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostResolution<'a> {
    /// No resource has `type` `"host"`, so `Host()` resolves to `""`.
    Missing,
    /// A host resource resolved, but its name is not the Agent hostname.
    /// Carries the resolved name, which may be empty when the series wrote a
    /// host resource with an empty name.
    Mismatch(&'a str),
}

impl<'a> HostResolution<'a> {
    /// A triage-friendly view of the resolution: a stable kind string and the
    /// resolved host name when one was present. `Missing` means no `type=host`
    /// resource at all. `Mismatch` carries the resolved name, which is the empty
    /// string when the series wrote a host resource without a name. The handler
    /// puts both in the assertion detail so a reader can tell an omitted host
    /// resource from one named something other than the expected hostname.
    #[must_use]
    pub fn as_detail(self) -> (&'static str, Option<&'a str>) {
        match self {
            HostResolution::Missing => ("missing", None),
            HostResolution::Mismatch(name) => ("mismatch", Some(name)),
        }
    }
}

/// W17 -- Resource, Host Resource Resolved. Specification §Properties.Payloads
/// W17: intake's `Host()` scan resolves a host resource named the Agent
/// hostname. Intake reads the host via [`host_resource`], the first resource
/// whose `type` is `"host"`, ignoring position. The Agent writes that resource
/// named its configured hostname, so a series it built resolves to `expected`.
/// W17 is observation-only. Returns [`HostResolution::Missing`] when no
/// resource is host-typed, [`HostResolution::Mismatch`] with the resolved name
/// when it is not `expected`, or `None` when the resolved host name equals
/// `expected`.
#[must_use]
pub fn w17_host_unresolved<'a>(series: &'a MetricSeries, expected: &str) -> Option<HostResolution<'a>> {
    match host_resource(series) {
        None => Some(HostResolution::Missing),
        Some(host) if host.name() != expected => Some(HostResolution::Mismatch(host.name())),
        Some(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use datadog_protos::metrics::metric_payload::{MetricSeries, MetricType, Resource};
    use datadog_protos::metrics::{Metadata, Origin};

    use super::*;

    fn host_res(name: &str) -> Resource {
        let mut r = Resource::new();
        r.set_type("host".into());
        r.set_name(name.into());
        r
    }

    #[test]
    fn w9_w10_w11_metric_name_checks() {
        let mut s = MetricSeries::new();
        assert!(!w9_metric_non_empty(&s));
        s.set_metric("system.cpu.idle".into());
        assert!(w9_metric_non_empty(&s));
        assert!(w10_metric_name_too_long(&s).is_none());
        assert!(!w11_metric_name_no_alpha(&s));

        s.set_metric("a".repeat(351));
        assert_eq!(w10_metric_name_too_long(&s), Some(351));

        s.set_metric("123.456".into());
        assert!(w11_metric_name_no_alpha(&s));
    }

    #[test]
    fn w12_unspecified_is_out_of_domain() {
        let mut s = MetricSeries::new();
        // proto3 default is UNSPECIFIED (0), outside the accepted set.
        assert_eq!(w12_type_out_of_domain(&s), Some(0));
        s.set_type(MetricType::GAUGE);
        assert!(w12_type_out_of_domain(&s).is_none());
    }

    #[test]
    fn w13_w15_w18_counts() {
        let mut s = MetricSeries::new();
        for i in 0..101 {
            s.tags.push(format!("k{i}:v"));
        }
        assert_eq!(w13_tag_count(&s, 100), Some(101));
        assert!(w13_tag_count(&s, 200).is_none());

        let mut s2 = MetricSeries::new();
        s2.resources.push(host_res("h"));
        assert!(w18_resource_count(&s2, 500).is_none());
    }

    #[test]
    fn w14_reserved_prefix_detected() {
        let mut s = MetricSeries::new();
        s.tags.push("env:prod".into());
        assert!(w14_reserved_tag_prefix(&s).is_none());
        s.tags.push("device:eth0".into());
        assert_eq!(w14_reserved_tag_prefix(&s), Some("device:eth0"));
    }

    #[test]
    fn w17_w19_host_resolution() {
        let mut s = MetricSeries::new();
        // No host resource resolves to Missing.
        assert_eq!(w17_host_unresolved(&s, "agent"), Some(HostResolution::Missing));

        assert_eq!(HostResolution::Missing.as_detail(), ("missing", None));

        s.resources.push(host_res("other"));
        assert_eq!(
            w17_host_unresolved(&s, "agent"),
            Some(HostResolution::Mismatch("other"))
        );

        // An empty host name resolves to Mismatch(""), not Missing: the resource
        // exists, it just has no name. The detail preserves that distinction.
        let mut s_empty = MetricSeries::new();
        s_empty.resources.push(host_res(""));
        assert_eq!(
            w17_host_unresolved(&s_empty, "agent").map(HostResolution::as_detail),
            Some(("mismatch", Some("")))
        );

        let mut s2 = MetricSeries::new();
        s2.resources.push(host_res("agent"));
        assert!(w17_host_unresolved(&s2, "agent").is_none());
        assert!(w19_host_name_too_long(&s2).is_none());

        let mut s3 = MetricSeries::new();
        s3.resources.push(host_res(&"h".repeat(256)));
        assert!(w19_host_name_too_long(&s3).is_some());
    }

    #[test]
    fn w16_origin_domain_checks() {
        let mut s = MetricSeries::new();
        // No metadata holds.
        assert!(w16_origin_out_of_domain(&s).is_none());

        let mut origin = Origin::new();
        origin.origin_product = ORIGIN_PRODUCT_MAX + 1;
        let mut meta = Metadata::new();
        meta.origin = Some(origin).into();
        s.metadata = Some(meta).into();
        assert_eq!(
            w16_origin_out_of_domain(&s),
            Some((OriginField::Product, ORIGIN_PRODUCT_MAX + 1))
        );

        // A reserved category ordinal is out of domain even below the max.
        let mut origin = Origin::new();
        origin.origin_category = ORIGIN_CATEGORY_RESERVED[0];
        let mut meta = Metadata::new();
        meta.origin = Some(origin).into();
        let mut s2 = MetricSeries::new();
        s2.metadata = Some(meta).into();
        assert_eq!(
            w16_origin_out_of_domain(&s2),
            Some((OriginField::Category, ORIGIN_CATEGORY_RESERVED[0]))
        );
    }
}
