//! On-demand diagnostic artifact collection.

use std::sync::Arc;

/// A named, on-demand producer of diagnostic artifact bytes.
///
/// A collector pairs an artifact name with a synchronous closure that produces the artifact's bytes when
/// invoked. Collectors are registered through a [`DiagnosticsEmitter`][super::DiagnosticsEmitter] and gathered on
/// demand by whichever subsystem is responsible for assembling diagnostic artifacts.
///
/// The artifact name is suitable, but not guaranteed, to be used as a filename.
#[derive(Clone)]
pub struct DiagnosticCollector {
    artifact_name: String,
    collect_fn: Arc<dyn Fn() -> Vec<u8> + Send + Sync>,
}

impl DiagnosticCollector {
    /// Creates a new collector with the given artifact name and collection closure.
    ///
    /// `collect_fn` runs synchronously and must return promptly, as it can delay the collection of artifacts for the
    /// whole system.
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
