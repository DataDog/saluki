//! Participant type declarations, effective configuration and realization-specific early facts.

use std::collections::BTreeMap;

use crate::{
    case::{FileSource, ParticipantConfig},
    layout::{Os, Realization, RealizationKind},
};

/// One participant's case configuration for its participant OS, after applying that OS's overlay.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CaseValues {
    /// Environment variables after applying the overlay. Defaults to empty.
    pub env: BTreeMap<String, String>,
    /// File sources as written, relative to the case directory. Defaults to empty.
    pub files: BTreeMap<String, FileSource>,
    /// Type-specific parameters; overlay values replace whole base values by key. Defaults to empty.
    pub params: BTreeMap<String, serde_json::Value>,
}

impl CaseValues {
    /// Applies the participant OS's overlay; each key replaces the whole base value.
    pub(crate) fn for_os(config: &ParticipantConfig, os: Os) -> Self {
        let mut values = Self {
            env: config.env.clone(),
            files: config.files.clone(),
            params: config.params.clone(),
        };
        if let Some(overlay) = config.os.get(&os) {
            values.env.extend(overlay.env.clone());
            values.files.extend(overlay.files.clone());
            values.params.extend(overlay.params.clone());
        }
        values
    }
}

/// A participant's configuration: its type's defaults with the case's values applied over them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EffectiveConfig {
    /// Type defaults with case environment variables replacing matching keys. Defaults to empty.
    pub env: BTreeMap<String, String>,
    /// Case-supplied file sources, relative to the case directory. Defaults to empty.
    pub files: BTreeMap<String, FileSource>,
}

impl EffectiveConfig {
    /// Applies case environment values over type defaults and takes files from the case.
    pub(crate) fn assemble(defaults: BTreeMap<String, String>, case: CaseValues) -> Self {
        let mut env = defaults;
        env.extend(case.env);
        Self { env, files: case.files }
    }
}

/// A participant fact available before execution for one realization.
pub struct FactDeclaration {
    /// Name used in participant fact references.
    pub name: &'static str,
    /// Literal value for the realization.
    pub value: &'static str,
}

/// A participant type a case may name, and what it accepts and provides.
pub trait ParticipantType: Send + Sync {
    /// Registry name, as written in a participant's `type`.
    fn name(&self) -> &'static str;

    /// Supported (participant OS, realization kind) pairs. Defaults to none.
    /// Every selected pair must be declared here.
    fn supports(&self) -> &'static [(Os, RealizationKind)] {
        &[]
    }

    /// Names a case may supply under `config.files`. Defaults to none.
    fn config_files(&self) -> &'static [&'static str] {
        &[]
    }

    /// Served endpoints: connection targets and keys of `config.endpoints`. Defaults to none.
    fn serves(&self) -> &'static [&'static str] {
        &[]
    }

    /// Consumed endpoints: keys of `connect`. Defaults to none.
    fn consumes(&self) -> &'static [&'static str] {
        &[]
    }

    /// Names a case may set under `config.params`. Defaults to none.
    fn params(&self) -> &'static [&'static str] {
        &[]
    }

    /// Checks the case's configuration after its OS overlay. Defaults to accepting it.
    /// Called only with realizations from [`Self::supports`].
    ///
    /// # Errors
    ///
    /// Returns an error describing configuration this type cannot use for the realization.
    fn validate_config(&self, _realization: Realization, _config: &CaseValues) -> Result<(), String> {
        Ok(())
    }

    /// Environment defaults for one realization. The case's `env` replaces matching keys.
    /// Omit defaults the case sets in another form the product reads with lower precedence,
    /// such as a key in a supplied configuration file. Defaults to none.
    /// Called after [`Self::validate_config`], only with realizations from [`Self::supports`].
    fn default_env(&self, _realization: Realization, _config: &CaseValues) -> BTreeMap<String, String> {
        BTreeMap::new()
    }

    /// Early facts steps may reference for this realization, sorted by name. Defaults to none.
    /// Called only with realizations from [`Self::supports`].
    fn facts(&self, _realization: Realization) -> &'static [FactDeclaration] {
        &[]
    }

    /// Named processes steps may address with `process`, in sorted order.
    fn processes(&self) -> &'static [&'static str];
}

/// Finds a registered participant type by its case-file name.
pub(crate) fn find_type<'a>(types: &[&'a dyn ParticipantType], name: &str) -> Option<&'a dyn ParticipantType> {
    types
        .iter()
        .copied()
        .find(|participant_type| participant_type.name() == name)
}

/// Describes an unknown participant type and lists the registered alternatives.
pub(crate) fn unknown_type_message(participant: &str, type_name: &str, types: &[&dyn ParticipantType]) -> String {
    format!(
        "participant '{participant}' has unknown type '{type_name}'; registered types: {}",
        types
            .iter()
            .map(|participant_type| participant_type.name())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const CONFIG: &str = r#"
env: {A: base, B: base}
files: {settings: base.yaml, scripts: scripts/}
params: {mode: base, nested: {keep: true, replace: base}}
os:
  windows:
    env: {A: windows, C: windows}
    files: {settings: {from: windows.yaml, template: true}, extra: extra/}
    params: {nested: {replace: windows}, extra: [a, b]}
"#;

    #[test]
    fn an_os_overlay_replaces_and_adds_env_files_and_params_only_on_its_os() {
        let config: ParticipantConfig = serde_saphyr::from_str(CONFIG).unwrap();
        let values = CaseValues::for_os(&config, Os::Windows);
        assert_eq!(
            values.env,
            BTreeMap::from([
                ("A".into(), "windows".into()),
                ("B".into(), "base".into()),
                ("C".into(), "windows".into()),
            ])
        );
        assert_eq!(
            values.files,
            BTreeMap::from([
                (
                    "settings".into(),
                    FileSource {
                        from: "windows.yaml".into(),
                        template: true
                    }
                ),
                (
                    "scripts".into(),
                    FileSource {
                        from: "scripts/".into(),
                        template: false
                    }
                ),
                (
                    "extra".into(),
                    FileSource {
                        from: "extra/".into(),
                        template: false
                    }
                ),
            ])
        );
        assert_eq!(
            values.params,
            BTreeMap::from([
                ("mode".into(), json!("base")),
                ("nested".into(), json!({"replace": "windows"})),
                ("extra".into(), json!(["a", "b"])),
            ])
        );
        assert_eq!(config.env["A"], "base");
        assert_eq!(config.files["settings"].from.to_str(), Some("base.yaml"));
        assert_eq!(config.params["nested"], json!({"keep": true, "replace": "base"}));
    }

    #[test]
    fn a_participant_without_an_overlay_for_its_os_keeps_its_base_values() {
        let config: ParticipantConfig = serde_saphyr::from_str(CONFIG).unwrap();
        for os in [Os::Linux, Os::MacOs] {
            assert_eq!(
                CaseValues::for_os(&config, os),
                CaseValues {
                    env: config.env.clone(),
                    files: config.files.clone(),
                    params: config.params.clone(),
                }
            );
        }
    }

    #[test]
    fn case_env_replaces_type_defaults_and_keeps_the_other_defaults() {
        let config: ParticipantConfig = serde_saphyr::from_str(CONFIG).unwrap();
        let effective = EffectiveConfig::assemble(
            BTreeMap::from([("A".into(), "default".into()), ("D".into(), "default".into())]),
            CaseValues::for_os(&config, Os::Linux),
        );
        assert_eq!(
            effective.env,
            BTreeMap::from([
                ("A".into(), "base".into()),
                ("B".into(), "base".into()),
                ("D".into(), "default".into()),
            ])
        );
        assert_eq!(effective.files, config.files);
    }
}
