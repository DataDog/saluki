//! Builds the typed configuration base from environment variables.
//!
//! [`DATADOG_ENV_KEYS`] maps the Agent's declared variable names to nested configuration paths;
//! paths are not inferred by splitting names. [`apply_datadog_env_vars`] decodes each key's first
//! non-empty environment value using [`crate::env_decode`]. Callers choose precedence with `overwrite`.

use std::collections::HashMap;

use serde_json::{Map, Value};

use crate::env_decode::{decode, EnvDecode};
use crate::generated::env_keys::DATADOG_ENV_KEYS;

/// One modeled configuration key and how to read it from the environment.
#[derive(Clone, Copy, Debug)]
pub struct EnvKey {
    /// The environment variable names bound to this key, highest priority first. The first one
    /// set to a non-empty value supplies the key's value, matching the Agent's per-key lookup.
    pub env_vars: &'static [&'static str],
    /// The nested path the typed deserializer reads (for example, `["proxy", "http"]`).
    pub path: &'static [&'static str],
    /// How to decode the raw string into the schema-declared JSON shape.
    pub decode: EnvDecode,
}

/// Decodes supplied environment variables into modeled Datadog paths in `base`.
///
/// Also applies `HTTP_PROXY`/`HTTPS_PROXY` as fallbacks for their `DD_` forms. When `overwrite` is
/// true, environment values replace existing values; otherwise, only absent paths are filled.
///
/// # Errors
///
/// Returns a message naming the environment variable when its value is malformed for the key's
/// declared shape. At startup this aborts the boot rather than silently dropping the value.
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

/// Decodes one key from the process environment into `base` at the supplied path.
///
/// Reads the first non-empty variable named in `env_vars`. When `overwrite` is false, existing values
/// win.
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

/// Decodes one key from supplied environment variables into `base` at the supplied path.
///
/// Identical to [`apply_env_at_path`] except for where the variables come from. Callers that read
/// the process environment once and reuse it pass it here, and tests pass their own variables so
/// that they do not depend on the ambient process environment.
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
        if let Some(raw) = env.lookup(&[canonical_var]) {
            write_at_path(root, path, Value::String(raw), overwrite);
        }
    }
}

/// Reads, decodes, and writes one key.
fn apply_one(
    root: &mut Map<String, Value>, env: &EnvSnapshot, env_vars: &[&str], path: &[&str], how: EnvDecode, overwrite: bool,
) -> Result<(), String> {
    let Some(raw) = env.lookup(env_vars) else {
        return Ok(());
    };
    let value = decode(&raw, how).map_err(|msg| {
        let name = env_vars.first().copied().unwrap_or_default();
        format!("environment variable `{name}` ({}): {msg}", path.join("."))
    })?;
    write_at_path(root, path, value, overwrite);
    Ok(())
}

/// A case-insensitive environment snapshot: uppercase and lowercase variable names resolve to the same key.
struct EnvSnapshot {
    /// Environment values keyed by lowercase variable name. Only non-empty values are kept.
    vars: HashMap<String, String>,
}

impl EnvSnapshot {
    /// Captures the environment, converting names to lowercase and dropping empty values.
    fn capture() -> Self {
        Self::from_vars(std::env::vars())
    }

    /// Builds a snapshot from environment variable pairs.
    ///
    /// An empty string counts as unset, matching the Agent (`os.LookupEnv` plus an emptiness
    /// check). If several case variants of one name are set, the first seen wins.
    fn from_vars(vars: impl IntoIterator<Item = (String, String)>) -> Self {
        let mut snapshot = HashMap::new();
        for (name, value) in vars {
            if value.is_empty() {
                continue;
            }
            snapshot.entry(name.to_lowercase()).or_insert(value);
        }
        Self { vars: snapshot }
    }

    /// Returns the value of the first name in `names` (highest priority first) that is set to a
    /// non-empty value, matched case-insensitively.
    fn lookup(&self, names: &[&str]) -> Option<String> {
        names
            .iter()
            .find_map(|name| self.vars.get(&name.to_lowercase()).cloned())
    }

    /// Whether any case variant of `name` is set to a non-empty value.
    fn contains(&self, name: &str) -> bool {
        self.vars.contains_key(&name.to_lowercase())
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

    fn vars(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn at<'a>(base: &'a Value, path: &[&str]) -> Option<&'a Value> {
        let mut cur = base;
        for seg in path {
            cur = cur.get(seg)?;
        }
        Some(cur)
    }

    #[test]
    fn decodes_scalars_and_lists_to_shape() {
        let env = vars(&[
            ("DD_DOGSTATSD_PORT", "9125"),
            ("DD_API_KEY", "00000"),
            ("DD_DOGSTATSD_TAGS", "env:prod team:core"),
        ]);

        let mut base = json!({});
        apply_datadog_env_vars(&mut base, env, true).unwrap();

        // Numeric-looking API keys must remain strings.
        assert_eq!(at(&base, &["dogstatsd_port"]), Some(&json!(9125)));
        assert_eq!(at(&base, &["api_key"]), Some(&json!("00000")));
        assert_eq!(at(&base, &["dogstatsd_tags"]), Some(&json!(["env:prod", "team:core"])));
    }

    #[test]
    fn writes_at_nested_path() {
        let mut base = json!({});
        apply_datadog_env_vars(&mut base, vars(&[("DD_APM_ERROR_TPS", "12.5")]), true).unwrap();

        assert_eq!(at(&base, &["apm_config", "errors_per_second"]), Some(&json!(12.5)));
    }

    #[test]
    fn precedence_overwrite_versus_fill() {
        let env = vars(&[("DD_DOGSTATSD_PORT", "9125")]);

        let mut env_wins = json!({ "dogstatsd_port": 8125 });
        apply_datadog_env_vars(&mut env_wins, env.clone(), true).unwrap();
        assert_eq!(at(&env_wins, &["dogstatsd_port"]), Some(&json!(9125)));

        let mut file_wins = json!({ "dogstatsd_port": 8125 });
        apply_datadog_env_vars(&mut file_wins, env, false).unwrap();
        assert_eq!(at(&file_wins, &["dogstatsd_port"]), Some(&json!(8125)));
    }

    #[test]
    fn env_var_names_are_matched_case_insensitively() {
        let env = EnvSnapshot::from_vars([
            ("dd_dogstatsd_port".to_string(), "9125".to_string()),
            ("Dd_Api_Key".to_string(), "00000".to_string()),
        ]);

        assert_eq!(env.lookup(&["DD_DOGSTATSD_PORT"]).as_deref(), Some("9125"));
        assert_eq!(env.lookup(&["DD_API_KEY"]).as_deref(), Some("00000"));
    }

    #[test]
    fn canonical_proxy_is_a_fallback_below_dd_form() {
        let canonical = ("HTTP_PROXY", "http://canonical:3128");
        let mut base = json!({});
        apply_datadog_env_vars(&mut base, vars(&[canonical]), true).unwrap();
        assert_eq!(at(&base, &["proxy", "http"]), Some(&json!("http://canonical:3128")));

        let both = vars(&[canonical, ("DD_PROXY_HTTP", "http://prefixed:3128")]);
        let mut base = json!({});
        apply_datadog_env_vars(&mut base, both, true).unwrap();
        assert_eq!(at(&base, &["proxy", "http"]), Some(&json!("http://prefixed:3128")));
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
        let mut base = json!({});
        let result = apply_datadog_env_vars(&mut base, vars(&[("DD_DOGSTATSD_PORT", "not-a-number")]), true);
        assert!(result.is_err());
    }
}
