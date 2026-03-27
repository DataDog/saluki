use std::collections::HashMap;

use facet_value::Value;
use tracing::error;

pub mod resolver;
use self::resolver::Resolver;

mod errors;
pub use self::errors::Error;

const SECRET_REF_PREFIX: &str = "ENC[";
const SECRET_REF_SUFFIX: &str = "]";

/// A secrets overlay that resolves secret references in configuration values.
///
/// After resolution, the overlay can be converted to a `Value` for merging into the configuration.
pub struct SecretsOverlay {
    secrets: Value,
}

impl SecretsOverlay {
    /// Creates a new `SecretsOverlay` instance, resolving any secrets found in the given source data.
    ///
    /// # Errors
    ///
    /// If there is an issue while resolving the secrets, an error will be returned.
    pub async fn new<R: Resolver>(resolver: R, source: &Value) -> Result<Self, Error> {
        let mut overlay = Self {
            secrets: Value::from(facet_value::VObject::new()),
        };

        // Extract any secret references from the input data, simply returning early if we find none.
        let secret_refs = extract_secret_refs(source);
        if secret_refs.is_empty() {
            return Ok(overlay);
        }

        // Try and resolve the detected secrets using the provider resolver.
        //
        // For any resolved secrets that we get back, we'll add them to our internal value.
        let resolved_secrets = resolver.resolve(secret_refs).await?;

        for (key, value) in resolved_secrets {
            set_nested_value(&mut overlay.secrets, key, value);
        }

        Ok(overlay)
    }

    /// Consumes the overlay and returns the resolved secrets as a `Value`.
    pub fn into_value(self) -> Value {
        self.secrets
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

fn set_nested_value(root: &mut Value, key: KeyPath, value: String) {
    let mut segments = key.into_segments();
    if segments.is_empty() {
        return;
    }

    let mut current = root;

    for segment in segments.drain(..segments.len() - 1) {
        // Ensure current is an object.
        if current.as_object().is_none() {
            *current = Value::from(facet_value::VObject::new());
        }

        let obj = current.as_object_mut().unwrap();
        if obj.get(&segment).is_none() {
            obj.insert(&segment, Value::from(facet_value::VObject::new()));
        }
        current = obj.get_mut(&segment).unwrap();
    }

    let leaf_key = segments
        .pop()
        .expect("parts should always have at least one element left");

    if current.as_object().is_none() {
        *current = Value::from(facet_value::VObject::new());
    }
    current.as_object_mut().unwrap().insert(&leaf_key, Value::from(value));
}

fn extract_secret_refs(source: &Value) -> HashMap<KeyPath, String> {
    let mut secrets = HashMap::new();

    match source.as_object() {
        Some(obj) => extract_secret_refs_inner(KeyPath::root(), obj, &mut secrets),
        None => {
            error!("Failed to extract configuration values as an object during secrets resolution. No secrets will be resolved.");
        }
    }

    secrets
}

fn extract_secret_refs_inner(parent_path: KeyPath, obj: &facet_value::VObject, secrets: &mut HashMap<KeyPath, String>) {
    for (key, value) in obj.iter() {
        let current_path = parent_path.push(key.as_str());

        if let Some(s) = value.as_string() {
            if let Some(secret_ref) = parse_secret_ref(s.as_str()) {
                secrets.insert(current_path, secret_ref.to_string());
            }
        } else if let Some(nested_obj) = value.as_object() {
            extract_secret_refs_inner(current_path, nested_obj, secrets);
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
    use facet_value::value;

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
        let source = value!({});
        let resolver = InMemoryResolver::default();
        let overlay = SecretsOverlay::new(resolver, &source)
            .await
            .expect("should not fail to resolve secrets in empty source config");

        let secrets_value = overlay.into_value();
        assert!(secrets_value.as_object().unwrap().is_empty());
    }

    #[tokio::test]
    async fn provider_basic_secret() {
        let source = value!({
            "my-secret": "ENC[my-secret-ref]"
        });

        let mut resolver = InMemoryResolver::default();
        resolver.add_secret("my-secret-ref", "my-secret-value");

        let overlay = SecretsOverlay::new(resolver, &source)
            .await
            .expect("should not fail to resolve secrets in non-empty source config");

        let secrets_value = overlay.into_value();
        let obj = secrets_value.as_object().unwrap();
        assert_eq!(
            obj.get("my-secret").and_then(|v| v.as_string()).map(|s| s.as_str()),
            Some("my-secret-value")
        );
    }

    #[tokio::test]
    async fn provider_nested_secret() {
        let source = value!({
            "server": {
                "api_key": "ENC[server-api-key-ref]"
            }
        });

        let mut resolver = InMemoryResolver::default();
        resolver.add_secret("server-api-key-ref", "my-server-api-key-value");

        let overlay = SecretsOverlay::new(resolver, &source)
            .await
            .expect("should not fail to resolve secrets in non-empty source config");

        let secrets_value = overlay.into_value();
        let server = secrets_value
            .as_object()
            .unwrap()
            .get("server")
            .unwrap()
            .as_object()
            .unwrap();
        assert_eq!(
            server.get("api_key").and_then(|v| v.as_string()).map(|s| s.as_str()),
            Some("my-server-api-key-value")
        );
    }

    #[tokio::test]
    async fn provider_nested_secret_period_separators() {
        let tenant1_url = "https://tenant1.saas.io";
        let source = value!({
            "additional_endpoints": {
                (tenant1_url): "ENC[tenant1-api-key-ref]"
            }
        });

        let mut resolver = InMemoryResolver::default();
        resolver.add_secret("tenant1-api-key-ref", "tenant1-api-key-value");

        let overlay = SecretsOverlay::new(resolver, &source)
            .await
            .expect("should not fail to resolve secrets in non-empty source config");

        let secrets_value = overlay.into_value();
        // We have to drill down through the object to avoid issues with period separators in the key.
        let additional_endpoints = secrets_value
            .as_object()
            .unwrap()
            .get("additional_endpoints")
            .expect("should have additional_endpoints")
            .as_object()
            .expect("additional_endpoints should be an object");

        assert_eq!(
            additional_endpoints
                .get(tenant1_url)
                .and_then(|v| v.as_string())
                .map(|s| s.as_str()),
            Some("tenant1-api-key-value")
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
