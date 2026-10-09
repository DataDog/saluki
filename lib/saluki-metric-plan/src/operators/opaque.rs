//! The `opaque_host` operator.

use super::{
    contract::{
        Boundary, Cardinality, EffectContract, Identity, Loss, LossClass, LossDecl, OrderContract, PerKind,
        PortRelation, StateContract,
    },
    names,
    ports::{InputPort, OutputPort, PortMapping, Ports, IN},
    support::error,
    OperatorDef,
};
use crate::{
    extension::TransitionClass,
    ir::{DecodedParams, NativeComponentId, ParamError, ParamSpec, ParamType, ParamValue, PortName},
    model::{FieldSet, PortType},
};

const OPAQUE_HOST_PARAMS: &[ParamSpec] = &[
    ParamSpec::required("component", ParamType::String),
    ParamSpec::required("input", ParamType::PortType),
    ParamSpec::required("outputs", ParamType::StringMap(&ParamType::PortType)),
];

/// Returns the declared outputs, by port name, skipping any whose name is not a valid port name.
fn outputs(params: &DecodedParams) -> Vec<(PortName, PortType)> {
    match params.known("outputs") {
        Some(ParamValue::StringMap(outputs)) => outputs
            .iter()
            .filter_map(|(name, value)| match value {
                ParamValue::PortType(port_type) => PortName::new(name.as_str()).ok().map(|name| (name, *port_type)),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// The `opaque_host` operator: a native component that has no extracted kernel, run as it is.
///
/// Parameters: `component` (the native component's identifier), `input` (the port type its input port `in` accepts),
/// and `outputs` (its output ports, by name, each with the port type it produces; at least one).
///
/// Nothing is known about what the component does, so its contract reads and writes every field, may copy records to
/// several outputs, may lose records, and fences every rewrite. Only native configuration can place an opaque host:
/// artifacts cannot declare operator semantics.
#[derive(Clone, Copy, Debug, Default)]
pub struct OpaqueHost;

impl OperatorDef for OpaqueHost {
    fn name(&self) -> &'static str {
        names::OPAQUE_HOST
    }

    fn summary(&self) -> &'static str {
        "Runs a native component that has no extracted kernel, as an opaque fence."
    }

    fn params(&self) -> &'static [ParamSpec] {
        OPAQUE_HOST_PARAMS
    }

    fn check_params(&self, params: &DecodedParams) -> Vec<ParamError> {
        let mut errors = Vec::new();
        if let Some(component) = params.str("component") {
            if let Err(invalid) = NativeComponentId::new(component) {
                errors.push(error("component", invalid.to_string()));
            }
        }
        if let Some(ParamValue::StringMap(declared)) = params.known("outputs") {
            if declared.is_empty() {
                errors.push(error("outputs", "an opaque host needs at least one output port"));
            }
            for name in declared.keys() {
                if let Err(invalid) = PortName::new(name.as_str()) {
                    errors.push(error(&format!("outputs.{}", name), invalid.to_string()));
                }
            }
        }
        errors
    }

    fn ports(&self, params: &DecodedParams) -> Ports {
        Ports {
            inputs: vec![InputPort {
                name: PortName::from_static(IN),
                accepts: params.port_type("input").unwrap_or(PortType::ANY),
            }],
            outputs: outputs(params)
                .into_iter()
                .map(|(name, _)| OutputPort {
                    name,
                    mapping: PortMapping::IDENTITY,
                })
                .collect(),
        }
    }

    fn contract(&self, _params: &DecodedParams) -> EffectContract {
        EffectContract {
            reads: PerKind::uniform(FieldSet::ALL),
            writes: PerKind::uniform(FieldSet::ALL),
            cardinality: Cardinality::Expand { max: None },
            ports: PortRelation::Copies,
            identity: PerKind::uniform(Identity::Opaque),
            state: StateContract::Opaque,
            order: OrderContract::Dependent,
            loss: Loss::Lossy(vec![LossDecl {
                class: LossClass::Policy,
                description: "May drop records, as the native component does.",
            }]),
            boundary: Boundary::OpaqueHost,
            ..EffectContract::passthrough()
        }
    }

    fn output_types(&self, params: &DecodedParams, _ports: &Ports, _inputs: &[PortType]) -> Vec<PortType> {
        outputs(params).into_iter().map(|(_, port_type)| port_type).collect()
    }

    fn transition(&self, old: &DecodedParams, new: &DecodedParams) -> TransitionClass {
        // The component's state is its own: it can only carry over to the same component, configured the same way.
        if old == new {
            TransitionClass::Retain
        } else {
            TransitionClass::Reject
        }
    }
}
