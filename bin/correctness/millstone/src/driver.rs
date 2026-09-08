use std::{
    path::PathBuf,
    thread,
    time::{Duration, Instant, SystemTime},
};

use bytesize::ByteSize;
use saluki_error::{ErrorContext as _, GenericError};
use tracing::{info, trace, warn};

use crate::{
    capture::{self, RunFacts},
    config::Config,
    corpus::Corpus,
    target::TargetSender,
};

/// Load driver.
///
/// This struct is the central point of the application, taking the provided configuration and generating the intended
/// payloads, as well as handling sending them to the target.
pub struct Driver {
    config: Config,
    corpus: Corpus,
    sender: TargetSender,
    traffic_capture_dir: Option<PathBuf>,
}

impl Driver {
    /// Creates a new `Driver` based on the given configuration.
    ///
    /// If `output_file` is provided, the driver will write all payloads to the given file path instead of the
    /// configured target. The configured target is still used to determine corpus generation parameters (for example,
    /// framing), so the bytes written to the file are identical to what would be sent over the wire.
    ///
    /// # Errors
    ///
    /// If an error occurs while creating the corpus, it will be returned.
    pub fn new(config: Config, output_file: Option<PathBuf>) -> Result<Self, GenericError> {
        let corpus = Corpus::from_config(&config).error_context("Failed to generate test corpus.")?;
        let sender = match output_file {
            Some(path) => TargetSender::from_file(&path).error_context("Failed to create file target sender.")?,
            None => TargetSender::from_config(&config).error_context("Failed to create target sender.")?,
        };

        Ok(Self {
            config,
            corpus,
            sender,
            traffic_capture_dir: None,
        })
    }

    /// Captures the payload stream this run sent into `dir`.
    ///
    /// The capture is written after the send loop has been timed, so requesting one cannot skew the run's reported
    /// send rate.
    pub fn with_traffic_capture_dir(mut self, dir: PathBuf) -> Self {
        self.traffic_capture_dir = Some(dir);
        self
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

        let send_delay = (self.config.send_delay_us > 0).then(|| Duration::from_micros(self.config.send_delay_us));

        // A send failure breaks the loop instead of returning immediately, so the prefix that did make it onto the
        // wire is still captured below before the error is reported.
        let mut send_error = None;

        loop {
            if payloads_sent >= max_payloads {
                break;
            }

            let payload = borrowed_payloads.next().unwrap();
            let bytes_sent = match self.sender.send(payload) {
                Ok(bytes_sent) => bytes_sent,
                Err(e) => {
                    send_error = Some(e);
                    break;
                }
            };

            trace!(payload_len = payload.len(), bytes_sent, "Payload sent.");

            if let Some(delay) = send_delay {
                thread::sleep(delay);
            }

            payload_bytes_sent += bytes_sent as u64;
            payloads_sent += 1;
            if payload.len() != bytes_sent {
                partial_sends += 1;
            }
        }

        let send_duration = start.elapsed();

        if send_error.is_none() {
            let throughput_bps = ByteSize((payload_bytes_sent as f64 / send_duration.as_secs_f64()) as u64);
            let payload_bytes_sent_human = ByteSize(payload_bytes_sent);
            let pct_partial_sends = (partial_sends as f64 / payloads_sent as f64) * 100.0;
            info!(
                "Sent {} payloads ({}), with {} partial sends ({}% of total), over {:?} ({}/s).",
                payloads_sent,
                payload_bytes_sent_human.display().si(),
                partial_sends,
                pct_partial_sends,
                send_duration,
                throughput_bps.display().si()
            );
        }

        // Written after the run is measured, so the capture stays out of the reported send timing. Written on a send
        // error too: the payloads already sent are exactly the input this diagnostic exists to preserve.
        if let Some(dir) = self.traffic_capture_dir.as_deref() {
            let facts = RunFacts {
                seed: self.config.seed.iter().map(|b| format!("{:02x}", b)).collect(),
                payload_kind: self.config.corpus.payload.name(),
                target_kind: self.config.target.kind(),
                send_delay_us: self.config.send_delay_us,
                volume: max_payloads,
                payloads_sent,
                wire_bytes: payload_bytes_sent,
                partial_sends,
                complete: send_error.is_none(),
            };

            // A lost capture is a lost diagnostic, not a failed run.
            if let Err(e) = capture::write_input_capture(dir, &payloads, facts) {
                warn!(error = %e, "Failed to write input traffic capture.");
            }
        }

        match send_error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}
