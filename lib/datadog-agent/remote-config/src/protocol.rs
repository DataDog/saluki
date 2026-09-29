//! Private wire formats: configuration paths and the targets metadata the Agent forwards from the backend.

use std::collections::HashMap;

use aws_lc_rs::digest;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use serde::Deserialize;

use crate::ConfigId;

/// The full path a configuration arrives under.
///
/// Configurations are identified on the wire as `datadog/<org_id>/<PRODUCT>/<config_id>/<name>`, or as
/// `employee/<PRODUCT>/<config_id>/<name>` for employee-signed products, which carry no organization segment. The
/// client keys its own state by the whole path because cache advertisement echoes paths back to the Agent, and
/// publishes only the configuration ID segment as a [`ConfigId`].
///
/// The trailing name segment is conventionally the literal `config` but is not required to be, and the client never
/// acts on it: an apply status cannot be reported against it, so it is parsed and discarded.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ConfigPath {
    /// The path exactly as it appeared on the wire, which is the form cache advertisement must echo back.
    pub(crate) raw: String,

    /// The name of the product the configuration belongs to.
    pub(crate) product: String,

    /// The configuration ID, which is the only segment a subscriber sees.
    pub(crate) config_id: ConfigId,
}

impl ConfigPath {
    /// Parses a path in either form, returning `None` if it matches neither.
    ///
    /// The forms match the Agent's own validation, which rejects a request that advertises a cached path it cannot
    /// parse.
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        let segments: Vec<&str> = raw.split('/').collect();
        let (product, config_id, name) = match segments.as_slice() {
            ["datadog", org_id, product, config_id, name]
                if !org_id.is_empty() && org_id.bytes().all(|b| b.is_ascii_digit()) =>
            {
                (*product, *config_id, *name)
            }
            ["employee", product, config_id, name] => (*product, *config_id, *name),
            _ => return None,
        };
        if product.is_empty() || config_id.is_empty() || name.is_empty() {
            return None;
        }

        Some(Self {
            raw: raw.to_owned(),
            product: product.to_owned(),
            config_id: ConfigId::new(config_id),
        })
    }
}

/// What the targets metadata says about one configuration file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TargetMeta {
    /// The configuration version.
    ///
    /// A version may be bumped while the contents stay identical, which does not count as a change: the configuration
    /// is not re-decoded and any status already reported for it carries onto the new version.
    pub(crate) version: u64,

    /// The payload length in bytes.
    pub(crate) length: u64,

    /// The payload SHA-256 hash.
    ///
    /// Doubles as the change detector. A differing hash is what makes a configuration eligible for re-decoding and what
    /// clears a status previously reported for it.
    pub(crate) sha256: [u8; 32],
}

impl TargetMeta {
    /// Checks that `payload` is the file this metadata describes.
    ///
    /// This guards against a delivery bug, not an adversary: the client trusts the Agent to have verified signatures.
    pub(crate) fn verify(&self, path: &str, payload: &[u8]) -> Result<(), GenericError> {
        if payload.len() as u64 != self.length {
            return Err(generic_error!(
                "Configuration {path} is {} bytes but its targets metadata says {}.",
                payload.len(),
                self.length
            ));
        }
        if sha256(payload) != self.sha256 {
            return Err(generic_error!(
                "Configuration {path} does not match the SHA-256 hash in its targets metadata."
            ));
        }
        Ok(())
    }
}

/// The parts of the director `targets` metadata the client uses.
///
/// Signatures are not checked; see the crate's documentation on trust.
pub(crate) struct Targets {
    /// The targets version, which the client echoes back as its cursor.
    pub(crate) version: u64,

    /// Backend state the client echoes back unchanged.
    pub(crate) backend_state: Vec<u8>,

    /// Metadata for every file the Agent knows about, including ones not assigned to this client.
    ///
    /// Entries are interpreted only when a file is assigned, so a malformed entry for another client's file does not
    /// invalidate the response.
    files: HashMap<String, serde_json::Value>,
}

impl Targets {
    pub(crate) fn parse(raw: &[u8]) -> Result<Self, GenericError> {
        #[derive(Deserialize)]
        struct Signed {
            signed: RawTargets,
        }

        #[derive(Deserialize)]
        struct RawTargets {
            version: u64,
            #[serde(default)]
            targets: HashMap<String, serde_json::Value>,
            #[serde(default)]
            custom: Option<serde_json::Value>,
        }

        let Signed { signed } = serde_json::from_slice(raw).error_context("Targets metadata is malformed.")?;

        // The Go client ignores a missing or malformed backend state rather than failing the update.
        let backend_state = signed
            .custom
            .as_ref()
            .and_then(|custom| custom.get("opaque_backend_state"))
            .and_then(|state| state.as_str())
            .and_then(|state| STANDARD.decode(state).ok())
            .unwrap_or_default();

        Ok(Self {
            version: signed.version,
            backend_state,
            files: signed.targets,
        })
    }

    /// Returns the metadata for an assigned file.
    pub(crate) fn meta(&self, path: &str) -> Result<TargetMeta, GenericError> {
        #[derive(Deserialize)]
        struct RawMeta {
            length: u64,
            hashes: HashMap<String, String>,
            custom: RawCustom,
        }

        #[derive(Deserialize)]
        struct RawCustom {
            v: u64,
        }

        let entry = self
            .files
            .get(path)
            .ok_or_else(|| generic_error!("Configuration {path} is assigned but missing from the targets metadata."))?;
        let meta = RawMeta::deserialize(entry)
            .with_error_context(|| format!("Targets metadata for configuration {path} is malformed."))?;
        let hash = meta
            .hashes
            .get("sha256")
            .ok_or_else(|| generic_error!("Targets metadata for configuration {path} has no SHA-256 hash."))?;
        let mut sha256 = [0; 32];
        if hash.len() != 64 || faster_hex::hex_decode(hash.as_bytes(), &mut sha256).is_err() {
            return Err(generic_error!(
                "Targets metadata for configuration {path} has a malformed SHA-256 hash."
            ));
        }

        Ok(TargetMeta {
            version: meta.custom.v,
            length: meta.length,
            sha256,
        })
    }
}

/// Returns the version of a TUF root, without checking its signatures.
pub(crate) fn root_version(raw: &[u8]) -> Result<u64, GenericError> {
    #[derive(Deserialize)]
    struct Signed {
        signed: Root,
    }

    #[derive(Deserialize)]
    struct Root {
        version: u64,
    }

    let Signed { signed } = serde_json::from_slice(raw).error_context("TUF root is malformed.")?;
    Ok(signed.version)
}

/// Hashes `payload` with SHA-256.
///
/// This uses `aws-lc-rs` so that, in a FIPS build, the hash is computed by the same validated module as TLS.
pub(crate) fn sha256(payload: &[u8]) -> [u8; 32] {
    let digest = digest::digest(&digest::SHA256, payload);
    let mut hash = [0; 32];
    hash.copy_from_slice(digest.as_ref());
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_path_forms() {
        let datadog = ConfigPath::parse("datadog/2/APM_SAMPLING/sampling.v1/config").unwrap();
        assert_eq!(datadog.product, "APM_SAMPLING");
        assert_eq!(&*datadog.config_id, "sampling.v1");

        let employee = ConfigPath::parse("employee/APM_SEMANTIC_CORE_DD/semantic.v1/backup").unwrap();
        assert_eq!(employee.product, "APM_SEMANTIC_CORE_DD");
        assert_eq!(&*employee.config_id, "semantic.v1");
        assert_eq!(employee.raw, "employee/APM_SEMANTIC_CORE_DD/semantic.v1/backup");
    }

    #[test]
    fn rejects_malformed_paths() {
        for raw in [
            "",
            "datadog/APM_SAMPLING/sampling.v1/config",
            "datadog/x2/APM_SAMPLING/sampling.v1/config",
            "datadog//APM_SAMPLING/sampling.v1/config",
            "datadog/2/APM_SAMPLING/sampling.v1/config/extra",
            "employee/2/APM_SEMANTIC_CORE_DD/semantic.v1/config",
            "employee/APM_SEMANTIC_CORE_DD//config",
            "employee/APM_SEMANTIC_CORE_DD/semantic.v1/",
            "other/APM_SEMANTIC_CORE_DD/semantic.v1/config",
        ] {
            assert!(ConfigPath::parse(raw).is_none(), "{raw} should not parse");
        }
    }

    #[test]
    fn parses_targets_leniently_and_file_metadata_strictly() {
        let hash = faster_hex::hex_string(&sha256(b"{}"));
        let raw = serde_json::json!({
            "signed": {
                "version": 7,
                "custom": {"opaque_backend_state": STANDARD.encode(b"state")},
                "targets": {
                    "employee/P/good/config": {"length": 2, "hashes": {"sha256": hash}, "custom": {"v": 3}},
                    "employee/P/unversioned/config": {"length": 2, "hashes": {"sha256": hash}},
                    "employee/P/unhashed/config": {"length": 2, "hashes": {}, "custom": {"v": 3}},
                    "employee/P/badhash/config": {"length": 2, "hashes": {"sha256": "zz"}, "custom": {"v": 3}},
                },
            },
            "signatures": [],
        });
        let targets = Targets::parse(raw.to_string().as_bytes()).unwrap();

        assert_eq!(targets.version, 7);
        assert_eq!(targets.backend_state, b"state");
        let meta = targets.meta("employee/P/good/config").unwrap();
        assert_eq!((meta.version, meta.length, meta.sha256), (3, 2, sha256(b"{}")));
        meta.verify("employee/P/good/config", b"{}").unwrap();
        assert!(meta.verify("employee/P/good/config", b"[]").is_err());
        assert!(meta.verify("employee/P/good/config", b"{ }").is_err());

        for path in [
            "employee/P/unversioned/config",
            "employee/P/unhashed/config",
            "employee/P/badhash/config",
            "employee/P/absent/config",
        ] {
            assert!(targets.meta(path).is_err(), "{path} should be rejected");
        }
    }

    #[test]
    fn reads_root_versions() {
        assert_eq!(
            root_version(br#"{"signed":{"version":4,"_type":"root"},"signatures":[]}"#).unwrap(),
            4
        );
        assert!(root_version(b"{}").is_err());
    }
}
