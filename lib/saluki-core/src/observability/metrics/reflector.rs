//! Mechanisms for processing a data source and sharing the processed results.
use std::sync::Arc;

use async_trait::async_trait;
use futures::{Stream, StreamExt};
use saluki_common::sync::shutdown::ShutdownHandle;
use tokio::{
    select,
    sync::{Mutex, Notify},
};

use crate::runtime::{InitializationError, Supervisable, SupervisorFuture};

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
/// a group of items from a data source, running them through the configured processor, and then notifying callers
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

/// `Reflector` composes a source of data with a processor that's used to transform the data, and then stores the
/// results and allows for shared access by multiple callers.
///
/// Reflectors are a term often found in the context of custom Kubernetes controllers, where they're used to reduce the
/// load on the Kubernetes API server by caching the state of resources in memory. `Reflector` provides comparable
/// functionality, allowing for a single data source to be consumed, and then shared amongst multiple callers. However,
///
/// `Reflector` utilizes the concept of a _processor_, which dictates both the type of data that can be consumed and
/// data that gets stored. This means that `Reflector` is more than just a cache of the data source, but also
/// potentially a mapped version of it, allowing for transforming the data in whatever way is necessary.
pub struct Reflector<P: Processor> {
    store: Store<P>,
}

impl<P: Processor> Clone for Reflector<P> {
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
        }
    }
}

impl<P: Processor> Reflector<P> {
    /// Creates a new reflector with the given data source and processor.
    ///
    /// A reflector composes a source of data with a processor that's used to transform the data, and then stores
    /// the processed results. It can be listened to for updates, and cheaply shared. This allows multiple interested
    /// components to subscribe to the same data source without having to duplicate the processing or storage of the
    /// data.
    ///
    /// Returns the reflector alongside a [`ReflectorWorker`] that consumes the data source and feeds the processed
    /// items into the shared state. The reflector is usable immediately, but reports only the initial state until
    /// the worker is added to a [`Supervisor`][crate::runtime::Supervisor] and starts running.
    ///
    /// `Reflector` is cheaply cloneable and can either be cloned for each caller or shared between them (for example, via
    /// `Arc<T>`).
    pub fn new<S, I>(source: S, processor: P) -> (Self, ReflectorWorker<P, S>)
    where
        S: Stream<Item = I> + Unpin + Send + 'static,
        I: IntoIterator<Item = P::Input> + Send,
        P: 'static,
    {
        let store = Store::from_processor(processor);
        let worker = ReflectorWorker {
            store: store.clone(),
            source: Arc::new(Mutex::new(source)),
        };

        (Self { store }, worker)
    }
}

/// A worker that drives a [`Reflector`]'s data source.
///
/// Consumes items from the source and feeds them through the processor into the reflector's shared state. Until this
/// worker runs, the reflector it was created with reports only the initial state its processor built.
///
/// The source is retained across restarts rather than being rebuilt, so a worker that fails and is restarted resumes
/// from wherever the source left off. That matters for a source backed by a subscription: rebuilding it would drop
/// whatever accumulated while the worker was down.
pub struct ReflectorWorker<P: Processor, S> {
    store: Store<P>,
    source: Arc<Mutex<S>>,
}

#[async_trait]
impl<P, S, I> Supervisable for ReflectorWorker<P, S>
where
    P: Processor + 'static,
    S: Stream<Item = I> + Unpin + Send + 'static,
    I: IntoIterator<Item = P::Input> + Send,
{
    fn name(&self) -> &str {
        "reflector"
    }

    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        let store = self.store.clone();
        let source = Arc::clone(&self.source);

        Ok(Box::pin(async move {
            let mut source = source.lock_owned().await;

            select! {
                _ = process_shutdown => {},
                _ = drive_source(&mut *source, &store) => {},
            }

            Ok(())
        }))
    }
}

/// Feeds every item the source yields through the store, returning once the source is exhausted.
async fn drive_source<P, S, I>(source: &mut S, store: &Store<P>)
where
    P: Processor,
    S: Stream<Item = I> + Unpin,
    I: IntoIterator<Item = P::Input>,
{
    while let Some(inputs) = source.next().await {
        store.process(inputs);
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

#[cfg(test)]
mod tests {
    use std::{
        pin::Pin,
        sync::Mutex as StdMutex,
        task::{Context, Poll},
        time::Duration,
    };

    use tokio::{sync::mpsc, time::timeout};

    use super::*;

    /// Accumulates every input it is handed, in order.
    struct TestProcessor;

    impl Processor for TestProcessor {
        type Input = u32;
        type State = StdMutex<Vec<u32>>;

        fn build_initial_state(&self) -> Self::State {
            StdMutex::new(Vec::new())
        }

        fn process(&self, input: Self::Input, state: &Self::State) {
            state.lock().unwrap().push(input);
        }
    }

    /// A source fed by a channel, so tests control exactly when items become available.
    struct TestSource(mpsc::UnboundedReceiver<Vec<u32>>);

    impl Stream for TestSource {
        type Item = Vec<u32>;

        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            self.0.poll_recv(cx)
        }
    }

    fn build() -> (
        mpsc::UnboundedSender<Vec<u32>>,
        Reflector<TestProcessor>,
        ReflectorWorker<TestProcessor, TestSource>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        let (reflector, worker) = Reflector::new(TestSource(rx), TestProcessor);
        (tx, reflector, worker)
    }

    fn observed(reflector: &Reflector<TestProcessor>) -> Vec<u32> {
        reflector.state().lock().unwrap().clone()
    }

    /// Waits until the reflector has observed at least `expected` items, so that tests synchronize on the worker
    /// having made progress rather than on a fixed delay.
    async fn wait_for_len(reflector: &Reflector<TestProcessor>, expected: usize) {
        timeout(Duration::from_secs(5), async {
            while observed(reflector).len() < expected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("timed out waiting for the reflector to observe the expected items");
    }

    #[tokio::test]
    async fn reports_initial_state_before_worker_runs() {
        // Creating a reflector must not depend on its worker: callers acquire the handle during bootstrap, well
        // before the supervisor that drives the worker is running.
        let (tx, reflector, _worker) = build();

        tx.send(vec![1, 2, 3]).unwrap();

        assert_eq!(observed(&reflector), Vec::<u32>::new());
    }

    #[tokio::test]
    async fn worker_feeds_source_items_into_shared_state() {
        let (tx, reflector, worker) = build();

        tx.send(vec![1, 2]).unwrap();
        tx.send(vec![3]).unwrap();
        drop(tx);

        let fut = worker.initialize(ShutdownHandle::noop()).await.unwrap();
        fut.await.unwrap();

        assert_eq!(observed(&reflector), vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn worker_completes_when_source_ends() {
        let (tx, _reflector, worker) = build();
        drop(tx);

        let fut = worker.initialize(ShutdownHandle::noop()).await.unwrap();

        // An exhausted source is a terminal condition, so the worker returns rather than waiting for shutdown.
        timeout(Duration::from_secs(5), fut)
            .await
            .expect("worker should complete once the source is exhausted")
            .unwrap();
    }

    #[tokio::test]
    async fn worker_stops_on_shutdown_signal() {
        // The source stays open here, so only the shutdown signal can end the worker.
        let (_tx, _reflector, worker) = build();
        let (coordinator, shutdown) = ShutdownHandle::paired();

        let fut = worker.initialize(shutdown).await.unwrap();
        let handle = tokio::spawn(fut);

        coordinator.shutdown();

        timeout(Duration::from_secs(5), handle)
            .await
            .expect("worker should stop once shutdown is signalled")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn restarted_worker_resumes_from_the_same_source() {
        // A restart re-enters `initialize`, which must not rebuild the source: doing so would drop everything the
        // source accumulated while the worker was down.
        let (tx, reflector, worker) = build();

        tx.send(vec![1]).unwrap();

        let fut = worker.initialize(ShutdownHandle::noop()).await.unwrap();
        let handle = tokio::spawn(fut);
        wait_for_len(&reflector, 1).await;

        // Abort rather than shut down cleanly, standing in for the worker failing mid-flight.
        handle.abort();
        let _ = handle.await;

        // Sent while nothing is draining the source, so it can only be observed if the restart reuses it.
        tx.send(vec![2]).unwrap();
        drop(tx);

        let fut = worker.initialize(ShutdownHandle::noop()).await.unwrap();
        fut.await.unwrap();

        assert_eq!(observed(&reflector), vec![1, 2]);
    }

    #[tokio::test]
    async fn state_is_shared_across_clones() {
        let (tx, reflector, worker) = build();
        let cloned = reflector.clone();

        tx.send(vec![7]).unwrap();
        drop(tx);

        let fut = worker.initialize(ShutdownHandle::noop()).await.unwrap();
        fut.await.unwrap();

        assert_eq!(observed(&cloned), vec![7]);
    }
}
