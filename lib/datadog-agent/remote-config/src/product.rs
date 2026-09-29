use std::fmt;
use std::ops::Deref;

/// Identifies one configuration assigned to a product.
///
/// A configuration's identity is its configuration ID: the `semantic.v1` in
/// `employee/APM_SEMANTIC_CORE_DD/semantic.v1/config`. Nothing else about where the configuration came from is part of
/// its identity, because nothing else is something the protocol can act on -- an apply status is reported against a
/// product and this ID alone.
///
/// Ordering is the order in which the client presents a product's configurations to its decoder.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ConfigId(pub(crate) String);

impl ConfigId {
    /// Creates a configuration ID, such as `semantic.v1`, for calling a decoder directly in tests.
    ///
    /// The client creates every ID a decoder receives in production; nothing in this crate accepts a `ConfigId` from a
    /// subscriber, so an ID created here cannot reach the client. Any string is accepted.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl Deref for ConfigId {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl fmt::Display for ConfigId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use crate::product::ConfigId;

    #[test]
    fn config_id_derefs_to_str_for_matching() {
        let config_id = ConfigId::new("semantic.v1");

        assert!(matches!(&*config_id, "semantic.v1"));
        assert_eq!("semantic.v1", config_id.to_string());
    }

    #[test]
    fn config_ids_order_ascending_by_id() {
        let mut ids = [
            ConfigId("registry.v3".to_string()),
            ConfigId("registry.v1".to_string()),
            ConfigId("registry.v2".to_string()),
        ];
        ids.sort();

        let sorted: Vec<&str> = ids.iter().map(|id| &**id).collect();
        assert_eq!(vec!["registry.v1", "registry.v2", "registry.v3"], sorted);
    }
}
