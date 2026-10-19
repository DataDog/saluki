use std::{
    num::NonZeroUsize,
    path::{Path, PathBuf},
};

use base64::Engine as _;
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

    /// HTTP endpoint that each payload is `POST`ed to.
    ///
    /// Stored as the full URL (for example, `http://baseline:8126/v0.4/traces`) so that Docker network aliases are
    /// resolved at connection time rather than at config-parse time.
    Http(String),
}

impl TargetAddress {
    /// Returns the name of the transport this address uses.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Tcp(_) => "tcp",
            Self::Udp(_) => "udp",
            Self::UnixDatagram(_) => "unixgram",
            Self::Unix(_) => "unix",
            Self::Grpc(_) => "grpc",
            Self::Http(_) => "http",
        }
    }
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
                "http" => {
                    // Validate the URL shape only; the host is resolved when sending, for the same reason as TCP.
                    let uri = value
                        .parse::<hyper::Uri>()
                        .map_err(|e| format!("invalid HTTP address '{}': {}", value, e))?;
                    if uri.authority().is_some() {
                        Ok(Self::Http(value))
                    } else {
                        Err(format!(
                            "invalid HTTP address '{}': expected http://host:port/path",
                            value
                        ))
                    }
                }
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

    /// Static UTF-8 payload bytes embedded directly in the config.
    #[serde(rename = "static")]
    Static(String),

    /// Static payload bytes embedded directly in the config as base64.
    #[serde(rename = "static_base64")]
    StaticBase64(String),

    /// OpenTelemetry-encoded metrics.
    #[serde(rename = "opentelemetry_metrics")]
    OpenTelemetryMetrics(lading_payload::opentelemetry::metric::Config),

    /// OpenTelemetry-encoded traces.
    #[serde(rename = "opentelemetry_traces")]
    OpenTelemetryTraces(lading_payload::opentelemetry::trace::Config),

    /// Datadog trace-agent traces (for example, v0.4 MessagePack), as sent by Datadog tracing libraries.
    #[serde(rename = "datadog_traces")]
    DatadogTraces(lading_payload::trace_agent::Config),
}

impl Payload {
    /// Returns the name of the payload type.
    pub fn name(&self) -> &'static str {
        match self {
            Self::DogStatsD(_) => "DogStatsD",
            Self::Static(_) => "Static",
            Self::StaticBase64(_) => "Static Base64",
            Self::OpenTelemetryMetrics(_) => "OpenTelemetry Metrics",
            Self::OpenTelemetryTraces(_) => "OpenTelemetry Traces",
            Self::DatadogTraces(_) => "Datadog Traces",
        }
    }

    /// Returns the headers to send along with each payload when the target is HTTP.
    ///
    /// The first header is always `Content-Type`.
    pub fn http_headers(&self) -> &'static [(&'static str, &'static str)] {
        match self {
            // Mimic a tracing library: the trace-agent reads the tracer's language and version from these headers.
            Self::DatadogTraces(_) => &[
                ("content-type", "application/msgpack"),
                ("datadog-meta-lang", "millstone"),
                ("datadog-meta-tracer-version", "0.1.0"),
            ],
            _ => &[("content-type", "application/octet-stream")],
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
            Payload::Static(payload) => {
                if payload.is_empty() {
                    Err(generic_error!("Invalid static payload: payload must not be empty"))
                } else {
                    Ok(())
                }
            }
            Payload::StaticBase64(payload) => {
                if payload.is_empty() {
                    Err(generic_error!(
                        "Invalid static_base64 payload: payload must not be empty"
                    ))
                } else {
                    base64::engine::general_purpose::STANDARD
                        .decode(payload.trim())
                        .map(|_| ())
                        .map_err(|e| generic_error!("Invalid static_base64 payload: {}", e))
                }
            }
            Payload::OpenTelemetryMetrics(config) => config
                .valid()
                .map_err(|e| generic_error!("Invalid OpenTelemetry Metrics payload configuration: {}", e)),
            Payload::OpenTelemetryTraces(_) => Ok(()),
            Payload::DatadogTraces(config) => config
                .valid()
                .map_err(|e| generic_error!("Invalid Datadog Traces payload configuration: {}", e)),
        }
    }
}

#[derive(Deserialize)]
pub struct Config {
    /// A fixed source of entropy for the random number generator (RNG) that's used to generated payloads to send to
    /// the configured target.
    ///
    /// When the same configuration is used multiple times, with an identical seed, the same exact payloads will be
    /// generated no matter how many times the application is run.
    pub seed: [u8; 32],

    /// Width of the target's aggregation buckets, in seconds.
    ///
    /// In some cases, correctness of certain metric types can only be asserted if they're all present within a single
    /// aggregation bucket, as the way they're emitted makes it impossible to compensate for after the fact. When this
    /// value is set, Millstone will delay sending payloads until crossing over the next aggregation bucket boundary,
    /// which is calculated by taking the modulo of the current time using this value.
    ///
    /// For example, when this value is set to 10, and the current time is 14, the result of `14 % 10` is 4, which means
    /// Millstone would wait 6 seconds, until the current time was 20 (20 % 10 == 0) before starting to send.
    pub aggregation_bucket_width_secs: Option<u64>,

    /// Delay before sending the first payload, in seconds, counted from when the target is ready.
    ///
    /// An HTTP target is ready once it answers with anything other than a server error, so a proxy only is once its
    /// upstream is up. The delay is useful when the target accepts payloads before it has finished starting up: for
    /// example, the trace-agent's tagger connects to the Core Agent after the trace-agent starts listening, and the
    /// trace-agent sends payloads that arrive before then without container tags. Applied before aligning to an aggregation bucket, so that
    /// alignment still holds.
    ///
    /// Defaults to `0` (no delay).
    #[serde(default)]
    pub start_delay_secs: u64,

    /// The target to send payloads to.
    pub target: TargetAddress,

    /// HTTP request path to `POST` each payload to over a `unix://` target.
    ///
    /// When set, the socket is spoken to as an HTTP/1.1 server (for example, the trace-agent's `apm.socket`) instead of
    /// written to as a raw stream. Only valid with a `unix://` target, and must start with `/`.
    #[serde(default)]
    pub http_path: Option<String>,

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
    /// what a DogStatsD agent can drain. At 8 KiB per payload, that's ~16 MB/s—low enough that the 208 KiB
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
        config.validate()?;

        Ok(config)
    }

    /// Validates settings that depend on each other.
    ///
    /// # Errors
    ///
    /// If `http_path` is set without a `unix://` target, or doesn't start with `/`, an error is returned.
    pub fn validate(&self) -> Result<(), GenericError> {
        let Some(http_path) = &self.http_path else {
            return Ok(());
        };

        if !matches!(self.target, TargetAddress::Unix(_)) {
            return Err(generic_error!(
                "`http_path` is only supported with a `unix://` target, not a `{}://` target.",
                self.target.kind()
            ));
        }
        if !http_path.starts_with('/') {
            return Err(generic_error!("`http_path` must start with '/', got '{}'.", http_path));
        }

        Ok(())
    }

    /// Returns the name of the transport payloads are sent over.
    pub fn target_kind(&self) -> &'static str {
        match (&self.target, &self.http_path) {
            (TargetAddress::Unix(_), Some(_)) => "http+unix",
            (target, _) => target.kind(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Config, Payload, TargetAddress};

    #[test]
    fn parses_an_http_target() {
        let target = TargetAddress::try_from("http://baseline:8126/v0.4/traces".to_string()).unwrap();

        assert!(matches!(&target, TargetAddress::Http(url) if url == "http://baseline:8126/v0.4/traces"));
        assert_eq!(target.kind(), "http");
    }

    #[test]
    fn rejects_an_http_target_without_a_host() {
        assert!(TargetAddress::try_from("http:///v0.4/traces".to_string()).is_err());
    }

    #[test]
    fn parses_a_datadog_traces_corpus() {
        let config: Config = serde_yaml::from_str(
            r#"
seed: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
target: "http://baseline:8126/v0.4/traces"
volume: 10
corpus:
  size: 10
  payload:
    datadog_traces:
      v0.4:
        contexts:
          inclusive: { min: 1, max: 2 }
"#,
        )
        .unwrap();

        assert!(matches!(config.corpus.payload, Payload::DatadogTraces(_)));
        assert_eq!(
            config.corpus.payload.http_headers()[0],
            ("content-type", "application/msgpack")
        );
        config.corpus.validate().unwrap();
    }

    fn config_with_target(target: &str, http_path: Option<&str>) -> Config {
        let http_path = http_path
            .map(|path| format!("http_path: \"{path}\"\n"))
            .unwrap_or_default();
        serde_yaml::from_str(&format!(
            r#"
seed: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
target: "{target}"
{http_path}volume: 10
corpus:
  size: 10
  payload:
    static: "payload"
"#
        ))
        .unwrap()
    }

    #[test]
    fn parses_an_http_over_unix_target() {
        let config = config_with_target("unix:///tmp/apm.socket", Some("/v0.4/traces"));

        config.validate().unwrap();
        assert_eq!(config.http_path.as_deref(), Some("/v0.4/traces"));
        assert_eq!(config.target_kind(), "http+unix");
    }

    #[test]
    fn unix_target_without_http_path_stays_a_raw_stream() {
        let config = config_with_target("unix:///tmp/metrics.sock", None);

        config.validate().unwrap();
        assert_eq!(config.target_kind(), "unix");
    }

    #[test]
    fn rejects_http_path_without_a_unix_target() {
        for target in [
            "tcp://baseline:8126",
            "http://baseline:8126/v0.4/traces",
            "unixgram:///tmp/dsd.sock",
        ] {
            let config = config_with_target(target, Some("/v0.4/traces"));
            assert!(
                config.validate().is_err(),
                "`http_path` should be rejected for '{target}'"
            );
        }
    }

    #[test]
    fn rejects_a_relative_http_path() {
        let config = config_with_target("unix:///tmp/apm.socket", Some("v0.4/traces"));

        assert!(config.validate().is_err());
    }
}
