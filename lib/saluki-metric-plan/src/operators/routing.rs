//! Routing operators: `split`, `tee`, `merge`, `emit`, and `drop`.

use super::{
    contract::{Cardinality, EffectContract, Loss, LossClass, LossDecl, PerKind, PortRelation},
    names,
    ports::{InputPort, OutputPort, PortMapping, Ports, IN, OUT},
    OperatorDef,
};
use crate::{
    ir::{DecodedParams, ParamSpec, ParamType, PortName},
    model::{FieldSet, PortType},
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
