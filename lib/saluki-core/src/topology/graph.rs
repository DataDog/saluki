#![allow(dead_code)]
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt,
};

use indexmap::IndexSet;
use snafu::Snafu;

use super::{ComponentId, ComponentOutputId, OutputDefinition, OutputName, TypedComponentId, TypedComponentOutputId};
use crate::{
    components::{
        destinations::DestinationBuilder, encoders::EncoderBuilder, forwarders::ForwarderBuilder,
        sources::SourceBuilder, transforms::TransformBuilder, ComponentType,
    },
    data_model::{event::EventType, payload::PayloadType},
};

/// Error type for graph operations.
#[derive(Debug, Snafu, Eq, PartialEq)]
#[snafu(context(suffix(false)))]
pub enum GraphError {
    #[snafu(display("invalid component ID '{}': {}", input, reason))]
    InvalidComponentId { input: String, reason: String },
    #[snafu(display("duplicate component ID '{}'", component_id))]
    DuplicateComponentId { component_id: ComponentId },
    #[snafu(display("nonexistent component ID '{}'", component_id))]
    NonexistentComponentId { component_id: ComponentId },
    #[snafu(display("invalid component output ID '{}': {}", input, reason))]
    InvalidComponentOutputId { input: String, reason: String },
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
    #[snafu(display("cycle detected: {:?}", path))]
    Cycle { path: Vec<ComponentId> },
    #[snafu(display("disconnected components: {:?}", component_ids))]
    DisconnectedComponents { component_ids: Vec<ComponentId> },
}

/// Component data type.
///
/// This is used to determine the type of data that a component can produce and/or consume.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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
    Transform {
        input_ty: EventType,
        outputs: Vec<TypedComponentOutputId>,
    },
    Destination {
        input_ty: EventType,
    },
    Encoder {
        input_ty: EventType,
        output_ty: PayloadType,
    },
    Forwarder {
        input_ty: PayloadType,
    },
}

#[derive(Debug, Clone)]
struct Edge {
    from: ComponentOutputId,
    to: ComponentId,
}

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

    /// Adds an edge to the graph.
    ///
    /// # Errors
    ///
    /// If either the `from` or `to` component IDs do not exist in the graph, or are invalid, an error is returned.
    pub fn add_edge<F, T>(&mut self, from: F, to: T) -> Result<(), GraphError>
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
                // Sources and transforms support named outputs, so we have to dig into their outputs to make sure the
                // given output ID exists.
                Node::Source { outputs } | Node::Transform { outputs, .. } => {
                    if !outputs
                        .iter()
                        .any(|output| output.component_output() == &component_output_id)
                    {
                        return Err(GraphError::NonexistentComponentOutputId { component_output_id });
                    }
                }
                // Encoders only have default outputs, so make sure the "from" side is referencing a default output.
                Node::Encoder { .. } => {
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

        self.edges.push(Edge {
            from: component_output_id,
            to: component_id,
        });
        Ok(())
    }

    fn get_input_type(&self, id: &ComponentId) -> DataType {
        match self.nodes[id] {
            Node::Source { .. } => panic!("no inputs on sources"),
            Node::Transform { input_ty, .. } => DataType::Event(input_ty),
            Node::Destination { input_ty } => DataType::Event(input_ty),
            Node::Encoder { input_ty, .. } => DataType::Event(input_ty),
            Node::Forwarder { input_ty } => DataType::Payload(input_ty),
        }
    }

    fn get_output_type(&self, id: &ComponentOutputId) -> DataType {
        match &self.nodes[&id.component_id()] {
            Node::Source { outputs } | Node::Transform { outputs, .. } => outputs
                .iter()
                .find(|output| output.component_output().output() == id.output())
                .map(|output| DataType::Event(output.output_ty()))
                .expect("output didn't exist"),
            Node::Encoder { output_ty, .. } => {
                if id.is_default() {
                    DataType::Payload(*output_ty)
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
    /// Forwarders can only accept payloads, so they cannot be connected directly to "event" components like sources or
    /// transforms, but they can be connected to encoders. Encoders can accept events and emit payloads, so they can
    /// be connected to sources and transforms as a downstream component (same "event" data type) but cannot be
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
        let destinations = self.nodes.iter().filter_map(|(id, node)| match node {
            Node::Destination { .. } => Some(id.clone()),
            _ => None,
        });

        // Run a depth-first search from each destination while keep tracking the current stack to detect cycles.
        for d in destinations {
            let mut traversal = VecDeque::new();
            let mut visited = HashSet::new();
            let mut stack = IndexSet::new();

            traversal.push_back(d);
            while !traversal.is_empty() {
                let n = traversal.back().cloned().expect("can't be empty");
                if !visited.contains(&n) {
                    visited.insert(n.clone());
                    stack.insert(n.clone());
                } else {
                    // We came back to the node after exploring all its children: remove it from the stack and
                    // traversal.
                    stack.shift_remove(&n);
                    traversal.pop_back();
                }
                let inputs = self.edges.iter().filter(|e| e.to == n).map(|e| &e.from);
                for input in inputs {
                    let input_component_id = input.component_id();
                    if !visited.contains(&input_component_id) {
                        traversal.push_back(input_component_id);
                    } else if stack.contains(&input_component_id) {
                        // We reached the node while it is on the current stack: it's a cycle.
                        let mut path = stack
                            .iter()
                            .skip(1) // skip the sink
                            .rev()
                            .cloned()
                            .collect::<Vec<_>>();
                        path.insert(0, input_component_id.clone());

                        return Err(GraphError::Cycle { path });
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
        match self.nodes.get(id) {
            Some(node) => match node {
                Node::Source { .. } => Some(ComponentType::Source),
                Node::Transform { .. } => Some(ComponentType::Transform),
                Node::Destination { .. } => Some(ComponentType::Destination),
                Node::Encoder { .. } => Some(ComponentType::Encoder),
                Node::Forwarder { .. } => Some(ComponentType::Forwarder),
            },
            None => None,
        }
    }

    /// Returns a mapping of component IDs to their outputs and the component IDs that are connected to them.
    pub fn get_outbound_directed_edges(&self) -> HashMap<TypedComponentId, HashMap<OutputName, Vec<TypedComponentId>>> {
        let mut mappings: HashMap<TypedComponentId, HashMap<OutputName, Vec<TypedComponentId>>> = HashMap::new();

        for edge in &self.edges {
            let raw_from_id = edge.from.component_id();
            let from_type = self.get_component_type(&raw_from_id).expect("component ID not found");
            let from_id = TypedComponentId::new(raw_from_id, from_type);

            let per_component = mappings.entry(from_id).or_default();
            let per_output = per_component.entry(edge.from.output()).or_default();

            let raw_to_id = edge.to.clone();
            let to_type = self.get_component_type(&raw_to_id).expect("component ID not found");
            let to_id = TypedComponentId::new(raw_to_id, to_type);

            per_output.push(to_id);
        }

        mappings
    }
}

fn try_into_component_id<I>(component_id: I) -> Result<ComponentId, GraphError>
where
    I: AsRef<str>,
{
    ComponentId::try_from(component_id.as_ref()).map_err(|e| GraphError::InvalidComponentId {
        input: component_id.as_ref().to_string(),
        reason: e.into(),
    })
}

fn try_into_component_output_id<I>(component_output_id: I) -> Result<ComponentOutputId, GraphError>
where
    I: AsRef<str>,
{
    ComponentOutputId::try_from(component_output_id.as_ref()).map_err(|e| GraphError::InvalidComponentOutputId {
        input: component_output_id.as_ref().into(),
        reason: e.into(),
    })
}

fn construct_typed_output_ids(
    component_id: &ComponentId, outputs: &[OutputDefinition],
) -> Result<Vec<TypedComponentOutputId>, GraphError> {
    let mut typed_output_ids = Vec::new();
    for output in outputs {
        let output_id = ComponentOutputId::from_definition(component_id.clone(), output)
            .map_err(|(input, reason)| GraphError::InvalidComponentOutputId { input, reason })?;
        typed_output_ids.push(TypedComponentOutputId::new(output_id, output.event_ty()));
    }

    Ok(typed_output_ids)
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::topology::test_util::{
        TestDestinationBuilder, TestEncoderBuilder, TestForwarderBuilder, TestSourceBuilder, TestTransformBuilder,
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

        assert_eq!(
            Err(GraphError::Cycle {
                path: into_component_ids(&["three", "one", "two", "three"]),
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
                path: into_component_ids(&["two", "three", "one", "two"]),
            }),
            graph.validate()
        );
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
}
