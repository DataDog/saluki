//! Builds the typed configuration base from environment variables.
//!
//! The Datadog Agent reads configuration by iterating its table of known keys
//! and, for each, looking up that key's exact environment variable names, never by scanning the
//! environment or splitting on a separator. This module mirrors that: [`DATADOG_ENV_KEYS`] is
//! generated from the vendored schema, and [`apply_datadog_env`] looks up each key's real names,
//! decodes the first non-empty value into the JSON shape the schema declares (see
//! [`crate::env_decode`]), and writes it at the key's nested path.
//!
//! Each value is decoded and written directly to the nested path the typed deserializer reads.
//! Precedence relative to the config file is controlled by the caller through `overwrite`.

use std::collections::HashMap;

use serde_json::{Map, Value};

use crate::env_decode::{decode, EnvDecode};
use crate::generated::env_keys::DATADOG_ENV_KEYS;

/// One modeled configuration key and how to read it from the environment.
#[derive(Clone, Copy, Debug)]
pub struct EnvKey {
    /// The environment variable names bound to this key, highest priority first. The first one
    /// set to a non-empty value supplies the key's value, matching the Agent's per-key lookup; see
    /// [`EnvSnapshot`] for how case is matched.
    pub env_vars: &'static [&'static str],
    /// The nested path the typed deserializer reads (`["dogstatsd", "port"]`).
    pub path: &'static [&'static str],
    /// How to decode the raw string into the schema-declared JSON shape.
    pub decode: EnvDecode,
}

/// Reads every modeled Datadog key from the environment and writes decoded values into `base`.
///
/// Applies the canonical proxy variables (`HTTP_PROXY`/`HTTPS_PROXY`) as well. When `overwrite` is
/// true an environment value replaces whatever is already at its path (environment wins over the
/// file); when false it fills only an absent path (the file wins).
///
/// # Errors
///
/// Returns a message naming the environment variable when its value is malformed for the key's
/// declared shape. At startup this aborts the boot rather than silently dropping the value.
pub fn apply_datadog_env(base: &mut Value, overwrite: bool) -> Result<(), String> {
    apply_env_snapshot(base, EnvSnapshot::capture(), overwrite)
}

/// Reads every modeled Datadog key from explicitly provided environment variable name/value pairs.
///
/// Identical to [`apply_datadog_env`] except for where the variables come from. Tests use this to
/// avoid depending on the ambient process environment.
///
/// # Errors
///
/// Returns a message naming the environment variable when its value is malformed for the key's
/// declared shape.
pub fn apply_datadog_env_vars(
    base: &mut Value, vars: impl IntoIterator<Item = (String, String)>, overwrite: bool,
) -> Result<(), String> {
    apply_env_snapshot(base, EnvSnapshot::from_vars(vars), overwrite)
}

fn apply_env_snapshot(base: &mut Value, env: EnvSnapshot, overwrite: bool) -> Result<(), String> {
    let Some(root) = base.as_object_mut() else {
        return Ok(());
    };
    for key in DATADOG_ENV_KEYS {
        apply_one(root, &env, key.env_vars, key.path, key.decode, overwrite)?;
    }
    apply_proxy_env(root, &env, overwrite);
    Ok(())
}

/// The nested path of every modeled Datadog key.
///
/// The merge that layers the Agent config stream over the base uses these to tell a schema section
/// (an intermediate object it descends into) from a leaf (a value it replaces wholesale), so a
/// map-typed leaf is never key-unioned across sources.
pub fn datadog_leaf_paths() -> impl Iterator<Item = &'static [&'static str]> {
    DATADOG_ENV_KEYS.iter().map(|key| key.path)
}

/// Reads one key from the environment and writes it into `root`.
///
/// The public entry for callers with a runtime-built key set (the Saluki-only model), which cannot
/// use the `'static` [`EnvKey`] table. Same semantics as [`apply_datadog_env`] for a single key.
///
/// # Errors
///
/// Returns a message when the environment value is malformed for `decode`.
pub fn apply_env_at_path(
    base: &mut Value, env_vars: &[&str], path: &[&str], decode: EnvDecode, overwrite: bool,
) -> Result<(), String> {
    let Some(root) = base.as_object_mut() else {
        return Ok(());
    };
    let env = EnvSnapshot::capture();
    apply_one(root, &env, env_vars, path, decode, overwrite)
}

/// Reads one key from explicitly provided environment variable name/value pairs and writes it into
/// `root`.
///
/// Identical to [`apply_env_at_path`] except for where the variables come from. Tests use this to
/// avoid depending on the ambient process environment.
///
/// # Errors
///
/// Returns a message when the environment value is malformed for `decode`.
pub fn apply_env_at_path_vars(
    base: &mut Value, vars: impl IntoIterator<Item = (String, String)>, env_vars: &[&str], path: &[&str],
    decode: EnvDecode, overwrite: bool,
) -> Result<(), String> {
    let Some(root) = base.as_object_mut() else {
        return Ok(());
    };
    let env = EnvSnapshot::from_vars(vars);
    apply_one(root, &env, env_vars, path, decode, overwrite)
}

/// The canonical proxy variables that carry no `DD_` prefix.
///
/// `HTTP_PROXY` and `HTTPS_PROXY` are honored by the Datadog Agent but are not declared by the
/// vendored schema (they are wired only by the Agent's bespoke proxy handling), so they are not in
/// [`DATADOG_ENV_KEYS`] and are injected here by hand. They are a fallback below the schema's
/// `DD_PROXY_HTTP`/`DD_PROXY_HTTPS`: when the `DD_`-prefixed form is set, the canonical form is
/// ignored, matching the Agent's ordering. Canonical `NO_PROXY` is intentionally unsupported.
///
/// The canonical names are matched case-insensitively through [`EnvSnapshot`], so the common
/// lowercase Unix convention (`http_proxy`/`https_proxy`) is honored alongside the uppercase form.
fn apply_proxy_env(root: &mut Map<String, Value>, env: &EnvSnapshot, overwrite: bool) {
    const PROXY: &[(&str, &str, &[&str])] = &[
        ("DD_PROXY_HTTP", "HTTP_PROXY", &["proxy", "http"]),
        ("DD_PROXY_HTTPS", "HTTPS_PROXY", &["proxy", "https"]),
    ];
    for (dd_var, canonical_var, path) in PROXY {
        // The DD_-prefixed form is handled by the generated table and wins; only fill from the
        // canonical form when the DD_ form is unset.
        if env.contains(dd_var) {
            continue;
        }
        if let Some((_, raw)) = env.lookup(&[canonical_var]) {
            write_at_path(root, path, Value::String(raw.to_owned()), overwrite);
        }
    }
}

/// Reads, decodes, and writes one key.
fn apply_one(
    root: &mut Map<String, Value>, env: &EnvSnapshot, env_vars: &[&str], path: &[&str], how: EnvDecode, overwrite: bool,
) -> Result<(), String> {
    let Some((name, raw)) = env.lookup(env_vars) else {
        return Ok(());
    };
    let value = decode(raw, how).map_err(|msg| format!("environment variable `{name}` ({}): {msg}", path.join(".")))?;
    write_at_path(root, path, value, overwrite);
    Ok(())
}

/// A snapshot of the non-empty process environment, with exact-case lookup and a case-insensitive
/// fallback.
///
/// The Agent looks each bound name up with a case-sensitive `os.LookupEnv`, so an exact-case match
/// wins. ADP also accepts other case spellings (`dd_dogstatsd_port` for `DD_DOGSTATSD_PORT`), which
/// apply only when no bound name is set in its exact case.
struct EnvSnapshot {
    /// Non-empty values keyed by exact variable name.
    exact: HashMap<String, String>,
    /// The first-seen non-empty variable (name, value) for each lowercase name.
    folded: HashMap<String, (String, String)>,
}

impl EnvSnapshot {
    /// Captures the process environment, dropping empty values.
    fn capture() -> Self {
        Self::from_vars(std::env::vars())
    }

    /// Builds a snapshot from environment variable pairs.
    ///
    /// An empty string counts as unset, matching the Agent (`os.LookupEnv` plus an emptiness
    /// check).
    fn from_vars(vars: impl IntoIterator<Item = (String, String)>) -> Self {
        let mut exact = HashMap::new();
        let mut folded = HashMap::new();
        for (name, value) in vars {
            if value.is_empty() {
                continue;
            }
            folded
                .entry(name.to_lowercase())
                .or_insert_with(|| (name.clone(), value.clone()));
            exact.entry(name).or_insert(value);
        }
        Self { exact, folded }
    }

    /// Returns the actual name and value of the variable that supplies `names` (highest priority
    /// first).
    ///
    /// Tries every name exactly, in order, then every name case-insensitively, in order. Among case
    /// variants of one name, the first variable captured wins.
    fn lookup(&self, names: &[&str]) -> Option<(&str, &str)> {
        names
            .iter()
            .find_map(|name| self.exact.get_key_value(*name))
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .or_else(|| {
                names
                    .iter()
                    .find_map(|name| self.folded.get(&name.to_lowercase()))
                    .map(|(name, value)| (name.as_str(), value.as_str()))
            })
    }

    /// Whether any case variant of `name` is set to a non-empty value.
    fn contains(&self, name: &str) -> bool {
        self.folded.contains_key(&name.to_lowercase())
    }
}

/// Writes `value` at `path`, creating intermediate objects. When `overwrite` is false, an existing
/// value at the full path is left in place (the file wins).
fn write_at_path(root: &mut Map<String, Value>, path: &[&str], value: Value, overwrite: bool) {
    let Some((leaf, sections)) = path.split_last() else {
        return;
    };
    if !overwrite && path_present(root, path) {
        return;
    }
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    // Process environment is global; serialize the tests that mutate it. Each test removes the
    // variables it sets so they do not leak into sibling tests.
    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn at<'a>(base: &'a Value, path: &[&str]) -> Option<&'a Value> {
        let mut cur = base;
        for seg in path {
            cur = cur.get(seg)?;
        }
        Some(cur)
    }

    #[test]
    fn decodes_scalars_and_lists_to_shape() {
        let _guard = ENV_MUTEX.lock().unwrap();
        std::env::set_var("DD_DOGSTATSD_PORT", "9125");
        std::env::set_var("DD_API_KEY", "00000");
        std::env::set_var("DD_DOGSTATSD_TAGS", "env:prod team:core");

        let mut base = json!({});
        apply_datadog_env(&mut base, true).unwrap();

        std::env::remove_var("DD_DOGSTATSD_PORT");
        std::env::remove_var("DD_API_KEY");
        std::env::remove_var("DD_DOGSTATSD_TAGS");

        // Integer decodes to a JSON number; a numeric-looking API key stays a string (the schema
        // says `api_key` is a string); a string list splits into a real array.
        assert_eq!(at(&base, &["dogstatsd_port"]), Some(&json!(9125)));
        assert_eq!(at(&base, &["api_key"]), Some(&json!("00000")));
        assert_eq!(at(&base, &["dogstatsd_tags"]), Some(&json!(["env:prod", "team:core"])));
    }

    #[test]
    fn writes_at_nested_path() {
        let _guard = ENV_MUTEX.lock().unwrap();
        std::env::set_var("DD_APM_ERROR_TPS", "12.5");

        let mut base = json!({});
        apply_datadog_env(&mut base, true).unwrap();
        std::env::remove_var("DD_APM_ERROR_TPS");

        assert_eq!(at(&base, &["apm_config", "errors_per_second"]), Some(&json!(12.5)));
    }

    #[test]
    fn precedence_overwrite_versus_fill() {
        let _guard = ENV_MUTEX.lock().unwrap();
        std::env::set_var("DD_DOGSTATSD_PORT", "9125");

        let mut env_wins = json!({ "dogstatsd_port": 8125 });
        apply_datadog_env(&mut env_wins, true).unwrap();
        assert_eq!(at(&env_wins, &["dogstatsd_port"]), Some(&json!(9125)));

        let mut file_wins = json!({ "dogstatsd_port": 8125 });
        apply_datadog_env(&mut file_wins, false).unwrap();
        assert_eq!(at(&file_wins, &["dogstatsd_port"]), Some(&json!(8125)));

        std::env::remove_var("DD_DOGSTATSD_PORT");
    }

    #[test]
    fn env_var_names_are_matched_case_insensitively() {
        let env = EnvSnapshot::from_vars([
            ("dd_dogstatsd_port".to_string(), "9125".to_string()),
            ("Dd_Api_Key".to_string(), "00000".to_string()),
        ]);

        assert_eq!(env.lookup(&["DD_DOGSTATSD_PORT"]), Some(("dd_dogstatsd_port", "9125")));
        assert_eq!(env.lookup(&["DD_API_KEY"]), Some(("Dd_Api_Key", "00000")));
    }

    fn vars(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn apply_vars(pairs: &[(&str, &str)]) -> Result<Value, String> {
        let mut base = json!({});
        apply_datadog_env_vars(&mut base, vars(pairs), true)?;
        Ok(base)
    }

    #[test]
    fn the_first_alias_in_declared_order_wins() {
        // `DD_APM_MAX_EPS` is declared before `DD_MAX_EPS`, whatever order the environment lists them in.
        for pairs in [
            [("DD_APM_MAX_EPS", "100"), ("DD_MAX_EPS", "200")],
            [("DD_MAX_EPS", "200"), ("DD_APM_MAX_EPS", "100")],
        ] {
            let base = apply_vars(&pairs).unwrap();
            assert_eq!(at(&base, &["apm_config", "max_events_per_second"]), Some(&json!(100.0)));
        }

        let base = apply_vars(&[("DD_MAX_EPS", "200")]).unwrap();
        assert_eq!(at(&base, &["apm_config", "max_events_per_second"]), Some(&json!(200.0)));
    }

    #[test]
    fn an_empty_first_alias_falls_through_to_the_next() {
        let base = apply_vars(&[("DD_APM_MAX_EPS", ""), ("DD_MAX_EPS", "200")]).unwrap();
        assert_eq!(at(&base, &["apm_config", "max_events_per_second"]), Some(&json!(200.0)));
    }

    #[test]
    fn an_exact_case_name_beats_a_different_case_duplicate() {
        // The Agent's case-sensitive lookup sees only the exact spelling.
        for pairs in [
            [("dd_dogstatsd_port", "2"), ("DD_DOGSTATSD_PORT", "1")],
            [("DD_DOGSTATSD_PORT", "1"), ("dd_dogstatsd_port", "2")],
        ] {
            let base = apply_vars(&pairs).unwrap();
            assert_eq!(at(&base, &["dogstatsd_port"]), Some(&json!(1)));
        }
    }

    #[test]
    fn an_exact_case_later_alias_beats_a_different_case_earlier_alias() {
        let base = apply_vars(&[("dd_apm_max_eps", "100"), ("DD_MAX_EPS", "200")]).unwrap();
        assert_eq!(at(&base, &["apm_config", "max_events_per_second"]), Some(&json!(200.0)));
    }

    #[test]
    fn the_decode_error_names_the_variable_that_was_read() {
        let error = apply_vars(&[("DD_MAX_EPS", "fast")]).unwrap_err();
        assert!(error.starts_with("environment variable `DD_MAX_EPS` (apm_config.max_events_per_second): "));

        // A case-insensitive match is reported under the name it was set with.
        let error = apply_vars(&[("dd_max_eps", "fast")]).unwrap_err();
        assert!(error.starts_with("environment variable `dd_max_eps` (apm_config.max_events_per_second): "));
    }

    #[test]
    fn agent_integer_and_list_spellings_decode_to_shape() {
        let base = apply_vars(&[
            ("DD_DOGSTATSD_PORT", "0x10"),
            ("DD_APM_OBFUSCATION_ELASTICSEARCH_KEEP_VALUES", r#"["cr-a","cr-b"]"#),
        ])
        .unwrap();
        assert_eq!(at(&base, &["dogstatsd_port"]), Some(&json!(16)));
        assert_eq!(
            at(&base, &["apm_config", "obfuscation", "elasticsearch", "keep_values"]),
            Some(&json!(["cr-a", "cr-b"]))
        );
    }

    #[test]
    fn canonical_proxy_is_a_fallback_below_dd_form() {
        let _guard = ENV_MUTEX.lock().unwrap();

        // Canonical form alone fills the slot.
        std::env::set_var("HTTP_PROXY", "http://canonical:3128");
        let mut base = json!({});
        apply_datadog_env(&mut base, true).unwrap();
        assert_eq!(at(&base, &["proxy", "http"]), Some(&json!("http://canonical:3128")));

        // The DD_ form wins when both are set.
        std::env::set_var("DD_PROXY_HTTP", "http://prefixed:3128");
        let mut base = json!({});
        apply_datadog_env(&mut base, true).unwrap();
        assert_eq!(at(&base, &["proxy", "http"]), Some(&json!("http://prefixed:3128")));

        std::env::remove_var("HTTP_PROXY");
        std::env::remove_var("DD_PROXY_HTTP");
    }

    #[test]
    fn lowercase_proxy_env_is_honored() {
        let env = EnvSnapshot::from_vars([("http_proxy".to_string(), "http://lower:3128".to_string())]);
        let mut base = json!({});
        apply_proxy_env(base.as_object_mut().unwrap(), &env, true);

        assert_eq!(at(&base, &["proxy", "http"]), Some(&json!("http://lower:3128")));
    }

    #[test]
    fn malformed_value_propagates() {
        let _guard = ENV_MUTEX.lock().unwrap();
        std::env::set_var("DD_DOGSTATSD_PORT", "not-a-number");
        let mut base = json!({});
        let result = apply_datadog_env(&mut base, true);
        std::env::remove_var("DD_DOGSTATSD_PORT");
        assert!(result.is_err());
    }
}
