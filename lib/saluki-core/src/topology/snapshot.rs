//! Read-only descriptions of a topology blueprint's structure.

use std::{
    num::{NonZeroU16, NonZeroUsize},
    time::Duration,
};

use super::{graph::DataType, ComponentId, ComponentOutputId, LaneEdge, OutputName};
use crate::{
    components::ComponentType,
    execution::{AdmissionMode, AdmissionPointId},
};

/// A read-only description of a topology blueprint's structure: its components, their outputs, input capacities, and
/// input lanes, the connections between them and how each picks a lane, and the outputs marked as admission points.
///
/// Every list is sorted, so two blueprints with the same structure have equal snapshots regardless of the order in
/// which they were assembled.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlueprintSnapshot {
    components: Vec<ComponentSnapshot>,
    edges: Vec<EdgeSnapshot>,
    admission_points: Vec<AdmissionPointSnapshot>,
}

impl BlueprintSnapshot {
    pub(super) fn new(
        components: Vec<ComponentSnapshot>, edges: Vec<EdgeSnapshot>, mut admission_points: Vec<AdmissionPointSnapshot>,
    ) -> Self {
        admission_points.sort_by(|a, b| a.output.cmp(&b.output));
        Self {
            components,
            edges,
            admission_points,
        }
    }

    /// Returns every component, sorted by component ID.
    pub fn components(&self) -> &[ComponentSnapshot] {
        &self.components
    }

    /// Returns the component with the given ID, if the blueprint has one.
    pub fn component(&self, component_id: &str) -> Option<&ComponentSnapshot> {
        self.components
            .binary_search_by(|component| (**component.id()).cmp(component_id))
            .ok()
            .map(|index| &self.components[index])
    }

    /// Returns every connection, sorted by upstream output and then by downstream component.
    pub fn edges(&self) -> &[EdgeSnapshot] {
        &self.edges
    }

    /// Returns every output marked as an admission point, sorted by output.
    pub fn admission_points(&self) -> &[AdmissionPointSnapshot] {
        &self.admission_points
    }
}

/// A read-only description of one component in a topology blueprint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComponentSnapshot {
    id: ComponentId,
    component_type: ComponentType,
    input: Option<(DataType, NonZeroUsize, NonZeroU16)>,
    outputs: Vec<OutputSnapshot>,
}

impl ComponentSnapshot {
    /// Creates the description of a component, whose input, if it has one, is of the given data type, capacity per
    /// lane, and number of lanes.
    pub(super) fn new(
        id: ComponentId, component_type: ComponentType, input: Option<(DataType, NonZeroUsize, NonZeroU16)>,
        mut outputs: Vec<OutputSnapshot>,
    ) -> Self {
        outputs.sort_by(|a, b| a.id.cmp(&b.id));
        Self {
            id,
            component_type,
            input,
            outputs,
        }
    }

    /// Returns the component ID.
    pub fn id(&self) -> &ComponentId {
        &self.id
    }

    /// Returns the component type.
    pub fn component_type(&self) -> ComponentType {
        self.component_type
    }

    /// Returns the data type the component accepts, or `None` for sources and relays, which have no input.
    pub fn input(&self) -> Option<DataType> {
        self.input.map(|(data_type, _, _)| data_type)
    }

    /// Returns the capacity of the component's input interconnect, in buffers, or `None` for sources and relays, which
    /// have no input.
    ///
    /// This is the blueprint's interconnect capacity, unless the component has a capacity of its own (see
    /// [`TopologyBlueprint::with_component_input_capacity`][super::TopologyBlueprint::with_component_input_capacity]).
    /// A component with several input lanes has one interconnect of this capacity per lane.
    pub fn input_capacity(&self) -> Option<NonZeroUsize> {
        self.input.map(|(_, capacity, _)| capacity)
    }

    /// Returns the number of the component's input lanes, or `None` for sources and relays, which have no input.
    ///
    /// Transforms have the lanes their builder declares
    /// ([`TransformBuilder::input_lanes`][crate::components::transforms::TransformBuilder::input_lanes]); every other
    /// component with an input has one.
    pub fn input_lanes(&self) -> Option<NonZeroU16> {
        self.input.map(|(_, _, lanes)| lanes)
    }

    /// Returns the outputs the component declares, sorted by output ID.
    ///
    /// Destinations and forwarders declare none; decoders and encoders declare only a default output.
    pub fn outputs(&self) -> &[OutputSnapshot] {
        &self.outputs
    }
}

/// A read-only description of one declared component output.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutputSnapshot {
    id: ComponentOutputId,
    data_type: DataType,
}

impl OutputSnapshot {
    pub(super) fn new(id: ComponentOutputId, data_type: DataType) -> Self {
        Self { id, data_type }
    }

    /// Returns the output ID.
    pub fn id(&self) -> &ComponentOutputId {
        &self.id
    }

    /// Returns the output's name within its component.
    pub fn name(&self) -> OutputName {
        self.id.output()
    }

    /// Returns the data type the output emits.
    pub fn data_type(&self) -> DataType {
        self.data_type
    }
}

/// A read-only description of one connection from a component output to a downstream component.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct EdgeSnapshot {
    from: ComponentOutputId,
    to: ComponentId,
    routing: Option<LaneEdge>,
}

impl EdgeSnapshot {
    pub(super) fn new(from: ComponentOutputId, to: ComponentId, routing: Option<LaneEdge>) -> Self {
        Self { from, to, routing }
    }

    /// Returns the upstream component output.
    pub fn from(&self) -> &ComponentOutputId {
        &self.from
    }

    /// Returns the downstream component.
    pub fn to(&self) -> &ComponentId {
        &self.to
    }

    /// Returns how the upstream output picks a lane of the downstream component, or `None` if that component has one
    /// lane, which the connection leads to whatever routing it was connected with.
    pub fn routing(&self) -> Option<LaneEdge> {
        self.routing
    }
}

/// A read-only description of one output marked as an admission point.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdmissionPointSnapshot {
    output: ComponentOutputId,
    id: AdmissionPointId,
    mode: AdmissionMode,
    heartbeat: Option<Duration>,
}

impl AdmissionPointSnapshot {
    pub(super) fn new(
        output: ComponentOutputId, id: AdmissionPointId, mode: AdmissionMode, heartbeat: Option<Duration>,
    ) -> Self {
        Self {
            output,
            id,
            mode,
            heartbeat,
        }
    }

    /// Returns the output that is the admission point.
    pub fn output(&self) -> &ComponentOutputId {
        &self.output
    }

    /// Returns the admission point's identifier.
    pub fn id(&self) -> AdmissionPointId {
        self.id
    }

    /// Returns the admission point's mode.
    pub fn mode(&self) -> AdmissionMode {
        self.mode
    }

    /// Returns the heartbeat period, if periodic heartbeats are enabled.
    pub fn heartbeat(&self) -> Option<Duration> {
        self.heartbeat
    }
}
