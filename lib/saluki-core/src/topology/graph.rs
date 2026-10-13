#![allow(dead_code)]
use std::{
    collections::{HashMap, HashSet},
    fmt,
    num::{NonZeroU16, NonZeroUsize},
};

use indexmap::IndexSet;
use snafu::Snafu;

use super::{
    snapshot::{ComponentSnapshot, EdgeSnapshot, OutputSnapshot},
    ComponentId, ComponentOutputId, LaneEdge, OutputDefinition, OutputName, TypedComponentId, TypedComponentOutputId,
};
use crate::{
    components::{
        decoders::DecoderBuilder, destinations::DestinationBuilder, encoders::EncoderBuilder,
        forwarders::ForwarderBuilder, relays::RelayBuilder, sources::SourceBuilder, transforms::TransformBuilder,
        ComponentType,
    },
    data_model::{event::EventType, payload::PayloadType},
};

/// Error type for graph operations.
#[derive(Debug, Snafu, Eq, PartialEq)]
#[snafu(context(suffix(false)))]
pub enum GraphError {
    #[snafu(display("invalid component ID '{}': {}", input, reason))]
    InvalidComponentId { input: String, reason: &'static str },
    #[snafu(display("duplicate component ID '{}'", component_id))]
    DuplicateComponentId { component_id: ComponentId },
    #[snafu(display("nonexistent component ID '{}'", component_id))]
    NonexistentComponentId { component_id: ComponentId },
    #[snafu(display("invalid component output ID '{}': {}", input, reason))]
    InvalidComponentOutputId { input: String, reason: &'static str },
    #[snafu(display("duplicate component output ID '{}' for component '{}'", output_id, component_id))]
    DuplicateComponentOutputId {
        component_id: ComponentId,
        output_id: ComponentOutputId,
    },
    #[snafu(display("nonexistent component output ID '{}'", component_output_id))]
    NonexistentComponentOutputId { component_output_id: ComponentOutputId },
    #[snafu(display(
        "data type mismatch from '{}' (type {}) to '{}' (type {})",
        from_component_output_id,
        from_ty,
        to_component_id,
        to_ty
    ))]
    DataTypeMismatch {
        from_component_output_id: ComponentOutputId,
        from_ty: DataType,
        to_component_id: ComponentId,
        to_ty: DataType,
    },
    #[snafu(display(
        "duplicate connection from '{}' to '{}': connect each component output to a given component at most once",
        from,
        to
    ))]
    DuplicateEdge { from: ComponentOutputId, to: ComponentId },
    #[snafu(display(
        "cycle detected: {}; components must be connected without cycles",
        path.iter().map(|id| id.as_ref()).collect::<Vec<&str>>().join(" -> ")
    ))]
    Cycle { path: Vec<ComponentId> },
    #[snafu(display("disconnected components: {:?}", component_ids))]
    DisconnectedComponents { component_ids: Vec<ComponentId> },
    #[snafu(display(
        "cannot connect '{}' to lane {} of '{}', which has {} input lane(s): connect it to a lane the component has",
        from,
        lane,
        to,
        lanes
    ))]
    LaneOutOfRange {
        from: ComponentOutputId,
        to: ComponentId,
        lane: u16,
        lanes: NonZeroU16,
    },
    #[snafu(display(
        "cannot connect '{}' to '{}' without naming how it picks one of the component's {} input lanes: connect it \
         with its lane routing",
        from,
        to,
        lanes
    ))]
    UnroutedLaneEdge {
        from: ComponentOutputId,
        to: ComponentId,
        lanes: NonZeroU16,
    },
    #[snafu(display(
        "cannot route '{}' to the lanes of '{}': it already routes to the lanes of '{}', and each send names one \
         route; give each component with lanes an output of its own",
        from,
        to,
        routed
    ))]
    SharedLaneRoute {
        from: ComponentOutputId,
        to: ComponentId,
        routed: ComponentId,
    },
}

/// Component data type.
///
/// This is used to determine the type of data that a component can produce and/or consume.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum DataType {
    /// Events.
    Event(EventType),

    /// Payloads.
    Payload(PayloadType),
}

impl DataType {
    /// Returns `true` if `self` is the same integral data type as `other`, and `other` has at least one overlapping
    /// subtype with `self`.
    ///
    /// This means that event and payload types are always disjoint.
    fn intersects(&self, other: DataType) -> bool {
        match (self, other) {
            (DataType::Event(a), DataType::Event(b)) => !a.is_none() && !b.is_none() && a.intersects(b),
            (DataType::Payload(a), DataType::Payload(b)) => !a.is_none() && !b.is_none() && a.intersects(b),
            _ => false,
        }
    }
}

impl From<EventType> for DataType {
    fn from(ty: EventType) -> Self {
        DataType::Event(ty)
    }
}

impl From<PayloadType> for DataType {
    fn from(ty: PayloadType) -> Self {
        DataType::Payload(ty)
    }
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DataType::Event(ty) => write!(f, "event({})", ty),
            DataType::Payload(ty) => write!(f, "payload({})", ty),
        }
    }
}

#[derive(Debug, Clone)]
enum Node {
    Source {
        outputs: Vec<TypedComponentOutputId>,
    },
    Relay {
        outputs: Vec<TypedComponentOutputId>,
    },
    Decoder {
        input_ty: PayloadType,
        output_ty: EventType,
    },
    Transform {
        input_ty: EventType,
        outputs: Vec<TypedComponentOutputId>,
        lanes: NonZeroU16,
    },
    Encoder {
        input_ty: EventType,
        output_ty: PayloadType,
    },
    Forwarder {
        input_ty: PayloadType,
    },
    Destination {
        input_ty: EventType,
    },
}

impl Node {
    fn component_type(&self) -> ComponentType {
        match self {
            Node::Source { .. } => ComponentType::Source,
            Node::Relay { .. } => ComponentType::Relay,
            Node::Decoder { .. } => ComponentType::Decoder,
            Node::Transform { .. } => ComponentType::Transform,
            Node::Encoder { .. } => ComponentType::Encoder,
            Node::Forwarder { .. } => ComponentType::Forwarder,
            Node::Destination { .. } => ComponentType::Destination,
        }
    }

    /// Returns the number of input lanes of the component: those of a transform, and one for every other component.
    fn input_lanes(&self) -> NonZeroU16 {
        match self {
            Node::Transform { lanes, .. } => *lanes,
            _ => NonZeroU16::MIN,
        }
    }
}

#[derive(Debug, Clone)]
struct Edge {
    from: ComponentOutputId,
    to: ComponentId,

    // How the sender picks a lane of `to`, which has more than one; `None` for a component with one lane, which every
    // edge leads to whatever it names.
    routing: Option<LaneEdge>,
}

/// A component an output feeds, and how the output picks one of its lanes, if it has more than one.
pub type OutboundEdge = (TypedComponentId, Option<LaneEdge>);

/// A directed graph of components.
///
/// `Graph` holds both the nodes (components) that represent the topology, as well as the edges (connections) between
/// them. It ensures that the resulting graph is minimally valid: valid component IDs, valid component connections, and
/// so on.
#[derive(Debug, Default)]
pub struct Graph {
    nodes: HashMap<ComponentId, Node>,
    edges: Vec<Edge>,
}

impl Graph {
    /// Adds a source node to the graph.
    ///
    /// # Errors
    ///
    /// If the component ID already exists in the graph, or if the component ID is invalid, or if any of the component
    /// output IDs are invalid, an error is returned.
    pub fn add_source<I>(&mut self, component_id: I, builder: &dyn SourceBuilder) -> Result<ComponentId, GraphError>
    where
        I: AsRef<str>,
    {
        let component_id = try_into_component_id(component_id)?;
        if self.nodes.contains_key(&component_id) {
            return Err(GraphError::DuplicateComponentId { component_id });
        }

        let outputs = construct_typed_output_ids(&component_id, builder.outputs())?;
        self.nodes.insert(component_id.clone(), Node::Source { outputs });

        Ok(component_id)
    }

    /// Adds a relay node to the graph.
    ///
    /// # Errors
    ///
    /// If the component ID already exists in the graph, or if the component ID is invalid, or if any of the component
    /// output IDs are invalid, an error is returned.
    pub fn add_relay<I>(&mut self, component_id: I, builder: &dyn RelayBuilder) -> Result<ComponentId, GraphError>
    where
        I: AsRef<str>,
    {
        let component_id = try_into_component_id(component_id)?;
        if self.nodes.contains_key(&component_id) {
            return Err(GraphError::DuplicateComponentId { component_id });
        }

        let outputs = construct_typed_output_ids(&component_id, builder.outputs())?;
        self.nodes.insert(component_id.clone(), Node::Relay { outputs });

        Ok(component_id)
    }

    /// Adds a decoder node to the graph.
    ///
    /// # Errors
    ///
    /// If the component ID already exists in the graph, or if the component ID is invalid, or if any of the component
    /// output IDs are invalid, an error is returned.
    pub fn add_decoder<I>(&mut self, component_id: I, builder: &dyn DecoderBuilder) -> Result<ComponentId, GraphError>
    where
        I: AsRef<str>,
    {
        let component_id = try_into_component_id(component_id)?;
        if self.nodes.contains_key(&component_id) {
            return Err(GraphError::DuplicateComponentId { component_id });
        }

        self.nodes.insert(
            component_id.clone(),
            Node::Decoder {
                input_ty: builder.input_payload_type(),
                output_ty: builder.output_event_type(),
            },
        );

        Ok(component_id)
    }

    /// Adds a transform node to the graph.
    ///
    /// # Errors
    ///
    /// If the component ID already exists in the graph, or if the component ID is invalid, or if any of the component
    /// output IDs are invalid, an error is returned.
    pub fn add_transform<I>(
        &mut self, component_id: I, builder: &dyn TransformBuilder,
    ) -> Result<ComponentId, GraphError>
    where
        I: AsRef<str>,
    {
        let component_id = try_into_component_id(component_id)?;
        if self.nodes.contains_key(&component_id) {
            return Err(GraphError::DuplicateComponentId { component_id });
        }

        let outputs = construct_typed_output_ids(&component_id, builder.outputs())?;
        self.nodes.insert(
            component_id.clone(),
            Node::Transform {
                input_ty: builder.input_event_type(),
                outputs,
                lanes: builder.input_lanes(),
            },
        );

        Ok(component_id)
    }

    /// Adds a encoder node to the graph.
    ///
    /// # Errors
    ///
    /// If the component ID already exists in the graph, or if the component ID is invalid, an error is returned.
    pub fn add_encoder<I>(&mut self, component_id: I, builder: &dyn EncoderBuilder) -> Result<ComponentId, GraphError>
    where
        I: AsRef<str>,
    {
        let component_id = try_into_component_id(component_id)?;
        if self.nodes.contains_key(&component_id) {
            return Err(GraphError::DuplicateComponentId { component_id });
        }

        self.nodes.insert(
            component_id.clone(),
            Node::Encoder {
                input_ty: builder.input_event_type(),
                output_ty: builder.output_payload_type(),
            },
        );

        Ok(component_id)
    }

    /// Adds a forwarder node to the graph.
    ///
    /// # Errors
    ///
    /// If the component ID already exists in the graph, or if the component ID is invalid, an error is returned.
    pub fn add_forwarder<I>(
        &mut self, component_id: I, builder: &dyn ForwarderBuilder,
    ) -> Result<ComponentId, GraphError>
    where
        I: AsRef<str>,
    {
        let component_id = try_into_component_id(component_id)?;
        if self.nodes.contains_key(&component_id) {
            return Err(GraphError::DuplicateComponentId { component_id });
        }

        self.nodes.insert(
            component_id.clone(),
            Node::Forwarder {
                input_ty: builder.input_payload_type(),
            },
        );

        Ok(component_id)
    }

    /// Adds a destination node to the graph.
    ///
    /// # Errors
    ///
    /// If the component ID already exists in the graph, or if the component ID is invalid, an error is returned.
    pub fn add_destination<I>(
        &mut self, component_id: I, builder: &dyn DestinationBuilder,
    ) -> Result<ComponentId, GraphError>
    where
        I: AsRef<str>,
    {
        let component_id = try_into_component_id(component_id)?;
        if self.nodes.contains_key(&component_id) {
            return Err(GraphError::DuplicateComponentId { component_id });
        }

        self.nodes.insert(
            component_id.clone(),
            Node::Destination {
                input_ty: builder.input_event_type(),
            },
        );

        Ok(component_id)
    }

    /// Adds an edge to the graph.
    ///
    /// The `from` side of an edge is a component output, and the `to` side is a component.
    ///
    /// # Errors
    ///
    /// If either the `from` or `to` component IDs don't exist in the graph, or are invalid, an error is returned.
    ///
    /// If the `from` side refers to a terminal component -- a destination or forwarder, which has no outputs -- the
    /// edge is rejected with [`GraphError::NonexistentComponentOutputId`], since a terminal component cannot be the
    /// source of a connection.
    ///
    /// If the `to` side has more than one input lane, the edge is rejected with [`GraphError::UnroutedLaneEdge`]: an
    /// edge into a component with lanes names how its sender picks a lane (see [`add_lane_edge`][Self::add_lane_edge]).
    pub fn add_edge<F, T>(&mut self, from: F, to: T) -> Result<(), GraphError>
    where
        F: AsRef<str>,
        T: AsRef<str>,
    {
        self.add_edge_inner(from, to, None)
    }

    /// Adds an edge to the graph whose sender picks a lane of the `to` side by `routing`.
    ///
    /// An edge into a component with one lane leads to that lane, whatever `routing` names, as
    /// [`add_edge`][Self::add_edge] adds it.
    ///
    /// # Errors
    ///
    /// The errors of [`add_edge`][Self::add_edge], except that the `to` side may have lanes, and: if `routing` is
    /// fixed to a lane the `to` side does not have, [`GraphError::LaneOutOfRange`]; if `routing` is keyed, affinity,
    /// or spread into a component with lanes, and the output already routes that way to the lanes of another
    /// component, [`GraphError::SharedLaneRoute`].
    pub fn add_lane_edge<F, T>(&mut self, from: F, to: T, routing: LaneEdge) -> Result<(), GraphError>
    where
        F: AsRef<str>,
        T: AsRef<str>,
    {
        self.add_edge_inner(from, to, Some(routing))
    }

    fn add_edge_inner<F, T>(&mut self, from: F, to: T, routing: Option<LaneEdge>) -> Result<(), GraphError>
    where
        F: AsRef<str>,
        T: AsRef<str>,
    {
        let component_id = try_into_component_id(to)?;
        let component_output_id = try_into_component_output_id(from)?;

        if !self.nodes.contains_key(&component_id) {
            return Err(GraphError::NonexistentComponentId { component_id });
        }

        // Find the node that the "from" side of the edge is connected to.
        match self.nodes.get(&component_output_id.component_id()) {
            Some(node) => match node {
                // Sources, relays, and transforms support named outputs, so we have to dig into their outputs to make
                // sure the given output ID exists.
                Node::Source { outputs } | Node::Relay { outputs } | Node::Transform { outputs, .. } => {
                    if !outputs
                        .iter()
                        .any(|output| output.component_output() == &component_output_id)
                    {
                        return Err(GraphError::NonexistentComponentOutputId { component_output_id });
                    }
                }
                // Decoders and encoders only have default outputs, so make sure the "from" side is referencing a
                // default output.
                Node::Decoder { .. } | Node::Encoder { .. } => {
                    if !component_output_id.is_default() {
                        return Err(GraphError::NonexistentComponentOutputId { component_output_id });
                    }
                }
                // Destinations and forwarders are terminal nodes, so they can't have outbound edges.
                Node::Destination { .. } | Node::Forwarder { .. } => {
                    return Err(GraphError::NonexistentComponentOutputId { component_output_id })
                }
            },
            None => {
                return Err(GraphError::NonexistentComponentId {
                    component_id: component_output_id.component_id(),
                })
            }
        }

        // A repeated edge would give the upstream output a second sender to the same component, which would then
        // receive every item twice.
        if self
            .edges
            .iter()
            .any(|edge| edge.from == component_output_id && edge.to == component_id)
        {
            return Err(GraphError::DuplicateEdge {
                from: component_output_id,
                to: component_id,
            });
        }

        let routing = self.check_lane_routing(&component_output_id, &component_id, routing)?;
        self.edges.push(Edge {
            from: component_output_id,
            to: component_id,
            routing,
        });
        Ok(())
    }

    /// Checks how an edge from `from` picks a lane of `to`, and returns what the edge records: `None` if `to` has one
    /// lane, which the edge leads to whatever it names.
    fn check_lane_routing(
        &self, from: &ComponentOutputId, to: &ComponentId, routing: Option<LaneEdge>,
    ) -> Result<Option<LaneEdge>, GraphError> {
        let lanes = self.input_lanes(to);
        match routing {
            Some(LaneEdge::Fixed(lane)) if lane >= lanes.get() => Err(GraphError::LaneOutOfRange {
                from: from.clone(),
                to: to.clone(),
                lane,
                lanes,
            }),
            _ if lanes.get() == 1 => Ok(None),
            None => Err(GraphError::UnroutedLaneEdge {
                from: from.clone(),
                to: to.clone(),
                lanes,
            }),
            Some(LaneEdge::Fixed(lane)) => Ok(Some(LaneEdge::Fixed(lane))),
            Some(routing) => {
                // A send names one route, which the lanes of only one component can follow.
                let routed = self
                    .edges
                    .iter()
                    .find(|edge| &edge.from == from && edge.routing.is_some_and(|routing| routing.is_routed()));
                match routed {
                    Some(edge) => Err(GraphError::SharedLaneRoute {
                        from: from.clone(),
                        to: to.clone(),
                        routed: edge.to.clone(),
                    }),
                    None => Ok(Some(routing)),
                }
            }
        }
    }

    /// Returns the component type and data type of the given component output.
    ///
    /// # Errors
    ///
    /// If the component doesn't exist, or doesn't declare the given output, an error is returned.
    pub fn get_output(&self, output_id: &ComponentOutputId) -> Result<(ComponentType, DataType), GraphError> {
        let component_id = output_id.component_id();
        let node = self
            .nodes
            .get(&component_id)
            .ok_or_else(|| GraphError::NonexistentComponentId {
                component_id: component_id.clone(),
            })?;

        let data_ty = match node {
            Node::Source { outputs } | Node::Relay { outputs } | Node::Transform { outputs, .. } => outputs
                .iter()
                .find(|output| output.component_output() == output_id)
                .map(|output| output.output_ty()),
            Node::Decoder { output_ty, .. } => output_id.is_default().then_some(DataType::Event(*output_ty)),
            Node::Encoder { output_ty, .. } => output_id.is_default().then_some(DataType::Payload(*output_ty)),
            Node::Destination { .. } | Node::Forwarder { .. } => None,
        };

        data_ty.map(|data_ty| (node.component_type(), data_ty)).ok_or_else(|| {
            GraphError::NonexistentComponentOutputId {
                component_output_id: output_id.clone(),
            }
        })
    }

    /// Returns the type of the given component, if the graph has it.
    pub fn component_type(&self, component_id: &ComponentId) -> Option<ComponentType> {
        self.nodes.get(component_id).map(Node::component_type)
    }

    /// Returns the number of input lanes of the given component: those of a transform, and one for every other
    /// component, or for a component the graph does not have.
    pub fn input_lanes(&self, component_id: &ComponentId) -> NonZeroU16 {
        self.nodes.get(component_id).map_or(NonZeroU16::MIN, Node::input_lanes)
    }

    /// Returns a description of every component, sorted by component ID, and every edge, sorted by output and then by
    /// downstream component.
    ///
    /// `input_capacity` gives the capacity of each component's input; it is asked only about components that have one.
    pub fn snapshot(
        &self, input_capacity: &dyn Fn(&ComponentId) -> NonZeroUsize,
    ) -> (Vec<ComponentSnapshot>, Vec<EdgeSnapshot>) {
        let mut components = self
            .nodes
            .iter()
            .map(|(id, node)| {
                let input = match node {
                    Node::Source { .. } | Node::Relay { .. } => None,
                    _ => Some((self.get_input_type(id), input_capacity(id), node.input_lanes())),
                };
                let outputs = match node {
                    Node::Source { outputs } | Node::Relay { outputs } | Node::Transform { outputs, .. } => outputs
                        .iter()
                        .map(|output| OutputSnapshot::new(output.component_output().clone(), output.output_ty()))
                        .collect(),
                    Node::Decoder { output_ty, .. } => vec![OutputSnapshot::new(
                        ComponentOutputId::from(id.clone()),
                        DataType::Event(*output_ty),
                    )],
                    Node::Encoder { output_ty, .. } => vec![OutputSnapshot::new(
                        ComponentOutputId::from(id.clone()),
                        DataType::Payload(*output_ty),
                    )],
                    Node::Destination { .. } | Node::Forwarder { .. } => Vec::new(),
                };
                ComponentSnapshot::new(id.clone(), node.component_type(), input, outputs)
            })
            .collect::<Vec<_>>();
        components.sort_by(|a, b| a.id().cmp(b.id()));

        let mut edges = self
            .edges
            .iter()
            .map(|edge| EdgeSnapshot::new(edge.from.clone(), edge.to.clone(), edge.routing))
            .collect::<Vec<_>>();
        edges.sort();

        (components, edges)
    }

    fn get_input_type(&self, id: &ComponentId) -> DataType {
        match self.nodes[id] {
            Node::Source { .. } | Node::Relay { .. } => panic!("no inputs on sources/relays"),
            Node::Decoder { input_ty, .. } => DataType::Payload(input_ty),
            Node::Transform { input_ty, .. } => DataType::Event(input_ty),
            Node::Encoder { input_ty, .. } => DataType::Event(input_ty),
            Node::Forwarder { input_ty } => DataType::Payload(input_ty),
            Node::Destination { input_ty } => DataType::Event(input_ty),
        }
    }

    fn get_output_type(&self, id: &ComponentOutputId) -> DataType {
        match &self.nodes[&id.component_id()] {
            Node::Source { outputs } | Node::Relay { outputs } | Node::Transform { outputs, .. } => outputs
                .iter()
                .find(|output| output.component_output().output() == id.output())
                .map(|output| output.output_ty())
                .expect("output didn't exist"),
            Node::Decoder { output_ty, .. } => {
                if id.is_default() {
                    (*output_ty).into()
                } else {
                    panic!("decoder should only have default output")
                }
            }
            Node::Encoder { output_ty, .. } => {
                if id.is_default() {
                    (*output_ty).into()
                } else {
                    panic!("encoder should only have default output")
                }
            }
            Node::Destination { .. } | Node::Forwarder { .. } => panic!("no outputs on destinations/forwarders"),
        }
    }

    /// Validates the graph.
    ///
    /// Several invariants are checked:
    ///
    /// - All edges must have compatible data types.
    /// - No cycles are allowed in the graph.
    /// - All components must be connected to at least one other component.
    ///
    /// ## Data types
    ///
    /// Components can have two possible data types: events and payloads. Each of these has a number of subtypes, such
    /// as a "metric" event, or a "raw" payload. When two components are connected by an edge, they must both have the
    /// same data type, and have an overlap in data subtypes.
    ///
    /// For example, sources, transforms, and destinations all deal exclusively with events, which means that to be
    /// connected, they must simply have an overlap in the event subtypes they support, such as a downstream component
    /// at least supporting metric events if the upstream only emits metrics.
    ///
    /// Additionally, there are encoder and forwarder components, which deal with events and payloads, respectively.
    /// Forwarders can only accept payloads, so they can't be connected directly to "event" components like sources or
    /// transforms, but they can be connected to encoders. Encoders can accept events and emit payloads, so they can
    /// be connected to sources and transforms as a downstream component (same "event" data type) but can't be
    /// connected to another encoder (different data types). Like "event" components, encoders and forwarders must
    /// have an overlap in "payload" subtypes to be connected.
    ///
    /// # Errors
    ///
    /// If any of the invariants are violated, an error is returned.
    pub fn validate(&self) -> Result<(), GraphError> {
        self.check_event_types()?;
        self.check_for_cycles()?;
        self.check_for_disconnected()?;

        Ok(())
    }

    fn check_event_types(&self) -> Result<(), GraphError> {
        // Check that all connected edges have compatible data types.
        for edge in &self.edges {
            let from_ty = self.get_output_type(&edge.from);
            let to_ty = self.get_input_type(&edge.to);

            if !from_ty.intersects(to_ty) {
                return Err(GraphError::DataTypeMismatch {
                    from_component_output_id: edge.from.clone(),
                    from_ty,
                    to_component_id: edge.to.clone(),
                    to_ty,
                });
            }
        }

        Ok(())
    }

    fn check_for_cycles(&self) -> Result<(), GraphError> {
        // The components each component sends to, in the order the edges were added.
        let mut downstream: HashMap<ComponentId, Vec<&ComponentId>> = HashMap::new();
        for edge in &self.edges {
            downstream.entry(edge.from.component_id()).or_default().push(&edge.to);
        }

        // Run a depth-first search from every component that no earlier search reached, in component ID order so that
        // the reported cycle is deterministic. A component is on the current path while it is in `path`, and finished
        // once it is in `finished`; reaching a component that is on the current path closes a cycle.
        let mut component_ids = self.nodes.keys().collect::<Vec<_>>();
        component_ids.sort();

        let mut finished = HashSet::new();
        for start in component_ids {
            if finished.contains(start) {
                continue;
            }

            let mut path: IndexSet<&ComponentId> = IndexSet::new();
            let mut cursors = vec![0usize];
            path.insert(start);

            while let Some(cursor) = cursors.last_mut() {
                let current = *path.last().expect("path and cursors have the same length");
                let next = downstream
                    .get(current)
                    .and_then(|targets| targets.get(*cursor))
                    .copied();
                *cursor += 1;

                match next {
                    Some(next) if path.contains(next) => {
                        let start_of_cycle = path.get_index_of(next).expect("component is on the path");
                        let cycle = path.iter().skip(start_of_cycle).map(|id| (*id).clone()).collect();
                        return Err(GraphError::Cycle {
                            path: canonical_cycle(cycle),
                        });
                    }
                    Some(next) if !finished.contains(next) => {
                        path.insert(next);
                        cursors.push(0);
                    }
                    Some(_) => {}
                    None => {
                        finished.insert(current);
                        path.pop();
                        cursors.pop();
                    }
                }
            }
        }

        Ok(())
    }

    fn check_for_disconnected(&self) -> Result<(), GraphError> {
        let mut unchecked_nodes = self.nodes.keys().cloned().collect::<HashSet<_>>();

        for edge in &self.edges {
            unchecked_nodes.remove(&edge.from.component_id());
            unchecked_nodes.remove(&edge.to);
        }

        if unchecked_nodes.is_empty() {
            Ok(())
        } else {
            let mut component_ids = unchecked_nodes.into_iter().collect::<Vec<_>>();
            component_ids.dedup();
            component_ids.sort();
            Err(GraphError::DisconnectedComponents { component_ids })
        }
    }

    fn get_component_type(&self, id: &ComponentId) -> Option<ComponentType> {
        self.nodes.get(id).map(Node::component_type)
    }

    /// Returns a mapping of component IDs to their outputs and the components connected to them, each with how the
    /// output picks one of its lanes, if it has more than one.
    pub fn get_outbound_directed_edges(&self) -> HashMap<TypedComponentId, HashMap<OutputName, Vec<OutboundEdge>>> {
        let mut mappings: HashMap<TypedComponentId, HashMap<OutputName, Vec<OutboundEdge>>> = HashMap::new();

        // Populate the map so that every component has its outputs represented.
        //
        // We want to ensure that the caller can reconstruct all of the _configured_ outputs of a component,
        // even if an output doesn't have any edges (connections).
        for (component_id, node) in &self.nodes {
            let component_type = self.get_component_type(component_id).unwrap();
            let typed_component_id = TypedComponentId::new(component_id.clone(), component_type);

            match node {
                Node::Source { outputs } | Node::Relay { outputs } | Node::Transform { outputs, .. } => {
                    let per_component = mappings.entry(typed_component_id).or_default();
                    for output in outputs {
                        per_component.insert(output.component_output().output(), Vec::new());
                    }
                }
                Node::Decoder { .. } | Node::Encoder { .. } => {
                    // Decoders and encoders have a default output
                    let per_component = mappings.entry(typed_component_id).or_default();
                    per_component.insert(OutputName::Default, Vec::new());
                }
                _ => {}
            }
        }

        for edge in &self.edges {
            let raw_from_id = edge.from.component_id();
            let from_type = self.get_component_type(&raw_from_id).expect("component ID not found");
            let from_id = TypedComponentId::new(raw_from_id, from_type);

            let per_component = mappings.entry(from_id).or_default();
            let per_output = per_component.entry(edge.from.output()).or_default();

            let raw_to_id = edge.to.clone();
            let to_type = self.get_component_type(&raw_to_id).expect("component ID not found");
            let to_id = TypedComponentId::new(raw_to_id, to_type);

            per_output.push((to_id, edge.routing));
        }

        mappings
    }
}

/// Rotates a cycle so that it starts at its smallest component ID, and closes it by repeating that ID at the end.
///
/// `cycle` lists each component of the cycle once, in edge order.
fn canonical_cycle(mut cycle: Vec<ComponentId>) -> Vec<ComponentId> {
    let smallest = cycle
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| a.cmp(b))
        .map(|(index, _)| index)
        .unwrap_or_default();
    cycle.rotate_left(smallest);
    if let Some(first) = cycle.first().cloned() {
        cycle.push(first);
    }
    cycle
}

fn try_into_component_id<I>(component_id: I) -> Result<ComponentId, GraphError>
where
    I: AsRef<str>,
{
    ComponentId::try_from(component_id.as_ref()).map_err(|e| GraphError::InvalidComponentId {
        input: component_id.as_ref().to_string(),
        reason: e,
    })
}

fn try_into_component_output_id<I>(component_output_id: I) -> Result<ComponentOutputId, GraphError>
where
    I: AsRef<str>,
{
    ComponentOutputId::try_from(component_output_id.as_ref()).map_err(|e| GraphError::InvalidComponentOutputId {
        input: component_output_id.as_ref().into(),
        reason: e,
    })
}

fn construct_typed_output_ids<T>(
    component_id: &ComponentId, outputs: &[OutputDefinition<T>],
) -> Result<Vec<TypedComponentOutputId>, GraphError>
where
    T: Into<DataType> + Copy,
{
    let mut typed_output_ids = Vec::new();
    for output in outputs {
        let output_id = ComponentOutputId::from_definition(component_id.clone(), output)
            .map_err(|(input, reason)| GraphError::InvalidComponentOutputId { input, reason })?;
        typed_output_ids.push(TypedComponentOutputId::new(output_id, output.data_ty().into()));
    }

    Ok(typed_output_ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::topology::test_util::{
        TestDecoderBuilder, TestDestinationBuilder, TestEncoderBuilder, TestForwarderBuilder, TestRelayBuilder,
        TestSourceBuilder, TestTransformBuilder,
    };

    impl Graph {
        pub fn with_source_fallible(&mut self, id: &str, output_event_ty: EventType) -> Result<&mut Self, GraphError> {
            let builder = TestSourceBuilder::default_output(output_event_ty);
            let _ = self.add_source(id, &builder)?;
            Ok(self)
        }

        #[track_caller]
        pub fn with_source(&mut self, id: &str, output_event_ty: EventType) -> &mut Self {
            self.with_source_fallible(id, output_event_ty)
                .expect("should not fail to add source")
        }

        pub fn with_relay_fallible(
            &mut self, id: &str, output_payload_ty: PayloadType,
        ) -> Result<&mut Self, GraphError> {
            let builder = TestRelayBuilder::default_output(output_payload_ty);
            let _ = self.add_relay(id, &builder)?;
            Ok(self)
        }

        #[track_caller]
        pub fn with_relay(&mut self, id: &str, output_payload_ty: PayloadType) -> &mut Self {
            self.with_relay_fallible(id, output_payload_ty)
                .expect("should not fail to add relay")
        }

        #[track_caller]
        pub fn with_relay_multiple_outputs<'a>(
            &mut self, id: &str, outputs: impl IntoIterator<Item = &'a (Option<&'a str>, PayloadType)>,
        ) -> &mut Self {
            let builder = TestRelayBuilder::multiple_outputs(outputs.into_iter());
            let _ = self.add_relay(id, &builder).expect("should not fail to add relay");
            self
        }

        pub fn with_decoder_fallible(
            &mut self, id: &str, input_payload_ty: PayloadType, output_event_ty: EventType,
        ) -> Result<&mut Self, GraphError> {
            let builder = TestDecoderBuilder::with_input_and_output_type(input_payload_ty, output_event_ty);
            let _ = self.add_decoder(id, &builder)?;
            Ok(self)
        }

        #[track_caller]
        pub fn with_decoder(
            &mut self, id: &str, input_payload_ty: PayloadType, output_event_ty: EventType,
        ) -> &mut Self {
            self.with_decoder_fallible(id, input_payload_ty, output_event_ty)
                .expect("should not fail to add decoder")
        }

        pub fn with_transform_fallible(
            &mut self, id: &str, input_event_ty: EventType, output_event_ty: EventType,
        ) -> Result<&mut Self, GraphError> {
            let builder = TestTransformBuilder::default_output(input_event_ty, output_event_ty);
            let _ = self.add_transform(id, &builder)?;
            Ok(self)
        }

        #[track_caller]
        pub fn with_transform(&mut self, id: &str, input_event_ty: EventType, output_event_ty: EventType) -> &mut Self {
            self.with_transform_fallible(id, input_event_ty, output_event_ty)
                .expect("should not fail to add transform")
        }

        #[track_caller]
        pub fn with_transform_multiple_outputs<'a>(
            &mut self, id: &str, input_event_ty: EventType,
            outputs: impl IntoIterator<Item = &'a (Option<&'a str>, EventType)>,
        ) -> &mut Self {
            let builder = TestTransformBuilder::multiple_outputs(input_event_ty, outputs.into_iter());
            let _ = self
                .add_transform(id, &builder)
                .expect("should not fail to add transform");
            self
        }

        pub fn with_destination_fallible(
            &mut self, id: &str, input_event_ty: EventType,
        ) -> Result<&mut Self, GraphError> {
            let builder = TestDestinationBuilder::with_input_type(input_event_ty);
            let _ = self.add_destination(id, &builder)?;
            Ok(self)
        }

        #[track_caller]
        pub fn with_destination(&mut self, id: &str, input_event_ty: EventType) -> &mut Self {
            self.with_destination_fallible(id, input_event_ty)
                .expect("should not fail to add destination")
        }

        pub fn with_encoder_fallible(
            &mut self, id: &str, input_event_ty: EventType, output_payload_ty: PayloadType,
        ) -> Result<&mut Self, GraphError> {
            let builder = TestEncoderBuilder::with_input_and_output_type(input_event_ty, output_payload_ty);
            let _ = self.add_encoder(id, &builder)?;
            Ok(self)
        }

        #[track_caller]
        pub fn with_encoder(
            &mut self, id: &str, input_event_ty: EventType, output_payload_ty: PayloadType,
        ) -> &mut Self {
            self.with_encoder_fallible(id, input_event_ty, output_payload_ty)
                .expect("should not fail to add encoder")
        }

        pub fn with_forwarder_fallible(
            &mut self, id: &str, input_payload_ty: PayloadType,
        ) -> Result<&mut Self, GraphError> {
            let builder = TestForwarderBuilder::with_input_type(input_payload_ty);
            let _ = self.add_forwarder(id, &builder)?;
            Ok(self)
        }

        #[track_caller]
        pub fn with_forwarder(&mut self, id: &str, input_payload_ty: PayloadType) -> &mut Self {
            self.with_forwarder_fallible(id, input_payload_ty)
                .expect("should not fail to add forwarder")
        }

        pub fn with_edge_fallible(&mut self, from: &str, to: &str) -> Result<&mut Self, GraphError> {
            self.add_edge(from, to)?;
            Ok(self)
        }

        #[track_caller]
        pub fn with_edge(&mut self, from: &str, to: &str) -> &mut Self {
            self.with_edge_fallible(from, to)
                .expect("should not fail to add graph edge")
        }

        #[track_caller]
        pub fn with_multi_edge(&mut self, froms: &[&str], to: &str) -> &mut Self {
            for from in froms {
                self.add_edge(*from, to).expect("should not fail to add graph edge");
            }
            self
        }

        /// Adds a transform of metrics with `lanes` input lanes and a default output.
        #[track_caller]
        pub fn with_laned_transform(&mut self, id: &str, lanes: u16) -> &mut Self {
            let builder = TestTransformBuilder::default_output(EventType::Metric, EventType::Metric).with_lanes(lanes);
            let _ = self
                .add_transform(id, &builder)
                .expect("should not fail to add transform");
            self
        }

        #[track_caller]
        pub fn with_lane_edge(&mut self, from: &str, to: &str, routing: LaneEdge) -> &mut Self {
            self.add_lane_edge(from, to, routing)
                .expect("should not fail to add graph edge");
            self
        }
    }

    fn component_id_doesnt_exist(component_id: &str) -> GraphError {
        GraphError::NonexistentComponentId {
            component_id: ComponentId::try_from(component_id).expect("should not fail to parse component ID"),
        }
    }

    fn output_id_doesnt_exist(output_id: &str) -> GraphError {
        GraphError::NonexistentComponentOutputId {
            component_output_id: ComponentOutputId::try_from(output_id)
                .expect("should not fail to parse component output ID"),
        }
    }

    fn into_component_ids(ids: &[&str]) -> Vec<ComponentId> {
        ids.iter().map(|s| ComponentId::try_from(*s).unwrap()).collect()
    }

    #[test]
    fn invalid_input_id() {
        let mut graph = Graph::default();
        let result = graph
            .with_source("in_eventd", EventType::EventD)
            .with_destination("out_eventd", EventType::EventD)
            .with_edge_fallible("in log", "out_eventd")
            .map(|_| ()); // ditch mutable self ref to allow for equality check

        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), GraphError::InvalidComponentOutputId { input, .. } if input == "in log"))
    }

    #[test]
    fn nonexistent_input_id() {
        let mut graph = Graph::default();
        let result = graph
            .with_source("in_eventd", EventType::EventD)
            .with_destination("out_eventd", EventType::EventD)
            .with_edge_fallible("in_loog", "out_eventd")
            .map(|_| ()); // ditch mutable self ref to allow for equality check

        assert_eq!(result, Err(component_id_doesnt_exist("in_loog")));

        let mut graph = Graph::default();
        let result = graph
            .with_source("in_all_but_logs", EventType::all_bits())
            .with_destination("out_eventd", EventType::EventD)
            .with_edge_fallible("in_all_but_logs.logs", "out_eventd")
            .map(|_| ()); // ditch mutable self ref to allow for equality check

        assert_eq!(result, Err(output_id_doesnt_exist("in_all_but_logs.logs")));
    }

    #[test]
    fn multiple_outputs() {
        let mut graph = Graph::default();
        graph
            .with_source("in_eventd", EventType::EventD)
            .with_transform_multiple_outputs(
                "eventd_to_eventd",
                EventType::EventD,
                &[(None, EventType::EventD), (Some("errors"), EventType::EventD)],
            )
            .with_destination("out_eventd", EventType::EventD)
            .with_destination("out_errored_eventd", EventType::EventD)
            .with_edge("in_eventd", "eventd_to_eventd")
            .with_edge("eventd_to_eventd", "out_eventd")
            .with_edge("eventd_to_eventd.errors", "out_errored_eventd");

        assert_eq!(Ok(()), graph.validate());
    }

    #[test]
    fn detect_cycles() {
        let mut graph = Graph::default();
        graph
            .with_source("in", EventType::EventD)
            .with_transform("one", EventType::EventD, EventType::EventD)
            .with_transform("two", EventType::EventD, EventType::EventD)
            .with_transform("three", EventType::EventD, EventType::EventD)
            .with_destination("out", EventType::EventD)
            .with_multi_edge(&["in", "three"], "one")
            .with_edge("one", "two")
            .with_edge("two", "three")
            .with_edge("three", "out");

        // Cycles are reported in edge order, starting and ending at their smallest component ID.
        assert_eq!(
            Err(GraphError::Cycle {
                path: into_component_ids(&["one", "two", "three", "one"]),
            }),
            graph.validate()
        );

        let mut graph = Graph::default();
        graph
            .with_source("in", EventType::EventD)
            .with_transform("one", EventType::EventD, EventType::EventD)
            .with_transform("two", EventType::EventD, EventType::EventD)
            .with_transform("three", EventType::EventD, EventType::EventD)
            .with_destination("out", EventType::EventD)
            .with_multi_edge(&["in", "three"], "one")
            .with_edge("one", "two")
            .with_edge("two", "three")
            .with_edge("two", "out");

        assert_eq!(
            Err(GraphError::Cycle {
                path: into_component_ids(&["one", "two", "three", "one"]),
            }),
            graph.validate()
        );
    }

    #[test]
    fn cycles_are_detected_without_a_destination() {
        // Paths that end in encoders and forwarders have no destination at all, which is where cycle detection used to
        // start its searches, so a cycle on such a path went unnoticed.
        let mut graph = Graph::default();
        graph
            .with_source("in", EventType::Metric)
            .with_transform("first", EventType::Metric, EventType::Metric)
            .with_transform("second", EventType::Metric, EventType::Metric)
            .with_encoder("encode", EventType::Metric, PayloadType::Http)
            .with_forwarder("out", PayloadType::Http)
            .with_multi_edge(&["in", "second"], "first")
            .with_edge("first", "second")
            .with_edge("second", "encode")
            .with_edge("encode", "out");

        assert_eq!(
            Err(GraphError::Cycle {
                path: into_component_ids(&["first", "second", "first"]),
            }),
            graph.validate()
        );
    }

    #[test]
    fn cycles_are_detected_through_named_outputs_and_self_loops() {
        // A named output feeding back into its own component is a cycle of one.
        let mut graph = Graph::default();
        graph
            .with_source("in", EventType::Metric)
            .with_transform_multiple_outputs(
                "loop",
                EventType::Metric,
                &[(None, EventType::Metric), (Some("again"), EventType::Metric)],
            )
            .with_destination("out", EventType::Metric)
            .with_edge("in", "loop")
            .with_edge("loop.again", "loop")
            .with_edge("loop", "out");

        assert_eq!(
            Err(GraphError::Cycle {
                path: into_component_ids(&["loop", "loop"]),
            }),
            graph.validate()
        );

        // The message lists the cycle in edge order.
        let error = graph.validate().unwrap_err();
        assert_eq!(
            error.to_string(),
            "cycle detected: loop -> loop; components must be connected without cycles"
        );
    }

    #[test]
    fn shared_downstream_components_are_not_cycles() {
        // Two paths that reconverge, reached from several starting points, never close a cycle.
        let mut graph = Graph::default();
        graph
            .with_source("a_in", EventType::Metric)
            .with_source("b_in", EventType::Metric)
            .with_transform("left", EventType::Metric, EventType::Metric)
            .with_transform("right", EventType::Metric, EventType::Metric)
            .with_transform("join", EventType::Metric, EventType::Metric)
            .with_encoder("encode", EventType::Metric, PayloadType::Http)
            .with_forwarder("out", PayloadType::Http)
            .with_multi_edge(&["a_in", "b_in"], "left")
            .with_edge("a_in", "right")
            .with_multi_edge(&["left", "right"], "join")
            .with_edge("join", "encode")
            .with_edge("encode", "out");

        assert_eq!(Ok(()), graph.validate());
    }

    #[test]
    fn duplicate_edges_are_rejected() {
        let mut graph = Graph::default();
        graph
            .with_source("in", EventType::Metric)
            .with_transform_multiple_outputs(
                "split",
                EventType::Metric,
                &[(None, EventType::Metric), (Some("other"), EventType::Metric)],
            )
            .with_destination("out", EventType::Metric)
            .with_edge("in", "split")
            .with_edge("split", "out");

        // Connecting the same output to the same component again is an error, and leaves the graph unchanged.
        let result = graph.with_edge_fallible("split", "out").map(|_| ());
        assert_eq!(
            result,
            Err(GraphError::DuplicateEdge {
                from: try_into_component_output_id("split").unwrap(),
                to: try_into_component_id("out").unwrap(),
            })
        );
        assert_eq!(
            result.unwrap_err().to_string(),
            "duplicate connection from 'split' to 'out': connect each component output to a given component at most \
             once"
        );
        assert_eq!(graph.edges.len(), 2);

        // A different output of the same component may connect to the same downstream component.
        graph.with_edge("split.other", "out");
        assert_eq!(Ok(()), graph.validate());
    }

    #[test]
    fn outputs_report_their_component_and_data_types() {
        let mut graph = Graph::default();
        graph
            .with_source("in", EventType::Metric)
            .with_transform_multiple_outputs(
                "split",
                EventType::Metric,
                &[(None, EventType::Metric), (Some("logs"), EventType::Log)],
            )
            .with_relay("relay", PayloadType::Raw)
            .with_decoder("decode", PayloadType::Raw, EventType::EventD)
            .with_encoder("encode", EventType::Metric, PayloadType::Http)
            .with_forwarder("forward", PayloadType::Http)
            .with_destination("out", EventType::Metric);

        let cases = [
            ("in", Ok((ComponentType::Source, DataType::Event(EventType::Metric)))),
            (
                "split",
                Ok((ComponentType::Transform, DataType::Event(EventType::Metric))),
            ),
            (
                "split.logs",
                Ok((ComponentType::Transform, DataType::Event(EventType::Log))),
            ),
            ("relay", Ok((ComponentType::Relay, DataType::Payload(PayloadType::Raw)))),
            (
                "decode",
                Ok((ComponentType::Decoder, DataType::Event(EventType::EventD))),
            ),
            (
                "encode",
                Ok((ComponentType::Encoder, DataType::Payload(PayloadType::Http))),
            ),
            ("split.missing", Err(output_id_doesnt_exist("split.missing"))),
            ("decode.named", Err(output_id_doesnt_exist("decode.named"))),
            ("forward", Err(output_id_doesnt_exist("forward"))),
            ("out", Err(output_id_doesnt_exist("out"))),
            ("absent", Err(component_id_doesnt_exist("absent"))),
        ];
        for (output, expected) in cases {
            let output_id = try_into_component_output_id(output).unwrap();
            assert_eq!(graph.get_output(&output_id), expected, "output {output}");
        }
    }

    #[test]
    fn disconnected_components() {
        let mut graph = Graph::default();
        graph
            .with_source("in", EventType::EventD)
            .with_transform("one", EventType::EventD, EventType::EventD)
            .with_transform("two", EventType::EventD, EventType::EventD)
            .with_destination("out", EventType::EventD)
            .with_edge("in", "out");

        assert_eq!(
            Err(GraphError::DisconnectedComponents {
                component_ids: into_component_ids(&["one", "two"]),
            }),
            graph.validate()
        );
    }

    #[test]
    fn diamond_edge_formation() {
        let mut graph = Graph::default();
        graph
            .with_source("in", EventType::EventD)
            .with_transform("one", EventType::EventD, EventType::EventD)
            .with_transform("two", EventType::EventD, EventType::EventD)
            .with_transform("three", EventType::EventD, EventType::EventD)
            .with_destination("out", EventType::EventD)
            .with_edge("in", "one")
            .with_edge("in", "two")
            .with_multi_edge(&["one", "two"], "three")
            .with_edge("three", "out");

        assert_eq!(Ok(()), graph.validate());
    }

    #[test]
    fn datatype_disjoint_types() {
        let mut graph = Graph::default();
        graph
            .with_source("in", EventType::Metric)
            .with_forwarder("out", PayloadType::Raw)
            .with_edge("in", "out");

        assert_eq!(
            Err(GraphError::DataTypeMismatch {
                from_component_output_id: try_into_component_output_id("in").unwrap(),
                from_ty: DataType::Event(EventType::Metric),
                to_component_id: try_into_component_id("out").unwrap(),
                to_ty: DataType::Payload(PayloadType::Raw),
            }),
            graph.validate()
        );
    }

    #[test]
    fn datatype_disjoint_sets_event() {
        let mut graph = Graph::default();
        graph
            .with_source("in", EventType::EventD)
            .with_destination("out", EventType::Metric)
            .with_edge("in", "out");

        assert_eq!(
            Err(GraphError::DataTypeMismatch {
                from_component_output_id: try_into_component_output_id("in").unwrap(),
                from_ty: DataType::Event(EventType::EventD),
                to_component_id: try_into_component_id("out").unwrap(),
                to_ty: DataType::Event(EventType::Metric),
            }),
            graph.validate()
        );
    }

    #[test]
    fn datatype_disjoint_sets_payload() {
        let mut graph = Graph::default();
        graph
            .with_source("in", EventType::EventD)
            .with_encoder("eventd_to_payload", EventType::EventD, PayloadType::Raw)
            .with_forwarder("out", PayloadType::Http)
            .with_edge("in", "eventd_to_payload")
            .with_edge("eventd_to_payload", "out");

        assert_eq!(
            Err(GraphError::DataTypeMismatch {
                from_component_output_id: try_into_component_output_id("eventd_to_payload").unwrap(),
                from_ty: DataType::Payload(PayloadType::Raw),
                to_component_id: try_into_component_id("out").unwrap(),
                to_ty: DataType::Payload(PayloadType::Http),
            }),
            graph.validate()
        );
    }

    #[test]
    fn datatype_subset_into_superset() {
        let mut graph = Graph::default();
        graph
            .with_source("in_eventd", EventType::EventD)
            .with_source("in_metric", EventType::Metric)
            .with_destination("out", EventType::all_bits())
            .with_multi_edge(&["in_eventd", "in_metric"], "out");

        assert_eq!(Ok(()), graph.validate());
    }

    #[test]
    fn datatype_superset_into_subset() {
        let mut graph = Graph::default();
        graph
            .with_source("in", EventType::all_bits())
            .with_transform("eventd_to_any", EventType::EventD, EventType::all_bits())
            .with_transform("any_to_eventd", EventType::all_bits(), EventType::EventD)
            .with_destination("out_eventd", EventType::EventD)
            .with_destination("out_metric", EventType::Metric)
            .with_edge("in", "eventd_to_any")
            .with_edge("in", "any_to_eventd")
            .with_multi_edge(&["in", "eventd_to_any", "any_to_eventd"], "out_eventd")
            .with_multi_edge(&["in", "eventd_to_any"], "out_metric");

        assert_eq!(Ok(()), graph.validate());
    }

    #[test]
    fn allows_both_directions_for_metrics() {
        let mut graph = Graph::default();
        graph
            .with_source("in_eventd", EventType::EventD)
            .with_source("in_metric", EventType::Metric)
            .with_transform("eventd_to_eventd", EventType::EventD, EventType::EventD)
            .with_transform("metric_to_metric", EventType::Metric, EventType::Metric)
            .with_transform("any_to_any", EventType::all_bits(), EventType::all_bits())
            .with_transform("any_to_eventd", EventType::all_bits(), EventType::EventD)
            .with_transform("any_to_metric", EventType::all_bits(), EventType::Metric)
            .with_destination("out_eventd", EventType::EventD)
            .with_destination("out_metric", EventType::Metric)
            .with_edge("in_eventd", "eventd_to_eventd")
            .with_edge("in_metric", "metric_to_metric")
            .with_multi_edge(&["eventd_to_eventd", "metric_to_metric"], "any_to_any")
            .with_edge("any_to_any", "any_to_eventd")
            .with_edge("any_to_any", "any_to_metric")
            .with_edge("any_to_eventd", "out_eventd")
            .with_edge("any_to_metric", "out_metric");

        assert_eq!(Ok(()), graph.validate());
    }

    #[test]
    fn basic_source_destination() {
        let mut graph = Graph::default();
        graph
            .with_source("in_eventd", EventType::EventD)
            .with_destination("out_eventd", EventType::EventD)
            .with_edge("in_eventd", "out_eventd");

        assert_eq!(Ok(()), graph.validate());
    }

    #[test]
    fn basic_source_transform_destination() {
        let mut graph = Graph::default();
        graph
            .with_source("in_eventd", EventType::EventD)
            .with_transform("eventd_to_eventd", EventType::EventD, EventType::EventD)
            .with_destination("out_eventd", EventType::EventD)
            .with_edge("in_eventd", "eventd_to_eventd")
            .with_edge("eventd_to_eventd", "out_eventd");

        assert_eq!(Ok(()), graph.validate());
    }

    #[test]
    fn basic_source_encoder_forwarder() {
        let mut graph = Graph::default();
        graph
            .with_source("in_eventd", EventType::EventD)
            .with_encoder("eventd_to_payload", EventType::EventD, PayloadType::Http)
            .with_forwarder("out_http", PayloadType::Http)
            .with_edge("in_eventd", "eventd_to_payload")
            .with_edge("eventd_to_payload", "out_http");

        assert_eq!(Ok(()), graph.validate());
    }

    #[test]
    fn basic_source_fanout_destination_encoder_forwarder() {
        let mut graph = Graph::default();
        graph
            .with_source("in_eventd", EventType::EventD)
            .with_transform("eventd_to_eventd", EventType::EventD, EventType::EventD)
            .with_destination("out_eventd", EventType::EventD)
            .with_encoder("eventd_to_http_payload", EventType::EventD, PayloadType::Http)
            .with_forwarder("out_http", PayloadType::Http)
            .with_edge("in_eventd", "eventd_to_eventd")
            .with_edge("eventd_to_eventd", "out_eventd")
            .with_edge("eventd_to_eventd", "eventd_to_http_payload")
            .with_edge("eventd_to_http_payload", "out_http");

        assert_eq!(Ok(()), graph.validate());
    }

    #[test]
    fn get_outbound_directed_edges_includes_all_configured_outputs() {
        let mut graph = Graph::default();

        // Create a source with default output (connected)
        graph.with_source("source1", EventType::EventD);

        // Create a transform with multiple outputs - one connected, one not connected
        graph.with_transform_multiple_outputs(
            "transform1",
            EventType::EventD,
            &[(None, EventType::EventD), (Some("errors"), EventType::EventD)],
        );

        // Create destinations
        graph.with_destination("dest1", EventType::EventD);

        // Add edges - note that transform1.errors is NOT connected to anything
        graph.with_edge("source1", "transform1");
        graph.with_edge("transform1", "dest1");
        // Intentionally NOT adding: graph.with_edge("transform1.errors", "dest1");

        let outbound_edges = graph.get_outbound_directed_edges();

        // Verify source1 has its default output
        let source1_id = TypedComponentId::new(ComponentId::try_from("source1").unwrap(), ComponentType::Source);
        let source1_outputs = outbound_edges
            .get(&source1_id)
            .expect("source1 should be in outbound edges");
        assert!(
            source1_outputs.contains_key(&OutputName::Default),
            "source1 should have default output"
        );

        // Verify transform1 has BOTH configured outputs (default and errors) even though errors is not connected
        let transform1_id =
            TypedComponentId::new(ComponentId::try_from("transform1").unwrap(), ComponentType::Transform);
        let transform1_outputs = outbound_edges
            .get(&transform1_id)
            .expect("transform1 should be in outbound edges");

        // Check that both outputs are present
        assert!(
            transform1_outputs.contains_key(&OutputName::Default),
            "transform1 should have default output"
        );
        assert!(
            transform1_outputs.contains_key(&OutputName::Given("errors".into())),
            "transform1 should have errors output"
        );

        // Check that default output has connections but errors output is empty
        let default_connections = transform1_outputs.get(&OutputName::Default).unwrap();
        assert_eq!(default_connections.len(), 1, "default output should have 1 connection");

        let errors_connections = transform1_outputs.get(&OutputName::Given("errors".into())).unwrap();
        assert_eq!(errors_connections.len(), 0, "errors output should have 0 connections");

        // Verify destinations are NOT in the outbound edges (they don't have outputs)
        let dest1_id = TypedComponentId::new(ComponentId::try_from("dest1").unwrap(), ComponentType::Destination);
        assert!(
            !outbound_edges.contains_key(&dest1_id),
            "destinations should not be in outbound edges"
        );
    }

    #[test]
    fn basic_relay_decoder_destination() {
        // Relay → Decoder → Destination
        // This is the basic pattern where Relay + Decoder emulates a Source
        let mut graph = Graph::default();
        graph
            .with_relay("relay_in", PayloadType::Raw)
            .with_decoder("decoder", PayloadType::Raw, EventType::EventD)
            .with_destination("out", EventType::EventD)
            .with_edge("relay_in", "decoder")
            .with_edge("decoder", "out");

        assert_eq!(Ok(()), graph.validate());
    }

    #[test]
    fn basic_relay_decoder_transform_destination() {
        // Relay → Decoder → Transform → Destination
        // Validates that Decoder output flows correctly through transforms
        let mut graph = Graph::default();
        graph
            .with_relay("relay_in", PayloadType::Raw)
            .with_decoder("decoder", PayloadType::Raw, EventType::EventD)
            .with_transform("transform", EventType::EventD, EventType::EventD)
            .with_destination("out", EventType::EventD)
            .with_edge("relay_in", "decoder")
            .with_edge("decoder", "transform")
            .with_edge("transform", "out");

        assert_eq!(Ok(()), graph.validate());
    }

    #[test]
    fn relay_decoder_fanout() {
        // Relay → Decoder → [Destination, Encoder → Forwarder]
        // Validates fanout from Decoder output
        let mut graph = Graph::default();
        graph
            .with_relay("relay_in", PayloadType::Raw)
            .with_decoder("decoder", PayloadType::Raw, EventType::EventD)
            .with_destination("out_eventd", EventType::EventD)
            .with_encoder("encoder", EventType::EventD, PayloadType::Http)
            .with_forwarder("out_http", PayloadType::Http)
            .with_edge("relay_in", "decoder")
            .with_edge("decoder", "out_eventd")
            .with_edge("decoder", "encoder")
            .with_edge("encoder", "out_http");

        assert_eq!(Ok(()), graph.validate());
    }

    #[test]
    fn datatype_mismatch_relay_to_destination() {
        // Relay → Destination (should fail - payload vs event type mismatch)
        // Validates that Relay can't connect directly to event-consuming components
        let mut graph = Graph::default();
        graph
            .with_relay("relay_in", PayloadType::Raw)
            .with_destination("out", EventType::EventD)
            .with_edge("relay_in", "out");

        assert_eq!(
            Err(GraphError::DataTypeMismatch {
                from_component_output_id: try_into_component_output_id("relay_in").unwrap(),
                from_ty: DataType::Payload(PayloadType::Raw),
                to_component_id: try_into_component_id("out").unwrap(),
                to_ty: DataType::Event(EventType::EventD),
            }),
            graph.validate()
        );
    }

    #[test]
    fn datatype_mismatch_relay_to_decoder() {
        // Relay(PayloadType::Raw) → Decoder(input: PayloadType::Http)
        // Validates payload type compatibility check between Relay and Decoder
        let mut graph = Graph::default();
        graph
            .with_relay("relay_in", PayloadType::Raw)
            .with_decoder("decoder", PayloadType::Http, EventType::EventD)
            .with_destination("out", EventType::EventD)
            .with_edge("relay_in", "decoder")
            .with_edge("decoder", "out");

        assert_eq!(
            Err(GraphError::DataTypeMismatch {
                from_component_output_id: try_into_component_output_id("relay_in").unwrap(),
                from_ty: DataType::Payload(PayloadType::Raw),
                to_component_id: try_into_component_id("decoder").unwrap(),
                to_ty: DataType::Payload(PayloadType::Http),
            }),
            graph.validate()
        );
    }

    #[test]
    fn terminal_node_cannot_be_edge_source() {
        // Destinations and forwarders are terminal nodes: they have no outputs, so using one as the *source* (the
        // "from" side) of an edge must be rejected with a nonexistent-output error, even though the node itself exists.

        // A destination used as an edge source is rejected.
        let mut graph = Graph::default();
        let result = graph
            .with_source("in", EventType::EventD)
            .with_destination("out", EventType::EventD)
            .with_destination("out_downstream", EventType::EventD)
            .with_edge_fallible("out", "out_downstream")
            .map(|_| ());
        assert_eq!(result, Err(output_id_doesnt_exist("out")));

        // A forwarder used as an edge source is rejected the same way.
        let mut graph = Graph::default();
        let result = graph
            .with_relay("relay_in", PayloadType::Raw)
            .with_forwarder("fwd", PayloadType::Raw)
            .with_forwarder("fwd_downstream", PayloadType::Raw)
            .with_edge_fallible("fwd", "fwd_downstream")
            .map(|_| ());
        assert_eq!(result, Err(output_id_doesnt_exist("fwd")));
    }

    #[test]
    fn decoder_only_has_default_output() {
        // Attempt to use named output on Decoder (for example, "decoder.errors")
        // Should fail with NonexistentComponentOutputId error
        let mut graph = Graph::default();
        graph
            .with_relay("relay_in", PayloadType::Raw)
            .with_decoder("decoder", PayloadType::Raw, EventType::EventD)
            .with_destination("out", EventType::EventD)
            .with_edge("relay_in", "decoder");

        let result = graph.with_edge_fallible("decoder.errors", "out").map(|_| ());

        assert_eq!(result, Err(output_id_doesnt_exist("decoder.errors")));
    }

    #[test]
    fn relay_with_multiple_outputs() {
        // Relay with multiple named outputs → multiple Decoders → Destinations
        // Validates named output handling for Relays
        let mut graph = Graph::default();
        graph
            .with_relay_multiple_outputs(
                "relay_in",
                &[(None, PayloadType::Raw), (Some("http"), PayloadType::Http)],
            )
            .with_decoder("decoder_raw", PayloadType::Raw, EventType::EventD)
            .with_decoder("decoder_http", PayloadType::Http, EventType::EventD)
            .with_destination("out_raw", EventType::EventD)
            .with_destination("out_http", EventType::EventD)
            .with_edge("relay_in", "decoder_raw")
            .with_edge("relay_in.http", "decoder_http")
            .with_edge("decoder_raw", "out_raw")
            .with_edge("decoder_http", "out_http");

        assert_eq!(Ok(()), graph.validate());
    }

    #[test]
    fn get_outbound_directed_edges_includes_relay_and_decoder() {
        // Verify that get_outbound_directed_edges() correctly includes Relay outputs and Decoder default output
        let mut graph = Graph::default();
        graph
            .with_relay_multiple_outputs("relay1", &[(None, PayloadType::Raw), (Some("alt"), PayloadType::Http)])
            .with_decoder("decoder1", PayloadType::Raw, EventType::EventD)
            .with_destination("dest1", EventType::EventD)
            .with_edge("relay1", "decoder1")
            .with_edge("decoder1", "dest1");
        // Note: relay1.alt is intentionally not connected

        let outbound_edges = graph.get_outbound_directed_edges();

        // Verify relay1 has both configured outputs (default and alt)
        let relay1_id = TypedComponentId::new(ComponentId::try_from("relay1").unwrap(), ComponentType::Relay);
        let relay1_outputs = outbound_edges
            .get(&relay1_id)
            .expect("relay1 should be in outbound edges");
        assert!(
            relay1_outputs.contains_key(&OutputName::Default),
            "relay1 should have default output"
        );
        assert!(
            relay1_outputs.contains_key(&OutputName::Given("alt".into())),
            "relay1 should have alt output"
        );

        // Verify default has connection but alt is empty
        let default_connections = relay1_outputs.get(&OutputName::Default).unwrap();
        assert_eq!(default_connections.len(), 1, "default output should have 1 connection");
        let alt_connections = relay1_outputs.get(&OutputName::Given("alt".into())).unwrap();
        assert_eq!(alt_connections.len(), 0, "alt output should have 0 connections");

        // Verify decoder1 has its default output
        let decoder1_id = TypedComponentId::new(ComponentId::try_from("decoder1").unwrap(), ComponentType::Decoder);
        let decoder1_outputs = outbound_edges
            .get(&decoder1_id)
            .expect("decoder1 should be in outbound edges");
        assert!(
            decoder1_outputs.contains_key(&OutputName::Default),
            "decoder1 should have default output"
        );
        let decoder_connections = decoder1_outputs.get(&OutputName::Default).unwrap();
        assert_eq!(
            decoder_connections.len(),
            1,
            "decoder default output should have 1 connection"
        );
    }

    fn output_id(id: &str) -> ComponentOutputId {
        ComponentOutputId::try_from(id).unwrap()
    }

    fn component_id(id: &str) -> ComponentId {
        ComponentId::try_from(id).unwrap()
    }

    #[test]
    fn an_edge_into_a_transform_with_lanes_names_its_routing_and_a_lane_the_transform_has() {
        let lanes = NonZeroU16::new(3).unwrap();
        let mut graph = Graph::default();
        graph
            .with_source("source", EventType::Metric)
            .with_source("other", EventType::Metric)
            .with_laned_transform("laned", 3)
            .with_destination("out", EventType::Metric);
        assert_eq!(graph.input_lanes(&component_id("laned")), lanes);
        assert_eq!(graph.input_lanes(&component_id("out")), NonZeroU16::MIN);
        assert_eq!(graph.input_lanes(&component_id("source")), NonZeroU16::MIN);

        assert_eq!(
            graph.add_edge("source", "laned"),
            Err(GraphError::UnroutedLaneEdge {
                from: output_id("source"),
                to: component_id("laned"),
                lanes,
            })
        );
        assert_eq!(
            graph.add_lane_edge("source", "laned", LaneEdge::Fixed(3)),
            Err(GraphError::LaneOutOfRange {
                from: output_id("source"),
                to: component_id("laned"),
                lane: 3,
                lanes,
            })
        );
        // Into a component with one lane, an edge leads to lane 0 whatever it names, but lane 1 does not exist.
        assert_eq!(
            graph.add_lane_edge("source", "out", LaneEdge::Fixed(1)),
            Err(GraphError::LaneOutOfRange {
                from: output_id("source"),
                to: component_id("out"),
                lane: 1,
                lanes: NonZeroU16::MIN,
            })
        );
        assert_eq!(
            graph.add_edge("source", "laned").unwrap_err().to_string(),
            "cannot connect 'source' to 'laned' without naming how it picks one of the component's 3 input lanes: \
             connect it with its lane routing"
        );

        graph
            .with_lane_edge("source", "laned", LaneEdge::Fixed(2))
            .with_lane_edge("other", "laned", LaneEdge::Spread)
            .with_lane_edge("source", "out", LaneEdge::Keyed)
            .with_edge("laned", "out");
        assert_eq!(Ok(()), graph.validate());

        // Only edges into a component with lanes record their routing.
        let (components, edges) = graph.snapshot(&|_| NonZeroUsize::new(4).unwrap());
        let edges = edges
            .iter()
            .map(|edge| (edge.from().to_string(), edge.to().to_string(), edge.routing()))
            .collect::<Vec<_>>();
        assert_eq!(
            edges,
            vec![
                ("laned".to_string(), "out".to_string(), None),
                ("other".to_string(), "laned".to_string(), Some(LaneEdge::Spread)),
                ("source".to_string(), "laned".to_string(), Some(LaneEdge::Fixed(2))),
                ("source".to_string(), "out".to_string(), None),
            ]
        );
        let lanes_of = components
            .iter()
            .map(|component| (component.id().to_string(), component.input_lanes().map(NonZeroU16::get)))
            .collect::<Vec<_>>();
        assert_eq!(
            lanes_of,
            vec![
                ("laned".to_string(), Some(3)),
                ("other".to_string(), None),
                ("out".to_string(), Some(1)),
                ("source".to_string(), None),
            ]
        );

        // The outbound edges carry the routing too.
        let outbound = graph.get_outbound_directed_edges();
        let source = TypedComponentId::new(component_id("source"), ComponentType::Source);
        let mut targets = outbound[&source][&OutputName::Default]
            .iter()
            .map(|(to, routing)| (to.component_id().to_string(), *routing))
            .collect::<Vec<_>>();
        targets.sort();
        assert_eq!(
            targets,
            vec![
                ("laned".to_string(), Some(LaneEdge::Fixed(2))),
                ("out".to_string(), None)
            ]
        );
    }

    #[test]
    fn an_output_routes_to_the_lanes_of_one_component_only() {
        let mut graph = Graph::default();
        graph
            .with_source("source", EventType::Metric)
            .with_laned_transform("first", 2)
            .with_laned_transform("second", 4)
            .with_laned_transform("third", 2)
            .with_destination("tap", EventType::Metric)
            .with_lane_edge("source", "first", LaneEdge::Keyed);

        // A send names one route, which the lanes of a second component could not follow.
        for routing in [LaneEdge::Keyed, LaneEdge::Affinity, LaneEdge::Spread] {
            assert_eq!(
                graph.add_lane_edge("source", "second", routing),
                Err(GraphError::SharedLaneRoute {
                    from: output_id("source"),
                    to: component_id("second"),
                    routed: component_id("first"),
                })
            );
        }
        // A lane the output is fixed to, and a component with one lane, ignore the route.
        graph
            .with_lane_edge("source", "second", LaneEdge::Fixed(3))
            .with_lane_edge("source", "tap", LaneEdge::Spread);
        // Another output routes on its own.
        graph.with_lane_edge("first", "third", LaneEdge::Affinity);
        assert_eq!(Ok(()), graph.validate());
    }
}
