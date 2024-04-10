use std::collections::{hash_map::Iter, HashMap};

use crate::{MemoryBounds, MemoryGrant};

#[derive(Debug, Eq, PartialEq)]
pub enum VerifierError {
    InvalidComponentBounds {
        component_name: String,
        reason: String,
    },
    InsufficientMinimumMemory {
        available_bytes: usize,
        minimum_required_bytes: usize,
    },
    SoftLimitExceedsAvailable {
        available_bytes: usize,
        soft_limit_bytes: usize,
    },
}

/// Verified bounds.
///
/// This structure contains the original set of parameters -- the grant, verify mode, and verified components -- used
/// when verifying bounds in `BoundsVerifier::verify`. It can then be used to feed into additional components, such as
/// `MemoryPartitioner`, to ensure that the same parameters are used, avoiding any potential misconfiguration.
pub struct VerifiedBounds<'a> {
    grant: MemoryGrant,
    components: HashMap<String, &'a dyn MemoryBounds>,
}

impl<'a> VerifiedBounds<'a> {
    /// Total number of bytes available for allocation.
    pub fn available_bytes(&self) -> usize {
        self.grant.effective_limit_bytes()
    }

    /// Returns the number of components that were verified.
    pub fn components_len(&self) -> usize {
        self.components.len()
    }

    /// Returns an iterator over the components and their memory bounds.
    pub fn components(&self) -> Iter<'_, String, &'a (dyn MemoryBounds + 'a)> {
        self.components.iter()
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
    /// - when a component has invalid bounds (e.g. minimum required bytes higher than soft limit)
    /// - when the combined total of the soft limit for all components exceeds the effective limit
    pub fn verify(self) -> Result<VerifiedBounds<'a>, VerifierError> {
        let available_bytes = self.grant.effective_limit_bytes();

        let mut total_minimum_required_bytes = 0;
        let mut total_soft_limit_bytes = 0;

        for (name, component) in &self.components {
            let minimum_required = component.minimum_required().unwrap_or(0);
            let soft_limit = component.soft_limit();

            if minimum_required > soft_limit {
                return Err(VerifierError::InvalidComponentBounds {
                    component_name: name.clone(),
                    reason: "minimum required bytes exceeds soft limit".to_string(),
                });
            }

            total_minimum_required_bytes += minimum_required;
            total_soft_limit_bytes += soft_limit;
        }

        // Check to ensure that the effective limit is sufficient to meet the minimum required bytes, and then do the
        // same for the soft limit.
        if available_bytes < total_minimum_required_bytes {
            return Err(VerifierError::InsufficientMinimumMemory {
                available_bytes,
                minimum_required_bytes: total_minimum_required_bytes,
            });
        }

        if available_bytes < total_soft_limit_bytes {
            return Err(VerifierError::SoftLimitExceedsAvailable {
                available_bytes,
                soft_limit_bytes: total_soft_limit_bytes,
            });
        }

        Ok(VerifiedBounds {
            grant: self.grant,
            components: self.components,
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
    ) -> (MemoryGrant, Result<VerifiedBounds<'_>, VerifierError>) {
        let initial_grant = get_grant(initial_limit_bytes);

        let mut verifier = BoundsVerifier::from_grant(initial_grant.clone());
        verifier.add_component("component".to_string(), component);

        (initial_grant, verifier.verify())
    }

    #[test]
    fn test_invalid_component_bounds() {
        let bounded = BoundedComponent::new(Some(20), 10);
        let initial_grant = MemoryGrant::effective(1).expect("should never be invalid");

        let mut verifier = BoundsVerifier::from_grant(initial_grant.clone());
        verifier.add_component("component".to_string(), &bounded);

        assert_eq!(
            verifier.verify().err(),
            Some(VerifierError::InvalidComponentBounds {
                component_name: "component".to_string(),
                reason: "minimum required bytes exceeds soft limit".to_string(),
            })
        );
    }

    #[test]
    fn test_verify() {
        let minimum_required_bytes = 10;
        let soft_limit_bytes = 20;

        let bounded = BoundedComponent::new(Some(minimum_required_bytes), soft_limit_bytes);

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

        // Now we have enough capacity for the minimum requirements, but the soft limit exceeds that.
        let (grant, result) = verify_component(15, &bounded);
        assert_eq!(
            result.err(),
            Some(VerifierError::SoftLimitExceedsAvailable {
                available_bytes: grant.effective_limit_bytes(),
                soft_limit_bytes,
            })
        );

        let (grant, result) = verify_component(20, &bounded);
        assert_eq!(
            result.err(),
            Some(VerifierError::SoftLimitExceedsAvailable {
                available_bytes: grant.effective_limit_bytes(),
                soft_limit_bytes,
            })
        );

        // We've finally provided enough capacity (30 bytes * 0.25 slop factor -> 22 bytes capacity) to meet the soft
        // limit, so this should pass.
        let (_, result) = verify_component(30, &bounded);
        assert!(result.is_ok());
    }
}
