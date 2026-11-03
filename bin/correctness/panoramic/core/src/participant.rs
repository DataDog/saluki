//! Participant type declarations, effective configuration and realization-specific early facts.

use std::collections::BTreeMap;

use crate::{
    artifact::ArtifactRole,
    backend::LateFact,
    case::{EndpointOverride, FileSource, ParticipantConfig},
    endpoint::{ConsumedEndpoint, Endpoint, Evidence},
    layout::{Os, Realization, RealizationKind},
    observe::output::host_output_file,
    recipe::Recipe,
};

/// One participant's case configuration for its participant OS, after applying that OS's overlay.
///
/// Contains only case values, before type defaults and connection deliveries.
///
/// In `env` and `params`, fact references and fixed late facts (a host process group's `ip`) are already substituted.
/// Other references stay exactly as written: another participant's `container_id` and endpoint and connection
/// addresses resolve at creation; the participant's own late facts listed by
/// [`ParticipantType::substituted_late_facts`] resolve after start.
/// A type that must interpret an unresolved value reports a configuration error.
/// Files marked `template: true` keep their references as written; their references are checked for this OS.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CaseValues {
    /// Environment variables after applying the overlay. Defaults to empty.
    pub env: BTreeMap<String, String>,
    /// File sources as written, relative to the case directory. Defaults to empty.
    /// Files marked `template: true` keep their references as written, checked for this OS.
    pub files: BTreeMap<String, FileSource>,
    /// Type-specific parameters; overlay values replace whole base values by key. Defaults to empty.
    pub params: BTreeMap<String, serde_json::Value>,
    /// Base endpoint overrides, shared by every OS. Defaults to empty.
    pub endpoints: BTreeMap<String, EndpointOverride>,
}

impl CaseValues {
    /// Applies the participant OS's overlay; each key replaces the whole base value.
    pub(crate) fn for_os(config: &ParticipantConfig, os: Os) -> Self {
        let mut values = Self {
            env: config.env.clone(),
            files: config.files.clone(),
            params: config.params.clone(),
            endpoints: config.endpoints.clone(),
        };
        if let Some(overlay) = config.os.get(&os) {
            values.env.extend(overlay.env.clone());
            values.files.extend(overlay.files.clone());
            values.params.extend(overlay.params.clone());
        }
        values
    }
}

/// A participant's configuration: type defaults, connection deliveries, then case values, with later layers winning.
///
/// Type defaults include assigned-port values. Case environment variables can pin those ports.
/// The `validate_config`, `default_env`, `endpoints`, `readiness` and `consumes` hooks see configuration
/// before connection deliveries.
/// After port assignment, `endpoints`, `readiness` and `consumes` see the reassembled configuration,
/// including assigned values.
/// A type must derive what it serves and consumes independently of where it connects.
/// Case references follow the substitution and checking rules of [`CaseValues`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EffectiveConfig {
    /// Type defaults (including assigned ports), then connection deliveries, then case variables. Defaults to empty.
    /// Case values must not set a key delivered by a connection.
    pub env: BTreeMap<String, String>,
    /// Case-supplied file sources, relative to the case directory. Defaults to empty.
    /// Files marked `template: true` keep their references as written, checked for this OS.
    pub files: BTreeMap<String, FileSource>,
    /// Case parameters after the OS overlay, interpreted by the participant type. Defaults to empty.
    pub params: BTreeMap<String, serde_json::Value>,
    /// Case-supplied ports for listeners whose port the type cannot derive. Defaults to empty.
    pub endpoints: BTreeMap<String, EndpointOverride>,
}

impl EffectiveConfig {
    /// Applies case environment values over type defaults, preserving files, parameters and endpoint overrides.
    pub(crate) fn assemble(defaults: BTreeMap<String, String>, case: CaseValues) -> Self {
        let mut env = defaults;
        env.extend(case.env);
        Self {
            env,
            files: case.files,
            params: case.params,
            endpoints: case.endpoints,
        }
    }
}

/// Configuration rendered for container creation, with case files keyed by configuration name.
pub struct RenderedConfig {
    pub(crate) env: BTreeMap<String, String>,
    pub(crate) params: BTreeMap<String, serde_json::Value>,
    pub(crate) files: BTreeMap<String, Vec<u8>>,
}

/// The core-provided fact naming a participant's directory for served sockets.
pub const SHARED_DIR_FACT: &str = "shared_dir";

/// A participant fact available before execution for one realization.
pub struct FactDeclaration {
    /// Name used in participant fact references.
    pub name: &'static str,
    /// Literal value for the realization.
    pub value: &'static str,
}

/// How a named process's exit and output are observed on one realization.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessSpec {
    /// A name declared by [`ParticipantType::processes`].
    pub name: &'static str,
    /// The source of the process's first exit observation.
    pub exit: ExitChannel,
    /// The source of the process's output.
    pub output: OutputChannel,
}

/// The source of a process's exit status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitChannel {
    /// The container's exit, owned by at most one process per participant.
    Container,
    /// A supervisor appends one line per exit to a file in the participant's exit directory.
    ///
    /// Each line is `<code> <signal> <seconds>.<nanoseconds>`, followed by a newline. A normal exit
    /// has code 0–255 and signal 0; a signal death has code 256 and a positive signal number.
    /// The timestamp is Unix time on the container's clock, with 1–9 fractional digits.
    /// The first line records the process's first exit, even if the supervisor restarts it.
    Record {
        /// A plain file name: nonempty, other than `.` or `..`, and containing neither `/` nor `\`.
        file: &'static str,
    },
}

/// The source of a process's output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputChannel {
    /// The container's log, owned by at most one process per participant.
    Container,
    /// A file the process writes in the participant's log directory.
    File {
        /// A plain file name: nonempty, other than `.` or `..`, and containing neither `/` nor `\`.
        file: &'static str,
        /// The streams the file contains.
        stream: OutputStream,
    },
}

/// The stream a process's output came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputStream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
    /// Standard output and standard error combined by the producer.
    Merged,
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

    /// Artifact roles this type runs as one realization. Called only with realizations from [`Self::supports`].
    /// Every declaration of a role name, by any type or backend, must agree on its kind and local default.
    fn artifacts(&self, realization: Realization) -> &'static [ArtifactRole];

    /// Supplies the creation recipe, called in create order with a supported realization.
    /// Every artifact role named by the recipe must be declared by [`Self::artifacts`] for that realization.
    ///
    /// # Errors
    ///
    /// Returns an error if the type cannot supply a recipe for the rendered configuration.
    fn recipe(&self, realization: Realization, _config: &RenderedConfig) -> Result<Recipe, String> {
        Err(format!(
            "participant type '{}' has no recipe for {} on {}",
            self.name(),
            realization.kind.name(),
            realization.os.name(),
        ))
    }

    /// Names a case may supply under `config.files`. Defaults to none.
    fn config_files(&self) -> &'static [&'static str] {
        &[]
    }

    /// Names a case may set under `config.params`. Defaults to none.
    fn params(&self) -> &'static [&'static str] {
        &[]
    }

    /// Checks the case's configuration after its OS overlay. Defaults to accepting it.
    /// Called only with realizations from [`Self::supports`].
    ///
    /// `config` holds substituted values; see [`CaseValues`].
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
    ///
    /// `config` holds substituted values; see [`CaseValues`].
    fn default_env(&self, _realization: Realization, _config: &CaseValues) -> BTreeMap<String, String> {
        BTreeMap::new()
    }

    /// Endpoints this participant serves with its configuration, by name. Defaults to none.
    /// Called after [`Self::validate_config`], only with realizations from [`Self::supports`].
    ///
    /// `config` holds substituted values before connection deliveries; see [`EffectiveConfig`].
    /// Derive these endpoints independently of where the participant connects.
    ///
    /// On a host process group, report `Port::Assigned` for a listener whose configuration leaves its port unset.
    /// Other realizations must not report it. Core delivers assigned ports into type defaults, reapplies case
    /// values and calls this hook again with that reassembled configuration. Keep the same endpoint names and
    /// kinds, derive each assigned listener as `Port::Fixed` at its assigned port and resolve every `Port::Assigned`.
    ///
    /// # Errors
    ///
    /// Returns an error describing configuration from which the type cannot derive its endpoints.
    fn endpoints(
        &self, _realization: Realization, _config: &EffectiveConfig,
    ) -> Result<BTreeMap<String, Endpoint>, String> {
        Ok(BTreeMap::new())
    }

    /// Readiness evidence applying to every endpoint this participant serves. Defaults to none.
    /// Called after [`Self::validate_config`], only with realizations from [`Self::supports`].
    /// Receives the same configuration [`Self::endpoints`] last saw, after port assignment and before
    /// connection deliveries; see [`EffectiveConfig`].
    fn readiness(&self, _realization: Realization, _config: &EffectiveConfig) -> Vec<Evidence> {
        Vec::new()
    }

    /// Endpoints this participant consumes with its configuration, by name: the keys a case may `connect`.
    /// Defaults to none. Called after [`Self::validate_config`], only with realizations from [`Self::supports`].
    ///
    /// `config` holds substituted values before connection deliveries; see [`EffectiveConfig`].
    /// Derive these endpoints independently of where the participant connects.
    fn consumes(&self, _realization: Realization, _config: &EffectiveConfig) -> BTreeMap<String, ConsumedEndpoint> {
        BTreeMap::new()
    }

    /// Early facts configuration and steps may reference for this realization, sorted by name. Defaults to none.
    /// Called only with realizations from [`Self::supports`].
    /// Types must not declare [`SHARED_DIR_FACT`]; panoramic supplies it for every participant.
    fn facts(&self, _realization: Realization) -> &'static [FactDeclaration] {
        &[]
    }

    /// The participant's own late facts this type writes into its environment variables
    /// after the participant starts and before its product reads them.
    /// Sorted by fact; defaults to none. Called only with realizations from [`Self::supports`].
    fn substituted_late_facts(&self, _realization: Realization) -> &'static [LateFact] {
        &[]
    }

    /// Named processes steps may address with `process`, in sorted order.
    fn processes(&self) -> &'static [&'static str];

    /// Exit and output channels for this realization. Defaults to none.
    /// Native preparation requires exactly one specification for every name in [`Self::processes`].
    fn process_specs(&self, _realization: Realization) -> &'static [ProcessSpec] {
        &[]
    }
}

/// Checks exit and output channels in process declaration order, returning the validated specifications.
pub(crate) fn check_process_specs(
    participant: &str, participant_type: &dyn ParticipantType, realization: Realization,
) -> Result<&'static [ProcessSpec], String> {
    let prefix = format!(
        "participant '{participant}' of type '{}' as {} on {}",
        participant_type.name(),
        realization.kind.name(),
        realization.os.name(),
    );
    let processes = participant_type.processes();
    let specs = participant_type.process_specs(realization);
    let mut container_owner = None;
    let mut output_owner = None;
    let mut output_files: Vec<(&str, &str)> = Vec::new();
    let mut host_files: BTreeMap<String, Option<&str>> = BTreeMap::from([
        ("container.stdout.log".into(), None),
        ("container.stderr.log".into(), None),
    ]);
    let mut record_files: Vec<(&str, &str)> = Vec::new();
    for &process in processes {
        if matches!(process, "" | "." | "..") || process.contains(['/', '\\']) {
            return Err(format!(
                "{prefix} declares process '{process}'; process names must be plain file names"
            ));
        }
        let mut channels = specs.iter().filter(|spec| spec.name == process);
        let Some(spec) = channels.next() else {
            return Err(format!("{prefix} declares no exit channel for process '{process}'"));
        };
        if channels.next().is_some() {
            return Err(format!("{prefix} declares two exit channels for process '{process}'"));
        }
        match spec.exit {
            ExitChannel::Container => {
                if let Some(owner) = container_owner {
                    return Err(format!(
                        "{prefix} gives the container's exit to processes '{owner}' and '{process}'; \
                         only one process can own it"
                    ));
                }
                container_owner = Some(process);
            }
            ExitChannel::Record { file } => {
                if matches!(file, "" | "." | "..") || file.contains(['/', '\\']) {
                    return Err(format!(
                        "{prefix} declares exit record file '{file}' for process '{process}'; \
                         record files must be plain file names"
                    ));
                }
                if let Some((_, owner)) = record_files.iter().find(|(seen, _)| *seen == file) {
                    return Err(format!(
                        "{prefix} declares exit record file '{file}' for processes '{owner}' and '{process}'; \
                         each process needs its own record file"
                    ));
                }
                record_files.push((file, process));
            }
        }
        match spec.output {
            OutputChannel::Container => {
                if let Some(owner) = output_owner {
                    return Err(format!(
                        "{prefix} gives the container's output to processes '{owner}' and '{process}'; \
                         only one process can own it"
                    ));
                }
                output_owner = Some(process);
            }
            OutputChannel::File { file, stream } => {
                if matches!(file, "" | "." | "..") || file.contains(['/', '\\']) {
                    return Err(format!(
                        "{prefix} declares output file '{file}' for process '{process}'; \
                         output files must be plain file names"
                    ));
                }
                if let Some((_, owner)) = output_files.iter().find(|(seen, _)| *seen == file) {
                    return Err(format!(
                        "{prefix} declares output file '{file}' for processes '{owner}' and '{process}'; \
                         each process needs its own output file"
                    ));
                }
                let host_file = host_output_file(process, spec.output).expect("file output has a host file name");
                if let Some(owner) = host_files.get(&host_file) {
                    let owner = match owner {
                        Some(owner) => format!("output of process '{owner}'"),
                        None => "the container's own output".into(),
                    };
                    return Err(format!(
                        "{prefix} declares file output for process '{process}' with stream {stream:?}; \
                         its host file would replace {owner} at '{host_file}'"
                    ));
                }
                host_files.insert(host_file, Some(process));
                output_files.push((file, process));
            }
        }
    }
    if let Some(spec) = specs.iter().find(|spec| !processes.contains(&spec.name)) {
        return Err(format!(
            "{prefix} declares an exit channel for process '{}', which the type does not run",
            spec.name,
        ));
    }
    Ok(specs)
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
endpoints: {api: {port: 80}, extra: {port: 90}}
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
                    endpoints: config.endpoints.clone(),
                }
            );
        }
    }

    #[test]
    fn case_values_carry_the_base_endpoint_overrides_on_every_os() {
        let config: ParticipantConfig = serde_saphyr::from_str(CONFIG).unwrap();
        let expected = BTreeMap::from([
            ("api".into(), EndpointOverride { port: 80 }),
            ("extra".into(), EndpointOverride { port: 90 }),
        ]);
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            assert_eq!(CaseValues::for_os(&config, os).endpoints, expected);
        }
    }

    #[test]
    fn effective_config_carries_the_case_params_and_endpoint_overrides() {
        let config: ParticipantConfig = serde_saphyr::from_str(CONFIG).unwrap();
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            let values = CaseValues::for_os(&config, os);
            let effective = EffectiveConfig::assemble(BTreeMap::new(), values.clone());
            assert_eq!(effective.params, values.params);
            assert_eq!(effective.endpoints, values.endpoints);
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
