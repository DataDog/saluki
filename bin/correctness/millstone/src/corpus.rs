use std::num::NonZeroUsize;

use base64::Engine as _;
use bytes::{BufMut as _, Bytes, BytesMut};
use bytesize::ByteSize;
use lading_payload::{
    opentelemetry::metric::OpentelemetryMetrics, trace_agent::v04::V04, trace_agent::v1::V1, DogStatsD,
    OpentelemetryLogs, OpentelemetryTraces,
};
use rand::{rngs::StdRng, SeedableRng as _};
use saluki_error::{generic_error, GenericError};
use tracing::info;

use crate::config::{Config, CorpusBlueprint, DatadogTraces, Payload, TargetAddress};
use crate::transcode::transcode_v04;

/// Maximum size of a single Datadog traces payload.
///
/// Tracing libraries flush batches of whole traces, which are routinely larger than a datagram-sized payload, so we
/// allow several traces of a few dozen spans each per payload. This is still far below the trace-agent's intake limit.
const DATADOG_TRACES_MAX_PAYLOAD_BYTES: usize = 64 * 1024;

/// A generated test corpus.
pub struct Corpus {
    payloads: Vec<Bytes>,
}

impl Corpus {
    /// Creates a new `Corpus` based on the given configuration, generating payloads in the process.
    ///
    /// # Errors
    ///
    /// If the corpus configuration is invalid, or there is an error while generating the payloads, it will be returned.
    pub fn from_config(config: &Config) -> Result<Self, GenericError> {
        // Build a finalized corpus blueprint, which updates any settings that can only be determined at runtime,
        // and validates the overall corpus blueprint to ensure it can be used to generate valid payloads.
        let blueprint = get_finalized_corpus_blueprint(config)?;
        let payload_name = blueprint.payload.name();

        let rng = StdRng::from_seed(config.seed);
        let (payloads, total_size_bytes) = generate_payloads(rng, blueprint)?;

        info!(
            "Generated test corpus with {} payloads ({}) in {} format.",
            payloads.len(),
            total_size_bytes.display().si(),
            payload_name
        );
        Ok(Self { payloads })
    }

    /// Consumes the corpus and returns the raw payloads.
    pub fn into_payloads(self) -> Vec<Bytes> {
        self.payloads
    }
}

fn get_finalized_corpus_blueprint(config: &Config) -> Result<CorpusBlueprint, GenericError> {
    // First, we'll handle any necessary modifications, such as updating the payload based on the target address being
    // used, etc.
    let mut blueprint = config.corpus.clone();

    // When generating DogStatsD payloads, we need to set the length-delimited framing mode when UDS is being used in
    // SOCK_STREAM mode. HTTP over UDS carries each payload as a request body, which needs no framing.
    match &mut blueprint.payload {
        Payload::DogStatsD(dsd_config) => {
            if matches!(config.target, TargetAddress::Unix(_)) && config.http_path.is_none() {
                dsd_config.length_prefix_framed = true;
            }
        }
        Payload::Static(_)
        | Payload::StaticBase64(_)
        | Payload::OpenTelemetryMetrics(_)
        | Payload::OpenTelemetryTraces(_)
        | Payload::OpenTelemetryLogs(_)
        | Payload::DatadogTraces(_) => {}
    }

    // Validate that the blueprint is valid from a payload generation standpoint.
    blueprint.validate()?;

    Ok(blueprint)
}

fn generate_payloads(mut rng: StdRng, blueprint: CorpusBlueprint) -> Result<(Vec<Bytes>, ByteSize), GenericError> {
    let mut payloads = Vec::new();

    match blueprint.payload {
        Payload::DogStatsD(config) => {
            // We set our `max_bytes` to 8192, which is the default packet size for the Datadog Agent's DogStatsD
            // server. It _can_ be increased beyond that, but rarely is, and so that's the fixed size we're going to
            // target here.
            let mut generator = DogStatsD::new(&config, &mut rng)?;
            generate_payloads_inner(&mut generator, rng, &mut payloads, blueprint.size, 8192)?
        }
        Payload::Static(payload) => generate_static_payloads(&payload, &mut payloads, blueprint.size),
        Payload::StaticBase64(payload) => generate_static_base64_payloads(&payload, &mut payloads, blueprint.size)?,
        Payload::OpenTelemetryMetrics(config) => {
            let mut generator = OpentelemetryMetrics::new(config, usize::MAX, &mut rng)?;
            generate_payloads_inner(&mut generator, rng, &mut payloads, blueprint.size, 8192)?
        }
        Payload::OpenTelemetryTraces(config) => {
            let mut generator = OpentelemetryTraces::with_config(&config, &mut rng)?;
            generate_payloads_inner(&mut generator, rng, &mut payloads, blueprint.size, 8192)?
        }
        Payload::OpenTelemetryLogs(config) => {
            let mut generator = OpentelemetryLogs::new(config, usize::MAX, &mut rng)?;
            generate_payloads_inner(&mut generator, rng, &mut payloads, blueprint.size, 8192)?
        }
        Payload::DatadogTraces(DatadogTraces {
            generator: lading_payload::trace_agent::Config::V04(config),
            transcode_to,
        }) => {
            let mut generator = V04::with_config(config, &mut rng)?;
            generate_payloads_inner(
                &mut generator,
                rng,
                &mut payloads,
                blueprint.size,
                DATADOG_TRACES_MAX_PAYLOAD_BYTES,
            )?;
            if let Some(encoding) = transcode_to {
                for payload in &mut payloads {
                    *payload = transcode_v04(payload, encoding)?;
                }
            }
        }
        Payload::DatadogTraces(DatadogTraces {
            generator: lading_payload::trace_agent::Config::V1(config),
            ..
        }) => {
            let mut generator = V1::with_config(config, &mut rng)?;
            generate_payloads_inner(
                &mut generator,
                rng,
                &mut payloads,
                blueprint.size,
                DATADOG_TRACES_MAX_PAYLOAD_BYTES,
            )?
        }
    }

    let total_size = payloads.iter().map(|p| p.len() as u64).sum();

    if payloads.is_empty() {
        Err(generic_error!("No payloads were generated."))
    } else {
        Ok((payloads, ByteSize(total_size)))
    }
}

fn generate_static_payloads(payload: &str, payloads: &mut Vec<Bytes>, size: NonZeroUsize) {
    generate_static_bytes_payloads(Bytes::copy_from_slice(payload.as_bytes()), payloads, size);
}

fn generate_static_base64_payloads(
    payload: &str, payloads: &mut Vec<Bytes>, size: NonZeroUsize,
) -> Result<(), GenericError> {
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(payload.trim())
        .map_err(|e| generic_error!("Failed to decode static_base64 payload: {}", e))?;
    generate_static_bytes_payloads(Bytes::from(decoded), payloads, size);
    Ok(())
}

fn generate_static_bytes_payloads(payload: Bytes, payloads: &mut Vec<Bytes>, size: NonZeroUsize) {
    for _ in 0..size.get() {
        payloads.push(payload.clone());
    }
}

fn generate_payloads_inner<G>(
    generator: &mut G, mut rng: StdRng, payloads: &mut Vec<Bytes>, size: NonZeroUsize, max_bytes: usize,
) -> Result<(), GenericError>
where
    G: lading_payload::Serialize,
{
    let first = payloads.len();
    for _ in 0..size.get() {
        let mut payload = BytesMut::new();
        let mut payload_writer = (&mut payload).writer();
        generator.to_bytes(&mut rng, max_bytes, &mut payload_writer)?;
        payloads.push(payload.freeze());
    }

    // Generators return nothing when an item doesn't fit. An occasional empty payload is harmless, but if every one
    // is empty, the configuration can't produce data at all, and millstone would only send empty requests.
    if payloads[first..].iter().all(Bytes::is_empty) {
        return Err(generic_error!(
            "Generated only empty payloads: the payload configuration can't produce any data within {} bytes.",
            max_bytes
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dogstatsd_config(target: &str, http_path: Option<&str>) -> Config {
        let http_path = http_path
            .map(|path| format!("http_path: \"{path}\"\n"))
            .unwrap_or_default();
        serde_saphyr::from_str(&format!(
            r#"
seed: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
target: "{target}"
{http_path}volume: 10
corpus:
  size: 10
  payload:
    dogstatsd: {{}}
"#
        ))
        .unwrap()
    }

    fn is_length_prefix_framed(config: &Config) -> bool {
        match get_finalized_corpus_blueprint(config).unwrap().payload {
            Payload::DogStatsD(dsd_config) => dsd_config.length_prefix_framed,
            _ => panic!("expected a DogStatsD payload"),
        }
    }

    #[test]
    fn dogstatsd_over_a_unix_stream_is_length_prefix_framed() {
        assert!(is_length_prefix_framed(&dogstatsd_config(
            "unix:///tmp/dsd.socket",
            None
        )));
    }

    #[test]
    fn dogstatsd_over_http_over_unix_is_not_framed() {
        assert!(!is_length_prefix_framed(&dogstatsd_config(
            "unix:///tmp/dsd.socket",
            Some("/v1/dogstatsd")
        )));
    }

    fn logs_config(seed_byte: u8) -> Config {
        logs_config_with(seed_byte, "{}")
    }

    fn logs_config_with(seed_byte: u8, payload_config: &str) -> Config {
        let yaml = format!(
            r#"
seed: [{seed}, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
target: "grpc://localhost:4317/opentelemetry.proto.collector.logs.v1.LogsService/Export"
volume: 1
corpus:
  size: 20
  payload:
    opentelemetry_logs: {payload_config}
"#,
            seed = seed_byte,
            payload_config = payload_config
        );
        serde_saphyr::from_str(&yaml).expect("config should parse")
    }

    #[test]
    fn opentelemetry_logs_corpus_is_deterministic_for_a_seed() {
        let first = Corpus::from_config(&logs_config(1)).expect("corpus should generate");
        let second = Corpus::from_config(&logs_config(1)).expect("corpus should generate");
        let other_seed = Corpus::from_config(&logs_config(2)).expect("corpus should generate");

        let first = first.into_payloads();
        assert_eq!(first.len(), 20);
        assert!(first.iter().all(|payload| !payload.is_empty() && payload.len() <= 8192));
        assert_eq!(first, second.into_payloads());
        assert_ne!(first, other_seed.into_payloads());
    }

    fn datadog_traces_config(transcode_to: Option<&str>) -> Config {
        let transcode_to = transcode_to
            .map(|encoding| format!("transcode_to: {encoding}\n      "))
            .unwrap_or_default();
        serde_saphyr::from_str(&format!(
            r#"
seed: [3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
target: "http://baseline:8126/v0.4/traces"
volume: 5
corpus:
  size: 5
  payload:
    datadog_traces:
      {transcode_to}v0.4:
        tags_per_span:
          inclusive: {{ min: 2, max: 5 }}
        metrics_per_span:
          inclusive: {{ min: 1, max: 3 }}
        priority_weights: {{ 1: 1 }}
"#
        ))
        .expect("config should parse")
    }

    #[test]
    fn transcoded_datadog_traces_carry_the_generated_spans() {
        // Transcoding decodes what lading generates, so this fails if lading's spans gain a field we don't carry.
        let v04 = Corpus::from_config(&datadog_traces_config(None))
            .unwrap()
            .into_payloads();
        let spans = |payloads: &[Bytes]| -> Vec<serde_json::Value> {
            payloads
                .iter()
                .flat_map(|payload| serde_json::from_slice::<Vec<serde_json::Value>>(payload).unwrap())
                .collect()
        };
        let json = Corpus::from_config(&datadog_traces_config(Some("json")))
            .unwrap()
            .into_payloads();
        let flat = Corpus::from_config(&datadog_traces_config(Some("spans_json")))
            .unwrap()
            .into_payloads();
        let span_count: usize = spans(&json).iter().map(|trace| trace.as_array().unwrap().len()).sum();
        assert!(span_count > 0);
        assert_eq!(spans(&flat).len(), span_count);
        for encoding in ["v0.5", "v0.7"] {
            let payloads = Corpus::from_config(&datadog_traces_config(Some(encoding)))
                .unwrap()
                .into_payloads();
            assert_eq!(payloads.len(), v04.len());
            assert!(payloads.iter().all(|payload| !payload.is_empty()), "{encoding}");
        }
    }

    #[test]
    fn a_payload_configuration_that_cant_fit_is_rejected() {
        let config = logs_config_with(1, "{body_size: {constant: 65535}}");

        let error = Corpus::from_config(&config)
            .err()
            .expect("an empty payload should be rejected");
        assert!(error.to_string().contains("Generated only empty payloads"), "{}", error);
    }
}
