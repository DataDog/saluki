use std::num::NonZeroUsize;

use base64::Engine as _;
use bytes::{BufMut as _, Bytes, BytesMut};
use bytesize::ByteSize;
use lading_payload::{opentelemetry::metric::OpentelemetryMetrics, DogStatsD, OpentelemetryTraces};
use rand::{rngs::StdRng, SeedableRng as _};
use saluki_error::{generic_error, GenericError};
use tracing::info;

use crate::config::{Config, CorpusBlueprint, Payload, TargetAddress};

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
    // SOCK_STREAM mode.
    match &mut blueprint.payload {
        Payload::DogStatsD(dsd_config) => {
            if let TargetAddress::Unix(_) = config.target {
                dsd_config.length_prefix_framed = true;
            }
        }
        Payload::Static(_)
        | Payload::StaticBase64(_)
        | Payload::OpenTelemetryMetrics(_)
        | Payload::OpenTelemetryTraces(_) => {}
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
    for _ in 0..size.get() {
        let mut payload = BytesMut::new();
        let mut payload_writer = (&mut payload).writer();
        generator.to_bytes(&mut rng, max_bytes, &mut payload_writer)?;

        payloads.push(payload.freeze());
    }

    Ok(())
}
