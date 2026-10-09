//! Effect contracts: what an operator does to the records that pass through it.

use std::time::Duration;

pub use saluki_core::execution::LossClass;

use crate::{
    extension::{AdmissionPolicy, SignalDecl},
    model::{FieldSet, Kind, KindSet},
    predicate::Predicate,
};

/// A value that may differ by the kind of the input record, such as the fields an operator writes.
///
/// Native operators often treat kinds differently: tag projection only rewrites counters and distributions, and
/// aggregation renames only histograms. Contracts state such differences per kind, so that rewrites and the duplicate
/// analysis can reason about each kind separately.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PerKind<T>([T; 6]);

impl<T> PerKind<T>
where
    T: Clone,
{
    /// Returns a value that is the same for every kind.
    pub fn uniform(value: T) -> Self {
        Self([
            value.clone(),
            value.clone(),
            value.clone(),
            value.clone(),
            value.clone(),
            value,
        ])
    }

    /// Returns this value with `value` for every kind in `kinds`.
    pub fn with(mut self, kinds: KindSet, value: T) -> Self {
        for kind in kinds.iter() {
            self.0[kind as usize] = value.clone();
        }
        self
    }

    /// Returns the value for `kind`.
    pub fn get(&self, kind: Kind) -> &T {
        &self.0[kind as usize]
    }

    /// Returns an iterator over every kind and its value, in kind declaration order.
    pub fn iter(&self) -> impl Iterator<Item = (Kind, &T)> {
        Kind::ALL.into_iter().map(move |kind| (kind, self.get(kind)))
    }
}

impl PerKind<FieldSet> {
    /// Returns the fields in the value of any kind in `kinds`.
    pub fn over(&self, kinds: KindSet) -> FieldSet {
        kinds
            .iter()
            .fold(FieldSet::EMPTY, |fields, kind| fields.union(*self.get(kind)))
    }

    /// Returns the fields in the value of any kind.
    pub fn any_kind(&self) -> FieldSet {
        self.over(KindSet::ALL)
    }
}

/// How many records an operator produces for each record it receives.
///
/// Records sent to several output ports declared as [copies][PortRelation::Copies] are counted once per port: each port
/// receives what the cardinality says. Records split across [point-disjoint][PortRelation::PointDisjoint] ports are
/// counted in total.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Cardinality {
    /// At most one record for each record received, unchanged apart from being kept or removed. A filter writes no
    /// field.
    Filter,

    /// Exactly one record for each record received.
    Map,

    /// At least one record for each record received, up to a declared maximum.
    ///
    /// Expansion is legitimate: the records it produces are new records, not duplicates of the input. `None` means no
    /// maximum is declared, which only opaque hosts may do.
    ///
    /// For a stateful operator, only the maximum applies: it bounds the records that one record received can
    /// contribute to downstream, such as the series one histogram expands into when aggregation flushes its bucket.
    /// The lower bound does not: a reducer merges the records of one key, loses some at capacity, and emits records of
    /// its own, such as idle counter zeros, on its own schedule.
    Expand {
        /// The most records produced for one record received.
        max: Option<u32>,
    },

    /// No output: the operator ends a path.
    Sink,

    /// No input: the operator is where records enter the plan.
    Source,
}

/// How the records on an operator's output ports relate to each other.
///
/// The relation only matters for operators with more than one output port.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PortRelation {
    /// Every port may receive the same records: a record's points can reach several ports.
    Copies,

    /// Each point of each record received reaches at most one port.
    PointDisjoint,
}

/// How an operator changes the context identity of the records it produces.
///
/// Undeclared identity is [`Opaque`][Self::Opaque].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Identity {
    /// Every output record has the context identity of its input record.
    Preserving,

    /// Distinct input contexts produce distinct output contexts.
    Injective {
        /// A predicate every output record satisfies, if one is declared.
        image: Option<Predicate>,
    },

    /// Distinct input contexts may produce the same output context.
    Quotient,

    /// Nothing is declared about output contexts.
    Opaque,
}

/// How an operator depends on time.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TimeContract {
    /// Whether the operator reads the admission time `t_a` of the batch it processes, for example to compare it with a
    /// deadline parameter.
    ///
    /// An operator never reads the time at which it runs: decisions that depend on host time use `t_a`, so fused and
    /// separate execution decide the same.
    pub reads_admission_time: bool,
}

/// What a stateful operator groups state by.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ReducerKey {
    /// Context identity: name, host, tags, and origin tags, compared by their 64-bit hash. Kind and metadata are not
    /// part of the key.
    Context,
}

/// How a stateful operator combines the records of one key.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Reducer {
    /// Native aggregation: counters sum, gauges keep the latest value, sets take the union, histograms keep their
    /// samples, and distributions merge their sketches, per bucket.
    NativeAggregation,
}

/// How a stateful operator assigns records to time buckets.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum StateTime {
    /// Buckets aligned to Unix time, `[k * width, (k + 1) * width)`, with untimestamped points assigned to the bucket
    /// containing their admission time, and closed once the state owner's watermark passes their end.
    ///
    /// This is the contract of aggregation as a kernel, from the stage that extracts it. Until then aggregation runs in
    /// its native host component, which assigns a point to the bucket containing the host's clock when its batch
    /// arrives, and closes buckets by that clock. The two differ only for points admitted within their queueing delay
    /// of a bucket boundary, as the specification's compatibility matrix records.
    Buckets {
        /// The bucket width, or `None` when it is bound to configuration and not yet resolved.
        width: Option<Duration>,
    },
}

impl StateTime {
    /// Returns `true` if every part of the time contract is known, so that two contracts can be compared.
    pub const fn is_known(&self) -> bool {
        match self {
            Self::Buckets { width } => width.is_some(),
        }
    }
}

/// The state contract of a keyed, stateful operator.
///
/// Whether two generations can share an instance of the state is decided by comparing these fields, which is how
/// [transitions][crate::extension::TransitionClass] are classified.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct KeyedState {
    /// What state is grouped by.
    pub key: ReducerKey,

    /// How the records of one key combine.
    pub reducer: Reducer,

    /// How records are assigned to time buckets.
    pub time: StateTime,

    /// Whether new keys enter state.
    pub admission: AdmissionPolicy,
}

/// The state an operator keeps between batches.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum StateContract {
    /// No state, or only internal caches whose presence is unobservable.
    None,

    /// State grouped by key.
    Keyed(KeyedState),

    /// State owned by a host component and not described.
    Opaque,
}

/// Whether an operator's output depends on the order in which it receives records.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum OrderContract {
    /// The output does not depend on order.
    Independent,

    /// The output can depend on order, as when a gauge keeps its latest value or the first metadata seen is kept.
    Dependent,
}

/// One way an operator can fail to deliver contributions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LossDecl {
    /// The class the loss is counted under.
    pub class: LossClass,

    /// What is lost, and when.
    pub description: &'static str,
}

/// Whether an operator can fail to deliver contributions it receives.
///
/// Every operator counts events it cannot process as `invalid` loss; that is never declared here. A lossy operator
/// with policy loss needs authority: its node must have provenance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Loss {
    /// Every contribution received is delivered.
    Lossless,

    /// Some contributions may be lost, in the declared ways.
    Lossy(Vec<LossDecl>),
}

impl Loss {
    /// Returns `true` if the operator can lose contributions.
    pub fn is_lossy(&self) -> bool {
        matches!(self, Self::Lossy(_))
    }

    /// Returns `true` if the operator can lose contributions of the given class.
    pub fn has_class(&self, class: LossClass) -> bool {
        match self {
            Self::Lossless => false,
            Self::Lossy(declared) => declared.iter().any(|loss| loss.class == class),
        }
    }
}

/// Where an operator runs: in synchronous work that can be fused with its neighbors, or in a host.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Boundary {
    /// A synchronous kernel that can be fused with its neighbors.
    Synchronous,

    /// An asynchronous host that performs I/O, such as a source. It is a fence.
    IoHost,

    /// A host component without an extracted kernel, which is treated as reading and writing everything. It is a
    /// fence.
    OpaqueHost,
}

impl Boundary {
    /// Returns `true` if the boundary is a fence that rewrites and fusion never cross.
    pub const fn is_fence(self) -> bool {
        !matches!(self, Self::Synchronous)
    }
}

/// The effect contract of one configured operator: everything validation and rewrites may assume about it.
///
/// Only trusted operator implementations supply contracts; a plan only chooses operators and their parameters. The
/// kinds an operator accepts and the kinds each output port produces are part of its [ports][super::Ports].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectContract {
    /// The fields the operator reads, per input kind. Operators never read configuration; it reaches them only as
    /// resolved parameters.
    pub reads: PerKind<FieldSet>,

    /// The fields the operator may change, per input kind.
    pub writes: PerKind<FieldSet>,

    /// How many records the operator produces for each record it receives.
    pub cardinality: Cardinality,

    /// How the records on the operator's output ports relate.
    pub ports: PortRelation,

    /// How the operator changes context identity, per input kind.
    pub identity: PerKind<Identity>,

    /// How the operator depends on time.
    pub time: TimeContract,

    /// The state the operator keeps.
    pub state: StateContract,

    /// Whether the operator's output depends on the order of its input.
    pub order: OrderContract,

    /// How the operator can lose contributions.
    pub loss: Loss,

    /// The host signals the operator reads. Reading any signal makes the operator dynamic, which excludes it from
    /// rewrites.
    pub signals: Vec<SignalDecl>,

    /// Where the operator runs.
    pub boundary: Boundary,
}

impl EffectContract {
    /// Returns the contract of an operator that reads and writes nothing and passes every record through unchanged,
    /// as a starting point for describing a synchronous operator.
    pub fn passthrough() -> Self {
        Self {
            reads: PerKind::uniform(FieldSet::EMPTY),
            writes: PerKind::uniform(FieldSet::EMPTY),
            cardinality: Cardinality::Map,
            ports: PortRelation::PointDisjoint,
            identity: PerKind::uniform(Identity::Preserving),
            time: TimeContract::default(),
            state: StateContract::None,
            order: OrderContract::Independent,
            loss: Loss::Lossless,
            signals: Vec::new(),
            boundary: Boundary::Synchronous,
        }
    }

    /// Returns `true` if the operator reads any host signal, which makes it dynamic.
    pub fn is_dynamic(&self) -> bool {
        !self.signals.is_empty()
    }

    /// Returns the rules of the contract model that this contract, with the given numbers of input and output ports,
    /// breaks.
    ///
    /// Every registered operator's contract satisfies these rules for every valid set of parameters:
    ///
    /// - a filter writes no field;
    /// - a source has no input port and at least one output port, and only a source has no input port;
    /// - a sink has no output port, and only a sink has no output port;
    /// - an expansion declares a maximum of at least one, unless the operator is an opaque host;
    /// - a lossy operator declares at least one way it loses contributions.
    pub fn violations(&self, input_ports: usize, output_ports: usize) -> Vec<&'static str> {
        let mut violations = Vec::new();
        let is_source = self.cardinality == Cardinality::Source;
        let is_sink = self.cardinality == Cardinality::Sink;

        if self.cardinality == Cardinality::Filter && !self.writes.any_kind().is_empty() {
            violations.push("a filter must not write any field");
        }
        if is_source != (input_ports == 0) {
            violations.push("exactly the sources have no input port");
        }
        if is_source && output_ports == 0 {
            violations.push("a source must have an output port");
        }
        if is_sink != (output_ports == 0) {
            violations.push("exactly the sinks have no output port");
        }
        match self.cardinality {
            Cardinality::Expand { max: Some(0) } => violations.push("an expansion's maximum must be at least one"),
            Cardinality::Expand { max: None } if self.boundary != Boundary::OpaqueHost => {
                violations.push("only an opaque host may expand without a declared maximum")
            }
            _ => {}
        }
        if matches!(&self.loss, Loss::Lossy(declared) if declared.is_empty()) {
            violations.push("a lossy operator must declare how it loses contributions");
        }
        violations
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Field;

    #[test]
    fn per_kind_values_override_only_the_given_kinds() {
        let sketches = KindSet::of(&[Kind::Counter, Kind::Distribution]);
        let tags = FieldSet::of(&[Field::Tags, Field::OriginTags]);
        let writes = PerKind::uniform(FieldSet::EMPTY).with(sketches, tags);

        assert_eq!(*writes.get(Kind::Counter), tags);
        assert_eq!(*writes.get(Kind::Distribution), tags);
        assert_eq!(*writes.get(Kind::Gauge), FieldSet::EMPTY);
        assert_eq!(writes.over(KindSet::of(&[Kind::Gauge, Kind::Rate])), FieldSet::EMPTY);
        assert_eq!(writes.over(KindSet::of(&[Kind::Gauge, Kind::Counter])), tags);
        assert_eq!(writes.any_kind(), tags);
        assert_eq!(
            writes
                .iter()
                .filter(|(_, fields)| !fields.is_empty())
                .map(|(kind, _)| kind)
                .collect::<Vec<_>>(),
            vec![Kind::Counter, Kind::Distribution]
        );
    }

    #[test]
    fn contract_rules_are_checked() {
        let filter = EffectContract {
            cardinality: Cardinality::Filter,
            ..EffectContract::passthrough()
        };
        assert!(filter.violations(1, 1).is_empty());

        let writing_filter = EffectContract {
            writes: PerKind::uniform(FieldSet::EMPTY).with(KindSet::of(&[Kind::Gauge]), FieldSet::of(&[Field::Name])),
            ..filter.clone()
        };
        assert_eq!(
            writing_filter.violations(1, 1),
            vec!["a filter must not write any field"]
        );

        // (contract, input ports, output ports, expected violations)
        let source = EffectContract {
            cardinality: Cardinality::Source,
            ..EffectContract::passthrough()
        };
        let sink = EffectContract {
            cardinality: Cardinality::Sink,
            ..EffectContract::passthrough()
        };
        let unbounded = EffectContract {
            cardinality: Cardinality::Expand { max: None },
            ..EffectContract::passthrough()
        };
        let cases: Vec<(EffectContract, usize, usize, Vec<&str>)> = vec![
            (source.clone(), 0, 1, vec![]),
            (source.clone(), 1, 1, vec!["exactly the sources have no input port"]),
            (
                source,
                0,
                0,
                vec![
                    "a source must have an output port",
                    "exactly the sinks have no output port",
                ],
            ),
            (sink.clone(), 1, 0, vec![]),
            (sink, 1, 1, vec!["exactly the sinks have no output port"]),
            (
                EffectContract::passthrough(),
                0,
                1,
                vec!["exactly the sources have no input port"],
            ),
            (
                EffectContract::passthrough(),
                1,
                0,
                vec!["exactly the sinks have no output port"],
            ),
            (
                unbounded.clone(),
                1,
                1,
                vec!["only an opaque host may expand without a declared maximum"],
            ),
            (
                EffectContract {
                    boundary: Boundary::OpaqueHost,
                    ..unbounded
                },
                1,
                1,
                vec![],
            ),
            (
                EffectContract {
                    cardinality: Cardinality::Expand { max: Some(0) },
                    ..EffectContract::passthrough()
                },
                1,
                1,
                vec!["an expansion's maximum must be at least one"],
            ),
            (
                EffectContract {
                    loss: Loss::Lossy(vec![]),
                    ..EffectContract::passthrough()
                },
                1,
                1,
                vec!["a lossy operator must declare how it loses contributions"],
            ),
        ];
        for (contract, inputs, outputs, expected) in cases {
            assert_eq!(contract.violations(inputs, outputs), expected, "{:?}", contract);
        }
    }

    #[test]
    fn loss_classes_are_queried_by_class() {
        let loss = Loss::Lossy(vec![LossDecl {
            class: LossClass::Capacity,
            description: "Drops new keys at the limit.",
        }]);
        assert!(loss.is_lossy());
        assert!(loss.has_class(LossClass::Capacity));
        assert!(!loss.has_class(LossClass::Policy));
        assert!(!Loss::Lossless.is_lossy());
        assert!(!Loss::Lossless.has_class(LossClass::Policy));
    }

    #[test]
    fn only_synchronous_operators_can_be_fused() {
        assert!(!Boundary::Synchronous.is_fence());
        assert!(Boundary::IoHost.is_fence());
        assert!(Boundary::OpaqueHost.is_fence());
    }
}
