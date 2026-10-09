//! The per-buffer cost of a region host: a region whose driver hands every batch to its one exit, against a
//! hand-written transform that receives each buffer and dispatches it, both run with the same input channel,
//! dispatcher, and downstream channel.
//!
//! What differs is the host's own work per buffer: its select over liveness, control messages, timers, and input,
//! the supervised call into the driver, and the dispatch of the driver's output by exit name. Small buffers make
//! that work the larger share; full buffers show what it adds to a batch.

use std::{num::NonZeroUsize, sync::Arc};

use async_trait::async_trait;
use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, Throughput};
use saluki_core::{
    accounting::{ComponentRegistry, MemoryLimiter},
    components::{
        transforms::{Transform, TransformBuilder, TransformContext},
        BuildContext, ComponentContext,
    },
    data_model::event::{metric::Metric, Event},
    execution::{
        control_mailbox, ExitId, ExitSink, PanicOutcome, Progress, RegionDriver, RegionFailure, RegionHostBuilder,
        RegionPanic, RegionTurn,
    },
    health::HealthRegistry,
    runtime::state::{DataspaceRegistry, ResourceRegistry},
    support::SubsystemIdentifier,
    topology::{
        interconnect::{Consumer, Dispatcher},
        ComponentId, EventsBuffer, OutputName, TopologyContext,
    },
};
use saluki_error::GenericError;
use tokio::{runtime::Handle, sync::mpsc};

/// The buffers each iteration sends through.
const BUFFERS: usize = 256;

/// A driver that hands every batch, whole, to its one exit.
struct Forward;

impl RegionDriver for Forward {
    type Control = ();

    fn process(&mut self, _turn: &mut RegionTurn<'_>, batch: EventsBuffer, exits: &mut ExitSink) -> Progress {
        exits.emit(ExitId::new(0), batch);
        Progress::Done
    }

    fn control(
        &mut self, _turn: &mut RegionTurn<'_>, _message: (), _exits: &mut ExitSink,
    ) -> Result<(), RegionFailure> {
        Ok(())
    }

    fn reset_after_panic(&mut self, _panic: &RegionPanic) -> PanicOutcome {
        PanicOutcome::Continue
    }
}

/// A hand-written transform that dispatches every buffer it receives, as a minimal native transform does.
struct Plain;

#[async_trait]
impl Transform for Plain {
    async fn run(self: Box<Self>, mut context: TransformContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();
        health.mark_ready();
        loop {
            tokio::select! {
                _ = health.live() => {}
                buffer = context.events().next() => match buffer {
                    Some(buffer) => {
                        let _ = context.dispatcher().dispatch(buffer).await;
                    }
                    None => return Ok(()),
                },
            }
        }
    }
}

/// A transform running on its own task, its input channel, and its downstream channel.
struct Running {
    input: mpsc::Sender<EventsBuffer>,
    output: mpsc::Receiver<EventsBuffer>,
}

/// Starts `transform` with an input channel and a downstream channel on `output`, the transform's output.
fn start(transform: Box<dyn Transform + Send>, output: OutputName) -> Running {
    let topology = SubsystemIdentifier::from_segments(["bench"]);
    let component = ComponentContext::transform(&topology, ComponentId::try_from("region").unwrap());
    let health_registry = HealthRegistry::new();
    let health = health_registry
        .register_component(&component.identity())
        .expect("the component registers once");
    let topology_context = TopologyContext::new(
        Arc::from("bench"),
        MemoryLimiter::noop(),
        health_registry,
        Handle::current(),
        DataspaceRegistry::new(),
    );
    let (output_tx, output_rx) = mpsc::channel(BUFFERS);
    let mut dispatcher = Dispatcher::new(component.clone());
    dispatcher.add_output(output.clone()).unwrap();
    dispatcher.attach_sender_to_output(&output, output_tx).unwrap();
    let (input_tx, input_rx) = mpsc::channel(BUFFERS);
    let context = TransformContext::new(
        &topology_context,
        &component,
        ComponentRegistry::default(),
        health,
        dispatcher,
        Consumer::new(component.clone(), input_rx),
    );
    tokio::spawn(transform.run(context));
    Running {
        input: input_tx,
        output: output_rx,
    }
}

/// Returns `buffers` buffers of `events` metrics each.
fn buffers(buffers: usize, events: usize) -> Vec<EventsBuffer> {
    (0..buffers)
        .map(|_| {
            let mut buffer = EventsBuffer::with_capacity(events);
            for _ in 0..events {
                assert!(buffer
                    .try_push(Event::Metric(Metric::counter("bench.counter", 1.0)))
                    .is_none());
            }
            buffer
        })
        .collect()
}

/// Sends every buffer through, and waits until every one has come out.
async fn send_through(running: &mut Running, buffers: Vec<EventsBuffer>) -> usize {
    let count = buffers.len();
    let sending = {
        let input = running.input.clone();
        tokio::spawn(async move {
            for buffer in buffers {
                input.send(buffer).await.unwrap();
            }
        })
    };
    for _ in 0..count {
        running.output.recv().await.unwrap();
    }
    sending.await.unwrap();
    count
}

fn bench_dispatch(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let mut group = c.benchmark_group("region_host_dispatch");
    for events in [1, 1024] {
        group.throughput(Throughput::Elements(BUFFERS as u64));
        let mut region = runtime.block_on(async {
            let (_control, mailbox) = control_mailbox::<()>();
            let builder =
                RegionHostBuilder::from_driver(["out"], NonZeroUsize::new(8).unwrap(), mailbox, Forward).unwrap();
            let topology = SubsystemIdentifier::from_segments(["bench"]);
            let component = ComponentContext::transform(&topology, ComponentId::try_from("region").unwrap());
            let transform = builder
                .build(BuildContext::new(component, ResourceRegistry::new()))
                .await
                .unwrap();
            start(transform, OutputName::Given("out".into()))
        });
        let mut plain = runtime.block_on(async { start(Box::new(Plain), OutputName::Default) });

        group.bench_function(
            BenchmarkId::new("region_host", format!("{} events per buffer", events)),
            |b| {
                b.iter_batched(
                    || buffers(BUFFERS, events),
                    |buffers| runtime.block_on(send_through(&mut region, buffers)),
                    BatchSize::SmallInput,
                )
            },
        );
        group.bench_function(
            BenchmarkId::new("plain_transform", format!("{} events per buffer", events)),
            |b| {
                b.iter_batched(
                    || buffers(BUFFERS, events),
                    |buffers| runtime.block_on(send_through(&mut plain, buffers)),
                    BatchSize::SmallInput,
                )
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_dispatch);
criterion_main!(benches);
