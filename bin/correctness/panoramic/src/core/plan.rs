//! Planning: turning a loaded case and one subject OS into everything the case will do.
//!
//! Planning runs for every OS in a case's `subject_os`, on any host, and reads nothing but the loaded case. For each
//! participant it lays out a realization, then builds the spec in five steps:
//!
//! 1. Take the case layer: the case's fields for this OS.
//! 2. Call the participant type's `spec`, which may read the case layer. A participant with no participant type
//!    starts from an empty spec, so it has no code of its own.
//! 3. Rewrite each file name in the case layer to its destination, normalized for the realization.
//! 4. Check the guards: two changes a case could make that would otherwise fail silently.
//! 5. Lay the case layer over the participant type's spec.
//!
//! The merged spec is then shaped for its realization: the launch shape, the file destinations, the processes steps may
//! name, the environment each declared connection delivers, and each endpoint's address and readiness evidence. Then,
//! across participants: each connection gets its route and its address as the consumer reaches it, every reference is
//! checked against its place and filled in where planning knows its value, and the create and start orders are
//! computed as levels, with what each consumer waits for. Last, each step of the procedure is built, and what it uses
//! and what its references name are checked.
//!
//! The plan owns everything the lifecycle acts on: the contents of every file and the operations steps call. After
//! planning, nothing reads the loaded case or calls a participant type, except an operation a step calls.
//!
//! Errors are collected, not returned at the first. Their order is not a contract.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::Serialize;
use serde_json::Value;

use super::case::{is_name, CaseError, CaseInfo, LoadedCase, LoadedParticipant, Source, NAME_RULE};
use super::participant::{Enclosure, Os, Realization, SpecContext};
use super::reference::Later;
use super::registry::Registry;
use super::spec::{
    is_file_name, CollectFile, CommandSpec, Consumed, Contents, Evidence, ExitChannel, FileSource, HostMount,
    OutputChannel, ParticipantSpec, ProcessSpec, Program, DEFAULT_PROCESS,
};
use super::step::{EndpointOperation, Need, Operation, ProcedureNode, Step, StepFields, StepPurpose, Uses};
use super::template::Template;
use super::wiring::{Gate, PlannedConnection, PlannedEndpoint, Reach, TypeWiring, UnwiredUses};
use super::{order, reference, wiring};

/// Environment keys with this prefix are the harness's own.
const RESERVED_ENV_PREFIX: &str = "PANORAMIC_";

/// Everything one case will do on one subject OS.
#[derive(Debug, Serialize)]
pub(crate) struct CasePlan {
    pub(crate) case: CaseInfo,
    pub(crate) subject_os: Os,
    /// What each realization backend the case uses makes for it.
    pub(crate) scope: BTreeMap<Enclosure, &'static str>,
    pub(crate) participants: BTreeMap<String, PlannedParticipant>,
    /// Levels of participants, each created after the participants whose container IDs it names.
    pub(crate) create_order: Vec<Vec<String>>,
    /// Levels of participants, each started after the participants it connects to.
    pub(crate) start_order: Vec<Vec<String>>,
    /// What each consumer waits for before it starts.
    pub(crate) gates: BTreeMap<String, Vec<Gate>>,
    pub(crate) procedure: Vec<ProcedureNode<PlannedStep>>,
}

/// A participant's merged spec, checked and made concrete for its realization.
#[derive(Debug, Serialize)]
pub(crate) struct PlannedParticipant {
    pub(crate) participant_type: Option<String>,
    pub(crate) subject: bool,
    pub(crate) realization: Realization,
    pub(crate) options: BTreeMap<String, bool>,
    pub(crate) launch: Launch,
    pub(crate) env: BTreeMap<String, Template>,
    /// Every placed file, by destination, with a directory source expanded to its files.
    pub(crate) files: Vec<PlannedFile>,
    pub(crate) mounts: Vec<HostMount>,
    pub(crate) endpoints: BTreeMap<String, PlannedEndpoint>,
    pub(crate) connections: BTreeMap<String, PlannedConnection>,
    pub(crate) processes: Vec<ProcessSpec>,
    pub(crate) commands: BTreeMap<String, CommandSpec>,
    pub(crate) collect: Vec<CollectFile>,
    /// What steps call, by name. Each serializes as what it uses.
    #[serde(serialize_with = "operations")]
    pub(crate) operations: BTreeMap<&'static str, &'static dyn Operation>,
    /// Templates that hold a reference planning cannot fill, by place, with each participant they wait for and what it
    /// must have done.
    pub(crate) later: BTreeMap<String, BTreeMap<String, Later>>,
}

/// What runs.
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Launch {
    /// An image, with the command and arguments that replace its own when set.
    Image {
        image: Replaceable,
        command: Option<Vec<Template>>,
        args: Option<Vec<Template>>,
    },
    /// Host programs, in start order.
    Programs(Vec<Program>),
}

#[derive(Debug, Serialize)]
pub(crate) struct PlannedFile {
    pub(crate) destination: String,
    /// The case file the contents come from, or the participant type's label for them. Only people read it.
    pub(crate) source: String,
    /// `None` on Windows, which has no file modes.
    #[serde(serialize_with = "octal")]
    pub(crate) mode: Option<u32>,
    /// The name the case wrote for the destination, if it wrote one.
    pub(crate) name: Option<String>,
    pub(crate) contents: Contents,
}

/// A reference to an image as written, and the reference the run uses in its place when it replaces it.
///
/// Serializes as the reference as written, or, when the run replaces it, as `{"written": ..., "replaced": ...}`.
#[derive(Clone, Debug)]
pub(crate) struct Replaceable {
    pub(crate) written: String,
    pub(crate) replaced: Option<String>,
}

impl Replaceable {
    /// `written`, with the replacement `set` holds for it, if any.
    fn new(written: String, set: &BTreeMap<String, String>) -> Self {
        let replaced = set.get(&written).cloned();
        Self { written, replaced }
    }

    /// The reference the run uses.
    pub(crate) fn used(&self) -> &str {
        self.replaced.as_deref().unwrap_or(&self.written)
    }
}

impl fmt::Display for Replaceable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.replaced {
            Some(replaced) => write!(f, "{} -> {}", self.written, replaced),
            None => f.write_str(&self.written),
        }
    }
}

impl Serialize for Replaceable {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        #[derive(Serialize)]
        struct Replaced<'a> {
            written: &'a str,
            replaced: &'a str,
        }

        match &self.replaced {
            Some(replaced) => Replaced {
                written: &self.written,
                replaced,
            }
            .serialize(serializer),
            None => serializer.serialize_str(&self.written),
        }
    }
}

/// A step built for this plan.
#[derive(Debug, Serialize)]
pub(crate) struct PlannedStep {
    pub(crate) name: String,
    pub(crate) fields: StepFields,
    /// Everything of the case the step uses.
    pub(crate) needs: Vec<Need>,
    /// How the runner reaches each endpoint the step uses, itself or through an operation, keyed by
    /// `<participant>.<endpoint>`.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) reach: BTreeMap<String, Reach>,
    #[serde(skip)]
    pub(crate) step: Box<dyn Step>,
}

/// Plans `case` for every OS in its `subject_os`. `set` holds the run's replacements: each image written as a key runs
/// the image it maps to.
///
/// # Errors
///
/// Returns every problem found, on every OS.
pub(crate) fn plan_all(
    case: &LoadedCase, registry: &Registry, set: &BTreeMap<String, String>,
) -> Result<BTreeMap<Os, CasePlan>, Vec<CaseError>> {
    let mut plans = BTreeMap::new();
    let mut errors = Vec::new();
    for &os in &case.subject_os {
        match plan(case, os, registry, set) {
            Ok(plan) => {
                plans.insert(os, plan);
            }
            Err(mut found) => errors.append(&mut found),
        }
    }
    if errors.is_empty() {
        Ok(plans)
    } else {
        Err(errors)
    }
}

/// Plans `case` for one subject OS, with the run's replacements `set`.
///
/// # Errors
///
/// Returns every problem found.
pub(crate) fn plan(
    case: &LoadedCase, os: Os, registry: &Registry, set: &BTreeMap<String, String>,
) -> Result<CasePlan, Vec<CaseError>> {
    let mut problems = Problems {
        case,
        os,
        errors: Vec::new(),
    };
    let realizations = lay_out(case, os, &mut problems);
    let scope = realizations
        .values()
        .map(|realization| (realization.enclosure, realization.enclosure.facts().per_case))
        .collect();
    let connected: BTreeSet<(&str, &str)> = case
        .participants
        .values()
        .flat_map(|participant| participant.placement.connect.values())
        .map(|target| (target.participant.as_str(), target.endpoint.as_str()))
        .collect();

    let mut participants = BTreeMap::new();
    // By participant, the connection names its participant type knows and what each delivers. Only planning reads it.
    let mut consumes = BTreeMap::new();
    for (name, participant) in &case.participants {
        let Some(&realization) = realizations.get(name) else {
            continue;
        };
        let mut at = problems.at(format!("participant '{}'", name));
        let mut planner = Planner {
            name,
            participant,
            realization,
            case,
            set,
            at: &mut at,
        };
        if let Some((planned, known)) = planner.plan(registry, &connected) {
            participants.insert(name.clone(), planned);
            consumes.insert(name.clone(), known);
        }
    }

    // Each phase across participants reads what the phase before it planned, so it runs only if that phase found no
    // problem. Otherwise it would report what is missing as a mistake of its own.
    let planned_cleanly = problems.errors.is_empty();
    let mut unwired = UnwiredUses::new();
    if planned_cleanly {
        unwired = wiring::plan_connections(case, &mut participants, &consumes, &mut problems);
    }
    let mut create_edges = BTreeSet::new();
    if problems.errors.is_empty() {
        create_edges = reference::fill_references(&mut participants, &mut problems);
    }
    let start_edges = participants
        .iter()
        .flat_map(|(consumer, participant)| {
            participant
                .connections
                .values()
                .map(|connection| (connection.target.participant.clone(), consumer.clone()))
        })
        .collect();
    let mut order = |which: &str, edges| {
        order::levels(participants.keys().cloned(), edges).unwrap_or_else(|chain| {
            problems.push(None, format!("the {} order has a cycle: {}", which, chain));
            Vec::new()
        })
    };
    let create_order = order("create", &create_edges);
    let start_order = order("start", &start_edges);
    let gates = wiring::gates(&participants);

    let checked = planned_cleanly.then_some(&participants);
    let procedure = build_procedure(case, registry, checked, &mut unwired, &mut problems);
    wiring::report_unwired(&participants, unwired, &mut problems);
    if !problems.errors.is_empty() {
        return Err(problems.errors);
    }
    Ok(CasePlan {
        case: case.info.clone(),
        subject_os: os,
        scope,
        participants,
        create_order,
        start_order,
        gates,
        procedure,
    })
}

/// Gives every participant a realization: the subject gets the enclosure it requests or its OS's default, and every
/// other participant the enclosure it requests or the subject's.
fn lay_out(case: &LoadedCase, os: Os, problems: &mut Problems<'_>) -> BTreeMap<String, Realization> {
    let requested = |participant: &LoadedParticipant| {
        participant
            .placement
            .realization
            .as_ref()
            .map(|request| request.enclosure)
    };
    let Some((subject_name, subject)) = case.participants.iter().find(|(_, p)| p.placement.subject) else {
        return BTreeMap::new();
    };
    let subject_enclosure = requested(subject).unwrap_or_else(|| Enclosure::default_for(os));
    if let Some(runs) = subject_enclosure.os().filter(|&runs| runs != os) {
        problems.push(
            Some(format!("participant '{}'", subject_name)),
            format!(
                "the subject's realization {} runs {}, not the subject OS {}",
                subject_enclosure, runs, os
            ),
        );
    }
    case.participants
        .iter()
        .map(|(name, participant)| {
            let enclosure = requested(participant).unwrap_or(subject_enclosure);
            let realization = Realization {
                os: enclosure.os().unwrap_or(os),
                enclosure,
            };
            (name.clone(), realization)
        })
        .collect()
}

/// Builds each step of the procedure. Once the participants have planned without a problem, it also checks what each
/// step needs and the references in its template fields against `participants`, and adds each endpoint a step uses
/// and its participant does not wire to `unwired`.
fn build_procedure(
    case: &LoadedCase, registry: &Registry, participants: Option<&BTreeMap<String, PlannedParticipant>>,
    unwired: &mut UnwiredUses, problems: &mut Problems<'_>,
) -> Vec<ProcedureNode<PlannedStep>> {
    let values = participants.map(reference::Values::of_plan);
    let mut procedure = Vec::new();
    for (index, node) in case.procedure.iter().enumerate() {
        let parallel = matches!(node, ProcedureNode::Parallel(_));
        let mut steps = Vec::new();
        for (member, entry) in node.steps().iter().enumerate() {
            let at = if parallel {
                format!("step {}.{}", index + 1, member + 1)
            } else {
                format!("step {}", index + 1)
            };
            let Some(factory) = registry.step(&entry.step) else {
                problems.push(Some(at), format!("unknown step '{}'", entry.step));
                continue;
            };
            if parallel && factory.purpose() == StepPurpose::Action {
                problems.push(
                    Some(at),
                    format!("'{}' is an action, and only assertions run in parallel", entry.step),
                );
                continue;
            }
            let template_fields = factory.template_fields();
            for (field, value) in &entry.fields.0 {
                let holds_reference = strings(value).iter().any(|text| text.contains("{{"));
                if holds_reference && !template_fields.contains(&field.as_str()) {
                    problems.push(
                        Some(at.clone()),
                        format!(
                            "{}: '{}' holds no references, so it cannot contain '{{{{'",
                            entry.step, field
                        ),
                    );
                }
            }
            let step = match factory.build(&entry.fields) {
                Ok(step) => step,
                Err(message) => {
                    problems.push(Some(at), format!("{}: {}", entry.step, message));
                    continue;
                }
            };
            let mut reach = BTreeMap::new();
            if let (Some(participants), Some(values)) = (participants, &values) {
                let mut found = check_needs(step.as_ref(), &at, participants, unwired, &mut reach);
                for field in template_fields {
                    for text in entry.fields.0.get(*field).map(strings).unwrap_or_default() {
                        match Template::parse(text) {
                            Ok(template) => found.extend(
                                reference::check_step_template(values, &template)
                                    .into_iter()
                                    .map(|message| format!("{}: {}", field, message)),
                            ),
                            Err(message) => found.push(format!("{}: {}", field, message)),
                        }
                    }
                }
                for message in found {
                    problems.push(Some(at.clone()), format!("{}: {}", entry.step, message));
                }
            }
            steps.push(PlannedStep {
                name: entry.step.clone(),
                fields: entry.fields.clone(),
                needs: step.needs(),
                reach,
                step,
            });
        }
        procedure.push(match node {
            ProcedureNode::Step(_) => match steps.pop() {
                Some(step) => ProcedureNode::Step(step),
                None => continue,
            },
            ProcedureNode::Parallel(_) => ProcedureNode::Parallel(steps),
        });
    }
    procedure
}

/// Checks that everything `step` needs exists in `participants`, with what each operation it calls uses, records how
/// the runner reaches each endpoint in `reach`, and adds each endpoint it uses that its participant does not wire to
/// `unwired`, as used by the step at `at`.
///
/// Returns the problems found.
fn check_needs(
    step: &dyn Step, at: &str, participants: &BTreeMap<String, PlannedParticipant>, unwired: &mut UnwiredUses,
    reach: &mut BTreeMap<String, Reach>,
) -> Vec<String> {
    let mut problems = Vec::new();
    for need in step.needs() {
        let name = match &need {
            Need::Participant(name)
            | Need::Process(name, _)
            | Need::Command(name, _)
            | Need::Operation(name, _)
            | Need::Endpoint(name, _, _) => name,
        };
        let Some(participant) = participants.get(name) else {
            problems.push(format!("names participant '{}', which the case does not have", name));
            continue;
        };
        let mut endpoint = |endpoint: &str, operation, used_by: String| {
            let Some(planned) = participant.endpoints.get(endpoint) else {
                return Some(format!("participant '{}' has no endpoint '{}'", name, endpoint));
            };
            if planned.unwired.is_some() {
                unwired
                    .entry((name.clone(), endpoint.to_string()))
                    .or_default()
                    .push(used_by);
            }
            match wiring::reach(
                operation,
                planned.protocol,
                planned.tls,
                participant.realization.enclosure,
            ) {
                Ok(route) => {
                    reach.insert(format!("{}.{}", name, endpoint), route);
                    None
                }
                Err(why) => Some(format!("endpoint '{}' of participant '{}': {}", endpoint, name, why)),
            }
        };
        match &need {
            Need::Participant(_) => {}
            Need::Process(_, process) => {
                if !participant.processes.iter().any(|spec| spec.name() == process) {
                    let processes: Vec<&str> = participant.processes.iter().map(ProcessSpec::name).collect();
                    problems.push(format!(
                        "participant '{}' has no process '{}': it has {}",
                        name,
                        process,
                        super::case::join(&processes)
                    ));
                }
            }
            Need::Command(_, command) => {
                if !participant.commands.contains_key(command) {
                    problems.push(format!("participant '{}' has no command '{}'", name, command));
                }
            }
            Need::Operation(_, operation) => match participant.operations.get(operation.as_str()) {
                None => problems.push(format!("participant '{}' offers no operation '{}'", name, operation)),
                Some(offered) => {
                    for uses in offered.uses() {
                        let problem = match *uses {
                            Uses::Command(command) => (!participant.commands.contains_key(command))
                                .then(|| format!("participant '{}' has no command '{}'", name, command)),
                            Uses::Request(used) => endpoint(
                                used,
                                EndpointOperation::Request,
                                format!("{} ({}, operation '{}')", at, step.name(), operation),
                            ),
                        };
                        problems.extend(problem.map(|problem| {
                            format!("operation '{}' of participant '{}': {}", operation, name, problem)
                        }));
                    }
                }
            },
            Need::Endpoint(_, used, operation) => {
                problems.extend(endpoint(used, *operation, format!("{} ({})", at, step.name())));
            }
        }
    }
    problems
}

/// Every string in `value`, however deep.
fn strings(value: &Value) -> Vec<&str> {
    match value {
        Value::String(text) => vec![text.as_str()],
        Value::Array(values) => values.iter().flat_map(strings).collect(),
        Value::Object(values) => values.values().flat_map(strings).collect(),
        Value::Null | Value::Bool(_) | Value::Number(_) => Vec::new(),
    }
}

/// Collects the problems of one case on one subject OS.
pub(super) struct Problems<'a> {
    case: &'a LoadedCase,
    os: Os,
    errors: Vec<CaseError>,
}

impl<'a> Problems<'a> {
    pub(super) fn push(&mut self, at: Option<String>, message: String) {
        self.errors.push(CaseError {
            case: self.case.info.name.clone(),
            file: self.case.info.file.clone(),
            subject_os: Some(self.os),
            at,
            message,
        });
    }

    fn at(&mut self, at: String) -> At<'_, 'a> {
        At { problems: self, at }
    }
}

/// Collects the problems of one participant.
struct At<'p, 'a> {
    problems: &'p mut Problems<'a>,
    at: String,
}

impl At<'_, '_> {
    fn push(&mut self, message: String) {
        self.problems.push(Some(self.at.clone()), message);
    }
}

/// Plans one participant.
struct Planner<'c, 'p, 'a> {
    name: &'c str,
    participant: &'c LoadedParticipant,
    realization: Realization,
    case: &'c LoadedCase,
    /// The run's replacements.
    set: &'c BTreeMap<String, String>,
    at: &'c mut At<'p, 'a>,
}

impl Planner<'_, '_, '_> {
    /// Plans the participant, and returns it with the connection names its participant type knows.
    fn plan(
        &mut self, registry: &Registry, connected: &BTreeSet<(&str, &str)>,
    ) -> Option<(PlannedParticipant, BTreeMap<&'static str, Consumed>)> {
        let placement = &self.participant.placement;
        let mut case_layer = self.participant.spec.clone();
        let (mut lower, type_name) = match &placement.participant_type {
            Some(name) => {
                // Loading rejects an unknown participant type.
                let participant_type = registry.participant_type(name)?;
                let supported = (self.realization.os, self.realization.enclosure);
                if !participant_type.supports().contains(&supported) {
                    self.at.push(format!(
                        "participant type '{}' has no spec for {} on {}",
                        name, self.realization.enclosure, self.realization.os
                    ));
                    return None;
                }
                let options = placement
                    .realization
                    .as_ref()
                    .map(|request| request.options.clone())
                    .unwrap_or_default();
                let cx = SpecContext {
                    realization: self.realization,
                    options: &options,
                    case: &case_layer,
                    connect: &placement.connect,
                    with: &placement.with,
                    sources: &self.case.sources,
                };
                match participant_type.spec(&cx) {
                    Ok(spec) => (spec, Some(participant_type.name())),
                    Err(message) => {
                        self.at.push(format!("{}: {}", name, message));
                        return None;
                    }
                }
            }
            None => (ParticipantSpec::default(), None),
        };

        lower.files = self.normalize_type_files(std::mem::take(&mut lower.files), type_name);
        let typed = TypeWiring::take(&mut lower);
        let names = self.rewrite_names(&mut case_layer, &lower, type_name);
        self.check_guards(&case_layer, &lower, type_name);
        for (endpoint, _) in case_layer.ready.iter() {
            if !connected.contains(&(self.name, endpoint.as_str())) {
                self.at.push(format!(
                    "ready.{}: no participant connects to this endpoint, so nothing waits for its evidence",
                    endpoint
                ));
            }
        }
        lower.lay(case_layer);
        self.shape(lower, names, type_name, typed, connected)
    }

    /// Keys a participant type's files by their normalized destinations.
    fn normalize_type_files(
        &mut self, files: BTreeMap<String, super::spec::FileSpec>, type_name: Option<&str>,
    ) -> BTreeMap<String, super::spec::FileSpec> {
        let mut normalized = BTreeMap::new();
        for (key, file) in files {
            match self.destination(&key) {
                Ok(destination) => {
                    normalized.insert(destination, file);
                }
                Err(message) => self.at.push(format!(
                    "participant type '{}' files.{}: {}",
                    type_name.unwrap_or_default(),
                    key,
                    message
                )),
            }
        }
        normalized
    }

    /// Rewrites the case layer's file keys to normalized destinations, and returns the name each came from.
    fn rewrite_names(
        &mut self, case_layer: &mut ParticipantSpec, lower: &ParticipantSpec, type_name: Option<&str>,
    ) -> BTreeMap<String, String> {
        let mut names = BTreeMap::new();
        let mut files = BTreeMap::new();
        for (key, file) in std::mem::take(&mut case_layer.files) {
            let destination = if looks_like_path(&key) {
                let destination = match self.destination(&key) {
                    Ok(destination) => destination,
                    Err(message) => {
                        self.at.push(format!("files.{}: {}", key, message));
                        continue;
                    }
                };
                // A directory that places a file at a name's destination is a second spelling of that file.
                let separator = self.separator();
                let targets: BTreeSet<String> = self
                    .targets(&destination, &file, separator)
                    .iter()
                    .map(|target| path_key(target, separator))
                    .collect();
                let named = lower.aliases.iter().find_map(|(alias, aliased)| {
                    let aliased = self.destination(aliased).ok()?;
                    targets
                        .contains(&path_key(&aliased, separator))
                        .then_some((alias, aliased))
                });
                if let Some((alias, aliased)) = named {
                    let message = if path_key(&aliased, separator) == path_key(&destination, separator) {
                        format!("this is the destination of the name '{}'; write the name", alias)
                    } else {
                        format!(
                            "places {}, the destination of the name '{}'; write the name",
                            aliased, alias
                        )
                    };
                    self.at.push(format!("files.{}: {}", key, message));
                    continue;
                }
                destination
            } else if let Some(aliased) = lower.aliases.get(key.as_str()) {
                match self.destination(aliased) {
                    Ok(destination) => {
                        names.insert(destination.clone(), key.clone());
                        destination
                    }
                    Err(message) => {
                        self.at.push(format!("files.{}: {}", key, message));
                        continue;
                    }
                }
            } else {
                let message = match type_name {
                    None => "a key that is not a path is a file name a participant type declares, and this \
                             participant has no participant type"
                        .to_string(),
                    Some(name) if lower.aliases.is_empty() => {
                        format!(
                            "participant type '{}' declares no file names; write a destination path",
                            name
                        )
                    }
                    Some(name) => {
                        let aliases: Vec<_> = lower.aliases.keys().collect();
                        format!(
                            "participant type '{}' declares no such file name; it declares {}",
                            name,
                            super::case::join(&aliases)
                        )
                    }
                };
                self.at.push(format!("files.{}: {}", key, message));
                continue;
            };
            if files.insert(destination.clone(), file).is_some() {
                self.at
                    .push(format!("files.{}: another entry lands on {} too", key, destination));
            }
        }
        case_layer.files = files;
        names
    }

    /// Checks the two changes a case could make that would otherwise fail silently.
    fn check_guards(&mut self, case_layer: &ParticipantSpec, lower: &ParticipantSpec, type_name: Option<&str>) {
        let type_name = type_name.unwrap_or_default();
        if lower.supervised() && case_layer.processes.is_none() {
            let separator = self.separator();
            let supervising: BTreeSet<String> = lower
                .files
                .iter()
                .filter(|(_, file)| file.supervises)
                .map(|(destination, _)| path_key(destination, separator))
                .collect();
            let mut replaced = Vec::new();
            let mut by_directory: BTreeMap<&String, Vec<String>> = BTreeMap::new();
            for (destination, file) in &case_layer.files {
                for target in self.targets(destination, file, separator) {
                    if !supervising.contains(&path_key(&target, separator)) {
                        continue;
                    }
                    if target == *destination {
                        replaced.push(target);
                    } else {
                        by_directory.entry(destination).or_default().push(target);
                    }
                }
            }
            let what = if case_layer.command.is_some() {
                Some("sets 'command'".to_string())
            } else if case_layer.args.is_some() {
                Some("sets 'args'".to_string())
            } else {
                (!replaced.is_empty()).then(|| format!("replaces {}", super::case::join(&replaced)))
            };
            if let Some(what) = what {
                self.at.push(format!(
                    "the case {}, which participant type '{}' uses to supervise its processes; set 'processes' too, \
                     or no exit record is written and a process that never started reads as running",
                    what, type_name
                ));
            }
            for (directory, targets) in by_directory {
                self.at.push(format!(
                    "files.{}: places {}, which participant type '{}' uses to supervise its processes; a directory \
                     cannot replace a participant type's file, so place each under its own destination and set \
                     'processes' too",
                    directory,
                    super::case::join(&targets),
                    type_name
                ));
            }
        }

        for name in self.participant.placement.connect.keys() {
            let Some(consumed) = lower.consumes.get(name.as_str()) else {
                continue;
            };
            for (key, _) in consumed.env {
                let set = case_layer.env.contains_key(*key)
                    || case_layer
                        .programs
                        .iter()
                        .flatten()
                        .any(|program| program.env.contains_key(*key))
                    || case_layer
                        .commands
                        .values()
                        .any(|command| command.env.contains_key(*key));
                if set {
                    self.at.push(format!(
                        "the case sets {}, which connection '{}' delivers; remove it, because it would win over the \
                         delivered value",
                        key, name
                    ));
                }
            }
        }
    }

    /// Makes a merged spec concrete for the realization, and returns it with the connection names its participant type
    /// knows.
    fn shape(
        &mut self, spec: ParticipantSpec, names: BTreeMap<String, String>, type_name: Option<&str>, typed: TypeWiring,
        connected: &BTreeSet<(&str, &str)>,
    ) -> Option<(PlannedParticipant, BTreeMap<&'static str, Consumed>)> {
        let enclosure = self.realization.enclosure;
        let ParticipantSpec {
            image,
            command,
            args,
            programs,
            mut env,
            files,
            mounts,
            endpoints,
            mut ready,
            processes,
            commands,
            collect,
            aliases: _,
            consumes,
            operations,
            wiring: _,
        } = spec;

        let connect = &self.participant.placement.connect;
        for (name, _) in consumes
            .iter()
            .filter(|(name, consumed)| consumed.required && !connect.contains_key(**name))
        {
            self.at.push(format!(
                "participant type '{}' requires connection '{}', and the case does not make it",
                type_name.unwrap_or_default(),
                name
            ));
        }
        for consumed in connect.keys().filter_map(|name| consumes.get(name.as_str())) {
            for (key, value) in consumed.env {
                match Template::parse(*value) {
                    Ok(value) => {
                        env.insert(key.to_string(), value);
                    }
                    Err(message) => self.at.push(format!(
                        "participant type '{}' delivers {}: {}",
                        type_name.unwrap_or_default(),
                        key,
                        message
                    )),
                }
            }
        }

        let launch = if enclosure.runs_an_image() {
            if programs.is_some() {
                self.at.push(format!(
                    "'programs' are host processes, and this participant is a {}",
                    enclosure
                ));
            }
            let Some(image) = image else {
                self.at
                    .push(format!("a {} needs an 'image', and nothing sets one", enclosure));
                return None;
            };
            Launch::Image {
                image: Replaceable::new(image, self.set),
                command,
                args,
            }
        } else {
            for (field, set) in [
                ("image", image.is_some()),
                ("command", command.is_some()),
                ("args", args.is_some()),
            ] {
                if set {
                    self.at.push(format!(
                        "'{}' is for containers and pods, and this participant is a {}",
                        field, enclosure
                    ));
                }
            }
            let programs = programs.unwrap_or_default();
            if programs.is_empty() {
                self.at
                    .push(format!("a {} needs 'programs', and nothing sets any", enclosure));
                return None;
            }
            Launch::Programs(programs)
        };

        let program_names: Vec<&str> = match &launch {
            Launch::Programs(programs) => programs.iter().map(|program| program.name.as_str()).collect(),
            Launch::Image { .. } => Vec::new(),
        };
        let mut seen = BTreeSet::new();
        for program in &program_names {
            if !is_name(program) {
                self.at.push(format!("program name '{}' {}", program, NAME_RULE));
            }
            if !seen.insert(*program) {
                self.at.push(format!("two programs are named '{}'", program));
            }
        }

        let mut reserved_env: Vec<&String> = env.keys().collect();
        if let Launch::Programs(programs) = &launch {
            reserved_env.extend(programs.iter().flat_map(|program| program.env.keys()));
        }
        reserved_env.extend(commands.values().flat_map(|command| command.env.keys()));
        for key in reserved_env
            .into_iter()
            .filter(|key| key.to_ascii_uppercase().starts_with(RESERVED_ENV_PREFIX))
        {
            self.at.push(format!(
                "environment key {} is reserved: the prefix {} belongs to the harness",
                key, RESERVED_ENV_PREFIX
            ));
        }

        let mounts = mounts.unwrap_or_default();
        if !mounts.is_empty() && !enclosure.facts().mounts {
            self.at.push(format!(
                "'mounts' apply to Linux containers and pods, and this participant is a {}",
                enclosure
            ));
        } else {
            for mount in &mounts {
                for (which, path) in [("host path", &mount.host), ("path", &mount.path)] {
                    if !path.starts_with('/') {
                        self.at
                            .push(format!("mounts: the {} '{}' is not an absolute path", which, path));
                    }
                }
            }
        }
        let collect = collect.unwrap_or_default();
        let mut collected = BTreeSet::new();
        for file in &collect {
            if !is_file_name(&file.name) {
                self.at.push(format!(
                    "collect: the name '{}' must be a plain file name, with no directory",
                    file.name
                ));
            }
            if !collected.insert(file.name.as_str()) {
                self.at
                    .push(format!("collect: two files are collected as '{}'", file.name));
            }
        }
        let files = self.place_files(files, &names, type_name);
        let processes = self.processes(processes, &program_names, &launch);
        let mut by_name = BTreeMap::new();
        for &operation in operations {
            if by_name.insert(operation.name(), operation).is_some() {
                self.at.push(format!(
                    "participant type '{}' offers two operations named '{}'",
                    type_name.unwrap_or_default(),
                    operation.name()
                ));
            }
        }

        for (endpoint, evidence) in &ready {
            if !endpoints.contains_key(endpoint) {
                self.at
                    .push(format!("ready.{}: the participant has no such endpoint", endpoint));
            }
            // Only a consumer waits for evidence, so a participant type's default may name a process the case's
            // `processes` left out, as long as nothing connects to the endpoint.
            let waited_for = connected.contains(&(self.name, endpoint.as_str()));
            if let Evidence::Output { process, .. } = evidence {
                if waited_for && !processes.iter().any(|candidate| candidate.name() == process) {
                    self.at.push(format!(
                        "ready.{}: the participant has no process '{}'",
                        endpoint, process
                    ));
                }
            }
        }
        let mut planned_endpoints = BTreeMap::new();
        for (name, endpoint) in endpoints {
            if !is_name(&name) {
                self.at.push(format!("endpoint name '{}' {}", name, NAME_RULE));
            }
            match PlannedEndpoint::plan(endpoint, ready.remove(&name), self.name, self.realization) {
                Ok(endpoint) => {
                    planned_endpoints.insert(name, endpoint);
                }
                Err(message) => self.at.push(format!("endpoints.{}: {}", name, message)),
            }
        }

        let mut planned = PlannedParticipant {
            participant_type: self.participant.placement.participant_type.clone(),
            subject: self.participant.placement.subject,
            realization: self.realization,
            options: self
                .participant
                .placement
                .realization
                .as_ref()
                .map(|request| request.options.clone())
                .unwrap_or_default(),
            launch,
            env,
            files,
            mounts,
            endpoints: planned_endpoints,
            connections: BTreeMap::new(),
            processes,
            commands,
            collect,
            operations: by_name,
            later: BTreeMap::new(),
        };
        for message in planned
            .check_addresses()
            .into_iter()
            .chain(planned.check_wiring(self.name, connect, typed, &consumes))
        {
            self.at.push(message);
        }
        Some((planned, consumes))
    }

    /// Expands each file to the files it places, with their contents, and checks modes, shapes and reserved paths.
    fn place_files(
        &mut self, files: BTreeMap<String, super::spec::FileSpec>, names: &BTreeMap<String, String>,
        type_name: Option<&str>,
    ) -> Vec<PlannedFile> {
        let windows = self.realization.os == Os::Windows;
        let separator = self.separator();
        let reserved = self.realization.enclosure.facts().reserved_paths(self.name);
        let mut placed = Vec::new();
        let mut seen = BTreeSet::new();
        for (destination, file) in files {
            if windows && file.mode.is_some() {
                self.at.push(format!(
                    "files.{}: 'mode' does not apply to a Windows participant",
                    destination
                ));
            }
            let mode = (!windows).then_some(file.mode.unwrap_or(0o644));
            let is_dir = destination.ends_with(separator);
            let labelled = |label: &str| format!("{}: {}", type_name.unwrap_or_default(), label);
            let (source, contents) = match file.source {
                FileSource::Case { from, template } => {
                    (from.display().to_string(), self.case.sources.placed(&from, template))
                }
                FileSource::Bytes { label, bytes } => (labelled(label), Ok(Source::File(Contents::of(bytes.to_vec())))),
                FileSource::Text { label, text, template } => {
                    let contents = if template {
                        Template::parse(&text).map(Contents::Template)
                    } else {
                        Ok(Contents::Text(text))
                    };
                    (labelled(label), contents.map(Source::File))
                }
            };
            let targets = match (contents, is_dir) {
                (Ok(Source::File(contents)), false) => vec![(destination.clone(), source, contents)],
                (Ok(Source::Dir(entries)), true) => entries
                    .into_iter()
                    .map(|(entry, contents)| {
                        (
                            under(&destination, &entry, separator),
                            format!("{}/{}", source.trim_end_matches('/'), entry),
                            contents,
                        )
                    })
                    .collect(),
                (Ok(Source::Dir(_)), false) => {
                    self.at.push(format!(
                        "files.{}: the source is a directory, so the destination must end with '{}'",
                        destination, separator
                    ));
                    continue;
                }
                (Ok(Source::File(_)), true) => {
                    self.at.push(format!(
                        "files.{}: the destination is a directory, and the source is a file",
                        destination
                    ));
                    continue;
                }
                (Err(message), _) => {
                    self.at.push(format!("files.{}: {}", destination, message));
                    continue;
                }
            };
            for (target, source, contents) in targets {
                let target = match self.destination(&target) {
                    Ok(target) => target,
                    Err(message) => {
                        self.at.push(format!("files.{}: {}", destination, message));
                        continue;
                    }
                };
                if let Some(path) = reserved.iter().find(|path| is_at_or_under(&target, path, separator)) {
                    self.at
                        .push(format!("files.{}: {} is reserved by the harness", destination, path));
                }
                if !seen.insert(path_key(&target, separator)) {
                    self.at.push(format!("two files land on {}", target));
                }
                placed.push(PlannedFile {
                    name: names.get(&destination).cloned(),
                    destination: target,
                    source,
                    mode,
                    contents,
                });
            }
        }
        placed
    }

    /// The separator of the realization's file paths.
    fn separator(&self) -> char {
        self.realization.enclosure.facts().separator
    }

    /// The destinations a file of the case places: one, or one per file of a directory source.
    fn targets(&self, destination: &str, file: &super::spec::FileSpec, separator: char) -> Vec<String> {
        let entries = match &file.source {
            FileSource::Case { from, .. } if destination.ends_with(separator) => self.case.sources.dir_entries(from),
            _ => None,
        };
        match entries {
            Some(entries) => entries
                .into_iter()
                .map(|entry| under(destination, entry, separator))
                .collect(),
            None => vec![destination.to_string()],
        }
    }

    /// The processes steps may name: derived from the launch when unset, or as declared.
    fn processes(
        &mut self, processes: Option<Vec<ProcessSpec>>, program_names: &[&str], launch: &Launch,
    ) -> Vec<ProcessSpec> {
        let Some(processes) = processes else {
            let names = match launch {
                Launch::Image { .. } => vec![DEFAULT_PROCESS],
                Launch::Programs(_) => program_names.to_vec(),
            };
            let mut derived = Vec::new();
            for name in names {
                match ProcessSpec::observed_by_realization(name) {
                    Ok(process) => derived.push(process),
                    Err(e) => self.at.push(e),
                }
            }
            return derived;
        };
        let mut seen = BTreeSet::new();
        for process in &processes {
            if !seen.insert(process.name()) {
                self.at.push(format!("two processes are named '{}'", process.name()));
            }
        }
        if self.realization.enclosure.runs_an_image() {
            let through: Vec<&str> = processes
                .iter()
                .filter(|process| {
                    *process.exit() == ExitChannel::Container || *process.output() == OutputChannel::Container
                })
                .map(ProcessSpec::name)
                .collect();
            if through.len() > 1 {
                self.at.push(format!(
                    "processes {} are observed through the container, which reports on its only process; observe all \
                     but one through an exit record and a log file",
                    super::case::join(&through)
                ));
            }
        }
        processes
    }

    /// Normalizes a destination for the realization, or says why it is not one.
    fn destination(&self, key: &str) -> Result<String, String> {
        let (path, separator, rule) = match self.realization.enclosure {
            Enclosure::LinuxContainer | Enclosure::KubernetesPod => match key.strip_prefix('/') {
                Some(rest) => (format!("/{}", rest), '/', rest.to_string()),
                None => {
                    return Err(format!(
                        "a destination in a {} is an absolute path",
                        self.realization.enclosure
                    ))
                }
            },
            Enclosure::WindowsContainer => {
                let path = key.replace('/', "\\");
                match path.strip_prefix("C:\\").or_else(|| path.strip_prefix("c:\\")) {
                    Some(rest) => (format!("C:\\{}", rest), '\\', rest.to_string()),
                    None => return Err("a destination in a Windows container is a path on drive C:".to_string()),
                }
            }
            Enclosure::HostProcessGroup => match key.strip_prefix("./") {
                Some(rest) if rest.contains('\\') => {
                    return Err("a destination of host processes separates its parts with '/'".to_string())
                }
                Some(rest) => (key.to_string(), '/', rest.to_string()),
                None => {
                    return Err(
                        "a destination of host processes is './...', under the participant's own directory".to_string(),
                    )
                }
            },
        };
        // The realization's root is itself a directory destination.
        if rule.is_empty() {
            return Ok(path);
        }
        // One trailing separator marks a directory; a second is an empty part.
        let segments: Vec<&str> = rule.strip_suffix(separator).unwrap_or(&rule).split(separator).collect();
        if segments
            .iter()
            .any(|segment| segment.is_empty() || *segment == "." || *segment == "..")
        {
            return Err(format!("'{}' has an empty, '.' or '..' part", key));
        }
        Ok(path)
    }
}

fn octal<S>(mode: &Option<u32>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    mode.map(|mode| format!("{:04o}", mode)).serialize(serializer)
}

fn operations<S>(operations: &BTreeMap<&'static str, &'static dyn Operation>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.collect_map(operations.iter().map(|(name, operation)| (name, operation.uses())))
}

/// Whether a files key is a destination rather than a name.
fn looks_like_path(key: &str) -> bool {
    key.contains(['/', '\\'])
}

/// The destination of `entry`, a `/`-separated path under the directory destination `dir`.
fn under(dir: &str, entry: &str, separator: char) -> String {
    format!("{}{}", dir, entry.replace('/', &separator.to_string()))
}

/// A path as compared with others: Windows paths ignore letter case.
fn path_key(path: &str, separator: char) -> String {
    if separator == '\\' {
        path.to_ascii_lowercase()
    } else {
        path.to_string()
    }
}

fn is_at_or_under(path: &str, reserved: &str, separator: char) -> bool {
    let (path, reserved) = (path_key(path, separator), path_key(reserved, separator));
    path == reserved
        || path
            .strip_prefix(&reserved)
            .is_some_and(|rest| rest.starts_with(separator))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::core::case::{self, CASE_FILE};

    #[test]
    fn a_plan_outlives_its_case_directory_and_loaded_case() {
        let suite = tempfile::tempdir().unwrap();
        let dir = suite.path().join("owned");
        fs::create_dir_all(dir.join("conf")).unwrap();
        fs::write(
            dir.join(CASE_FILE),
            r#"
timeout: 1m
subject_os: [linux]
participants:
  app:
    image: alpine:3.20
    subject: true
    files:
      /etc/app/blob: blob
      /etc/app/conf/: conf/
procedure: []
"#,
        )
        .unwrap();
        fs::write(dir.join("blob"), [0xff, 0x00]).unwrap();
        fs::write(dir.join("conf").join("app.conf"), "ping\n").unwrap();
        let registry = Registry::default();

        let case = case::load(&dir, &registry).unwrap();
        let plan = plan(&case, Os::Linux, &registry, &BTreeMap::new()).unwrap();
        drop((case, registry, suite));

        let files: Vec<(&str, &Contents)> = plan.participants["app"]
            .files
            .iter()
            .map(|file| (file.destination.as_str(), &file.contents))
            .collect();
        assert_eq!(
            files,
            [
                ("/etc/app/blob", &Contents::Bytes(vec![0xff, 0x00])),
                ("/etc/app/conf/app.conf", &Contents::Text("ping\n".to_string())),
            ]
        );
    }
}
