use std::collections::HashMap;

use figment::{
    value::{Dict, Map, Value},
    Figment, Metadata, Profile,
};
use tracing::error;

pub mod resolver;
use self::resolver::Resolver;

mod errors;
pub use self::errors::Error;

const SECRET_REF_PREFIX: &str = "ENC[";
const SECRET_REF_SUFFIX: &str = "]";

/// A provider that handles the resolution of secrets in a configuration.
pub struct Provider {
    metadata: Metadata,
    secrets: Dict,
}

impl Provider {
    /// Creates a new `Provider` instance, resolving any secrets found in the given source data.
    ///
    /// # Errors
    ///
    /// If there is an issue while resolving the secrets, an error will be returned.
    pub async fn new<R: Resolver>(resolver: R, source: &Figment) -> Result<Self, Error> {
        let mut provider = Self {
            metadata: Metadata::from("Secrets", resolver.source()),
            secrets: Dict::default(),
        };

        // Extract any secret references from the input data, simply returning early if we find none.
        let secret_refs = extract_secret_refs(source);
        if secret_refs.is_empty() {
            return Ok(provider);
        }

        // Try and resolve the detected secrets using the provider resolver.
        //
        // For any resolved secrets that we get back, we'll add them to our internal dictionary.
        let resolved_secrets = resolver.resolve(secret_refs).await?;

        for (key, value) in resolved_secrets {
            set_nested_dict_entry(&mut provider.secrets, key, value);
        }

        Ok(provider)
    }
}

impl figment::Provider for Provider {
    fn metadata(&self) -> Metadata {
        self.metadata.clone()
    }

    fn data(&self) -> Result<Map<Profile, Dict>, figment::Error> {
        let mut data = Map::new();
        data.insert(Profile::default(), self.secrets.clone());

        Ok(data)
    }
}

/// A path abstraction for nested dictionary keys.
///
/// This is used to represent the individual segments of a nested dictionary key, such that we can properly distinguish
/// the different levels of nesting compared to using a scheme where a single, long string is built using an arbitrary
/// separator character (such as a period), as such a scheme is at risk of producing incorrect results if any individual
/// segment also contains the chosen separator character.
#[derive(Eq, Hash, PartialEq)]
pub struct KeyPath {
    segments: Vec<String>,
}

impl KeyPath {
    fn root() -> Self {
        Self { segments: Vec::new() }
    }

    fn push(&self, segment: &str) -> Self {
        Self {
            segments: {
                let mut segments = self.segments.clone();
                segments.push(segment.to_string());
                segments
            },
        }
    }

    fn into_segments(self) -> Vec<String> {
        self.segments
    }
}

fn set_nested_dict_entry(dict: &mut Dict, key: KeyPath, value: String) {
    fn get_or_create(dict: &mut Dict, key: String) -> Option<&mut Dict> {
        let entry = dict.entry(key).or_insert_with(|| Dict::default().into());
        if let Value::Dict(_, dict) = entry {
            Some(dict)
        } else {
            None
        }
    }

    let mut current_dict = dict;
    let mut segments = key.into_segments();

    for segment in segments.drain(..segments.len() - 1) {
        match get_or_create(current_dict, segment) {
            Some(dict) => current_dict = dict,
            None => return,
        }
    }

    let leaf_key = segments
        .pop()
        .expect("parts should always have at least one element left");
    current_dict.insert(leaf_key, value.into());
}

fn extract_secret_refs(source: &Figment) -> HashMap<KeyPath, String> {
    let mut secrets = HashMap::new();

    match source.extract::<Value>() {
        Ok(value) => match value.as_dict() {
            Some(dict) => extract_secret_refs_inner(KeyPath::root(), dict, &mut secrets),
            None => {
                error!("Failed to extract configuration values as a dictionary during secrets resolution. No secrets will be resolved.");
            }
        },
        Err(e) => {
            error!(error = %e, "Failed to iterate over existing configuration values during secrets resolution. No secrets will be resolved.");

            return secrets;
        }
    };

    secrets
}

fn extract_secret_refs_inner(parent_path: KeyPath, dict: &Dict, secrets: &mut HashMap<KeyPath, String>) {
    for (key, value) in dict.iter() {
        let current_path = parent_path.push(key);

        match value {
            Value::String(_, value) => {
                if let Some(secret_ref) = parse_secret_ref(value) {
                    secrets.insert(current_path, secret_ref.to_string());
                }
            }
            Value::Dict(_, dict) => extract_secret_refs_inner(current_path, dict, secrets),
            _ => {}
        }
    }
}

fn parse_secret_ref(value: &str) -> Option<&str> {
    // We get our the reference extraction and validity check all in one: as we strip the prefix/suffix away, we only
    // get `Some(...)` if both the prefix and suffix were present.
    value
        .strip_prefix(SECRET_REF_PREFIX)
        .and_then(|s| s.strip_suffix(SECRET_REF_SUFFIX))
}

#[cfg(test)]
mod tests {
    use figment::{providers::Serialized, Source};
    use serde_json::json;

    use super::*;

    #[derive(Default)]
    struct InMemoryResolver {
        secrets: HashMap<&'static str, &'static str>,
    }

    impl InMemoryResolver {
        fn add_secret(&mut self, secret_ref: &'static str, value: &'static str) {
            self.secrets.insert(secret_ref, value);
        }
    }

    impl Resolver for InMemoryResolver {
        fn source(&self) -> Source {
            Source::Custom("secrets[in-memory]".to_string())
        }

        async fn resolve(&self, secrets: HashMap<KeyPath, String>) -> Result<HashMap<KeyPath, String>, Error> {
            let mut resolved = HashMap::new();
            for secret in secrets {
                if let Some(value) = self.secrets.get(secret.1.as_str()) {
                    resolved.insert(secret.0, value.to_string());
                }
            }

            Ok(resolved)
        }
    }

    #[tokio::test]
    async fn provider_no_secrets() {
        let source = Figment::new();
        let resolver = InMemoryResolver::default();
        let provider = Provider::new(resolver, &source)
            .await
            .expect("should not fail to resolve secrets in empty source config");

        assert!(provider.secrets.is_empty());
    }

    #[tokio::test]
    async fn provider_basic_secret() {
        let source = Figment::new().adjoin(("my-secret", "ENC[my-secret-ref]"));

        let mut resolver = InMemoryResolver::default();
        resolver.add_secret("my-secret-ref", "my-secret-value");

        let provider = Provider::new(resolver, &source)
            .await
            .expect("should not fail to resolve secrets in non-empty source config");

        assert!(!provider.secrets.is_empty());
        assert_eq!(
            provider.secrets.get("my-secret").and_then(|v| v.as_str()),
            Some("my-secret-value")
        );
    }

    #[tokio::test]
    async fn provider_nested_secret() {
        let source = Figment::new().adjoin(("server.api_key", "ENC[server-api-key-ref]"));

        let mut resolver = InMemoryResolver::default();
        resolver.add_secret("server-api-key-ref", "my-server-api-key-value");

        let provider = Provider::new(resolver, &source)
            .await
            .expect("should not fail to resolve secrets in non-empty source config");

        assert!(!provider.secrets.is_empty());

        let secrets_value = Value::from(provider.secrets.clone());
        assert_eq!(
            secrets_value.find("server.api_key"),
            Some(Value::from("my-server-api-key-value"))
        );
    }

    #[tokio::test]
    async fn provider_nested_secret_period_separators() {
        let tenant1_url = "https://tenant1.saas.io";
        let source_secrets = json!({
            "additional_endpoints": {
                tenant1_url: "ENC[tenant1-api-key-ref]"
            }
        });

        let source = Figment::new().adjoin(Serialized::defaults(source_secrets));

        let mut resolver = InMemoryResolver::default();
        resolver.add_secret("tenant1-api-key-ref", "tenant1-api-key-value");

        let provider = Provider::new(resolver, &source)
            .await
            .expect("should not fail to resolve secrets in non-empty source config");

        assert!(!provider.secrets.is_empty());

        // We have to drop down to the level of `Dict`, and avoid using `Value::find`, due to the period separators in
        // `tenant1_url`, but this simulates what would happen when the configuration is deserialized to a typed struct,
        // etc.
        let additional_endpoints = provider
            .secrets
            .get("additional_endpoints")
            .expect("should have additional_endpoints")
            .as_dict()
            .expect("additional_endpoints should be a dictionary");

        assert_eq!(
            additional_endpoints.get(tenant1_url),
            Some(&Value::from("tenant1-api-key-value"))
        );
    }

    #[test]
    fn parse_secret_ref_valid() {
        let valid_secret_keys = &[
            "secret",
            "/path/to/secret/file",
            "vault://services/my-data-plane/keys/api_key#/data/value",
        ];

        for secret_key in valid_secret_keys {
            let wrapped_secret_key = format!("{}{}{}", SECRET_REF_PREFIX, secret_key, SECRET_REF_SUFFIX);
            assert_eq!(parse_secret_ref(&wrapped_secret_key), Some(*secret_key));
        }
    }

    #[test]
    fn parse_secret_ref_invalid() {
        assert_eq!(parse_secret_ref("secret]"), None);
        assert_eq!(parse_secret_ref("ENC[secret"), None);
        assert_eq!(parse_secret_ref("secret"), None);
    }
}
