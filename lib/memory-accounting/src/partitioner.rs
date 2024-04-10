use std::{cmp, collections::HashMap};

use crate::{MemoryGrant, VerifiedBounds};

/// Partitioning mode.
pub enum PartitionMode {
    /// Partitions memory based on the weight (soft limit) of each component.
    ///
    /// From the overall grant, a scale factor is determined based on the sum of all component soft limits. That scale
    /// factor is then applied to determine each component's individual grant.
    ///
    /// For example, if there two components with a soft limit of 100 and 500 bytes, respectively, their total is 600
    /// bytes. If the overall grant is 2400 bytes, then the scale factor is 2400 / 600, or 4. The first component would
    /// then be granted 400 (100 * 4) bytes, and the second component would be granted 2000 (500 * 4) bytes.
    Scaled,

    /// Partitions memory uniformly across all components.
    ///
    /// From the overall grant, each component is granted an equal share of the total. If any individual component would
    /// be granted less than its minimum required bytes, then that component will be granted only its minimum required
    /// bytes, and all remaining memory will be partitioned uniformly.
    ///
    /// This logic is applied iteratively, based on a sorted iteration of components, where higher soft limits are
    /// ordered first.
    AllEqual,
}

/// Partitioner error.
#[derive(Debug)]
pub enum PartitionerError {
    /// A invalid grant was requested.
    InvalidGrant { grant_amount: usize },

    /// Insufficient capacity was available when partitioning a grant for a specific component.
    InsufficientCapacity { reason: String },
}

/// Memory partitioner.
pub struct MemoryPartitioner<'a> {
    verified_bounds: VerifiedBounds<'a>,
}

impl<'a> MemoryPartitioner<'a> {
    /// Create a new memory partitioner based on verified memory bounds.
    pub fn from_verified_bounds(verified_bounds: VerifiedBounds<'a>) -> Self {
        Self { verified_bounds }
    }

    /// Calculate partitions based on the given partitioning mode.
    pub fn calculate_partitions(&self, mode: PartitionMode) -> Result<HashMap<String, MemoryGrant>, PartitionerError> {
        match mode {
            PartitionMode::Scaled => self.calculate_scaled_partitions(),
            PartitionMode::AllEqual => self.calculate_all_equal_partitions(),
        }
    }

    fn calculate_scaled_partitions(&self) -> Result<HashMap<String, MemoryGrant>, PartitionerError> {
        let total_soft_limit = self
            .verified_bounds
            .components()
            .map(|(_, c)| c.soft_limit())
            .sum::<usize>();
        let mut available_bytes = self.verified_bounds.available_bytes();
        let total_components = self.verified_bounds.components_len();

        let scale_factor = available_bytes as f64 / total_soft_limit as f64;

        let mut partitioned = HashMap::new();
        for (i, (component_name, component)) in self.verified_bounds.components().enumerate() {
            let soft_limit = component.soft_limit();
            let granted_bytes = if i == total_components - 1 {
                available_bytes
            } else {
                (soft_limit as f64 * scale_factor) as usize
            };

            available_bytes -= granted_bytes;

            let grant = MemoryGrant::effective(granted_bytes).ok_or(PartitionerError::InvalidGrant {
                grant_amount: granted_bytes,
            })?;

            partitioned.insert(component_name.to_string(), grant);
        }

        Ok(partitioned)
    }

    fn calculate_all_equal_partitions(&self) -> Result<HashMap<String, MemoryGrant>, PartitionerError> {
        let total_components = self.verified_bounds.components_len();

        // Get a list of components sorted by soft limit, with the highest first.
        let mut sorted_components = self.verified_bounds.components().collect::<Vec<_>>();
        sorted_components.sort_by(|(_, a), (_, b)| b.soft_limit().cmp(&a.soft_limit()));

        let mut available_bytes = self.verified_bounds.available_bytes();
        let mut equal_split = available_bytes / total_components;

        let mut partitioned = HashMap::new();

        for (i, (component_name, component)) in sorted_components.iter().enumerate() {
            // Figure out if the equal split amount is enough to satisfy the soft limit or not. If not, we have to
            // adjust our grant upwards, while removing those number of bytes from `available_bytes`.
            let soft_limit_bytes = component.soft_limit();
            let (granted_bytes, exceeded_equal_split) = if soft_limit_bytes > equal_split {
                (soft_limit_bytes, true)
            } else {
                (equal_split, false)
            };

            if available_bytes < granted_bytes {
                return Err(PartitionerError::InsufficientCapacity {
                    reason: format!(
                        "insufficient available bytes for component '{}': available={} granted={} exceeded_equal_split={:?}",
                        component_name, available_bytes, granted_bytes, exceeded_equal_split
                    ),
                });
            }

            // Just make sure that if we're the last component being partitioned, we get all remaining available bytes
            // in our grant.
            let granted_bytes = if i == total_components - 1 {
                available_bytes
            } else {
                granted_bytes
            };

            // Consume the granted bytes from the available bytes, and add the partitioned grant to our mappings. If we
            // had to exceed the equal split for this component, we'll recalculate the equal split for the remaining
            // components before continuing.
            available_bytes -= granted_bytes;

            let grant = MemoryGrant::effective(granted_bytes).ok_or(PartitionerError::InvalidGrant {
                grant_amount: granted_bytes,
            })?;
            partitioned.insert(component_name.to_string(), grant);

            if exceeded_equal_split {
                let remaining_components = total_components - partitioned.len();
                equal_split = cmp::max(1, available_bytes / remaining_components);
            }
        }

        Ok(partitioned)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use proptest::prelude::*;

    use super::{MemoryPartitioner, PartitionMode};
    use crate::{test_util::BoundedComponent, BoundsVerifier, MemoryGrant, VerifiedBounds};

    fn components(components: &[(&str, usize)]) -> HashMap<String, BoundedComponent> {
        components
            .iter()
            .map(|(name, soft_limit)| (name.to_string(), BoundedComponent::new(None, *soft_limit)))
            .collect()
    }

    fn effective_grant(n: usize) -> MemoryGrant {
        MemoryGrant::effective(n).expect("should never create invalid grants in tests")
    }

    fn create_verified_bounds(
        grant: MemoryGrant, components: &HashMap<String, BoundedComponent>,
    ) -> VerifiedBounds<'_> {
        let mut verifier = BoundsVerifier::from_grant(grant);
        for (component_name, component) in components {
            verifier.add_component(component_name.clone(), component);
        }

        verifier.verify().expect("should not fail to verify bounds")
    }

    fn arb_effective_grant(max_scale: usize) -> impl Strategy<Value = MemoryGrant> {
        // We limit grants to a size of 2^53 (~9PB) because at numbers bigger than that, we end up with floating point
        // loss when we do scaling calculations. Since we are not going to support grants that large -- and if we ever
        // have to, well, then, I'll eat my synpatic implant or whatever makes sense 40 years from now -- we limit them
        // in this way.
        //
        // On top of that, we allow for a maximum scale factor to be passed in, which will be used to scale down our
        // upper bound here. This is to ensure that we don't generate grants that can't be scaled up appropriately
        // without also violating the bit about staying within 2^53.
        let upper_bound = 2usize.pow(f64::MANTISSA_DIGITS) / max_scale;
        (1..=upper_bound).prop_map(|n| MemoryGrant::effective(n).unwrap())
    }

    fn arb_bounded_components() -> impl Strategy<Value = (MemoryGrant, HashMap<String, BoundedComponent>)> {
        const SCALE_FACTOR: usize = 4;

        // We generate an overall grant, and then from that grant, slice it into an arbitrary number (at least one) of
        // components. Each component's soft limit is based on the sliced up portion of the overall grant, such that the
        // combination of all component's soft limits should be equal to the overall grant.
        //
        // We then generate a "scaled grant", which is anywhere from the overall grant to 4X the overall grant. This is
        // to exercise the scaling logic in the partitioner.

        // Start from the overall grant limit, and then slice it up to get our components.
        arb_effective_grant(SCALE_FACTOR).prop_perturb(|overall_grant, mut rng| {
            let overall_grant_bytes = overall_grant.effective_limit_bytes();
            let mut remaining_grant_bytes = overall_grant_bytes;
            let mut components = HashMap::new();

            while remaining_grant_bytes > 0 || components.is_empty() {
                let grant_bytes = rng.gen_range(1..=remaining_grant_bytes);
                remaining_grant_bytes -= grant_bytes;

                components.insert(
                    format!("component-{}", components.len()),
                    BoundedComponent::new(None, grant_bytes),
                );
            }

            let scaled_grant = rng.gen_range(overall_grant_bytes..overall_grant_bytes.saturating_mul(SCALE_FACTOR));
            (MemoryGrant::effective(scaled_grant).unwrap(), components)
        })
    }

    #[test]
    fn partition_all_equal_exact_fit() {
        let components = components(&[("a", 100), ("b", 200), ("c", 300)]);
        let verified_bounds = create_verified_bounds(effective_grant(600), &components);

        let partitioner = MemoryPartitioner::from_verified_bounds(verified_bounds);
        let partitions = partitioner.calculate_partitions(PartitionMode::AllEqual).unwrap();

        assert_eq!(partitions.len(), 3);
        assert_eq!(partitions["c"], effective_grant(300)); // checked first,  max(200, 300) -> 300, so granted 300 (300 remaining, equal split now 150)
        assert_eq!(partitions["b"], effective_grant(200)); // checked second, max(150, 200) -> 200, so granted 200 (100 remaining, equal split now 100)
        assert_eq!(partitions["c"], effective_grant(300)); // checked third,  max(100, 100) -> 100, so granted 100 (0 remaining, all done)
    }

    #[test]
    fn partition_all_equal_scale_to_fit() {
        let components = components(&[("a", 100), ("b", 200), ("c", 300)]);
        let total_grant_bytes = 1800;
        let verified_bounds = create_verified_bounds(effective_grant(total_grant_bytes), &components);
        let equal_split = total_grant_bytes / components.len();

        let partitioner = MemoryPartitioner::from_verified_bounds(verified_bounds);
        let partitions = partitioner.calculate_partitions(PartitionMode::AllEqual).unwrap();

        assert_eq!(partitions.len(), 3);
        assert_eq!(partitions["a"], effective_grant(equal_split)); // max(600, 100) -> 600, granted 600
        assert_eq!(partitions["b"], effective_grant(equal_split)); // max(600, 200) -> 600, granted 600
        assert_eq!(partitions["c"], effective_grant(equal_split)); // max(600, 300) -> 600, granted 600
    }

    #[test]
    fn partition_all_equal_scale_to_fit_with_exceeds() {
        let components = components(&[("a", 100), ("b", 200), ("c", 900)]);
        let verified_bounds = create_verified_bounds(effective_grant(1800), &components);

        let partitioner = MemoryPartitioner::from_verified_bounds(verified_bounds);
        let partitions = partitioner.calculate_partitions(PartitionMode::AllEqual).unwrap();

        assert_eq!(partitions.len(), 3);
        assert_eq!(partitions["c"], effective_grant(900)); // checked first, max(600, 900) -> 900, so granted 900 (900 remaining, equal split now 450)
        assert_eq!(partitions["b"], effective_grant(450)); // checked second, max(450, 200) -> 450, so granted 450 (450 remaining, equal split now 450)
        assert_eq!(partitions["a"], effective_grant(450)); // checked third, max(450, 100) -> 450, so granted 450 (0 remaining, all done)
    }

    #[test]
    fn partition_scaled_exact_fit() {
        let components = components(&[("a", 100), ("b", 200), ("c", 300)]);
        let verified_bounds = create_verified_bounds(effective_grant(600), &components);

        let partitioner = MemoryPartitioner::from_verified_bounds(verified_bounds);
        let partitions = partitioner.calculate_partitions(PartitionMode::Scaled).unwrap();

        assert_eq!(partitions.len(), 3);
        assert_eq!(partitions["a"], effective_grant(100));
        assert_eq!(partitions["b"], effective_grant(200));
        assert_eq!(partitions["c"], effective_grant(300));
    }

    #[test]
    fn partition_scaled_scale_to_fit() {
        let components = components(&[("a", 100), ("b", 200), ("c", 300)]);
        let verified_bounds = create_verified_bounds(effective_grant(1800), &components); // 3X scale

        let partitioner = MemoryPartitioner::from_verified_bounds(verified_bounds);
        let partitions = partitioner.calculate_partitions(PartitionMode::Scaled).unwrap();

        assert_eq!(partitions.len(), 3);
        assert_eq!(partitions["a"], effective_grant(300));
        assert_eq!(partitions["b"], effective_grant(600));
        assert_eq!(partitions["c"], effective_grant(900));
    }

    proptest! {
        #[test]
        fn property_test_partition_all_equal((overall_grant, components) in arb_bounded_components()) {
            // What we're exercising here is that regardless of the ordering of components, their soft limits, and so
            // on, we always grant all bytes from the overall grant to the configured components, such that there are
            // never any leftover bytes from the overall grant.
            let total_grant_bytes = overall_grant.effective_limit_bytes();
            let verified_bounds = create_verified_bounds(overall_grant, &components);
            let partitioner = MemoryPartitioner::from_verified_bounds(verified_bounds);
            let partitions = partitioner.calculate_partitions(PartitionMode::AllEqual).unwrap();

            let total_granted_component_bytes = partitions.values().map(|g| g.effective_limit_bytes()).sum::<usize>();
            assert_eq!(total_granted_component_bytes, total_grant_bytes);
        }

        #[test]
        fn property_test_partition_scaled((overall_grant, components) in arb_bounded_components()) {
            // What we're exercising here is that regardless of how much we end up scaling components by, we always make
            // sure to grant all bytes from the overall grant to the configured components, such that there are never
            // any leftover bytes from the overall grant.
            let total_grant_bytes = overall_grant.effective_limit_bytes();
            let verified_bounds = create_verified_bounds(overall_grant, &components);
            let partitioner = MemoryPartitioner::from_verified_bounds(verified_bounds);
            let partitions = partitioner.calculate_partitions(PartitionMode::Scaled).unwrap();

            let total_granted_component_bytes = partitions.values().map(|g| g.effective_limit_bytes()).sum::<usize>();
            assert_eq!(total_granted_component_bytes, total_grant_bytes);
        }
    }
}
