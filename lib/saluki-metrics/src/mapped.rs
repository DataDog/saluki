//! Support for "mapped" metrics whose labels are resolved dynamically at emission time.

use std::sync::Arc;

use metrics::{Label, SharedString};
use papaya::HashMap;

/// A metric that is resolved dynamically by one or more label values.
///
/// Unlike a plain metric handle, a mapped metric does not hold a single handle. Instead it holds a concurrent map
/// from a set of dynamic label values to the corresponding registered handle. Handles are registered lazily on first
/// use and cached for subsequent lookups.
///
/// The underlying map (and the fixed label set) is shared across clones, so every clone of the containing metrics
/// struct observes the same cache. Lookups are lock-free once a handle has been registered.
///
/// This type is an implementation detail of the `static_metrics` attribute macro and is not meant to be constructed
/// or used directly.
pub struct MappedMetric<H> {
    labels: Arc<Vec<Label>>,
    handles: Arc<HashMap<Box<[SharedString]>, H>>,
}

impl<H> Clone for MappedMetric<H> {
    fn clone(&self) -> Self {
        // Only the shared handles are cloned (via `Arc`), never the handles themselves, so all clones share one cache.
        Self {
            labels: Arc::clone(&self.labels),
            handles: Arc::clone(&self.handles),
        }
    }
}

impl<H> MappedMetric<H>
where
    H: Clone,
{
    /// Creates a new `MappedMetric` whose handles are registered with the given fixed labels.
    pub fn new(labels: Arc<Vec<Label>>) -> Self {
        Self {
            labels,
            handles: Arc::new(HashMap::new()),
        }
    }

    /// Returns the handle for the given dynamic label values, registering and caching it on the first lookup.
    ///
    /// `keys` are the dynamic label names and `values` their stringified values, in the same order and of the same
    /// length. The `register` closure is invoked only on a cache miss and receives the full label set (the fixed
    /// labels followed by the dynamic labels) with which to register the handle.
    pub fn get_or_register(
        &self, keys: &[&'static str], values: &[SharedString], register: impl FnOnce(&[Label]) -> H,
    ) -> H {
        let map_key: Box<[SharedString]> = values.into();
        self.handles
            .pin()
            .get_or_insert_with(map_key, || {
                let mut labels = Vec::with_capacity(self.labels.len() + values.len());
                labels.extend(self.labels.iter().cloned());
                for (key, value) in keys.iter().zip(values.iter()) {
                    labels.push(Label::new(*key, value.clone()));
                }
                register(&labels)
            })
            .clone()
    }
}
