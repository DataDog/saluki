use std::collections::{hash_map::Iter, HashMap};

use snafu::Snafu;

use crate::{CalculatedBounds, MemoryBounds, MemoryBoundsBuilder, MemoryGrant};

#[derive(Debug, Eq, PartialEq, Snafu)]
pub enum VerifierError {
    #[snafu(display("invalid component bounds for {}: {}", component_name, reason))]
    InvalidComponentBounds { component_name: String, reason: String },

    #[snafu(display(
        "insufficient memory available to meet minimum required bytes: {} < {}",
        available_bytes,
        minimum_required_bytes
    ))]
    InsufficientMinimumMemory {
        available_bytes: usize,
        minimum_required_bytes: usize,
    },

    #[snafu(display("firm limit exceeds available memory: {} < {}", available_bytes, firm_limit_bytes))]
    FirmLimitExceedsAvailable {
        available_bytes: usize,
        firm_limit_bytes: usize,
    },
}

/// Verified bounds.
///
/// This structure contains the original set of parameters -- the grant, verify mode, and verified components -- used
/// when verifying bounds in `BoundsVerifier::verify`. It can then be used to feed into additional components, such as
/// `MemoryPartitioner`, to ensure that the same parameters are used, avoiding any potential misconfiguration.
pub struct VerifiedBounds {
    grant: MemoryGrant,
    components: HashMap<String, CalculatedBounds>,
}

impl VerifiedBounds {
    /// Total number of bytes available for allocation.
    pub fn available_bytes(&self) -> usize {
        self.grant.effective_limit_bytes()
    }

    /// Returns the number of components that were verified.
    pub fn components_len(&self) -> usize {
        self.components.len()
    }

    /// Returns an iterator over the components and their memory bounds.
    pub fn components(&self) -> Iter<'_, String, CalculatedBounds> {
        self.components.iter()
    }

    /// Returns the total number of minimum required bytes for all components that were verified.
    pub fn minimum_required_bytes(&self) -> usize {
        self.components.values().map(|cb| cb.minimum_required).sum()
    }

    /// Returns the total firm limit, in bytes, for all components that were verified.
    pub fn firm_limit_bytes(&self) -> usize {
        self.components.values().map(|cb| cb.firm_limit).sum()
    }
}

/// Memory bounds verifier.
pub struct BoundsVerifier<'a> {
    grant: MemoryGrant,
    components: HashMap<String, &'a dyn MemoryBounds>,
}

impl<'a> BoundsVerifier<'a> {
    /// Creates a new memory bounds verifier with the given memory grant.
    pub fn from_grant(grant: MemoryGrant) -> Self {
        Self {
            grant,
            components: HashMap::new(),
        }
    }

    /// Adds a bounded component to the verifier.
    pub fn add_component(&mut self, name: String, component: &'a dyn MemoryBounds) {
        self.components.insert(name, component);
    }

    /// Validates that all components are able to respect the calculated effective limit.
    ///
    /// If validation succeeds, a `MemoryGrant` is returned which provides information about the effective limit that
    /// can be used for allocating memory.
    ///
    /// ## Errors
    ///
    /// A number of invalid conditions are checked and will cause an error to be returned:
    ///
    /// - when a component has invalid bounds (e.g. minimum required bytes higher than firm limit)
    /// - when the combined total of the firm limit for all components exceeds the effective limit
    pub fn verify(self) -> Result<VerifiedBounds, VerifierError> {
        let available_bytes = self.grant.effective_limit_bytes();
        let mut components = HashMap::new();

        let mut total_minimum_required_bytes: usize = 0;
        let mut total_firm_limit_bytes: usize = 0;

        for (name, component) in &self.components {
            let mut bounds_builder = MemoryBoundsBuilder::default();
            component.calculate_bounds(&mut bounds_builder);
            let component_bounds = bounds_builder.calculated_bounds();

            if component_bounds.minimum_required > component_bounds.firm_limit {
                return Err(VerifierError::InvalidComponentBounds {
                    component_name: name.clone(),
                    reason: "minimum required bytes exceeds firm limit".to_string(),
                });
            }

            total_minimum_required_bytes =
                total_minimum_required_bytes.saturating_add(component_bounds.minimum_required);
            total_firm_limit_bytes = total_firm_limit_bytes.saturating_add(component_bounds.firm_limit);

            components.insert(name.clone(), component_bounds);
        }

        // Check to ensure that the effective limit is sufficient to meet the minimum required bytes, and then do the
        // same for the firm limit.
        if available_bytes < total_minimum_required_bytes {
            return Err(VerifierError::InsufficientMinimumMemory {
                available_bytes,
                minimum_required_bytes: total_minimum_required_bytes,
            });
        }

        if available_bytes < total_firm_limit_bytes {
            return Err(VerifierError::FirmLimitExceedsAvailable {
                available_bytes,
                firm_limit_bytes: total_firm_limit_bytes,
            });
        }

        Ok(VerifiedBounds {
            grant: self.grant,
            components,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{BoundsVerifier, VerifiedBounds, VerifierError};
    use crate::{test_util::BoundedComponent, MemoryGrant};

    fn get_grant(initial_limit_bytes: usize) -> MemoryGrant {
        const SLOP_FACTOR: f64 = 0.25;

        MemoryGrant::with_slop_factor(initial_limit_bytes, SLOP_FACTOR).expect("should never be invalid")
    }

    fn verify_component(
        initial_limit_bytes: usize, component: &BoundedComponent,
    ) -> (MemoryGrant, Result<VerifiedBounds, VerifierError>) {
        let initial_grant = get_grant(initial_limit_bytes);

        let mut verifier = BoundsVerifier::from_grant(initial_grant);
        verifier.add_component("component".to_string(), component);

        (initial_grant, verifier.verify())
    }

    #[test]
    fn test_invalid_component_bounds() {
        let bounded = BoundedComponent::new(Some(20), 10);
        let initial_grant = MemoryGrant::effective(1).expect("should never be invalid");

        let mut verifier = BoundsVerifier::from_grant(initial_grant);
        verifier.add_component("component".to_string(), &bounded);

        assert_eq!(
            verifier.verify().err(),
            Some(VerifierError::InvalidComponentBounds {
                component_name: "component".to_string(),
                reason: "minimum required bytes exceeds firm limit".to_string(),
            })
        );
    }

    #[test]
    fn test_verify() {
        let minimum_required_bytes = 10;
        let firm_limit_bytes = 20;

        let bounded = BoundedComponent::new(Some(minimum_required_bytes), firm_limit_bytes);

        // First two verifications don't have enough capacity to meet the minimum requirements, based on the slop
        // factor.
        let (grant, result) = verify_component(1, &bounded);
        assert_eq!(
            result.err(),
            Some(VerifierError::InsufficientMinimumMemory {
                available_bytes: grant.effective_limit_bytes(),
                minimum_required_bytes,
            })
        );

        let (grant, result) = verify_component(10, &bounded);
        assert_eq!(
            result.err(),
            Some(VerifierError::InsufficientMinimumMemory {
                available_bytes: grant.effective_limit_bytes(),
                minimum_required_bytes,
            })
        );

        // Now we have enough capacity for the minimum requirements, but the firm limit exceeds that.
        let (grant, result) = verify_component(15, &bounded);
        assert_eq!(
            result.err(),
            Some(VerifierError::FirmLimitExceedsAvailable {
                available_bytes: grant.effective_limit_bytes(),
                firm_limit_bytes,
            })
        );

        let (grant, result) = verify_component(20, &bounded);
        assert_eq!(
            result.err(),
            Some(VerifierError::FirmLimitExceedsAvailable {
                available_bytes: grant.effective_limit_bytes(),
                firm_limit_bytes,
            })
        );

        // We've finally provided enough capacity (30 bytes * 0.25 slop factor -> 22 bytes capacity) to meet the firm
        // limit, so this should pass.
        let (_, result) = verify_component(30, &bounded);
        assert!(result.is_ok());
    }
}
