//! Service definition configuration.

use serde::Deserialize;

/// A service definition in the architecture.
#[derive(Clone, Debug, Deserialize)]
pub struct ServiceDefinition {
    /// The unique name of this service instance.
    pub name: String,

    /// The template type to use for this service.
    #[serde(rename = "type")]
    pub template_type: String,

    /// The names of downstream services this service calls.
    #[serde(default)]
    pub downstream: Vec<String>,
}
