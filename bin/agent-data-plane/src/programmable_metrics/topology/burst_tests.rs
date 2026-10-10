//! Bursts, the plan-driven topology against the legacy one: how many batches each takes from a component before the
//! component has to wait, while the component at the end of the stretch it feeds reads nothing.
//!
//! Natively, each transform of the metrics path has an input of 128 buffers and holds one more buffer while it waits to
//! send, so a component waits to send once every input and transform between it and a stalled component is full.
//! During a burst, the DogStatsD source then stops reading its socket, which drops what does not fit; and while
//! aggregation sends a large flush, it reads nothing, and what reaches it backs up to the source. The plan-driven
//! topology runs several of those transforms in one region, whose input takes what theirs held together (see
//! `region_input_capacity`), so both topologies take exactly as many batches before backpressure reaches the source,
//! and deliver every one of them once the stalled component reads.
//!
//! The clock is paused, so a test can wait for a topology to settle: a sleep ends only once every task waits.

use std::time::Duration;

use saluki_core::data_model::event::metric::{context::Context, Metric, MetricValues};

use crate::programmable_metrics::{
    lowering::matrix::matrix_entry,
    test_support::configuration_system,
    test_topology::{Aggregation, Delivery, MetricsPath, Options, TestTopology, DSD_AGG},
};

/// How many batches of one metric each a burst sends: more than any stretch of the metrics path holds.
const BATCHES: usize = 1_000;

/// How long a test lets a topology settle, on the paused clock, before it counts what the burst's component sent.
const SETTLE: Duration = Duration::from_secs(10);

/// What a native transform holds: a full input, and one buffer while it waits to send.
const TRANSFORM: usize = 128 + 1;

/// What the input of the stalled component at the end of a stretch holds.
const STALLED: usize = 128;

/// One burst: where it enters, what stalls, and what the native components on the way hold.
struct Burst {
    /// What the burst shows.
    name: &'static str,

    /// The configuration.
    config: &'static str,

    /// What runs aggregation: gated, to stall the stretch before it, or scripted, to send the burst after it.
    aggregation: Aggregation,

    /// The feed the burst enters through.
    feed: &'static str,

    /// The name of the burst's metrics, which every component on the way lets through.
    metric: &'static str,

    /// The gate of the component that reads nothing until the burst has settled: aggregation, or an output.
    stalled: &'static str,

    /// The output that receives the burst.
    output: &'static str,

    /// How many batches the stretch holds before the feed's component waits, from the native components on it.
    held: usize,
}

/// The bursts: before aggregation, through one region and through a pipeline of three; after aggregation, through
/// enrichment to the main output, and through a gateway to its output.
fn bursts() -> [Burst; 5] {
    [
        // The mapper, the prefix filter, the no-aggregation split, and the tag filterlist, then aggregation's input.
        Burst {
            name: "a burst into aggregation",
            config: "",
            aggregation: Aggregation::Gated,
            feed: "dsd_in.metrics",
            metric: "burst.requests",
            stalled: DSD_AGG,
            output: "dd_metrics_encode",
            held: 4 * TRANSFORM + STALLED,
        },
        // The same transforms, which three regions run.
        Burst {
            name: "a burst into aggregation through pipelined regions",
            config: matrix_entry("mapper profiles, a namespace prefix, and filterlists"),
            aggregation: Aggregation::Gated,
            feed: "dsd_in.metrics",
            metric: "burst.requests",
            stalled: DSD_AGG,
            output: "dd_metrics_encode",
            held: 4 * TRANSFORM + STALLED,
        },
        // The post-aggregation filter and enrichment, then the encoder's input.
        Burst {
            name: "a flush to the main output",
            config: "",
            aggregation: Aggregation::Scripted,
            feed: DSD_AGG,
            metric: "burst.requests",
            stalled: "dd_metrics_encode",
            output: "dd_metrics_encode",
            held: 2 * TRANSFORM + STALLED,
        },
        // The post-aggregation filter, enrichment, and the routing filter, then the encoder's input.
        Burst {
            name: "a flush to a routing group",
            config: matrix_entry("one routing group"),
            aggregation: Aggregation::Scripted,
            feed: DSD_AGG,
            metric: "orders.total",
            stalled: "metrics_routing_encode_0",
            output: "metrics_routing_encode_0",
            held: 3 * TRANSFORM + STALLED,
        },
        // The post-aggregation filter, enrichment, and the failover gateway, then the encoder's input.
        Burst {
            name: "a flush to multi-region failover",
            config: matrix_entry("multi-region failover mirroring an allowlist"),
            aggregation: Aggregation::Scripted,
            feed: DSD_AGG,
            metric: "orders.total",
            stalled: "mrf_metrics_encode",
            output: "mrf_metrics_encode",
            held: 3 * TRANSFORM + STALLED,
        },
    ]
}

/// Sends a burst through one path's topology: returns how many batches the feed's component sent before it had to
/// wait, and, once the stalled component reads, what the outputs received.
async fn run(burst: &Burst, path: MetricsPath) -> (usize, Vec<Delivery>) {
    let system = configuration_system(burst.config).await;
    let mut options = Options::new(path, burst.aggregation);
    if burst.stalled != DSD_AGG {
        options.gated.insert(burst.stalled.to_string());
    }
    let topology = TestTopology::start(&system, options, |_| {}).await;
    for value in 0..BATCHES {
        let metric = Metric::gauge(Context::from_static_parts(burst.metric, &[]), value as f64);
        topology.feed(burst.feed, vec![metric]);
    }
    tokio::time::sleep(SETTLE).await;
    let held = topology.sent(burst.feed);
    topology.open_gate(burst.stalled);
    (held, topology.stop().await)
}

/// Returns the values of the gauges `output` received, sorted.
fn received_values(deliveries: &[Delivery], output: &str) -> Vec<f64> {
    let mut values = deliveries
        .iter()
        .filter(|delivery| delivery.output == output)
        .flat_map(|delivery| &delivery.metrics)
        .flat_map(|metric| match metric.values() {
            MetricValues::Gauge(points) => points.into_iter().map(|(_, value)| value).collect::<Vec<_>>(),
            other => panic!("`{}` received {:?}", output, other),
        })
        .collect::<Vec<_>>();
    values.sort_by(f64::total_cmp);
    values
}

#[tokio::test(start_paused = true)]
async fn the_plan_driven_topology_takes_as_many_batches_as_the_legacy_one_before_a_burst_backs_up_to_its_source() {
    let every_batch = (0..BATCHES).map(|value| value as f64).collect::<Vec<_>>();
    for burst in bursts() {
        let (legacy, legacy_deliveries) = run(&burst, MetricsPath::Legacy).await;
        let (plan, plan_deliveries) = run(&burst, MetricsPath::Plan).await;
        assert_eq!(
            (legacy, plan),
            (burst.held, burst.held),
            "{}: batches taken (legacy, plan-driven)",
            burst.name
        );
        // Nothing the stretch held is lost or sent twice.
        for (path, deliveries) in [("legacy", &legacy_deliveries), ("plan-driven", &plan_deliveries)] {
            assert_eq!(
                received_values(deliveries, burst.output),
                every_batch,
                "{}: what `{}` received on the {} path",
                burst.name,
                burst.output,
                path
            );
        }
    }
}
