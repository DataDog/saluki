//! Component-native configuration for the Cluster Agent forwarder.

/// Configuration for the Cluster Agent forwarder.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct ClusterAgentConfig {
    /// Whether Cluster Agent forwarding is enabled.
    pub enabled: bool,

    /// Explicit Cluster Agent HTTPS endpoint URL.
    ///
    /// Takes precedence over `kubernetes_service_name`. When empty, falls back to Kubernetes
    /// service environment variable resolution.
    pub url: String,

    /// Kubernetes service name used to resolve the Cluster Agent endpoint via env vars.
    ///
    /// Defaults to `datadog-cluster-agent`.
    pub kubernetes_service_name: String,

    /// Bearer token for Cluster Agent authentication.
    pub auth_token: String,
}
