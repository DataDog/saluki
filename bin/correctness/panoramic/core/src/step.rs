//! Step declarations and field decoding.

use std::{collections::BTreeMap, fmt};

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::backend::LateFact;

/// A step's fields as written in the case file, keyed by field name.
pub type StepFields = BTreeMap<String, Value>;

/// Something a built step uses, checked against the case when it loads.
/// Fact and late-fact needs are checked again against each OS's layout when the case is planned.
#[derive(Debug, PartialEq)]
pub enum Need {
    /// The participant the step runs on.
    Handle(String),
    /// One named process of a participant: (participant, process).
    Process(String, String),
    /// An early fact of a participant: (participant, fact).
    Fact(String, String),
    /// A late fact of a participant: (participant, fact). Known only once the participant exists or runs.
    Late(String, LateFact),
}

/// A step built from its case-file fields.
pub trait Step: fmt::Debug + Send + Sync {
    /// Participants, processes, facts and late facts the step uses.
    fn needs(&self) -> Vec<Need>;
}

/// Builds a named step from its case-file fields at load time.
///
/// The step crate registers factories for steps such as log and file assertions; the case loader
/// calls them before checking each built step's [`Need`]s against the case and participant types.
pub trait StepFactory: Send + Sync {
    /// Registry name, as written in a node's `step`.
    fn name(&self) -> &'static str;

    /// The only fields in which the loader accepts `{{ … }}` template references; defaults to none.
    ///
    /// The built step must report each field's references through [`Template::needs`](crate::case::Template::needs).
    fn template_fields(&self) -> &'static [&'static str] {
        &[]
    }

    /// Decodes and validates fields without performing I/O or running the step.
    ///
    /// Report participant, process, fact and late-fact dependencies through [`Step::needs`]; the loader validates them.
    ///
    /// # Errors
    ///
    /// Returns the first problem with the step's fields.
    fn build(&self, fields: &StepFields) -> Result<Box<dyn Step>, String>;
}

/// Decodes step fields into `T`; errors name the failing field.
pub fn decode_fields<T>(fields: &StepFields) -> Result<T, String>
where
    T: DeserializeOwned,
{
    serde_path_to_error::deserialize(Value::Object(fields.clone().into_iter().collect())).map_err(|e| e.to_string())
}
