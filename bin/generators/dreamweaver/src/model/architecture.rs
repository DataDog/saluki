//! Resolved architecture with templates applied.

use std::collections::HashMap;

use rand::Rng;
use saluki_error::GenericError;

use super::ResolvedService;
use crate::config::Config;

/// A resolved architecture with all templates applied to services.
#[derive(Clone, Debug)]
pub struct ResolvedArchitecture {
    /// The resolved services.
    pub services: Vec<ResolvedService>,

    /// Indices of entry point services (services with no incoming edges).
    entry_points: Vec<usize>,
}

impl ResolvedArchitecture {
    /// Creates a resolved architecture from the configuration.
    pub fn from_config(config: &Config) -> Result<Self, GenericError> {
        // Build a map from service name to index
        let name_to_index: HashMap<&str, usize> = config
            .services
            .iter()
            .enumerate()
            .map(|(i, s)| (s.name.as_str(), i))
            .collect();

        // Resolve each service
        let services: Vec<ResolvedService> = config
            .services
            .iter()
            .map(|service_def| {
                let template = config
                    .templates
                    .get(&service_def.template_type)
                    .cloned()
                    .ok_or_else(|| {
                        saluki_error::generic_error!(
                            "Template '{}' not found for service '{}'",
                            service_def.template_type,
                            service_def.name
                        )
                    })?;

                let downstream_indices = service_def
                    .downstream
                    .iter()
                    .map(|name| {
                        name_to_index.get(name.as_str()).copied().ok_or_else(|| {
                            saluki_error::generic_error!(
                                "Downstream service '{}' not found for service '{}'",
                                name,
                                service_def.name
                            )
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;

                Ok(ResolvedService {
                    name: service_def.name.clone(),
                    template,
                    downstream_indices,
                })
            })
            .collect::<Result<Vec<_>, GenericError>>()?;

        // Find entry points (services that are not downstream of any other service)
        let mut has_incoming = vec![false; services.len()];
        for service in &services {
            for &downstream_idx in &service.downstream_indices {
                has_incoming[downstream_idx] = true;
            }
        }

        let entry_points: Vec<usize> = has_incoming
            .iter()
            .enumerate()
            .filter_map(|(i, &has)| if !has { Some(i) } else { None })
            .collect();

        if entry_points.is_empty() {
            return Err(saluki_error::generic_error!(
                "No entry points found in the architecture (all services have incoming edges)"
            ));
        }

        Ok(Self { services, entry_points })
    }

    /// Returns a random entry point service index.
    pub fn random_entry_point(&self, rng: &mut impl Rng) -> usize {
        let idx = rng.random_range(0..self.entry_points.len());
        self.entry_points[idx]
    }

    /// Returns the service at the given index.
    pub fn get_service(&self, index: usize) -> &ResolvedService {
        &self.services[index]
    }

    /// Returns the number of services in the architecture.
    pub fn service_count(&self) -> usize {
        self.services.len()
    }

    /// Returns the entry point service names (for logging).
    pub fn entry_point_names(&self) -> Vec<&str> {
        self.entry_points
            .iter()
            .map(|&i| self.services[i].name.as_str())
            .collect()
    }
}
