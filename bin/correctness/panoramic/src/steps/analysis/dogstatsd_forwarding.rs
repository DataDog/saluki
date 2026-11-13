use std::collections::{btree_map::Entry, BTreeMap, BTreeSet};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use saluki_error::{generic_error, GenericError};
use tracing::info;

/// Number of times each distinct forwarded message was seen, keyed by the raw message bytes.
type MessageCounts = BTreeMap<Vec<u8>, usize>;

/// Analysis failure: the overall error, plus the per-message details backing it.
type AnalysisError = (GenericError, Vec<String>);

pub fn run_analysis(
    baseline_packets: &[String], comparison_packets: &[String], require_packets: bool,
) -> Result<(), AnalysisError> {
    let (baseline_counts, baseline_filtered) = forwarded_message_counts(baseline_packets)?;
    let (comparison_counts, comparison_filtered) = forwarded_message_counts(comparison_packets)?;

    info!(
        "Filtered {} internal telemetry DogStatsD message(s) from baseline, and {} internal telemetry DogStatsD message(s) from comparison.",
        baseline_filtered, comparison_filtered
    );

    if baseline_counts.is_empty() && comparison_counts.is_empty() {
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

    let details = message_count_differences(&baseline_counts, &comparison_counts);

    if details.is_empty() {
        Ok(())
    } else {
        Err((
            generic_error!("Forwarded DogStatsD messages differ between baseline and comparison."),
            details,
        ))
    }
}

fn forwarded_message_counts(packets: &[String]) -> Result<(MessageCounts, usize), AnalysisError> {
    let mut counts = BTreeMap::new();
    let mut filtered = 0;

    for packet in packets {
        let payload = decode_forwarded_packet(packet)?;
        for message in payload.split(|byte| *byte == b'\n') {
            if message.is_empty() {
                continue;
            }

            if is_internal_telemetry(message) {
                filtered += 1;
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

    Ok((counts, filtered))
}

fn is_internal_telemetry(message: &[u8]) -> bool {
    message.starts_with(b"datadog.")
}

fn message_count_differences(baseline_counts: &MessageCounts, comparison_counts: &MessageCounts) -> Vec<String> {
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

fn decode_forwarded_packet(packet: &str) -> Result<Vec<u8>, AnalysisError> {
    STANDARD.decode(packet).map_err(|e| {
        (
            generic_error!("Failed to decode forwarded DogStatsD packet capture."),
            vec![format!(
                "invalid forwarded packet base64: packet_base64={packet}, error={e}"
            )],
        )
    })
}

#[cfg(test)]
mod tests {
    use base64::{engine::general_purpose::STANDARD, Engine as _};

    use super::run_analysis;

    /// Encodes `messages` as a single captured packet, the way the intake records them.
    fn packet(messages: &[&str]) -> String {
        STANDARD.encode(messages.join("\n"))
    }

    const BASELINE_TELEMETRY: &str = "datadog.dogstatsd.client.metrics:1|c|#client:go|c:baseline-container-id";
    const COMPARISON_TELEMETRY: &str = "datadog.dogstatsd.client.metrics:1|c|#client:go|c:comparison-container-id";
    const CORPUS_MESSAGE: &str = "my.metric:1|c";

    #[test]
    fn matching_messages_pass_regardless_of_packet_boundaries() {
        let baseline = vec!["YQpiCmE=".to_string()];
        let comparison = vec!["YQ==".to_string(), "Ygph".to_string()];

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
    fn internal_agent_telemetry_is_ignored() {
        let baseline = vec![packet(&[BASELINE_TELEMETRY]), packet(&[CORPUS_MESSAGE])];
        let comparison = vec![packet(&[COMPARISON_TELEMETRY]), packet(&[CORPUS_MESSAGE])];

        run_analysis(&baseline, &comparison, true).expect("internal telemetry should be filtered out");
    }

    #[test]
    fn internal_agent_telemetry_is_ignored_within_a_packet() {
        let baseline = vec![packet(&[BASELINE_TELEMETRY, CORPUS_MESSAGE])];
        let comparison = vec![packet(&[CORPUS_MESSAGE])];

        run_analysis(&baseline, &comparison, true).expect("internal telemetry should be filtered out");
    }

    #[test]
    fn required_packets_fail_when_only_internal_telemetry_was_captured() {
        let packets = vec![packet(&[BASELINE_TELEMETRY])];

        let error = run_analysis(&packets, &packets, true).expect_err("filtered-only captures should fail");

        assert!(error.0.to_string().contains("No forwarded DogStatsD packets"));
    }

    #[test]
    fn missing_extra_and_duplicate_mismatches_are_reported() {
        let baseline = vec![
            "c2FtZQ==".to_string(),
            "bWlzc2luZw==".to_string(),
            "ZHVwZQpkdXBl".to_string(),
        ];
        let comparison = vec!["c2FtZQ==".to_string(), "ZXh0cmE=".to_string(), "ZHVwZQ==".to_string()];

        let error = run_analysis(&baseline, &comparison, false).expect_err("captures should differ");
        let details = error.1.join("\n");

        assert!(details.contains("missing forwarded DogStatsD message in comparison:"));
        assert!(details.contains("extra forwarded DogStatsD message in comparison:"));
        assert!(details.contains(
            "forwarded DogStatsD message count differs: message_base64=ZHVwZQ==, baseline_count=2, comparison_count=1"
        ));
    }
}
