//! Metric name filterlist compilation, namespace prefixing, and post-aggregate histogram filtering.

use saluki_core::data_model::event::metric::{Metric, MetricValues};
use serde::{Deserialize, Serialize};
use stringtheory::MetaString;

use crate::histogram::HistogramConfiguration;
use crate::metric_name::{is_normalized, normalize_into, normalize_prefix_into, NameBuf};

/// Metric names or prefixes that a metric filterlist drops.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct MetricNameFilter {
    /// Metric names or prefixes that are dropped.
    pub values: Vec<String>,

    /// Whether entries in `values` match by prefix instead of exact name.
    pub match_prefix: bool,

    /// Additional prefix rules with exceptions shared across all prefix rules.
    pub prefix_rules: Vec<MetricPrefixRule>,
}

impl MetricNameFilter {
    /// Returns `true` when no filter entries or prefix rules are configured.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty() && self.prefix_rules.is_empty()
    }
}

/// A metric-name prefix to drop, with exceptions shared across all prefix rules.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct MetricPrefixRule {
    /// Prefix to drop. An empty prefix matches every stored metric name.
    pub prefix: String,

    /// Prefixes to keep, unless blocked by unconditional filter entries.
    pub except_prefix: Vec<String>,

    /// Exact names to keep, unless blocked by unconditional filter entries.
    pub except_exact: Vec<String>,
}

/// Compiled blocklist for metric names that should be filtered.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Blocklist {
    exact: Vec<String>,
    prefixes: Vec<String>,
    rule_prefixes: Vec<String>,
    except_exact: Vec<String>,
    except_prefix: Vec<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct PrefixRule {
    prefix: String,
    except_exact: Vec<String>,
    except_prefix: Vec<String>,
}

/// Diagnostics produced when compiling a [`MetricNameFilter`].
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CompilationReport {
    /// Entries dropped because no stored metric name can match them.
    pub dropped_entries: Vec<String>,
    /// Prefix rules dropped because their prefixes cannot match any stored metric name.
    pub dropped_rules: Vec<String>,
    /// Prefix-rule exceptions dropped because they cannot match any stored metric name.
    pub dropped_exceptions: Vec<String>,
    /// Prefix rules shadowed by an unconditional prefix entry.
    pub shadowed_rules: Vec<String>,
}

/// Compiles a [`MetricNameFilter`] into a [`Blocklist`] and a [`CompilationReport`].
pub fn compile_metric_filter(metric_filter: &MetricNameFilter) -> (Blocklist, CompilationReport) {
    let (normalized_entries, dropped_entries) = normalize_entries(&metric_filter.values, metric_filter.match_prefix);
    let (normalized_rules, dropped_rules, dropped_exceptions) = normalize_prefix_rules(&metric_filter.prefix_rules);

    let (matcher, shadowed_rules) = Blocklist::new_with_prefix_rules(
        normalized_entries.iter().map(String::as_str),
        metric_filter.match_prefix,
        &normalized_rules,
    );

    (
        matcher,
        CompilationReport {
            dropped_entries,
            dropped_rules,
            dropped_exceptions,
            shadowed_rules,
        },
    )
}

impl Blocklist {
    /// Creates a matcher from already-normalized entries without prefix rules.
    pub fn from_normalized<T, I>(values: I, match_prefix: bool) -> Self
    where
        T: AsRef<str>,
        I: IntoIterator<Item = T>,
    {
        let (matcher, _) = Self::new_with_prefix_rules(values, match_prefix, &[]);
        matcher
    }

    fn new_with_prefix_rules<T, I>(values: I, match_prefix: bool, rules: &[PrefixRule]) -> (Self, Vec<String>)
    where
        T: AsRef<str>,
        I: IntoIterator<Item = T>,
    {
        let mut exact = Vec::new();
        let mut prefixes = Vec::new();
        for value in values {
            if match_prefix {
                prefixes.push(value.as_ref().to_string());
            } else {
                exact.push(value.as_ref().to_string());
            }
        }

        let mut rule_prefixes = Vec::with_capacity(rules.len());
        let mut except_exact = Vec::new();
        let mut except_prefix = Vec::new();
        for rule in rules {
            rule_prefixes.push(rule.prefix.clone());
            except_exact.extend(rule.except_exact.iter().cloned());
            except_prefix.extend(rule.except_prefix.iter().cloned());
        }

        prefixes = compact_prefixes(prefixes);
        exact = compact_exact(exact, &prefixes);
        except_prefix = compact_prefixes(except_prefix);
        except_exact = compact_exact(except_exact, &except_prefix);

        let mut shadowed_rules = Vec::new();
        if !prefixes.is_empty() {
            rule_prefixes.retain(|prefix| {
                let shadowed = test_prefixes(&prefixes, prefix.as_bytes());
                if shadowed {
                    shadowed_rules.push(prefix.clone());
                }
                !shadowed
            });
        }
        rule_prefixes = compact_prefixes(rule_prefixes);

        (
            Self {
                exact,
                prefixes,
                rule_prefixes,
                except_exact,
                except_prefix,
            },
            shadowed_rules,
        )
    }

    /// Returns whether `name` matches the blocklist after normalization.
    pub fn contains(&self, name: &str) -> bool {
        if self.is_empty() {
            return false;
        }

        if is_normalized(name) {
            return self.search(name.as_bytes());
        }

        let mut buf = NameBuf::new();
        match normalize_into(&mut buf, name) {
            Some(normalized) => self.search(normalized),
            None => false,
        }
    }

    /// Returns whether `name` is blocked by a prefix rule while having an exception below its namespace.
    ///
    /// This preserves histogram inputs that are needed to produce excepted post-aggregate series (for example,
    /// preserving `foo` when `foo.avg` is excepted).
    pub fn has_descendant_exception(&self, name: &str) -> bool {
        if self.rule_prefixes.is_empty() || (self.except_exact.is_empty() && self.except_prefix.is_empty()) {
            return false;
        }

        if is_normalized(name) {
            return self.has_descendant_exception_normalized(name.as_bytes());
        }

        let mut buf = NameBuf::new();
        match normalize_into(&mut buf, name) {
            Some(normalized) => self.has_descendant_exception_normalized(normalized),
            None => false,
        }
    }

    fn has_descendant_exception_normalized(&self, name: &[u8]) -> bool {
        if binary_search_exact(&self.exact, name) || test_prefixes(&self.prefixes, name) {
            return false;
        }

        if !test_prefixes(&self.rule_prefixes, name)
            || test_prefixes(&self.except_prefix, name)
            || binary_search_exact(&self.except_exact, name)
        {
            return false;
        }

        let has_descendant = |exceptions: &[String]| {
            let index = exceptions
                .partition_point(|candidate| candidate.bytes().cmp(name.iter().copied().chain([b'.'])).is_lt());

            exceptions.get(index).is_some_and(|candidate| {
                candidate
                    .as_bytes()
                    .strip_prefix(name)
                    .is_some_and(|suffix| suffix.starts_with(b"."))
            })
        };

        has_descendant(&self.except_exact) || has_descendant(&self.except_prefix)
    }

    /// Restricts exact entries while leaving all prefix behavior unchanged.
    pub fn restrict_exact(&self, keep: impl Fn(&str) -> bool) -> Self {
        let exact = self
            .exact
            .iter()
            .filter(|entry| keep(entry.as_str()))
            .cloned()
            .collect::<Vec<_>>();

        Self {
            exact,
            prefixes: self.prefixes.clone(),
            rule_prefixes: self.rule_prefixes.clone(),
            except_exact: self.except_exact.clone(),
            except_prefix: self.except_prefix.clone(),
        }
    }

    /// Returns the number of compiled entries.
    pub fn len(&self) -> usize {
        self.exact.len() + self.prefixes.len() + self.rule_prefixes.len()
    }

    /// Returns `true` when the blocklist has no compiled entries.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns whether the blocklist matches every stored metric name.
    pub fn matches_all(&self) -> bool {
        if self.prefixes.len() == 1 && self.prefixes[0].is_empty() {
            return true;
        }

        self.rule_prefixes.len() == 1
            && self.rule_prefixes[0].is_empty()
            && self.except_exact.is_empty()
            && self.except_prefix.is_empty()
    }

    fn search(&self, name: &[u8]) -> bool {
        if binary_search_exact(&self.exact, name) {
            return true;
        }

        if test_prefixes(&self.prefixes, name) {
            return true;
        }

        if !test_prefixes(&self.rule_prefixes, name) {
            return false;
        }

        if test_prefixes(&self.except_prefix, name) {
            return false;
        }

        !binary_search_exact(&self.except_exact, name)
    }
}

/// Pure kernel that applies a metric namespace prefix and a listener-side metric blocklist.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PrefixFilterKernel {
    metric_prefix: String,
    metric_prefix_blocklist: Vec<String>,
    matcher: Blocklist,
}

impl PrefixFilterKernel {
    /// Creates a new `PrefixFilterKernel`.
    ///
    /// A non-empty `metric_prefix` without a trailing period receives one so metric processing does not re-check it.
    pub fn new(metric_prefix: String, metric_prefix_blocklist: Vec<String>, matcher: Blocklist) -> Self {
        let mut normalized_prefix = metric_prefix;
        if !normalized_prefix.is_empty() && !normalized_prefix.ends_with('.') {
            normalized_prefix.push('.');
        }

        Self {
            metric_prefix: normalized_prefix,
            metric_prefix_blocklist,
            matcher,
        }
    }

    /// Creates a new `PrefixFilterKernel` by compiling `metric_filter`.
    pub fn from_filter(
        metric_prefix: String, metric_prefix_blocklist: Vec<String>, metric_filter: &MetricNameFilter,
    ) -> (Self, CompilationReport) {
        let (matcher, report) = compile_metric_filter(metric_filter);
        (Self::new(metric_prefix, metric_prefix_blocklist, matcher), report)
    }

    /// Replaces the compiled blocklist.
    pub fn set_matcher(&mut self, matcher: Blocklist) {
        self.matcher = matcher;
    }

    /// Returns a reference to the compiled blocklist.
    pub fn matcher(&self) -> &Blocklist {
        &self.matcher
    }

    /// Returns `true` when this kernel neither adds a prefix nor drops any metric.
    pub fn is_noop(&self) -> bool {
        self.metric_prefix.is_empty() && self.matcher.is_empty()
    }

    /// Applies namespace prefixing and blocklist filtering to `metric`.
    ///
    /// Returns `true` if `metric` is kept, or `false` if it is dropped by the blocklist. Metrics whose name does not
    /// change keep their original [`saluki_core::data_model::event::metric::context::Context`] untouched.
    pub fn apply(&self, metric: &mut Metric) -> bool {
        let original_name = metric.context().name();
        if self.metric_prefix.is_empty() || self.has_excluded_prefix(original_name.as_ref()) {
            return !self.is_filtered(metric, original_name.as_ref());
        }

        let mut prefixed_metric_name = self.metric_prefix.clone();
        prefixed_metric_name.push_str(original_name.as_ref());
        let new_metric_name = MetaString::from(prefixed_metric_name);

        if self.is_filtered(metric, &new_metric_name) {
            return false;
        }

        let new_context = metric.context().with_name(new_metric_name);
        *metric.context_mut() = new_context;

        true
    }

    fn is_filtered(&self, metric: &Metric, name: &str) -> bool {
        if !self.matcher.contains(name) {
            return false;
        }

        if matches!(metric.values(), MetricValues::Histogram(_)) && self.matcher.has_descendant_exception(name) {
            return false;
        }

        true
    }

    fn has_excluded_prefix(&self, metric_name: &str) -> bool {
        !self.metric_prefix.is_empty()
            && self
                .metric_prefix_blocklist
                .iter()
                .any(|prefix| metric_name.starts_with(prefix))
    }
}

/// Suffixes produced when histograms are expanded into scalar aggregate series.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HistogramSuffixes {
    values: Vec<MetaString>,
}

impl HistogramSuffixes {
    /// Builds the suffixes produced by the given histogram aggregates and percentiles.
    pub fn new(aggregates: &[String], percentiles: &[String]) -> Self {
        let mut values = aggregates
            .iter()
            .map(|aggregate| MetaString::from(aggregate.as_str()))
            .collect::<Vec<_>>();
        values.extend(HistogramConfiguration::percentile_suffixes(percentiles));

        Self { values }
    }

    /// Returns the compiled suffix list.
    pub fn values(&self) -> &[MetaString] {
        &self.values
    }

    /// Returns whether `value` targets a generated histogram aggregate output (`<metric>.<suffix>`).
    pub fn contains_filter_entry(&self, value: &str) -> bool {
        self.values.iter().any(|suffix| {
            let suffix: &str = suffix.as_ref();
            value
                .strip_suffix(suffix)
                .map(|prefix| prefix.ends_with('.'))
                .unwrap_or(false)
        })
    }
}

/// Pure kernel that drops post-aggregation scalar metrics matching histogram-aggregate filter entries.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PostAggregateFilterKernel {
    matcher: Blocklist,
    histogram_suffixes: HistogramSuffixes,
}

impl PostAggregateFilterKernel {
    /// Creates a new `PostAggregateFilterKernel` from a metric filter and histogram output suffixes.
    pub fn new(
        metric_filter: &MetricNameFilter, histogram_aggregates: &[String], histogram_percentiles: &[String],
    ) -> Self {
        let histogram_suffixes = HistogramSuffixes::new(histogram_aggregates, histogram_percentiles);
        let mut kernel = Self {
            matcher: Blocklist::default(),
            histogram_suffixes,
        };
        kernel.update_filter(metric_filter);
        kernel
    }

    /// Recompiles the matcher from `metric_filter`, keeping only exact entries that match a histogram suffix.
    pub fn update_filter(&mut self, metric_filter: &MetricNameFilter) {
        let (matcher, _) = compile_metric_filter(metric_filter);
        self.matcher = matcher.restrict_exact(|value| self.histogram_suffixes.contains_filter_entry(value));
    }

    /// Returns `true` when this kernel drops no metrics.
    pub fn is_noop(&self) -> bool {
        self.matcher.is_empty()
    }

    /// Returns `true` if `metric` should be dropped.
    pub fn should_filter(&self, metric: &Metric) -> bool {
        is_scalar_series_metric(metric) && self.matcher.contains(metric.context().name())
    }

    /// Returns `true` if `metric` should be kept.
    pub fn should_keep(&self, metric: &Metric) -> bool {
        !self.should_filter(metric)
    }
}

fn is_scalar_series_metric(metric: &Metric) -> bool {
    matches!(
        metric.values(),
        MetricValues::Counter(_) | MetricValues::Rate(_, _) | MetricValues::Gauge(_) | MetricValues::Set(_)
    )
}

fn normalize_entries(entries: &[String], match_prefix: bool) -> (Vec<String>, Vec<String>) {
    let mut normalized = Vec::with_capacity(entries.len());
    let mut dropped = Vec::new();
    let mut buf = NameBuf::new();

    for entry in entries {
        let key = if match_prefix {
            normalize_prefix_into(&mut buf, entry)
        } else {
            normalize_into(&mut buf, entry)
        };

        match key {
            Some(key) => normalized.push(
                simdutf8::basic::from_utf8(key)
                    .expect("normalized metric names are ASCII")
                    .to_owned(),
            ),
            None => dropped.push(entry.clone()),
        }
    }

    (normalized, dropped)
}

fn normalize_prefix_rules(rules: &[MetricPrefixRule]) -> (Vec<PrefixRule>, Vec<String>, Vec<String>) {
    let mut normalized = Vec::with_capacity(rules.len());
    let mut dropped_rules = Vec::new();
    let mut dropped_exceptions = Vec::new();
    let mut buf = NameBuf::new();

    for rule in rules {
        let Some(prefix) = normalize_prefix_into(&mut buf, &rule.prefix) else {
            dropped_rules.push(rule.prefix.clone());
            continue;
        };

        let mut normalized_rule = PrefixRule {
            prefix: simdutf8::basic::from_utf8(prefix)
                .expect("normalized metric names are ASCII")
                .to_owned(),
            except_exact: Vec::new(),
            except_prefix: Vec::new(),
        };

        for exact in &rule.except_exact {
            match normalize_into(&mut buf, exact) {
                Some(key) => normalized_rule.except_exact.push(
                    simdutf8::basic::from_utf8(key)
                        .expect("normalized metric names are ASCII")
                        .to_owned(),
                ),
                None => dropped_exceptions.push(exact.clone()),
            }
        }

        for prefix in &rule.except_prefix {
            match normalize_prefix_into(&mut buf, prefix) {
                Some(key) => normalized_rule.except_prefix.push(
                    simdutf8::basic::from_utf8(key)
                        .expect("normalized metric names are ASCII")
                        .to_owned(),
                ),
                None => dropped_exceptions.push(prefix.clone()),
            }
        }

        normalized.push(normalized_rule);
    }

    (normalized, dropped_rules, dropped_exceptions)
}

fn compact_prefixes(mut prefixes: Vec<String>) -> Vec<String> {
    if prefixes.is_empty() {
        return Vec::new();
    }

    prefixes.sort_unstable();

    let mut compacted = Vec::with_capacity(prefixes.len());
    for prefix in prefixes {
        if compacted.last().map(|kept| prefix.starts_with(kept)).unwrap_or(false) {
            continue;
        }
        compacted.push(prefix);
    }

    compacted
}

fn compact_exact(mut exact: Vec<String>, prefixes: &[String]) -> Vec<String> {
    if exact.is_empty() {
        return Vec::new();
    }

    exact.sort_unstable();
    exact.dedup();

    if prefixes.is_empty() {
        return exact;
    }

    exact.retain(|name| !test_prefixes(prefixes, name.as_bytes()));
    exact
}

fn binary_search_exact(entries: &[String], name: &[u8]) -> bool {
    entries
        .binary_search_by(|candidate| candidate.as_bytes().cmp(name))
        .is_ok()
}

fn test_prefixes(prefixes: &[String], name: &[u8]) -> bool {
    if prefixes.is_empty() {
        return false;
    }

    let index = prefixes
        .binary_search_by(|candidate| candidate.as_bytes().cmp(name))
        .unwrap_or_else(|idx| idx);

    if index > 0 && name.starts_with(prefixes[index - 1].as_bytes()) {
        return true;
    }

    index < prefixes.len() && prefixes[index].as_bytes() == name
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_filter_kernel_prepends_and_filters() {
        let filter = MetricNameFilter {
            values: vec!["app.blocked".to_string()],
            match_prefix: false,
            prefix_rules: vec![],
        };
        let (kernel, _) = PrefixFilterKernel::from_filter("app".to_string(), vec!["datadog.".to_string()], &filter);

        let mut allowed = Metric::gauge("requests", 1.0);
        assert!(kernel.apply(&mut allowed));
        assert_eq!(allowed.context().name(), "app.requests");

        let mut excluded_prefix = Metric::gauge("datadog.agent.running", 1.0);
        let original = excluded_prefix.context().clone();
        assert!(kernel.apply(&mut excluded_prefix));
        assert!(original.ptr_eq(excluded_prefix.context()));
        assert_eq!(excluded_prefix.context().name(), "datadog.agent.running");

        let mut blocked = Metric::gauge("blocked", 1.0);
        assert!(!kernel.apply(&mut blocked));
    }

    #[test]
    fn prefix_filter_keeps_histogram_with_descendant_exception() {
        let filter = MetricNameFilter {
            values: vec![],
            match_prefix: false,
            prefix_rules: vec![MetricPrefixRule {
                prefix: "service.".to_string(),
                except_exact: vec!["service.latency.avg".to_string()],
                except_prefix: vec![],
            }],
        };
        let (kernel, _) = PrefixFilterKernel::from_filter(String::new(), vec![], &filter);

        let mut hist = Metric::histogram("service.latency", 42.0);
        assert!(kernel.apply(&mut hist));

        let mut gauge = Metric::gauge("service.latency", 42.0);
        assert!(!kernel.apply(&mut gauge));
    }

    #[test]
    fn post_aggregate_filter_kernel_filters_only_histogram_suffix_exact_entries() {
        let filter = MetricNameFilter {
            values: vec!["req.latency.avg".to_string(), "req.other".to_string()],
            match_prefix: false,
            prefix_rules: vec![],
        };
        let kernel = PostAggregateFilterKernel::new(&filter, &["avg".to_string()], &["0.95".to_string()]);

        assert!(kernel.should_filter(&Metric::gauge("req.latency.avg", 1.0)));
        assert!(!kernel.should_filter(&Metric::gauge("req.other", 1.0)));
        assert!(!kernel.should_filter(&Metric::distribution("req.latency.avg", 1.0)));
    }

    fn rule(prefix: &str, except_exact: &[&str], except_prefix: &[&str]) -> MetricPrefixRule {
        MetricPrefixRule {
            prefix: prefix.to_string(),
            except_exact: except_exact.iter().map(ToString::to_string).collect(),
            except_prefix: except_prefix.iter().map(ToString::to_string).collect(),
        }
    }

    #[test]
    fn compiles_entries_verbatim() {
        let compiled = |values: &[&str]| Blocklist::from_normalized(values.iter().copied(), true).prefixes;

        assert_eq!(compiled(&[]), Vec::<String>::new());
        assert_eq!(compiled(&["a"]), vec!["a"]);
        assert_eq!(compiled(&["a", "aa"]), vec!["a"]);
        assert_eq!(compiled(&["a", "aa", "b", "bb"]), vec!["a", "b"]);
        assert_eq!(compiled(&["a", "b", "bb"]), vec!["a", "b"]);
        assert_eq!(compiled(&["a-b", "a_b"]), vec!["a-b", "a_b"]);
    }

    #[test]
    fn matching_normalizes_the_metric_name() {
        let cases: &[(bool, &str, &[&str], bool)] = &[
            (true, "my metric-name", &["my_metric_name"], false),
            (true, "custom.metric one", &["custom.metric_one"], false),
            (true, "host.cpu%util", &["host.cpu_util"], false),
            (true, "1app.requests", &["app.requests"], false),
            (true, "café.requests", &["caf.requests"], false),
            (false, "my_metric_name", &["my metric-name"], false),
            (true, "custom.metric name.count", &["custom.metric_name"], true),
            (false, "", &["foo"], false),
            (false, "123", &["foo"], false),
        ];

        for (expected, name, entries, match_prefix) in cases {
            let blocklist = Blocklist::from_normalized(entries.iter().copied(), *match_prefix);
            assert_eq!(blocklist.contains(name), *expected, "name: {:?}", name);
        }
    }

    #[test]
    fn prefix_rule_exceptions_and_shadowing() {
        let metric_filter = MetricNameFilter {
            values: vec!["postgresql.".to_string()],
            match_prefix: true,
            prefix_rules: vec![rule("postgresql.locks.", &["postgresql.locks.waiting"], &[])],
        };
        let (matcher, report) = compile_metric_filter(&metric_filter);
        assert_eq!(report.shadowed_rules, vec!["postgresql.locks.".to_string()]);
        assert!(matcher.contains("postgresql.locks.waiting"));
    }

    #[test]
    fn compile_metric_filter_drops_unusable_entries_and_preserves_prefix_boundaries() {
        let metric_filter = MetricNameFilter {
            values: vec!["service_".to_string(), "123".to_string()],
            match_prefix: true,
            prefix_rules: vec![
                rule("123", &[], &[]),
                rule("foo.", &["123", "foo.keep"], &["123", "foo.skip."]),
            ],
        };
        let (matcher, report) = compile_metric_filter(&metric_filter);

        assert_eq!(report.dropped_entries, vec!["123".to_string()]);
        assert_eq!(report.dropped_rules, vec!["123".to_string()]);
        assert_eq!(report.dropped_exceptions, vec!["123".to_string(), "123".to_string()]);
        assert!(matcher.contains("service_requests"));
        assert!(!matcher.contains("service.requests"));
        assert!(matcher.contains("foo.anything"));
        assert!(!matcher.contains("foo.keep"));
        assert!(!matcher.contains("foo.skip.waiting"));
    }
}
