//! Component-native configuration for the checks IPC source.
//!
//! Mirrors `ChecksIPCConfiguration` in `saluki-components` with the source key name and the
//! `Deserialize` impl stripped.

use saluki_io::net::ListenAddress;

/// Configuration for the checks IPC source component.
///
/// Mirrors `ChecksIPCConfiguration` in `saluki-components`.
///
/// # Derive deviation
///
/// This struct reuses [`ListenAddress`] from `saluki-io` to keep the field type identical to the
/// component struct for a clean cutover. [`ListenAddress`] does not implement `PartialEq`,
/// `serde::Serialize`, or `Default`, so this struct cannot derive them either. It derives `Clone`
/// and `Debug` and provides a manual [`Default`]. When `ListenAddress` gains those impls upstream,
/// this struct should be brought in line with the rest of the crate. The lack of `Serialize` means
/// this slice is, for now, omitted from the future `/config/internal` view.
#[derive(Clone, Debug)]
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
