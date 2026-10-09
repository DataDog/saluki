//! Opaque hosts: native components without kernels, run through synchronous adapters.

use saluki_core::{
    data_model::event::metric::Metric,
    execution::{AdmissionTime, DrainPolicy, LossClass, PortIndex, TurnCounters},
};
use snafu::Snafu;

use crate::operators::ConfiguredOperator;

/// One batch arriving at an opaque host.
#[derive(Clone, Debug)]
pub struct HostBatch {
    /// The admission time the batch was stamped with upstream, if it is stamped.
    ///
    /// Opaque hosts do not carry headers: whatever the host emits is re-admitted at the host's own output.
    pub admitted_at: Option<AdmissionTime>,

    /// The host time at which the batch arrives: the later of the last flush tick and its admission time.
    ///
    /// A native component that reads the wall clock when a batch arrives, as native aggregation does to choose a
    /// bucket, reads this.
    pub arrived_at: AdmissionTime,

    /// The metrics.
    pub events: Vec<Metric>,
}

/// One flush tick of an opaque host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HostTick {
    /// The host time of the tick.
    pub now: AdmissionTime,

    /// The host's watermark: the earliest of the latest admission times of the admission points upstream of it, or
    /// `None` if one of them has admitted nothing yet.
    pub watermark: Option<AdmissionTime>,
}

/// The end of an opaque host's input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HostFinish {
    /// The host time: the clock of the last flush tick.
    pub now: AdmissionTime,

    /// Why the host finishes: at shutdown, or because its generation retires.
    pub policy: DrainPolicy,
}

/// The in-memory output of an opaque host: the metrics it emits, per output port, and the loss it reports.
///
/// Ports are numbered in the order of the operator's output ports. Whatever a host emits during one call is re-admitted
/// downstream when the call returns, stamped by the host's admission point. A host emits only records its output ports'
/// types allow: the interpreter rejects anything else, as it rejects an ill-typed batch at a source.
///
/// A host reports every contribution it fails to deliver with [`record_loss`][Self::record_loss], in the classes its
/// operator's contract declares, such as aggregation's capacity loss at its context limit and shutdown loss of its
/// open buckets. The interpreter adds it to the node's loss in the run's output.
#[derive(Debug)]
pub struct HostOutput {
    ports: Vec<Vec<Metric>>,
    counters: TurnCounters,
}

impl HostOutput {
    /// Returns an empty output with `port_count` ports.
    pub fn new(port_count: usize) -> Self {
        Self {
            ports: (0..port_count).map(|_| Vec::new()).collect(),
            counters: TurnCounters::default(),
        }
    }

    /// Returns the number of output ports.
    pub fn port_count(&self) -> usize {
        self.ports.len()
    }

    /// Emits a metric on a port.
    ///
    /// # Panics
    ///
    /// Panics if the port does not exist. Writing to a port the operator does not have is a bug in the adapter.
    pub fn push(&mut self, port: PortIndex, metric: Metric) {
        let count = self.ports.len();
        match self.ports.get_mut(port.get()) {
            Some(metrics) => metrics.push(metric),
            None => panic!(
                "adapter wrote to port {} but the host has only {} ports",
                port.get(),
                count
            ),
        }
    }

    /// Records the loss of `events` events, carrying `points` data points in total, as `class`.
    pub fn record_loss(&mut self, class: LossClass, events: u64, points: u64) {
        self.counters.record_loss(class, events, points);
    }

    /// Returns the loss recorded so far.
    pub fn loss(&self) -> &TurnCounters {
        &self.counters
    }

    /// Returns the metrics emitted on each port, in port order, and the loss recorded, leaving the output empty.
    pub(crate) fn take(&mut self) -> (Vec<Vec<Metric>>, TurnCounters) {
        (
            self.ports.iter_mut().map(std::mem::take).collect(),
            self.counters.take(),
        )
    }
}

/// A synchronous adapter over a native component that has no kernel, for the reference interpreter.
///
/// The interpreter runs every node with the `opaque_host` boundary through an adapter, which drives the component's
/// state machine directly, without its task, timers, or channels. Native aggregation runs this way, through
/// [`AggregateAdapter`][super::adapters::AggregateAdapter], until it is a kernel.
///
/// Each method receives the host time explicitly and emits into a [`HostOutput`]. What it emits is **re-admitted**
/// downstream at the host's output: stamped with the host admission clock's time, which is the arrival time for output
/// emitted while admitting a batch and the tick's time for output emitted at a tick, and with the generation active at
/// that moment.
pub trait OpaqueHostAdapter: Send {
    /// Processes one batch arriving at the host's input.
    fn admit(&mut self, batch: HostBatch, output: &mut HostOutput);

    /// Runs one flush tick.
    ///
    /// The default does nothing, for hosts whose output depends only on their input.
    fn tick(&mut self, _tick: HostTick, _output: &mut HostOutput) {}

    /// Finishes: at the end of input under [`DrainPolicy::Shutdown`], or when the host's generation retires under
    /// [`DrainPolicy::Retire`].
    ///
    /// The default does nothing.
    fn finish(&mut self, _finish: HostFinish, _output: &mut HostOutput) {}
}

/// An error building an opaque host's adapter.
#[derive(Clone, Debug, Eq, PartialEq, Snafu)]
#[snafu(display("Failed to build the opaque host's adapter: {}", message))]
pub struct AdapterError {
    /// What went wrong.
    pub message: String,
}

/// Builds the adapter of one opaque host node from the node's configured operator.
pub type HostFactory<'a> = dyn Fn(&ConfiguredOperator) -> Result<Box<dyn OpaqueHostAdapter>, AdapterError> + 'a;
