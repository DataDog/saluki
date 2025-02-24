# Architecture

Saluki is a toolkit for building observability data planes. It provides a set of building blocks for
ingesting, processing, and forwarding observability data, as well as all of the related helpers for
doing so in a performant, reliable, and efficient way.

## Core Concepts

### Topologies

In Saluki, users construct a **topology**, which is a collection of uniquely identified
**components** connected together in a directed, acyclic graph. This just means that components send
data to each other in a single direction, like a pipeline of Unix commands (e.g., `cat messages |
grep error | wc -l`), and that there can be no cycles between components (e.g., componentA ->
component B -> component A).

OK, so why does this matter? This structure allows components to be easily composed together, and it
allow for a component to send data to multiple downstream components _or_ for a component to receive
data from multiple upstream components.

Topologies are required to have an input component and an output component, as we need a way for
data to enter the topology as well as leave it. This means a topology needs to have _at least_ two
components, but could have arbitrarily more. Connections between components are manually specified
using the unique identifiers, such as declaring a connection between "A" and "B", where "A" and "B"
are both the names of components in the topology.

Below is an visual example of a simple topology -- source, transform, and destination -- as well as
an example of a more complex topology with multiple sources, transforms, and destinations:

```text
Simple:                                                           
┌─────────────┐        ┌───────────────┐       ┌─────────────────┐
│  DogStatsD  ├────────▶   Aggregate   ├───────▶   DD Metrics    │
│  (source)   │        │  (transform)  │       │  (destination)  │
└─────────────┘        └───────────────┘       └─────────────────┘
                                                                  
Slightly complex:                                                 
┌────────────────────┐        ┌───────────────┐                   
│  Internal Metrics  ├────────▶   Aggregate   ├──────────┐       
│      (source)      │        │  (transform)  │          │       
└────────────────────┘        └───────────────┘          │       
                                                         │       
                                                         │       
┌─────────────────┐      ┌───────────────┐     ┌─────────▼───────┐
│  OpenTelemetry  ├──────▶   Aggregate   ├─────▶    DD Metrics   │
│    (source)     │      │  (transform)  │     │  (destination)  │
└─────┬──────┬────┘      └───────────────┘     └─────────────────┘
      │      │       ▲                                           ▲
	  │      │       └────────────────── metrics ────────────────┘
      │      │                                                     
      │      │          ┌───────────────┐      ┌─────────────────┐
      │      └──────────▶    Redact     ├──────▶     DD Logs     │
      │                 │  (transform)  │      │  (destination)  │
      │                 └───────────────┘      └─────────────────┘
      │      ▲                                                   ▲
	  │      └────────────────────── logs ───────────────────────┘
      │                                                            
	  |      ┌───────────────────── traces ──────────────────────┐
	  |      ▼                                                   ▼
      │                                        ┌─────────────────┐
      └────────────────────────────────────────▶    DD Traces    │
                                               │  (destination)  │
                                               └─────────────────┘
```

### Components

Components are the discrete chunks of functionality that make up a topology, and are grouped into
three categories: **sources**, **transforms**, and **destinations**. All components interact with
each other over channels, which are the primary mechanism for sending data from one component to
another.

Components implement specific traits (an `interface` in Go) in order to indicate what type of
component they are, and these traits also requiring describing the input and/or output data types
they support, which is used by the topology graph to ensure that components are connected correctly.

#### Sources

Sources are the group of components used to get data into a topology.

There are no real limitations on how to get data in, but generally speaking, most sources are either
_push-_ or _pull-based_, where data is either _pushed_ in, such as by a client, or _pulled_ in, such
as by querying an external service. In some cases, a source might be able to generate its own data.

Examples of sources (some of which do not currently exist):

- DogStatsD (receive metrics from StatsD/DogStatsD clients)
- File (pull data from files on disk)
- Internal metrics (capture internal telemetry generated by Saluki)

#### Transforms

Transforms are the group of components used to process data within a topology.

Broadly speaking, transforms are used to either combine data (e.g., aggregation), modify data (e.g.,
enrichment, conversion) or filter data (e.g., dropping, sampling). Transforms are always in the
"middle" of a topology, as they don't generate data themselves.

Examples of transforms (some of which do not currently exist):

- Aggregate (aggregate metrics over a time window, based on their name/tags)
- Origin Enrichment (enrich metrics with additional tags based on their point of origin)
- Sampling (deterministically allow a certain percentage of events to pass through, dropping the
  rest)
- Router (route events to different outputs based on configurable logic)

#### Destinations

Destinations are the group of components used to send data out of a topology.

Like sources, there are no real limitations on how to get data out, and most destinations will
either be push or pull, but the majority of destinations will be push-based, where data is pushed to
an external system.

Examples of destinations (some of which do not currently exist):

- Datadog Metrics (send metrics to the Datadog platform)
- Prometheus Scrape (expose a Prometheus-compatible scrape endpoint for metrics)
- OpenTelemetry (send logs, metrics, and traces to an OpenTelemetry receiver)

### Events

In order to facilitate communication between components in a generic way, Saluki uses a unified data
model based on a single enum type, `Event`. Events represent all possible data types that Saluki is
able to handle, such as metrics (currently supported), logs and traces (not yet supported), and so
on.

Naturally, not every component will emit all event types, and not every component will be able to
handle all event types. This is dealt with by the logic mentioned prior, where connected components
must have a compatible set of input/output data types. For example, if component A and component B
are connected together, and A only emits metrics while B only accepts logs, the topology would throw
an error during validation.

On the component side, components will do a minimal amount of runtime checking / destructing to
unwrap `Event`s and access the true event, such as the actual `Metric` container within.

### Outputs

Source and transform components can emit their data in one of two ways: using a default output, or a
**named** output.

Default outputs are exactly what they sound like, and are used as the primary output of a component.
This is the pattern used when a component only emits a single event type, and has no specialization
otherwise.

For some components, however, they may emit multiple event types, or they may dynamically emit
certain events/event types based on their configuration. In order to support this, Saluki has a
concept of "named" outputs, where a component can have a dynamic number of outputs, each with a
qualified name.

The type of output used influences how the component connections are declared, where connecting to
the default output of a component is achieved by specifying just the unique identifier of the
upstream component, but connecting to a named output uses a compound identifier, based on the
component's unique identifier and the name of the output.

For example, a hypothetical OpenTelemetry source could receive logs, metrics, or traces from
clients. It would be inefficient to have a default output that emitted all three event types, since
all connected downstream components would have to be able to handle all of those event types, even
if it just meant forwarding the ones they didn't care about. Instead, named outputs could be used to
send metrics, logs, and traces each on their own dedicated outputs. This would allow downstream
components to connect only to the named output that had the event type they cared about, such as
first sending the metrics to an aggregate transform while sending logs and traces directly to an
OpenTelemetry destination.

Another example would be a hypothetical router transform used to route log events based on their
severity. There could be a route for low-priority logs and one for high-priority logs, where each
route would create a specific named output on the transform. The topology could then be configured
to connect the low-priority output to a destination that perhaps batches logs more aggressively, and
results in less frequent writes, while the high-priority output could be connected to a destination
that prioritizes real-time ingestion.

Below is an example of the available outputs of two different components, where one component has
only a default output, and the other has multiple named outputs:

```text
┌──────────────────────────┐                                                            
│                          │                                                            
│    DogStatsD (source)    │           default metrics output (output ID: "dsd")        
│                          ├───────────────────────────────────────────────────────────▶
│   Component ID: "dsd"    │                                                            
│                          │                                                            
└──────────────────────────┘                                                            
                                                                                        
                                       named metrics output (output ID: "otel.metrics") 
                                    ┌──────────────────────────────────────────────────▶
┌──────────────────────────┐        │                                                   
│                          ├────────┘                                                   
│  OpenTelemetry (source)  │           named logs output (output ID: "otel.logs")       
│                          ├───────────────────────────────────────────────────────────▶
│   Component ID: "otel"   │                                                            
│                          ├────────┐                                                   
└──────────────────────────┘        │  named traces output (output ID: "otel.traces")   
                                    └──────────────────────────────────────────────────▶
```

### Shutdown

Topologies provide ordered shutdown through two mechanisms: the used of a shutdown coordinator, and
the implicit behavior of the channels used to connect components together.

Shutdown starts at the top level, controlled by the topology itself. A signal is sent to all sources
indicating that shutdown should proceed. Sources will then begin to shutdown, stopping new
data/connections/etc from coming in, as well as waiting for existing work to complete. Once a source
shuts down, it signals back to the topology that it is done. Once all sources have signaled that
they have shut down, the topology waits for all remaining components to complete as well.

However, transforms and destinations are not signaled directly to shutdown. Instead, they depend on
the implicit behavior of the channels that are used for receiving events. Once these channels have
been drained of any remaining events, and all of the senders have shutdown, the channel will be
marked as closed. This lets transforms and destination focus on simply receiving from the channel
until it is closed, at which point they will naturally complete and shutdown.

By triggering shutdown at the source level, and then having each subsequent component process any
remaining events, we ensure that all remaining events are processed before the topology is
completely shutdown.

### Runtime

Constructing a topology is split into two phases of _building_ and _spawning_, which ensures that we
can validate that each component in the topology as being both configured and connected correctly, and
then finally spawn the topology to begin accepting, processing, and forwarding data.

When a topology is spawned, we do so by using an asynchronous runtime, where each component is
treated as an individual "task," and individual components can spawn their own tasks. Saluki uses
[Tokio](https://docs.rs/tokio) as the underlying runtime implementation, as it provides a
high-performance, work-stealing runtime that is well-suited for running data-intensive pipelines
such as the ones built with Saluki.

#### Concurrency and parallelism

Asynchronous runtimes in Rust are based on "futures", which models computation that depends on
external resources (I/O, timers, messages, etc) which may become ready at an arbitrary point in time
in the _future_. We spawn these futures as _tasks_. If you're familiar with JavaScript's _promises_,
or Go's _goroutines_, you can think of futures as a similar concept. These tasks are scheduled
across multiple operating system threads, and are able to run concurrently, and potentially in
parallel, allowing for a smaller number of OS threads to effectively (and resource efficiently) run
many tasks.

#### Task isolation

Tasks also provide a natural level of isolation between each other, and so are useful for splitting
up independent workloads, such as spawning a separate task for each client connection or for each
log file being read.

In fact, splitting computation into more granular tasks is ideal, as it helps to allow for better
balancing the work across the runtime's worker threads. As Tokio is a work-stealing runtime, idle
worker threads can "steal" tasks from other worker threads when they are busy or blocked.
