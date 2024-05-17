use snafu::Snafu;
use ubyte::ToByteUnit as _;

use crate::{ComponentBounds, MemoryGrant};

#[derive(Debug, Eq, PartialEq, Snafu)]
pub enum VerifierError {
    #[snafu(display("invalid component bounds for {}: {}", component_name, reason))]
    InvalidComponentBounds { component_name: String, reason: String },

    #[snafu(display(
        "minimum require memory ({}) exceeds available memory ({})",
        minimum_required_bytes.bytes(),
        available_bytes.bytes()
    ))]
    InsufficientMinimumMemory {
        available_bytes: usize,
        minimum_required_bytes: usize,
    },

    #[snafu(display("firm limit ({}) exceeds available memory ({})", firm_limit_bytes.bytes(), available_bytes.bytes()))]
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
    component_bounds: ComponentBounds,
}

impl VerifiedBounds {
    /// Total number of bytes available for allocation.
    pub fn total_available_bytes(&self) -> usize {
        self.grant.effective_limit_bytes()
    }

    /// Returns the total number of minimum required bytes for all components that were verified.
    pub fn total_minimum_required_bytes(&self) -> usize {
        self.component_bounds.total_minimum_required_bytes()
    }

    /// Returns the total firm limit, in bytes, for all components that were verified.
    pub fn total_firm_limit_bytes(&self) -> usize {
        self.component_bounds.total_firm_limit_bytes()
    }

    /// Gets a reference to the original component bounds that were verified.
    pub fn bounds(&self) -> &ComponentBounds {
        &self.component_bounds
    }
}

/// Memory bounds verifier.
pub struct BoundsVerifier {
    grant: MemoryGrant,
    component_bounds: ComponentBounds,
}

impl BoundsVerifier {
    /// Creates a new memory bounds verifier with the given memory grant and components bounds.
    pub fn new(grant: MemoryGrant, component_bounds: ComponentBounds) -> Self {
        Self {
            grant,
            component_bounds,
        }
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
        // Iterate over each component in the calculated bounds and do some basic validation to ensure the calculations
        // are correct and logically consistent.
        //
        // We only do this for leaf components because the minimum required/firm limits bytes on components with
        // subcomponents is already calculated on demand, so we know that a parent component is also valid if all of its
        // subcomponents are valid.
        for (name, bounds) in self.component_bounds.leaf_components() {
            if bounds.self_minimum_required_bytes > bounds.self_firm_limit_bytes {
                return Err(VerifierError::InvalidComponentBounds {
                    component_name: name.clone(),
                    reason: "minimum required bytes exceeds firm limit".to_string(),
                });
            }
        }

        // Evaluate the total minimum required and firm limit bytes to make sure our memory grant is sufficient.
        let available_bytes = self.grant.effective_limit_bytes();
        let total_minimum_required_bytes = self.component_bounds.total_minimum_required_bytes();
        let total_firm_limit_bytes = self.component_bounds.total_firm_limit_bytes();

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
            component_bounds: self.component_bounds,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{BoundsVerifier, VerifiedBounds, VerifierError};
    use crate::{
        test_util::{get_component_bounds, BoundedComponent},
        MemoryGrant,
    };

    fn get_grant(initial_limit_bytes: usize) -> MemoryGrant {
        const SLOP_FACTOR: f64 = 0.25;

        MemoryGrant::with_slop_factor(initial_limit_bytes, SLOP_FACTOR).expect("should never be invalid")
    }

    fn verify_component(
        initial_limit_bytes: usize, component: &BoundedComponent,
    ) -> (MemoryGrant, Result<VerifiedBounds, VerifierError>) {
        let initial_grant = get_grant(initial_limit_bytes);
        let bounds = get_component_bounds(component);

        let verifier = BoundsVerifier::new(initial_grant, bounds);
        (initial_grant, verifier.verify())
    }

    #[test]
    fn test_invalid_component_bounds() {
        let bounded = BoundedComponent::new(Some(20), 10);
        let bounds = get_component_bounds(&bounded);
        let initial_grant = MemoryGrant::effective(1).expect("should never be invalid");

        let verifier = BoundsVerifier::new(initial_grant, bounds);

        assert_eq!(
            verifier.verify().err(),
            Some(VerifierError::InvalidComponentBounds {
                component_name: "root.component".to_string(),
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
