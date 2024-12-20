use indexmap::Equivalent;
use stringtheory::MetaString;
use tracing::warn;

/// External data associated with a workload entity.
///
/// An external data string is a comma-separated list of key/value pairs, where each key represents a particular aspect
/// of the workload entity. The following keys are supported:
///
/// - `it-<true/false>`: A boolean value indicating whether the entity is an init container.
/// - `pu-<pod_uid>`: The pod UID associated with the entity.
/// - `cn-<container_name>`: The container name associated with the entity.
///
/// `ExternalData` represents an owned variant of this data. For parsing external data strings without allocating, see [`ExternalDataRef`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExternalData {
    pod_uid: MetaString,
    container_name: MetaString,
}

impl ExternalData {
    /// Creates a new `ExternalData` instance.
    pub fn new(pod_uid: MetaString, container_name: MetaString) -> Self {
        Self {
            pod_uid,
            container_name,
        }
    }
}

impl std::hash::Hash for ExternalData {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (*self.pod_uid).hash(state);
        (*self.container_name).hash(state);
    }
}

impl<'a> Equivalent<ExternalData> for ExternalDataRef<'a> {
    fn equivalent(&self, other: &ExternalData) -> bool {
        self.pod_uid == &*other.pod_uid && self.container_name == &*other.container_name
    }
}

/// An external data reference based on borrowed data.
///
/// This is a borrowed version of `ExternalData` that is used to parse external data strings without needing to allocate
/// backing storage for any of the fields, and can be used to look up map entries (such as when using `HashMap`) when
/// the key is [`ExternalData`].
pub struct ExternalDataRef<'a> {
    pod_uid: &'a str,
    container_name: &'a str,
}

impl<'a> ExternalDataRef<'a> {
    /// Creates a new `ExternalDataRef` from a raw string.
    ///
    /// If the external data is not valid, `None` is returned.
    pub fn from_raw(raw: &'a str) -> Option<Self> {
        if raw.is_empty() {
            return None;
        }

        let mut data = Self {
            pod_uid: "",
            container_name: "",
        };

        let parts = raw.split(',');
        for part in parts {
            if part.len() < 4 {
                // All key/value pairs have a prefix of `xx-` where `xx` is some short code, so we basically can't have
                // any real key/value pair that's less than four characters overall.
                warn!("Parsed external data with invalid key/value pair: {}", part);
                continue;
            }

            let key = &part[0..3];
            let value = &part[3..];

            match key {
                "it-" => {
                    // We explicitly ignore this key because we don't actually need it, based on how we capture and
                    // construct our mapping table.
                }
                "pu-" => data.pod_uid = value,
                "cn-" => data.container_name = value,
                _ => {
                    // Unknown key, ignore.
                    warn!("Parsed external data with unknown key: {}", key);
                }
            }
        }

        Some(data)
    }
}

impl<'a> std::hash::Hash for ExternalDataRef<'a> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.pod_uid.hash(state);
        self.container_name.hash(state);
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::hash_map::DefaultHasher,
        hash::{Hash as _, Hasher as _},
    };

    use proptest::prelude::*;

    use super::*;

    proptest! {
        #[test]
        fn property_test_identical_hash_impls(pod_uid in "[a-z0-9]{1,64}", container_name in "[a-z0-9]{1,64}") {
            let external_data = ExternalData::new(pod_uid.clone().into(), container_name.clone().into());
            let external_data_ref = ExternalDataRef {
                pod_uid: &pod_uid,
                container_name: &container_name,
            };

            let mut hasher = DefaultHasher::new();
            external_data.hash(&mut hasher);
            let external_data_hash = hasher.finish();

            let mut hasher = DefaultHasher::new();
            external_data_ref.hash(&mut hasher);
            let external_data_ref_hash = hasher.finish();

            assert_eq!(external_data_hash, external_data_ref_hash);
        }
    }
}
