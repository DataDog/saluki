# Building a Pipeline

This guide walks through the mechanics of assembling a Saluki pipeline from scratch. It covers
the `TopologyBlueprint` API, shows how each component type is implemented, and references
existing components you can read alongside this document.

> [!NOTE]
> This guide assumes you are familiar with the core vocabulary (topology, source, transform,
> destination, named output, etc.). If you're not, read
> [`docs/reference/architecture/index.md`](../reference/architecture/index.md) first.

---

## Prerequisites

Make sure you understand the following terms before continuing:

- **Topology** — a directed, acyclic graph (DAG) of components.
- **Source** — brings data *into* the topology.
- **Transform** — processes data in the middle of the topology.
- **Destination** — sends data *out of* the topology.
- **Default / named output** — how a component's output(s) are identified when wiring components together.
- **Event** — the unified in-topology data type (`Metric`, `Log`, `Trace`, …).

All of these are covered in depth in the [architecture reference](../reference/architecture/index.md).

---

## How a topology is constructed

A topology is assembled in three phases.

### Phase 1 — Blueprint

`TopologyBlueprint` (`lib/saluki-core/src/topology/blueprint.rs`) is the builder that holds
all component declarations and their connections. Nothing runs yet at this stage.

```rust
let mut blueprint = TopologyBlueprint::new("my-pipeline", &component_registry);

// Add components by giving each one a unique string ID and a builder instance.
blueprint
    .add_source("heartbeat", HeartbeatConfiguration::default())?
    .add_destination("sink", BlackholeConfiguration)?;

// Declare connections: connect_component(downstream_id, [upstream_output_ids])
blueprint.connect_component("sink", ["heartbeat"])?;
```

`add_source`, `add_transform`, `add_destination`, `add_encoder`, `add_forwarder`, `add_relay`,
and `add_decoder` are the seven component-type variants. Each returns `&mut Self` so calls can
be chained.

### Phase 2 — Build

`build()` validates the graph (types match, no cycles, at least one source and one destination)
and instantiates all component objects:

```rust
let built_topology = blueprint.build().await?;
```

If a component's `build()` returns an error, or if the graph has an invalid connection, this is
where it surfaces.

### Phase 3 — Spawn

`spawn()` starts each component as an independent Tokio async task and begins the data flow:

```rust
let mut running_topology = built_topology
    .spawn(&health_registry, memory_limiter)
    .await?;
```

After this call the pipeline is live. Shutdown is initiated by dropping or signalling
`running_topology` (see [architecture docs on shutdown](../reference/architecture/index.md#shutdown)).

---

## Connecting components: default and named outputs

When a component has a **single output**, connect to it with just its ID:

```rust
blueprint.connect_component("aggregate", ["my_source"])?;
//                                        ^ component ID alone = default output
```

When a component has **multiple named outputs** (e.g. DogStatsD emits `metrics`, `events`, and
`service_checks` on separate outputs), use the `"component_id.output_name"` notation:

```rust
blueprint.connect_component("metrics_transform", ["dsd_in.metrics"])?;
blueprint.connect_component("events_encoder",    ["dsd_in.events"])?;
blueprint.connect_component("checks_encoder",    ["dsd_in.service_checks"])?;
```

A component declares its outputs in its `SourceBuilder::outputs()` (or equivalent) method using
`OutputDefinition::default_output(event_type)` for a single output, or
`OutputDefinition::named_output(name, event_type)` for named ones.

---

## Writing a source

A source consists of three pieces:

1. **A config struct** — holds settings, implements `SourceBuilder` and `MemoryBounds`.
2. **A runtime struct** — implements `Source`.

### Minimal example: `HeartbeatConfiguration`

Full source: `lib/saluki-components/src/sources/heartbeat/mod.rs`

```rust
// 1. Config struct — deserialized from configuration, describes one output.
pub struct HeartbeatConfiguration {
    pub heartbeat_interval_secs: u64,
}

#[async_trait]
impl SourceBuilder for HeartbeatConfiguration {
    // Declare the outputs this source emits.
    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        static OUTPUTS: &[OutputDefinition<EventType>] =
            &[OutputDefinition::default_output(EventType::Metric)];
        OUTPUTS
    }

    // Instantiate the runtime struct.
    async fn build(&self, _context: ComponentContext) -> Result<Box<dyn Source + Send>, GenericError> {
        Ok(Box::new(Heartbeat {
            heartbeat_interval_secs: self.heartbeat_interval_secs,
        }))
    }
}

impl MemoryBounds for HeartbeatConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder.minimum().with_single_value::<Heartbeat>("component struct");
    }
}

// 2. Runtime struct — does the actual work.
struct Heartbeat { heartbeat_interval_secs: u64 }

#[async_trait]
impl Source for Heartbeat {
    async fn run(self: Box<Self>, mut context: SourceContext) -> Result<(), GenericError> {
        // Always grab the shutdown handle and health handle first.
        let mut global_shutdown = context.take_shutdown_handle();
        let mut health = context.take_health_handle();
        let mut tick = interval(Duration::from_secs(self.heartbeat_interval_secs));

        health.mark_ready(); // signal that the component is up

        loop {
            select! {
                _ = &mut global_shutdown => break,  // topology is shutting down
                _ = health.live() => continue,       // keep-alive tick
                _ = tick.tick() => {
                    // Get a buffered view of the default output and push events.
                    let mut out = context.dispatcher()
                        .buffered()
                        .expect("default output must exist");
                    out.push(Event::Metric(Metric::gauge(ctx, 1.0))).await?;
                    out.flush().await?;
                }
            }
        }
        Ok(())
    }
}
```

Key points:
- Call `health.mark_ready()` as soon as the component is operational.
- Always select on `global_shutdown` and `health.live()` in addition to your work branch.
- Use `context.dispatcher().buffered()` to send to the default output; use
  `.buffered_for_output("output_name")` for named outputs.
- Flush the buffered dispatcher after filling it; events are not sent until flushed.

---

## Writing a transform

There are two flavours of transform.

### Synchronous transform (preferred for simple per-event mutations)

Synchronous transforms implement `SynchronousTransform::transform_buffer()`, which receives a
mutable `EventsBuffer` and operates on it in-place. They don't deal with channels or async
at all — that is handled for them by the framework.

Full source: `lib/saluki-components/src/transforms/host_enrichment/mod.rs`

```rust
// Config struct implements SynchronousTransformBuilder.
pub struct HostEnrichmentConfiguration<E> { env_provider: E }

#[async_trait]
impl<E: EnvironmentProvider + Send + Sync + 'static> SynchronousTransformBuilder
    for HostEnrichmentConfiguration<E>
{
    async fn build(&self, _ctx: ComponentContext)
        -> Result<Box<dyn SynchronousTransform + Send>, GenericError>
    {
        Ok(Box::new(HostEnrichment::from_environment_provider(&self.env_provider).await?))
    }
}

// Runtime struct.
pub struct HostEnrichment { hostname: Arc<str> }

impl SynchronousTransform for HostEnrichment {
    fn transform_buffer(&mut self, buffer: &mut EventsBuffer) {
        for event in buffer {
            if let Some(metric) = event.try_as_metric_mut() {
                if metric.metadata().hostname().is_none() {
                    metric.metadata_mut().set_hostname(self.hostname.clone());
                }
            }
        }
    }
}
```

Multiple synchronous transforms can be fused into a single component using
`ChainedConfiguration` (`lib/saluki-components/src/transforms/chained/mod.rs`), which avoids
per-transform channel overhead when several lightweight operations are applied in sequence.

### Async transform

For transforms that need timers, I/O, or their own internal tasks, implement the full
`Transform` trait instead:

```rust
#[async_trait]
impl Transform for MyTransform {
    async fn run(self: Box<Self>, mut context: TransformContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();
        health.mark_ready();

        // Receive event buffers from upstream.
        while let Some(events) = context.events().next().await {
            // process events …

            // Forward downstream.
            let mut out = context.dispatcher().buffered().expect("default output");
            for event in events { out.push(event).await?; }
            out.flush().await?;
        }
        // Channel closed → all upstream senders shut down. Clean exit.
        Ok(())
    }
}
```

Key points:
- The event loop ends naturally when `context.events().next()` returns `None` — this means all
  upstream sources have shut down and the channel is drained. Do not add explicit shutdown logic
  for this case.
- If you have timers or background tasks, select over both the event stream and your other
  futures, similar to the source pattern above.

---

## Writing a destination

A destination receives events and sends them somewhere external. It only needs to handle the
input side — there is no downstream dispatcher.

Full source: `lib/saluki-components/src/destinations/blackhole/mod.rs`

```rust
// Config struct — implements DestinationBuilder and MemoryBounds.
#[derive(Default)]
pub struct BlackholeConfiguration;

#[async_trait]
impl DestinationBuilder for BlackholeConfiguration {
    // Declare which event types this destination accepts.
    fn input_event_type(&self) -> EventType { EventType::all_bits() }

    async fn build(&self, _ctx: ComponentContext)
        -> Result<Box<dyn Destination + Send>, GenericError>
    {
        Ok(Box::new(Blackhole))
    }
}

impl MemoryBounds for BlackholeConfiguration { /* … */ }

// Runtime struct.
struct Blackhole;

#[async_trait]
impl Destination for Blackhole {
    async fn run(mut self: Box<Self>, mut context: DestinationContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();
        health.mark_ready();

        loop {
            select! {
                _ = health.live() => continue,
                result = context.events().next() => match result {
                    Some(events) => { /* consume events */ }
                    None => break, // all upstream senders shut down
                }
            }
        }
        Ok(())
    }
}
```

---

## Putting it all together — a minimal pipeline

```rust
use memory_accounting::ComponentRegistry;
use saluki_components::{
    destinations::BlackholeConfiguration,
    sources::HeartbeatConfiguration,
};
use saluki_core::topology::TopologyBlueprint;
use saluki_health::HealthRegistry;

async fn run_minimal_pipeline() -> Result<(), saluki_error::GenericError> {
    let component_registry = ComponentRegistry::default();
    let health_registry = HealthRegistry::new();

    // Phase 1 — Blueprint
    let mut blueprint = TopologyBlueprint::new("minimal", &component_registry);
    blueprint
        .add_source("heartbeat", HeartbeatConfiguration::default())?
        .add_destination("sink", BlackholeConfiguration)?;
    blueprint.connect_component("sink", ["heartbeat"])?;

    // Phase 2 — Build (validates graph + constructs components)
    let built = blueprint.build().await?;

    // Phase 3 — Spawn (starts all async tasks)
    let memory_limiter = None; // no limit for this example
    let mut topology = built.spawn(&health_registry, memory_limiter).await?;

    // Wait for SIGINT or unexpected component exit.
    topology.wait_for_unexpected_finish().await;
    topology.shutdown_with_timeout(std::time::Duration::from_secs(10)).await?;

    Ok(())
}
```

---

## Real-world example: ADP's DogStatsD pipeline

The production pipeline in `bin/agent-data-plane/src/cli/run.rs` (`create_topology` function)
illustrates a more involved topology. The data flow for DogStatsD metrics is:

```
dsd_in (DogStatsDConfiguration)
  ├─[.metrics]──▶ dsd_prefix_filter (DogStatsDPrefixFilterConfiguration)
  │                  ▶ dsd_enrich (HostEnrichmentConfiguration, via Chained)
  │                      ▶ dsd_tag_filterlist (TagFilterlistConfiguration)
  │                          ▶ dsd_agg (AggregateConfiguration)
  │                              ▶ metrics_enrich (HostEnrichmentConfiguration)
  │                                  ▶ dd_metrics_encode (DatadogMetricsConfiguration)
  │                                      ▶ dd_out (DatadogConfiguration forwarder)
  ├─[.events]───▶ dd_events_encode (DatadogEventsConfiguration)
  │                  ▶ dd_out
  └─[.service_checks]▶ dd_service_checks_encode (DatadogServiceChecksConfiguration)
                          ▶ dd_out
```

Notice:
- `dsd_in` uses three **named outputs** (`.metrics`, `.events`, `.service_checks`).
- The metrics path uses several synchronous transforms chained together for efficiency.
- A single forwarder (`dd_out`) receives inputs from three different encoders — components may
  have multiple upstream connections.

Each component in this pipeline has a corresponding implementation under
`lib/saluki-components/src/`.

---

## Where to add your component

```
lib/saluki-components/src/
├── sources/
│   ├── mod.rs          ← re-export your new source here
│   └── my_source/
│       └── mod.rs      ← implement SourceBuilder + Source + MemoryBounds here
├── transforms/
│   ├── mod.rs
│   └── my_transform/
│       └── mod.rs
└── destinations/
    ├── mod.rs
    └── my_destination/
        └── mod.rs
```

1. Create `lib/saluki-components/src/{sources,transforms,destinations}/my_component/mod.rs`.
2. Re-export the configuration type from the parent `mod.rs`.
3. Wire it into a topology (e.g. `bin/agent-data-plane/src/cli/run.rs` or your own binary).

---

## Testing your pipeline

Use `BlackholeConfiguration` as a drop-in destination during development — it accepts any event
type and logs throughput without needing a real external service.

For more structured testing see [`docs/development/testing.md`](testing.md), which covers:

- **Unit tests** — test individual component logic with `cargo nextest`.
- **Integration / smoke tests** — spin up a full topology in a container and verify it starts
  and responds correctly (`make test-integration`).
- **Correctness tests** — run your pipeline alongside the Datadog Agent and compare outputs
  semantically (`make test-correctness`).
