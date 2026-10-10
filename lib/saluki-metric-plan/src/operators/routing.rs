//! Routing operators: `split`, `tee`, `merge`, `emit`, and `drop`.

use super::{
    contract::{Cardinality, EffectContract, Loss, LossClass, LossDecl, PerKind, PortRelation},
    names,
    ports::{InputPort, OutputPort, PortMapping, Ports, IN, OUT},
    OperatorDef,
};
use crate::{
    compile::MEMO_COST_FLOOR,
    ir::{DecodedParams, ParamSpec, ParamType, PortName},
    model::{Field, FieldSet, PortType},
    predicate::Predicate,
};

/// The largest number of ports a `tee` or `merge` can have: one per port index.
const MAX_PORTS: i64 = PortName::MAX_INDEX as i64 + 1;

/// Returns the numbered port names `0` to `count - 1`.
fn numbered_ports(count: i64) -> impl Iterator<Item = PortName> {
    (0..count.clamp(0, MAX_PORTS)).filter_map(|index| u16::try_from(index).ok().and_then(PortName::index))
}

/// The `split` operator: routes each record to its `true` port if a predicate matches it, and to its `false` port
/// otherwise.
///
/// Parameters: `predicate` (a [predicate](crate::predicate)). The ports carry the input's type, narrowed by the kinds
/// for which the predicate can be true or false.
#[derive(Clone, Copy, Debug, Default)]
pub struct Split;

/// The output port a `split` sends matching records to.
pub const TRUE_PORT: &str = "true";

/// The output port a `split` sends other records to.
pub const FALSE_PORT: &str = "false";

const SPLIT_PARAMS: &[ParamSpec] = &[ParamSpec::required("predicate", ParamType::Predicate)];

impl OperatorDef for Split {
    fn name(&self) -> &'static str {
        names::SPLIT
    }

    fn summary(&self) -> &'static str {
        "Routes each record to its `true` port if the predicate matches it, and to its `false` port otherwise."
    }

    fn params(&self) -> &'static [ParamSpec] {
        SPLIT_PARAMS
    }

    fn ports(&self, _params: &DecodedParams) -> Ports {
        Ports {
            inputs: vec![InputPort {
                name: PortName::from_static(IN),
                accepts: PortType::ANY,
            }],
            outputs: [TRUE_PORT, FALSE_PORT]
                .into_iter()
                .map(|name| OutputPort {
                    name: PortName::from_static(name),
                    mapping: PortMapping::IDENTITY,
                })
                .collect(),
        }
    }

    fn contract(&self, params: &DecodedParams) -> EffectContract {
        let reads = params
            .predicate("predicate")
            .map_or(FieldSet::ALL, |predicate| predicate.reads());
        EffectContract {
            reads: PerKind::uniform(reads),
            ..EffectContract::passthrough()
        }
    }

    fn output_types(&self, params: &DecodedParams, _ports: &Ports, inputs: &[PortType]) -> Vec<PortType> {
        let input = super::union_of(inputs);
        let narrowed = |value| PortType {
            kinds: params
                .predicate("predicate")
                .map_or(input.kinds, |predicate| predicate.kinds_where(value, input.kinds)),
            timestamps: input.timestamps,
        };
        vec![narrowed(true), narrowed(false)]
    }

    fn cost(&self, params: &DecodedParams) -> u32 {
        // Routing every event by its predicate, plus moving what leaves the buffer.
        params
            .predicate("predicate")
            .map_or(20, |predicate| evaluation_cost([predicate]))
            .saturating_add(ROUTE_COST)
    }
}

/// The estimated work, in nanoseconds per event, of moving the events a `split`, or a decision with `split`s, routes
/// out of its buffer.
const ROUTE_COST: u32 = 5;

/// The estimated work, in nanoseconds per event, of looking an event's exit set up in a decision's memo (R4): reading
/// the context's identity, one probe, and one comparison.
const MEMO_LOOKUP_COST: u32 = 10;

/// The estimated work, in nanoseconds per event, of normalizing a name into its intake name, which a predicate or a
/// decision does once per event however many `intake_name` leaves it has (see `EventView::intake_name`).
const INTAKE_NAME_COST: u32 = 10;

/// Returns R1's estimate of the work, in nanoseconds per event, of the `split`s of one decision, a group of `split`s
/// and `tee`s that a region runs as one function: routing by the exit set, plus evaluating the `predicates` with every
/// derived field computed once, or one memo lookup in place of that when regions memoize the decision (R4): when it is
/// memoizable (every predicate reads only context fields and the kind, as `Decision::memoizable` decides) and its
/// `split`s cost at least [`MEMO_COST_FLOOR`] together, each as it costs alone. The estimate is a memo hit's.
///
/// A decision without `split`s costs nothing here: its `tee`s' copies are their own estimates. Through stage 4, the
/// predicate forms read nothing but context fields and the kind, so every decision with a `split` is memoizable.
pub(crate) fn decision_cost(predicates: &[&Predicate]) -> u32 {
    if predicates.is_empty() {
        return 0;
    }
    let memoizable = predicates
        .iter()
        .all(|predicate| predicate.reads().is_subset(FieldSet::CONTEXT.with(Field::Kind)));
    let alone = predicates
        .iter()
        .map(|&predicate| evaluation_cost([predicate]).saturating_add(ROUTE_COST))
        .fold(0, u32::saturating_add);
    let decide = if memoizable && alone >= MEMO_COST_FLOOR {
        MEMO_LOOKUP_COST
    } else {
        evaluation_cost(predicates.iter().copied())
    };
    decide.saturating_add(ROUTE_COST)
}

/// Returns an estimate of the work, in nanoseconds per event, of evaluating predicates in full on one event, with the
/// intake name normalized once.
///
/// The estimates only rank a `split` against the kernels of its chain. A tag scan that finds no tag with its key costs
/// more than this says on records with many tags; R7, which orders the operands of junctions, ranks leaves by its own
/// static costs (see [`r7`][crate::compile::rewrite::r7]).
fn evaluation_cost<'a>(predicates: impl IntoIterator<Item = &'a Predicate>) -> u32 {
    let mut intake_name = false;
    predicates
        .into_iter()
        .map(|predicate| leaves_cost(predicate, &mut intake_name))
        .fold(0, u32::saturating_add)
}

/// Returns the estimate of evaluating every leaf of `predicate`, adding the intake name's normalization unless
/// `intake_name` says an earlier leaf has paid for it.
fn leaves_cost(predicate: &Predicate, intake_name: &mut bool) -> u32 {
    match predicate {
        Predicate::Const(_)
        | Predicate::Flag(_)
        | Predicate::IsEmpty(_)
        | Predicate::HostUnset {}
        | Predicate::KindIn(_) => 1,
        Predicate::Name(_) | Predicate::Host(_) => 5,
        // The intake name is normalized before it is matched, once per event.
        Predicate::IntakeName(_) => {
            let normalize = if std::mem::replace(intake_name, true) {
                0
            } else {
                INTAKE_NAME_COST
            };
            5 + normalize
        }
        Predicate::HasTag(_) | Predicate::Tag(_) | Predicate::HasOriginTag(_) | Predicate::OriginTag(_) => 10,
        Predicate::All(operands) | Predicate::Any(operands) => operands
            .iter()
            .map(|operand| leaves_cost(operand, intake_name))
            .fold(0, u32::saturating_add),
        Predicate::Not(operand) => leaves_cost(operand, intake_name),
    }
}

/// The `tee` operator: copies each record to every output port.
///
/// Parameters: `ports` (the number of output ports, from 1 to 10,000). The output ports are numbered from `0`.
#[derive(Clone, Copy, Debug, Default)]
pub struct Tee;

const TEE_PARAMS: &[ParamSpec] = &[ParamSpec::required(
    "ports",
    ParamType::Integer { min: 1, max: MAX_PORTS },
)];

impl OperatorDef for Tee {
    fn name(&self) -> &'static str {
        names::TEE
    }

    fn summary(&self) -> &'static str {
        "Copies each record to every output port."
    }

    fn params(&self) -> &'static [ParamSpec] {
        TEE_PARAMS
    }

    fn ports(&self, params: &DecodedParams) -> Ports {
        Ports {
            inputs: vec![InputPort {
                name: PortName::from_static(IN),
                accepts: PortType::ANY,
            }],
            outputs: numbered_ports(params.integer("ports").unwrap_or(0))
                .map(|name| OutputPort {
                    name,
                    mapping: PortMapping::IDENTITY,
                })
                .collect(),
        }
    }

    fn contract(&self, _params: &DecodedParams) -> EffectContract {
        EffectContract {
            ports: PortRelation::Copies,
            ..EffectContract::passthrough()
        }
    }

    fn cost(&self, params: &DecodedParams) -> u32 {
        // Every port but the last gets a copy of each event.
        let copies = params.integer("ports").unwrap_or(1).saturating_sub(1);
        u32::try_from(copies.saturating_mul(15)).unwrap_or(u32::MAX)
    }
}

/// The `merge` operator: joins several inputs into one output. The order in which inputs interleave is unspecified.
///
/// Parameters: `inputs` (the number of input ports, from 1 to 10,000). The input ports are numbered from `0`.
#[derive(Clone, Copy, Debug, Default)]
pub struct Merge;

const MERGE_PARAMS: &[ParamSpec] = &[ParamSpec::required(
    "inputs",
    ParamType::Integer { min: 1, max: MAX_PORTS },
)];

impl OperatorDef for Merge {
    fn name(&self) -> &'static str {
        names::MERGE
    }

    fn summary(&self) -> &'static str {
        "Joins several inputs into one output."
    }

    fn params(&self) -> &'static [ParamSpec] {
        MERGE_PARAMS
    }

    fn ports(&self, params: &DecodedParams) -> Ports {
        Ports {
            inputs: numbered_ports(params.integer("inputs").unwrap_or(0))
                .map(|name| InputPort {
                    name,
                    accepts: PortType::ANY,
                })
                .collect(),
            outputs: vec![OutputPort {
                name: PortName::from_static(OUT),
                mapping: PortMapping::IDENTITY,
            }],
        }
    }

    fn contract(&self, _params: &DecodedParams) -> EffectContract {
        EffectContract::passthrough()
    }
}

/// The `emit` operator: delivers every record to a declared output.
///
/// Parameters: `output` (the identifier of a declared output). Each `emit` receives its own records: operations on
/// other paths never change them.
#[derive(Clone, Copy, Debug, Default)]
pub struct Emit;

const EMIT_PARAMS: &[ParamSpec] = &[ParamSpec::required("output", ParamType::OutputRef)];

impl OperatorDef for Emit {
    fn name(&self) -> &'static str {
        names::EMIT
    }

    fn summary(&self) -> &'static str {
        "Delivers every record to a declared output."
    }

    fn params(&self) -> &'static [ParamSpec] {
        EMIT_PARAMS
    }

    fn ports(&self, _params: &DecodedParams) -> Ports {
        Ports::sink()
    }

    fn contract(&self, _params: &DecodedParams) -> EffectContract {
        EffectContract {
            cardinality: Cardinality::Sink,
            ..EffectContract::passthrough()
        }
    }
}

/// The `drop` operator: discards every record, as policy loss.
///
/// It takes no parameters.
#[derive(Clone, Copy, Debug, Default)]
pub struct DropOperator;

impl OperatorDef for DropOperator {
    fn name(&self) -> &'static str {
        names::DROP
    }

    fn summary(&self) -> &'static str {
        "Discards every record, as policy loss."
    }

    fn params(&self) -> &'static [ParamSpec] {
        &[]
    }

    fn ports(&self, _params: &DecodedParams) -> Ports {
        Ports::sink()
    }

    fn contract(&self, _params: &DecodedParams) -> EffectContract {
        EffectContract {
            cardinality: Cardinality::Sink,
            loss: Loss::Lossy(vec![LossDecl {
                class: LossClass::Policy,
                description: "Discards every record it receives.",
            }]),
            ..EffectContract::passthrough()
        }
    }
}
