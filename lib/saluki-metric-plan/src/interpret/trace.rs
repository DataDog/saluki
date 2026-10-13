//! Traces: the sequences of events that plans are executed over.

use std::sync::Arc;

use saluki_core::{data_model::event::metric::Metric, execution::AdmissionTime};

use crate::ir::{NodeId, OutputId, Plan, SignalName};

/// One event of a [`Trace`].
#[derive(Clone, Debug)]
pub enum TraceEvent {
    /// A batch of metrics enters the plan at a source, stamped with its admission time.
    ///
    /// `admitted_at` is the time the source's admission clock stamped. A batch can arrive after flush ticks later than
    /// its stamp, as a queued batch would: the trace's order is the order of arrival. An empty batch is a heartbeat.
    Admit {
        /// The `source` node the batch enters at.
        source: NodeId,

        /// The lane of the source the batch enters at: each lane of a source with lanes is an admission point of its
        /// own, and every lane shares the source's clock. Lane 0 for a source without lanes.
        lane: u16,

        /// The batch's admission time, `t_a`.
        admitted_at: AdmissionTime,

        /// The metrics, at most the [batch size][crate::limits::BATCH_SIZE].
        batch: Vec<Metric>,
    },

    /// An idle source reports that its admission clock has reached `admitted_at`, without admitting anything.
    ///
    /// A heartbeat is stamped as a batch is: it can reset the source's clock, and it is assigned a generation.
    Heartbeat {
        /// The `source` node.
        source: NodeId,

        /// The lane of the source whose admission point reports, as for `admit`.
        lane: u16,

        /// The source's admission time.
        admitted_at: AdmissionTime,
    },

    /// The host's flush timer fires at `clock`: every state owner runs one flush tick.
    Advance {
        /// The host time of the tick. Ticks follow a monotonic timer, so their clocks never decrease.
        clock: AdmissionTime,
    },

    /// A plan is installed: it replaces the active one from the next event on, or, when its state owners' lineages
    /// change, from a cut. An installation while a cut is in flight waits for the cut to finish.
    Install {
        /// The plan.
        plan: Arc<Plan>,
    },

    /// An output's capacity is bounded, for tests of delivery policies.
    Capacity {
        /// The output.
        output: OutputId,

        /// How many more events the output accepts.
        limit: u64,
    },

    /// The host publishes a new value of a signal.
    Signal {
        /// The signal.
        name: SignalName,

        /// Its new value.
        value: SignalValue,
    },

    /// Input ends: every kernel and host finishes under the shutdown policy.
    Close,

    /// The installation whose cut is in flight is abandoned, if no admission point has stamped a batch or heartbeat
    /// with its generation yet.
    Cancel,
}

impl TraceEvent {
    /// Returns the event's name, as the specification writes it: `admit`, `heartbeat`, `advance`, `install`,
    /// `capacity`, `signal`, `close`, or `cancel`.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Admit { .. } => "admit",
            Self::Heartbeat { .. } => "heartbeat",
            Self::Advance { .. } => "advance",
            Self::Install { .. } => "install",
            Self::Capacity { .. } => "capacity",
            Self::Signal { .. } => "signal",
            Self::Close => "close",
            Self::Cancel => "cancel",
        }
    }
}

/// A value of a host signal, of one of the [signal types][crate::extension::SignalType].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SignalValue {
    /// A boolean signal.
    Bool(bool),

    /// A count.
    Count(u64),

    /// A ratio, such as queue occupancy.
    Ratio(f64),
}

/// A trace: the sequence of events that a plan is executed over.
///
/// Equivalence is defined over traces: two executions of one trace are equivalent when every output receives the same
/// points. Build a trace with its builder methods, in order:
///
/// ```
/// # use std::str::FromStr;
/// # use saluki_core::{data_model::event::metric::Metric, execution::AdmissionTime};
/// # use saluki_metric_plan::{interpret::Trace, ir::{NodeId, Plan}};
/// # fn example(plan: Plan, metrics: Vec<Metric>) {
/// let source = NodeId::from_str("dsd_in").unwrap();
/// let trace = Trace::new()
///     .install(plan)
///     .admit(source, AdmissionTime::from_unix_secs(100), metrics)
///     .advance(AdmissionTime::from_unix_secs(115))
///     .close();
/// assert_eq!(trace.events().len(), 4);
/// # }
/// ```
#[derive(Clone, Debug, Default)]
pub struct Trace {
    events: Vec<TraceEvent>,
}

impl Trace {
    /// Returns an empty trace.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the events, in order.
    pub fn events(&self) -> &[TraceEvent] {
        &self.events
    }

    /// Appends an event.
    pub fn push(&mut self, event: TraceEvent) {
        self.events.push(event);
    }

    /// Returns this trace without its `capacity` events: the same trace with no output bounded.
    ///
    /// What the original plan delivers on it is what the policy includes, which
    /// [`compare_under_capacity`][crate::equivalence::compare_under_capacity] checks executions under capacity against.
    pub fn without_capacity(&self) -> Self {
        self.events
            .iter()
            .filter(|event| !matches!(event, TraceEvent::Capacity { .. }))
            .cloned()
            .collect()
    }

    /// Returns this trace followed by an `install` of `plan`.
    pub fn install(mut self, plan: Plan) -> Self {
        self.push(TraceEvent::Install { plan: Arc::new(plan) });
        self
    }

    /// Returns this trace followed by an `admit` of `batch` at `source`, stamped `admitted_at`.
    pub fn admit(self, source: NodeId, admitted_at: AdmissionTime, batch: Vec<Metric>) -> Self {
        self.admit_at_lane(source, 0, admitted_at, batch)
    }

    /// Returns this trace followed by an `admit` of `batch` at lane `lane` of `source`, stamped `admitted_at`.
    pub fn admit_at_lane(mut self, source: NodeId, lane: u16, admitted_at: AdmissionTime, batch: Vec<Metric>) -> Self {
        self.push(TraceEvent::Admit {
            source,
            lane,
            admitted_at,
            batch,
        });
        self
    }

    /// Returns this trace followed by a `heartbeat` from `source` at `admitted_at`.
    pub fn heartbeat(self, source: NodeId, admitted_at: AdmissionTime) -> Self {
        self.heartbeat_at_lane(source, 0, admitted_at)
    }

    /// Returns this trace followed by a `heartbeat` from lane `lane` of `source` at `admitted_at`.
    pub fn heartbeat_at_lane(mut self, source: NodeId, lane: u16, admitted_at: AdmissionTime) -> Self {
        self.push(TraceEvent::Heartbeat {
            source,
            lane,
            admitted_at,
        });
        self
    }

    /// Returns this trace followed by an `advance` to `clock`.
    pub fn advance(mut self, clock: AdmissionTime) -> Self {
        self.push(TraceEvent::Advance { clock });
        self
    }

    /// Returns this trace followed by a `capacity` bound of `limit` events on `output`.
    pub fn capacity(mut self, output: OutputId, limit: u64) -> Self {
        self.push(TraceEvent::Capacity { output, limit });
        self
    }

    /// Returns this trace followed by a `signal` setting `name` to `value`.
    pub fn signal(mut self, name: SignalName, value: SignalValue) -> Self {
        self.push(TraceEvent::Signal { name, value });
        self
    }

    /// Returns this trace followed by `close`.
    pub fn close(mut self) -> Self {
        self.push(TraceEvent::Close);
        self
    }

    /// Returns this trace followed by `cancel`.
    pub fn cancel(mut self) -> Self {
        self.push(TraceEvent::Cancel);
        self
    }

    /// Returns this trace with the stamps of `source` spread over `lanes` of its lanes, as a trace that stamps one
    /// point runs on a source with lanes: the trace's `i`-th stamp of `source` (an `admit` or a `heartbeat`) is stamped
    /// at lane `i mod lanes`, and mirrored at once, at the same admission time, as a heartbeat of every other lane,
    /// before the trace's next event.
    ///
    /// Every lane's latest time then equals the single point's after each event, so a state owner's watermark, its
    /// ticks, gates, and expired graces, a cut's crossing, and a reset's window, which opens and closes within one
    /// mirrored stamp, all happen at the instants and seconds they happen at one lane. Stamps of other sources keep
    /// their lanes.
    pub fn over_lanes(&self, source: &NodeId, lanes: u16) -> Self {
        let lanes = lanes.max(1);
        let mut spread = Self::new();
        let mut stamps = 0usize;
        for event in &self.events {
            let (stamped, admitted_at) = match event {
                TraceEvent::Admit {
                    source: at,
                    admitted_at,
                    ..
                }
                | TraceEvent::Heartbeat {
                    source: at,
                    admitted_at,
                    ..
                } if at == source => (true, *admitted_at),
                _ => (false, AdmissionTime::UNIX_EPOCH),
            };
            if !stamped {
                spread.push(event.clone());
                continue;
            }
            let lane = (stamps % usize::from(lanes)) as u16;
            stamps += 1;
            spread.push(match event.clone() {
                TraceEvent::Admit {
                    source,
                    admitted_at,
                    batch,
                    ..
                } => TraceEvent::Admit {
                    source,
                    lane,
                    admitted_at,
                    batch,
                },
                TraceEvent::Heartbeat {
                    source, admitted_at, ..
                } => TraceEvent::Heartbeat {
                    source,
                    lane,
                    admitted_at,
                },
                other => other,
            });
            for other in (0..lanes).filter(|other| *other != lane) {
                spread.push(TraceEvent::Heartbeat {
                    source: source.clone(),
                    lane: other,
                    admitted_at,
                });
            }
        }
        spread
    }
}

impl FromIterator<TraceEvent> for Trace {
    fn from_iter<I: IntoIterator<Item = TraceEvent>>(events: I) -> Self {
        Self {
            events: events.into_iter().collect(),
        }
    }
}

impl Extend<TraceEvent> for Trace {
    fn extend<I: IntoIterator<Item = TraceEvent>>(&mut self, events: I) {
        self.events.extend(events);
    }
}
