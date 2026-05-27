use std::collections::{btree_map::Entry, BTreeMap, BTreeSet};

use base64::{engine::general_purpose::STANDARD, Engine as _};
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

    let baseline_counts = forwarded_message_counts(baseline_packets)?;
    let comparison_counts = forwarded_message_counts(comparison_packets)?;
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

fn decode_forwarded_packet(packet: &str) -> Result<Vec<u8>, (GenericError, Vec<String>)> {
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
    use super::run_analysis;

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
