use std::collections::{btree_map::Entry, BTreeMap, BTreeSet};

use saluki_error::{generic_error, GenericError};

pub fn run_analysis(
    baseline_packets: &[String], comparison_packets: &[String], require_packets: bool,
) -> Result<(), (GenericError, Vec<String>)> {
    if baseline_packets.is_empty() && comparison_packets.is_empty() {
        if require_packets {
            return Err((
                generic_error!("No forwarded DogStatsD packets were captured."),
                vec![
                    "Expected forwarded DogStatsD packets, but both baseline and comparison captures were empty."
                        .to_string(),
                ],
            ));
        }

        return Ok(());
    }

    let baseline_counts = packet_counts(baseline_packets);
    let comparison_counts = packet_counts(comparison_packets);
    let details = packet_count_differences(&baseline_counts, &comparison_counts);

    if details.is_empty() {
        Ok(())
    } else {
        Err((
            generic_error!("Forwarded DogStatsD packet captures differ between baseline and comparison."),
            details,
        ))
    }
}

fn packet_counts(packets: &[String]) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();

    for packet in packets {
        match counts.entry(packet.clone()) {
            Entry::Vacant(entry) => {
                entry.insert(1);
            }
            Entry::Occupied(mut entry) => {
                *entry.get_mut() += 1;
            }
        }
    }

    counts
}

fn packet_count_differences(
    baseline_counts: &BTreeMap<String, usize>, comparison_counts: &BTreeMap<String, usize>,
) -> Vec<String> {
    let packets = baseline_counts
        .keys()
        .chain(comparison_counts.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut details = Vec::new();

    for packet in packets {
        let baseline_count = baseline_counts.get(&packet).copied().unwrap_or_default();
        let comparison_count = comparison_counts.get(&packet).copied().unwrap_or_default();

        match (baseline_count, comparison_count) {
            (0, count) => details.push(format!(
                "extra forwarded packet in comparison: packet_base64={packet}, comparison_count={count}"
            )),
            (count, 0) => details.push(format!(
                "missing forwarded packet in comparison: packet_base64={packet}, baseline_count={count}"
            )),
            (baseline_count, comparison_count) if baseline_count != comparison_count => {
                details.push(format!(
                    "forwarded packet count differs: packet_base64={packet}, \
                     baseline_count={baseline_count}, comparison_count={comparison_count}"
                ));
            }
            _ => {}
        }
    }

    details
}

#[cfg(test)]
mod tests {
    use super::run_analysis;

    #[test]
    fn matching_packets_pass_regardless_of_order() {
        let baseline = vec!["b".to_string(), "a".to_string(), "b".to_string()];
        let comparison = vec!["b".to_string(), "b".to_string(), "a".to_string()];

        run_analysis(&baseline, &comparison, true).expect("captures should match");
    }

    #[test]
    fn required_packets_fail_when_both_sides_are_empty() {
        let error = run_analysis(&[], &[], true).expect_err("empty required captures should fail");

        assert!(error.0.to_string().contains("No forwarded DogStatsD packets"));
        assert!(error.1[0].contains("both baseline and comparison captures were empty"));
    }

    #[test]
    fn optional_empty_captures_pass() {
        run_analysis(&[], &[], false).expect("empty optional captures should pass");
    }

    #[test]
    fn missing_extra_and_duplicate_mismatches_are_reported() {
        let baseline = vec![
            "same".to_string(),
            "missing".to_string(),
            "dupe".to_string(),
            "dupe".to_string(),
        ];
        let comparison = vec!["same".to_string(), "extra".to_string(), "dupe".to_string()];

        let error = run_analysis(&baseline, &comparison, false).expect_err("captures should differ");
        let details = error.1.join("\n");

        assert!(details.contains("missing forwarded packet in comparison: packet_base64=missing"));
        assert!(details.contains("extra forwarded packet in comparison: packet_base64=extra"));
        assert!(details
            .contains("forwarded packet count differs: packet_base64=dupe, baseline_count=2, comparison_count=1"));
    }
}
