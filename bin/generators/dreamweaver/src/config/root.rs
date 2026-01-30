//! Root configuration structure.

use std::path::Path;

use indexmap::IndexMap;
use saluki_error::{ErrorContext as _, GenericError};
use serde::Deserialize;
use sha3::{Digest, Sha3_256};

use super::{OutputConfig, ServiceDefinition, ServiceTemplate, WorkloadConfig};

/// A seed value that can be deserialized from an arbitrary string.
///
/// The string is hashed using SHA3-256 to produce a 32-byte seed for the RNG.
#[derive(Clone, Debug)]
pub struct Seed(pub [u8; 32]);

impl<'de> Deserialize<'de> for Seed {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        let mut hasher = Sha3_256::new();
        hasher.update(s.as_bytes());
        let result = hasher.finalize();
        Ok(Seed(result.into()))
    }
}

/// Root configuration for dreamweaver.
#[derive(Clone, Debug, Deserialize)]
pub struct Config {
    /// Seed string for deterministic generation (hashed to 32 bytes via SHA3-256).
    pub seed: Seed,

    /// Service type templates.
    pub templates: IndexMap<String, ServiceTemplate>,

    /// Service definitions in the architecture.
    pub services: Vec<ServiceDefinition>,

    /// Workload configuration.
    pub workload: WorkloadConfig,

    /// Output configuration.
    pub output: OutputConfig,
}

impl Config {
    /// Attempts to load a serialized `Config` from the given file path.
    pub fn try_from_file<P>(config_path: P) -> Result<Self, GenericError>
    where
        P: AsRef<Path>,
    {
        let config_path = config_path.as_ref();
        let config_file_raw =
            std::fs::read_to_string(config_path).error_context("Failed to read configuration file.")?;
        let config: Self =
            serde_yaml::from_str(&config_file_raw).error_context("Failed to parse configuration file.")?;

        config.validate()?;
        Ok(config)
    }

    /// Validates the configuration.
    fn validate(&self) -> Result<(), GenericError> {
        // Check that all service template references exist
        for service in &self.services {
            if !self.templates.contains_key(&service.template_type) {
                return Err(saluki_error::generic_error!(
                    "Service '{}' references unknown template type '{}'",
                    service.name,
                    service.template_type
                ));
            }
        }

        // Check that all downstream service references exist
        let service_names: std::collections::HashSet<_> = self.services.iter().map(|s| &s.name).collect();
        for service in &self.services {
            for downstream in &service.downstream {
                if !service_names.contains(downstream) {
                    return Err(saluki_error::generic_error!(
                        "Service '{}' references unknown downstream service '{}'",
                        service.name,
                        downstream
                    ));
                }
            }
        }

        // Check for duplicate service names
        let mut seen = std::collections::HashSet::new();
        for service in &self.services {
            if !seen.insert(&service.name) {
                return Err(saluki_error::generic_error!(
                    "Duplicate service name '{}'",
                    service.name
                ));
            }
        }

        // Check for cycles in the service graph
        self.detect_cycles()?;

        Ok(())
    }

    /// Detects cycles in the service dependency graph.
    fn detect_cycles(&self) -> Result<(), GenericError> {
        use std::collections::{HashMap, HashSet};

        let service_map: HashMap<&str, &ServiceDefinition> =
            self.services.iter().map(|s| (s.name.as_str(), s)).collect();

        let mut visited = HashSet::new();
        let mut rec_stack = HashSet::new();

        fn has_cycle<'a>(
            node: &'a str, service_map: &HashMap<&str, &'a ServiceDefinition>, visited: &mut HashSet<&'a str>,
            rec_stack: &mut HashSet<&'a str>, path: &mut Vec<&'a str>,
        ) -> Option<Vec<&'a str>> {
            visited.insert(node);
            rec_stack.insert(node);
            path.push(node);

            if let Some(service) = service_map.get(node) {
                for downstream in &service.downstream {
                    if !visited.contains(downstream.as_str()) {
                        if let Some(cycle) = has_cycle(downstream.as_str(), service_map, visited, rec_stack, path) {
                            return Some(cycle);
                        }
                    } else if rec_stack.contains(downstream.as_str()) {
                        // Found a cycle
                        let cycle_start = path.iter().position(|&n| n == downstream.as_str()).unwrap();
                        let mut cycle: Vec<_> = path[cycle_start..].to_vec();
                        cycle.push(downstream.as_str());
                        return Some(cycle);
                    }
                }
            }

            path.pop();
            rec_stack.remove(node);
            None
        }

        for service in &self.services {
            if !visited.contains(service.name.as_str()) {
                let mut path = Vec::new();
                if let Some(cycle) = has_cycle(&service.name, &service_map, &mut visited, &mut rec_stack, &mut path) {
                    return Err(saluki_error::generic_error!(
                        "Cycle detected in service graph: {}",
                        cycle.join(" -> ")
                    ));
                }
            }
        }

        Ok(())
    }
}
