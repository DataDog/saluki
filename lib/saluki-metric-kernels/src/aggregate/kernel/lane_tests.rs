//! Tests of aggregation partitioned over lanes: one kernel per lane, each holding the contexts routed to it, sharing
//! one [`LaneShared`], against one kernel holding every context.
//!
//! The expected outputs are the one kernel's: partitioning by context changes nothing a context's own lane decides,
//! and the shared record makes the rules that couple contexts (where idle counters start, the flush count) the one
//! kernel's.

use std::{num::NonZeroU64, sync::Arc, time::Duration};

use proptest::{collection::vec, prelude::*};
use saluki_core::{
    data_model::{
        event::{
            metric::{context::Context, Metric},
            Event,
        },
        tags::TagSet,
    },
    execution::{
        AdmissionTime, BatchHeader, Budget, ClockReset, DrainPolicy, Kernel, LaneFlush, LaneShare, LaneTick, LossClass,
        PortIndex, PortOutputs, Turn, TurnCounters,
    },
    topology::EventsBuffer,
};
use saluki_metrics::{test::TestRecorder, MetricsBuilder};

use crate::aggregate::{
    lanes::{ContextCount, CountHeld, LaneShared, Restarted, TakeLaneFallbacks},
    tests::multiset,
    AggregateKernel, AggregateParams, AggregateTelemetry, ContextBudget, ContextSnapshot, HistogramConfiguration,
};

/// A realistic Unix time that is a multiple of every bucket width used here.
const B: u64 = 1_700_000_100;

fn params(counter_expiry: Option<u64>) -> AggregateParams {
    AggregateParams {
        window: NonZeroU64::new(10).unwrap(),
        context_limit: 1_000,
        counter_expiry: counter_expiry.and_then(NonZeroU64::new),
        flush_open_windows: false,
        histograms: HistogramConfiguration::default(),
    }
}

/// `B` plus `millis` milliseconds.
fn at(millis: u64) -> AdmissionTime {
    AdmissionTime::from_unix_nanos(B * 1_000_000_000 + millis * 1_000_000)
}

/// One kernel and what its host keeps for it.
struct Host {
    kernel: AggregateKernel,
    outputs: PortOutputs,
    counters: TurnCounters,
    lane: Option<u16>,
}

impl Host {
    fn new(params: AggregateParams, telemetry: AggregateTelemetry, lanes: Option<(u16, &Arc<LaneShared>)>) -> Self {
        let kernel = AggregateKernel::new(params, Duration::from_secs(15), ContextBudget::exclusive(), telemetry);
        let (kernel, lane) = match lanes {
            Some((lane, shared)) => (kernel.with_lanes(Arc::clone(shared)), Some(lane)),
            None => (kernel, None),
        };
        Self {
            kernel,
            outputs: PortOutputs::new(1),
            counters: TurnCounters::default(),
            lane,
        }
    }

    fn turn<R>(
        &mut self, header: &BatchHeader, watermark: Option<AdmissionTime>, flush: Option<(u32, LaneFlush)>,
        call: impl FnOnce(&mut AggregateKernel, &mut Turn<'_>) -> R,
    ) -> R {
        let mut budget = Budget::unlimited();
        let mut turn = Turn::new(
            header,
            std::time::Instant::now(),
            &mut budget,
            &mut self.outputs,
            &mut self.counters,
        )
        .with_watermark(watermark);
        if let (Some(lane), Some((epoch, flush))) = (self.lane, flush) {
            turn = turn.with_lane_tick(LaneTick { epoch, flush, lane });
        }
        call(&mut self.kernel, &mut turn)
    }

    fn admit(&mut self, admitted_at: AdmissionTime, metrics: Vec<Metric>) {
        let header = BatchHeader {
            admitted_at: Some(admitted_at),
            ..BatchHeader::default()
        };
        let mut buffer = EventsBuffer::with_capacity(metrics.len().max(1));
        for metric in metrics {
            assert!(buffer.try_push(Event::Metric(metric)).is_none());
        }
        self.turn(&header, None, None, |kernel, turn| kernel.consume(turn, &mut buffer));
    }

    fn time_driven(&mut self, watermark: AdmissionTime, epoch: u32, flush: LaneFlush, call: Call) -> Vec<Metric> {
        let header = BatchHeader::default();
        self.turn(
            &header,
            Some(watermark),
            Some((epoch, flush)),
            |kernel, turn| match call {
                Call::Tick => kernel.advance_time(turn),
                Call::Reset => kernel.control(turn, &mut ClockReset),
                Call::Shutdown => kernel.finish(turn, DrainPolicy::Shutdown),
            },
        );
        let emitted = self
            .outputs
            .drain(PortIndex::PRIMARY)
            .flat_map(|buffer| {
                buffer
                    .into_iter()
                    .filter_map(Event::try_into_metric)
                    .collect::<Vec<_>>()
            })
            .collect();
        self.outputs.clear();
        emitted
    }

    fn shutdown_loss(&mut self) -> u64 {
        self.counters.take().loss(LossClass::Shutdown).points
    }
}

#[derive(Clone, Copy, Debug)]
enum Call {
    Tick,
    Reset,
    Shutdown,
}

/// The lane of a context: by the high bits of its key, as the frame's key groups route.
fn lane_of(metric: &Metric, lanes: u16) -> u16 {
    ((metric.context().key().as_u64() >> 56) * u64::from(lanes) / 256) as u16
}

/// A lineage on `lanes` lanes beside one kernel holding everything, with their flush counts.
struct Pair {
    one: Host,
    lanes: Vec<Host>,
    shared: Arc<LaneShared>,
    one_recorder: TestRecorder,
    lanes_recorder: TestRecorder,
    epoch: u32,
}

impl Pair {
    fn new(lanes: u16, counter_expiry: Option<u64>) -> Self {
        let (one_recorder, lanes_recorder) = (TestRecorder::default(), TestRecorder::default());
        let shared = Arc::new(LaneShared::new(lanes));
        let one = {
            let _local = metrics::set_default_local_recorder(&one_recorder);
            Host::new(
                params(counter_expiry),
                AggregateTelemetry::new(&MetricsBuilder::default()),
                None,
            )
        };
        let lanes_hosts = {
            let _local = metrics::set_default_local_recorder(&lanes_recorder);
            (0..lanes)
                .map(|lane| {
                    shared.join(0, lane, 0);
                    Host::new(
                        params(counter_expiry),
                        AggregateTelemetry::new(&MetricsBuilder::default()),
                        Some((lane, &shared)),
                    )
                })
                .collect()
        };
        Self {
            one,
            lanes: lanes_hosts,
            shared,
            one_recorder,
            lanes_recorder,
            epoch: 0,
        }
    }

    fn admit(&mut self, admitted_at: AdmissionTime, metrics: Vec<Metric>) {
        let count = self.lanes.len() as u16;
        let mut routed = vec![Vec::new(); self.lanes.len()];
        for metric in &metrics {
            routed[usize::from(lane_of(metric, count))].push(metric.clone());
        }
        self.one.admit(admitted_at, metrics);
        for (lane, metrics) in routed.into_iter().enumerate() {
            if !metrics.is_empty() {
                self.lanes[lane].admit(admitted_at, metrics);
            }
        }
    }

    /// Runs one time-driven call at `watermark` in every lane, in `order`, and in the one kernel: returns what each
    /// emitted, as multisets.
    fn time_driven(
        &mut self, watermark: AdmissionTime, flush: LaneFlush, call: Call, order: &[usize],
    ) -> (Vec<String>, Vec<String>) {
        let one = self.one.time_driven(watermark, self.epoch, flush, call);
        let mut lanes = Vec::new();
        for &lane in order {
            lanes.extend(self.lanes[lane].time_driven(watermark, self.epoch, flush, call));
        }
        (multiset(&one), multiset(&lanes))
    }

    /// Every lane moves from its epoch to the next, as a reset window's close makes its instances continue there.
    fn next_epoch(&mut self) {
        for lane in 0..self.lanes.len() as u16 {
            self.shared.leave(self.epoch, lane);
            self.shared.join(self.epoch + 1, lane, 0);
        }
        self.epoch += 1;
    }

    fn flushes(recorder: &TestRecorder) -> u64 {
        recorder.counter("aggregate_flushes_total").unwrap_or(0)
    }

    fn assert_flush_counts_equal(&self) {
        assert_eq!(
            Self::flushes(&self.lanes_recorder),
            Self::flushes(&self.one_recorder),
            "the lanes count each flush once, as the one kernel does"
        );
    }
}

fn counter(name: &str, value: f64) -> Metric {
    Metric::counter(Context::from_parts(name.to_string(), TagSet::default()), value)
}

fn gauge(name: &str, value: f64) -> Metric {
    Metric::gauge(Context::from_parts(name.to_string(), TagSet::default()), value)
}

/// Returns a name whose context the lanes route to `lane` of `lanes`.
fn name_on_lane(prefix: &str, lane: u16, lanes: u16) -> String {
    (0..)
        .map(|index| format!("{}{}", prefix, index))
        .find(|name| lane_of(&counter(name, 0.0), lanes) == lane)
        .expect("some name routes to every lane")
}

#[test]
fn a_lane_that_was_empty_starts_its_idle_counters_at_the_latest_flush_any_lane_held_contexts_at() {
    // Fixture X1 at kernel level: lane 0 holds counter `a` and flushes at 15 and 30, lane 1 is empty at both, and
    // counter `b` arrives in lane 1 at 31: `b`'s zeros at the next tick, at 60, start at `align(30)`, as the one
    // kernel's do, so it gains zeros at 40 and 50 beside its point at 30, never none from lane 1's own never.
    let mut pair = Pair::new(2, Some(300));
    let (a, b) = (name_on_lane("a", 0, 2), name_on_lane("b", 1, 2));
    pair.admit(at(3_000), vec![counter(&a, 1.0)]);
    for (k, tick) in [(1, 15_000), (2, 30_000)] {
        let (one, lanes) = pair.time_driven(at(tick), LaneFlush::Ordinal(k), Call::Tick, &[1, 0]);
        assert_eq!(lanes, one);
    }
    pair.admit(at(31_000), vec![counter(&b, 2.0)]);
    let emitted = pair.lanes[1].time_driven(at(60_000), 0, LaneFlush::Ordinal(3), Call::Tick);
    assert_eq!(emitted.len(), 1);
    assert_eq!(
        emitted[0].values().len(),
        3,
        "b at 30, and zeros at 40 and 50: {:?}",
        multiset(&emitted)
    );
    let mut lanes = emitted;
    lanes.extend(pair.lanes[0].time_driven(at(60_000), 0, LaneFlush::Ordinal(3), Call::Tick));
    let one = pair.one.time_driven(at(60_000), 0, LaneFlush::Ordinal(3), Call::Tick);
    assert_eq!(multiset(&lanes), multiset(&one));
    pair.assert_flush_counts_equal();
}

#[test]
fn a_closing_flush_starts_its_idle_counters_where_the_lanes_last_flush_left_them() {
    // Fixture X1's closing case: lane 0 holds `a` and flushes at 30 and 45; lane 1 last held contexts at 15, and `b`
    // arrives in lane 1 at 47; a reset drains the lineage at 60. Lane 1's closing flush gives `b` a zero at 50 only, as
    // the one kernel's does, and never zeros back to its own last flush at 15.
    let mut pair = Pair::new(2, Some(300));
    let (a, b, c) = (
        name_on_lane("a", 0, 2),
        name_on_lane("b", 1, 2),
        name_on_lane("c", 1, 2),
    );
    pair.admit(at(3_000), vec![counter(&a, 1.0), gauge(&c, 1.0)]);
    let (one, lanes) = pair.time_driven(at(15_000), LaneFlush::Ordinal(1), Call::Tick, &[0, 1]);
    assert_eq!(lanes, one);
    pair.admit(at(16_000), vec![counter(&a, 1.0)]);
    for (k, tick) in [(2, 30_000), (3, 45_000)] {
        let (one, lanes) = pair.time_driven(at(tick), LaneFlush::Ordinal(k), Call::Tick, &[0, 1]);
        assert_eq!(lanes, one);
    }
    pair.admit(at(47_000), vec![counter(&b, 5.0)]);
    let (one, lanes) = pair.time_driven(at(60_000), LaneFlush::Closing, Call::Reset, &[1, 0]);
    assert_eq!(lanes, one);
    pair.assert_flush_counts_equal();
}

#[test]
fn a_rebuilt_lanes_first_flush_adds_no_zeros_and_its_second_starts_where_the_lanes_left_them() {
    // A lane's instance rebuilt after a panic starts as if it had never flushed, as a restarted aggregator does.
    let shared = Arc::new(LaneShared::new(2));
    shared.join(0, 0, 0);
    shared.join(0, 1, 0);
    let mut lane_0 = Host::new(params(Some(300)), AggregateTelemetry::noop(), Some((0, &shared)));
    let mut lane_1 = Host::new(params(Some(300)), AggregateTelemetry::noop(), Some((1, &shared)));
    lane_0.admit(at(3_000), vec![counter("a", 1.0)]);
    lane_0.time_driven(at(15_000), 0, LaneFlush::Ordinal(1), Call::Tick);
    lane_1.time_driven(at(15_000), 0, LaneFlush::Ordinal(1), Call::Tick);

    // Lane 1's instance is rebuilt, then takes `b` at 31.
    lane_1 = Host::new(params(Some(300)), AggregateTelemetry::noop(), Some((1, &shared)));
    lane_1.turn(&BatchHeader::default(), None, None, |kernel, turn| {
        kernel.control(turn, &mut Restarted)
    });
    lane_1.admit(at(31_000), vec![counter("b", 2.0)]);
    lane_0.time_driven(at(45_000), 0, LaneFlush::Ordinal(2), Call::Tick);
    let first = lane_1.time_driven(at(45_000), 0, LaneFlush::Ordinal(2), Call::Tick);
    assert_eq!(first.len(), 1);
    assert_eq!(
        first[0].values().len(),
        1,
        "only b's bucket [30, 40), without zeros back to 10 and 20 from the lanes' flush at 15: {:?}",
        multiset(&first)
    );
    lane_1.admit(at(46_000), vec![counter("b", 1.0)]);
    lane_0.time_driven(at(60_000), 0, LaneFlush::Ordinal(3), Call::Tick);
    let second = lane_1.time_driven(at(60_000), 0, LaneFlush::Ordinal(3), Call::Tick);
    assert_eq!(second.len(), 1);
    assert_eq!(
        second[0].values().len(),
        2,
        "b's bucket [40, 50) and its zero at 50, from the lanes' flush at 45: {:?}",
        multiset(&second)
    );
}

#[test]
fn a_flush_no_lane_holds_contexts_at_counts_nothing_and_one_any_lane_holds_counts_once() {
    // Fixture X3 at kernel level: four lanes, two holding contexts, count one flush per tick; four empty lanes count
    // none.
    let mut pair = Pair::new(4, None);
    pair.admit(
        at(3_000),
        vec![
            gauge(&name_on_lane("g", 0, 4), 1.0),
            gauge(&name_on_lane("h", 2, 4), 1.0),
        ],
    );
    pair.time_driven(at(15_000), LaneFlush::Ordinal(1), Call::Tick, &[3, 2, 1, 0]);
    assert_eq!(Pair::flushes(&pair.lanes_recorder), 1);
    // The gauges were flushed and left: every lane is empty at the next tick.
    pair.time_driven(at(30_000), LaneFlush::Ordinal(2), Call::Tick, &[0, 1, 2, 3]);
    assert_eq!(Pair::flushes(&pair.lanes_recorder), 1);
    pair.assert_flush_counts_equal();
}

#[test]
fn a_flush_that_cannot_start_its_zeros_where_the_lanes_left_them_is_taken_as_a_fallback() {
    // Lane 0 flushes `a` at the ticks at 15 and 30 while lane 1 has resolved neither: at 30 lane 0 cannot know whether
    // lane 1 held contexts at 15, so it starts its zeros from its own last flush, which its host takes as one tick
    // resolved apart. Lane 1 then runs ahead while lane 0 stays at 30: a flush of its holding nothing, and its first
    // flush after a rebuild, which starts from its own last flush by rule, are none; its next flush is one.
    let shared = Arc::new(LaneShared::new(2));
    shared.join(0, 0, 0);
    shared.join(0, 1, 0);
    let mut lane_0 = Host::new(params(Some(300)), AggregateTelemetry::noop(), Some((0, &shared)));
    let mut lane_1 = Host::new(params(Some(300)), AggregateTelemetry::noop(), Some((1, &shared)));
    let take = |host: &mut Host| {
        let mut take = TakeLaneFallbacks::default();
        host.turn(&BatchHeader::default(), None, None, |kernel, turn| {
            kernel.control(turn, &mut take)
        });
        take.flushes
    };
    lane_0.admit(at(3_000), vec![counter("a", 1.0)]);
    lane_0.time_driven(at(15_000), 0, LaneFlush::Ordinal(1), Call::Tick);
    assert_eq!(take(&mut lane_0), 0, "ordinal 1 has no ordinal before it");
    lane_0.admit(at(16_000), vec![counter("a", 1.0)]);
    lane_0.time_driven(at(30_000), 0, LaneFlush::Ordinal(2), Call::Tick);
    assert_eq!(take(&mut lane_0), 1);
    assert_eq!(take(&mut lane_0), 0, "taken once");

    lane_1.time_driven(at(60_000), 0, LaneFlush::Ordinal(4), Call::Tick);
    assert_eq!(take(&mut lane_1), 0, "lane 1 held nothing to flush");
    lane_1.turn(&BatchHeader::default(), None, None, |kernel, turn| {
        kernel.control(turn, &mut Restarted)
    });
    lane_1.admit(at(61_000), vec![counter("b", 1.0)]);
    lane_1.time_driven(at(75_000), 0, LaneFlush::Ordinal(5), Call::Tick);
    assert_eq!(take(&mut lane_1), 0, "a rebuilt instance's first flush");
    lane_1.admit(at(76_000), vec![counter("b", 1.0)]);
    lane_1.time_driven(at(90_000), 0, LaneFlush::Ordinal(6), Call::Tick);
    assert_eq!(take(&mut lane_1), 1);
}

#[test]
fn a_host_asks_an_instance_how_many_contexts_it_holds_and_how_many_of_anothers_it_holds_too() {
    let mut retiring = Host::new(params(None), AggregateTelemetry::noop(), None);
    let mut new = Host::new(params(None), AggregateTelemetry::noop(), None);
    let header = BatchHeader::default();
    let count = |host: &mut Host| {
        let mut count = ContextCount::default();
        host.turn(&header, None, None, |kernel, turn| kernel.control(turn, &mut count));
        count.contexts
    };
    assert_eq!(count(&mut new), Some(0));
    retiring.admit(at(3_000), vec![counter("a", 1.0), counter("b", 1.0), gauge("c", 1.0)]);
    new.admit(at(4_000), vec![counter("b", 1.0), gauge("c", 2.0), gauge("d", 2.0)]);
    assert_eq!(count(&mut retiring), Some(3));
    let mut snapshot = ContextSnapshot::default();
    new.turn(&header, None, None, |kernel, turn| kernel.control(turn, &mut snapshot));
    let mut held = CountHeld {
        entries: snapshot.entries,
        held: 0,
    };
    retiring.turn(&header, None, None, |kernel, turn| kernel.control(turn, &mut held));
    assert_eq!(held.held, 2, "b and c are held by both");
}

/// One step of a generated run.
#[derive(Clone, Debug)]
enum LaneStep {
    /// Metrics admitted `after` milliseconds after the previous step: `(context, value)`, contexts below 4 counters
    /// and the rest gauges.
    Admit { after: u64, metrics: Vec<(u8, u8)> },

    /// A flush tick `after` milliseconds after the previous step, at the time it runs, with the order the lanes run it
    /// in.
    Tick { after: u64, order: Vec<usize> },

    /// A clock reset `after` milliseconds after the previous step, which drains everything; the next epoch's
    /// timeline continues from it.
    Reset { after: u64, order: Vec<usize> },
}

fn arb_lane_steps(lanes: usize) -> impl Strategy<Value = Vec<LaneStep>> {
    let order = Just((0..lanes).collect::<Vec<_>>()).prop_shuffle();
    let step = prop_oneof![
        4 => (0u64..12_000, vec((0u8..8, 0u8..4), 0..6))
            .prop_map(|(after, metrics)| LaneStep::Admit { after, metrics }),
        3 => (1_000u64..40_000, order.clone()).prop_map(|(after, order)| LaneStep::Tick { after, order }),
        1 => (0u64..20_000, order).prop_map(|(after, order)| LaneStep::Reset { after, order }),
    ];
    vec(step, 1..30)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Lanes that hold disjoint shares of a lineage's contexts and resolve every tick together, in any order of lanes,
    /// emit what one kernel holding every context emits, tick by tick: idle-counter zeros start at the latest flush
    /// any lane held contexts at, counters expire at the same flush, closing flushes (resets and shutdown) start where
    /// the lanes' last flush left them, and each flush is counted once.
    #[test]
    fn property_test_lane_flush_log_matches_one_core(
        (lanes, steps) in (1usize..5).prop_flat_map(|lanes| (Just(lanes), arb_lane_steps(lanes))),
        expiry in proptest::option::of(proptest::sample::select(&[10u64, 20, 300][..])),
    ) {
        let mut pair = Pair::new(lanes as u16, expiry);
        let mut now = 0u64;
        let mut ordinal = 0u64;
        for step in steps {
            match step {
                LaneStep::Admit { after, metrics } => {
                    now += after;
                    let metrics = metrics
                        .into_iter()
                        .map(|(context, value)| {
                            let name = format!("m{}", context);
                            if context < 4 { counter(&name, f64::from(value)) } else { gauge(&name, f64::from(value)) }
                        })
                        .collect();
                    pair.admit(at(now), metrics);
                }
                LaneStep::Tick { after, order } => {
                    now += after;
                    ordinal += 1;
                    let (one, lanes) = pair.time_driven(at(now), LaneFlush::Ordinal(ordinal), Call::Tick, &order);
                    prop_assert_eq!(lanes, one, "tick {} at {} ms", ordinal, now);
                }
                LaneStep::Reset { after, order } => {
                    now += after;
                    let (one, lanes) = pair.time_driven(at(now), LaneFlush::Closing, Call::Reset, &order);
                    prop_assert_eq!(lanes, one, "reset at {} ms", now);
                    pair.next_epoch();
                    ordinal = 0;
                }
            }
        }
        let order = (0..lanes).collect::<Vec<_>>();
        now += 1_000;
        let (one, lanes_emitted) = pair.time_driven(at(now), LaneFlush::Closing, Call::Shutdown, &order);
        prop_assert_eq!(lanes_emitted, one, "shutdown at {} ms", now);
        let lost = pair.lanes.iter_mut().map(Host::shutdown_loss).sum::<u64>();
        prop_assert_eq!(lost, pair.one.shutdown_loss());
        prop_assert_eq!(Pair::flushes(&pair.lanes_recorder), Pair::flushes(&pair.one_recorder));
    }
}
