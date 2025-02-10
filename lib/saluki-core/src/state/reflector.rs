//! Mechanisms for processing a data source and sharing the processed results.
use std::sync::Arc;

use futures::{Stream, StreamExt};
use tokio::sync::Notify;

/// Processes input data and modifies shared state based on the result.
pub trait Processor: Send + Sync {
    /// The type of input to the processor.
    type Input;

    /// The state that the processor acts on.
    type State: Send + Sync;

    /// Builds the initial state for the processor.
    fn build_initial_state(&self) -> Self::State;

    /// Processes the input, potentially updating the reflector state.
    fn process(&self, input: Self::Input, state: &Self::State);
}

struct StoreInner<P: Processor> {
    processor: P,
    state: P::State,
    notify_update: Notify,
}

/// Shared state based on a processor.
///
/// `Store` acts as the glue between a processor and the data that it processes. It acts as the entrypoint for taking in
/// a a group of items from a data source, running them through the configured processor, and then notifying callers
/// that an update has taken place.
struct Store<P: Processor> {
    inner: Arc<StoreInner<P>>,
}

impl<P: Processor> Store<P> {
    fn from_processor(processor: P) -> Self {
        let state = processor.build_initial_state();
        Self {
            inner: Arc::new(StoreInner {
                processor,
                state,
                notify_update: Notify::const_new(),
            }),
        }
    }

    pub fn process<I>(&self, inputs: I)
    where
        I: IntoIterator<Item = P::Input>,
    {
        for input in inputs {
            self.inner.processor.process(input, &self.inner.state);
        }
        self.inner.notify_update.notify_waiters();
    }

    pub async fn wait_for_update(&self) {
        self.inner.notify_update.notified().await;
    }

    pub fn state(&self) -> &P::State {
        &self.inner.state
    }
}

impl<P: Processor> Clone for Store<P> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

/// `Reflector` composes a source of data with a processor that is used to transform the data, and then stores the
/// results and allows for shared access by multiple callers.
///
/// Reflectors are a term often found in the context of custom Kubernetes controllers, where they are used to reduce the
/// load on the Kubernetes API server by caching the state of resources in memory. `Reflector` provides comparable
/// functionality, allowing for a single data source to be consumed, and then shared amongst multiple callers. However,
///
/// `Reflector` utilizes the concept of a "processor", which dictates both the type of data that can be consumed and
/// data that gets stored. This means that `Reflector` is more than just a cache of the data source, but also
/// potentially a mapped version of it, allowing for transforming the data in whatever way is necessary.
#[derive(Clone)]
pub struct Reflector<P: Processor> {
    store: Store<P>,
}

impl<P: Processor> Reflector<P> {
    /// Creates a new reflector with the given data source and processor.
    ///
    /// A reflector composes a source of data with a processor that is used to transform the data, and then stores the
    /// the processed results. It can be listened to for updates, and cheaply shared. This allows multiple interested
    /// components to subscribe to the same data source without having to duplicate the processing or storage of the
    /// data.
    /// 
    /// A task will be spawned that drives consumption of the data source and processes the items, feeding them into the
    /// reflector state, which can be queried from the returned `Reflector.`
    ///
    /// `Reflector` is cheaply cloneable and can either be cloned for each caller or shared between them (e.g., via
    /// `Arc<T>`).
    pub async fn new<S, I>(mut source: S, processor: P) -> Self
    where
        S: Stream<Item = I> + Unpin + Send + 'static,
        I: IntoIterator<Item = P::Input> + Send,
        P: 'static,
    {
        let store = Store::from_processor(processor);

        let sender_store = store.clone();
        tokio::spawn(async move {
            while let Some(inputs) = source.next().await {
                sender_store.process(inputs);
            }
        });

        Self { store }
    }
}

impl<P: Processor> Reflector<P> {
    /// Waits for the next update to the reflector.
    ///
    /// When this method completes, callers must query the reflector to acquire the latest state.
    pub async fn wait_for_update(&self) {
        self.store.wait_for_update().await;
    }

    /// Returns a reference a to the reflector's state.
    pub fn state(&self) -> &P::State {
        self.store.state()
    }
}
