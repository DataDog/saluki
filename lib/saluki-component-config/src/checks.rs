//! Component-native configuration for the checks IPC source.
//!
//! Mirrors `ChecksIPCConfiguration` in `saluki-components` with the source key name and the
//! `Deserialize` impl stripped.

use saluki_io::net::ListenAddress;

/// Configuration for the checks IPC source component.
///
/// Mirrors `ChecksIPCConfiguration` in `saluki-components`. Reuses [`ListenAddress`] from
/// `saluki-io` to keep the field type identical to the component struct for a clean cutover.
/// `ListenAddress` has no sensible universal default, so [`Default`] is implemented manually.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct ChecksIPCConfig {
    /// The gRPC endpoint the checks IPC server listens on.
    ///
    /// Defaults to `tcp://0.0.0.0:5105`.
    pub grpc_endpoint: ListenAddress,
}

impl Default for ChecksIPCConfig {
    fn default() -> Self {
        Self {
            grpc_endpoint: ListenAddress::any_tcp(5105),
        }
    }
}
