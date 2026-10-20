use std::sync::Mutex;

use async_trait::async_trait;
use saluki_error::{generic_error, GenericError};

use crate::accounting::{MemoryBounds, MemoryBoundsBuilder};
use crate::components::sources::{Source, SourceBuilder};
use crate::components::BuildContext;
use crate::data_model::event::EventType;
use crate::topology::OutputDefinition;

/// A source builder that hands over a source the test has already built.
///
/// Use it to run a source through [`TestComponentDriver`][super::TestComponentDriver] when its builder can't produce
/// what the test needs, such as a listener bound to an ephemeral port:
///
/// ```no_run
/// # use saluki_core::components::{sources::Source, test_util::{PrebuiltSource, TestComponentDriver}};
/// # use saluki_core::data_model::event::EventType;
/// # use saluki_core::topology::OutputDefinition;
/// # async fn example(source: Box<dyn Source + Send>) -> Result<(), saluki_error::GenericError> {
/// let outputs = vec![OutputDefinition::default_output(EventType::Metric)];
/// let (control, mut outputs) = TestComponentDriver::source(PrebuiltSource::new(source, outputs)).await?;
/// # Ok(())
/// # }
/// ```
///
/// The source was built outside the driver, so whatever it allocated while being built isn't attributed to the
/// component, and any dataspace it used then isn't the component's. It specifies no memory bounds.
pub struct PrebuiltSource {
    source: Mutex<Option<Box<dyn Source + Send>>>,
    outputs: Vec<OutputDefinition<EventType>>,
}

impl PrebuiltSource {
    /// Creates a builder that hands over `source`, declaring `outputs` as its outputs.
    ///
    /// `outputs` should match what the source dispatches to, as its own builder would declare them.
    pub fn new(source: Box<dyn Source + Send>, outputs: Vec<OutputDefinition<EventType>>) -> Self {
        Self {
            source: Mutex::new(Some(source)),
            outputs,
        }
    }
}

#[async_trait]
impl SourceBuilder for PrebuiltSource {
    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        &self.outputs
    }

    async fn build(&self, _context: BuildContext) -> Result<Box<dyn Source + Send>, GenericError> {
        self.source
            .lock()
            .expect("pre-built source mutex poisoned")
            .take()
            .ok_or_else(|| generic_error!("A `PrebuiltSource` can only be built once, and was already built."))
    }
}

impl MemoryBounds for PrebuiltSource {
    fn specify_bounds(&self, _builder: &mut MemoryBoundsBuilder) {}
}
