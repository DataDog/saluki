//! Identifiers used in plans.
//!
//! Most identifiers follow one grammar: a lowercase ASCII letter, then up to 62 lowercase letters, digits, or single
//! underscores between them, so an identifier never ends with an underscore or contains two in a row. As a regular
//! expression: `^[a-z](?:[a-z0-9]|_(?=[a-z0-9])){0,62}$`.

use std::{borrow::Borrow, fmt, str::FromStr};

use saluki_core::topology::ComponentId;
use schemars::{
    gen::SchemaGenerator,
    schema::{InstanceType, Metadata, Schema, SchemaObject, StringValidation},
    JsonSchema,
};
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use snafu::Snafu;

/// The maximum length of an identifier, in bytes.
pub const MAX_IDENTIFIER_LEN: usize = 63;

/// The identifier grammar, as a regular expression.
pub const IDENTIFIER_PATTERN: &str = "^[a-z](?:[a-z0-9]|_(?=[a-z0-9])){0,62}$";

/// The local identifier that artifact edges use to refer to their scope's inputs, as in `input.metrics`.
pub const SCOPE_INPUT_NODE: &str = "input";

/// The local identifier reserved for artifact edges that reach their scope's exits, as in `exit.aggregated`.
pub const SCOPE_EXIT_NODE: &str = "exit";

const IDENTIFIER_RULE: &str = "identifiers start with a lowercase ASCII letter and contain only lowercase letters, \
                               digits, and single underscores between them, up to 63 characters";
const PORT_NAME_RULE: &str = "port names are identifiers or decimal indices from 0 to 9999";
const BINDING_NAME_RULE: &str =
    "binding names are one or more segments of lowercase letters, digits, and underscores, \
                                 separated by periods, up to 255 characters";
const OPAQUE_REFERENCE_RULE: &str =
    "references contain only printable, non-space ASCII characters, up to 512 characters";

/// An error parsing an identifier.
#[derive(Clone, Debug, Eq, PartialEq, Snafu)]
#[snafu(context(suffix(false)))]
pub enum IdError {
    /// The value does not follow the grammar for its kind of identifier.
    #[snafu(display("Invalid {} `{}`: {}.", what, value, rule))]
    Invalid {
        /// What kind of identifier was being parsed.
        what: &'static str,

        /// The rejected value.
        value: String,

        /// The rule the value breaks.
        rule: &'static str,
    },
}

fn invalid(what: &'static str, value: &str, rule: &'static str) -> IdError {
    IdError::Invalid {
        what,
        value: value.to_string(),
        rule,
    }
}

/// Returns `true` if `value` follows the identifier grammar.
pub fn is_identifier(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_IDENTIFIER_LEN || !bytes[0].is_ascii_lowercase() {
        return false;
    }

    let is_alphanumeric = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    for (i, &b) in bytes.iter().enumerate().skip(1) {
        if is_alphanumeric(b) {
            continue;
        }
        if b != b'_' || !bytes.get(i + 1).is_some_and(|&next| is_alphanumeric(next)) {
            return false;
        }
    }

    true
}

/// Returns a JSON Schema for a string with the given pattern and length bounds.
fn string_schema(pattern: &str, max_length: u32, description: &str) -> Schema {
    SchemaObject {
        instance_type: Some(InstanceType::String.into()),
        string: Some(Box::new(StringValidation {
            max_length: Some(max_length),
            min_length: Some(1),
            pattern: Some(pattern.to_string()),
        })),
        metadata: Some(Box::new(Metadata {
            description: Some(description.to_string()),
            ..Default::default()
        })),
        ..Default::default()
    }
    .into()
}

/// Implements `Serialize` and `Deserialize` as a string, through `Display` and `FromStr`, plus `JsonSchema` from the
/// given pattern.
macro_rules! string_serde {
    ($ty:ident, $pattern:expr, $max_length:expr, $description:expr) => {
        impl Serialize for $ty {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for $ty {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let value = String::deserialize(deserializer)?;
                value.parse().map_err(de::Error::custom)
            }
        }

        impl JsonSchema for $ty {
            fn schema_name() -> String {
                stringify!($ty).to_string()
            }

            fn json_schema(_: &mut SchemaGenerator) -> Schema {
                string_schema(&$pattern, $max_length, $description)
            }
        }
    };
}

/// An extra identifier rule that accepts every identifier.
fn no_extra_rule(_value: &str) -> Option<&'static str> {
    None
}

/// An extra identifier rule that rejects [`SCOPE_INPUT_NODE`] and [`SCOPE_EXIT_NODE`].
fn not_scope_boundary(value: &str) -> Option<&'static str> {
    match value {
        SCOPE_INPUT_NODE => Some("`input` is reserved for the scope's inputs"),
        SCOPE_EXIT_NODE => Some("`exit` is reserved for the scope's exits"),
        _ => None,
    }
}

/// Defines a string newtype that follows the identifier grammar, plus an optional extra rule given as a function that
/// returns the rule a value breaks, if any.
macro_rules! identifier {
    ($(#[$meta:meta])* $ty:ident, $article:literal $what:literal) => {
        identifier!($(#[$meta])* $ty, $article $what, no_extra_rule);
    };
    ($(#[$meta:meta])* $ty:ident, $article:literal $what:literal, $extra_rule:path) => {
        $(#[$meta])*
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $ty(String);

        impl $ty {
            /// Parses an identifier.
            ///
            /// # Errors
            ///
            /// Returns an error if `value` does not follow the identifier grammar.
            pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
                let value = value.into();
                if !is_identifier(&value) {
                    return Err(invalid($what, &value, IDENTIFIER_RULE));
                }
                if let Some(rule) = $extra_rule(value.as_str()) {
                    return Err(invalid($what, &value, rule));
                }
                Ok(Self(value))
            }

            /// Returns the identifier as a string.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl FromStr for $ty {
            type Err = IdError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }

        impl AsRef<str> for $ty {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl Borrow<str> for $ty {
            fn borrow(&self) -> &str {
                &self.0
            }
        }

        string_serde!($ty, IDENTIFIER_PATTERN, MAX_IDENTIFIER_LEN as u32, concat!($article, " ", $what, "."));
    };
}

identifier!(
    /// The local part of a node identifier.
    LocalId,
    "A" "node ID"
);

identifier!(
    /// The name of a registered operator, such as `split` or `tee`.
    OperatorName,
    "An" "operator name"
);

identifier!(
    /// The identifier of a declared output.
    ///
    /// Identifiers starting with [`DECLARED_OUTPUT_PREFIX`] are reserved for outputs that a resources artifact
    /// declares: lowering never gives a native output one.
    OutputId,
    "An" "output ID"
);

/// The prefix of the identifiers of outputs a resources artifact declares, as in `resources_team_d`.
///
/// Lowering reserves it: no native output identifier starts with it, so a declared output never collides with a native
/// one, now or later. A declared output's identifier is an ordinary identifier, so the identifier grammar and the
/// surface syntax are unchanged (`emit resources_team_d`).
pub const DECLARED_OUTPUT_PREFIX: &str = "resources_";

/// The longest name of a declared output, in bytes: what fits after [`DECLARED_OUTPUT_PREFIX`] in an identifier.
pub const MAX_DECLARED_OUTPUT_NAME_LEN: usize = MAX_IDENTIFIER_LEN - DECLARED_OUTPUT_PREFIX.len();

impl OutputId {
    /// Returns the identifier of the output a resources artifact declares as `name`: `resources_<name>`.
    ///
    /// # Errors
    ///
    /// Returns an error if `resources_<name>` does not follow the identifier grammar, as when `name` is empty, longer
    /// than [`MAX_DECLARED_OUTPUT_NAME_LEN`], or not itself an identifier.
    pub fn declared(name: &str) -> Result<Self, IdError> {
        if !is_identifier(name) {
            return Err(invalid("declared output name", name, IDENTIFIER_RULE));
        }
        Self::new(format!("{}{}", DECLARED_OUTPUT_PREFIX, name))
            .map_err(|_| invalid("declared output name", name, DECLARED_OUTPUT_NAME_RULE))
    }

    /// Returns `true` if the identifier is reserved for outputs a resources artifact declares.
    pub fn is_declared(&self) -> bool {
        self.declared_name().is_some()
    }

    /// Returns the name a resources artifact declared the output under, if the identifier is reserved for declared
    /// outputs: `team_d` for `resources_team_d`.
    pub fn declared_name(&self) -> Option<&str> {
        self.0
            .strip_prefix(DECLARED_OUTPUT_PREFIX)
            .filter(|name| !name.is_empty())
    }
}

const DECLARED_OUTPUT_NAME_RULE: &str = "declared output names are identifiers of at most 53 characters, so that \
                                         `resources_<name>` is an identifier";

identifier!(
    /// The identifier of a delivery host: a fixed component that multiplexes recipients, such as `dd_out`.
    ///
    /// Output bindings, recipients, and host settings name the delivery host they run in. Each delivery host has one
    /// encode host, which runs the leases of the outputs bound to it.
    HostId,
    "A" "delivery host ID"
);

identifier!(
    /// The name of a role an output plays for its delivery host, such as `main` or `multi_region_failover`.
    ///
    /// A role delivers to whatever native configuration resolves for it, so an output selected by its role keeps its
    /// fingerprint when its recipients change.
    RoleName,
    "A" "role name"
);

identifier!(
    /// The name of a scope: a replaceable region of a plan.
    ScopeName,
    "A" "scope name"
);

identifier!(
    /// The name of one of a scope's inputs.
    InputName,
    "A" "scope input name"
);

identifier!(
    /// The name of an operator parameter.
    ParamName,
    "A" "parameter name"
);

identifier!(
    /// The name of a host-published signal, such as a queue's occupancy.
    SignalName,
    "A" "signal name"
);

identifier!(
    /// The name of one of a scope's exits.
    ExitName,
    "A" "scope exit name"
);

identifier!(
    /// The local identifier of a node in a policy artifact.
    ///
    /// This is a [`LocalId`] other than [`SCOPE_INPUT_NODE`] and [`SCOPE_EXIT_NODE`], which artifact edges reserve for
    /// the scope's inputs and exits.
    ArtifactNodeId,
    "An" "artifact node ID",
    not_scope_boundary
);

impl ArtifactNodeId {
    /// Returns this identifier as a [`LocalId`].
    pub fn to_local_id(&self) -> LocalId {
        LocalId(self.0.clone())
    }
}

/// The namespace a node identifier belongs to.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Namespace {
    /// Identifiers assigned by lowering native configuration. They derive from native component identifiers.
    Reserved,

    /// Identifiers of nodes added by a policy artifact, which can therefore never collide with reserved ones.
    Policy,
}

const POLICY_PREFIX: &str = "policy/";
const NODE_ID_PATTERN: &str = "^(?:policy/)?[a-z](?:[a-z0-9]|_(?=[a-z0-9])){0,62}$";

/// The identifier of a node: a namespace and a local identifier.
///
/// Serialized as the local identifier alone for the reserved namespace, and as `policy/<local>` for the policy
/// namespace.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct NodeId {
    namespace: Namespace,
    local: LocalId,
}

impl NodeId {
    /// Creates a node identifier in the reserved namespace.
    pub fn reserved(local: LocalId) -> Self {
        Self {
            namespace: Namespace::Reserved,
            local,
        }
    }

    /// Creates a node identifier in the policy namespace.
    pub fn policy(local: LocalId) -> Self {
        Self {
            namespace: Namespace::Policy,
            local,
        }
    }

    /// Returns the namespace.
    pub fn namespace(&self) -> Namespace {
        self.namespace
    }

    /// Returns the local identifier.
    pub fn local(&self) -> &LocalId {
        &self.local
    }

    /// Returns `true` if this identifier is in the reserved namespace.
    pub fn is_reserved(&self) -> bool {
        self.namespace == Namespace::Reserved
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.namespace {
            Namespace::Reserved => write!(f, "{}", self.local),
            Namespace::Policy => write!(f, "{}{}", POLICY_PREFIX, self.local),
        }
    }
}

impl FromStr for NodeId {
    type Err = IdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (namespace, local) = match value.strip_prefix(POLICY_PREFIX) {
            Some(local) => (Namespace::Policy, local),
            None => (Namespace::Reserved, value),
        };
        if !is_identifier(local) {
            return Err(invalid(
                "node ID",
                value,
                "node IDs are an identifier, optionally prefixed with `policy/`; identifiers start with a lowercase \
                 ASCII letter and contain only lowercase letters, digits, and single underscores between them, up to \
                 63 characters",
            ));
        }
        Ok(Self {
            namespace,
            local: LocalId(local.to_string()),
        })
    }
}

string_serde!(
    NodeId,
    NODE_ID_PATTERN,
    (POLICY_PREFIX.len() + MAX_IDENTIFIER_LEN) as u32,
    "A node ID: an identifier, prefixed with `policy/` for nodes in the policy namespace."
);

const PORT_NAME_PATTERN: &str = "^(?:[a-z](?:[a-z0-9]|_(?=[a-z0-9])){0,62}|0|[1-9][0-9]{0,3})$";

/// The name of a port: an identifier such as `true` or `passthrough`, or a decimal index such as `0`.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PortName(String);

impl PortName {
    /// The largest port index.
    pub const MAX_INDEX: u16 = 9999;

    /// Parses a port name.
    ///
    /// # Errors
    ///
    /// Returns an error if `value` is neither an identifier nor a decimal index from 0 to 9999 without leading zeros.
    pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
        let value = value.into();
        let is_index = !value.is_empty()
            && value.len() <= 4
            && value.bytes().all(|b| b.is_ascii_digit())
            && (value == "0" || !value.starts_with('0'));
        if is_index || is_identifier(&value) {
            Ok(Self(value))
        } else {
            Err(invalid("port name", &value, PORT_NAME_RULE))
        }
    }

    /// Creates a port name from a name known to be valid, such as an operator's fixed port names.
    pub(crate) fn from_static(name: &'static str) -> Self {
        debug_assert!(Self::new(name).is_ok(), "`{}` is a valid port name", name);
        Self(name.to_string())
    }

    /// Creates the port name for an index, such as one of the numbered outputs of a `tee`.
    ///
    /// Returns `None` if `index` is larger than [`MAX_INDEX`][Self::MAX_INDEX].
    pub fn index(index: u16) -> Option<Self> {
        (index <= Self::MAX_INDEX).then(|| Self(index.to_string()))
    }

    /// Returns the index this port name denotes, if it is a decimal index.
    pub fn as_index(&self) -> Option<u16> {
        if self.0.as_bytes()[0].is_ascii_digit() {
            self.0.parse().ok()
        } else {
            None
        }
    }

    /// Returns the port name as a string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PortName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for PortName {
    type Err = IdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

string_serde!(
    PortName,
    PORT_NAME_PATTERN,
    63,
    "A port name: an identifier, or a decimal index."
);

const PORT_REF_PATTERN: &str =
    "^(?:policy/)?[a-z](?:[a-z0-9]|_(?=[a-z0-9])){0,62}\\.(?:[a-z](?:[a-z0-9]|_(?=[a-z0-9])){0,62}|0|[1-9][0-9]{0,3})$";

/// A reference to one port of one node, serialized as `<node>.<port>`.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PortRef {
    /// The node.
    pub node: NodeId,

    /// The port on the node.
    pub port: PortName,
}

impl PortRef {
    /// Creates a port reference.
    pub fn new(node: NodeId, port: PortName) -> Self {
        Self { node, port }
    }
}

impl fmt::Display for PortRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.node, self.port)
    }
}

impl FromStr for PortRef {
    type Err = IdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (node, port) = value.split_once('.').ok_or_else(|| {
            invalid(
                "port reference",
                value,
                "port references are a node ID and a port name separated by a period, such as `my_split.true`",
            )
        })?;
        Ok(Self {
            node: node.parse()?,
            port: port.parse()?,
        })
    }
}

string_serde!(
    PortRef,
    PORT_REF_PATTERN,
    (POLICY_PREFIX.len() + MAX_IDENTIFIER_LEN + 1 + MAX_IDENTIFIER_LEN) as u32,
    "A port reference: `<node>.<port>`."
);

const ARTIFACT_PORT_REF_PATTERN: &str =
    "^[a-z](?:[a-z0-9]|_(?=[a-z0-9])){0,62}\\.(?:[a-z](?:[a-z0-9]|_(?=[a-z0-9])){0,62}|0|[1-9][0-9]{0,3})$";

/// A reference to a port within a policy artifact, serialized as `input.<name>`, `exit.<name>`, or `<node>.<port>`.
///
/// Artifact nodes are named by local identifiers, because composing the artifact into a plan decides their namespace.
/// A scope's inputs and exits are named by the scope: an edge leaves a scope input, as `input.metrics`, and an edge
/// that leaves the scope before the end of its paths enters one of its exits, as `exit.aggregated`. Which end of an
/// edge each may be is checked when an artifact is read ([`PolicyArtifact::from_value`][super::PolicyArtifact]), and
/// whether the scope declares the name when the artifact is composed into a plan.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ArtifactPortRef {
    /// One of the scope's named inputs.
    Input(InputName),

    /// One of the scope's named exits: where an edge leaves the scope. Only an edge's target can be an exit.
    Exit(ExitName),

    /// A port of one of the artifact's nodes.
    Node {
        /// The node.
        node: ArtifactNodeId,

        /// The port on the node.
        port: PortName,
    },
}

impl fmt::Display for ArtifactPortRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Input(name) => write!(f, "{}.{}", SCOPE_INPUT_NODE, name),
            Self::Exit(name) => write!(f, "{}.{}", SCOPE_EXIT_NODE, name),
            Self::Node { node, port } => write!(f, "{}.{}", node, port),
        }
    }
}

impl FromStr for ArtifactPortRef {
    type Err = IdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (node, port) = value.split_once('.').ok_or_else(|| {
            invalid(
                "artifact port reference",
                value,
                "artifact port references are `input.<name>`, `exit.<name>`, or a node ID and a port name separated by \
                 a period",
            )
        })?;
        if node == SCOPE_INPUT_NODE {
            Ok(Self::Input(port.parse()?))
        } else if node == SCOPE_EXIT_NODE {
            Ok(Self::Exit(port.parse()?))
        } else {
            Ok(Self::Node {
                node: node.parse()?,
                port: port.parse()?,
            })
        }
    }
}

string_serde!(
    ArtifactPortRef,
    ARTIFACT_PORT_REF_PATTERN,
    (MAX_IDENTIFIER_LEN + 1 + MAX_IDENTIFIER_LEN) as u32,
    "A port reference within a policy artifact: `input.<name>` (a scope input, only as an edge's source), \
     `exit.<name>` (a scope exit, only as an edge's target), or `<node>.<port>`."
);

const BINDING_NAME_PATTERN: &str = "^[a-z0-9_]+(?:\\.[a-z0-9_]+)*$";
const MAX_BINDING_NAME_LEN: usize = 255;

/// The name of a configuration binding, such as `dogstatsd.metric_filter.values`.
///
/// A binding names a value that lowering resolves from configuration. Binding names are paths in the typed
/// configuration model, never raw configuration keys.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BindingName(String);

impl BindingName {
    /// Parses a binding name.
    ///
    /// # Errors
    ///
    /// Returns an error if `value` is not one or more segments of lowercase ASCII letters, digits, and underscores,
    /// separated by periods, of at most 255 bytes in total.
    pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
        let value = value.into();
        let valid = value.len() <= MAX_BINDING_NAME_LEN
            && value.split('.').all(|segment| {
                !segment.is_empty()
                    && segment
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            });
        if valid {
            Ok(Self(value))
        } else {
            Err(invalid("binding name", &value, BINDING_NAME_RULE))
        }
    }

    /// Returns the binding name as a string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BindingName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for BindingName {
    type Err = IdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

string_serde!(
    BindingName,
    BINDING_NAME_PATTERN,
    MAX_BINDING_NAME_LEN as u32,
    "The name of a configuration binding: period-separated segments of lowercase letters, digits, and underscores."
);

const OPAQUE_REFERENCE_PATTERN: &str = "^[!-~]{1,512}$";
const MAX_OPAQUE_REFERENCE_LEN: usize = 512;

fn is_opaque_reference(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_OPAQUE_REFERENCE_LEN && value.bytes().all(|b| b.is_ascii_graphic())
}

/// Defines a string newtype whose values are opaque references: printable, non-space ASCII.
macro_rules! opaque_reference {
    ($(#[$meta:meta])* $ty:ident, $what:literal) => {
        $(#[$meta])*
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $ty(String);

        impl $ty {
            /// Parses a reference.
            ///
            /// # Errors
            ///
            /// Returns an error if `value` is empty, longer than 512 bytes, or contains anything other than printable,
            /// non-space ASCII characters.
            pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
                let value = value.into();
                if is_opaque_reference(&value) {
                    Ok(Self(value))
                } else {
                    Err(invalid($what, &value, OPAQUE_REFERENCE_RULE))
                }
            }

            /// Returns the reference as a string.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl FromStr for $ty {
            type Err = IdError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }

        string_serde!($ty, OPAQUE_REFERENCE_PATTERN, MAX_OPAQUE_REFERENCE_LEN as u32, concat!("A ", $what, "."));
    };
}

opaque_reference!(
    /// The identity of a physical recipient of an output, such as one endpoint of a forwarder.
    ///
    /// Outputs that share a recipient feed the same delivery queue, so the duplicate analysis treats them as one.
    RecipientId,
    "recipient ID"
);

opaque_reference!(
    /// A reference to a credential, such as an API key, by name. Plans never contain credential values.
    CredentialRef,
    "credential reference"
);

const NATIVE_COMPONENT_ID_PATTERN: &str = "^[A-Za-z0-9](?:[A-Za-z0-9_]*[A-Za-z0-9])?$";

/// The identifier of a native component, as used in node provenance.
///
/// Native component identifiers contain only ASCII letters, digits, and underscores, and start and end with a letter or
/// digit.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct NativeComponentId(String);

impl NativeComponentId {
    /// Parses a native component identifier.
    ///
    /// # Errors
    ///
    /// Returns an error if `value` is not a valid component identifier.
    pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
        let value = value.into();
        match ComponentId::try_from(value.as_str()) {
            Ok(_) => Ok(Self(value)),
            Err(rule) => Err(invalid("native component ID", &value, rule)),
        }
    }

    /// Returns the identifier as a string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for NativeComponentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for NativeComponentId {
    type Err = IdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

string_serde!(
    NativeComponentId,
    NATIVE_COMPONENT_ID_PATTERN,
    1024,
    "The identifier of a native component."
);

/// Returns the identifier of the inline terminal node attached to `port` of `parent`.
///
/// Surface syntax lets a port lead directly to a terminal, as in `true -> emit my_output` or `false -> drop`, without
/// naming the terminal node. Every producer of plans derives those names with this one function, so the same plan
/// always gets the same node identifiers. The result is in the parent's namespace.
///
/// The identifier is `<parent>_<port>`. When that would be longer than an identifier may be, it is shortened to a
/// prefix of it followed by `_` and eight hexadecimal digits of a SHA-256 digest of the full `<parent>_<port>`, so
/// distinct long names stay distinct. Because the identifier contains an underscore, it never equals
/// [`SCOPE_INPUT_NODE`]. It can still collide with a node an author named the same way, which validation reports as a
/// duplicate node ID.
pub fn derived_terminal_id(parent: &NodeId, port: &PortName) -> NodeId {
    const DIGEST_LEN: usize = 8;

    let full = format!("{}_{}", parent.local(), port);
    let local = if full.len() <= MAX_IDENTIFIER_LEN {
        full
    } else {
        let digest = saluki_tls::sha256(full.as_bytes());
        let mut hex = String::with_capacity(DIGEST_LEN);
        for byte in &digest[..DIGEST_LEN / 2] {
            hex.push_str(&format!("{:02x}", byte));
        }

        // Every byte of `full` is ASCII, so any length is a character boundary.
        let mut prefix = &full[..MAX_IDENTIFIER_LEN - 1 - DIGEST_LEN];
        prefix = prefix.trim_end_matches('_');
        format!("{}_{}", prefix, hex)
    };

    debug_assert!(is_identifier(&local), "derived terminal IDs are identifiers: {}", local);
    NodeId {
        namespace: parent.namespace(),
        local: LocalId(local),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifier_grammar() {
        let at_limit = format!("a{}", "b".repeat(MAX_IDENTIFIER_LEN - 1));
        let over_limit = format!("a{}", "b".repeat(MAX_IDENTIFIER_LEN));
        // (value, valid)
        let cases: &[(&str, bool)] = &[
            ("a", true),
            ("dsd_in", true),
            ("metrics_routing_filter_0", true),
            ("a1_b2_c3", true),
            ("x9", true),
            (&at_limit, true),
            // Too long.
            (&over_limit, false),
            // Must start with a lowercase letter.
            ("", false),
            ("0a", false),
            ("_a", false),
            ("Abc", false),
            // Underscores only between alphanumerics.
            ("a_", false),
            ("a__b", false),
            ("a_b_", false),
            // Nothing else is allowed.
            ("a-b", false),
            ("a.b", false),
            ("a/b", false),
            ("aB", false),
            ("a b", false),
            ("é", false),
        ];

        for (value, valid) in cases {
            assert_eq!(is_identifier(value), *valid, "value: {:?}", value);
            assert_eq!(LocalId::new(*value).is_ok(), *valid, "value: {:?}", value);
        }
    }

    #[test]
    fn node_ids_parse_their_namespace() {
        // (serialized, namespace, local); `None` means invalid.
        let cases: &[(&str, Option<(Namespace, &str)>)] = &[
            ("dsd_in", Some((Namespace::Reserved, "dsd_in"))),
            ("policy/my_filter", Some((Namespace::Policy, "my_filter"))),
            ("policy/policy", Some((Namespace::Policy, "policy"))),
            ("policy", Some((Namespace::Reserved, "policy"))),
            // Only the policy namespace has a prefix, and the local part must still be an identifier.
            ("reserved/dsd_in", None),
            ("policy/", None),
            ("policy/Bad", None),
            ("policy/policy/a", None),
            ("Policy/a", None),
            ("/a", None),
            ("a_", None),
        ];

        for (serialized, expected) in cases {
            let parsed = serialized.parse::<NodeId>();
            match expected {
                Some((namespace, local)) => {
                    let id = parsed.unwrap_or_else(|e| panic!("{:?} should parse: {}", serialized, e));
                    assert_eq!(id.namespace(), *namespace, "{:?}", serialized);
                    assert_eq!(id.local().as_str(), *local, "{:?}", serialized);
                    assert_eq!(id.to_string(), *serialized, "round trip of {:?}", serialized);
                }
                None => assert!(parsed.is_err(), "{:?} should be rejected", serialized),
            }
        }
    }

    #[test]
    fn node_ids_serialize_as_strings() {
        let id = NodeId::policy(LocalId::new("drop_debug").unwrap());
        assert_eq!(
            serde_json::to_value(&id).unwrap(),
            serde_json::json!("policy/drop_debug")
        );
        assert_eq!(
            serde_json::from_value::<NodeId>(serde_json::json!("policy/drop_debug")).unwrap(),
            id
        );

        let error = serde_json::from_value::<NodeId>(serde_json::json!("Drop")).unwrap_err();
        assert!(error.to_string().contains("Invalid node ID `Drop`"), "{}", error);
        assert!(serde_json::from_value::<NodeId>(serde_json::json!(5)).is_err());
    }

    #[test]
    fn port_names_are_identifiers_or_indices() {
        let cases: &[(&str, bool, Option<u16>)] = &[
            ("true", true, None),
            ("passthrough", true, None),
            ("0", true, Some(0)),
            ("7", true, Some(7)),
            ("9999", true, Some(9999)),
            ("10000", false, None),
            ("01", false, None),
            ("", false, None),
            ("-1", false, None),
            ("in_", false, None),
        ];
        for (value, valid, index) in cases {
            match PortName::new(*value) {
                Ok(name) => {
                    assert!(*valid, "{:?} should be rejected", value);
                    assert_eq!(name.as_index(), *index, "{:?}", value);
                }
                Err(_) => assert!(!*valid, "{:?} should parse", value),
            }
        }
        assert_eq!(PortName::index(3).unwrap().as_str(), "3");
        assert!(PortName::index(10_000).is_none());
    }

    #[test]
    fn port_refs_split_at_the_first_period() {
        let port = "policy/routing.true".parse::<PortRef>().unwrap();
        assert_eq!(port.node, NodeId::policy(LocalId::new("routing").unwrap()));
        assert_eq!(port.port.as_str(), "true");
        assert_eq!(port.to_string(), "policy/routing.true");

        let port = "metrics_policy.0".parse::<PortRef>().unwrap();
        assert_eq!(port.port.as_index(), Some(0));

        for invalid in ["no_port", "a.b.c", ".x", "a.", "A.b", "a.B"] {
            assert!(invalid.parse::<PortRef>().is_err(), "{:?} should be rejected", invalid);
        }
    }

    #[test]
    fn artifact_port_refs_distinguish_scope_inputs() {
        assert_eq!(
            "input.metrics".parse::<ArtifactPortRef>().unwrap(),
            ArtifactPortRef::Input(InputName::new("metrics").unwrap())
        );
        assert_eq!(
            "drop_debug.in".parse::<ArtifactPortRef>().unwrap(),
            ArtifactPortRef::Node {
                node: ArtifactNodeId::new("drop_debug").unwrap(),
                port: PortName::new("in").unwrap(),
            }
        );
        // Artifact nodes are local: a namespace prefix is not allowed.
        assert!("policy/x.in".parse::<ArtifactPortRef>().is_err());
        // The scope input is named, never numbered.
        assert!("input.0".parse::<ArtifactPortRef>().is_err());
        // An artifact node cannot be called `input`, nor `exit`, which edges to a scope's exits will use.
        let error = ArtifactNodeId::new("input").unwrap_err().to_string();
        assert!(error.contains("`input` is reserved"), "{}", error);
        let error = ArtifactNodeId::new("exit").unwrap_err().to_string();
        assert!(error.contains("`exit` is reserved"), "{}", error);
        // A scope's exits are named, as its inputs are, and print as they parse.
        let exit = "exit.aggregated".parse::<ArtifactPortRef>().unwrap();
        assert_eq!(exit, ArtifactPortRef::Exit(ExitName::new("aggregated").unwrap()));
        assert_eq!(exit.to_string(), "exit.aggregated");
        assert!("exit.0".parse::<ArtifactPortRef>().is_err());
        assert!("exit.Aggregated".parse::<ArtifactPortRef>().is_err());
        assert!(ArtifactNodeId::new("exits").is_ok());
        assert_eq!(
            "exits.in".parse::<ArtifactPortRef>().unwrap(),
            ArtifactPortRef::Node {
                node: ArtifactNodeId::new("exits").unwrap(),
                port: PortName::new("in").unwrap(),
            }
        );
    }

    #[test]
    fn binding_names_are_dotted_paths() {
        let cases: &[(&str, bool)] = &[
            ("dogstatsd.metric_filter.values", true),
            ("endpoint_routing.group.0a1b2c.names", true),
            ("single", true),
            ("", false),
            ("a..b", false),
            (".a", false),
            ("a.", false),
            ("Upper.case", false),
            ("dash-ed", false),
        ];
        for (value, valid) in cases {
            assert_eq!(BindingName::new(*value).is_ok(), *valid, "{:?}", value);
        }
        assert!(BindingName::new("a".repeat(256)).is_err());
    }

    #[test]
    fn opaque_references_are_printable_ascii() {
        assert!(RecipientId::new("dd_out/https://app.example.com/0").is_ok());
        assert!(CredentialRef::new("api_key.primary").is_ok());
        for invalid in ["", "has space", "tab\t", "naïve"] {
            assert!(RecipientId::new(invalid).is_err(), "{:?}", invalid);
        }
        assert!(RecipientId::new("x".repeat(513)).is_err());
    }

    #[test]
    fn native_component_ids_follow_component_rules() {
        assert!(NativeComponentId::new("dsd_in").is_ok());
        assert!(NativeComponentId::new("Mixed_Case9").is_ok());
        assert!(NativeComponentId::new("_bad").is_err());
        assert!(NativeComponentId::new("bad.dot").is_err());
    }

    #[test]
    fn derived_terminal_ids_join_parent_and_port() {
        let parent = NodeId::reserved(LocalId::new("metrics_routing_filter_0").unwrap());
        let id = derived_terminal_id(&parent, &PortName::new("true").unwrap());
        assert_eq!(id.to_string(), "metrics_routing_filter_0_true");

        let parent = NodeId::policy(LocalId::new("fanout").unwrap());
        let id = derived_terminal_id(&parent, &PortName::index(2).unwrap());
        assert_eq!(id.to_string(), "policy/fanout_2", "the parent's namespace is kept");
    }

    #[test]
    fn long_derived_terminal_ids_are_shortened_with_a_digest() {
        let long = format!("a{}", "b".repeat(MAX_IDENTIFIER_LEN - 1));
        let parent = NodeId::reserved(LocalId::new(long.clone()).unwrap());
        let on_true = derived_terminal_id(&parent, &PortName::new("true").unwrap());
        let on_false = derived_terminal_id(&parent, &PortName::new("false").unwrap());

        // Reserved IDs must not change between releases. The expected IDs were computed independently, with Python:
        // `full[:54] + "_" + hashlib.sha256(full.encode()).hexdigest()[:8]` for `full = "a" + "b" * 62 + "_true"`, and
        // likewise for `_false`.
        assert_eq!(on_true.to_string(), format!("a{}_3c6e0fb7", "b".repeat(53)));
        assert_eq!(on_false.to_string(), format!("a{}_08eae15a", "b".repeat(53)));
        // The digest covers the local identifier only, so the namespace never changes it.
        let policy = derived_terminal_id(
            &NodeId::policy(LocalId::new(long.clone()).unwrap()),
            &PortName::new("true").unwrap(),
        );
        assert_eq!(policy.to_string(), format!("policy/a{}_3c6e0fb7", "b".repeat(53)));

        for id in [&on_true, &on_false] {
            let local = id.local().as_str();
            assert_eq!(local.len(), MAX_IDENTIFIER_LEN);
            assert!(local.starts_with(&long[..54]), "{}", local);
            assert!(is_identifier(local));
        }
        assert_ne!(on_true, on_false, "the digest keeps distinct ports distinct");

        // Shortening never leaves a doubled underscore where the prefix is cut.
        let parent = NodeId::reserved(LocalId::new(format!("{}_{}", "a".repeat(53), "b".repeat(9))).unwrap());
        let id = derived_terminal_id(&parent, &PortName::new("true").unwrap());
        assert!(is_identifier(id.local().as_str()), "{}", id);
        assert!(id.local().as_str().starts_with(&format!("{}_", "a".repeat(53))));
        assert_eq!(id.local().as_str().len(), 53 + 1 + 8);
    }
}
