use std::collections::{HashMap, HashSet, VecDeque};

use indexmap::IndexSet;
use snafu::Snafu;

use saluki_event::DataType;

use crate::components::{destinations::DestinationBuilder, sources::SourceBuilder, transforms::TransformBuilder};

use super::{ComponentId, ComponentOutputId, OutputDefinition, OutputName, TypedComponentOutputId};

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

#[derive(Debug, Clone)]
pub enum Node {
    Source {
        outputs: Vec<TypedComponentOutputId>,
    },
    Transform {
        input_ty: DataType,
        outputs: Vec<TypedComponentOutputId>,
    },
    Destination {
        input_ty: DataType,
    },
}

#[derive(Debug, Clone)]
struct Edge {
    from: ComponentOutputId,
    to: ComponentId,
}

#[derive(Debug, Default)]
pub struct Graph {
    nodes: HashMap<ComponentId, Node>,
    edges: Vec<Edge>,
}

impl Graph {
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
                input_ty: builder.input_data_type(),
                outputs,
            },
        );

        Ok(component_id)
    }

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
                input_ty: builder.input_data_type(),
            },
        );

        Ok(component_id)
    }

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

        match self.nodes.get(&component_output_id.component_id()) {
            Some(node) => match node {
                Node::Source { outputs } | Node::Transform { outputs, .. } => {
                    if !outputs
                        .iter()
                        .any(|output| output.component_output() == &component_output_id)
                    {
                        return Err(GraphError::NonexistentComponentOutputId { component_output_id });
                    }
                }
                Node::Destination { .. } => {
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
            Node::Transform { input_ty, .. } => input_ty,
            Node::Destination { input_ty } => input_ty,
        }
    }

    fn get_output_type(&self, id: &ComponentOutputId) -> DataType {
        match &self.nodes[&id.component_id()] {
            Node::Source { outputs } => outputs
                .iter()
                .find(|output| output.component_output().output() == id.output())
                .map(|output| output.output_ty())
                .expect("output didn't exist"),
            Node::Transform { outputs, .. } => outputs
                .iter()
                .find(|output| output.component_output().output() == id.output())
                .map(|output| output.output_ty())
                .expect("output didn't exist"),
            Node::Destination { .. } => panic!("no outputs on sinks"),
        }
    }

    pub fn validate(&self) -> Result<(), GraphError> {
        self.check_data_types()?;
        self.check_for_cycles()?;
        self.check_for_disconnected()?;

        Ok(())
    }

    fn check_data_types(&self) -> Result<(), GraphError> {
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

    pub fn get_outbound_directed_edges(&self) -> HashMap<ComponentId, HashMap<OutputName, Vec<ComponentId>>> {
        let mut mappings: HashMap<ComponentId, HashMap<OutputName, Vec<ComponentId>>> = HashMap::new();

        for edge in &self.edges {
            let per_component = mappings.entry(edge.from.component_id()).or_default();
            let per_output = per_component.entry(edge.from.output()).or_default();
            per_output.push(edge.to.clone());
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
        typed_output_ids.push(TypedComponentOutputId::new(output_id, output.data_ty()));
    }

    Ok(typed_output_ids)
}

#[cfg(test)]
mod test {
    use similar_asserts::assert_eq;

    use crate::topology::test_util::{TestDestinationBuilder, TestSourceBuilder, TestTransformBuilder};

    use super::*;

    impl Graph {
        pub fn with_source_fallible(&mut self, id: &str, output_data_ty: DataType) -> Result<&mut Self, GraphError> {
            let builder = TestSourceBuilder::default_output(output_data_ty);
            let _ = self.add_source(id, &builder)?;
            Ok(self)
        }

        pub fn with_source(&mut self, id: &str, output_data_ty: DataType) -> &mut Self {
            self.with_source_fallible(id, output_data_ty)
                .expect("should not fail to add source")
        }

        pub fn with_transform_fallible(
            &mut self, id: &str, input_data_ty: DataType, output_data_ty: DataType,
        ) -> Result<&mut Self, GraphError> {
            let builder = TestTransformBuilder::default_output(input_data_ty, output_data_ty);
            let _ = self.add_transform(id, &builder)?;
            Ok(self)
        }

        pub fn with_transform(&mut self, id: &str, input_data_ty: DataType, output_data_ty: DataType) -> &mut Self {
            self.with_transform_fallible(id, input_data_ty, output_data_ty)
                .expect("should not fail to add transform")
        }

        pub fn with_transform_multiple_outputs<'a>(
            &mut self, id: &str, input_data_ty: DataType,
            outputs: impl IntoIterator<Item = &'a (Option<&'a str>, DataType)>,
        ) -> &mut Self {
            let builder = TestTransformBuilder::multiple_outputs(input_data_ty, outputs.into_iter());
            let _ = self
                .add_transform(id, &builder)
                .expect("should not fail to add transform");
            self
        }

        pub fn with_destination_fallible(
            &mut self, id: &str, input_data_ty: DataType,
        ) -> Result<&mut Self, GraphError> {
            let builder = TestDestinationBuilder::with_input_type(input_data_ty);
            let _ = self.add_destination(id, &builder)?;
            Ok(self)
        }

        pub fn with_destination(&mut self, id: &str, input_data_ty: DataType) -> &mut Self {
            self.with_destination_fallible(id, input_data_ty)
                .expect("should not fail to add destination")
        }

        pub fn with_edge_fallible(&mut self, from: &str, to: &str) -> Result<&mut Self, GraphError> {
            self.add_edge(from, to)?;
            Ok(self)
        }

        pub fn with_edge(&mut self, from: &str, to: &str) -> &mut Self {
            self.with_edge_fallible(from, to)
                .expect("should not fail to add graph edge")
        }

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
            .with_source("in_eventd", DataType::EventD)
            .with_destination("out_eventd", DataType::EventD)
            .with_edge_fallible("in log", "out_eventd")
            .map(|_| ()); // ditch mutable self ref to allow for equality check

        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), GraphError::InvalidComponentOutputId { input, .. } if input == "in log"))
    }

    #[test]
    fn nonexistent_input_id() {
        let mut graph = Graph::default();
        let result = graph
            .with_source("in_eventd", DataType::EventD)
            .with_destination("out_eventd", DataType::EventD)
            .with_edge_fallible("in_loog", "out_eventd")
            .map(|_| ()); // ditch mutable self ref to allow for equality check

        assert_eq!(result, Err(component_id_doesnt_exist("in_loog")));

        let mut graph = Graph::default();
        let result = graph
            .with_source("in_all_but_logs", DataType::all_bits())
            .with_destination("out_eventd", DataType::EventD)
            .with_edge_fallible("in_all_but_logs.logs", "out_eventd")
            .map(|_| ()); // ditch mutable self ref to allow for equality check

        assert_eq!(result, Err(output_id_doesnt_exist("in_all_but_logs.logs")));
    }

    #[test]
    fn multiple_outputs() {
        let mut graph = Graph::default();
        graph
            .with_source("in_eventd", DataType::EventD)
            .with_transform_multiple_outputs(
                "eventd_to_eventd",
                DataType::EventD,
                &[(None, DataType::EventD), (Some("errors"), DataType::EventD)],
            )
            .with_destination("out_eventd", DataType::EventD)
            .with_destination("out_errored_eventd", DataType::EventD)
            .with_edge("in_eventd", "eventd_to_eventd")
            .with_edge("eventd_to_eventd", "out_eventd")
            .with_edge("eventd_to_eventd.errors", "out_errored_eventd");

        assert_eq!(Ok(()), graph.check_data_types());
    }

    #[test]
    fn detect_cycles() {
        let mut graph = Graph::default();
        graph
            .with_source("in", DataType::EventD)
            .with_transform("one", DataType::EventD, DataType::EventD)
            .with_transform("two", DataType::EventD, DataType::EventD)
            .with_transform("three", DataType::EventD, DataType::EventD)
            .with_destination("out", DataType::EventD)
            .with_multi_edge(&["in", "three"], "one")
            .with_edge("one", "two")
            .with_edge("two", "three")
            .with_edge("three", "out");

        assert_eq!(
            Err(GraphError::Cycle {
                path: into_component_ids(&["three", "one", "two", "three"]),
            }),
            graph.check_for_cycles()
        );

        let mut graph = Graph::default();
        graph
            .with_source("in", DataType::EventD)
            .with_transform("one", DataType::EventD, DataType::EventD)
            .with_transform("two", DataType::EventD, DataType::EventD)
            .with_transform("three", DataType::EventD, DataType::EventD)
            .with_destination("out", DataType::EventD)
            .with_multi_edge(&["in", "three"], "one")
            .with_edge("one", "two")
            .with_edge("two", "three")
            .with_edge("two", "out");

        assert_eq!(
            Err(GraphError::Cycle {
                path: into_component_ids(&["two", "three", "one", "two"]),
            }),
            graph.check_for_cycles()
        );
    }

    #[test]
    fn disconnected_components() {
        let mut graph = Graph::default();
        graph
            .with_source("in", DataType::EventD)
            .with_transform("one", DataType::EventD, DataType::EventD)
            .with_transform("two", DataType::EventD, DataType::EventD)
            .with_destination("out", DataType::EventD)
            .with_edge("in", "out");

        assert_eq!(
            Err(GraphError::DisconnectedComponents {
                component_ids: into_component_ids(&["one", "two"]),
            }),
            graph.check_for_disconnected()
        );
    }

    #[test]
    fn diamond_edge_formation() {
        let mut graph = Graph::default();
        graph
            .with_source("in", DataType::EventD)
            .with_transform("one", DataType::EventD, DataType::EventD)
            .with_transform("two", DataType::EventD, DataType::EventD)
            .with_transform("three", DataType::EventD, DataType::EventD)
            .with_destination("out", DataType::EventD)
            .with_edge("in", "one")
            .with_edge("in", "two")
            .with_multi_edge(&["one", "two"], "three")
            .with_edge("three", "out");

        graph.check_for_cycles().unwrap();
    }

    #[test]
    fn datatype_disjoint_sets() {
        let mut graph = Graph::default();
        graph
            .with_source("in", DataType::EventD)
            .with_destination("out", DataType::Metric)
            .with_edge("in", "out");

        assert_eq!(
            Err(GraphError::DataTypeMismatch {
                from_component_output_id: try_into_component_output_id("in").unwrap(),
                from_ty: DataType::EventD,
                to_component_id: try_into_component_id("out").unwrap(),
                to_ty: DataType::Metric,
            }),
            graph.check_data_types()
        );
    }

    #[test]
    fn datatype_subset_into_superset() {
        let mut graph = Graph::default();
        graph
            .with_source("in_eventd", DataType::EventD)
            .with_source("in_metric", DataType::Metric)
            .with_destination("out", DataType::all_bits())
            .with_multi_edge(&["in_eventd", "in_metric"], "out");

        assert_eq!(Ok(()), graph.check_data_types());
    }

    #[test]
    fn datatype_superset_into_subset() {
        let mut graph = Graph::default();
        graph
            .with_source("in", DataType::all_bits())
            .with_transform("eventd_to_any", DataType::EventD, DataType::all_bits())
            .with_transform("any_to_eventd", DataType::all_bits(), DataType::EventD)
            .with_destination("out_eventd", DataType::EventD)
            .with_destination("out_metric", DataType::Metric)
            .with_edge("in", "eventd_to_any")
            .with_edge("in", "any_to_eventd")
            .with_multi_edge(&["in", "eventd_to_any", "any_to_eventd"], "out_eventd")
            .with_multi_edge(&["in", "eventd_to_any"], "out_metric");

        assert_eq!(Ok(()), graph.check_data_types());
    }

    #[test]
    fn allows_both_directions_for_metrics() {
        let mut graph = Graph::default();
        graph
            .with_source("in_eventd", DataType::EventD)
            .with_source("in_metric", DataType::Metric)
            .with_transform("eventd_to_eventd", DataType::EventD, DataType::EventD)
            .with_transform("metric_to_metric", DataType::Metric, DataType::Metric)
            .with_transform("any_to_any", DataType::all_bits(), DataType::all_bits())
            .with_transform("any_to_eventd", DataType::all_bits(), DataType::EventD)
            .with_transform("any_to_metric", DataType::all_bits(), DataType::Metric)
            .with_destination("out_eventd", DataType::EventD)
            .with_destination("out_metric", DataType::Metric)
            .with_edge("in_eventd", "eventd_to_eventd")
            .with_edge("in_metric", "metric_to_metric")
            .with_multi_edge(&["eventd_to_eventd", "metric_to_metric"], "any_to_any")
            .with_edge("any_to_any", "any_to_eventd")
            .with_edge("any_to_any", "any_to_metric")
            .with_edge("any_to_eventd", "out_eventd")
            .with_edge("any_to_metric", "out_metric");

        assert_eq!(Ok(()), graph.check_data_types());
    }
}
