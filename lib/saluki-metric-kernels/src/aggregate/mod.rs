//! Native aggregation: the state machine that aggregates metrics into time buckets, and the decisions a host makes
//! around it.
//!
//! [`AggregateCore`] holds one entry per context and is driven by its host with explicit times, in whole Unix seconds:
//! [`admit`][AggregateCore::admit] inserts metrics when a batch arrives, and [`flush`][AggregateCore::flush] emits the
//! buckets closed by the flush time. The host owns the clock, the flush schedule, and delivery; the core owns every
//! decision about what is held and what is emitted, so every host aggregates alike.
//!
//! # Semantics
//!
//! - **Key.** A context is identified by its 64-bit identity hash; kind and metadata are not part of it. Contexts whose
//!   hashes collide merge into the first one inserted, which keeps its name and tags.
//! - **Buckets.** Buckets are aligned to Unix time and half-open, `[k·w, (k+1)·w)` for the window `w`. An inserted
//!   metric's untimestamped values collapse, merged by kind, into one point at the start of the bucket that contains
//!   the insertion time. Timestamped values keep their timestamps, unaligned.
//! - **Merge.** Into an existing entry, each point merges with the point at the same timestamp, or is added: counters
//!   and rates with equal intervals sum, gauges take the incoming value, sets unite, histograms gather samples, and
//!   distributions merge their sketches. A rate with another interval, or a value of another kind, replaces the entry's
//!   whole value set, closed buckets included. The entry keeps the metadata of its first insertion.
//! - **Context limit.** A metric for a new context is dropped while the core holds `context_limit` contexts, idle
//!   counters included; metrics for held contexts are always admitted. Dropping sets the breach flag, which only a
//!   flush that leaves fewer than `context_limit` contexts clears.
//! - **Flush.** A flush at time `t` emits every point whose bucket ended by the start of the bucket containing `t`. A
//!   host flushes only while the core holds contexts, so a flush time not reached while the core was empty is never
//!   recorded as a flush.
//! - **Idle counters.** At every flush after the first, each counter gains a zero at every bucket start `b` with
//!   `align(last flush) ≤ b < t`, `b + w - 1 < t`, and `b < last seen + expiry`, summed into any point already there.
//!   The rule does not ask whether the counter existed in those buckets, so a counter first seen after an idle period
//!   gains zeros back to the previous flush. A counter is removed at the first flush after `last seen + expiry` that
//!   leaves it without points; with no expiry, counters are never removed. Every other context is removed by the flush
//!   that empties it.
//! - **Emission.** Counters leave as rates over the window, zeros included. Histograms expand: first into a
//!   distribution under an optionally prefixed name, when they are copied to distributions, with sample weights
//!   truncated to integers, then into `<name>.<suffix>` per statistic, in order, where the count is a rate with its
//!   unit cleared and every other statistic a gauge. Every other kind leaves unchanged.
//! - **Final flush.** The last flush of a host treats every bucket as closed when it computes idle-counter zeros if
//!   `flush_open_windows` is set, but still emits only closed buckets. Open buckets are discarded.
//!
//! # Flushing
//!
//! [`AggregateCore::flush`] starts a [`Flush`], which visits one context per [`Flush::emit_next`] call, so a host that
//! delivers asynchronously can deliver each context's metrics before visiting the next one, and never holds more than
//! one context's expansion. [`AggregateCore::flush_into`] runs a whole flush into a synchronous sink.

use std::{mem::size_of, num::NonZeroU64};

use saluki_core::{
    accounting::UsageExpr,
    data_model::event::metric::{context::Context, Metric},
};
use tracing::trace;

mod histogram;
pub use self::histogram::{HistogramConfiguration, HistogramStatistic};

mod snapshot;
pub use self::snapshot::{AggregateContextSnapshotEntry, AggregateMetricType};

mod state;
use self::state::{AggregatedMetric, AggregationState, FlushCursor};

mod telemetry;
pub use self::telemetry::AggregateTelemetry;

/// The memory one held context takes in the state: its context handle and its aggregated values.
///
/// Values with more points than fit inline take more; see [`state_usage`].
pub const STATE_ENTRY_BYTES: usize = size_of::<Context>() + size_of::<AggregatedMetric>();

/// Returns the memory the state takes when it holds `context_limit` contexts.
///
/// Values with more points than fit inline, such as timestamped values spread over many buckets, take memory this does
/// not count.
pub fn state_usage(context_limit: usize) -> UsageExpr {
    UsageExpr::product(
        "aggregation state map",
        UsageExpr::sum(
            "context map entry",
            UsageExpr::struct_size::<Context>("context"),
            UsageExpr::struct_size::<AggregatedMetric>("aggregated metric"),
        ),
        UsageExpr::config("aggregate_context_limit", context_limit),
    )
}

/// Returns the memory a snapshot of `context_limit` held contexts takes.
///
/// A snapshot is built while the state stays live, so a host that serves snapshots needs this in addition to
/// [`state_usage`].
pub fn snapshot_usage(context_limit: usize) -> UsageExpr {
    UsageExpr::product(
        "retained context snapshot",
        UsageExpr::struct_size::<AggregateContextSnapshotEntry>("snapshot entry"),
        UsageExpr::config("aggregate_context_limit", context_limit),
    )
}

/// The parameters of native aggregation.
#[derive(Clone, Debug)]
pub struct AggregateParams {
    /// The bucket width, in seconds.
    pub window: NonZeroU64,

    /// The most contexts held at once, idle counters included.
    ///
    /// With `0`, every metric is dropped.
    pub context_limit: usize,

    /// How long after it was last updated an idle counter keeps emitting zeros, in seconds.
    ///
    /// `None` keeps counters forever: they still gain zeros for the closed buckets before they were last updated, but
    /// never after, and they keep counting against the context limit.
    pub counter_expiry: Option<NonZeroU64>,

    /// Whether the final flush treats every bucket as closed when it computes idle-counter zeros.
    ///
    /// A final flush emits only closed buckets either way, so this has no effect on what is emitted.
    pub flush_open_windows: bool,

    /// The statistics computed for each histogram, and whether histograms are also emitted as distributions.
    pub histograms: HistogramConfiguration,
}

/// What admitting a batch did.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AdmitOutcome {
    /// The metrics inserted.
    pub accepted: usize,

    /// The metrics dropped because the context limit was reached.
    pub dropped: usize,

    /// The points the dropped metrics carried.
    pub dropped_points: usize,

    /// Whether this batch dropped the first metric since the breach flag was last clear, which a host reports once.
    pub limit_reached: bool,
}

/// What a completed flush did.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FlushOutcome {
    /// Whether the flush cleared the breach flag: new contexts are admitted again.
    pub recovered: bool,
}

/// Native aggregation, driven by a host.
///
/// See the [module documentation][self] for the semantics. Times are whole Unix seconds read from the host's clock:
/// the arrival time of each batch, and the time of each flush.
pub struct AggregateCore {
    state: AggregationState,
    flush_open_windows: bool,
}

impl AggregateCore {
    /// Creates an empty core.
    ///
    /// The core reports its telemetry through `telemetry`; see [`AggregateTelemetry`].
    pub fn new(params: AggregateParams, telemetry: AggregateTelemetry) -> Self {
        Self {
            state: AggregationState::new(
                params.window,
                params.context_limit,
                params.counter_expiry,
                params.histograms,
                telemetry,
            ),
            flush_open_windows: params.flush_open_windows,
        }
    }

    /// Returns `true` if the core holds no contexts.
    pub fn is_empty(&self) -> bool {
        self.state.is_empty()
    }

    /// Returns the number of contexts the core holds, idle counters included.
    pub fn len(&self) -> usize {
        self.state.len()
    }

    /// Returns the context limit.
    pub fn context_limit(&self) -> usize {
        self.state.context_limit()
    }

    /// Returns `true` if a metric was dropped at the context limit since the last flush that left fewer contexts than
    /// the limit.
    pub fn context_limit_breached(&self) -> bool {
        self.state.context_limit_breached()
    }

    /// Returns the number of held contexts that hold points, and the number of points they hold.
    ///
    /// A host that stops discards these: they are the open buckets, plus any closed bucket not yet flushed.
    pub fn held_points(&self) -> (usize, usize) {
        self.state.held_points()
    }

    /// Returns a snapshot of every held context, with the kind of its values and its unit.
    pub fn snapshot_contexts(&self) -> Vec<AggregateContextSnapshotEntry> {
        self.state.snapshot_contexts()
    }

    /// Admits a batch of metrics that arrived at `now`, in whole Unix seconds.
    ///
    /// Each metric is inserted in order; one for a new context is dropped, and counted as dropped, while the context
    /// limit is reached.
    pub fn admit<I>(&mut self, now: u64, metrics: I) -> AdmitOutcome
    where
        I: IntoIterator<Item = Metric>,
    {
        let mut outcome = AdmitOutcome::default();
        for metric in metrics {
            let was_breached = self.state.context_limit_breached();
            let points = metric.values().len();
            if self.state.insert(now, metric) {
                outcome.accepted += 1;
            } else {
                trace!("Dropping metric due to context limit.");
                if !was_breached {
                    // First drop since the last recovery.
                    outcome.limit_reached = true;
                }
                self.state.telemetry().increment_events_dropped();
                outcome.dropped += 1;
                outcome.dropped_points += points;
            }
        }
        outcome
    }

    /// Starts a flush at `now`, in whole Unix seconds, or returns `None` if the core holds no contexts.
    ///
    /// An empty core is not flushed, and so a flush time reached while it is empty is not recorded: the next flush
    /// computes idle-counter zeros from the last flush that ran. `last` marks the host's final flush, which applies
    /// `flush_open_windows`.
    ///
    /// The flush must be [finished][Flush::finish], or [abandoned][Flush::abandon] if the host cannot deliver what it
    /// emitted, before the core is used again; the borrow enforces it.
    pub fn flush(&mut self, now: u64, last: bool) -> Option<Flush<'_>> {
        if self.state.is_empty() {
            return None;
        }

        // Remember if the context limit had been surpassed before this flush.
        let was_breached = self.state.context_limit_breached();
        let cursor = self.state.start_flush(now, last && self.flush_open_windows);
        Some(Flush {
            state: &mut self.state,
            cursor,
            was_breached,
        })
    }

    /// Runs a whole flush at `now` into `sink`, or does nothing and returns `None` if the core holds no contexts.
    ///
    /// See [`flush`][Self::flush].
    pub fn flush_into<F>(&mut self, now: u64, last: bool, mut sink: F) -> Option<FlushOutcome>
    where
        F: FnMut(Metric),
    {
        let mut flush = self.flush(now, last)?;
        while flush.emit_next(&mut sink) {}
        Some(flush.finish())
    }
}

/// A flush in progress.
///
/// Each [`emit_next`][Self::emit_next] call visits one held context and emits the metrics of its closed buckets, so a
/// host can deliver them before the next call. A flush ends with [`finish`][Self::finish], which removes the contexts
/// the flush emptied or expired and records the flush, or with [`abandon`][Self::abandon] when the host failed to
/// deliver: the contexts visited keep the state the visit left them in, and the flush is not recorded, so the next
/// flush computes idle-counter zeros from the previous one and removes what this one would have.
#[must_use = "a flush must be finished or abandoned"]
pub struct Flush<'a> {
    state: &'a mut AggregationState,
    cursor: FlushCursor,
    was_breached: bool,
}

impl Flush<'_> {
    /// Visits the next held context, emitting the metrics of its closed buckets into `sink`.
    ///
    /// Returns `false`, without emitting anything, once every context has been visited.
    pub fn emit_next<F>(&mut self, sink: F) -> bool
    where
        F: FnMut(Metric),
    {
        self.state.flush_next(&mut self.cursor, sink)
    }

    /// Completes the flush: removes the contexts it emptied or expired, clears the breach flag if fewer contexts than
    /// the limit remain, and records the flush time.
    ///
    /// The host finishes a flush once [`emit_next`][Self::emit_next] has returned `false`, after visiting every
    /// context.
    pub fn finish(self) -> FlushOutcome {
        let Self {
            state,
            cursor,
            was_breached,
        } = self;
        state.finish_flush(cursor);
        state.telemetry().increment_flushes();
        FlushOutcome {
            recovered: was_breached && !state.context_limit_breached(),
        }
    }

    /// Gives up on the flush after a failed delivery: nothing more is removed or recorded.
    pub fn abandon(self) {
        self.state.telemetry().increment_flushes();
    }
}

#[cfg(test)]
mod reference;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod compatibility_tests;
