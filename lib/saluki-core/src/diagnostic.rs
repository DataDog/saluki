//! Diagnostics collection.

use std::sync::Arc;

/// A handle for exposing diagnostics from a component.
///
///
/// Components assert a handle where artifact name is suitable, but not guaranteed, to be
/// used a filename
///
/// # Example:
///
/// ```ignore
/// DataspaceRegistry::try_current()?
///     .assert(
///         DiagnosticHandle::new("my_artifact.json", move || {
///             serde_json::to_vec(&my_state).unwrap_or_default()
///         }),
///         "diag-my-component",
///     );
/// ```
///

#[derive(Clone)]
pub struct DiagnosticHandle {
    artifact_name: String,
    collect_fn: Arc<dyn Fn() -> Vec<u8> + Send + Sync>,
}

impl DiagnosticHandle {
    /// Creates a new handle with the given artifact name and collection closure.
    /// `collect_fn` must run synchronously and reasonably fast to produce artifact bytes
    /// as it can delay entire diagnostic response
    pub fn new(artifact_name: impl Into<String>, collect_fn: impl Fn() -> Vec<u8> + Send + Sync + 'static) -> Self {
        Self {
            artifact_name: artifact_name.into(),
            collect_fn: Arc::new(collect_fn),
        }
    }

    /// Returns the artifact name.
    pub fn artifact_name(&self) -> &str {
        &self.artifact_name
    }

    /// Collects and returns the artifact bytes.
    pub fn collect(&self) -> Vec<u8> {
        (self.collect_fn)()
    }
}
