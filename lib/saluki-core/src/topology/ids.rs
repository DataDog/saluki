//! Component and component output identifiers.
use core::fmt;
use std::{borrow::Cow, ops::Deref};

use crate::{components::ComponentType, support::SubsystemIdentifier, topology::graph::DataType};

const INVALID_COMPONENT_ID: &str = "component IDs may only contain alphanumerics (a-z, A-Z, or 0-9) and underscores, \
     and must start and end with an alphanumeric character";
const INVALID_COMPONENT_OUTPUT_ID: &str =
    "component output IDs may only contain alphanumerics (a-z, A-Z, or 0-9), underscores, and up to one period \
     separator, where each side of the separator must start and end with an alphanumeric character";

/// A component identifier.
///
/// Component identifiers contain only alphanumerics and underscores, and must start and end with an alphanumeric
/// character.
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
    /// # Errors
    ///
    /// If generated component output ID isn't valid (identifier or output definition containing invalid characters,
    /// etc), an error is returned.
    pub fn from_definition<T: Copy>(
        component_id: ComponentId, output_def: &OutputDefinition<T>,
    ) -> Result<Self, (String, &'static str)> {
        match output_def.output_name() {
            None => Ok(Self(component_id.0)),
            Some(output_name) => {
                let output_id = format!("{}.{}", component_id.0, output_name);

                if validate_component_id(&output_id, true) {
                    Ok(Self(output_id.into()))
                } else {
                    Err((output_id, INVALID_COMPONENT_OUTPUT_ID))
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

    /// Returns `true` if this is a default output.
    pub fn is_default(&self) -> bool {
        self.0.split_once('.').is_none()
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

/// Validates a component ID, or -- when `as_output_id` is `true` -- a component output ID.
///
/// A component ID must be non-empty, contain only ASCII alphanumerics and underscores, and both start and end with an
/// alphanumeric character. A component output ID additionally permits a single period, which separates the component ID
/// from the output name; each side of the separator must independently satisfy the component ID rules.
const fn validate_component_id(id: &str, as_output_id: bool) -> bool {
    let id_bytes = id.as_bytes();
    let end = id_bytes.len();

    // Identifiers cannot be empty strings.
    if end == 0 {
        return false;
    }

    // Walk the string a byte at a time. Periods are only meaningful for output IDs, where a single period separates the
    // component ID from the output name; every other byte must be an alphanumeric or underscore. Each segment (the whole
    // string when there's no separator) is validated as it's closed out.
    let mut idx = 0;
    let mut segment_start = 0;
    let mut seen_separator = false;
    while idx < end {
        let b = id_bytes[idx];
        if b == b'.' {
            if !as_output_id || seen_separator {
                // We're not validating as an output ID, or we already saw a period separator: either way, invalid.
                return false;
            }
            seen_separator = true;

            // Close out and validate the segment that just ended.
            if !is_valid_component_id_segment(id_bytes, segment_start, idx) {
                return false;
            }
            segment_start = idx + 1;
        } else if !b.is_ascii_alphanumeric() && b != b'_' {
            // Anything other than an alphanumeric, underscore, or (handled above) period separator is invalid.
            return false;
        }

        idx += 1;
    }

    // Validate the final (or only) segment.
    is_valid_component_id_segment(id_bytes, segment_start, end)
}

/// Returns `true` if the byte range `[start, end)` of `bytes` is a valid component ID segment.
///
/// The caller guarantees the range contains only alphanumerics and underscores; this additionally requires the segment
/// to be non-empty and to start and end with an alphanumeric character (which rejects leading/trailing underscores as
/// well as empty segments arising from leading, trailing, or duplicate separators).
const fn is_valid_component_id_segment(bytes: &[u8], start: usize, end: usize) -> bool {
    // Segment cannot be empty.
    if start >= end {
        return false;
    }

    // Segments must start and end with an alphanumeric character.
    bytes[start].is_ascii_alphanumeric() && bytes[end - 1].is_ascii_alphanumeric()
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
pub struct OutputDefinition<T> {
    name: OutputName,
    data_ty: T,
}

impl<T> OutputDefinition<T>
where
    T: Copy,
{
    /// Creates a default output with the given data type.
    pub const fn default_output(data_ty: T) -> Self {
        Self {
            name: OutputName::Default,
            data_ty,
        }
    }

    /// Creates a named output with the given name and data type.
    pub fn named_output<S>(name: S, data_ty: T) -> Self
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
    pub fn data_ty(&self) -> T {
        self.data_ty
    }
}

/// A component identifier that specifies the component type.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TypedComponentId {
    id: ComponentId,
    ty: ComponentType,
}

impl TypedComponentId {
    /// Creates a new `TypedComponentId` from the given component ID and component type.
    pub fn new(id: ComponentId, ty: ComponentType) -> Self {
        Self { id, ty }
    }

    /// Returns a reference to the component ID.
    pub fn component_id(&self) -> &ComponentId {
        &self.id
    }

    /// Returns the component type.
    pub fn component_type(&self) -> ComponentType {
        self.ty
    }

    /// Consumes the `TypedComponentId` and returns its component ID and component type.
    pub fn into_parts(self) -> (ComponentId, ComponentType) {
        (self.id, self.ty)
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

/// Disambiguation marker for [`AsComponentIds`] when a single component ID is given.
pub struct Single;

/// Disambiguation marker for [`AsComponentIds`] when multiple component IDs are given.
pub struct Multiple;

/// Conversion into an iterator of component IDs.
///
/// Intended for use in methods that accept component IDs as string references, where a single or multiple IDs may be
/// passed within a single parameter. This allows being generic over those possibilities such that callers can use more
/// natural values rather than contrived values (such as always having to wrap a single string in a slice, etc).
pub trait AsComponentIds<Marker> {
    /// Converts `self` into an iterator of component output IDs.
    ///
    /// This borrows `self` -- rather than consuming it as the `into_` prefix would normally imply -- so that the
    /// iterator can be built multiple times from the same value, which is necessary for connecting every upstream ID
    /// to every downstream ID when making many-to-many connections.
    fn as_component_ids(&self) -> impl Iterator<Item: AsRef<str>>;
}

impl<T> AsComponentIds<Single> for T
where
    T: AsRef<str>,
{
    fn as_component_ids(&self) -> impl Iterator<Item: AsRef<str>> {
        std::iter::once(self)
    }
}

impl<I> AsComponentIds<Multiple> for I
where
    for<'a> &'a I: IntoIterator<Item: AsRef<str>>,
{
    fn as_component_ids(&self) -> impl Iterator<Item: AsRef<str>> {
        self.into_iter()
    }
}

pub(super) fn get_component_relative_identifier(
    component_type: ComponentType, component_id: &ComponentId,
) -> SubsystemIdentifier {
    SubsystemIdentifier::from_segments([component_type.as_category_str(), component_id])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn component_id() {
        let id = ComponentId::try_from("component").unwrap();
        assert_eq!(id, ComponentId::try_from("component").unwrap());
        assert_eq!(&*id, "component");

        let id = ComponentId::try_from("component_1").unwrap();
        assert_eq!(id, ComponentId::try_from("component_1").unwrap());
        assert_eq!(&*id, "component_1");
    }

    #[test]
    fn component_id_invalid() {
        assert!(ComponentId::try_from("").is_err());
        assert!(ComponentId::try_from("non_alphanumeric_$#!").is_err());
        assert!(ComponentId::try_from("cant_have_periods_for_non_component_output_id.foo").is_err());

        // Hyphens are rejected: they would sanitize to underscores, so `foo-bar` and `foo_bar` would otherwise collide
        // on the same canonical identity.
        assert!(ComponentId::try_from("dsd-in").is_err());
        assert!(ComponentId::try_from("foo-bar").is_err());

        // Leading/trailing underscores are rejected: they are trimmed during sanitization, so `_foo`/`foo_` would
        // otherwise collide with `foo`.
        assert!(ComponentId::try_from("_foo").is_err());
        assert!(ComponentId::try_from("foo_").is_err());
        assert!(ComponentId::try_from("--").is_err());
    }

    #[test]
    fn component_id_is_sanitization_fixed_point() {
        // A ComponentId must be valid if and only if it is already in canonical form -- that is, sanitizing it via
        // `get_sanitized_name` is a no-op. This is what guarantees distinct component IDs can never collapse to the same
        // canonical identity across subsystems. If the ComponentId rules and the sanitizer ever drift apart, this test
        // fails.
        use crate::runtime::get_sanitized_name;

        let cases = [
            "component",
            "component_1",
            "foo__bar",
            "a",
            "0",
            "dsd-mapper",
            "foo-bar",
            "_foo",
            "foo_",
            "_foo_",
            "--",
            "",
            "foo.bar",
            "foo bar",
            "foo$bar",
        ];

        for case in cases {
            let is_valid = ComponentId::try_from(case).is_ok();

            // The empty string is a trivial fixed point of the sanitizer but is not a valid ID, so we require
            // non-emptiness alongside the fixed-point property.
            let is_canonical = !case.is_empty() && &*get_sanitized_name(case) == case;

            assert_eq!(
                is_valid, is_canonical,
                "ComponentId validity must match the sanitization fixed point for {case:?}: valid={is_valid}, canonical={is_canonical}"
            );
        }
    }

    #[test]
    fn component_output_id_default() {
        let id = ComponentOutputId::try_from("component").unwrap();
        assert_eq!(id.component_id(), ComponentId::try_from("component").unwrap());
        assert_eq!(id.output(), OutputName::Default);
        assert!(id.is_default());
    }

    #[test]
    fn component_output_id_named() {
        let id = ComponentOutputId::try_from("component.metrics").unwrap();
        assert_eq!(id.component_id(), ComponentId::try_from("component").unwrap());
        assert_eq!(id.output(), OutputName::Given("metrics".into()));
        assert!(!id.is_default());
    }

    #[test]
    fn component_output_id_invalid() {
        assert!(ComponentOutputId::try_from("").is_err());
        assert!(ComponentOutputId::try_from("non_alphanumeric_$#!").is_err());
        assert!(ComponentOutputId::try_from("too.many.periods").is_err());
        assert!(ComponentOutputId::try_from(".one_side_of_named_output_is_empty").is_err());
        assert!(ComponentOutputId::try_from("one_side_of_named_output_is_empty.").is_err());
    }
}

#[cfg(test)]
mod property_tests {
    use proptest::prelude::*;

    use super::ComponentId;
    use crate::runtime::get_sanitized_name;

    proptest! {
        #[test]
        fn property_test_component_id_sanitized_name_equality_ascii(s in "[A-Za-z0-9_.\\- ]{0,16}") {
            // Anti-drift invariant: when a given ASCII string requires no sanitization (`get_sanitized_name(input) ==
            // input`), we should never fail to parse it as a valid `ComponentId`.
            //
            // This tries to ensure that if the parsing rules for `ComponentId` or `get_sanitized_name` change, we will
            // surface that through this test failing.
            let is_valid = ComponentId::try_from(s.as_str()).is_ok();
            let is_canonical = !s.is_empty() && &*get_sanitized_name(&s) == s.as_str();
            prop_assert_eq!(is_valid, is_canonical, "ComponentId validity must match canonical form for {:?}", s);
        }

        #[test]
        fn property_test_valid_component_id_always_canonical(s in ".{0,16}") {
            // Safety invariant: when a given string (any Unicode string) is accepted as a `ComponentId`, it must
            // already be canonical, so routing it through the sanitizer is a no-op and two distinct IDs can never
            // collapse onto the same identity.
            //
            // This is a superset of the ASCII-only validation: `get_sanitized_name` is able to sanitize non-ASCII names
            // into a canonical representation, but only ASCII names are accepted by `ComponentId`, so we only check for
            // canonical equality if the string was able to be parsed as a `ComponentId` in the first place.
            if ComponentId::try_from(s.as_str()).is_ok() {
                prop_assert!(!s.is_empty(), "an accepted ComponentId must be non-empty");
                prop_assert_eq!(&*get_sanitized_name(&s), s.as_str(), "an accepted ComponentId must already be canonical");
            }
        }
    }
}
