use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use super::artifact::AgentContextRecord;

const HEADING: &str = "   Contexts\tMetric name\t(number of unique values for each tag)\n";

#[derive(Default)]
pub(crate) struct ContextReport {
    metrics: HashMap<String, MetricSummary>,
}

impl ContextReport {
    pub(super) fn new() -> Self {
        Self::default()
    }

    pub(super) fn ingest(&mut self, record: AgentContextRecord) {
        let AgentContextRecord {
            name,
            host: _,
            metric_type: _,
            tagger_tags: _,
            metric_tags,
            no_index: _,
            source: _,
        } = record;
        let summary = self.metrics.entry(name).or_default();
        summary.context_count += 1;
        summary.metric_tags.extend(metric_tags);
    }

    pub(crate) fn render(&self, metric_limit: usize, tag_limit: usize) -> String {
        let mut output = String::from(HEADING);
        let mut metrics: Vec<_> = self.metrics.iter().collect();
        metrics.sort_unstable_by(|(left_name, left), (right_name, right)| {
            right
                .context_count
                .cmp(&left.context_count)
                .then_with(|| left_name.cmp(right_name))
        });

        let visible_count = visible_item_count(metrics.len(), metric_limit);
        for (name, summary) in &metrics[..visible_count] {
            write!(output, " {:>10}\t{}\t(", summary.context_count, name).expect("writing to a String should not fail");
            summary.write_tags(&mut output, tag_limit);
            output.push_str(")\n");
        }

        let hidden_metrics = &metrics[visible_count..];
        if !hidden_metrics.is_empty() {
            let hidden_context_count: usize = hidden_metrics.iter().map(|(_, summary)| summary.context_count).sum();
            writeln!(
                output,
                " {hidden_context_count:>10}\t(other {} metrics)",
                hidden_metrics.len()
            )
            .expect("writing to a String should not fail");
        }

        output
    }
}

#[derive(Default)]
struct MetricSummary {
    context_count: usize,
    metric_tags: HashSet<String>,
}

impl MetricSummary {
    fn write_tags(&self, output: &mut String, tag_limit: usize) {
        let mut cardinalities: HashMap<&str, usize> = HashMap::new();
        for tag in &self.metric_tags {
            let key = tag.split_once(':').map_or(tag.as_str(), |(key, _)| key);
            *cardinalities.entry(key).or_default() += 1;
        }

        let mut tags: Vec<_> = cardinalities.into_iter().collect();
        tags.sort_unstable_by(|(left_key, left_count), (right_key, right_count)| {
            right_count.cmp(left_count).then_with(|| left_key.cmp(right_key))
        });

        let visible_count = visible_item_count(tags.len(), tag_limit);
        for (index, (key, cardinality)) in tags[..visible_count].iter().enumerate() {
            if index > 0 {
                output.push_str(", ");
            }
            write!(output, "{cardinality} {key}").expect("writing to a String should not fail");
        }

        let hidden_tags = &tags[visible_count..];
        if !hidden_tags.is_empty() {
            let hidden_value_count: usize = hidden_tags.iter().map(|(_, cardinality)| cardinality).sum();
            if visible_count > 0 {
                output.push_str(", ");
            }
            write!(
                output,
                "{hidden_value_count} values in {} other tags",
                hidden_tags.len()
            )
            .expect("writing to a String should not fail");
        }
    }
}

fn visible_item_count(item_count: usize, limit: usize) -> usize {
    if item_count.saturating_sub(limit) > 1 {
        limit
    } else {
        item_count
    }
}

#[cfg(test)]
mod tests {
    use super::ContextReport;
    use crate::dogstatsd_contexts::artifact::AgentContextRecord;

    const HEADING: &str = "   Contexts\tMetric name\t(number of unique values for each tag)\n";

    #[test]
    fn sorts_metric_and_tag_ties_lexically() {
        let mut report = ContextReport::new();
        report.ingest(record("beta", "host", &[], &["z:value", "a:value"]));
        report.ingest(record("alpha", "host", &[], &["z:value", "a:value"]));

        assert_eq!(
            report.render(10, 10),
            concat!(
                "   Contexts\tMetric name\t(number of unique values for each tag)\n",
                "          1\talpha\t(1 a, 1 z)\n",
                "          1\tbeta\t(1 a, 1 z)\n",
            )
        );
    }

    #[test]
    fn deduplicates_full_metric_tags_before_calculating_cardinality() {
        let mut report = ContextReport::new();
        report.ingest(record(
            "metric",
            "host-a",
            &[],
            &["env:prod", "env:prod", "env:staging"],
        ));
        report.ingest(record("metric", "host-b", &[], &["env:prod"]));

        assert_eq!(
            report.render(10, 10),
            concat!(
                "   Contexts\tMetric name\t(number of unique values for each tag)\n",
                "          2\tmetric\t(2 env)\n",
            )
        );
    }

    #[test]
    fn counts_host_and_origin_variants_but_excludes_tagger_tags() {
        let mut report = ContextReport::new();
        report.ingest(record("metric", "host-a", &["pod_name:a"], &["env:prod"]));

        let mut second = record("metric", "host-b", &["pod_name:b"], &["env:prod"]);
        second.metric_type = "Counter".to_owned();
        second.no_index = true;
        second.source = 99;
        report.ingest(second);

        report.ingest(record("metric", "host-c", &["pod_name:c"], &["env:prod"]));

        assert_eq!(
            report.render(10, 10),
            concat!(
                "   Contexts\tMetric name\t(number of unique values for each tag)\n",
                "          3\tmetric\t(1 env)\n",
            )
        );
    }

    #[test]
    fn uses_the_whole_colonless_tag_or_the_prefix_before_only_the_first_colon() {
        let mut report = ContextReport::new();
        report.ingest(record(
            "metric",
            "host",
            &[],
            &["bare", "image:registry/repo:v1", "image:registry/repo:v2"],
        ));

        assert_eq!(
            report.render(10, 10),
            concat!(
                "   Contexts\tMetric name\t(number of unique values for each tag)\n",
                "          1\tmetric\t(2 image, 1 bare)\n",
            )
        );
    }

    #[test]
    fn applies_agent_metric_limit_plus_one_rule_without_overflow() {
        let cases = [
            MetricLimitCase {
                name: "zero limit shows its one allowed extra item",
                counts: &[("a", 1)],
                limit: 0,
                expected_body: "          1\ta\t()\n",
            },
            MetricLimitCase {
                name: "zero limit summarizes two items",
                counts: &[("a", 2), ("b", 1)],
                limit: 0,
                expected_body: "          3\t(other 2 metrics)\n",
            },
            MetricLimitCase {
                name: "item count equal to limit is shown",
                counts: &[("a", 2), ("b", 1)],
                limit: 2,
                expected_body: "          2\ta\t()\n          1\tb\t()\n",
            },
            MetricLimitCase {
                name: "limit plus one items are all shown",
                counts: &[("a", 2), ("b", 1)],
                limit: 1,
                expected_body: "          2\ta\t()\n          1\tb\t()\n",
            },
            MetricLimitCase {
                name: "limit plus two items hide the remainder and sum their contexts",
                counts: &[("a", 3), ("b", 2), ("c", 1)],
                limit: 1,
                expected_body: "          3\ta\t()\n          3\t(other 2 metrics)\n",
            },
            MetricLimitCase {
                name: "maximum limit does not overflow",
                counts: &[("a", 3), ("b", 2), ("c", 1)],
                limit: usize::MAX,
                expected_body: "          3\ta\t()\n          2\tb\t()\n          1\tc\t()\n",
            },
        ];

        for case in cases {
            let report = report_with_counts(case.counts);
            assert_eq!(
                report.render(case.limit, usize::MAX),
                format!("{HEADING}{}", case.expected_body),
                "{}",
                case.name
            );
        }
    }

    #[test]
    fn applies_agent_tag_limit_plus_one_rule_without_overflow() {
        let cases = [
            TagLimitCase {
                name: "zero limit shows its one allowed extra tag",
                tags: &["a:1"],
                limit: 0,
                expected_tags: "1 a",
            },
            TagLimitCase {
                name: "zero limit summarizes two tags and their values",
                tags: &["a:1", "a:2", "b:1"],
                limit: 0,
                expected_tags: "3 values in 2 other tags",
            },
            TagLimitCase {
                name: "tag count equal to limit is shown",
                tags: &["a:1", "b:1"],
                limit: 2,
                expected_tags: "1 a, 1 b",
            },
            TagLimitCase {
                name: "limit plus one tags are all shown",
                tags: &["a:1", "b:1"],
                limit: 1,
                expected_tags: "1 a, 1 b",
            },
            TagLimitCase {
                name: "limit plus two tags hide the remainder and sum their values",
                tags: &["a:1", "a:2", "a:3", "b:1", "b:2", "c:1"],
                limit: 1,
                expected_tags: "3 a, 3 values in 2 other tags",
            },
            TagLimitCase {
                name: "maximum limit does not overflow",
                tags: &["a:1", "a:2", "a:3", "b:1", "b:2", "c:1"],
                limit: usize::MAX,
                expected_tags: "3 a, 2 b, 1 c",
            },
        ];

        for case in cases {
            let mut report = ContextReport::new();
            report.ingest(record("metric", "host", &[], case.tags));
            assert_eq!(
                report.render(usize::MAX, case.limit),
                format!("{HEADING}          1\tmetric\t({})\n", case.expected_tags),
                "{}",
                case.name
            );
        }
    }

    fn report_with_counts(counts: &[(&str, usize)]) -> ContextReport {
        let mut report = ContextReport::new();
        for (name, count) in counts {
            for index in 0..*count {
                report.ingest(record(name, &format!("host-{index}"), &[], &[]));
            }
        }
        report
    }

    fn record(name: &str, host: &str, tagger_tags: &[&str], metric_tags: &[&str]) -> AgentContextRecord {
        AgentContextRecord {
            name: name.to_owned(),
            host: host.to_owned(),
            metric_type: "Gauge".to_owned(),
            tagger_tags: tagger_tags.iter().map(|tag| (*tag).to_owned()).collect(),
            metric_tags: metric_tags.iter().map(|tag| (*tag).to_owned()).collect(),
            no_index: false,
            source: 1,
        }
    }

    struct MetricLimitCase {
        name: &'static str,
        counts: &'static [(&'static str, usize)],
        limit: usize,
        expected_body: &'static str,
    }

    struct TagLimitCase {
        name: &'static str,
        tags: &'static [&'static str],
        limit: usize,
        expected_tags: &'static str,
    }
}
