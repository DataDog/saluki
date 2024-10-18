use bytes::{BufMut as _, Bytes, BytesMut};
use bytesize::ByteSize;
use lading_payload::DogStatsD;
use rand::{rngs::StdRng, Rng, SeedableRng as _};
use saluki_error::{generic_error, GenericError};
use tracing::info;

use crate::config::{Config, CorpusBlueprint, CountOrSize, Payload, TargetAddress};

/// A generated test corpus.
pub struct Corpus {
    payloads: Vec<Bytes>,
    max_payload_size: usize,
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
        let (payloads, max_payload_size, total_size_bytes) = generate_payloads(rng, blueprint)?;

        info!(
            "Generated test corpus with {} payloads ({}) in {} format.",
            payloads.len(),
            total_size_bytes.to_string_as(true),
            payload_name
        );
        Ok(Self {
            payloads,
            max_payload_size,
        })
    }

    /// Returns the maximum size of any payload in the corpus, in bytes.
    pub fn max_payload_size(&self) -> usize {
        self.max_payload_size
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
    {
        let Payload::DogStatsD(dsd_config) = &mut blueprint.payload;
        if let TargetAddress::Unix(_) = config.target {
            dsd_config.length_prefix_framed = true;
        }
    }

    // Validate that the blueprint is valid from a payload generation standpoint.
    let _ = blueprint.validate()?;

    Ok(blueprint)
}

fn generate_payloads<R>(mut rng: R, blueprint: CorpusBlueprint) -> Result<(Vec<Bytes>, usize, ByteSize), GenericError>
where
    R: Rng,
{
    let mut payloads = Vec::new();

    let max_payload_size = match blueprint.payload {
        Payload::DogStatsD(config) => {
            // We set our `max_bytes` to 8192, which is the default packet size for the Datadog Agent's DogStatsD
            // server. It _can_ be increased beyond that, but rarely is, and so that's the fixed size we're going to
            // target here.
            let generator = DogStatsD::new(config, &mut rng)?;
            generate_payloads_inner(&generator, rng, &mut payloads, blueprint.size, 8192)?
        }
    };

    let total_size = payloads.iter().map(|p| p.len() as u64).sum();

    if payloads.is_empty() {
        return Err(generic_error!("No payloads were generated."));
    } else {
        Ok((payloads, max_payload_size, ByteSize(total_size)))
    }
}

fn generate_payloads_inner<G, R>(
    generator: &G, mut rng: R, payloads: &mut Vec<Bytes>, size: CountOrSize, max_bytes: usize,
) -> Result<usize, GenericError>
where
    G: lading_payload::Serialize,
    R: Rng,
{
    let mut max_payload_size = 0;

    match size {
        CountOrSize::FixedCount(count) => {
            for _ in 0..count.get() {
                let mut payload = BytesMut::new();
                let mut payload_writer = (&mut payload).writer();
                generator.to_bytes(&mut rng, max_bytes, &mut payload_writer)?;

                if payload.len() > max_payload_size {
                    max_payload_size = payload.len();
                }

                payloads.push(payload.freeze());
            }
        }
        CountOrSize::FixedSize(size) => {
            let mut current_size = 0;
            let max_size = size.as_u64();

            while current_size < max_size {
                let mut payload = BytesMut::new();
                let mut payload_writer = (&mut payload).writer();
                generator.to_bytes(&mut rng, max_bytes, &mut payload_writer)?;

                let payload_size = payload.len() as u64;
                if current_size + payload_size > max_size {
                    break;
                }

                if payload.len() > max_payload_size {
                    max_payload_size = payload.len();
                }

                current_size += payload_size;
                payloads.push(payload.freeze());
            }
        }
    }

    Ok(max_payload_size)
}
