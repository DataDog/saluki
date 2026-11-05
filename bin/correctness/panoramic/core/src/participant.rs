//! Participant type declarations, effective configuration and realization-specific early facts.

use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

use async_trait::async_trait;
use serde_json::Value;

use crate::{
    artifact::ArtifactRole,
    backend::LateFact,
    case::{
        template::{Reference, Template},
        FileSource, ParticipantConfig,
    },
    endpoint::{ConsumedEndpoint, Endpoint, Evidence, RunnerHostRoute},
    handle::{http_get_from_runner_host, ExecCommand, ExecOutput, HandleError, HttpResponse, Primitives},
    layout::{Os, Realization, RealizationKind},
    observe::output::host_output_file,
    probe::AttemptLimits,
    recipe::Recipe,
};

/// One participant's case configuration for its participant OS, after applying that OS's overlay.
///
/// Contains only case values, before type defaults and connection deliveries.
///
/// In `env` and `params`, fact references and fixed late facts (a host process group's `ip`) are already substituted.
/// The participant's own late facts listed by [`ParticipantType::substituted_late_facts`] use the canonical
/// spelling from [`LateFact::substituted_reference`], which the type replaces after start.
/// Other references stay as written: another participant's `container_id` and endpoint and connection
/// addresses resolve at creation.
/// A type that must interpret an unresolved value reports a configuration error.
/// File contents substitute early facts only in sources marked `template: true`.
/// Address and late references stay as written until their values are available.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CaseValues {
    /// Environment variables after applying the overlay. Defaults to empty.
    pub env: BTreeMap<String, String>,
    /// File sources as written, relative to the case directory. Defaults to empty.
    /// Files marked `template: true` keep their references as written, checked for this OS.
    pub files: BTreeMap<String, FileSource>,
    /// File bytes after the OS overlay and substitution of early facts. Defaults to empty.
    /// Only `template: true` sources are interpolated; other files retain their bytes verbatim.
    /// Directory sources expand to one entry per regular file, keyed by `name/<relative path>`.
    pub file_contents: BTreeMap<String, Vec<u8>>,
    /// Type-specific parameters; overlay values replace whole base values by key. Defaults to empty.
    pub params: BTreeMap<String, serde_json::Value>,
    /// Base endpoint overrides, shared by every OS. Defaults to empty.
    pub endpoints: BTreeMap<String, u16>,
}

impl CaseValues {
    /// Applies the participant OS's overlay; each key replaces the whole base value.
    pub(crate) fn for_os(config: &ParticipantConfig, os: Os) -> Self {
        let mut values = Self {
            env: config.env.clone(),
            files: config.files.clone(),
            file_contents: BTreeMap::new(),
            params: config.params.clone(),
            endpoints: config
                .endpoints
                .iter()
                .map(|(name, endpoint)| (name.clone(), endpoint.port))
                .collect(),
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
/// The `validate_config`, `validate_realization_options`, `default_env`, `endpoints`, `readiness` and `consumes` hooks
/// see configuration before connection deliveries.
/// After port assignment, `endpoints`, `readiness` and `consumes` see the reassembled configuration,
/// including assigned values.
/// A type must derive what it serves and consumes independently of where it connects.
/// Case references follow the substitution and checking rules of [`CaseValues`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EffectiveConfig {
    /// Type defaults (including assigned ports), then connection deliveries, then case variables. Defaults to empty.
    /// Case values must not set a key delivered by a connection.
    pub env: BTreeMap<String, String>,
    /// Case file bytes, with directories expanded as in [`CaseValues::file_contents`]. Defaults to empty.
    /// Only `template: true` sources are interpolated. Hooks see early facts substituted;
    /// the completed plan also substitutes endpoint and connection addresses. Late references stay as written.
    /// Definitions must reject non-early references in values they read with [`require_early_config_value`].
    pub files: BTreeMap<String, Vec<u8>>,
    /// Case parameters after the OS overlay, interpreted by the participant type. Defaults to empty.
    pub params: BTreeMap<String, serde_json::Value>,
    /// Case-supplied ports for listeners whose port the type cannot derive. Defaults to empty.
    pub endpoints: BTreeMap<String, u16>,
}

impl EffectiveConfig {
    /// Applies case environment values over type defaults, carrying file contents, parameters and endpoint overrides.
    pub(crate) fn assemble(defaults: BTreeMap<String, String>, case: CaseValues) -> Self {
        let mut env = defaults;
        env.extend(case.env);
        Self {
            env,
            files: case.file_contents,
            params: case.params,
            endpoints: case.endpoints,
        }
    }
}

/// Checks a configuration value a definition needs while planning, after parsing its containing file.
/// Pass only the string being read, so unrelated references elsewhere in the file remain valid.
///
/// A definition reading the value in a hook sees only early facts substituted. Late, endpoint and connection
/// references resolve later and stay as written, so the value is acceptable only if it contains literal text
/// and fact references.
///
/// # Errors
///
/// Returns an error naming the file and key if the value is malformed or contains a late, endpoint or
/// connection reference. The error quotes the first such reference exactly as written.
pub fn require_early_config_value(file: &str, key: &str, value: &str) -> Result<(), String> {
    let field = format!("configuration file '{file}' key '{key}'");
    let template: Template = value.parse().map_err(|error| format!("{field}: {error}"))?;
    for (written, reference) in template.written_references() {
        if !matches!(reference, Reference::Fact { .. }) {
            return Err(format!(
                "{field} contains reference '{written}'; only early facts are substituted before planning"
            ));
        }
    }
    Ok(())
}

/// Configuration rendered for participant creation, with case files keyed by configuration name.
pub struct RenderedConfig {
    pub(crate) directory: String,
    pub(crate) env: BTreeMap<String, String>,
    pub(crate) params: BTreeMap<String, serde_json::Value>,
    pub(crate) files: BTreeMap<String, Vec<u8>>,
    /// Config names whose pod templates retain deferred late references.
    pub(crate) late_files: BTreeSet<String>,
}

impl RenderedConfig {
    /// Returns whether rendering left late references in this template file, to be written after startup.
    pub fn file_needs_late_write(&self, name: &str) -> bool {
        self.late_files.contains(name)
    }

    /// The participant's directory under its scope's shared root: an absolute path in the
    /// participant OS's style. On a host process group, its processes run in it and recipe files
    /// are placed relative to it.
    pub fn directory(&self) -> &str {
        &self.directory
    }

    /// Environment after resolving references and connection deliveries for creation.
    pub fn env(&self) -> &BTreeMap<String, String> {
        &self.env
    }

    /// Parameters after resolving references for creation.
    pub fn params(&self) -> &BTreeMap<String, Value> {
        &self.params
    }

    /// Case file contents in key order. A regular file uses its declared name as the key.
    /// A directory named `d/` contributes its regular files recursively as `d/<relative path>`,
    /// with `/` separators. Empty directories contribute nothing.
    pub fn files(&self) -> &BTreeMap<String, Vec<u8>> {
        &self.files
    }
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
    /// The wait result for a process spawned in a host process group.
    Wait,
    /// The container's exit, owned by at most one process per participant.
    Container,
    /// A supervisor appends one line per exit to a file in the participant's exit directory.
    /// In Linux containers, `PANORAMIC_EXIT_DIR` gives that directory's absolute path.
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
    /// Separate stdout and stderr pipes of a process spawned in a host process group.
    Pipes,
    /// The container's log, owned by at most one process per participant.
    Container,
    /// A file the process writes in the participant's log directory.
    /// In Linux containers, `PANORAMIC_LOG_DIR` gives that directory's absolute path.
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

/// A named operation implemented by a participant type for one realization.
#[async_trait]
pub trait Operation: Send + Sync {
    /// Name steps call, unique within the type's operations for a realization.
    fn name(&self) -> &'static str;

    /// The participant's own endpoints this operation requests over HTTP from the runner host. Defaults to none.
    fn http_endpoints(&self) -> &'static [&'static str] {
        &[]
    }

    /// Runs with the participant's primitives and the caller's time limit.
    ///
    /// # Errors
    ///
    /// Argument and mechanism problems are harness errors; unsuccessful product answers are product errors.
    async fn run(&self, cx: &OpContext<'_>, args: Value) -> Result<Value, HandleError>;
}

/// The participant's execution primitive, declared HTTP routes and a step-supplied limit.
pub struct OpContext<'a> {
    pub(crate) primitives: &'a dyn Primitives,
    pub(crate) limit: Duration,
    pub(crate) requests: BTreeMap<&'static str, RunnerHostRoute>,
}

impl OpContext<'_> {
    /// Runs a command with the operation's limit, inheriting the participant's environment.
    ///
    /// # Errors
    ///
    /// Returns a harness error if execution fails or exceeds its limit.
    pub async fn exec(&self, argv: Vec<String>) -> Result<ExecOutput, HandleError> {
        self.primitives
            .exec(
                ExecCommand {
                    argv,
                    env: Vec::new(),
                    stdin: None,
                },
                self.limit,
            )
            .await
    }

    /// Runs one of the participant's declared processes' programs with `args` in place of its declared
    /// arguments, in that process's environment and the participant's directory. Realizations whose
    /// programs are not runner-host files return a harness error.
    ///
    /// # Errors
    ///
    /// Returns a harness error for an unknown process, unsupported realization, execution failure or timeout.
    pub async fn exec_process(&self, process: &str, args: Vec<String>) -> Result<ExecOutput, HandleError> {
        self.primitives.exec_process(process, args, self.limit).await
    }

    /// Gets an HTTP response from a declared endpoint, using the operation's limit for the whole request.
    ///
    /// # Errors
    ///
    /// Missing request routes, invalid requests and unsupported routes are harness errors.
    /// Connection failures, incomplete bodies and timeouts are product errors.
    pub async fn http_get(&self, endpoint: &str, path: &str) -> Result<HttpResponse, HandleError> {
        let route = self.requests.get(endpoint).ok_or_else(|| HandleError::Harness {
            message: format!(
                "no HTTP request route for endpoint '{endpoint}'; the operation must declare it in http_endpoints and the step must declare an operation need"
            ),
        })?;
        let limits = AttemptLimits {
            attempt: self.limit,
            ..AttemptLimits::new(self.limit)
        };
        http_get_from_runner_host(route, self.primitives, path, &limits).await
    }
}

/// Default graceful-stop allowance, matching the ten-second Docker stop timeout.
const DEFAULT_STOP_GRACE: Duration = Duration::from_secs(10);

/// Where a participant file is retained in the case artifacts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CollectDestination {
    /// The participant's `collected/` directory.
    Participant,
    /// The case's shared `traffic/` directory.
    Traffic,
}

/// A regular file copied after the procedure, while the participant still exists.
#[derive(Clone, Copy, Debug)]
pub struct CollectFile {
    /// Absolute path inside the participant.
    pub path: &'static str,
    /// Plain filename used in the destination directory.
    pub name: &'static str,
    /// Definition-owned artifact kind reported with the collected path.
    pub kind: &'static str,
    /// Destination relative to the case's artifact directory.
    pub destination: CollectDestination,
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

    /// Time allowed for graceful shutdown on this realization. Defaults to ten seconds.
    fn stop_grace(&self, _realization: Realization) -> Duration {
        DEFAULT_STOP_GRACE
    }

    /// Supplies the creation recipe, called in create order with a supported realization.
    /// Every artifact role named by the recipe must be declared by [`Self::artifacts`] for that realization.
    /// Linux containers receive reserved environment variables: `PANORAMIC_PARTICIPANT` names the participant,
    /// `PANORAMIC_EXIT_DIR` is its exit-record directory and `PANORAMIC_LOG_DIR` is its process-log directory.
    /// Recipes must not set these variables.
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

    /// Files to copy after the procedure from every started participant, running or exited.
    /// Each failed copy is reported as a collection issue and preserves the procedure verdict.
    fn collect(&self, _realization: Realization) -> &'static [CollectFile] {
        &[]
    }

    /// Names a case may supply under `config.files`. Defaults to none.
    /// A name ending in `/` requires a directory source; every other name requires a regular file.
    /// Directory sources cannot use `template: true`.
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

    /// Checks whether backend-validated realization options support the validated case configuration.
    /// Defaults to accepting them. Called after [`Self::validate_config`].
    ///
    /// # Errors
    ///
    /// Returns an error when the configuration requires different realization options.
    fn validate_realization_options(
        &self, _realization: Realization, _options: &BTreeMap<String, serde_json::Value>, _config: &CaseValues,
    ) -> Result<(), String> {
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
    /// Type hooks receive these placeholders in [`LateFact::substituted_reference`] spelling.
    /// Sorted by fact; defaults to none. Called only with realizations from [`Self::supports`].
    fn substituted_late_facts(&self, _realization: Realization) -> &'static [LateFact] {
        &[]
    }

    /// Named operations available on this realization. Defaults to none.
    fn operations(&self, _realization: Realization) -> &'static [&'static dyn Operation] {
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

/// Checks that process names can be used as output file names.
pub(crate) fn check_process_names(prefix: &str, processes: &[&str]) -> Result<(), String> {
    for &process in processes {
        if matches!(process, "" | "." | "..") || process.contains(['/', '\\']) {
            return Err(format!(
                "{prefix} declares process '{process}'; process names must be plain file names"
            ));
        }
    }
    Ok(())
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
    check_process_names(&prefix, processes)?;
    let specs = participant_type.process_specs(realization);
    let mut container_owner = None;
    let mut output_owner = None;
    let mut output_files: Vec<(&str, &str)> = Vec::new();
    let host = realization.kind == RealizationKind::HostProcessGroup;
    let mut host_files: BTreeMap<String, Option<&str>> = if host {
        BTreeMap::new()
    } else {
        BTreeMap::from([
            ("container.stdout.log".into(), None),
            ("container.stderr.log".into(), None),
        ])
    };
    let mut record_files: Vec<(&str, &str)> = Vec::new();
    for &process in processes {
        let mut channels = specs.iter().filter(|spec| spec.name == process);
        let Some(spec) = channels.next() else {
            return Err(format!("{prefix} declares no exit channel for process '{process}'"));
        };
        if channels.next().is_some() {
            return Err(format!("{prefix} declares two exit channels for process '{process}'"));
        }
        let invalid_exit = match spec.exit {
            ExitChannel::Wait if !host => Some("Wait"),
            ExitChannel::Container if host => Some("Container"),
            ExitChannel::Record { .. } if host => Some("Record"),
            _ => None,
        };
        if let Some(channel) = invalid_exit {
            return Err(format!(
                "{prefix} declares exit channel {channel} for process '{process}'; \
                this channel is not supported on {}",
                realization.kind.name()
            ));
        }
        let invalid_output = match spec.output {
            OutputChannel::Pipes if !host => Some("Pipes"),
            OutputChannel::Container if host => Some("Container"),
            OutputChannel::File { .. } if host => Some("File"),
            _ => None,
        };
        if let Some(channel) = invalid_output {
            return Err(format!(
                "{prefix} declares output channel {channel} for process '{process}'; \
                this channel is not supported on {}",
                realization.kind.name()
            ));
        }
        match spec.exit {
            ExitChannel::Wait => {}
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
        let host_paths = match spec.output {
            OutputChannel::Pipes => Vec::new(),
            OutputChannel::Container => {
                if let Some(owner) = output_owner {
                    return Err(format!(
                        "{prefix} gives the container's output to processes '{owner}' and '{process}'; \
                         only one process can own it"
                    ));
                }
                output_owner = Some(process);
                Vec::new()
            }
            OutputChannel::File { file, .. } => {
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
                output_files.push((file, process));
                vec![host_output_file(process, spec.output).expect("file output has a host file name")]
            }
        };
        for host_file in host_paths {
            if let Some(owner) = host_files.get(&host_file) {
                let owner = match owner {
                    Some(owner) => format!("output of process '{owner}'"),
                    None => "the container's own output".into(),
                };
                let OutputChannel::File { stream, .. } = spec.output else {
                    unreachable!("only file output needs a host-file collision check");
                };
                let channel = format!("file output for process '{process}' with stream {stream:?}");
                return Err(format!(
                    "{prefix} declares {channel}; its host file would replace {owner} at '{host_file}'"
                ));
            }
            host_files.insert(host_file, Some(process));
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

#[cfg(any(test, feature = "test-util"))]
pub mod test_support;

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn an_early_file_value_check_accepts_only_literals_and_early_facts() {
        for value in [
            "8123",
            "/run/input.sock",
            "{{self.facts.shared_dir}}",
            "{{ peer.facts.port }}:{{self.facts.shared_dir}}/input.sock",
        ] {
            assert_eq!(
                require_early_config_value("settings.yaml", "listen.socket", value),
                Ok(())
            );
        }
        for written in [
            "{{  peer.late.container_id  }}",
            "{{self.late.ip}}",
            "{{ self.endpoints.api.url }}",
            "{{peer.endpoints.api.host}}",
            "{{ self.connections.upstream.url }}",
            "{{self.connections.upstream.port}}",
        ] {
            assert_eq!(
                require_early_config_value(
                    "settings.yaml",
                    "listen.socket",
                    &format!("{{{{self.facts.shared_dir}}}}/{written}"),
                ),
                Err(format!("configuration file 'settings.yaml' key 'listen.socket' contains reference '{written}'; only early facts are substituted before planning")),
            );
        }
        assert_eq!(
            require_early_config_value("settings.yaml", "listen.socket", "{{ unclosed"),
            Err("configuration file 'settings.yaml' key 'listen.socket': unterminated template reference '{{ unclosed': expected '}}'".into()),
        );
    }

    #[tokio::test]
    async fn an_operation_http_request_uses_its_full_limit_instead_of_the_probe_limit() {
        use tokio::{
            io::AsyncReadExt,
            net::TcpListener,
            time::{advance, pause, resume, timeout},
        };

        use crate::{
            endpoint::{EndpointKind, ResolvedAddress},
            probe::PROBE_TIMEOUT,
            step::test_support::FakeParticipant,
        };

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let request = tokio::spawn(async move {
            let primitives = FakeParticipant::default();
            let cx = OpContext {
                primitives: &primitives,
                limit: PROBE_TIMEOUT * 2,
                requests: BTreeMap::from([(
                    "api",
                    RunnerHostRoute::Direct(ResolvedAddress {
                        kind: EndpointKind::Http,
                        url: format!("http://127.0.0.1:{port}"),
                        host: Some("127.0.0.1".into()),
                        port: Some(port),
                        path: None,
                    }),
                )]),
            };
            cx.http_get("api", "/dump").await
        });
        let mut stream = timeout(Duration::from_secs(5), async {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            while !bytes.ends_with(b"\r\n\r\n") {
                bytes.push(stream.read_u8().await.unwrap());
            }
            stream
        })
        .await
        .unwrap();
        pause();
        advance(PROBE_TIMEOUT + Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert!(!request.is_finished(), "HTTP operations use the caller's limit");
        advance(PROBE_TIMEOUT).await;
        assert_eq!(
            request.await.unwrap(),
            Err(HandleError::Product {
                message: "HTTP request timed out after 20s".into(),
            })
        );
        resume();
        assert_eq!(
            timeout(Duration::from_secs(5), stream.read(&mut [0]))
                .await
                .unwrap()
                .unwrap(),
            0
        );
    }

    struct Channels {
        names: &'static [&'static str],
        specs: &'static [ProcessSpec],
    }

    impl ParticipantType for Channels {
        fn name(&self) -> &'static str {
            "channels"
        }

        fn artifacts(&self, _: Realization) -> &'static [ArtifactRole] {
            &[]
        }

        fn processes(&self) -> &'static [&'static str] {
            self.names
        }

        fn process_specs(&self, _: Realization) -> &'static [ProcessSpec] {
            self.specs
        }
    }

    const HOST_PROCESS: ProcessSpec = ProcessSpec {
        name: "main",
        exit: ExitChannel::Wait,
        output: OutputChannel::Pipes,
    };

    #[test]
    fn host_process_groups_accept_waits_and_separate_pipes_for_each_named_process() {
        let participant_type = Channels {
            names: &["main", "worker"],
            specs: &[
                HOST_PROCESS,
                ProcessSpec {
                    name: "worker",
                    ..HOST_PROCESS
                },
            ],
        };
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            assert_eq!(
                check_process_specs(
                    "p",
                    &participant_type,
                    Realization {
                        os,
                        kind: RealizationKind::HostProcessGroup,
                    }
                )
                .unwrap(),
                participant_type.specs,
            );
        }
    }

    #[test]
    fn host_channels_are_rejected_on_every_other_realization_with_the_channel_and_realization() {
        for (os, kind) in [
            (Os::Linux, RealizationKind::LinuxContainer),
            (Os::Windows, RealizationKind::WindowsContainer),
            (Os::Linux, RealizationKind::KubernetesPod),
        ] {
            for (specs, channel, name) in [
                (&[HOST_PROCESS][..], "exit", "Wait"),
                (
                    &[ProcessSpec {
                        exit: ExitChannel::Container,
                        ..HOST_PROCESS
                    }][..],
                    "output",
                    "Pipes",
                ),
            ] {
                let error = check_process_specs(
                    "p",
                    &Channels {
                        names: &["main"],
                        specs,
                    },
                    Realization { os, kind },
                )
                .unwrap_err();
                assert_eq!(error, format!(
                    "participant 'p' of type 'channels' as {} on {} declares {channel} channel {name} for process 'main'; \
                     this channel is not supported on {}", kind.name(), os.name(), kind.name(),
                ));
            }
        }
    }

    #[test]
    fn host_process_groups_reject_container_record_and_file_channels() {
        for (specs, channel, name) in [
            (
                &[ProcessSpec {
                    exit: ExitChannel::Container,
                    ..HOST_PROCESS
                }][..],
                "exit",
                "Container",
            ),
            (
                &[ProcessSpec {
                    exit: ExitChannel::Record { file: "main" },
                    ..HOST_PROCESS
                }][..],
                "exit",
                "Record",
            ),
            (
                &[ProcessSpec {
                    output: OutputChannel::Container,
                    ..HOST_PROCESS
                }][..],
                "output",
                "Container",
            ),
            (
                &[ProcessSpec {
                    output: OutputChannel::File {
                        file: "main",
                        stream: OutputStream::Merged,
                    },
                    ..HOST_PROCESS
                }][..],
                "output",
                "File",
            ),
        ] {
            let error = check_process_specs(
                "p",
                &Channels {
                    names: &["main"],
                    specs,
                },
                Realization {
                    os: Os::MacOs,
                    kind: RealizationKind::HostProcessGroup,
                },
            )
            .unwrap_err();
            assert_eq!(error, format!(
                "participant 'p' of type 'channels' as host-process-group on macos declares {channel} channel {name} for process 'main'; \
                 this channel is not supported on host-process-group",
            ));
        }
    }

    #[test]
    fn a_host_process_may_use_the_name_container_for_its_own_pipe_files() {
        let participant_type = Channels {
            names: &["container"],
            specs: &[ProcessSpec {
                name: "container",
                ..HOST_PROCESS
            }],
        };
        assert_eq!(
            check_process_specs(
                "p",
                &participant_type,
                Realization {
                    os: Os::MacOs,
                    kind: RealizationKind::HostProcessGroup
                }
            )
            .unwrap(),
            participant_type.specs
        );
    }

    #[test]
    fn the_default_stop_grace_is_ten_seconds_on_every_supported_realization() {
        use crate::case::fixtures::Server;
        for &(os, kind) in Server.supports() {
            assert_eq!(Server.stop_grace(Realization { os, kind }), Duration::from_secs(10));
        }
    }

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
                    file_contents: BTreeMap::new(),
                    params: config.params.clone(),
                    endpoints: BTreeMap::from([("api".into(), 80), ("extra".into(), 90)]),
                }
            );
        }
    }

    #[test]
    fn case_values_carry_the_base_endpoint_overrides_on_every_os() {
        let config: ParticipantConfig = serde_saphyr::from_str(CONFIG).unwrap();
        let expected = BTreeMap::from([("api".into(), 80), ("extra".into(), 90)]);
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
        assert!(effective.files.is_empty());
    }
}
