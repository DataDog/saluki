//! Memory management.

use std::collections::VecDeque;

use memory_accounting::{BoundsVerifier, MemoryBoundsBuilder, MemoryGrant, VerifiedBounds};
use saluki_config::GenericConfiguration;
use saluki_error::{ErrorContext as _, GenericError};
use serde::Deserialize;
use tracing::info;
use ubyte::{ByteUnit, ToByteUnit as _};

const fn default_memory_slop_factor() -> f64 {
    0.25
}

/// Configuration for memory bounds verification.
#[derive(Deserialize)]
pub struct MemoryBoundsVerificationConfiguration {
    /// The memory limit to adhere to.
    ///
    /// This should be the overall memory limit for the entire process. The value can either be an integer for
    /// specifying the limit in bytes, or a string that uses SI byte prefixes (case-insensitive) such as `1mb` or `1GB`.
    ///
    /// If not specified, no memory bounds verification will be performed.
    #[serde(default)]
    memory_limit: Option<ByteUnit>,

    /// The slop factor to apply to the given memory limit.
    ///
    /// Memory bounds are inherently fuzzy, as components are required to manually define their bounds, and as such, can
    /// only account for memory usage that they know about. The slop factor is applied as a reduction to the overall
    /// memory limit, such that we account for the "known unknowns" -- memory that hasn't yet been accounted for -- by
    /// simply ensuring that we can fit within a portion of the overall limit.
    ///
    /// Values between 0 to 1 are allowed, and represent the percentage of `memory_limit` that is held back. This means
    /// that a slop factor of 0.25, for example, will cause 25% of `memory_limit` to be withheld. If `memory_limit` was
    /// 100MB, we would then verify that the memory bounds can fit within 75MB (100MB * (1 - 0.25) => 75MB).
    #[serde(default = "default_memory_slop_factor")]
    memory_slop_factor: f64,
}

impl MemoryBoundsVerificationConfiguration {
    /// Attempts to read verification configuration from the provided configuration.
    ///
    /// ## Errors
    ///
    /// If an error occurs during deserialization, an error will be returned.
    pub fn try_from_config(config: &GenericConfiguration) -> Result<Self, GenericError> {
        config
            .as_typed::<Self>()
            .error_context("Failed to parse memory bounds verification configuration.")
    }
}

/// Attempts to verify the populated memory bounds against the memory limit configuration.
///
/// If no memory limit is configured, or if the configured memory bounds can fit within the memory limit, `Ok(())` is
/// returned.
///
/// ## Errors
///
/// If the bounds could not be validated, an error will be returned.
pub fn try_verify_memory_bounds<F>(
    configuration: MemoryBoundsVerificationConfiguration, populate_bounds: F,
) -> Result<(), GenericError>
where
    F: FnOnce(&mut MemoryBoundsBuilder),
{
    let initial_grant = match configuration.memory_limit {
        Some(limit) => MemoryGrant::with_slop_factor(limit.as_u64() as usize, configuration.memory_slop_factor)?,
        None => {
            info!("No memory limit set for the process. Skipping memory bounds verification.");
            return Ok(());
        }
    };

    // Run the provided closure to populate the memory bounds that we want to verify.
    let mut bounds_builder = MemoryBoundsBuilder::new();
    populate_bounds(&mut bounds_builder);
    let component_bounds = bounds_builder.finalize();

    // Now verify the bounds.
    let bounds_verifier = BoundsVerifier::new(initial_grant, component_bounds);
    let verified_bounds = bounds_verifier.verify()?;

    info!(
		"Verified memory bounds. Minimum memory requirement of {}, with a calculated firm memory bound of {} out of {} available, from an initial {} grant.",
		verified_bounds.total_minimum_required_bytes().bytes(),
		verified_bounds.total_firm_limit_bytes().bytes(),
		verified_bounds.total_available_bytes().bytes(),
		initial_grant.initial_limit_bytes().bytes(),
	);

    print_verified_bounds(verified_bounds);

    Ok(())
}

fn print_verified_bounds(bounds: VerifiedBounds) {
    info!("Breakdown of verified bounds:");
    info!(
        "- (root): {} minimum, {} firm",
        bounds.bounds().total_minimum_required_bytes().bytes(),
        bounds.bounds().total_firm_limit_bytes().bytes(),
    );

    let mut to_visit = VecDeque::new();
    to_visit.extend(
        bounds
            .bounds()
            .subcomponents()
            .into_iter()
            .map(|(name, bounds)| (1, name, bounds)),
    );

    while let Some((depth, component_name, component_bounds)) = to_visit.pop_front() {
        info!(
            "{:indent$}- {}: {} minimum, {} firm",
            "",
            component_name,
            component_bounds.total_minimum_required_bytes().bytes(),
            component_bounds.total_firm_limit_bytes().bytes(),
            indent = depth * 2
        );

        let mut subcomponents = component_bounds.subcomponents().into_iter().collect::<Vec<_>>();
        while let Some((subcomponent_name, subcomponent_bounds)) = subcomponents.pop() {
            to_visit.push_front((depth + 1, subcomponent_name, subcomponent_bounds));
        }
    }

    info!("");
}
