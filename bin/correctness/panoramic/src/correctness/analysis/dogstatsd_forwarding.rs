use std::collections::{btree_map::Entry, BTreeMap, BTreeSet};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use saluki_error::{generic_error, GenericError};
use serde::Deserialize;

/// How forwarded DogStatsD captures should be compared.
#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DogStatsDForwardingComparisonMode {
    /// Compare forwarded UDP datagrams exactly as captured.
    Packets,

    /// Compare DogStatsD messages after splitting forwarded datagrams on newline boundaries.
    Messages,
}

pub fn run_analysis(
    baseline_packets: &[String], comparison_packets: &[String], require_packets: bool,
    require_batched_packets: bool, comparison_mode: DogStatsDForwardingComparisonMode,
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

    if require_batched_packets {
        let details = batched_packet_requirement_failures(baseline_packets, comparison_packets)?;
        if !details.is_empty() {
            return Err((
                generic_error!("Expected forwarded DogStatsD packet batching, but capture did not include it."),
                details,
            ));
        }
    }

    let details = match comparison_mode {
        DogStatsDForwardingComparisonMode::Packets => {
            let baseline_counts = packet_counts(baseline_packets);
            let comparison_counts = packet_counts(comparison_packets);
            packet_count_differences(&baseline_counts, &comparison_counts)
        }
        DogStatsDForwardingComparisonMode::Messages => {
            let baseline_counts = forwarded_message_counts(baseline_packets)?;
            let comparison_counts = forwarded_message_counts(comparison_packets)?;
            message_count_differences(&baseline_counts, &comparison_counts)
        }
    };

    if details.is_empty() {
        Ok(())
    } else {
        Err((
            generic_error!("Forwarded DogStatsD packet captures differ between baseline and comparison."),
            details,
        ))
    }
}

const fn default_comparison_mode() -> DogStatsDForwardingComparisonMode {
    DogStatsDForwardingComparisonMode::Packets
}

impl Default for DogStatsDForwardingComparisonMode {
    fn default() -> Self {
        default_comparison_mode()
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

fn forwarded_message_counts(packets: &[String]) -> Result<BTreeMap<Vec<u8>, usize>, (GenericError, Vec<String>)> {
    let mut counts = BTreeMap::new();

    for packet in packets {
        let payload = decode_forwarded_packet(packet)?;
        for message in payload.split(|byte| *byte == b'\n') {
            if message.is_empty() {
                continue;
            }

            match counts.entry(message.to_vec()) {
                Entry::Vacant(entry) => {
                    entry.insert(1);
                }
                Entry::Occupied(mut entry) => {
                    *entry.get_mut() += 1;
                }
            }
        }
    }

    Ok(counts)
}

fn message_count_differences(
    baseline_counts: &BTreeMap<Vec<u8>, usize>, comparison_counts: &BTreeMap<Vec<u8>, usize>,
) -> Vec<String> {
    let messages = baseline_counts
        .keys()
        .chain(comparison_counts.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut details = Vec::new();

    for message in messages {
        let baseline_count = baseline_counts.get(&message).copied().unwrap_or_default();
        let comparison_count = comparison_counts.get(&message).copied().unwrap_or_default();
        let message_base64 = STANDARD.encode(message.as_slice());

        match (baseline_count, comparison_count) {
            (0, count) => details.push(format!(
                "extra forwarded DogStatsD message in comparison: \
                 message_base64={message_base64}, comparison_count={count}"
            )),
            (count, 0) => details.push(format!(
                "missing forwarded DogStatsD message in comparison: \
                 message_base64={message_base64}, baseline_count={count}"
            )),
            (baseline_count, comparison_count) if baseline_count != comparison_count => {
                details.push(format!(
                    "forwarded DogStatsD message count differs: message_base64={message_base64}, \
                     baseline_count={baseline_count}, comparison_count={comparison_count}"
                ));
            }
            _ => {}
        }
    }

    details
}

fn batched_packet_requirement_failures(
    baseline_packets: &[String], comparison_packets: &[String],
) -> Result<Vec<String>, (GenericError, Vec<String>)> {
    let baseline_has_batched_packet = has_batched_packet(baseline_packets)?;
    let comparison_has_batched_packet = has_batched_packet(comparison_packets)?;
    let mut details = Vec::new();

    if !baseline_has_batched_packet {
        details.push(
            "baseline did not capture any forwarded UDP packet containing multiple DogStatsD messages".to_string(),
        );
    }
    if !comparison_has_batched_packet {
        details.push(
            "comparison did not capture any forwarded UDP packet containing multiple DogStatsD messages".to_string(),
        );
    }

    Ok(details)
}

fn has_batched_packet(packets: &[String]) -> Result<bool, (GenericError, Vec<String>)> {
    for packet in packets {
        let payload = decode_forwarded_packet(packet)?;
        if payload.contains(&b'\n') {
            return Ok(true);
        }
    }

    Ok(false)
}

fn decode_forwarded_packet(packet: &str) -> Result<Vec<u8>, (GenericError, Vec<String>)> {
    STANDARD.decode(packet).map_err(|e| {
        (
            generic_error!("Failed to decode forwarded DogStatsD packet capture."),
            vec![format!("invalid forwarded packet base64: packet_base64={packet}, error={e}")],
        )
    })
}

#[cfg(test)]
mod tests {
    use super::{run_analysis, DogStatsDForwardingComparisonMode};

    #[test]
    fn matching_packets_pass_regardless_of_order() {
        let baseline = vec!["b".to_string(), "a".to_string(), "b".to_string()];
        let comparison = vec!["b".to_string(), "b".to_string(), "a".to_string()];

        run_analysis(
            &baseline,
            &comparison,
            true,
            false,
            DogStatsDForwardingComparisonMode::Packets,
        )
        .expect("captures should match");
    }

    #[test]
    fn required_packets_fail_when_both_sides_are_empty() {
        let error = run_analysis(
            &[],
            &[],
            true,
            false,
            DogStatsDForwardingComparisonMode::Packets,
        )
        .expect_err("empty required captures should fail");

        assert!(error.0.to_string().contains("No forwarded DogStatsD packets"));
        assert!(error.1[0].contains("both baseline and comparison captures were empty"));
    }

    #[test]
    fn optional_empty_captures_pass() {
        run_analysis(
            &[],
            &[],
            false,
            false,
            DogStatsDForwardingComparisonMode::Packets,
        )
        .expect("empty optional captures should pass");
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

        let error = run_analysis(
            &baseline,
            &comparison,
            false,
            false,
            DogStatsDForwardingComparisonMode::Packets,
        )
        .expect_err("captures should differ");
        let details = error.1.join("\n");

        assert!(details.contains("missing forwarded packet in comparison: packet_base64=missing"));
        assert!(details.contains("extra forwarded packet in comparison: packet_base64=extra"));
        assert!(details
            .contains("forwarded packet count differs: packet_base64=dupe, baseline_count=2, comparison_count=1"));
    }

    #[test]
    fn message_mode_ignores_forwarded_packet_batch_boundaries() {
        let baseline = vec!["YQpiCmM=".to_string()];
        let comparison = vec!["YQpi".to_string(), "Yw==".to_string()];

        run_analysis(
            &baseline,
            &comparison,
            true,
            true,
            DogStatsDForwardingComparisonMode::Messages,
        )
        .expect("message captures should match despite different batching");
    }

    #[test]
    fn batched_packet_requirement_fails_when_either_side_has_no_batch() {
        let baseline = vec!["YTpiCmM6ZHxl".to_string()];
        let comparison = vec!["YTpi".to_string(), "YzpkfGU=".to_string()];

        let error = run_analysis(
            &baseline,
            &comparison,
            true,
            true,
            DogStatsDForwardingComparisonMode::Packets,
        )
        .expect_err("comparison should fail the batching requirement");

        assert!(error.1[0].contains("comparison did not capture any forwarded UDP packet"));
    }
}
