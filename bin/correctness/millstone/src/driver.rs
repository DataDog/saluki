use std::time::{Duration, Instant, SystemTime};

use bytesize::ByteSize;
use saluki_error::{ErrorContext as _, GenericError};
use tracing::{info, trace};

use crate::{config::Config, corpus::Corpus, target::TargetSender};

/// Load driver.
///
/// This struct is the central point of the application, taking the provided configuration and generating the intended
/// payloads, as well as handling sending them to the target.
pub struct Driver {
    config: Config,
    corpus: Corpus,
    sender: TargetSender,
}

impl Driver {
    /// Creates a new `Driver` based on the given configuration.
    ///
    /// # Errors
    ///
    /// If an error occurs while creating the corpus, it will be returned.
    pub fn new(config: Config) -> Result<Self, GenericError> {
        let corpus = Corpus::from_config(&config).error_context("Failed to generate test corpus.")?;
        let sender = TargetSender::from_config(&config).error_context("Failed to create target sender.")?;

        Ok(Self { config, corpus, sender })
    }

    /// Runs the driver, sending all generated payloads to the target until the configured target volume has been reached.
    ///
    /// # Errors
    ///
    /// If an error occurs while sending payloads to the target, it will be returned.
    pub fn run(mut self) -> Result<(), GenericError> {
        let payloads = self.corpus.into_payloads();
        let mut borrowed_payloads = payloads.iter().map(|b| &b[..]).cycle();

        let mut payloads_sent = 0;
        let mut payload_bytes_sent = 0;
        let mut partial_sends = 0;

        let max_payloads = self.config.volume.get();

        // If we're trying to align to an aggregation bucket's start, figure out how long we need to wait and then add 1
        // second to that just to ensure we don't start sending until we're within the bucket window.
        if let Some(aggregation_bucket_width_secs) = self.config.aggregation_bucket_width_secs {
            let now_secs = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let next_bucket_boundary_secs = aggregation_bucket_width_secs - (now_secs % aggregation_bucket_width_secs);

            info!(
                "Waiting for next aggregation bucket boundary in {} seconds.",
                next_bucket_boundary_secs
            );

            std::thread::sleep(Duration::from_secs(next_bucket_boundary_secs + 1));
        }

        let start = Instant::now();

        loop {
            if payloads_sent >= max_payloads {
                break;
            }

            let payload = borrowed_payloads.next().unwrap();
            let bytes_sent = self.sender.send(payload)?;

            trace!(payload_len = payload.len(), bytes_sent, "Payload sent.");

            payload_bytes_sent += bytes_sent as u64;
            payloads_sent += 1;
            if payload.len() != bytes_sent {
                partial_sends += 1;
            }
        }

        let send_duration = start.elapsed();
        let throughput_bps = ByteSize((payload_bytes_sent as f64 / send_duration.as_secs_f64()) as u64);

        let payload_bytes_sent_human = ByteSize(payload_bytes_sent);
        let pct_partial_sends = (partial_sends as f64 / payloads_sent as f64) * 100.0;
        info!(
            "Sent {} payloads ({}), with {} partial sends ({}% of total), over {:?} ({}/s).",
            payloads_sent,
            payload_bytes_sent_human.to_string_as(true),
            partial_sends,
            pct_partial_sends,
            send_duration,
            throughput_bps.to_string_as(true)
        );

        Ok(())
    }
}
