use std::{
    num::NonZeroUsize,
    path::{Path, PathBuf},
};

use saluki_error::{generic_error, ErrorContext as _, GenericError};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(try_from = "String")]
pub enum TargetAddress {
    /// TCP socket.
    ///
    /// Stored as a `host:port` string so that Docker network aliases (for example, `baseline:8125`)
    /// are resolved at connection time rather than at config-parse time.
    Tcp(String),

    /// UDP socket.
    ///
    /// Stored as a `host:port` string so that Docker network aliases (for example, `baseline:8125`)
    /// are resolved at connection time rather than at config-parse time.
    Udp(String),

    /// Unix Domain Socket in SOCK_DGRAM mode.
    UnixDatagram(PathBuf),

    /// Unix Domain Socket in SOCK_STREAM mode.
    Unix(PathBuf),

    /// gRPC endpoint with service/method path.
    Grpc(String),
}

impl TryFrom<String> for TargetAddress {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        // Try to parse the value as a URI first, where the scheme indicates the socket type.
        if let Some((scheme, addr_data)) = value.split_once("://") {
            match scheme {
                "tcp" => {
                    // Validate that the address looks like host:port without forcing DNS
                    // resolution at parse time. Docker network aliases such as `baseline:8125`
                    // are only resolvable inside the container network, not on the host.
                    if addr_data.contains(':') {
                        Ok(Self::Tcp(addr_data.to_string()))
                    } else {
                        Err(format!("invalid TCP address '{}': expected host:port", addr_data))
                    }
                }
                "udp" => {
                    // Same rationale as TCP above.
                    if addr_data.contains(':') {
                        Ok(Self::Udp(addr_data.to_string()))
                    } else {
                        Err(format!("invalid UDP address '{}': expected host:port", addr_data))
                    }
                }
                "unixgram" => Ok(Self::UnixDatagram(PathBuf::from(addr_data))),
                "unix" => Ok(Self::Unix(PathBuf::from(addr_data))),
                "grpc" => Ok(Self::Grpc(addr_data.to_string())),
                _ => Err(format!("invalid scheme '{}' for target address '{}'", scheme, value)),
            }
        } else {
            Err(format!("invalid target address '{}': missing scheme", value))
        }
    }
}

#[derive(Clone, Deserialize)]
pub enum Payload {
    /// DogStatsD-encoded metrics.
    #[serde(rename = "dogstatsd")]
    DogStatsD(Box<lading_payload::dogstatsd::Config>),

    /// OpenTelemetry-encoded metrics.
    #[serde(rename = "opentelemetry_metrics")]
    OpenTelemetryMetrics(lading_payload::opentelemetry::metric::Config),

    /// OpenTelemetry-encoded traces.
    #[serde(rename = "opentelemetry_traces")]
    OpenTelemetryTraces(lading_payload::opentelemetry::trace::Config),
}

impl Payload {
    /// Returns the name of the payload type.
    pub fn name(&self) -> &'static str {
        match self {
            Self::DogStatsD(_) => "DogStatsD",
            Self::OpenTelemetryMetrics(_) => "OpenTelemetry Metrics",
            Self::OpenTelemetryTraces(_) => "OpenTelemetry Traces",
        }
    }
}

#[derive(Clone, Deserialize)]
pub struct CorpusBlueprint {
    /// The number of payloads to generate.
    pub size: NonZeroUsize,

    /// The payload configuration.
    pub payload: Payload,
}

impl CorpusBlueprint {
    /// Validates the corpus configuration, ensuring that all settings are valid for generating payloads.
    pub fn validate(&self) -> Result<(), GenericError> {
        match &self.payload {
            Payload::DogStatsD(config) => config
                .valid()
                .map_err(|e| generic_error!("Invalid DogStatsD payload configuration: {}", e)),
            Payload::OpenTelemetryMetrics(config) => config
                .valid()
                .map_err(|e| generic_error!("Invalid OpenTelemetry Metrics payload configuration: {}", e)),
            Payload::OpenTelemetryTraces(_) => Ok(()),
        }
    }
}

#[derive(Deserialize)]
pub struct Config {
    /// A fixed source of entropy for the random number generator (RNG) that is used to generated payloads to send to
    /// the configured target.
    ///
    /// When the same configuration is used multiple times, with an identical seed, the same exact payloads will be
    /// generated no matter how many times the application is run.
    pub seed: [u8; 32],

    /// Width of the target's aggregation buckets, in seconds.
    ///
    /// In some cases, correctness of certain metric types can only be asserted if they're all present within a single
    /// aggregation bucket, as the way they are emitted makes it impossible to compensate for after the fact. When this
    /// value is set, Millstone will delay sending payloads until crossing over the next aggregation bucket boundary,
    /// which is calculated by taking the modulo of the current time using this value.
    ///
    /// For example, when this value is set to 10, and the current time is 14, the result of `14 % 10` is 4, which means
    /// Millstone would wait 6 seconds, until the current time was 20 (20 % 10 == 0) before starting to send.
    pub aggregation_bucket_width_secs: Option<u64>,

    /// The target to send payloads to.
    pub target: TargetAddress,

    /// Output volume.
    ///
    /// This controls the number of payloads to send. If this number is larger than the corpus size, then the entire
    /// corpus will be sent multiple times, repeatedly cycling through it, until the target volume is reached.
    pub volume: NonZeroUsize,

    /// Delay between individual payload sends, in microseconds.
    ///
    /// When set to a nonzero value, millstone sleeps for this many microseconds after each send. This is
    /// primarily useful for UDP targets, where the kernel socket receive buffer is small (typically ~208 KiB
    /// on Linux) and a zero-delay blast of payloads will overflow the buffer and cause packet loss before the
    /// receiver has a chance to drain it.
    ///
    /// For example, a value of `500` (500 µs) limits the send rate to ~2,000 payloads/s, which is well within
    /// what a DogStatsD agent can drain. At 8 KiB per payload, that is ~16 MB/s—low enough that the 208 KiB
    /// buffer never accumulates more than a handful of packets at any moment, and all 10,000 payloads are
    /// delivered in ~5 seconds—well within a single 10-second aggregation bucket.
    ///
    /// Defaults to `0` (no delay).
    #[serde(default)]
    pub send_delay_us: u64,

    /// Corpus blueprint.
    ///
    /// This controls how payloads are generated, including the type of payload to generate and how many payloads to
    /// generate.
    #[serde(with = "serde_yaml::with::singleton_map_recursive")]
    pub corpus: CorpusBlueprint,
}

impl Config {
    /// Attempts to load a serialized `Config` from the given file path.
    ///
    /// # Errors
    ///
    /// If an error occurs while reading the file, or deserializing the configuration data, it will be returned.
    pub fn try_from_file<P>(config_path: P) -> Result<Self, GenericError>
    where
        P: AsRef<Path>,
    {
        let config_path = config_path.as_ref();
        let config_file_raw =
            std::fs::read_to_string(config_path).error_context("Failed to read configuration file.")?;
        let config: Self =
            serde_yaml::from_str(&config_file_raw).error_context("Failed to parse configuration file.")?;

        Ok(config)
    }
}
