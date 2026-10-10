//! Tests of aggregation as a kernel: bucketing by admission time, flushing at the watermark, per-call parameters,
//! resets, finishing, and bounded, resumable passes.

use std::{
    num::NonZeroU64,
    panic::AssertUnwindSafe,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use proptest::{collection::vec, prelude::*};
use saluki_core::{
    data_model::event::{
        metric::{context::Context, Metric, MetricMetadata, ScalarPoints},
        service_check::{CheckStatus, ServiceCheck},
        Event,
    },
    execution::{
        AdmissionTime, BatchHeader, Budget, ClockReset, DrainPolicy, Kernel, LossClass, LossCount, PortIndex,
        PortOutputs, Progress, StateBudget, Turn, TurnCounters, WorkKind,
    },
    topology::EventsBuffer,
};
use saluki_metrics::{test::TestRecorder, MetricsBuilder};

use crate::aggregate::{
    tests::{
        aggregate_params, arb_params, arb_steps, emission::TestFilter, multiset, reference_run_loop, reference_tick,
        snapshot_set, telemetry_values, Step,
    },
    AdmitParams, AggregateKernel, AggregateParams, AggregateStepParams, AggregateTelemetry, ContextBudget,
    ContextSnapshot, EmissionFilter, HistogramConfiguration, OutputKind, SiteIndex, Suppressed, SuppressedBySite,
    TakeSuppressed,
};

/// A realistic Unix time that is a multiple of every bucket width used here.
const B: u64 = 1_700_000_100;

const WINDOW: Duration = Duration::from_secs(10);

fn params(context_limit: usize, counter_expiry: Option<u64>) -> AggregateParams {
    AggregateParams {
        window: NonZeroU64::new(WINDOW.as_secs()).unwrap(),
        context_limit,
        counter_expiry: counter_expiry.and_then(NonZeroU64::new),
        flush_open_windows: false,
        histograms: HistogramConfiguration::default(),
    }
}

/// `B + secs` seconds and `millis` milliseconds, as an admission time.
fn at(secs: u64, millis: u64) -> AdmissionTime {
    AdmissionTime::from_unix_nanos((B + secs) * 1_000_000_000 + millis * 1_000_000)
}

fn rate(name: &'static str, points: &[(u64, f64)]) -> Metric {
    let points = points
        .iter()
        .map(|(secs, value)| (B + secs, *value))
        .collect::<Vec<_>>();
    Metric::rate(name, ScalarPoints::from(&points[..]), WINDOW)
}

fn gauge(name: &'static str, points: &[(u64, f64)]) -> Metric {
    let points = points
        .iter()
        .map(|(secs, value)| (B + secs, *value))
        .collect::<Vec<_>>();
    Metric::gauge(name, ScalarPoints::from(&points[..]))
}

/// What a host keeps for one kernel, as a region driver does: its output port and its counters, and the per-call
/// parameters it passes when a call names none.
struct Host {
    kernel: AggregateKernel,
    outputs: PortOutputs,
    counters: TurnCounters,
    header: BatchHeader,
    now: std::time::Instant,
    step: Option<AggregateStepParams>,
}

impl Host {
    fn new(params: AggregateParams) -> Self {
        Self::with(params, ContextBudget::exclusive(), AggregateTelemetry::noop())
    }

    fn with(params: AggregateParams, budget: ContextBudget, telemetry: AggregateTelemetry) -> Self {
        Self {
            kernel: AggregateKernel::new(params, Duration::from_secs(15), budget, telemetry),
            outputs: PortOutputs::new(1),
            counters: TurnCounters::default(),
            header: BatchHeader::default(),
            now: std::time::Instant::now(),
            step: None,
        }
    }

    /// Passes `step` with every call that names no parameters of its own.
    fn with_step(mut self, step: AggregateStepParams) -> Self {
        self.step = Some(step);
        self
    }

    /// Runs `call` in one turn with the given watermark, budget, and per-call parameters.
    fn call<R>(
        &mut self, watermark: Option<AdmissionTime>, budget: Budget, param: Option<&AggregateStepParams>,
        call: impl FnOnce(&mut AggregateKernel, &mut Turn<'_>) -> R,
    ) -> R {
        let param = param.or(self.step.as_ref());
        let mut budget = budget;
        let mut turn = Turn::new(
            &self.header,
            self.now,
            &mut budget,
            &mut self.outputs,
            &mut self.counters,
        )
        .with_watermark(watermark);
        if let Some(param) = param {
            turn = turn.with_param(param);
        }
        call(&mut self.kernel, &mut turn)
    }

    /// Delivers a batch admitted at `admitted_at`, with the turn's parameters, as a host does.
    fn consume(&mut self, admitted_at: Option<AdmissionTime>, events: Vec<Event>, param: Option<&AggregateStepParams>) {
        self.header = BatchHeader {
            admitted_at,
            ..BatchHeader::default()
        };
        let mut input = EventsBuffer::with_capacity(events.len());
        for event in events {
            assert!(input.try_push(event).is_none());
        }
        let progress = self.call(None, Budget::unlimited(), param, |kernel, turn| {
            kernel.consume(turn, &mut input)
        });
        assert_eq!(progress, Progress::Done, "consuming never yields");
        self.outputs.complete(input);
        self.header = BatchHeader::default();
    }

    fn admit(&mut self, admitted_at: AdmissionTime, metrics: Vec<Metric>) {
        self.consume(
            Some(admitted_at),
            metrics.into_iter().map(Event::Metric).collect(),
            None,
        );
    }

    /// Removes and returns what the kernel emitted so far.
    fn emitted(&mut self) -> Vec<Metric> {
        self.outputs
            .drain(PortIndex::PRIMARY)
            .flat_map(|buffer| buffer.into_iter())
            .map(|event| event.try_into_metric().expect("aggregation emits metrics"))
            .collect()
    }

    /// Runs one call of the kernel with an unlimited budget, which completes it.
    fn whole(
        &mut self, watermark: Option<AdmissionTime>, call: impl FnOnce(&mut AggregateKernel, &mut Turn<'_>) -> Progress,
    ) {
        let progress = self.call(watermark, Budget::unlimited(), None, call);
        assert_eq!(progress, Progress::Done, "an unlimited call completes");
    }

    fn tick(&mut self, watermark: Option<AdmissionTime>) -> Vec<Metric> {
        self.whole(watermark, |kernel, turn| kernel.advance_time(turn));
        self.emitted()
    }

    fn snapshot(&mut self) -> ContextSnapshot {
        let mut snapshot = ContextSnapshot::default();
        let progress = self.call(None, Budget::unlimited(), None, |kernel, turn| {
            kernel.control(turn, &mut snapshot)
        });
        assert_eq!(progress, Progress::Done, "a snapshot never yields");
        snapshot
    }
}

#[test]
fn batches_are_bucketed_by_admission_time_and_flushed_at_the_watermark() {
    let mut host = Host::new(params(10, None));
    host.admit(at(3, 900), vec![Metric::counter("c", 1.0)]);
    host.admit(at(9, 999), vec![Metric::counter("c", 2.0)]);
    host.admit(at(10, 0), vec![Metric::gauge("g", 5.0)]);
    assert!(host.emitted().is_empty(), "admitting writes nothing");

    // Without a watermark nothing closes; a watermark short of a bucket's end closes nothing either.
    assert!(host.tick(None).is_empty());
    assert!(host.tick(Some(at(9, 999))).is_empty());

    // The watermark reaches the end of [B, B+10), but not of [B+10, B+20).
    assert_eq!(
        multiset(&host.tick(Some(at(10, 500)))),
        multiset(&[rate("c", &[(0, 3.0)])])
    );
    assert_eq!(
        multiset(&host.tick(Some(at(20, 0)))),
        multiset(&[gauge("g", &[(10, 5.0)])])
    );
    assert_eq!(host.counters, TurnCounters::default(), "nothing was lost");
}

#[test]
fn batches_without_an_admission_time_and_events_other_than_metrics_are_invalid() {
    let mut host = Host::new(params(10, None));
    let two_points = Metric::gauge("b", ScalarPoints::from([1.0, 2.0]));
    host.consume(
        None,
        vec![Event::Metric(Metric::gauge("a", 1.0)), Event::Metric(two_points)],
        None,
    );
    assert_eq!(
        host.counters.loss(LossClass::Invalid),
        LossCount { events: 2, points: 3 }
    );
    assert!(host.kernel.core().is_empty());

    let check = Event::ServiceCheck(ServiceCheck::new("check", CheckStatus::Ok));
    host.consume(
        Some(at(1, 0)),
        vec![Event::Metric(Metric::gauge("a", 1.0)), check],
        None,
    );
    assert_eq!(
        host.counters.loss(LossClass::Invalid),
        LossCount { events: 3, points: 3 }
    );
    assert_eq!(host.kernel.core().len(), 1);
    assert_eq!(host.counters.loss(LossClass::Capacity), LossCount::default());
}

#[test]
fn the_turns_parameters_replace_the_kernels_own_for_each_call() {
    let mut host = Host::new(params(1, None));
    let count_only = HistogramConfiguration::try_new(&["count".to_string()], &[], false, String::new()).unwrap();
    let mut step = host.kernel.defaults().clone();
    step.admit = AdmitParams { context_limit: 3 };
    step.flush.histograms = count_only;

    // The kernel's own limit admits one context; the turn's admits up to three.
    host.admit(at(1, 0), vec![Metric::gauge("a", 1.0), Metric::gauge("b", 1.0)]);
    assert_eq!(
        host.counters.loss(LossClass::Capacity),
        LossCount { events: 1, points: 1 }
    );
    host.consume(
        Some(at(2, 0)),
        vec![
            Event::Metric(Metric::gauge("c", 1.0)),
            Event::Metric(Metric::histogram("h", [1.0, 2.0])),
        ],
        Some(&step),
    );
    assert_eq!(host.kernel.core().len(), 3);
    assert_eq!(
        host.counters.loss(LossClass::Capacity),
        LossCount { events: 1, points: 1 }
    );

    // The flush expands histograms with the turn's statistics.
    host.call(Some(at(10, 0)), Budget::unlimited(), Some(&step), |kernel, turn| {
        assert_eq!(kernel.advance_time(turn), Progress::Done);
    });
    let names = {
        let mut names = host
            .emitted()
            .iter()
            .map(|metric| metric.context().name().to_string())
            .collect::<Vec<_>>();
        names.sort();
        names
    };
    assert_eq!(names, vec!["a", "c", "h.count"]);
}

#[test]
fn a_flush_yields_between_contexts_and_never_splits_the_records_of_one() {
    let metrics = || {
        vec![
            Metric::histogram("h1", [1.0, 2.0]),
            Metric::histogram("h2", [3.0]),
            Metric::gauge("g1", 1.0),
            Metric::gauge("g2", 2.0),
        ]
    };
    let mut whole = Host::new(params(10, None));
    whole.admit(at(1, 0), metrics());
    let expected = whole.tick(Some(at(10, 0)));
    assert_eq!(
        expected.len(),
        2 * 5 + 2,
        "two histograms of five statistics, and two gauges"
    );

    // One event of budget a call: each call still emits one context's records whole, then yields.
    let mut yielding = Host::new(params(10, None));
    yielding.admit(at(1, 0), metrics());
    let mut per_call = Vec::new();
    let mut emitted = Vec::new();
    loop {
        let budget = Budget::unlimited().with_limit(WorkKind::Events, 1);
        let progress = yielding.call(Some(at(10, 0)), budget, None, |kernel, turn| kernel.advance_time(turn));
        let records = yielding.emitted();
        per_call.push(records.len());
        emitted.extend(records);
        if progress == Progress::Done {
            break;
        }
        assert_eq!(progress, Progress::Yielded);
        assert!(yielding.kernel.has_pending_work());
    }
    per_call.sort();
    assert_eq!(
        per_call,
        vec![0, 1, 1, 5, 5],
        "one context a call, and a last call that finds none left"
    );
    assert!(!yielding.kernel.has_pending_work());
    assert_eq!(multiset(&emitted), multiset(&expected));
    assert!(yielding.kernel.core().is_empty());

    // The budget is charged one unit of state work per context and one event per record.
    let mut charged = Host::new(params(10, None));
    charged.admit(at(1, 0), metrics());
    let mut budget = Budget::unlimited()
        .with_limit(WorkKind::StateWork, 2)
        .with_limit(WorkKind::Events, 100);
    let progress = {
        let mut turn = Turn::new(
            &charged.header,
            charged.now,
            &mut budget,
            &mut charged.outputs,
            &mut charged.counters,
        )
        .with_watermark(Some(at(10, 0)));
        charged.kernel.advance_time(&mut turn)
    };
    assert_eq!(progress, Progress::Yielded);
    let records = charged.emitted().len() as u64;
    assert_eq!(budget.remaining(WorkKind::StateWork), 0);
    assert_eq!(budget.remaining(WorkKind::Events), 100 - records);
}

#[test]
fn a_clock_reset_closes_everything_and_the_kernel_continues_as_if_it_never_flushed() {
    // Two kernels see the same batches; one is reset between two flushes, the other only flushes.
    let mut reset = Host::new(params(10, Some(300)));
    let mut ticked = Host::new(params(10, Some(300)));
    for host in [&mut reset, &mut ticked] {
        host.admit(at(1, 0), vec![Metric::counter("c", 1.0)]);
        assert_eq!(
            multiset(&host.tick(Some(at(10, 0)))),
            multiset(&[rate("c", &[(0, 1.0)])])
        );
        host.admit(at(12, 0), vec![Metric::counter("c", 2.0)]);
        host.admit(at(25, 0), vec![Metric::gauge("g", 3.0)]);
    }

    // The reset flushes at the watermark, closing [B+10, B+20), then emits what is still open, [B+20, B+30) of `g`.
    reset.whole(Some(at(21, 0)), |kernel, turn| kernel.control(turn, &mut ClockReset));
    assert_eq!(
        multiset(&reset.emitted()),
        multiset(&[rate("c", &[(10, 2.0)]), gauge("g", &[(20, 3.0)])])
    );
    assert!(reset.kernel.core().is_empty(), "the idle counter is removed too");
    assert_eq!(reset.kernel.held(), LossCount::default());

    // After the reset the kernel has never flushed: a counter seen next gains no zeros back to the last flush, while
    // without the reset it gains one for every bucket since then.
    assert_eq!(
        multiset(&ticked.tick(Some(at(21, 0)))),
        multiset(&[rate("c", &[(10, 2.0)])])
    );
    for host in [&mut reset, &mut ticked] {
        host.admit(at(100, 0), vec![Metric::counter("d", 5.0)]);
    }
    assert_eq!(
        multiset(&reset.tick(Some(at(110, 0)))),
        multiset(&[rate("d", &[(100, 5.0)])])
    );
    let zeros = (20..100)
        .step_by(10)
        .map(|secs| (secs, 0.0))
        .chain([(100, 5.0)])
        .collect::<Vec<_>>();
    let ticked_emitted = ticked.tick(Some(at(110, 0)));
    assert!(
        multiset(&ticked_emitted).contains(&multiset(&[rate("d", &zeros)])[0]),
        "without a reset, the counter gains zeros back to the last flush: {:?}",
        ticked_emitted
    );
}

#[test]
fn a_yielded_reset_resumes_with_another_clock_reset_and_a_snapshot_leaves_it_in_place() {
    let metrics = || {
        vec![
            Metric::gauge("a", 1.0),
            Metric::gauge("b", 1.0),
            Metric::gauge("c", 1.0),
        ]
    };
    let mut whole = Host::new(params(10, None));
    whole.admit(at(1, 0), metrics());
    whole.admit(at(22, 0), vec![Metric::gauge("d", 2.0), Metric::gauge("e", 2.0)]);
    whole.whole(Some(at(15, 0)), |kernel, turn| kernel.control(turn, &mut ClockReset));
    let expected = whole.emitted();
    assert_eq!(expected.len(), 5);

    let mut yielding = Host::new(params(10, None));
    yielding.admit(at(1, 0), metrics());
    yielding.admit(at(22, 0), vec![Metric::gauge("d", 2.0), Metric::gauge("e", 2.0)]);
    let one_context = || Budget::unlimited().with_limit(WorkKind::StateWork, 1);
    let progress = yielding.call(Some(at(15, 0)), one_context(), None, |kernel, turn| {
        kernel.control(turn, &mut ClockReset)
    });
    assert_eq!(progress, Progress::Yielded);

    // A snapshot while the reset is pending lists every context, and changes nothing.
    assert_eq!(yielding.snapshot().entries.len(), 5);
    assert!(yielding.kernel.has_pending_work());

    let mut calls = 1;
    loop {
        // The resumed reset keeps the watermark it started with, whatever later turns carry.
        let progress = yielding.call(None, one_context(), None, |kernel, turn| {
            kernel.control(turn, &mut ClockReset)
        });
        calls += 1;
        if progress == Progress::Done {
            break;
        }
        assert_eq!(progress, Progress::Yielded);
    }
    // One call per context visited: the flush visits all five, emitting the three whose bucket closed, and the drain
    // the two it left; and one last call that finds the drain finished. The flush's end needs no call of its own: the
    // drain starts in the same call.
    assert_eq!(calls, 5 + 2 + 1);
    assert_eq!(multiset(&yielding.emitted()), multiset(&expected));
    assert!(yielding.kernel.core().is_empty());
    assert!(yielding.snapshot().entries.is_empty());
}

#[test]
fn retiring_emits_everything_and_shutting_down_discards_the_open_buckets_as_loss() {
    let batches = |host: &mut Host| {
        host.admit(at(1, 0), vec![Metric::counter("c", 1.0)]);
        host.admit(at(12, 0), vec![Metric::gauge("g", 2.0), Metric::histogram("h", [4.0])]);
    };
    let mut retiring = Host::new(params(10, None));
    let mut stopping = Host::new(params(10, None));
    batches(&mut retiring);
    batches(&mut stopping);
    assert_eq!(retiring.kernel.held(), LossCount { events: 3, points: 3 });

    // Retiring flushes [B, B+10) at the watermark, then drains [B+10, B+20).
    retiring.whole(Some(at(15, 0)), |kernel, turn| kernel.finish(turn, DrainPolicy::Retire));
    let mut names = retiring
        .emitted()
        .iter()
        .map(|metric| format!("{} {:?}", metric.context().name(), metric.values().as_str()))
        .collect::<Vec<_>>();
    names.sort();
    assert_eq!(
        names,
        vec![
            "c \"rate\"",
            "g \"gauge\"",
            "h.95percentile \"gauge\"",
            "h.avg \"gauge\"",
            "h.count \"rate\"",
            "h.max \"gauge\"",
            "h.median \"gauge\"",
        ]
    );
    assert_eq!(retiring.kernel.held(), LossCount::default());
    assert_eq!(retiring.counters, TurnCounters::default(), "retiring loses nothing");

    // Shutting down runs the final flush, then discards the open bucket's contexts as shutdown loss.
    stopping.whole(Some(at(15, 0)), |kernel, turn| {
        kernel.finish(turn, DrainPolicy::Shutdown)
    });
    assert_eq!(multiset(&stopping.emitted()), multiset(&[rate("c", &[(0, 1.0)])]));
    assert_eq!(
        stopping.counters.loss(LossClass::Shutdown),
        LossCount { events: 2, points: 2 }
    );
    assert_eq!(stopping.kernel.held(), LossCount::default());
    assert!(stopping.kernel.core().is_empty());
}

#[test]
fn a_kind_conflict_is_policy_loss_of_the_points_it_replaces() {
    let mut host = Host::new(params(10, None));
    host.admit(at(1, 0), vec![gauge("x", &[(1, 1.0), (2, 2.0)])]);
    host.admit(at(3, 0), vec![Metric::counter("x", 1.0)]);
    assert_eq!(
        host.counters.loss(LossClass::Policy),
        LossCount { events: 1, points: 2 }
    );

    // Values of the held kind, and a context that held nothing, replace nothing.
    host.admit(at(4, 0), vec![Metric::counter("x", 1.0), Metric::gauge("y", 1.0)]);
    assert_eq!(
        host.counters.loss(LossClass::Policy),
        LossCount { events: 1, points: 2 }
    );
    assert_eq!(
        multiset(&host.tick(Some(at(10, 0)))),
        multiset(&[rate("x", &[(0, 2.0)]), gauge("y", &[(0, 1.0)])])
    );
}

/// Takes the kernel's suppression counts with a [`TakeSuppressed`] control message, as a host does.
fn take(host: &mut Host) -> SuppressedBySite {
    let mut take = TakeSuppressed::default();
    let progress = host.call(None, Budget::unlimited(), None, |kernel, turn| {
        kernel.control(turn, &mut take)
    });
    assert_eq!(progress, Progress::Done, "taking the counts never yields");
    take.counts
}

#[test]
fn suppressed_records_are_counted_as_each_call_returns_and_taken_by_site() {
    let mut host = Host::new(params(10, None));
    let mut step = host.kernel.defaults().clone();
    step.flush.filter = Some(Arc::new(TestFilter::names(&["a", "b", "d"])));
    host.admit(
        at(1, 0),
        vec![
            Metric::gauge("a", 1.0),
            Metric::gauge("b", 1.0),
            Metric::gauge("c", 1.0),
        ],
    );
    host.admit(at(12, 0), vec![Metric::gauge("d", 1.0), Metric::gauge("e", 1.0)]);
    let site = SiteIndex::new(7);

    // A tick in one context a call: every call that skips a record counts it as it returns, yielded or not, with the
    // records it emitted, and taking the counts between calls leaves the flush in place. All five contexts are visited
    // by yielded calls, and a last call finds none left.
    let one_context = || Budget::unlimited().with_limit(WorkKind::StateWork, 1);
    let mut emitted = Vec::new();
    let mut taken_while_pending = Suppressed::default();
    loop {
        let progress = host.call(Some(at(15, 0)), one_context(), Some(&step), |kernel, turn| {
            kernel.advance_time(turn)
        });
        emitted.extend(host.emitted());
        let counts = take(&mut host);
        if progress == Progress::Done {
            assert!(counts.is_empty(), "the last call visits nothing");
            break;
        }
        assert!(host.kernel.has_pending_work());
        for (counted, suppressed) in counts {
            assert_eq!(counted, site);
            assert_eq!(suppressed, Suppressed { records: 1, points: 1 }, "one context a call");
            taken_while_pending.add(suppressed);
        }
    }
    assert_eq!(multiset(&emitted), multiset(&[gauge("c", &[(0, 1.0)])]));
    assert_eq!(taken_while_pending, Suppressed { records: 2, points: 2 });
    assert!(take(&mut host).is_empty(), "taking the counts resets them");

    // A reset's flush and drain both skip what the filter drops, and their counts add up under one site: the flush
    // skips `a` and `d` in [B+10, B+20), and the drain `a` in [B+20, B+30).
    host.admit(at(16, 0), vec![Metric::gauge("a", 1.0)]);
    host.admit(at(22, 0), vec![Metric::gauge("a", 1.0)]);
    host.call(Some(at(21, 0)), Budget::unlimited(), Some(&step), |kernel, turn| {
        assert_eq!(kernel.control(turn, &mut ClockReset), Progress::Done);
    });
    assert_eq!(multiset(&host.emitted()), multiset(&[gauge("e", &[(10, 1.0)])]));
    assert_eq!(
        host.kernel.take_suppressed().as_slice(),
        &[(site, Suppressed { records: 3, points: 3 })]
    );
    assert!(host.kernel.core().is_empty());
}

/// An emission filter that drops every record, and panics the `panic_at`-th time it is consulted.
struct PanickingFilter {
    consulted: AtomicUsize,
    panic_at: usize,
}

impl EmissionFilter for PanickingFilter {
    fn drops(&self, _: &Context, _: OutputKind, _: &MetricMetadata) -> bool {
        let consulted = self.consulted.fetch_add(1, Ordering::Relaxed) + 1;
        assert!(
            consulted < self.panic_at,
            "the filter panics when consulted {} times",
            consulted
        );
        true
    }

    fn site(&self) -> SiteIndex {
        SiteIndex::new(1)
    }
}

#[test]
fn a_call_that_panics_counts_nothing_it_skipped_and_the_calls_before_it_keep_their_counts() {
    // A host discards what a call that panics wrote, so the filter never receives the records that call emitted, and
    // the records it skipped are not counted either. What earlier calls skipped stays counted, for the host to take
    // before it drops the kernel.
    let mut host = Host::new(params(10, None));
    let mut step = host.kernel.defaults().clone();
    step.flush.filter = Some(Arc::new(PanickingFilter {
        consulted: AtomicUsize::new(0),
        panic_at: 4,
    }));

    // A tick's first call visits `p`, skips it, and yields; the call that resumes it finds nothing left.
    host.admit(at(1, 0), vec![Metric::gauge("p", 1.0)]);
    let one_context = Budget::unlimited().with_limit(WorkKind::StateWork, 1);
    let progress = host.call(Some(at(10, 0)), one_context, Some(&step), |kernel, turn| {
        kernel.advance_time(turn)
    });
    assert_eq!(progress, Progress::Yielded);
    host.call(Some(at(10, 0)), Budget::unlimited(), Some(&step), |kernel, turn| {
        assert_eq!(kernel.advance_time(turn), Progress::Done);
    });

    // A reset's flush skips `a` and `b` and completes, and its drain panics at `d`, all in one call.
    host.admit(at(11, 0), vec![Metric::gauge("a", 1.0), Metric::gauge("b", 1.0)]);
    host.admit(at(22, 0), vec![Metric::gauge("d", 1.0)]);
    let panicked = std::panic::catch_unwind(AssertUnwindSafe(|| {
        host.call(Some(at(25, 0)), Budget::unlimited(), Some(&step), |kernel, turn| {
            kernel.control(turn, &mut ClockReset)
        })
    }));
    assert!(panicked.is_err());
    assert_eq!(
        host.kernel.take_suppressed().as_slice(),
        &[(SiteIndex::new(1), Suppressed { records: 1, points: 1 })],
        "only the tick's skip of `p` counts"
    );
}

#[test]
fn a_clock_reset_leaves_the_kernel_as_if_it_never_flushed_even_with_nothing_left_to_drain() {
    // A reset that finds nothing to drain still ends the timeline: the instance held nothing when the reset came, or
    // the reset's own flush emptied it. Either way, the next timeline's first flush adds no idle-counter zeros back to
    // a flush of the old one, however late it comes.
    type Before = fn(&mut Host);
    let cases: [(&str, Before); 2] = [
        ("an empty kernel", |host| {
            host.admit(at(1, 0), vec![Metric::gauge("g", 1.0)]);
            assert_eq!(host.tick(Some(at(10, 0))).len(), 1);
            assert!(host.kernel.core().is_empty());
        }),
        ("a kernel the reset's flush empties", |host| {
            host.admit(at(1, 0), vec![Metric::gauge("g", 1.0)]);
        }),
    ];
    for (case, before) in cases {
        let mut host = Host::new(params(10, Some(300)));
        before(&mut host);
        host.whole(Some(at(15, 0)), |kernel, turn| kernel.control(turn, &mut ClockReset));
        host.emitted();
        assert!(host.kernel.core().is_empty(), "{}", case);

        host.admit(at(605, 0), vec![Metric::counter("c", 2.0)]);
        assert_eq!(
            multiset(&host.tick(Some(at(620, 0)))),
            multiset(&[rate("c", &[(600, 2.0)])]),
            "{}",
            case
        );
    }
}

#[test]
fn a_yielded_shutdown_records_what_each_call_discards() {
    // Each call of a shutdown records what it discarded, so what the kernel still holds and what it recorded as lost
    // add up to what it held when the discard began, between any two calls.
    let mut host = Host::new(params(10, None));
    host.admit(
        at(12, 0),
        vec![
            Metric::gauge("a", 1.0),
            gauge("b", &[(12, 1.0), (13, 1.0)]),
            Metric::gauge("c", 1.0),
        ],
    );
    let held = host.kernel.held();
    assert_eq!(held, LossCount { events: 3, points: 4 });

    let mut calls = 0;
    loop {
        let one_context = Budget::unlimited().with_limit(WorkKind::StateWork, 1);
        let progress = host.call(Some(at(15, 0)), one_context, None, |kernel, turn| {
            kernel.finish(turn, DrainPolicy::Shutdown)
        });
        calls += 1;
        let (now_held, lost) = (host.kernel.held(), host.counters.loss(LossClass::Shutdown));
        assert_eq!(
            (now_held.events + lost.events, now_held.points + lost.points),
            (held.events, held.points),
            "after call {}",
            calls
        );
        if progress == Progress::Done {
            break;
        }
    }
    assert!(host.emitted().is_empty(), "nothing closed by the final flush");
    // The final flush visits the three contexts, and the discard the three again, removing one each call.
    assert_eq!(calls, 3 + 3 + 1);
    assert_eq!(host.counters.loss(LossClass::Shutdown), held);
    assert!(host.kernel.core().is_empty());
}

#[test]
fn a_kernel_built_with_a_shared_budget_gives_back_its_share_when_dropped() {
    let global = StateBudget::new(3);
    let budget = || ContextBudget {
        global: global.clone(),
        node: StateBudget::unlimited(),
    };
    let mut first = Host::with(params(10, None), budget(), AggregateTelemetry::noop());
    let mut second = Host::with(params(10, None), budget(), AggregateTelemetry::noop());
    first.admit(at(1, 0), vec![Metric::gauge("a", 1.0), Metric::gauge("b", 1.0)]);
    second.admit(at(1, 0), vec![Metric::gauge("c", 1.0), Metric::gauge("d", 1.0)]);
    assert_eq!(
        second.counters.loss(LossClass::Capacity),
        LossCount { events: 1, points: 1 }
    );
    assert_eq!(global.used(), 3);

    // A host that discards the first kernel, as it does to rebuild one after a panic, frees its units for the second.
    drop(first);
    assert_eq!(global.used(), 1);
    second.admit(at(2, 0), vec![Metric::gauge("d", 1.0), Metric::gauge("e", 1.0)]);
    assert_eq!(second.kernel.core().len(), 3);
    assert_eq!(global.used(), 3);
}

/// One step of a generated kernel run, beyond the core's steps: a clock reset, at the time of the step before it, after
/// which the clock continues `after` seconds away, earlier or later.
#[derive(Clone, Debug)]
enum KernelStep {
    Core(Step),
    Reset { after: i64 },
}

fn arb_kernel_steps() -> impl Strategy<Value = Vec<KernelStep>> {
    // A reset follows a backward jump beyond the reset threshold, but the kernel must not depend on that: the new
    // timeline may also continue at once, or later than the old one ended.
    let after = prop_oneof![
        2 => -1200i64..-300,
        1 => Just(0i64),
        2 => 1i64..1200,
    ];
    let resets = vec(proptest::option::weighted(0.1, after), 40);
    (arb_steps(), resets).prop_map(|(steps, resets)| {
        let mut out = Vec::new();
        for (step, reset) in steps.into_iter().zip(resets) {
            out.push(KernelStep::Core(step));
            if let Some(after) = reset {
                out.push(KernelStep::Reset { after });
            }
        }
        out
    })
}

/// The frozen pre-extraction state machine as the oracle of one timeline of a kernel: created at the start of the run,
/// and again at each reset, which leaves the kernel as a new native transform.
struct Oracle {
    reference: crate::aggregate::reference::RunLoop,
    recorder: TestRecorder,
    /// The kernel's telemetry when the timeline started, which the reference's counts from zero are compared above;
    /// `None` for the first timeline, whose telemetry is compared exactly.
    kernel_base: Option<Vec<(String, Option<f64>)>>,
    /// Whether the kernel's breach flag was set when the timeline started. A reset clears it only below the context
    /// limit, so with a limit of zero it stays set, while a new transform sets it at its first drop.
    breached_before: bool,
}

impl Oracle {
    fn new(
        params: &crate::aggregate::tests::GeneratedParams, kernel_base: Option<Vec<(String, Option<f64>)>>,
        breached_before: bool,
    ) -> Self {
        let recorder = TestRecorder::default();
        Self {
            reference: reference_run_loop(params, &recorder),
            recorder,
            kernel_base,
            breached_before,
        }
    }

    /// Checks that the kernel's breach flag, its contexts, and its telemetry in this timeline are the reference's.
    fn check(
        &self, kernel: &AggregateKernel, snapshot: Vec<String>, telemetry: &TestRecorder,
    ) -> Result<(), TestCaseError> {
        prop_assert_eq!(
            self.reference.state.context_limit_breached() || self.breached_before,
            kernel.core().context_limit_breached()
        );
        prop_assert_eq!(snapshot_set(self.reference.state.snapshot_contexts()), snapshot);
        let (expected, actual) = (telemetry_values(&self.recorder), telemetry_values(telemetry));
        match &self.kernel_base {
            None => prop_assert_eq!(expected, actual),
            Some(base) => prop_assert_eq!(since(&expected, None), since(&actual, Some(base))),
        }
        Ok(())
    }
}

/// Returns telemetry values as counted since `base`, or from zero, with an unset value counted as zero.
fn since(values: &[(String, Option<f64>)], base: Option<&[(String, Option<f64>)]>) -> Vec<(String, f64)> {
    values
        .iter()
        .enumerate()
        .map(|(position, (name, value))| {
            let base = base.and_then(|base| base[position].1).unwrap_or(0.0);
            (name.clone(), value.unwrap_or(0.0) - base)
        })
        .collect()
}

/// Runs one time-driven call of `host`'s kernel to completion, in as many turns as `budgets` allow, taking a snapshot
/// between turns, and returns what it emitted.
fn in_turns(
    host: &mut Host, watermark: AdmissionTime, budgets: &mut impl Iterator<Item = Budget>,
    call: impl Fn(&mut AggregateKernel, &mut Turn<'_>) -> Progress,
) -> Result<Vec<Metric>, TestCaseError> {
    for _ in 0..100_000 {
        let budget = budgets.next().expect("budgets cycle");
        if host.call(Some(watermark), budget, None, &call) != Progress::Yielded {
            return Ok(host.emitted());
        }
        // A read-only snapshot while the call is yielded must not disturb it.
        host.snapshot();
        prop_assert!(host.kernel.has_pending_work());
    }
    Err(TestCaseError::fail("a call made no progress"))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]


    #[test]
    fn property_test_resumable_flush_matches_whole_flush(
        generated in arb_params(),
        start in 0u64..3600,
        steps in arb_kernel_steps(),
        budgets in vec((1u64..4, 1u64..12), 1..8),
        capacity in proptest::option::of(0u64..8),
        filter in proptest::option::weighted(0.3, crate::aggregate::tests::emission::arb_filter()),
        retire in any::<bool>(),
    ) {
        // Two kernels see the same run, each with a global budget of its own of the same capacity, and the same
        // emission filter, if any: one completes every call at once, the other yields at random budgets, one context a
        // call included, with a snapshot and a take of the suppressed counts between its calls.
        let params = aggregate_params(&generated);
        let budget = || ContextBudget {
            global: capacity.map_or_else(StateBudget::unlimited, StateBudget::new),
            node: StateBudget::unlimited(),
        };
        let (whole_budget, yielding_budget) = (budget(), budget());
        let (_, admit, mut flush) = params.clone().split();
        flush.filter = filter.clone().map(|filter| Arc::new(filter) as _);
        let step_params = AggregateStepParams { admit, flush, flush_interval: Duration::from_secs(15) };
        let mut whole = Host::with(params.clone(), whole_budget.clone(), AggregateTelemetry::noop())
            .with_step(step_params.clone());
        let kernel_recorder = TestRecorder::default();
        let mut yielding = {
            let _local = metrics::set_default_local_recorder(&kernel_recorder);
            Host::with(params, yielding_budget.clone(), AggregateTelemetry::new(&MetricsBuilder::default()))
                .with_step(step_params)
        };

        // Without a filter, the frozen pre-extraction state machine driven at the floored times, with the limit the
        // budget leaves, as one native transform whose clock read those times, and a new one after each reset.
        let effective = generated.limited_to(capacity);
        let mut oracle = filter.is_none().then(|| Oracle::new(&effective, None, false));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("the test runtime builds");

        let mut budgets = budgets
            .iter()
            .map(|(state_work, events)| {
                Budget::unlimited()
                    .with_limit(WorkKind::StateWork, *state_work)
                    .with_limit(WorkKind::Events, *events)
            })
            .collect::<Vec<_>>()
            .into_iter()
            .cycle();

        // Admission times carry a fraction of a second, which bucketing floors.
        let time = |secs: u64, millis: u64| AdmissionTime::from_unix_nanos(secs * 1_000_000_000 + millis * 1_000_000);
        let mut now = 1_700_000_000 + start;
        for step in &steps {
            match step {
                KernelStep::Core(Step::Admit { after, metrics }) => {
                    now = now.saturating_add_signed(*after);
                    let metrics = metrics.iter().map(|metric| metric.build(now)).collect::<Vec<_>>();
                    for host in [&mut whole, &mut yielding] {
                        host.admit(time(now, 750), metrics.clone());
                    }
                    if let Some(oracle) = &mut oracle {
                        oracle.reference.events(now, metrics.into_iter().map(Event::Metric));
                    }
                }
                KernelStep::Core(Step::Tick { after }) => {
                    now = now.saturating_add_signed(*after);
                    let expected = whole.tick(Some(time(now, 250)));
                    let actual = in_turns(&mut yielding, time(now, 250), &mut budgets, |kernel, turn| {
                        kernel.advance_time(turn)
                    })?;
                    prop_assert_eq!(multiset(&expected), multiset(&actual), "flush at {}", now);
                    if let Some(oracle) = &mut oracle {
                        let emitted = runtime
                            .block_on(reference_tick(&mut oracle.reference, now, false))
                            .map_or_else(Vec::new, |(_, emitted)| emitted);
                        prop_assert_eq!(multiset(&emitted), multiset(&expected), "frozen flush at {}", now);
                    }
                }
                KernelStep::Reset { after } => {
                    whole.whole(Some(time(now, 0)), |kernel, turn| kernel.control(turn, &mut ClockReset));
                    let expected = whole.emitted();
                    let actual = in_turns(&mut yielding, time(now, 0), &mut budgets, |kernel, turn| {
                        kernel.control(turn, &mut ClockReset)
                    })?;
                    prop_assert_eq!(multiset(&expected), multiset(&actual), "reset at {}", now);
                    prop_assert!(whole.kernel.core().is_empty());
                    // The kernel continues as a new native transform would, in a timeline that starts anywhere.
                    if oracle.is_some() {
                        let base = telemetry_values(&kernel_recorder);
                        let breached = yielding.kernel.core().context_limit_breached();
                        prop_assert!(
                            !breached || effective.context_limit == 0,
                            "a reset clears the breach below the limit"
                        );
                        oracle = Some(Oracle::new(&effective, Some(base), breached));
                    }
                    now = now.saturating_add_signed(*after);
                }
            }
            prop_assert_eq!(whole.counters, yielding.counters);
            prop_assert_eq!(whole.kernel.held(), yielding.kernel.held());
            prop_assert_eq!(
                whole.kernel.core().context_limit_breached(),
                yielding.kernel.core().context_limit_breached()
            );
            prop_assert_eq!(snapshot_set(whole.snapshot().entries), snapshot_set(yielding.snapshot().entries));
            prop_assert_eq!(whole.kernel.take_suppressed(), yielding.kernel.take_suppressed());
            prop_assert!(!yielding.kernel.has_pending_work());
            prop_assert_eq!(yielding_budget.global.used(), yielding.kernel.core().len() as u64);
            if let Some(oracle) = &oracle {
                let snapshot = snapshot_set(yielding.snapshot().entries);
                oracle.check(&yielding.kernel, snapshot, &kernel_recorder)?;
            }
        }

        // The run ends by retiring, which emits everything, or by shutting down, which discards the open buckets.
        let policy = if retire { DrainPolicy::Retire } else { DrainPolicy::Shutdown };
        whole.whole(Some(time(now, 0)), |kernel, turn| kernel.finish(turn, policy));
        let expected = whole.emitted();
        let actual = in_turns(&mut yielding, time(now, 0), &mut budgets, |kernel, turn| kernel.finish(turn, policy))?;
        prop_assert_eq!(multiset(&expected), multiset(&actual), "{:?} at {}", policy, now);
        prop_assert_eq!(whole.counters, yielding.counters);
        prop_assert_eq!(whole.kernel.take_suppressed(), yielding.kernel.take_suppressed());
        prop_assert!(whole.kernel.core().is_empty());
        prop_assert!(yielding.kernel.core().is_empty());
        prop_assert_eq!((whole_budget.global.used(), yielding_budget.global.used()), (0, 0));
        if retire {
            // Retiring emits everything it holds, so nothing is lost.
            prop_assert_eq!(whole.counters.loss(LossClass::Shutdown), LossCount::default());
        }
    }
}
