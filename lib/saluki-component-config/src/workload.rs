//! Component-native configuration for the workload domain.
//!
//! This module is intentionally empty for now. The workload domain in `saluki-components` is
//! expressed entirely through injected runtime state -- `workload_provider` handles
//! (`Arc<dyn WorkloadProvider + Send + Sync>`) and capture-entity resolvers -- which the design
//! explicitly classifies as non-config component-internal state that does not move into the leaf
//! crate. No component currently has a workload-owned `from_configuration` config struct.
//!
//! The module exists so the ownership-domain layout is complete and so workload-owned config has an
//! obvious home if any is introduced later.
