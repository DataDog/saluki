use std::collections::HashMap;

use snafu::{ResultExt as _, Snafu};

use crate::components::{DestinationBuilder, SourceBuilder, TransformBuilder};

use super::{
    built::BuiltTopology,
    graph::{Graph, GraphError},
    ComponentId, ComponentOutputId,
};

#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub enum BlueprintError {
    #[snafu(display("graph error: {}", source))]
    InvalidGraph { source: GraphError },

    #[snafu(display("builder error: {}", source))]
    Builder {
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

/// A topology blueprint represents a directed graph of components.
#[derive(Default)]
pub struct TopologyBlueprint {
    graph: Graph,
    sources: HashMap<ComponentId, Box<dyn SourceBuilder>>,
    transforms: HashMap<ComponentId, Box<dyn TransformBuilder>>,
    destinations: HashMap<ComponentId, Box<dyn DestinationBuilder>>,
}

impl TopologyBlueprint {
    pub fn add_source<I, B>(&mut self, maybe_raw_component_id: I, builder: B) -> Result<&mut Self, BlueprintError>
    where
        I: Into<String> + Clone,
        ComponentId: TryFrom<I>,
        <ComponentId as TryFrom<I>>::Error: Into<String>,
        B: SourceBuilder + 'static,
    {
        let component_id = self
            .graph
            .add_source(maybe_raw_component_id, &builder)
            .context(InvalidGraph)?;
        let _ = self.sources.insert(component_id, Box::new(builder));

        Ok(self)
    }

    pub fn add_transform<I, B>(&mut self, maybe_raw_component_id: I, builder: B) -> Result<&mut Self, BlueprintError>
    where
        I: Into<String> + Clone,
        ComponentId: TryFrom<I>,
        <ComponentId as TryFrom<I>>::Error: Into<String>,
        B: TransformBuilder + 'static,
    {
        let component_id = self
            .graph
            .add_transform(maybe_raw_component_id, &builder)
            .context(InvalidGraph)?;
        let _ = self.transforms.insert(component_id, Box::new(builder));

        Ok(self)
    }

    pub fn add_destination<I, B>(&mut self, maybe_raw_component_id: I, builder: B) -> Result<&mut Self, BlueprintError>
    where
        I: Into<String> + Clone,
        ComponentId: TryFrom<I>,
        <ComponentId as TryFrom<I>>::Error: Into<String>,
        B: DestinationBuilder + 'static,
    {
        let component_id = self
            .graph
            .add_destination(maybe_raw_component_id, &builder)
            .context(InvalidGraph)?;
        let _ = self.destinations.insert(component_id, Box::new(builder));

        Ok(self)
    }

    pub fn connect_component<I, I2, I2T>(
        &mut self, maybe_raw_component_id: I, inputs: I2,
    ) -> Result<&mut Self, BlueprintError>
    where
        I: Into<String> + Clone,
        ComponentId: TryFrom<I>,
        <ComponentId as TryFrom<I>>::Error: Into<String>,
        I2: IntoIterator<Item = I2T>,
        I2T: Into<String> + Clone,
        ComponentOutputId: TryFrom<I2T>,
        <ComponentOutputId as TryFrom<I2T>>::Error: Into<String>,
    {
        for maybe_raw_output_id in inputs.into_iter() {
            self.graph
                .add_edge(maybe_raw_output_id, maybe_raw_component_id.clone())
                .context(InvalidGraph)?;
        }

        Ok(self)
    }

    pub async fn build(self) -> Result<BuiltTopology, BlueprintError> {
        self.graph.validate().context(InvalidGraph)?;

        let mut sources = HashMap::new();
        for (id, builder) in self.sources {
            let source = builder.build().await.context(Builder)?;
            sources.insert(id, source);
        }

        let mut transforms = HashMap::new();
        for (id, builder) in self.transforms {
            let transform = builder.build().await.context(Builder)?;
            transforms.insert(id, transform);
        }

        let mut destinations = HashMap::new();
        for (id, builder) in self.destinations {
            let destination = builder.build().await.context(Builder)?;
            destinations.insert(id, destination);
        }

        Ok(BuiltTopology::from_parts(self.graph, sources, transforms, destinations))
    }
}
