//! Component catalog and CLI settings resolution.
//!
//! The testbench mirrors the Go observer's component catalog so its `--config` file, `--only`,
//! `--enable`/`--disable`, and exported `component_configs` share the Go semantics. Three details
//! matter:
//!
//! * `--config` takes **full precedence**: when it is supplied, `--only`/`--enable`/`--disable` are
//!   ignored entirely, exactly as `main.go` does.
//! * An explicit `--config` entry is populated from the **production** catalog defaults and bypasses
//!   the testbench replay tuning for that component. `{"bocpd":{"enabled":true}}` therefore keeps the
//!   production warmup (60 points), not the replay warmup (40), matching the pinned Go source.
//! * Components the Rust port deliberately does not migrate (the log extractors, `time_cluster`, and
//!   the testbench-only `passthrough` adapter) are rejected when enabled, and the baseline controller
//!   is always disabled: a non-zero `--baseline-duration` is an error.
//!
//! The catalog deviates from Go in one documented way: the port's default profile enables all five
//! metric detectors in addition to `anomaly_scorer`, because the tool is used for whole-detector
//! replays. The Go catalog only enables BOCPD by default.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::Path;

use saluki_anomaly_detection::config::AnomalyScorerConfig;
use serde_json::{json, Map, Value};

/// Name of the testbench-only passthrough correlator (not migrated).
pub const PASSTHROUGH: &str = "passthrough";

/// The category a component belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComponentKind {
    /// A metric detector.
    Detector,
    /// A correlator or the anomaly scorer.
    Correlator,
    /// A log-to-metric extractor.
    Extractor,
}

impl ComponentKind {
    /// Returns the Go catalog label (`detector`, `correlator`, or `extractor`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Detector => "detector",
            Self::Correlator => "correlator",
            Self::Extractor => "extractor",
        }
    }
}

/// A catalog entry: a component the testbench knows about.
#[derive(Clone, Copy, Debug)]
pub struct CatalogEntry {
    /// The Go component name used on the CLI and in config files.
    pub name: &'static str,
    /// The human-readable display name.
    pub display_name: &'static str,
    /// The component category.
    pub kind: ComponentKind,
    /// Whether the Rust port implements this component. Enabling a non-migrated component is an error.
    pub migrated: bool,
}

/// Returns the component catalog in the Go catalog order, plus the testbench-only passthrough entry.
pub fn catalog() -> Vec<CatalogEntry> {
    vec![
        CatalogEntry {
            name: "log_metrics_extractor",
            display_name: "Log Metrics Extractor",
            kind: ComponentKind::Extractor,
            migrated: false,
        },
        CatalogEntry {
            name: "connection_error_extractor",
            display_name: "Connection Error Extractor",
            kind: ComponentKind::Extractor,
            migrated: false,
        },
        CatalogEntry {
            name: "log_pattern_extractor",
            display_name: "Log Pattern Extractor",
            kind: ComponentKind::Extractor,
            migrated: false,
        },
        CatalogEntry {
            name: "bocpd",
            display_name: "BOCPD",
            kind: ComponentKind::Detector,
            migrated: true,
        },
        CatalogEntry {
            name: "scanmw",
            display_name: "ScanMW",
            kind: ComponentKind::Detector,
            migrated: true,
        },
        CatalogEntry {
            name: "scanwelch",
            display_name: "ScanWelch",
            kind: ComponentKind::Detector,
            migrated: true,
        },
        CatalogEntry {
            name: "holt_residual",
            display_name: "HoltResidual",
            kind: ComponentKind::Detector,
            migrated: true,
        },
        CatalogEntry {
            name: "tukey_biweight",
            display_name: "TukeyBiweight",
            kind: ComponentKind::Detector,
            migrated: true,
        },
        CatalogEntry {
            name: "time_cluster",
            display_name: "TimeCluster",
            kind: ComponentKind::Correlator,
            migrated: false,
        },
        CatalogEntry {
            name: "anomaly_scorer",
            display_name: "AnomalyScorer",
            kind: ComponentKind::Correlator,
            migrated: true,
        },
        CatalogEntry {
            name: PASSTHROUGH,
            display_name: "Passthrough",
            kind: ComponentKind::Correlator,
            migrated: false,
        },
    ]
}

/// Looks up a catalog entry by name.
pub fn catalog_entry(name: &str) -> Option<CatalogEntry> {
    catalog().into_iter().find(|entry| entry.name == name)
}

/// Returns the production (Go catalog) parameter object for a component, when it has hyperparameters.
///
/// Components without hyperparameters (`connection_error_extractor`) return `None`, matching the Go
/// catalog entries whose `parseJSON` is nil.
pub fn production_params(name: &str) -> Option<Value> {
    let value = match name {
        "bocpd" => json!({
            "warmup_points": 60,
            "hazard": 0.05,
            "cp_threshold": 0.6,
            "short_run_length": 5,
            "cp_mass_threshold": 0.7,
            "max_run_length": 120,
            "prior_variance_scale": 10.0,
            "min_variance": 1.0,
            "recovery_points": 10,
        }),
        "scanmw" | "scanwelch" => json!({
            "min_points": 30,
            "max_points": 120,
        }),
        "holt_residual" => json!({
            "alpha": 0.2,
            "beta": 0.05,
            "warmup_points": 24,
            "residual_window": 60,
            "z_threshold": 4.5,
            "confirm_m": 2,
            "min_deviation_mad": 3.0,
            "refractory": 20,
        }),
        "tukey_biweight" => json!({
            "window_size": 80,
            "min_points": 80,
            "biweight_c": 4.685,
            "irls_iterations": 4,
            "z_threshold": 5.0,
            "score_every": 4,
            "cooldown_points": 30,
        }),
        "log_pattern_extractor" => json!({}),
        "time_cluster" => json!({}),
        "anomaly_scorer" => json!({
            "alpha": 0.014,
            "saturation_k": 5.0,
            "window_secs": 15,
            "low_threshold": 0.15,
            "high_threshold": 0.40,
            "margin_pct": 0.20,
            "correlation_events": false,
            "correlation_event_threshold": "high",
            "cooldown_secs": 300,
            "max_episode_anomalies": 50,
            "max_reported_items": 16,
        }),
        _ => return None,
    };
    Some(value)
}

/// Returns the replay-tuned parameters applied to components not explicitly configured.
///
/// This is the port of `ApplyTestbenchDefaults`: BOCPD uses a 40-point warmup, Holt 15/25, Tukey
/// 40/40, and the scorer enables episodes with cooldown 0.
fn testbench_params(name: &str) -> Option<Value> {
    let mut params = production_params(name)?;
    match name {
        "bocpd" => params["warmup_points"] = json!(40),
        "holt_residual" => {
            params["warmup_points"] = json!(15);
            params["residual_window"] = json!(25);
        }
        "tukey_biweight" => {
            params["window_size"] = json!(40);
            params["min_points"] = json!(40);
        }
        "anomaly_scorer" => {
            params["correlation_events"] = json!(true);
            params["cooldown_secs"] = json!(0);
        }
        _ => {}
    }
    Some(params)
}

/// Returns the default enabled state used by the testbench when no overrides are supplied.
///
/// This is the documented deviation from Go: all five metric detectors plus the scorer are enabled,
/// while the extractors, `time_cluster`, and `passthrough` stay disabled.
fn default_enabled() -> BTreeMap<String, bool> {
    let mut enabled = BTreeMap::new();
    for entry in catalog() {
        enabled.insert(
            entry.name.to_string(),
            entry.migrated && entry.kind != ComponentKind::Extractor,
        );
    }
    enabled
}

/// One resolved component: its enabled state and fully resolved parameter object.
#[derive(Clone, Debug)]
pub struct ResolvedComponent {
    /// The component name.
    pub name: String,
    /// The component category.
    pub kind: ComponentKind,
    /// Whether the component is enabled for this run.
    pub enabled: bool,
    /// The resolved parameters (production or replay defaults overlaid with an explicit config entry),
    /// or `None` for components without hyperparameters.
    pub params: Option<Value>,
}

/// Fully resolved testbench settings for one run.
#[derive(Clone, Debug)]
pub struct ResolvedSettings {
    components: Vec<ResolvedComponent>,
    scorer: AnomalyScorerConfig,
}

impl ResolvedSettings {
    /// Returns every resolved component, in catalog order.
    pub fn components(&self) -> &[ResolvedComponent] {
        &self.components
    }

    /// Returns a resolved component by name.
    pub fn component(&self, name: &str) -> Option<&ResolvedComponent> {
        self.components.iter().find(|component| component.name == name)
    }

    /// Returns whether a component is enabled. Unknown names are disabled.
    pub fn enabled(&self, name: &str) -> bool {
        self.component(name).is_some_and(|component| component.enabled)
    }

    /// Returns the resolved anomaly-scorer configuration.
    pub fn scorer(&self) -> &AnomalyScorerConfig {
        &self.scorer
    }

    /// Returns the sorted names of enabled detectors.
    pub fn detectors_enabled(&self) -> Vec<String> {
        self.enabled_names(ComponentKind::Detector)
    }

    /// Returns the sorted names of enabled correlators.
    pub fn correlators_enabled(&self) -> Vec<String> {
        self.enabled_names(ComponentKind::Correlator)
    }

    fn enabled_names(&self, kind: ComponentKind) -> Vec<String> {
        let mut names: Vec<String> = self
            .components
            .iter()
            .filter(|component| component.enabled && component.kind == kind)
            .map(|component| component.name.clone())
            .collect();
        names.sort();
        names
    }

    /// Builds the `metadata.component_configs` map: `{enabled, ...resolved params}` per component.
    pub fn component_configs(&self) -> BTreeMap<String, Value> {
        let mut configs = BTreeMap::new();
        for component in &self.components {
            let mut object = Map::new();
            object.insert("enabled".to_string(), Value::Bool(component.enabled));
            if let Some(Value::Object(params)) = &component.params {
                for (key, value) in params {
                    object.insert(key.clone(), value.clone());
                }
            }
            configs.insert(component.name.clone(), Value::Object(object));
        }
        configs
    }
}

/// An error resolving CLI settings or a `--config` file.
#[derive(Debug)]
pub enum ConfigError {
    /// The config file could not be read.
    Io(std::io::Error),
    /// The config file was not valid JSON.
    Json(serde_json::Error),
    /// The config file was structurally invalid or referenced an unknown component.
    Invalid(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "{err}"),
            Self::Json(err) => write!(f, "parsing params file: {err}"),
            Self::Invalid(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::Json(err) => Some(err),
            Self::Invalid(_) => None,
        }
    }
}

impl From<std::io::Error> for ConfigError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<serde_json::Error> for ConfigError {
    fn from(err: serde_json::Error) -> Self {
        Self::Json(err)
    }
}

/// Resolves the run's component settings from the CLI inputs.
///
/// `config_file` takes full precedence over `only`/`enable`/`disable` when present. The baseline
/// argument is validated here too: only an absent value, `0`, or `disabled` is accepted, because the
/// baseline controller is not migrated.
pub fn resolve(
    config_file: Option<&Path>, only: Option<&str>, enable: Option<&str>, disable: Option<&str>,
    baseline_duration: Option<&str>,
) -> Result<ResolvedSettings, ConfigError> {
    validate_baseline(baseline_duration)?;

    let mut enabled = default_enabled();
    let mut params: BTreeMap<String, Value> = BTreeMap::new();
    let mut explicit: BTreeSet<String> = BTreeSet::new();
    let mut explicitly_enabled: BTreeSet<String> = BTreeSet::new();

    if let Some(path) = config_file {
        apply_config_file(path, &mut enabled, &mut params, &mut explicit, &mut explicitly_enabled)?;
    } else {
        apply_flag_overrides(only, enable, disable, &mut enabled, &mut explicitly_enabled);
    }

    // Apply the replay tuning to every component that was not explicitly configured, exactly as
    // `ApplyTestbenchDefaults` does.
    for entry in catalog() {
        if explicit.contains(entry.name) {
            continue;
        }
        if let Some(tuned) = testbench_params(entry.name) {
            params.insert(entry.name.to_string(), tuned);
        } else if let Some(production) = production_params(entry.name) {
            params.insert(entry.name.to_string(), production);
        }
    }
    if !explicitly_enabled.contains("anomaly_scorer") {
        enabled.insert("anomaly_scorer".to_string(), true);
    }

    // Reject enabling any component the port does not implement.
    for entry in catalog() {
        if enabled.get(entry.name).copied().unwrap_or(false) && !entry.migrated {
            return Err(ConfigError::Invalid(format!(
                "component {:?} is not migrated to Rust and cannot be enabled",
                entry.name
            )));
        }
    }

    let scorer_value = params.get("anomaly_scorer").cloned().unwrap_or_else(|| json!({}));
    let scorer = build_scorer_config(&scorer_value);

    let components = catalog()
        .into_iter()
        .map(|entry| {
            let is_enabled = enabled.get(entry.name).copied().unwrap_or(false);
            ResolvedComponent {
                name: entry.name.to_string(),
                kind: entry.kind,
                enabled: is_enabled,
                params: params.get(entry.name).cloned(),
            }
        })
        .collect();

    Ok(ResolvedSettings { components, scorer })
}

fn validate_baseline(baseline_duration: Option<&str>) -> Result<(), ConfigError> {
    match baseline_duration {
        None | Some("") | Some("0") | Some("disabled") => Ok(()),
        Some(other) => Err(ConfigError::Invalid(format!(
            "the baseline controller is not migrated to Rust: --baseline-duration must be 0 or \
             \"disabled\" (got {other:?})"
        ))),
    }
}

fn apply_config_file(
    path: &Path, enabled: &mut BTreeMap<String, bool>, params: &mut BTreeMap<String, Value>,
    explicit: &mut BTreeSet<String>, explicitly_enabled: &mut BTreeSet<String>,
) -> Result<(), ConfigError> {
    let text = fs::read_to_string(path)?;
    let root: Value = serde_json::from_str(&text)?;
    let Some(components) = root.get("components") else {
        return Ok(());
    };
    let Some(components) = components.as_object() else {
        return Err(ConfigError::Invalid(
            "params file \"components\" must be a JSON object".to_string(),
        ));
    };

    for (name, raw) in components {
        let entry = catalog_entry(name)
            .ok_or_else(|| ConfigError::Invalid(format!("unknown component {name:?} in params file")))?;
        let Some(object) = raw.as_object() else {
            return Err(ConfigError::Invalid(format!(
                "params entry for component {name:?} must be a JSON object"
            )));
        };

        let requested_enabled = object.get("enabled").and_then(Value::as_bool).unwrap_or(false);
        if requested_enabled && !entry.migrated {
            return Err(ConfigError::Invalid(format!(
                "component {name:?} is not migrated to Rust and cannot be enabled"
            )));
        }
        if let Some(value) = object.get("enabled").and_then(Value::as_bool) {
            enabled.insert(name.clone(), value);
            explicitly_enabled.insert(name.clone());
        }

        explicit.insert(name.clone());
        if let Some(base) = production_params(name) {
            let mut merged = base.as_object().cloned().unwrap_or_default();
            for (key, value) in object {
                if key == "enabled" {
                    continue;
                }
                merged.insert(key.clone(), value.clone());
            }
            params.insert(name.clone(), Value::Object(merged));
        }
    }
    Ok(())
}

fn apply_flag_overrides(
    only: Option<&str>, enable: Option<&str>, disable: Option<&str>, enabled: &mut BTreeMap<String, bool>,
    explicitly_enabled: &mut BTreeSet<String>,
) {
    if let Some(only) = only {
        let selected = split_names(only);
        for entry in catalog() {
            if entry.kind == ComponentKind::Extractor {
                continue;
            }
            enabled.insert(entry.name.to_string(), selected.contains(entry.name));
            explicitly_enabled.insert(entry.name.to_string());
        }
        return;
    }

    if let Some(enable) = enable {
        for name in split_names(enable) {
            if catalog_entry(&name).is_some() {
                enabled.insert(name.clone(), true);
                explicitly_enabled.insert(name);
            }
        }
    }
    if let Some(disable) = disable {
        for name in split_names(disable) {
            if catalog_entry(&name).is_some() {
                enabled.insert(name.clone(), false);
                explicitly_enabled.insert(name);
            }
        }
    }
}

fn split_names(value: &str) -> BTreeSet<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
}

/// Builds the typed scorer configuration from the resolved parameter object.
///
/// Unspecified fields keep their production defaults; the scorer constructor then applies the same
/// clamping rules as the Go constructor.
pub fn build_scorer_config(value: &Value) -> AnomalyScorerConfig {
    let mut config = AnomalyScorerConfig::production();
    let Some(object) = value.as_object() else {
        return config;
    };

    if let Some(number) = object.get("alpha").and_then(Value::as_f64) {
        config.alpha = number;
    }
    if let Some(number) = object.get("saturation_k").and_then(Value::as_f64) {
        config.saturation_k = number;
    }
    if let Some(number) = object.get("window_secs").and_then(Value::as_i64) {
        config.window_secs = number;
    }
    if let Some(number) = object.get("low_threshold").and_then(Value::as_f64) {
        config.low_threshold = number;
    }
    if let Some(number) = object.get("high_threshold").and_then(Value::as_f64) {
        config.high_threshold = number;
    }
    if let Some(number) = object.get("margin_pct").and_then(Value::as_f64) {
        config.margin_pct = number;
    }
    if let Some(flag) = object.get("correlation_events").and_then(Value::as_bool) {
        config.correlation_events = flag;
    }
    if let Some(text) = object.get("correlation_event_threshold").and_then(Value::as_str) {
        config.correlation_event_threshold = text.to_string();
    }
    if let Some(number) = object.get("cooldown_secs").and_then(Value::as_i64) {
        config.cooldown_secs = number;
    }
    if let Some(number) = object.get("max_episode_anomalies").and_then(Value::as_u64) {
        config.max_episode_anomalies = number as usize;
    }
    if let Some(number) = object.get("max_reported_items").and_then(Value::as_u64) {
        config.max_reported_items = number as usize;
    }
    if let Some(number) = object.get("max_buckets").and_then(Value::as_i64) {
        config.max_buckets = number;
    }
    config
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve_no_config() -> ResolvedSettings {
        resolve(None, None, None, None, Some("0")).unwrap()
    }

    #[test]
    fn default_profile_enables_detectors_and_scorer_only() {
        let settings = resolve_no_config();
        assert_eq!(
            settings.detectors_enabled(),
            ["bocpd", "holt_residual", "scanmw", "scanwelch", "tukey_biweight"]
        );
        assert_eq!(settings.correlators_enabled(), ["anomaly_scorer"]);
        assert!(!settings.enabled("log_metrics_extractor"));
        assert!(!settings.enabled("time_cluster"));
        assert!(!settings.enabled("passthrough"));
    }

    #[test]
    fn replay_tuning_is_applied_by_default() {
        let settings = resolve_no_config();
        assert_eq!(
            settings.component("bocpd").unwrap().params.as_ref().unwrap()["warmup_points"],
            json!(40)
        );
        assert_eq!(
            settings.component("holt_residual").unwrap().params.as_ref().unwrap()["warmup_points"],
            json!(15)
        );
        assert_eq!(
            settings.component("tukey_biweight").unwrap().params.as_ref().unwrap()["window_size"],
            json!(40)
        );
        assert!(settings.scorer().correlation_events);
        assert_eq!(settings.scorer().cooldown_secs, 0);
    }

    #[test]
    fn only_wins_over_enable_and_disable() {
        let settings = resolve(None, Some("bocpd"), Some("bocpd"), Some("scanmw"), Some("0")).unwrap();
        assert!(settings.enabled("bocpd"));
        assert!(!settings.enabled("scanmw"));
        assert!(!settings.enabled("scanwelch"));
        assert!(!settings.enabled("anomaly_scorer"));
    }

    #[test]
    fn enable_and_disable_override_defaults() {
        let settings = resolve(None, None, Some("scanmw"), Some("bocpd"), Some("0")).unwrap();
        assert!(settings.enabled("scanmw"));
        assert!(!settings.enabled("bocpd"));
    }

    #[test]
    fn config_file_takes_precedence_over_flags() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("params.json");
        std::fs::write(
            &path,
            r#"{"components":{"bocpd":{"enabled":false},"scanmw":{"enabled":true}}}"#,
        )
        .unwrap();

        // --only would enable bocpd and disable scanmw, and --enable/--disable agree with that; the
        // config file must still win over all three.
        let settings = resolve(Some(&path), Some("bocpd"), Some("bocpd"), Some("scanmw"), Some("0")).unwrap();
        assert!(!settings.enabled("bocpd"));
        assert!(settings.enabled("scanmw"));
    }

    #[test]
    fn explicit_config_entry_uses_production_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("params.json");
        std::fs::write(&path, r#"{"components":{"bocpd":{"enabled":true}}}"#).unwrap();

        let settings = resolve(Some(&path), None, None, None, Some("0")).unwrap();
        let params = settings.component("bocpd").unwrap().params.as_ref().unwrap();
        assert_eq!(
            params["warmup_points"],
            json!(60),
            "production warmup, not the replay 40"
        );
        // Components not mentioned in the file still get the replay tuning.
        let holt = settings.component("holt_residual").unwrap().params.as_ref().unwrap();
        assert_eq!(holt["warmup_points"], json!(15));
    }

    #[test]
    fn explicit_scorer_entry_keeps_production_episodes_off() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("params.json");
        std::fs::write(&path, r#"{"components":{"anomaly_scorer":{"enabled":true}}}"#).unwrap();

        let settings = resolve(Some(&path), None, None, None, Some("0")).unwrap();
        assert!(settings.enabled("anomaly_scorer"));
        assert!(!settings.scorer().correlation_events);
        assert_eq!(settings.scorer().cooldown_secs, 300);
    }

    #[test]
    fn unknown_component_in_config_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("params.json");
        std::fs::write(&path, r#"{"components":{"nope":{"enabled":true}}}"#).unwrap();
        let err = resolve(Some(&path), None, None, None, Some("0")).unwrap_err();
        assert!(err.to_string().contains("unknown component"), "{err}");
    }

    #[test]
    fn enabling_omitted_components_is_rejected() {
        for name in ["time_cluster", "passthrough", "log_metrics_extractor"] {
            // Via an explicit config entry...
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("params.json");
            std::fs::write(&path, format!(r#"{{"components":{{"{name}":{{"enabled":true}}}}}}"#)).unwrap();
            let err = resolve(Some(&path), None, None, None, Some("0")).unwrap_err();
            assert!(err.to_string().contains("not migrated"), "config {name}: {err}");

            // ...and via --enable.
            let err = resolve(None, None, Some(name), None, Some("0")).unwrap_err();
            assert!(err.to_string().contains("not migrated"), "--enable {name}: {err}");
        }
    }

    #[test]
    fn disabling_omitted_components_is_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("params.json");
        std::fs::write(&path, r#"{"components":{"time_cluster":{"enabled":false}}}"#).unwrap();
        let settings = resolve(Some(&path), None, None, None, Some("0")).unwrap();
        assert!(!settings.enabled("time_cluster"));
    }

    #[test]
    fn nonzero_baseline_duration_is_rejected() {
        for value in ["7m", "120", "1s", "disabledx"] {
            let err = resolve(None, None, None, None, Some(value)).unwrap_err();
            assert!(err.to_string().contains("baseline"), "{value}: {err}");
        }
    }

    #[test]
    fn baseline_accepts_disabled_spellings() {
        for value in [None, Some(""), Some("0"), Some("disabled")] {
            assert!(resolve(None, None, None, None, value).is_ok(), "{value:?}");
        }
    }

    #[test]
    fn component_configs_include_enabled_and_params() {
        let settings = resolve_no_config();
        let configs = settings.component_configs();
        assert_eq!(configs["bocpd"]["enabled"], json!(true));
        assert_eq!(configs["bocpd"]["warmup_points"], json!(40));
        assert_eq!(configs["time_cluster"]["enabled"], json!(false));
        assert!(configs["time_cluster"].get("warmup_points").is_none());
    }

    #[test]
    fn scorer_config_parses_max_buckets() {
        let config = build_scorer_config(&json!({"max_buckets": 100_000_000}));
        assert_eq!(config.max_buckets, 100_000_000);
        // Absent means the live-Agent default (cap at window_secs).
        assert_eq!(build_scorer_config(&json!({})).max_buckets, 0);
    }
}
