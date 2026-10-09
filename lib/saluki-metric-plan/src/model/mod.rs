//! The semantic model that plans are written against.
//!
//! A **record** is one metric: a context (name, host, tags, and origin tags) with one or more points of one kind. This
//! module names the parts of a record that operators read and write ([`Field`]), the read-only fields derived from
//! them ([`DerivedField`]), the kinds of metric a record can be ([`Kind`]), and the types that ports carry
//! ([`PortType`]). [`EventView`] is the borrowed view of a record that predicates evaluate against.

use std::fmt;

use saluki_core::data_model::event::metric::MetricValues;
use schemars::{
    gen::SchemaGenerator,
    schema::{ArrayValidation, InstanceType, Metadata, Schema, SchemaObject, SubschemaValidation},
    JsonSchema,
};
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};

mod view;
pub use self::view::EventView;

/// The kind of a metric.
///
/// Each kind corresponds to one variant of [`MetricValues`]. A rate's interval is part of its values, not of its kind:
/// predicates and port types never distinguish rates by interval.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A sum of deltas.
    Counter,

    /// A total change over an interval.
    Rate,

    /// A latest value.
    Gauge,

    /// A set of distinct members.
    Set,

    /// A collection of samples, aggregated on the client side.
    Histogram,

    /// A sketch of samples, mergeable on the server side.
    Distribution,
}

impl Kind {
    /// Every kind, in declaration order.
    pub const ALL: [Self; 6] = [
        Self::Counter,
        Self::Rate,
        Self::Gauge,
        Self::Set,
        Self::Histogram,
        Self::Distribution,
    ];

    /// Returns the kind of the given metric values.
    pub fn of(values: &MetricValues) -> Self {
        match values {
            MetricValues::Counter(_) => Self::Counter,
            MetricValues::Rate(_, _) => Self::Rate,
            MetricValues::Gauge(_) => Self::Gauge,
            MetricValues::Set(_) => Self::Set,
            MetricValues::Histogram(_) => Self::Histogram,
            MetricValues::Distribution(_) => Self::Distribution,
        }
    }

    /// Returns the name of this kind, as it is serialized.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::Rate => "rate",
            Self::Gauge => "gauge",
            Self::Set => "set",
            Self::Histogram => "histogram",
            Self::Distribution => "distribution",
        }
    }

    const fn bit(self) -> u8 {
        1 << (self as u8)
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A set of metric kinds.
///
/// Serialized as the string `"all"` when it contains every kind, and otherwise as a list of kinds in declaration order.
/// Either form is accepted when deserializing; a list may not repeat a kind.
#[derive(Clone, Copy, Default, Eq, Hash, PartialEq)]
pub struct KindSet(u8);

impl KindSet {
    /// The empty set.
    pub const EMPTY: Self = Self(0);

    /// The set of every kind.
    pub const ALL: Self = Self(0b11_1111);

    /// Creates a set from a list of kinds.
    pub const fn of(kinds: &[Kind]) -> Self {
        let mut bits = 0;
        let mut i = 0;
        while i < kinds.len() {
            bits |= kinds[i].bit();
            i += 1;
        }
        Self(bits)
    }

    /// Returns `true` if the set contains `kind`.
    pub const fn contains(self, kind: Kind) -> bool {
        self.0 & kind.bit() != 0
    }

    /// Returns this set with `kind` added.
    pub const fn with(self, kind: Kind) -> Self {
        Self(self.0 | kind.bit())
    }

    /// Returns the kinds in either set.
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Returns the kinds in both sets.
    pub const fn intersection(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    /// Returns the kinds in this set but not in `other`.
    pub const fn difference(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }

    /// Returns `true` if every kind in this set is also in `other`.
    pub const fn is_subset(self, other: Self) -> bool {
        self.0 & !other.0 == 0
    }

    /// Returns `true` if the set contains no kind.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Returns `true` if the set contains every kind.
    pub const fn is_all(self) -> bool {
        self.0 == Self::ALL.0
    }

    /// Returns the number of kinds in the set.
    pub const fn len(self) -> usize {
        self.0.count_ones() as usize
    }

    /// Returns an iterator over the kinds in the set, in declaration order.
    pub fn iter(self) -> impl Iterator<Item = Kind> {
        Kind::ALL.into_iter().filter(move |kind| self.contains(*kind))
    }
}

impl FromIterator<Kind> for KindSet {
    fn from_iter<I: IntoIterator<Item = Kind>>(iter: I) -> Self {
        iter.into_iter().fold(Self::EMPTY, Self::with)
    }
}

impl fmt::Debug for KindSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

impl Serialize for KindSet {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.is_all() {
            serializer.serialize_str("all")
        } else {
            serializer.collect_seq(self.iter())
        }
    }
}

impl<'de> Deserialize<'de> for KindSet {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            All(AllMarker),
            List(Vec<Kind>),
        }

        #[derive(Deserialize)]
        enum AllMarker {
            #[serde(rename = "all")]
            All,
        }

        match Repr::deserialize(deserializer).map_err(|_| {
            de::Error::custom(
                "expected `\"all\"` or a list of metric kinds (counter, rate, gauge, set, histogram, distribution)",
            )
        })? {
            Repr::All(AllMarker::All) => Ok(Self::ALL),
            Repr::List(kinds) => {
                let mut set = Self::EMPTY;
                for kind in kinds {
                    if set.contains(kind) {
                        return Err(de::Error::custom(format_args!(
                            "metric kind `{}` is listed more than once",
                            kind
                        )));
                    }
                    set = set.with(kind);
                }
                Ok(set)
            }
        }
    }
}

impl JsonSchema for KindSet {
    fn schema_name() -> String {
        "KindSet".to_string()
    }

    fn json_schema(gen: &mut SchemaGenerator) -> Schema {
        let all = SchemaObject {
            const_value: Some(serde_json::Value::String("all".to_string())),
            ..Default::default()
        };
        let list = SchemaObject {
            instance_type: Some(InstanceType::Array.into()),
            array: Some(Box::new(ArrayValidation {
                items: Some(gen.subschema_for::<Kind>().into()),
                unique_items: Some(true),
                ..Default::default()
            })),
            ..Default::default()
        };
        SchemaObject {
            metadata: Some(Box::new(Metadata {
                description: Some("A set of metric kinds: `\"all\"`, or a list of distinct kinds.".to_string()),
                ..Default::default()
            })),
            subschemas: Some(Box::new(SubschemaValidation {
                any_of: Some(vec![all.into(), list.into()]),
                ..Default::default()
            })),
            ..Default::default()
        }
        .into()
    }
}

/// Which points on a port carry a timestamp.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TimestampForm {
    /// Every point carries a timestamp.
    Timestamped,

    /// No point carries a timestamp.
    Untimestamped,

    /// Points may or may not carry a timestamp.
    Mixed,
}

impl TimestampForm {
    /// Returns the form of a port that carries points of both forms, as when two ports merge.
    pub const fn union(self, other: Self) -> Self {
        match (self, other) {
            (Self::Timestamped, Self::Timestamped) => Self::Timestamped,
            (Self::Untimestamped, Self::Untimestamped) => Self::Untimestamped,
            _ => Self::Mixed,
        }
    }

    /// Returns `true` if a port of this form can receive points of form `input`.
    pub const fn accepts(self, input: Self) -> bool {
        matches!(
            (self, input),
            (Self::Mixed, _) | (Self::Timestamped, Self::Timestamped) | (Self::Untimestamped, Self::Untimestamped)
        )
    }
}

/// The type of the records a port carries: their kinds and their timestamp form.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PortType {
    /// The kinds of metric the port carries.
    pub kinds: KindSet,

    /// Which of the port's points carry a timestamp.
    pub timestamps: TimestampForm,
}

impl PortType {
    /// The port type that carries anything: every kind, with or without timestamps.
    pub const ANY: Self = Self {
        kinds: KindSet::ALL,
        timestamps: TimestampForm::Mixed,
    };

    /// Returns `true` if a port of this type can receive records from a port of type `input`.
    ///
    /// Every kind `input` carries must be accepted, and so must its timestamp form. A port that carries no kind
    /// carries nothing, so every port can receive from it, whatever its timestamp form says.
    pub const fn accepts(&self, input: &Self) -> bool {
        input.kinds.is_empty() || (input.kinds.is_subset(self.kinds) && self.timestamps.accepts(input.timestamps))
    }

    /// Returns the type of a port that carries the records of both ports, as when two ports merge.
    ///
    /// A port that carries no kind contributes nothing, so the union is the other port's type.
    pub const fn union(&self, other: &Self) -> Self {
        if self.kinds.is_empty() {
            return *other;
        }
        if other.kinds.is_empty() {
            return *self;
        }
        Self {
            kinds: self.kinds.union(other.kinds),
            timestamps: self.timestamps.union(other.timestamps),
        }
    }
}

/// A stored field of a record, as operators read and write it.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Field {
    /// The raw metric name, exactly as it is forwarded.
    Name,

    /// The host, which may be unset, and which is distinct from an empty host.
    Host,

    /// The instrumented tags.
    Tags,

    /// The origin tags, which are separate from the instrumented tags.
    OriginTags,

    /// The metric kind, including a rate's interval.
    Kind,

    /// The values and their optional timestamps.
    Points,

    /// The metadata, such as unit and origin.
    Metadata,
}

impl Field {
    /// Every field, in declaration order.
    pub const ALL: [Self; 7] = [
        Self::Name,
        Self::Host,
        Self::Tags,
        Self::OriginTags,
        Self::Kind,
        Self::Points,
        Self::Metadata,
    ];

    /// Returns the name of this field, as it is serialized.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Host => "host",
            Self::Tags => "tags",
            Self::OriginTags => "origin_tags",
            Self::Kind => "kind",
            Self::Points => "points",
            Self::Metadata => "metadata",
        }
    }

    const fn bit(self) -> u8 {
        1 << (self as u8)
    }
}

impl fmt::Display for Field {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A read-only field computed from stored fields.
///
/// Reading a derived field counts as reading its [sources][Self::sources], so a write to any source invalidates it.
/// Derived fields are computed lazily, at most once per record, and only when something reads them.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DerivedField {
    /// The name as the intake stores it.
    ///
    /// A name the intake rejects -- one that is empty, longer than 350 bytes, or without an ASCII letter -- has no
    /// intake name. The intake name is the same for every output.
    IntakeName,
}

impl DerivedField {
    /// Returns the stored fields this field is computed from.
    pub const fn sources(self) -> FieldSet {
        match self {
            Self::IntakeName => FieldSet::of(&[Field::Name]),
        }
    }

    /// Returns the name of this field, as it is serialized.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::IntakeName => "intake_name",
        }
    }
}

/// A set of stored fields.
///
/// Serialized as a list of field names in declaration order. A list may not repeat a field.
#[derive(Clone, Copy, Default, Eq, Hash, PartialEq)]
pub struct FieldSet(u8);

impl FieldSet {
    /// The empty set.
    pub const EMPTY: Self = Self(0);

    /// The set of every field.
    pub const ALL: Self = Self(0b111_1111);

    /// The fields that make up a record's context, and therefore its identity: name, host, tags, and origin tags.
    pub const CONTEXT: Self = Self::of(&[Field::Name, Field::Host, Field::Tags, Field::OriginTags]);

    /// Creates a set from a list of fields.
    pub const fn of(fields: &[Field]) -> Self {
        let mut bits = 0;
        let mut i = 0;
        while i < fields.len() {
            bits |= fields[i].bit();
            i += 1;
        }
        Self(bits)
    }

    /// Returns `true` if the set contains `field`.
    pub const fn contains(self, field: Field) -> bool {
        self.0 & field.bit() != 0
    }

    /// Returns this set with `field` added.
    pub const fn with(self, field: Field) -> Self {
        Self(self.0 | field.bit())
    }

    /// Returns the fields in either set.
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Returns the fields in both sets.
    pub const fn intersection(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    /// Returns `true` if the two sets share a field.
    ///
    /// Rewrites use this to check that nothing between two points writes a field that a predicate reads.
    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    /// Returns `true` if every field in this set is also in `other`.
    pub const fn is_subset(self, other: Self) -> bool {
        self.0 & !other.0 == 0
    }

    /// Returns `true` if the set contains no field.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Returns an iterator over the fields in the set, in declaration order.
    pub fn iter(self) -> impl Iterator<Item = Field> {
        Field::ALL.into_iter().filter(move |field| self.contains(*field))
    }
}

impl FromIterator<Field> for FieldSet {
    fn from_iter<I: IntoIterator<Item = Field>>(iter: I) -> Self {
        iter.into_iter().fold(Self::EMPTY, Self::with)
    }
}

impl fmt::Debug for FieldSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

impl Serialize for FieldSet {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.iter())
    }
}

impl<'de> Deserialize<'de> for FieldSet {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut set = Self::EMPTY;
        for field in Vec::<Field>::deserialize(deserializer)? {
            if set.contains(field) {
                return Err(de::Error::custom(format_args!(
                    "field `{}` is listed more than once",
                    field
                )));
            }
            set = set.with(field);
        }
        Ok(set)
    }
}

impl JsonSchema for FieldSet {
    fn schema_name() -> String {
        "FieldSet".to_string()
    }

    fn json_schema(gen: &mut SchemaGenerator) -> Schema {
        SchemaObject {
            instance_type: Some(InstanceType::Array.into()),
            array: Some(Box::new(ArrayValidation {
                items: Some(gen.subschema_for::<Field>().into()),
                unique_items: Some(true),
                ..Default::default()
            })),
            ..Default::default()
        }
        .into()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use saluki_core::data_model::event::metric::{
        HistogramPoints, MetricValues, ScalarPoints, SetPoints, SketchPoints,
    };
    use serde_json::json;

    use super::*;

    #[test]
    fn every_metric_values_variant_maps_to_its_kind() {
        let cases = [
            (MetricValues::counter(1.0), Kind::Counter),
            (MetricValues::rate(1.0, Duration::from_secs(10)), Kind::Rate),
            // A zero interval is still a rate: kinds never look at the interval.
            (MetricValues::rate(ScalarPoints::from(1.0), Duration::ZERO), Kind::Rate),
            (MetricValues::gauge((1_700_000_000, 1.0)), Kind::Gauge),
            (MetricValues::set(SetPoints::from("member")), Kind::Set),
            (MetricValues::histogram(HistogramPoints::from(2.5)), Kind::Histogram),
            (MetricValues::distribution(SketchPoints::from(2.5)), Kind::Distribution),
        ];

        for (values, expected) in cases {
            assert_eq!(Kind::of(&values), expected, "values: {}", values);
            // The serialized kind name agrees with the native one.
            assert_eq!(expected.as_str(), values.as_str());
        }
    }

    #[test]
    fn kind_sets_serialize_as_all_or_a_list_in_declaration_order() {
        assert_eq!(serde_json::to_value(KindSet::ALL).unwrap(), json!("all"));
        assert_eq!(serde_json::to_value(KindSet::EMPTY).unwrap(), json!([]));
        assert_eq!(
            serde_json::to_value(KindSet::of(&[Kind::Set, Kind::Counter])).unwrap(),
            json!(["counter", "set"])
        );
        assert_eq!(
            serde_json::to_value(KindSet::ALL.difference(KindSet::of(&[Kind::Histogram]))).unwrap(),
            json!(["counter", "rate", "gauge", "set", "distribution"])
        );
    }

    #[test]
    fn kind_sets_deserialize_from_all_or_a_list() {
        let parse = |value: serde_json::Value| serde_json::from_value::<KindSet>(value);

        assert_eq!(parse(json!("all")).unwrap(), KindSet::ALL);
        assert_eq!(parse(json!([])).unwrap(), KindSet::EMPTY);
        assert_eq!(
            parse(json!(["gauge", "rate"])).unwrap(),
            KindSet::of(&[Kind::Rate, Kind::Gauge])
        );
        // Listing every kind is the same set as "all".
        assert_eq!(
            parse(json!(["counter", "rate", "gauge", "set", "histogram", "distribution"])).unwrap(),
            KindSet::ALL
        );

        let duplicate = parse(json!(["gauge", "gauge"])).unwrap_err().to_string();
        assert!(duplicate.contains("`gauge` is listed more than once"), "{}", duplicate);
        for invalid in [json!("everything"), json!(["sketch"]), json!(3)] {
            let error = parse(invalid.clone()).unwrap_err().to_string();
            assert!(
                error.contains("expected `\"all\"` or a list of metric kinds"),
                "{}: {}",
                invalid,
                error
            );
        }
    }

    #[test]
    fn kind_set_operations() {
        let scalars = KindSet::of(&[Kind::Counter, Kind::Rate, Kind::Gauge]);
        let counters = KindSet::of(&[Kind::Counter]);
        assert!(scalars.contains(Kind::Rate));
        assert!(!scalars.contains(Kind::Set));
        assert!(counters.is_subset(scalars));
        assert!(!scalars.is_subset(counters));
        assert_eq!(scalars.intersection(counters), counters);
        assert_eq!(scalars.difference(counters), KindSet::of(&[Kind::Rate, Kind::Gauge]));
        assert_eq!(scalars.union(KindSet::of(&[Kind::Set])).len(), 4);
        assert_eq!(
            scalars.iter().collect::<Vec<_>>(),
            vec![Kind::Counter, Kind::Rate, Kind::Gauge]
        );
        assert_eq!(Kind::ALL.into_iter().collect::<KindSet>(), KindSet::ALL);
        assert!(KindSet::EMPTY.is_empty() && KindSet::ALL.is_all() && !scalars.is_all());
    }

    #[test]
    fn port_types_accept_compatible_inputs() {
        let scalars = KindSet::of(&[Kind::Counter, Kind::Rate, Kind::Gauge]);
        let port = |kinds, timestamps| PortType { kinds, timestamps };

        // (accepting port, input, expected)
        let cases = [
            (PortType::ANY, port(scalars, TimestampForm::Timestamped), true),
            (PortType::ANY, PortType::ANY, true),
            (
                port(scalars, TimestampForm::Mixed),
                port(KindSet::of(&[Kind::Gauge]), TimestampForm::Untimestamped),
                true,
            ),
            // A kind the port does not accept.
            (
                port(scalars, TimestampForm::Mixed),
                port(KindSet::of(&[Kind::Set]), TimestampForm::Mixed),
                false,
            ),
            // A port that requires timestamps cannot take points that may lack them, and vice versa.
            (
                port(scalars, TimestampForm::Timestamped),
                port(scalars, TimestampForm::Mixed),
                false,
            ),
            (
                port(scalars, TimestampForm::Untimestamped),
                port(scalars, TimestampForm::Timestamped),
                false,
            ),
            (
                port(scalars, TimestampForm::Timestamped),
                port(scalars, TimestampForm::Timestamped),
                true,
            ),
            // An input that carries no kind carries nothing, so every port accepts it, whatever its timestamp form.
            (
                port(KindSet::EMPTY, TimestampForm::Untimestamped),
                port(KindSet::EMPTY, TimestampForm::Untimestamped),
                true,
            ),
            (
                port(scalars, TimestampForm::Untimestamped),
                port(KindSet::EMPTY, TimestampForm::Timestamped),
                true,
            ),
            (
                port(KindSet::EMPTY, TimestampForm::Timestamped),
                port(KindSet::EMPTY, TimestampForm::Mixed),
                true,
            ),
        ];

        for (accepting, input, expected) in cases {
            assert_eq!(
                accepting.accepts(&input),
                expected,
                "{:?} accepting {:?}",
                accepting,
                input
            );
        }
    }

    #[test]
    fn merged_port_types_widen() {
        let timestamped = PortType {
            kinds: KindSet::of(&[Kind::Rate]),
            timestamps: TimestampForm::Timestamped,
        };
        let untimestamped = PortType {
            kinds: KindSet::of(&[Kind::Gauge]),
            timestamps: TimestampForm::Untimestamped,
        };
        assert_eq!(
            timestamped.union(&untimestamped),
            PortType {
                kinds: KindSet::of(&[Kind::Rate, Kind::Gauge]),
                timestamps: TimestampForm::Mixed,
            }
        );
        assert_eq!(timestamped.union(&timestamped), timestamped);

        // A port that carries no kind adds no timestamp form: merging it changes nothing.
        let nothing_timestamped = PortType {
            kinds: KindSet::EMPTY,
            timestamps: TimestampForm::Timestamped,
        };
        assert_eq!(untimestamped.union(&nothing_timestamped), untimestamped);
        assert_eq!(nothing_timestamped.union(&untimestamped), untimestamped);
    }

    #[test]
    fn port_types_round_trip_through_json() {
        let port = PortType {
            kinds: KindSet::ALL,
            timestamps: TimestampForm::Mixed,
        };
        let value = json!({"kinds": "all", "timestamps": "mixed"});
        assert_eq!(serde_json::to_value(port).unwrap(), value);
        assert_eq!(serde_json::from_value::<PortType>(value).unwrap(), port);

        let unknown = serde_json::from_value::<PortType>(json!({"kinds": "all", "timestamps": "mixed", "x": 1}));
        assert!(unknown.unwrap_err().to_string().contains("unknown field `x`"));
    }

    #[test]
    fn field_sets_and_derived_field_sources() {
        assert_eq!(DerivedField::IntakeName.sources(), FieldSet::of(&[Field::Name]));
        assert!(DerivedField::IntakeName.sources().is_subset(FieldSet::CONTEXT));
        assert!(!FieldSet::CONTEXT.contains(Field::Kind));
        assert!(FieldSet::CONTEXT.intersects(FieldSet::of(&[Field::Tags, Field::Points])));
        assert!(!FieldSet::CONTEXT.intersects(FieldSet::of(&[Field::Points, Field::Metadata])));
        assert_eq!(Field::ALL.into_iter().collect::<FieldSet>(), FieldSet::ALL);

        let set = FieldSet::of(&[Field::OriginTags, Field::Name]);
        assert_eq!(serde_json::to_value(set).unwrap(), json!(["name", "origin_tags"]));
        assert_eq!(
            serde_json::from_value::<FieldSet>(json!(["origin_tags", "name"])).unwrap(),
            set
        );
        assert!(serde_json::from_value::<FieldSet>(json!(["name", "name"])).is_err());
    }
}
