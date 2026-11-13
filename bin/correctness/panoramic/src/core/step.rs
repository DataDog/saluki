//! Steps and operations: what a case's procedure runs, and the capabilities a participant offers to steps.
//!
//! A step is a registered name and a struct built from the case's fields. A step never learns how a participant is
//! realized, and it does not know a participant type: it names a capability, and any participant that offers it
//! works.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use tokio::time::Instant;

use super::handle::{Handle, HandleError};
use super::observe::Kept;
use super::record::StepStatus;
use super::reference::Values;
use super::spec::Keep;
use super::template::Template;

/// What a step does to the case.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StepPurpose {
    /// Checks a condition. Only assertions may run in a `parallel` block.
    Assertion,
    /// Changes state.
    Action,
}

/// A registered step: builds a [`Step`] from its fields.
pub(crate) trait StepFactory: Send + Sync {
    /// The name a case writes as `step`.
    fn name(&self) -> &'static str;

    fn purpose(&self) -> StepPurpose {
        StepPurpose::Assertion
    }

    /// The fields that may hold references. A reference in any other field is an error.
    fn template_fields(&self) -> &'static [&'static str] {
        &[]
    }

    /// Decodes and checks the fields. Does no I/O.
    ///
    /// # Errors
    ///
    /// Returns an error if the fields do not decode or cannot be honored.
    fn build(&self, fields: &StepFields) -> Result<Box<dyn Step>, String>;
}

/// A step built for one case and one subject OS.
#[async_trait]
pub(crate) trait Step: fmt::Debug + Send + Sync {
    fn name(&self) -> &'static str;

    /// Everything of the case the step uses, so the plan can check it before anything starts.
    fn needs(&self) -> Vec<Need>;

    fn budget(&self) -> Duration;

    /// Runs the step. The driver may drop this future on cancellation or when the budget runs out.
    async fn run(&self, cx: &StepContext<'_>) -> StepReport;
}

/// Something of the case a step uses.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Need {
    Participant(String),
    /// A participant and one of its processes.
    Process(String, String),
    /// A participant and one of its named commands.
    // TODO: remove the dead_code allowance once a step runs a named command.
    #[allow(dead_code)]
    Command(String, String),
    /// A participant and one of its operations.
    Operation(String, String),
    /// A participant, one of its endpoints, and what the step does to it.
    Endpoint(String, String, EndpointOperation),
}

/// What a step does to an endpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EndpointOperation {
    Probe,
    Send,
    Request,
}

/// A capability a participant offers to steps: a name, what it uses of its participant, and a function.
#[async_trait]
pub(crate) trait Operation: fmt::Debug + Send + Sync {
    fn name(&self) -> &'static str;

    /// The commands and endpoints of its own participant the operation uses. The plan checks each.
    fn uses(&self) -> &'static [Uses];

    /// Runs on one participant, through the same handle a step holds.
    async fn run(&self, on: &Handle<'_>, args: Value, limit: Duration) -> Result<Value, HandleError>;
}

/// Something of its own participant an operation uses.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Uses {
    /// One of the participant's named commands.
    // TODO: remove the dead_code allowance once an operation runs a named command.
    #[allow(dead_code)]
    Command(&'static str),
    /// A request to one of the participant's endpoints.
    Request(&'static str),
}

/// What a running step can reach: a handle to each of the case's started participants, the values its template fields
/// refer to, where it keeps files, and when it must be done.
pub(crate) struct StepContext<'a> {
    handles: &'a BTreeMap<String, Handle<'a>>,
    values: &'a Values,
    kept: &'a Kept,
    /// When the step's own budget, or the case's deadline, runs out.
    pub(crate) deadline: Instant,
}

impl<'a> StepContext<'a> {
    pub(crate) fn new(
        handles: &'a BTreeMap<String, Handle<'a>>, values: &'a Values, kept: &'a Kept, deadline: Instant,
    ) -> Self {
        Self {
            handles,
            values,
            kept,
            deadline,
        }
    }

    /// Keeps `contents` as the collected file `name` of participant `on`, by `keep`, with the files collected from it.
    pub(crate) async fn keep(&self, on: &str, name: &str, contents: &[u8], keep: Keep) {
        self.kept.write(on, name, contents, keep).await;
    }

    /// The text of `template`, one of the step's template fields, with every reference filled in. Each value is
    /// escaped when the text is matched as a regex, so a path or an address matches itself.
    ///
    /// # Errors
    ///
    /// Returns a harness error if a reference has no value.
    pub(crate) fn fill(&self, template: &Template, regex: bool) -> Result<String, HandleError> {
        self.values.fill_step(template, regex).map_err(HandleError::Harness)
    }

    /// The handle to participant `name`.
    ///
    /// # Errors
    ///
    /// Returns a harness error if the case has no started participant `name`.
    pub(crate) fn handle(&self, name: &str) -> Result<&Handle<'a>, HandleError> {
        self.handles
            .get(name)
            .ok_or_else(|| HandleError::Harness(format!("the case has no started participant '{}'", name)))
    }
}

/// How a step ended.
#[derive(Clone, Debug)]
pub(crate) struct StepReport {
    pub(crate) status: StepStatus,
    pub(crate) message: String,
}

/// A step's own fields, as the case wrote them: everything but `step`.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(transparent)]
pub(crate) struct StepFields(pub(crate) BTreeMap<String, Value>);

impl StepFields {
    /// Decodes the fields into the step's own struct, which should deny unknown fields.
    ///
    /// # Errors
    ///
    /// Returns an error if the fields do not decode into `T`.
    pub(crate) fn decode<T>(&self) -> Result<T, String>
    where
        T: DeserializeOwned,
    {
        let fields = Value::Object(self.0.clone().into_iter().collect());
        serde_json::from_value(fields).map_err(|e| e.to_string())
    }
}

/// One entry of a procedure: a step, or assertions that run together.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProcedureNode<T> {
    Step(T),
    Parallel(Vec<T>),
}

impl<T> ProcedureNode<T> {
    /// The steps of this entry, in order.
    pub(crate) fn steps(&self) -> &[T] {
        match self {
            Self::Step(step) => std::slice::from_ref(step),
            Self::Parallel(steps) => steps,
        }
    }
}

/// A step as the case wrote it: its name and its fields.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct StepEntry {
    pub(crate) step: String,
    pub(crate) fields: StepFields,
}

impl StepEntry {
    /// Decodes one procedure entry.
    ///
    /// # Errors
    ///
    /// Returns an error if the entry is neither a step nor a `parallel` block of steps.
    pub(crate) fn decode_node(value: Value) -> Result<ProcedureNode<Self>, String> {
        let Value::Object(mut map) = value else {
            return Err("a procedure entry is a map with a 'step' or a 'parallel' key".to_string());
        };
        match map.remove("parallel") {
            Some(Value::Array(members)) if members.is_empty() => {
                Err("a 'parallel' block holds at least one step".to_string())
            }
            Some(Value::Array(members)) if map.is_empty() => {
                let members = members.into_iter().map(Self::decode).collect::<Result<_, _>>()?;
                Ok(ProcedureNode::Parallel(members))
            }
            Some(_) if !map.is_empty() => Err("a 'parallel' entry holds nothing but its list of steps".to_string()),
            Some(_) => Err("'parallel' holds a list of steps".to_string()),
            None => Self::decode(Value::Object(map)).map(ProcedureNode::Step),
        }
    }

    fn decode(value: Value) -> Result<Self, String> {
        let Value::Object(mut map) = value else {
            return Err("a step is a map with a 'step' key".to_string());
        };
        if map.contains_key("parallel") {
            return Err("a 'parallel' block cannot hold another".to_string());
        }
        match map.remove("step") {
            Some(Value::String(step)) => Ok(Self {
                step,
                fields: StepFields(map.into_iter().collect()),
            }),
            Some(other) => Err(format!("a step's name must be a string, not {}", other)),
            None => Err("a step needs a 'step' key that names it".to_string()),
        }
    }
}
