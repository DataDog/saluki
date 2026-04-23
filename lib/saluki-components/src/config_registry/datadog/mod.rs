//! Datadog Agent configuration registry entries.

/// Proxy configuration annotations.
pub mod proxy;

use std::sync::LazyLock;

use super::{ConfigKey, SalukiAnnotation};

/// All saluki annotations across every sub-system, in registration order.
///
/// The source of truth for which config keys saluki knows about and how they are consumed.
/// Used by the smoke test runner and runtime unknown-key detection.
pub static ALL_ANNOTATIONS: LazyLock<Vec<&'static SalukiAnnotation>> = LazyLock::new(|| {
    let mut v = Vec::new();
    v.extend_from_slice(proxy::ALL);
    v
});

/// All resolved [`ConfigKey`] entries, derived from [`ALL_ANNOTATIONS`] at first access.
///
/// Provides a flattened, owned view suitable for runtime unknown-key detection.
pub static ALL_KEYS: LazyLock<Vec<ConfigKey>> =
    LazyLock::new(|| ALL_ANNOTATIONS.iter().map(|a| ConfigKey::from(*a)).collect());
