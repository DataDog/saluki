//! Resolved service with template applied.

use crate::config::ServiceTemplate;

/// A resolved service with its template applied.
#[derive(Clone, Debug)]
pub struct ResolvedService {
    /// The unique name of this service.
    pub name: String,

    /// The resolved template for this service.
    pub template: ServiceTemplate,

    /// Indices of downstream services in the architecture's service list.
    pub downstream_indices: Vec<usize>,
}
