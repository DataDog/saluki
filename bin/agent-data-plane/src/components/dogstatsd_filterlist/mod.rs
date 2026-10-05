//! Shared DogStatsD metric filterlist matcher.

use agent_data_plane_config::domains::dogstatsd::{MetricFilter, MetricPrefixRule};

use super::metric_name::{is_normalized, normalize_into, normalize_prefix_into, NameBuf};

/// Compiled blocklist for metric names that should be filtered.
///
/// `metric_filterlist` entries are exact names by default. They are treated as prefixes only when
/// the global `metric_filterlist_match_prefix` setting is enabled.
///
/// Per-entry prefix rules (and their exceptions) come from `metric_filterlist_prefix`. They do not
/// come from `metric_filterlist`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct Blocklist {
    // Sorted, deduplicated, and not covered by `prefixes`.
    exact: Vec<String>,
    // Sorted, deduplicated, and compacted prefixes from `metric_filterlist` when
    // `metric_filterlist_match_prefix` is enabled.
    prefixes: Vec<String>,
    // Sorted, deduplicated, and compacted prefixes from `metric_filterlist_prefix`.
    rule_prefixes: Vec<String>,
    // Global exceptions from `metric_filterlist_prefix`.
    except_exact: Vec<String>,
    except_prefix: Vec<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct PrefixRule {
    prefix: String,
    except_exact: Vec<String>,
    except_prefix: Vec<String>,
}

/// Diagnostics from compiling a runtime metric filter.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct CompilationReport {
    /// `metric_filterlist` entries dropped because no stored metric name can match them.
    pub dropped_entries: Vec<String>,
    /// `metric_filterlist_prefix` rules dropped because their prefixes cannot match any stored
    /// metric name.
    pub dropped_rules: Vec<String>,
    /// `metric_filterlist_prefix` exceptions dropped because they cannot match any stored metric
    /// name.
    pub dropped_exceptions: Vec<String>,
    /// `metric_filterlist_prefix` rules shadowed by an unconditional global prefix from
    /// `metric_filterlist` when `metric_filterlist_match_prefix` is enabled.
    pub shadowed_rules: Vec<String>,
}

/// Compiles a runtime [`MetricFilter`] into a matcher.
pub(super) fn compile_metric_filter(metric_filter: &MetricFilter) -> (Blocklist, CompilationReport) {
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
    /// Creates a matcher for `metric_filterlist` only.
    ///
    /// Entries are taken verbatim and are expected to already be normalized in the same namespace
    /// the intake stores metric names in.
    #[cfg(test)]
    pub(super) fn new<T, I>(values: I, match_prefix: bool) -> Self
    where
        T: AsRef<str>,
        I: IntoIterator<Item = T>,
    {
        let (matcher, _) = Self::new_with_prefix_rules(values, match_prefix, &[]);
        matcher
    }

    /// Creates a matcher from normalized `metric_filterlist` and `metric_filterlist_prefix`
    /// entries.
    ///
    /// Returns any `metric_filterlist_prefix` rules shadowed by an unconditional
    /// `metric_filterlist` prefix introduced by global `metric_filterlist_match_prefix` mode.
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

    /// Returns whether `name` matches a configured metric name.
    ///
    /// The name is normalized before being compared. Metric names arrive exactly as they were
    /// submitted, but the intake rewrites them on ingest: a raw name such as `my metric-name` is
    /// stored as `my_metric_name`, which is what users copy into a filterlist.
    pub(super) fn contains(&self, name: &str) -> bool {
        if self.len() == 0 {
            return false;
        }

        // Fast path: already normalized, so compare the name as given.
        if is_normalized(name) {
            return self.search(name.as_bytes());
        }

        let mut buf = NameBuf::new();
        match normalize_into(&mut buf, name) {
            Some(normalized) => self.search(normalized),
            None => false,
        }
    }

    /// Restricts exact entries and keeps all prefix behavior unchanged.
    pub(super) fn restrict_exact(&self, keep: impl Fn(&str) -> bool) -> Self {
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
    pub(super) fn len(&self) -> usize {
        self.exact.len() + self.prefixes.len() + self.rule_prefixes.len()
    }

    /// Returns whether the matcher blocks every storable metric name.
    pub(super) fn matches_all(&self) -> bool {
        if self.prefixes.len() == 1 && self.prefixes[0].is_empty() {
            return true;
        }

        self.rule_prefixes.len() == 1
            && self.rule_prefixes[0].is_empty()
            && self.except_exact.is_empty()
            && self.except_prefix.is_empty()
    }

    /// Looks `name` up in the compiled entries.
    ///
    /// `name` must already be normalized.
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

/// Normalizes raw `metric_filterlist` entries.
///
/// `match_prefix` corresponds to the global `metric_filterlist_match_prefix` setting: per-entry
/// prefixes are configured with `metric_filterlist_prefix` instead.
///
/// Dropped entries cannot match any metric name stored by the intake.
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
                std::str::from_utf8(key)
                    .expect("normalized metric names are ASCII")
                    .to_owned(),
            ),
            None => dropped.push(entry.clone()),
        }
    }

    (normalized, dropped)
}

/// Normalizes raw `metric_filterlist_prefix` rules.
///
/// Prefix and `except_prefix` entries normalize as prefixes; `except_exact` entries normalize as
/// full names.
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
            prefix: std::str::from_utf8(prefix)
                .expect("normalized metric names are ASCII")
                .to_owned(),
            except_exact: Vec::new(),
            except_prefix: Vec::new(),
        };

        for exact in &rule.except_exact {
            match normalize_into(&mut buf, exact) {
                Some(key) => normalized_rule.except_exact.push(
                    std::str::from_utf8(key)
                        .expect("normalized metric names are ASCII")
                        .to_owned(),
                ),
                None => dropped_exceptions.push(exact.clone()),
            }
        }

        for prefix in &rule.except_prefix {
            match normalize_prefix_into(&mut buf, prefix) {
                Some(key) => normalized_rule.except_prefix.push(
                    std::str::from_utf8(key)
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

/// Returns true if `name` starts with one of `prefixes`.
///
/// `prefixes` must be sorted and compacted by [`compact_prefixes`].
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

    fn rule(prefix: &str, except_exact: &[&str], except_prefix: &[&str]) -> MetricPrefixRule {
        MetricPrefixRule {
            prefix: prefix.to_string(),
            except_exact: except_exact.iter().map(ToString::to_string).collect(),
            except_prefix: except_prefix.iter().map(ToString::to_string).collect(),
        }
    }

    #[test]
    fn compiles_entries_verbatim() {
        let compiled = |values: &[&str]| Blocklist::new(values.iter().copied(), true).prefixes;

        assert_eq!(compiled(&[]), Vec::<String>::new());
        assert_eq!(compiled(&["a"]), vec!["a"]);
        assert_eq!(compiled(&["a", "aa"]), vec!["a"]);
        assert_eq!(compiled(&["a", "aa", "b", "bb"]), vec!["a", "b"]);
        assert_eq!(compiled(&["a", "b", "bb"]), vec!["a", "b"]);

        // Entries are taken verbatim. Normalization happens before compilation.
        assert_eq!(compiled(&["a-b", "a_b"]), vec!["a-b", "a_b"]);
    }

    #[test]
    fn prefix_entries_are_not_rewritten() {
        let blocklist = Blocklist::new(["redis.checkpoint_"], true);

        assert!(blocklist.contains("redis.checkpoint_bytes"));
        assert!(
            blocklist.contains("redis.checkpoint-bytes"),
            "raw name normalizes into the family"
        );

        assert!(!blocklist.contains("redis.checkpointing.count"));
        assert!(!blocklist.contains("redis.checkpointed"));
    }

    #[test]
    fn matching_normalizes_the_metric_name() {
        // (expected, name, entries, match_prefix)
        let cases: &[(bool, &str, &[&str], bool)] = &[
            (true, "my metric-name", &["my_metric_name"], false),
            (true, "custom.metric one", &["custom.metric_one"], false),
            (true, "host.cpu%util", &["host.cpu_util"], false),
            (true, "1app.requests", &["app.requests"], false),
            (true, "café.requests", &["caf.requests"], false),
            (true, "multiple-norm-1", &["multiple_norm_1"], false),
            (true, "multiple_norm-1", &["multiple_norm_1"], false),
            (false, "my_metric_name", &["my metric-name"], false),
            (false, "my metric-name", &["my-metric-name"], false),
            (false, "my.metric", &["my_metric"], false),
            (false, "other metric", &["my_metric"], false),
            (true, "custom.metric name.count", &["custom.metric_name"], true),
            (false, "custom.metric name.count", &["custom.other"], true),
            (false, "", &["foo"], false),
            (false, "123", &["foo"], false),
        ];

        for (expected, name, entries, match_prefix) in cases {
            let blocklist = Blocklist::new(entries.iter().copied(), *match_prefix);
            assert_eq!(
                blocklist.contains(name),
                *expected,
                "name: {:?}, entries: {:?}, match_prefix: {}",
                name,
                entries,
                match_prefix
            );
        }
    }

    #[test]
    fn overlong_names_never_match() {
        let name = "foo".repeat(200);

        assert!(!Blocklist::new(["foo"], true).contains(&name));
        assert!(!Blocklist::new([name.as_str()], false).contains(&name));
    }

    #[test]
    fn matches_exact_and_prefix_entries() {
        // (expected, name, entries, match_prefix)
        let cases: &[(bool, &str, &[&str], bool)] = &[
            (false, "some", &[], false),
            (false, "some", &[], true),
            (false, "foo", &["bar", "baz"], false),
            (false, "foo", &["bar", "baz"], true),
            (false, "bar", &["foo", "baz"], false),
            (false, "bar", &["foo", "baz"], true),
            (true, "baz", &["foo", "baz"], false),
            (true, "baz", &["foo", "baz"], true),
            (false, "foobar", &["foo", "baz"], false),
            (true, "foobar", &["foo", "baz"], true),
        ];

        for (expected, name, entries, match_prefix) in cases {
            let blocklist = Blocklist::new(entries.iter().copied(), *match_prefix);
            assert_eq!(
                blocklist.contains(name),
                *expected,
                "name: {:?}, entries: {:?}, match_prefix: {}",
                name,
                entries,
                match_prefix
            );
        }
    }

    #[test]
    fn prefix_rule_exceptions_apply_across_rules() {
        let rules = vec![
            PrefixRule {
                prefix: "foo.".to_string(),
                except_exact: vec!["foo.bar".to_string()],
                except_prefix: Vec::new(),
            },
            PrefixRule {
                prefix: "foo.b".to_string(),
                except_exact: Vec::new(),
                except_prefix: Vec::new(),
            },
        ];

        let (matcher, shadowed_rules) = Blocklist::new_with_prefix_rules(Vec::<String>::new(), false, &rules);
        assert!(shadowed_rules.is_empty());

        assert!(!matcher.contains("foo.bar"));
        assert!(matcher.contains("foo.baz"));
        assert!(matcher.contains("foo.other"));
    }

    #[test]
    fn shadowed_rules_are_dropped_when_global_prefixes_cover_them() {
        let rules = vec![PrefixRule {
            prefix: "postgresql.locks.".to_string(),
            except_exact: vec!["postgresql.locks.waiting".to_string()],
            except_prefix: Vec::new(),
        }];

        let (matcher, shadowed_rules) = Blocklist::new_with_prefix_rules(["postgresql."], true, &rules);

        assert_eq!(shadowed_rules, vec!["postgresql.locks.".to_string()]);
        assert!(matcher.rule_prefixes.is_empty());
        assert!(matcher.contains("postgresql.locks.waiting"));
    }

    #[test]
    fn matches_all_detects_unconditional_prefixes() {
        assert!(Blocklist::new([""], true).matches_all());

        let rules = vec![PrefixRule {
            prefix: "".to_string(),
            except_exact: Vec::new(),
            except_prefix: Vec::new(),
        }];
        let (matcher, _) = Blocklist::new_with_prefix_rules(Vec::<String>::new(), false, &rules);
        assert!(matcher.matches_all());

        let rules_with_exception = vec![PrefixRule {
            prefix: "".to_string(),
            except_exact: vec!["keep.me".to_string()],
            except_prefix: Vec::new(),
        }];
        let (matcher, _) = Blocklist::new_with_prefix_rules(Vec::<String>::new(), false, &rules_with_exception);
        assert!(!matcher.matches_all());
    }

    #[test]
    fn restrict_exact_keeps_prefix_state() {
        let rules = vec![PrefixRule {
            prefix: "bar.".to_string(),
            except_exact: vec!["bar.keep".to_string()],
            except_prefix: Vec::new(),
        }];
        let (matcher, _) = Blocklist::new_with_prefix_rules(["foo.count", "foo.max"], false, &rules);

        let restricted = matcher.restrict_exact(|name| name.ends_with(".count"));

        assert_eq!(restricted.exact, vec!["foo.count".to_string()]);
        assert_eq!(restricted.prefixes, matcher.prefixes);
        assert_eq!(restricted.rule_prefixes, matcher.rule_prefixes);
        assert_eq!(restricted.except_exact, matcher.except_exact);
        assert_eq!(restricted.except_prefix, matcher.except_prefix);
    }

    #[test]
    fn compile_metric_filter_normalizes_entries_and_rules() {
        let metric_filter = MetricFilter {
            values: vec!["my metric-name".to_string(), "123".to_string(), "service_".to_string()],
            match_prefix: false,
            prefix_rules: vec![rule("prefix.", &["prefix exact"], &["prefix-keep-"])],
        };

        let (matcher, report) = compile_metric_filter(&metric_filter);

        assert_eq!(report.dropped_entries, vec!["123".to_string()]);
        assert!(report.dropped_rules.is_empty());
        assert!(report.dropped_exceptions.is_empty());
        assert!(report.shadowed_rules.is_empty());

        // Exact entries normalize as full names.
        assert!(matcher.contains("my metric-name"));
        assert!(matcher.contains("my_metric_name"));
        assert!(matcher.contains("service"));
        assert!(matcher.contains("service_"));
        assert!(!matcher.contains("service.other"));

        // Prefix rules normalize their prefix and exceptions as prefixes.
        assert!(matcher.contains("prefix.stuff"));
        assert!(!matcher.contains("prefix_exact"));
        assert!(!matcher.contains("prefix_keep_metric"));
    }

    #[test]
    fn compile_metric_filter_prefix_mode_preserves_boundaries() {
        let metric_filter = MetricFilter {
            values: vec!["service_".to_string()],
            match_prefix: true,
            prefix_rules: Vec::new(),
        };

        let (matcher, report) = compile_metric_filter(&metric_filter);
        assert!(report.dropped_entries.is_empty());

        assert!(matcher.contains("service_requests"));
        assert!(matcher.contains("service requests"));
        assert!(!matcher.contains("service.requests"));
        assert!(!matcher.contains("service"));
    }

    #[test]
    fn compile_metric_filter_drops_unusable_prefix_rule_but_keeps_others() {
        let metric_filter = MetricFilter {
            values: Vec::new(),
            match_prefix: false,
            prefix_rules: vec![
                rule("123", &[], &[]),
                rule("foo.", &["123", "foo.keep"], &["123", "foo.skip."]),
            ],
        };

        let (matcher, report) = compile_metric_filter(&metric_filter);
        assert_eq!(report.dropped_rules, vec!["123".to_string()]);
        assert_eq!(report.dropped_exceptions, vec!["123".to_string(), "123".to_string()]);

        assert!(matcher.contains("foo.anything"));
        assert!(!matcher.contains("foo.keep"));
        assert!(!matcher.contains("foo.skip.waiting"));
    }
}
