//! Read-only descriptions of a topology blueprint's structure.

use std::time::Duration;

use super::{graph::DataType, ComponentId, ComponentOutputId, OutputName};
use crate::{
    components::ComponentType,
    execution::{AdmissionMode, AdmissionPointId},
};

/// A read-only description of a topology blueprint's structure: its components, their outputs, the connections between
/// them, and the outputs marked as admission points.
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
    input: Option<DataType>,
    outputs: Vec<OutputSnapshot>,
}

impl ComponentSnapshot {
    pub(super) fn new(
        id: ComponentId, component_type: ComponentType, input: Option<DataType>, mut outputs: Vec<OutputSnapshot>,
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
        self.input
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
}

impl EdgeSnapshot {
    pub(super) fn new(from: ComponentOutputId, to: ComponentId) -> Self {
        Self { from, to }
    }

    /// Returns the upstream component output.
    pub fn from(&self) -> &ComponentOutputId {
        &self.from
    }

    /// Returns the downstream component.
    pub fn to(&self) -> &ComponentId {
        &self.to
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
