use core::fmt;
use std::{borrow::Cow, ops::Deref};

use saluki_event::DataType;

const INVALID_COMPONENT_ID: &str =
    "component IDs may only contain alphanumeric characters (a-z, A-Z, or 0-9), underscores, and hyphens";
const INVALID_COMPONENT_OUTPUT_ID: &str = "component IDs may only contain alphanumeric characters (a-z, A-Z, or 0-9), underscores, hyphens, and an optional period";

/// A component identifier.
#[derive(Clone, Debug, Hash, Eq, Ord, PartialEq, PartialOrd)]
pub struct ComponentId(Cow<'static, str>);

impl TryFrom<&str> for ComponentId {
    type Error = &'static str;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        if !validate_component_id(value, false) {
            Err(INVALID_COMPONENT_ID)
        } else {
            Ok(Self(value.to_string().into()))
        }
    }
}

impl Deref for ComponentId {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.0.as_ref()
    }
}

impl fmt::Display for ComponentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A component output identifier.
#[derive(Clone, Debug, Hash, Eq, Ord, PartialEq, PartialOrd)]
pub struct ComponentOutputId(Cow<'static, str>);

impl ComponentOutputId {
    /// Creates a new `ComponentOutputId` from an identifier and output definition.
    ///
    /// ## Errors
    ///
    /// If generated component output ID is not valid, such as due the identifier or output definition containing
    /// invalid characters, an error is returned.
    pub fn from_definition(component_id: ComponentId, output_def: &OutputDefinition) -> Result<Self, (String, String)> {
        match output_def.output_name() {
            None => Ok(Self(component_id.0)),
            Some(output_name) => {
                let output_id = format!("{}.{}", component_id.0, output_name);

                if validate_component_id(&output_id, true) {
                    Ok(Self(output_id.into()))
                } else {
                    // TODO: make the error reason better
                    Err((output_id, "invalid".to_string()))
                }
            }
        }
    }

    /// Returns the component ID.
    pub fn component_id(&self) -> ComponentId {
        if let Some((component_id, _)) = self.0.split_once('.') {
            ComponentId(component_id.to_string().into())
        } else {
            ComponentId(self.0.clone())
        }
    }

    /// Returns the output name.
    pub fn output(&self) -> OutputName {
        if let Some((_, output_name)) = self.0.split_once('.') {
            OutputName::Given(output_name.to_string().into())
        } else {
            OutputName::Default
        }
    }
}

impl TryFrom<&str> for ComponentOutputId {
    type Error = &'static str;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        if !validate_component_id(value, true) {
            Err(INVALID_COMPONENT_OUTPUT_ID)
        } else {
            Ok(Self(value.to_string().into()))
        }
    }
}

impl fmt::Display for ComponentOutputId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

const fn validate_component_id(id: &str, as_output_id: bool) -> bool {
    let id_bytes = id.as_bytes();

    // Identifiers cannot be empty strings.
    if id_bytes.is_empty() {
        return false;
    }

    // Keep track of whether or not we've seen a period yet. If we have, we track it's index, which serves two purposes:
    // figure out if we see _another_ period (can only have one), and ensure that either side of the string (when split
    // by the separator) isn't empty.
    let mut idx = 0;
    let end = id_bytes.len();
    let mut separator_idx = end;
    while idx < end {
        let b = id_bytes[idx];
        if !b.is_ascii_alphanumeric() && b != b'_' && b != b'-' {
            if as_output_id && b == b'.' && separator_idx == end {
                // Found our period separator.
                separator_idx = idx;
            } else {
                // We're not validating as an output ID, or we already saw a period separator, which means this is
                // invalid.
                return false;
            }
        }

        idx += 1;
    }

    if as_output_id && (separator_idx == 0 || separator_idx == end - 1) {
        // Can't have the separator as the first or last character.
        return false;
    }

    true
}

/// An output name.
///
/// Components must always have at least one output, but an output can either be the default output or a named output.
/// This allows for components to have multiple outputs, potentially with one (the default) acting as a catch-all.
///
/// `OutputName` is used to differentiate between a default output and named outputs.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum OutputName {
    /// Default output.
    Default,

    /// Named output.
    Given(Cow<'static, str>),
}

impl fmt::Display for OutputName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OutputName::Default => write!(f, "_default"),
            OutputName::Given(name) => write!(f, "{}", name),
        }
    }
}

/// An output definition.
///
/// Outputs are a combination of the output name and data type, which defines the data type (or types) of events that
/// can be emitted from a particular component output.
#[derive(Clone, Debug)]
pub struct OutputDefinition {
    name: OutputName,
    data_ty: DataType,
}

impl OutputDefinition {
    /// Creates a default output with the given data type.
    pub const fn default_output(data_ty: DataType) -> Self {
        Self {
            name: OutputName::Default,
            data_ty,
        }
    }

    /// Creates a named output with the given name and data type.
    pub fn named_output<S>(name: S, data_ty: DataType) -> Self
    where
        S: Into<Cow<'static, str>>,
    {
        Self {
            name: OutputName::Given(name.into()),
            data_ty,
        }
    }

    /// Returns the output name.
    ///
    /// If this is a default output, `None` is returned.
    pub fn output_name(&self) -> Option<&str> {
        match &self.name {
            OutputName::Default => None,
            OutputName::Given(name) => Some(name.as_ref()),
        }
    }

    /// Returns the data type.
    pub fn data_ty(&self) -> DataType {
        self.data_ty
    }
}

/// Unique identifier for a specified output of a component, including the data type of the output.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TypedComponentOutputId {
    component_output: ComponentOutputId,
    output_ty: DataType,
}

impl TypedComponentOutputId {
    /// Creates a new `TypedComponentOutputId` from the given component output ID and output data type.
    pub fn new(component_output: ComponentOutputId, output_ty: DataType) -> Self {
        Self {
            component_output,
            output_ty,
        }
    }

    /// Gets a reference to the component output ID.
    pub fn component_output(&self) -> &ComponentOutputId {
        &self.component_output
    }

    /// Returns the output data type.
    pub fn output_ty(&self) -> DataType {
        self.output_ty
    }
}
