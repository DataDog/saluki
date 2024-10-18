use std::{
    net::SocketAddr,
    num::NonZeroUsize,
    path::{Path, PathBuf},
};

use bytesize::ByteSize;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(try_from = "String")]
pub enum TargetAddress {
    /// TCP socket.
    Tcp(SocketAddr),

    /// UDP socket.
    Udp(SocketAddr),

    /// Unix Domain Socket in SOCK_DGRAM mode.
    UnixDatagram(PathBuf),

    /// Unix Domain Socket in SOCK_STREAM mode.
    Unix(PathBuf),
}

impl TryFrom<String> for TargetAddress {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        // Try to parse the value as a URI first, where the scheme indicates the socket type.
        if let Some((scheme, addr_data)) = value.split_once("://") {
            match scheme {
                "tcp" => addr_data
                    .parse::<SocketAddr>()
                    .map(Self::Tcp)
                    .map_err(|e| format!("invalid TCP address: {}", e)),
                "udp" => addr_data
                    .parse::<SocketAddr>()
                    .map(Self::Udp)
                    .map_err(|e| format!("invalid TCP address: {}", e)),
                "unixgram" => Ok(Self::UnixDatagram(PathBuf::from(addr_data))),
                "unix" => Ok(Self::Unix(PathBuf::from(addr_data))),
                _ => Err(format!("invalid scheme '{}' for target address '{}'", scheme, value)),
            }
        } else {
            Err(format!("invalid target address '{}': missing scheme", value))
        }
    }
}

#[derive(Deserialize)]
pub struct TargetVolume {
    /// The size of payloads to send.
    ///
    /// This controls how many payloads, or the number of payload bytes, that are sent overall.
    ///
    /// This can be larger than the size of the corpus, in which case the corpus will be cycled through as many times as
    /// needed to reach the target volume size.
    pub size: CountOrSize,
}

#[derive(Clone, Deserialize)]
#[serde(try_from = "ByteSize")]
pub struct NonZeroByteSize(ByteSize);

impl NonZeroByteSize {
    /// Returns the number of bytes represented by this value.
    pub fn as_u64(&self) -> u64 {
        self.0.as_u64()
    }
}

impl TryFrom<ByteSize> for NonZeroByteSize {
    type Error = String;

    fn try_from(value: ByteSize) -> Result<Self, Self::Error> {
        if value.as_u64() == 0 {
            Err("value must be non-zero".to_string())
        } else {
            Ok(Self(value))
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(tag = "mode", content = "value")]
pub enum CountOrSize {
    /// A fixed number of payloads.
    ///
    /// This mode is useful when a deterministic number of payloads -- whether in terms of generating them or sending
    /// them -- is desired.
    #[serde(rename = "count")]
    FixedCount(NonZeroUsize),

    /// A fixed size, in bytes, of payloads.
    ///
    /// Supports either a pure integer value, which is interpreted as the number of bytes, and additionally supports
    /// human-friendly values using either binary or decimal SI prefixes, such as `12 KiB`, `1.5MB`, and so on.
    #[serde(rename = "size")]
    FixedSize(NonZeroByteSize),
}

#[derive(Clone, Deserialize)]
pub enum Payload {
    /// DogStatsD-encoded metrics.
    #[serde(rename = "dogstatsd")]
    DogStatsD(lading_payload::dogstatsd::Config),
}

impl Payload {
    /// Returns the name of the payload type.
    pub fn name(&self) -> &'static str {
        match self {
            Self::DogStatsD(_) => "DogStatsD",
        }
    }
}

#[derive(Clone, Deserialize)]
pub struct CorpusBlueprint {
    /// The size of the corpus to generate.
    pub size: CountOrSize,

    /// The payload configuration.
    pub payload: Payload,
}

impl CorpusBlueprint {
    /// Validates the corpus configuration, ensuring that all settings are valid for generating payloads.
    pub fn validate(&self) -> Result<(), GenericError> {
        match self.payload {
            Payload::DogStatsD(config) => config
                .valid()
                .map_err(|e| generic_error!("Invalid DogStatsD payload configuration: {}", e)),
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

    /// The target to send payloads to.
    pub target: TargetAddress,

    /// Output volume.
    ///
    /// This controls how much total data volume is sent to the target.
    #[serde(with = "serde_yaml::with::singleton_map_recursive")]
    pub volume: TargetVolume,

    /// Corpus blueprint.
    ///
    /// This controls how payloads are generated, including the type of payload to generate and how many payloads/how
    /// much to generate.
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
