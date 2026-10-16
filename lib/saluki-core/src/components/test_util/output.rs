use std::time::Duration;

use tokio::sync::mpsc;

use crate::data_model::event::Event;
use crate::support::SubsystemIdentifier;
use crate::topology::{EventsBuffer, OutputName};

/// One output of a component under test.
///
/// Receives everything the component dispatches to the output. The output closes once the component drops its
/// dispatcher, which happens when its `run` returns.
///
/// Dropping an `Output` makes every later dispatch to it fail, so keep it alive for as long as the component runs.
pub struct Output<T> {
    identity: SubsystemIdentifier,
    name: OutputName,
    receiver: mpsc::Receiver<T>,
    wait_timeout: Duration,
}

impl<T> Output<T> {
    pub(super) fn new(
        identity: SubsystemIdentifier, name: OutputName, receiver: mpsc::Receiver<T>, wait_timeout: Duration,
    ) -> Self {
        Self {
            identity,
            name,
            receiver,
            wait_timeout,
        }
    }

    /// Waits for the next item dispatched to this output.
    ///
    /// Returns `None` once the output is closed and empty, which means the component dropped its dispatcher.
    ///
    /// # Panics
    ///
    /// Panics if no item arrives, and the output doesn't close, within the wait timeout.
    pub async fn next(&mut self) -> Option<T> {
        match tokio::time::timeout(self.wait_timeout, self.receiver.recv()).await {
            Ok(item) => item,
            Err(_) => panic!(
                "timed out after {:?} waiting for an item on output '{}' of component '{}'",
                self.wait_timeout, self.name, self.identity
            ),
        }
    }

    /// Returns the next item dispatched to this output, if one is available right now.
    ///
    /// Returns `None` both when the output is empty and when it's closed.
    pub fn try_next(&mut self) -> Option<T> {
        self.receiver.try_recv().ok()
    }

    /// Collects every item dispatched to this output until it closes.
    ///
    /// The output only closes once the component stops, so shut the component down first, or drain the output
    /// concurrently with the shutdown.
    ///
    /// # Panics
    ///
    /// Panics if the wait timeout elapses between two items, or between the last item and the output closing.
    pub async fn collect(&mut self) -> Vec<T> {
        let mut items = Vec::new();
        loop {
            match tokio::time::timeout(self.wait_timeout, self.receiver.recv()).await {
                Ok(Some(item)) => items.push(item),
                Ok(None) => return items,
                Err(_) => panic!(
                    "timed out after {:?} waiting for output '{}' of component '{}' to close after {} item(s); did \
                     you shut the component down?",
                    self.wait_timeout,
                    self.name,
                    self.identity,
                    items.len()
                ),
            }
        }
    }
}

impl Output<EventsBuffer> {
    /// Collects every event dispatched to this output until it closes, in dispatch order.
    ///
    /// Flattens the event buffers that [`collect`][Self::collect] would return.
    ///
    /// # Panics
    ///
    /// Panics under the same conditions as [`collect`][Self::collect].
    pub async fn collect_events(&mut self) -> Vec<Event> {
        self.collect().await.into_iter().flatten().collect()
    }
}

/// Every output of a component under test.
///
/// Holds one [`Output`] for each output the component's builder declares. Borrow an output to read from it, or take it
/// out to drain several outputs concurrently.
pub struct Outputs<T> {
    identity: SubsystemIdentifier,
    outputs: Vec<(OutputName, Option<Output<T>>)>,
}

impl<T> Outputs<T> {
    pub(super) fn new(identity: SubsystemIdentifier, outputs: Vec<Output<T>>) -> Self {
        Self {
            identity,
            outputs: outputs
                .into_iter()
                .map(|output| (output.name.clone(), Some(output)))
                .collect(),
        }
    }

    /// Returns the default output.
    ///
    /// # Panics
    ///
    /// Panics if the component doesn't declare a default output, or if the default output was already taken.
    #[track_caller]
    pub fn default_output(&mut self) -> &mut Output<T> {
        self.get_mut(&OutputName::Default)
    }

    /// Returns the output named `name`.
    ///
    /// # Panics
    ///
    /// Panics if the component doesn't declare an output named `name`, or if that output was already taken.
    #[track_caller]
    pub fn named_output(&mut self, name: &str) -> &mut Output<T> {
        self.get_mut(&OutputName::Given(name.to_string().into()))
    }

    /// Takes the default output out, so it can be drained independently of the other outputs.
    ///
    /// # Panics
    ///
    /// Panics if the component doesn't declare a default output, or if the default output was already taken.
    #[track_caller]
    pub fn take_default_output(&mut self) -> Output<T> {
        self.take(&OutputName::Default)
    }

    /// Takes the output named `name` out, so it can be drained independently of the other outputs.
    ///
    /// # Panics
    ///
    /// Panics if the component doesn't declare an output named `name`, or if that output was already taken.
    #[track_caller]
    pub fn take_named_output(&mut self, name: &str) -> Output<T> {
        self.take(&OutputName::Given(name.to_string().into()))
    }

    #[track_caller]
    fn get_mut(&mut self, name: &OutputName) -> &mut Output<T> {
        let index = self.index_of(name);
        let identity = &self.identity;
        match &mut self.outputs[index].1 {
            Some(output) => output,
            None => panic!("output '{}' of component '{}' was already taken", name, identity),
        }
    }

    #[track_caller]
    fn take(&mut self, name: &OutputName) -> Output<T> {
        let index = self.index_of(name);
        match self.outputs[index].1.take() {
            Some(output) => output,
            None => panic!("output '{}' of component '{}' was already taken", name, self.identity),
        }
    }

    #[track_caller]
    fn index_of(&self, name: &OutputName) -> usize {
        match self.outputs.iter().position(|(declared, _)| declared == name) {
            Some(index) => index,
            None => {
                let declared = self
                    .outputs
                    .iter()
                    .map(|(declared, _)| format!("'{}'", declared))
                    .collect::<Vec<_>>()
                    .join(", ");
                panic!(
                    "component '{}' doesn't declare output '{}'; declared outputs: [{}]",
                    self.identity, name, declared
                );
            }
        }
    }
}
