# Typed components spike

This spike answers one question: how does a component like `Transform<Metric, Log>` actually run in a topology? Where
do the concrete `Metric` and `Log` types meet the topology, and what happens at runtime when a component consumes and
dispatches them? It's built on top of the single-type event buffers from #3078, and is meant for discussion, not for
merging as is.

## Short answer

Interconnects keep carrying `EventsBuffer`, which since #3078 is a `Vec` of a single event type behind a tag. A typed
component never sees that tag:

- **Consuming**: when a buffer arrives, the consumer checks its tag once and moves the `Vec<Metric>` out of it. That's
  a `match` on an enum, the same cost whether the buffer holds 1 event or 1024.
- **Dispatching**: when the component dispatches a buffer of logs, the dispatcher wraps its `Vec<Log>` back in an
  `EventsBuffer`. That's a move, with no work per event.
- **Building the topology**: the topology checks once that every event type that can reach a typed component is a type
  it consumes, and fails to build if not. This is where the flexibility of configuration is resolved.

Nothing is transmuted, downcast with `Any`, or converted per event. Everything inside the component is ordinary typed
Rust, and components that work with `Event` are unchanged.

## What a typed component looks like

```rust
struct MetricsToLogs;

#[async_trait]
impl Transform<Metric, Log> for MetricsToLogs {
    async fn run(self: Box<Self>, mut context: TransformContext<Metric, Log>) -> Result<(), GenericError> {
        // `metrics` is a `TypedEventsBuffer<Metric>`, a fixed-size `Vec<Metric>`.
        while let Some(metrics) = context.events().next().await {
            let logs = metrics.into_iter().map(|metric| Log::new(metric.context().name().to_string()));
            // The buffered dispatcher only accepts `Log`.
            context.dispatcher().buffered()?.send_all(logs).await?;
        }
        Ok(())
    }
}
```

Its builder wraps it in `Typed`, which is what lets the topology run it like any other transform:

```rust
async fn build(&self, _context: BuildContext) -> Result<Box<dyn Transform + Send>, GenericError> {
    Ok(Box::new(Typed::new(MetricsToLogs)))
}
```

`Transform` and `TransformContext` take the consumed and dispatched types as parameters that default to `Event`, so
`impl Transform for X` and `TransformContext` on their own mean exactly what they meant before. Components can move to
typed APIs one at a time.

The spike converts two real transforms:

- `apm_stats` is `Transform<Trace, TraceStats>`. It iterates over traces directly and dispatches its `TraceStats`
  without wrapping each one in `Event::TraceStats`.
- `metric_filter` is `Transform<Metric, Metric>`. It filters the buffer it received in place and forwards that same
  buffer, where it used to copy the remaining metrics into a new one.

## How it works

The pieces, from the bottom up:

| Piece | Where | What it does |
|---|---|---|
| `TypedEventBuffer<T, N>` (`TypedEventsBuffer<T>`) | `topology/interconnect/typed_event_buffer.rs` | A fixed-size `Vec<T>`. Converts to and from `FixedSizeEventBuffer<N>` by moving the `Vec` |
| `FromInterconnect<W>` | `topology/interconnect/mod.rs` | Converts what an interconnect carries into what a component consumes. Every type converts from itself |
| `Consumer<T, W = T>`, `Dispatcher<T, W = T>` | `topology/interconnect/` | The interconnect carries `W`, the component sees `T`. The consumer converts on receive, the dispatcher on send. With the default `W = T`, nothing changes |
| `BufferItem` | `topology/mod.rs` | Maps an item type to its buffer: `Event` to `EventsBuffer`, each event type `T` to `TypedEventsBuffer<T>` |
| `Transform<I = Event, O = Event>`, `TransformContext<I = Event, O = Event>` | `components/transforms/` | The component API, generic over what it consumes and dispatches |
| `Typed<C, I, O>` | `components/transforms/mod.rs` | Runs a `Transform<I, O>` as a `Transform`: converts its context when it starts, and reports `I`'s event types |
| Build-time check | `TopologyBlueprint` build | Fails if a transform can receive event types it doesn't consume |

At startup, the runner builds every transform as a `Box<dyn Transform>` and hands it a `TransformContext` holding an
`EventsConsumer` and an `EventsDispatcher`, exactly as before. `Typed::run` converts that context to a
`TransformContext<I, O>` with `with_item_type`, which only changes the types the consumer hands out and the dispatcher
accepts. The channels, outputs, and senders are the same ones.

### The build-time check

The graph only requires the event types on each side of an edge to overlap, and buffers are sent whole, so a
transform that declares it accepts metrics can still be sent logs. A component that works with `Event` handles that by
ignoring them. A typed component can't express that it received a log, so the topology checks it instead: after
building each transform, it compares the event types that can reach it (`Graph::effective_event_types`, from #3078)
with the types it consumes, and fails with an error like:

```text
Transform 'metrics' consumes Metric events, but can receive Metric|Log events.
```

Transforms that work with `Event` consume every type, so they always pass.

If a buffer of the wrong type does arrive at runtime anyway, for example from a test driver that skips the topology,
the consumer drops it and counts it in a new `component_events_rejected_total` metric, so the component never sees it.

## What the spike doesn't cover

These are the open questions to discuss before doing this for real.

- **Other component kinds.** Only transforms are typed. Sources, decoders, destinations, and encoders would follow the
  same pattern: a type parameter on each trait and context, defaulting to `Event`, plus a `Typed` adapter.
- **Outputs of different types.** `O` is one type for every output. `trace_sampler` emits traces on its default output
  and metrics on `metrics`, and the DogStatsD source emits a different type on each of its three outputs. These need a
  typed dispatcher per output, for example `context.output::<Metric>("metrics")`, checked against the declared outputs
  when the topology is built.
- **Inputs of more than one type.** Some components consume several event types, such as destinations and encoders
  that accept every type, or `host_enrichment`. They can keep using `Event`. Or a component could define its own small
  enum implementing `BufferItem`, which is still one tag check per buffer.
- **Declarations duplicate the types.** A builder's `input_event_type()` and `outputs()` still say what the component's
  type parameters already say. A typed builder trait could derive them from `I` and `O`, which would also let the graph
  run the check before any component is built.
- **Synchronous transforms.** `SynchronousTransform` works on `&mut EventsBuffer`. A typed version would take
  `&mut TypedEventsBuffer<T>`, and chained transforms would need a common type.
- **Fully typed channels.** Interconnects could carry `TypedEventsBuffer<T>` on edges that only carry `T`, with the
  channel type chosen when the topology is built. That would skip the per-buffer tag check, but it's one check per
  1024 events, and it would mean erasing channel types with `Any` at build time. The spike suggests it isn't needed.

## Something found along the way

`BufferedDispatcher::flush` increments `events_sent_total` by the number of buffered items, and `DispatchTarget::send`
increments it again for each buffer it sends, so items sent through a buffered dispatcher look like they're counted
twice. The comment in `flush` predates `send` counting items. This is separate from the spike, but `metric_filter` now
dispatches its buffer directly, so its count would halve if this is real.
