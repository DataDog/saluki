//! Overlay flat environment-variable keys onto the nested Datadog configuration shape.
//!
//! Environment variables reach the config map as flat, underscore-joined top-level keys:
//! `DD_AUTOSCALING_FAILOVER_ENABLED` lands as `autoscaling_failover_enabled`, not as the nested
//! `autoscaling.failover.enabled` object. [`DatadogConfiguration`](crate::DatadogConfiguration)
//! deserializes the nested shape, so it never reads those flat keys and the value is silently lost.
//!
//! The legacy per-key `GenericConfiguration::get` bridged this at query time by retrying the dotted
//! key with `.` replaced by `_`. The typed path deserializes the whole struct at once and never
//! issues per-key queries, so that bridge no longer fires. This module reinstates it as a one-time
//! pass over the merged value, driven by the generated table of supported multi-segment keys
//! ([`ENV_OVERLAY_KEYS`]): for each known dotted path, it relocates any matching flat key into the
//! nested slot the deserializer reads.
//!
//! The relocation runs the safe, unambiguous direction (known path to its single flat form), so it
//! never has to guess where the underscores in an arbitrary flat key are nesting boundaries.
//!
//! This pass only *relocates*; it never reshapes the value. A flat env value is moved verbatim, even
//! for string lists: `DatadogConfiguration`'s string-list leaves deserialize with a shape-tolerant
//! reader (see the generated crate's `list_de`) that splits a space-separated env string into a
//! sequence. Keeping shape at the deserializer means this table needs to know only the path, not the
//! type.

use serde_json::{Map, Value};

use crate::generated::env_overlay_keys::ENV_OVERLAY_KEYS;

/// How a flat environment value relates to the value already in the nested slot.
///
/// Chosen at the point of deserialization, once the merged map is in hand.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnvOverlayMode {
    /// Do not relocate anything. The typed deserializer sees only the nested shape.
    Disabled,
    /// Fill a nested slot only when it is absent. Matches the legacy `get` precedence, where a
    /// value already supplied nested (for example by the Datadog Agent) wins over the flat env key.
    Fallback,
    /// Relocate the flat env value even when the nested slot is already populated, so an env var
    /// overrides an Agent-supplied value.
    Override,
}

/// One supported Datadog key: the flat env forms that reach it and its nested path.
#[derive(Clone, Copy, Debug)]
pub struct EnvOverlayKey {
    /// The flat, underscore-joined keys an environment variable produces for this field
    /// (`autoscaling_failover_enabled`). A field may be reachable through several env vars; they are
    /// listed here in schema priority order, **highest priority first**. When more than one is
    /// present in the merged map, [`apply_env_overlay`] takes the value from the first that appears,
    /// matching the Agent (for example `dd_url` prefers `DD_DD_URL` over `DD_URL`).
    pub flats: &'static [&'static str],
    /// The nested path the typed deserializer reads (`["autoscaling", "failover", "enabled"]`).
    pub path: &'static [&'static str],
}

/// Relocates flat environment-variable keys in `merged` into the nested slots the Datadog
/// deserializer reads, according to `mode`.
///
/// Operates on the caller's owned `merged` value; it never touches the shared source map. Apply it
/// only to the value fed to [`DatadogConfiguration`](crate::DatadogConfiguration): the table covers
/// only supported Datadog keys.
pub fn apply_env_overlay(merged: &mut Value, mode: EnvOverlayMode) {
    let clobber = match mode {
        EnvOverlayMode::Disabled => return,
        EnvOverlayMode::Override => true,
        EnvOverlayMode::Fallback => false,
    };

    let Some(root) = merged.as_object_mut() else {
        return;
    };

    for key in ENV_OVERLAY_KEYS {
        // Pick the source from the highest-priority present flat; `clobber` then only decides whether
        // it replaces a value already sitting in the nested slot.
        let Some(flat) = select_flat(root, key.flats) else {
            continue;
        };
        let value = flat.clone();

        if !clobber && path_present(root, key.path) {
            continue;
        }

        set_at_path(root, key.path, value);
    }
}

/// Chooses the source value for a key from its flat env forms, honoring priority.
///
/// `flats` is ordered highest priority first, so this returns the value of the first form present in
/// `root`. That makes the winning env var independent of the overlay mode: when several env vars for
/// one key are set, the highest-priority one always supplies the value (for example `dd_url` prefers
/// `DD_DD_URL` over `DD_URL`).
fn select_flat<'a>(root: &'a Map<String, Value>, flats: &[&str]) -> Option<&'a Value> {
    flats.iter().find_map(|flat| root.get(*flat))
}

/// Returns whether a leaf already exists at `path`, walking object nodes from `root`.
fn path_present(root: &Map<String, Value>, path: &[&str]) -> bool {
    let Some((first, rest)) = path.split_first() else {
        return false;
    };
    let mut current = match root.get(*first) {
        Some(v) => v,
        None => return false,
    };
    for segment in rest {
        match current.get(*segment) {
            Some(v) => current = v,
            None => return false,
        }
    }
    true
}

/// Sets `value` at `path`, creating intermediate object nodes as needed and clobbering the leaf.
///
/// An intermediate segment that exists but is not an object is replaced with one; this cannot lose
/// a real value, since a supported key's parents are always sections (objects).
fn set_at_path(root: &mut Map<String, Value>, path: &[&str], value: Value) {
    let Some((leaf, sections)) = path.split_last() else {
        return;
    };

    let mut current = root;
    for segment in sections {
        let node = current
            .entry((*segment).to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if !node.is_object() {
            *node = Value::Object(Map::new());
        }
        current = node.as_object_mut().expect("node was just ensured to be an object");
    }

    current.insert((*leaf).to_string(), value);
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    // Real supported keys the generated table carries: a nested boolean and a nested string list.
    const ENABLED_FLAT: &str = "autoscaling_failover_enabled";
    const METRICS_FLAT: &str = "autoscaling_failover_metrics";

    fn enabled(v: &Value) -> Option<&Value> {
        v.get("autoscaling")?.get("failover")?.get("enabled")
    }

    #[test]
    fn disabled_relocates_nothing() {
        let mut v = json!({ ENABLED_FLAT: true });
        apply_env_overlay(&mut v, EnvOverlayMode::Disabled);
        assert!(enabled(&v).is_none());
    }

    #[test]
    fn fallback_fills_absent_nested_slot() {
        let mut v = json!({ ENABLED_FLAT: true });
        apply_env_overlay(&mut v, EnvOverlayMode::Fallback);
        assert_eq!(enabled(&v), Some(&Value::Bool(true)));
    }

    #[test]
    fn fallback_keeps_present_nested_value() {
        let mut v = json!({ ENABLED_FLAT: true, "autoscaling": { "failover": { "enabled": false } } });
        apply_env_overlay(&mut v, EnvOverlayMode::Fallback);
        assert_eq!(enabled(&v), Some(&Value::Bool(false)));
    }

    #[test]
    fn override_clobbers_present_nested_value() {
        let mut v = json!({ ENABLED_FLAT: true, "autoscaling": { "failover": { "enabled": false } } });
        apply_env_overlay(&mut v, EnvOverlayMode::Override);
        assert_eq!(enabled(&v), Some(&Value::Bool(true)));
    }

    #[test]
    fn string_list_env_value_is_relocated_verbatim() {
        // The overlay only relocates; it does not reshape. The space-separated env string lands at
        // the nested path unchanged — splitting into a sequence is the deserializer's job (see the
        // string-list leaf's shape-tolerant reader, tested in `list_de`).
        let mut v = json!({ METRICS_FLAT: "container.memory.usage container.cpu.usage" });
        apply_env_overlay(&mut v, EnvOverlayMode::Fallback);
        let metrics = v
            .get("autoscaling")
            .unwrap()
            .get("failover")
            .unwrap()
            .get("metrics")
            .unwrap();
        assert_eq!(metrics, &json!("container.memory.usage container.cpu.usage"));
    }

    #[test]
    fn set_at_path_creates_intermediate_objects() {
        let mut root = Map::new();
        set_at_path(&mut root, &["a", "b", "c"], Value::Bool(true));
        assert_eq!(Value::Object(root), json!({ "a": { "b": { "c": true } } }));
    }

    // The following tests guard the fix for the `apm_config.*` keys whose environment variables
    // diverge from the mechanical `a.b.c` -> `a_b_c` form. Their env vars drop the `_config`
    // segment (`DD_APM_ERROR_TRACKING_STANDALONE_ENABLED` -> `apm_error_tracking_standalone_enabled`),
    // and two are renamed outright (`errors_per_second` -> `DD_APM_ERROR_TPS`). The generated table
    // must carry the schema's real env names, not the dotted path, or these fields stay unreachable.

    fn overlay_key(flat: &str) -> Option<&'static EnvOverlayKey> {
        ENV_OVERLAY_KEYS.iter().find(|k| k.flats.contains(&flat))
    }

    #[test]
    fn table_uses_schema_env_names_for_divergent_apm_keys() {
        // The real (schema-declared) env flat forms are present, mapped to their nested path.
        let cases = [
            (
                "apm_error_tracking_standalone_enabled",
                &["apm_config", "error_tracking_standalone", "enabled"][..],
            ),
            ("apm_enable_rare_sampler", &["apm_config", "enable_rare_sampler"][..]),
            ("apm_error_tps", &["apm_config", "errors_per_second"][..]),
            ("apm_target_tps", &["apm_config", "target_traces_per_second"][..]),
            (
                "apm_probabilistic_sampler_sampling_percentage",
                &["apm_config", "probabilistic_sampler", "sampling_percentage"][..],
            ),
        ];
        for (flat, path) in cases {
            let key = overlay_key(flat).unwrap_or_else(|| panic!("missing overlay key {flat:?}"));
            assert_eq!(key.path, path, "wrong nested path for {flat:?}");
        }
    }

    #[test]
    fn table_omits_mechanical_forms_for_overridden_apm_keys() {
        // The old, wrong dot-to-underscore forms must not appear: the Agent does not accept them
        // once explicit env names are declared, so relocating them would over-accept.
        for wrong in [
            "apm_config_error_tracking_standalone_enabled",
            "apm_config_enable_rare_sampler",
            "apm_config_errors_per_second",
        ] {
            assert!(
                overlay_key(wrong).is_none(),
                "unexpected mechanical overlay key {wrong:?}"
            );
        }
    }

    #[test]
    fn divergent_env_key_reaches_nested_slot() {
        // Reachability: the env-only flat key lands in the nested slot the typed model reads.
        let mut v = json!({ "apm_error_tracking_standalone_enabled": true });
        apply_env_overlay(&mut v, EnvOverlayMode::Fallback);
        assert_eq!(
            v.get("apm_config")
                .and_then(|c| c.get("error_tracking_standalone"))
                .and_then(|e| e.get("enabled")),
            Some(&Value::Bool(true)),
        );
    }

    #[test]
    fn divergent_env_key_fallback_yields_to_agent_value() {
        // Precedence under Fallback: an Agent-supplied nested value wins over the env key.
        let mut v = json!({
            "apm_enable_rare_sampler": true,
            "apm_config": { "enable_rare_sampler": false },
        });
        apply_env_overlay(&mut v, EnvOverlayMode::Fallback);
        assert_eq!(
            v.get("apm_config").and_then(|c| c.get("enable_rare_sampler")),
            Some(&Value::Bool(false)),
        );
    }

    #[test]
    fn divergent_env_key_override_replaces_agent_value() {
        // Override precedence: the env key clobbers an Agent-supplied nested value.
        let mut v = json!({
            "apm_enable_rare_sampler": true,
            "apm_config": { "enable_rare_sampler": false },
        });
        apply_env_overlay(&mut v, EnvOverlayMode::Override);
        assert_eq!(
            v.get("apm_config").and_then(|c| c.get("enable_rare_sampler")),
            Some(&Value::Bool(true)),
        );
    }

    #[test]
    fn select_flat_honors_priority_order() {
        // `flats` is highest priority first, so the first present form supplies the value even when a
        // lower-priority form is also set.
        let root = json!({ "dd_dd_url": "primary", "url": "secondary" });
        let root = root.as_object().unwrap();
        assert_eq!(
            select_flat(root, &["dd_dd_url", "url"]),
            Some(&Value::String("primary".to_string())),
        );
        // With only the lower-priority form present, it is used.
        let root = json!({ "url": "secondary" });
        let root = root.as_object().unwrap();
        assert_eq!(
            select_flat(root, &["dd_dd_url", "url"]),
            Some(&Value::String("secondary".to_string())),
        );
    }

    #[test]
    fn aliased_env_key_reaches_single_segment_slot() {
        // `dd_url` carries two env vars: `DD_DD_URL` (flat `dd_url`, already in place) and `DD_URL`
        // (flat `url`). Only the divergent `url` alias needs relocation into `dd_url`.
        assert!(
            overlay_key("dd_url").is_none(),
            "no-op alias should not be in the table"
        );
        assert_eq!(overlay_key("url").map(|k| k.path), Some(&["dd_url"][..]));

        let mut v = json!({ "url": "https://example.com" });
        apply_env_overlay(&mut v, EnvOverlayMode::Fallback);
        assert_eq!(v.get("dd_url"), Some(&Value::String("https://example.com".to_string())));
    }
}
