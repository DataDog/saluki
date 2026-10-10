//! Products, configs, and the complete snapshots the channel delivers.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};
use snafu::Snafu;

use crate::canonical::PlanHash;

/// A product of the artifact channel: a kind of artifact, with its own configs, revisions, and authority.
///
/// Serialized as its name, such as `"policy"`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactProduct {
    /// Policy artifacts, each replacing the policy region of the plan.
    Policy,
}

impl ArtifactProduct {
    /// Returns the product's name, as the channel writes it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Policy => "policy",
        }
    }
}

impl fmt::Display for ArtifactProduct {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The name of a product the channel does not know.
#[derive(Clone, Debug, Eq, PartialEq, Snafu)]
#[snafu(display("Unknown artifact product `{}`: the products are `policy`.", name))]
pub struct UnknownProduct {
    /// The name.
    pub name: String,
}

impl FromStr for ArtifactProduct {
    type Err = UnknownProduct;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        match name {
            "policy" => Ok(Self::Policy),
            other => Err(UnknownProduct {
                name: other.to_string(),
            }),
        }
    }
}

/// The identifier of one config within a product, chosen by the channel's publisher.
///
/// Acknowledgments are kept per config ID: the latest state of each config the host has seen.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ArtifactConfigId(String);

impl ArtifactConfigId {
    /// Creates a config ID.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Returns the config ID as a string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ArtifactConfigId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One config of a product, as the channel delivers it.
///
/// The `payload` is the [envelope](super::Envelope) as JSON text, and `sha256` is the lowercase hexadecimal SHA-256 of
/// the payload's UTF-8 bytes, so that the transport hash covers exact bytes. Serialized with the members `id`,
/// `version`, `sha256`, and `payload`; readers ignore unknown members.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ArtifactConfig {
    /// The config's identifier.
    pub id: ArtifactConfigId,

    /// The config's version, as the publisher numbers it.
    pub version: u64,

    /// The lowercase hexadecimal SHA-256 of the payload's UTF-8 bytes.
    pub sha256: String,

    /// The envelope, as JSON text.
    pub payload: String,
}

impl ArtifactConfig {
    /// Returns the transport hash of a payload: the lowercase hexadecimal SHA-256 of its UTF-8 bytes.
    pub fn transport_hash(payload: &str) -> String {
        PlanHash::of_bytes(payload.as_bytes())
            .digest()
            .iter()
            .map(|byte| format!("{:02x}", byte))
            .collect()
    }

    /// Checks that the config's `sha256` is the transport hash of its payload.
    ///
    /// # Errors
    ///
    /// Returns an error if it is not: the payload was changed or truncated in transit, or the publisher hashed other
    /// bytes than it sent.
    pub fn verify_transport_hash(&self) -> Result<(), TransportHashMismatch> {
        let actual = Self::transport_hash(&self.payload);
        if actual == self.sha256 {
            Ok(())
        } else {
            Err(TransportHashMismatch {
                declared: self.sha256.clone(),
                actual,
            })
        }
    }
}

/// A config whose `sha256` is not the transport hash of its payload.
#[derive(Clone, Debug, Eq, PartialEq, Snafu)]
#[snafu(display(
    "transport hash mismatch: the config declares sha256 {}, but its payload hashes to {}",
    declared,
    actual
))]
pub struct TransportHashMismatch {
    /// The hash the config declares.
    pub declared: String,

    /// The hash of the payload received.
    pub actual: String,
}

/// A complete snapshot of one product's configs: never a delta.
///
/// Each snapshot replaces the previous one. An empty snapshot revokes: the default program runs. A snapshot of the
/// policy product with more than one config is rejected as a whole, since one policy replaces the policy region.
/// A transport failure delivers no snapshot, so the current one stays in effect.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactSnapshot {
    /// The product.
    pub product: ArtifactProduct,

    /// Every config of the product, in the order the channel delivered them.
    pub configs: Vec<ArtifactConfig>,
}

impl ArtifactSnapshot {
    /// Creates a snapshot of a product's configs.
    pub fn new(product: ArtifactProduct, configs: Vec<ArtifactConfig>) -> Self {
        Self { product, configs }
    }

    /// Creates an empty snapshot of a product, which revokes it.
    pub fn revoked(product: ArtifactProduct) -> Self {
        Self::new(product, Vec::new())
    }

    /// Returns `true` if the snapshot revokes the product: it has no configs.
    pub fn revokes(&self) -> bool {
        self.configs.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(payload: &str, sha256: String) -> ArtifactConfig {
        ArtifactConfig {
            id: ArtifactConfigId::new("demo"),
            version: 7,
            sha256,
            payload: payload.to_string(),
        }
    }

    #[test]
    fn transport_hashes_are_the_lowercase_hexadecimal_sha256_of_the_payload_bytes() {
        // FIPS 180-2, appendix B.1.
        assert_eq!(
            ArtifactConfig::transport_hash("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let payload = r#"{"revision": 1}"#;
        assert_eq!(
            config(payload, ArtifactConfig::transport_hash(payload)).verify_transport_hash(),
            Ok(())
        );
        let uppercase = ArtifactConfig::transport_hash(payload).to_uppercase();
        let mismatch = config(payload, uppercase.clone()).verify_transport_hash().unwrap_err();
        assert_eq!(mismatch.declared, uppercase);
        assert_eq!(mismatch.actual, ArtifactConfig::transport_hash(payload));
        assert!(
            config(&format!("{} ", payload), ArtifactConfig::transport_hash(payload))
                .verify_transport_hash()
                .is_err()
        );
    }

    #[test]
    fn configs_read_the_stub_protocol_entry_and_ignore_unknown_members() {
        let entry = serde_json::json!({
            "id": "demo", "version": 7, "sha256": "00", "payload": "{}", "future": true
        });
        let read: ArtifactConfig = serde_json::from_value(entry).unwrap();
        assert_eq!(read, config("{}", "00".to_string()));
        assert_eq!(
            serde_json::to_value(&read).unwrap(),
            serde_json::json!({"id": "demo", "version": 7, "sha256": "00", "payload": "{}"})
        );
    }

    #[test]
    fn products_are_named_as_the_channel_writes_them() {
        assert_eq!(serde_json::to_value(ArtifactProduct::Policy).unwrap(), "policy");
        assert_eq!("policy".parse(), Ok(ArtifactProduct::Policy));
        assert!("resources".parse::<ArtifactProduct>().is_err());
        assert!(ArtifactSnapshot::revoked(ArtifactProduct::Policy).revokes());
    }
}
