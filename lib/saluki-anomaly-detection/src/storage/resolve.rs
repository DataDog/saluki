//! Resolution of the store's effective configuration.
//!
//! The store's detector-derived bounds (the per-series point cap and the point-retention window) must not
//! be starved by operator configuration: an explicitly configured retention that is shorter than what the
//! enabled detectors require is ignored in favour of the detector-derived value, and negative values fall
//! back to the derived/default value. This module ports that precedence logic from the Go observer's
//! `storageConfigFromAgentConfig`, expressing the rejected inputs as structured warnings rather than
//! logging them directly (the crate has no logging dependency).
//!
//! Configuration here is modelled as a set of optional overrides. `None` means "not configured", matching
//! the Go `IsConfigured` check; this matters because an explicit `0` means "disable this bound", not
//! "unset".

use std::fmt::{self, Display, Formatter};

use crate::config::StorageConfig;

/// A configuration value that was rejected, with the value that was used instead.
///
/// The caller decides how to surface these (logging, diagnostics, or a testbench report); the resolver
/// never drops a rejected value silently.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageConfigWarning {
    /// `point_retention_secs` was configured below zero. The detector-derived value won.
    NegativePointRetention {
        /// The configured (negative) retention, in seconds.
        configured_secs: i64,
        /// The detector-derived retention that was used instead, in seconds.
        used_secs: i64,
    },
    /// `point_retention_secs` was configured below the detector-derived requirement. The requirement won.
    PointRetentionBelowRequirement {
        /// The configured retention, in seconds.
        configured_secs: i64,
        /// The minimum retention the enabled detectors require, in seconds.
        required_secs: i64,
    },
    /// `inactive_series_ttl_secs` was configured below zero. The default won.
    NegativeInactiveSeriesTtl {
        /// The configured (negative) TTL, in seconds.
        configured_secs: i64,
        /// The default TTL that was used instead, in seconds.
        used_secs: i64,
    },
    /// `inactive_series_check_interval_secs` was configured below zero. The default won.
    NegativeInactiveSeriesCheckInterval {
        /// The configured (negative) interval, in seconds.
        configured_secs: i64,
        /// The default interval that was used instead, in seconds.
        used_secs: i64,
    },
}

impl Display for StorageConfigWarning {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match *self {
            Self::NegativePointRetention {
                configured_secs,
                used_secs,
            } => write!(
                f,
                "point retention must be >= 0, got {configured_secs}s; using {used_secs}s derived from enabled detector windows"
            ),
            Self::PointRetentionBelowRequirement {
                configured_secs,
                required_secs,
            } => write!(
                f,
                "point retention {configured_secs}s is below the {required_secs}s required by enabled detector windows; using {required_secs}s"
            ),
            Self::NegativeInactiveSeriesTtl {
                configured_secs,
                used_secs,
            } => write!(
                f,
                "inactive series TTL must be >= 0, got {configured_secs}s; using default {used_secs}s"
            ),
            Self::NegativeInactiveSeriesCheckInterval {
                configured_secs,
                used_secs,
            } => write!(
                f,
                "inactive series check interval must be >= 0, got {configured_secs}s; using default {used_secs}s"
            ),
        }
    }
}

/// Operator-supplied storage overrides, each absent when the key was not configured.
///
/// A present `0` is meaningful and is never treated as "unset": `point_retention_secs = Some(0)` keeps the
/// detector-derived retention, while `inactive_series_ttl_secs = Some(0)` disables inactivity eviction.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StorageConfigOverrides {
    /// Overrides [`StorageConfig::max_series`]. `Some(0)` disables capacity eviction.
    pub max_series: Option<usize>,
    /// Overrides [`StorageConfig::eviction_floor_ratio`].
    pub eviction_floor_ratio: Option<f64>,
    /// Overrides [`StorageConfig::point_retention_secs`].
    pub point_retention_secs: Option<i64>,
    /// Overrides [`StorageConfig::inactive_series_ttl_secs`].
    pub inactive_series_ttl_secs: Option<i64>,
    /// Overrides [`StorageConfig::inactive_series_check_interval_secs`].
    pub inactive_series_check_interval_secs: Option<i64>,
}

/// The resolved storage configuration plus any overrides that were rejected.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedStorageConfig {
    /// The effective configuration the store should use.
    pub config: StorageConfig,
    /// Rejected overrides, in resolution order. Empty when every override was accepted.
    pub warnings: Vec<StorageConfigWarning>,
}

/// Resolves the effective storage configuration for a set of enabled detectors.
///
/// `detector_max_points` is the widest per-series point window across the enabled detectors; it determines
/// the point cap and the detector-derived retention (`max_points * 15s + 16s`). The overrides are then
/// applied, except that a `point_retention_secs` override shorter than the detector-derived requirement is
/// ignored with a [`StorageConfigWarning`], as is any negative retention or inactivity value.
pub fn resolve_storage_config(detector_max_points: usize, overrides: &StorageConfigOverrides) -> ResolvedStorageConfig {
    let mut config = StorageConfig::from_detector_windows(detector_max_points);
    let required_retention = config.point_retention_secs;
    let mut warnings = Vec::new();

    if let Some(max_series) = overrides.max_series {
        config.max_series = max_series;
    }
    if let Some(ratio) = overrides.eviction_floor_ratio {
        config.eviction_floor_ratio = ratio;
    }

    if let Some(configured_retention) = overrides.point_retention_secs {
        match configured_retention {
            // An explicit zero keeps the detector-derived retention.
            0 => {}
            retention if retention < 0 => warnings.push(StorageConfigWarning::NegativePointRetention {
                configured_secs: retention,
                used_secs: required_retention,
            }),
            retention if retention < required_retention => {
                warnings.push(StorageConfigWarning::PointRetentionBelowRequirement {
                    configured_secs: retention,
                    required_secs: required_retention,
                })
            }
            retention => config.point_retention_secs = retention,
        }
    }

    if let Some(configured_ttl) = overrides.inactive_series_ttl_secs {
        if configured_ttl < 0 {
            warnings.push(StorageConfigWarning::NegativeInactiveSeriesTtl {
                configured_secs: configured_ttl,
                used_secs: config.inactive_series_ttl_secs,
            });
        } else {
            config.inactive_series_ttl_secs = configured_ttl;
        }
    }

    if let Some(configured_interval) = overrides.inactive_series_check_interval_secs {
        if configured_interval < 0 {
            warnings.push(StorageConfigWarning::NegativeInactiveSeriesCheckInterval {
                configured_secs: configured_interval,
                used_secs: config.inactive_series_check_interval_secs,
            });
        } else {
            config.inactive_series_check_interval_secs = configured_interval;
        }
    }

    ResolvedStorageConfig { config, warnings }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_bounds_from_detector_windows() {
        let resolved = resolve_storage_config(120, &StorageConfigOverrides::default());

        assert_eq!(resolved.config.max_points_per_series, 120);
        // 120 * 15s + 16s == 1816s.
        assert_eq!(resolved.config.point_retention_secs, 1816);
        assert_eq!(resolved.config.max_series, 50_000);
        assert!(resolved.warnings.is_empty());
    }

    #[test]
    fn explicit_zero_retention_keeps_detector_derived() {
        let overrides = StorageConfigOverrides {
            point_retention_secs: Some(0),
            ..StorageConfigOverrides::default()
        };
        let resolved = resolve_storage_config(120, &overrides);

        assert_eq!(resolved.config.point_retention_secs, 1816);
        assert!(resolved.warnings.is_empty());
    }

    #[test]
    fn negative_retention_is_rejected_with_warning() {
        let overrides = StorageConfigOverrides {
            point_retention_secs: Some(-5),
            ..StorageConfigOverrides::default()
        };
        let resolved = resolve_storage_config(120, &overrides);

        assert_eq!(resolved.config.point_retention_secs, 1816);
        assert_eq!(
            resolved.warnings,
            vec![StorageConfigWarning::NegativePointRetention {
                configured_secs: -5,
                used_secs: 1816,
            }]
        );
    }

    #[test]
    fn retention_below_requirement_is_ignored_with_warning() {
        let overrides = StorageConfigOverrides {
            point_retention_secs: Some(60),
            ..StorageConfigOverrides::default()
        };
        let resolved = resolve_storage_config(120, &overrides);

        assert_eq!(resolved.config.point_retention_secs, 1816);
        assert_eq!(
            resolved.warnings,
            vec![StorageConfigWarning::PointRetentionBelowRequirement {
                configured_secs: 60,
                required_secs: 1816,
            }]
        );
    }

    #[test]
    fn retention_above_requirement_is_used() {
        let overrides = StorageConfigOverrides {
            point_retention_secs: Some(3600),
            ..StorageConfigOverrides::default()
        };
        let resolved = resolve_storage_config(120, &overrides);

        assert_eq!(resolved.config.point_retention_secs, 3600);
        assert!(resolved.warnings.is_empty());
    }

    #[test]
    fn max_series_and_ratio_overrides_are_applied() {
        let overrides = StorageConfigOverrides {
            max_series: Some(1000),
            eviction_floor_ratio: Some(0.25),
            ..StorageConfigOverrides::default()
        };
        let resolved = resolve_storage_config(1, &overrides);

        assert_eq!(resolved.config.max_series, 1000);
        assert_eq!(resolved.config.eviction_floor_ratio, 0.25);
    }

    #[test]
    fn explicit_zero_ttl_disables_inactivity_eviction() {
        let overrides = StorageConfigOverrides {
            inactive_series_ttl_secs: Some(0),
            inactive_series_check_interval_secs: Some(0),
            ..StorageConfigOverrides::default()
        };
        let resolved = resolve_storage_config(120, &overrides);

        assert_eq!(resolved.config.inactive_series_ttl_secs, 0);
        assert_eq!(resolved.config.inactive_series_check_interval_secs, 0);
        assert!(resolved.warnings.is_empty());
    }

    #[test]
    fn negative_inactivity_values_fall_back_to_default_with_warnings() {
        let overrides = StorageConfigOverrides {
            inactive_series_ttl_secs: Some(-1),
            inactive_series_check_interval_secs: Some(-1),
            ..StorageConfigOverrides::default()
        };
        let resolved = resolve_storage_config(120, &overrides);

        assert_eq!(resolved.config.inactive_series_ttl_secs, 300);
        assert_eq!(resolved.config.inactive_series_check_interval_secs, 300);
        assert_eq!(
            resolved.warnings,
            vec![
                StorageConfigWarning::NegativeInactiveSeriesTtl {
                    configured_secs: -1,
                    used_secs: 300,
                },
                StorageConfigWarning::NegativeInactiveSeriesCheckInterval {
                    configured_secs: -1,
                    used_secs: 300,
                },
            ]
        );
    }

    #[test]
    fn warnings_render_human_readable_messages() {
        let warning = StorageConfigWarning::PointRetentionBelowRequirement {
            configured_secs: 60,
            required_secs: 1816,
        };
        assert_eq!(
            warning.to_string(),
            "point retention 60s is below the 1816s required by enabled detector windows; using 1816s"
        );
    }
}
