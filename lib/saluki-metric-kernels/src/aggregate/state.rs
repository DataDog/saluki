//! The native aggregation state machine: one entry per context, merged on insert and flushed by closed bucket.

use std::{hash::BuildHasher as _, num::NonZeroU64, time::Duration};

use ddsketch::DDSketch;
use hashbrown::HashTable;
use saluki_core::{
    data_model::event::metric::{context::Context, *},
    execution::BudgetReservation,
};
use smallvec::SmallVec;
use stringtheory::MetaString;
use tracing::{debug, trace};

use super::{
    emission::{EmissionFilter, OutputKind, Suppressed},
    histogram::{HistogramConfiguration, HistogramStatistic},
    snapshot::{AggregateContextSnapshotEntry, AggregateMetricType},
    telemetry::AggregateTelemetry,
    ContextBudget, LastSeen,
};

/// One context's aggregated values, the metadata of its first insertion, and when it was last updated.
#[derive(Clone)]
pub(super) struct AggregatedMetric {
    values: MetricValues,
    metadata: MetricMetadata,
    last_seen: u64,
}

/// The hasher of the context table: randomly seeded per state, as the native map's is.
type ContextHasher = foldhash::quality::RandomState;

/// The aggregation state: every context held, and what a flush needs to know about previous flushes.
///
/// Contexts are held in a hash table keyed by context identity, its 64-bit hash, so colliding contexts merge into the
/// first one inserted. The table is a [`HashTable`] rather than a map so that a flush can visit its entries one at a
/// time by bucket position and resume between them. Positions stay valid because nothing is inserted while a flush is
/// in progress, and the only entries removed then, by a drain or a discard, are the ones it visits: removing an entry
/// leaves every other one where it is.
///
/// The state keeps only what defines its buckets, the bucket width. Every other parameter is passed to the call it
/// applies to, so that one state can serve batches and flushes whose parameters differ: the context limit to each
/// insertion and to the end of each flush, and the counter expiry, histogram statistics, final-flush setting, and
/// emission filter to each flush.
///
/// A state either admits by its own context count, as native aggregation does, or shares budgets of contexts with
/// other states (see [`ContextLimit`]). A state that shares budgets holds one unit of each budget per context, and
/// releases them as it removes contexts; its owner releases what is left when it discards the state
/// ([`release_all`][Self::release_all]).
pub(super) struct AggregationState {
    contexts: HashTable<(Context, AggregatedMetric)>,
    hasher: ContextHasher,
    contexts_remove_buf: Vec<Context>,
    bucket_width_secs: NonZeroU64,
    last_flush: u64,
    /// How a held context's last update follows the times it is updated at.
    last_seen: LastSeen,
    telemetry: AggregateTelemetry,
    /// Tracks whether the context limit has been breached. Starts out as `false`. Set to `true` on the first dropped
    /// metric. Reset to `false` when the context count drops below the limit during flush.
    context_limit_breached: bool,
}

/// The limit a state admits new contexts by, and clears its breach flag by, for one call: the admission of one batch,
/// or the end of one flush.
///
/// A limit that shares budgets holds the call's reservation of their units, which the call's new contexts take. It
/// takes each unit as its new context is admitted, unless the state is one lane's share of a state owner on several
/// lanes, which takes them in blocks ([`in_blocks`][Self::in_blocks]); the reservation returns the units it took and
/// no new context used when the limit is dropped, at the end of the call.
#[derive(Debug)]
pub(super) struct ContextLimit<'a> {
    /// The most contexts held: by the state itself, or, when it shares budgets, by every state of its node together.
    pub(super) limit: usize,

    /// The budgets the state shares with other states, or `None` if it admits by its own context count.
    pub(super) shared: Option<&'a ContextBudget>,

    /// The units of the node's and the global budget the call reserved for its new contexts, if the state shares
    /// budgets.
    reservation: Option<BudgetReservation<'a>>,
}

impl<'a> ContextLimit<'a> {
    /// The limit of a state that admits by its own context count.
    pub(super) fn exclusive(limit: usize) -> Self {
        Self {
            limit,
            shared: None,
            reservation: None,
        }
    }

    /// The limit of a state that shares `budget`, where `limit` caps the contexts of every state of its node.
    ///
    /// Its new contexts take one unit of each budget as each is admitted.
    pub(super) fn shared(limit: usize, budget: &'a ContextBudget) -> Self {
        let reservation = BudgetReservation::new(&budget.node, limit_units(limit), &budget.global).with_max_block(1);
        Self {
            limit,
            shared: Some(budget),
            reservation: Some(reservation),
        }
    }

    /// Makes the call's new contexts take their units in blocks, for a state that is one lane's share of a state owner
    /// on several lanes: a first block of `first_block` units, as many as the lane's previous batch admitted, and later
    /// ones as large as the units handed out, up to [`BudgetReservation::MAX_BLOCK`].
    ///
    /// A limit that does not share budgets is unchanged.
    pub(super) fn in_blocks(mut self, first_block: u64) -> Self {
        self.reservation = self.reservation.map(|reservation| {
            reservation
                .with_max_block(BudgetReservation::MAX_BLOCK)
                .with_first_block(first_block)
        });
        self
    }

    /// Returns the new contexts the call admitted against shared budgets: the units its reservation handed out.
    pub(super) fn admitted(&self) -> u64 {
        self.reservation.as_ref().map_or(0, BudgetReservation::taken)
    }

    /// Takes room for a new context in a state holding `len` contexts, or returns `false` if there is none.
    ///
    /// A state that admits by its own count has room below the limit. A state that shares budgets has room while its
    /// node's budget counts fewer contexts than the limit and the global budget has a unit left; the new context then
    /// holds one unit of each.
    ///
    /// The units come from the call's reservation; see [`BudgetReservation`]. Each acquisition takes the node's units
    /// first, bounded by the limit, as native aggregation checks its own count first: states of one node that admit at
    /// once, from several tasks, then never hold more than the limit together, and a node at its limit never holds a
    /// global unit, even for a moment, that another node's context could be refused for. The node's units the global
    /// budget has no room for go back at once.
    ///
    /// By default the reservation takes one unit of each budget per new context, so the state is refused only while
    /// admitted contexts fill a budget, whatever other tasks do at once, as with no reservation at all. With
    /// [`in_blocks`][Self::in_blocks], it takes them in blocks, one compare-and-swap on each budget per block, and
    /// returns the ones it did not use when the call ends. Units that sit in the reservation of another lane's batch,
    /// fewer than [`BudgetReservation::MAX_BLOCK`] for each, count as in use until that batch ends, which can refuse a
    /// new context: capacity behavior, as is which contexts win at the limit.
    fn try_admit(&mut self, len: usize) -> bool {
        // `saluki-core`'s `loom` models of budgets (`execution::budget::loom_tests`) run this protocol, the reservation
        // a batch takes its units through, in blocks and one unit at a time: a model here would only call it.
        match &mut self.reservation {
            None => len < self.limit,
            Some(reservation) => reservation.take(),
        }
    }

    /// Returns `true` if a state holding `len` contexts is below the limit, so that a flush clears its breach flag.
    ///
    /// A state that shares budgets reads their counts. No unit stays reserved between batches, and none sits in a
    /// reservation that takes units one at a time, so a flush that ends while no lane's batch takes units in blocks
    /// decides by held contexts alone. One that ends during another lane's batch counts the units that batch reserved
    /// and does not use, fewer than [`BudgetReservation::MAX_BLOCK`], and can keep the flag for a later pass to clear:
    /// a drop before that pass belongs to the episode it continues, which warned already. It never clears the flag
    /// while the held contexts fill a budget.
    fn is_below(&self, len: usize) -> bool {
        match self.shared {
            None => len < self.limit,
            Some(budget) => {
                budget.node.used() < limit_units(self.limit) && budget.global.used() < budget.global.capacity()
            }
        }
    }
}

/// Returns a context limit as budget units.
fn limit_units(limit: usize) -> u64 {
    u64::try_from(limit).unwrap_or(u64::MAX)
}

/// What inserting a metric did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Insertion {
    /// The metric was for a new context and the limit left no room for it: it was dropped, and the breach flag set.
    Rejected,

    /// The metric was inserted. Its context held `replaced_points` points of another kind, or of a rate over another
    /// interval, which the metric's values replaced.
    Inserted { replaced_points: usize },
}

/// What a pass over the state counted as it visited contexts.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct PassCounts {
    /// The contexts a discard removed while they held points.
    pub(super) discarded: usize,

    /// The points those contexts held.
    pub(super) discarded_points: usize,

    /// The records the pass skipped because its emission filter drops them.
    pub(super) suppressed: Suppressed,
}

/// What a pass over the state does with each context it visits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FlushMode {
    /// A native flush: emits the closed buckets, adds idle-counter zeros, and removes the contexts it empties or
    /// expires. It is recorded as the last flush.
    Closed,

    /// A drain: emits every point held, whatever its bucket, and removes every context. Afterwards the state is as if
    /// it had never flushed.
    All,

    /// A discard: emits nothing, removes every context, and counts the contexts that held points and their points.
    /// Afterwards the state is as if it had never flushed.
    Discard,
}

/// What a flush computes once, before it visits each context.
#[derive(Debug)]
pub(super) struct FlushPlan {
    mode: FlushMode,
    current_time: u64,
    split_timestamp: u64,
    counter_expire_secs: u64,
    zero_value_buckets: SmallVec<[(u64, MetricValues); 4]>,
}

/// A pass in progress: its plan, the position of the next table bucket to visit, the context whose removal has yet to
/// be queued, the contexts removed as they were visited, and what the pass has counted since its counts were last
/// taken.
///
/// In a native flush, a context is queued for removal only once its metrics have been emitted and the next context is
/// visited, or the flush finishes, so a flush abandoned while its last context's metrics are being delivered leaves
/// that context queued exactly as the native flush, which stopped at the failed delivery, did. A drain or a discard
/// instead removes each context as it visits it, so that its removals, like its emissions, are spread over its steps.
#[derive(Debug)]
pub(super) struct FlushCursor {
    plan: FlushPlan,
    next_bucket: usize,
    pending_removal: Option<usize>,
    removed: usize,
    counts: PassCounts,
}

impl FlushCursor {
    pub(super) fn mode(&self) -> FlushMode {
        self.plan.mode
    }

    pub(super) fn current_time(&self) -> u64 {
        self.plan.current_time
    }

    /// Takes what the pass has counted since its counts were last taken.
    pub(super) fn take_counts(&mut self) -> PassCounts {
        std::mem::take(&mut self.counts)
    }
}

impl AggregationState {
    pub(super) fn new(bucket_width_secs: NonZeroU64, telemetry: AggregateTelemetry) -> Self {
        Self {
            contexts: HashTable::new(),
            hasher: ContextHasher::default(),
            contexts_remove_buf: Vec::new(),
            bucket_width_secs,
            last_flush: 0,
            last_seen: LastSeen::default(),
            telemetry,
            context_limit_breached: false,
        }
    }

    /// Sets how a held context's last update follows the times it is updated at.
    pub(super) fn set_last_seen(&mut self, rule: LastSeen) {
        self.last_seen = rule;
    }

    pub(super) fn is_empty(&self) -> bool {
        self.contexts.is_empty()
    }

    pub(super) fn len(&self) -> usize {
        self.contexts.len()
    }

    pub(super) fn bucket_width_secs(&self) -> NonZeroU64 {
        self.bucket_width_secs
    }

    pub(super) fn context_limit_breached(&self) -> bool {
        self.context_limit_breached
    }

    pub(super) fn telemetry_mut(&mut self) -> &mut AggregateTelemetry {
        &mut self.telemetry
    }

    /// Returns the time of the last flush, in whole Unix seconds, or `0` if the state never flushed since it was
    /// created or last forgot its flushes.
    pub(super) fn last_flush(&self) -> u64 {
        self.last_flush
    }

    /// Returns `true` if the state, holding what it holds now, is below `limit`, as the end of a flush judges it to
    /// clear its breach flag.
    pub(super) fn is_below_limit(&self, limit: &ContextLimit<'_>) -> bool {
        limit.is_below(self.contexts.len())
    }

    /// Returns `true` if the state holds `context`, idle or not.
    pub(super) fn contains(&self, context: &Context) -> bool {
        self.contexts
            .find(self.hasher.hash_one(context), |(held, _)| held == context)
            .is_some()
    }

    /// Forgets every flush, as a drain or a discard does when it ends: the next flush is as the first one.
    pub(super) fn forget_flushes(&mut self) {
        self.last_flush = 0;
    }

    /// Returns the number of contexts holding values, and the number of points they hold.
    pub(super) fn held_points(&self) -> (usize, usize) {
        self.contexts
            .iter()
            .filter(|(_, aggregated)| !aggregated.values.is_empty())
            .fold((0, 0), |(contexts, points), (_, aggregated)| {
                (contexts + 1, points + aggregated.values.len())
            })
    }

    pub(super) fn snapshot_contexts(&self) -> Vec<AggregateContextSnapshotEntry> {
        let mut snapshot = Vec::with_capacity(self.contexts.len());
        for (context, aggregated) in &self.contexts {
            snapshot.push(AggregateContextSnapshotEntry::new(
                context.clone(),
                AggregateMetricType::from(&aggregated.values),
                aggregated.metadata.unit.clone(),
            ));
        }
        snapshot
    }

    /// Inserts `metric`, which arrived at `timestamp`, unless it is for a new context and `limit` leaves no room for
    /// it.
    pub(super) fn insert(&mut self, timestamp: u64, metric: Metric, limit: &mut ContextLimit<'_>) -> Insertion {
        let hash = self.hasher.hash_one(metric.context());

        // If we haven't seen this context yet, and it would put us over the limit to insert it, then return early.
        if self
            .contexts
            .find(hash, |(context, _)| context == metric.context())
            .is_none()
            && !limit.try_admit(self.contexts.len())
        {
            self.context_limit_breached = true;
            return Insertion::Rejected;
        }

        let (context, mut values, metadata) = metric.into_parts();

        // Collapse all non-timestamped values into a single timestamped value.
        //
        // We do this pre-aggregation step because unless we're merging into an existing context, we'll end up with
        // however many values were in the original metric instead of full aggregated values.
        let bucket_ts = align_to_bucket_start(timestamp, self.bucket_width_secs);
        values.collapse_non_timestamped(bucket_ts);

        trace!(
            bucket_ts,
            kind = values.as_str(),
            "Inserting metric into aggregation state."
        );

        // If we're already tracking this context, update the last seen time and merge the new values into the existing
        // values. Otherwise, create a new entry.
        match self.contexts.find_mut(hash, |(existing, _)| *existing == context) {
            Some((_, aggregated)) => {
                // We ignore metadata changes within a flush interval to keep things simple.
                aggregated.last_seen = match self.last_seen {
                    LastSeen::LastArrival => timestamp,
                    LastSeen::Latest => aggregated.last_seen.max(timestamp),
                };

                // Values of another kind, or a rate over another interval, replace every point held, closed buckets
                // included: those points are lost.
                let replaced_points = if replaces(&aggregated.values, &values) {
                    aggregated.values.len()
                } else {
                    0
                };
                aggregated.values.merge(values);
                Insertion::Inserted { replaced_points }
            }
            None => {
                self.telemetry.increment_contexts(&context, &values);

                let hasher = &self.hasher;
                self.contexts.insert_unique(
                    hash,
                    (
                        context,
                        AggregatedMetric {
                            values,
                            metadata,
                            last_seen: timestamp,
                        },
                    ),
                    |(context, _)| hasher.hash_one(context),
                );

                self.check_bounds(limit);
                Insertion::Inserted { replaced_points: 0 }
            }
        }
    }

    /// Checks the bounds the context count keeps under `limit`, with Antithesis and, in debug builds, assertions.
    ///
    /// A state that admits by its own count holds at most `limit` contexts whenever it admits one. A state that shares
    /// budgets is bounded by them instead, since a later call may pass a lower limit than the contexts it already
    /// holds, which keep their units: the global budget never counts more than its capacity, and the node's budget
    /// counts at least this state's contexts.
    fn check_bounds(&self, limit: &ContextLimit<'_>) {
        let len = self.contexts.len();
        match limit.shared {
            None => {
                let context_limit = limit.limit;
                debug_assert!(
                    len <= context_limit,
                    "{} contexts held, over the limit of {}",
                    len,
                    context_limit
                );
                saluki_antithesis::always_le!(
                    len,
                    context_limit,
                    "aggregate context map within context_limit",
                    { "len": len, "limit": context_limit }
                );
            }
            Some(budget) => {
                let (global_used, global_capacity) = (budget.global.used(), budget.global.capacity());
                let (held, node_used) = (limit_units(len), budget.node.used());
                debug_assert!(
                    global_used <= global_capacity,
                    "the global context budget counts {} contexts, over its capacity of {}",
                    global_used,
                    global_capacity
                );
                debug_assert!(
                    held <= node_used,
                    "{} contexts held, but the node budget counts {}",
                    held,
                    node_used
                );
                saluki_antithesis::always_le!(
                    global_used,
                    global_capacity,
                    "aggregate global context budget within its capacity",
                    { "used": global_used, "capacity": global_capacity }
                );
                saluki_antithesis::always_le!(
                    held,
                    node_used,
                    "aggregate contexts held within the node context budget",
                    { "len": held, "node_used": node_used }
                );
            }
        }
    }

    /// Starts a flush at `current_time`: computes the split timestamp and the zero-value buckets for idle counters.
    ///
    /// `counter_expiry` is how long after it was last updated an idle counter keeps emitting zeros, or `None` to keep
    /// counters forever.
    pub(super) fn start_flush(
        &self, current_time: u64, flush_open_buckets: bool, counter_expiry: Option<NonZeroU64>,
    ) -> FlushCursor {
        self.start_flush_since(current_time, self.last_flush, flush_open_buckets, counter_expiry)
    }

    /// Starts a flush at `current_time` as [`start_flush`][Self::start_flush] does, with idle-counter zeros starting
    /// from `last_flush`, the previous flush, in place of the state's own: `0` for none, so that the flush adds no
    /// zeros, as a first flush does.
    ///
    /// The lanes of a lineage partitioned over several states pass the latest flush at which any of them held
    /// contexts, which with one state is its own.
    pub(super) fn start_flush_since(
        &self, current_time: u64, last_flush: u64, flush_open_buckets: bool, counter_expiry: Option<NonZeroU64>,
    ) -> FlushCursor {
        let bucket_width_secs = self.bucket_width_secs;
        let counter_expire_secs = counter_expiry.map(|d| d.get()).unwrap_or(0);

        // We want our split timestamp to be before the start of the current bucket, which ensures any timestamp that is
        // less than or equal to `split_timestamp` resides in a closed bucket.
        let split_timestamp = align_to_bucket_start(current_time, bucket_width_secs).saturating_sub(1);

        // Calculate the buckets we need to potentially generate zero-value counters for.
        //
        // We only need to do this if we've flushed before, since we won't have any knowledge of which counters are idle
        // or not until that happens.
        let mut zero_value_buckets = SmallVec::<[(u64, MetricValues); 4]>::new();
        if last_flush != 0 {
            let start = align_to_bucket_start(last_flush, bucket_width_secs);

            // Clock-skew guards. Bucketing reads the wall clock while the flush cadence is monotonic, so a wall-clock
            // jump is not bounded by the flush interval. A backward jump empties the zero-value range (a silent counter
            // gap); a forward jump makes the loop below run once per bucket across the whole jumped span — O(jump)
            // work and allocation. Assert before the loop so a flood fails fast rather than after the damage is done.
            saluki_antithesis::always_ge!(
                current_time,
                last_flush,
                "aggregate flush wall-clock did not move backward",
                { "current_time": current_time, "last_flush": last_flush }
            );
            // The 10_000 bound is generous. A default 15s flush over a 10s bucket yields one or two buckets. The bound
            // trips only on a multi-hour wall-clock jump, never on a slow-but-sane flush.
            saluki_antithesis::always_le!(
                current_time.saturating_sub(last_flush) / bucket_width_secs.get(),
                10_000,
                "aggregate zero-value bucket span bounded across a flush",
                {
                    "current_time": current_time,
                    "last_flush": last_flush,
                    "bucket_width_secs": bucket_width_secs.get()
                }
            );

            for bucket_start in (start..current_time).step_by(bucket_width_secs.get() as usize) {
                if is_bucket_closed(current_time, bucket_start, bucket_width_secs, flush_open_buckets) {
                    zero_value_buckets.push((bucket_start, MetricValues::counter((bucket_start, 0.0))));
                }
            }

            // Anti-vacuity anchor: prove the idle-counter zero-value path actually runs in some timeline.
            saluki_antithesis::sometimes!(
                !zero_value_buckets.is_empty(),
                "aggregate flush generated zero-value counter buckets",
                { "count": zero_value_buckets.len() }
            );
        }

        // Iterate over each context we're tracking, and flush any values that are in buckets which are now closed.
        debug!(timestamp = current_time, "Flushing buckets.");

        FlushCursor {
            plan: FlushPlan {
                mode: FlushMode::Closed,
                current_time,
                split_timestamp,
                counter_expire_secs,
                zero_value_buckets,
            },
            next_bucket: 0,
            pending_removal: None,
            removed: 0,
            counts: PassCounts::default(),
        }
    }

    /// Starts a pass that removes every context: a drain, which emits every point held, or a discard, which emits
    /// nothing.
    ///
    /// Neither adds idle-counter zeros, and neither depends on the time.
    pub(super) fn start_drain(&self, mode: FlushMode) -> FlushCursor {
        debug_assert!(mode != FlushMode::Closed, "a drain removes every context");
        debug!(mode = ?mode, "Draining aggregation state.");
        FlushCursor {
            plan: FlushPlan {
                mode,
                current_time: self.last_flush,
                split_timestamp: u64::MAX,
                counter_expire_secs: 0,
                zero_value_buckets: SmallVec::new(),
            },
            next_bucket: 0,
            pending_removal: None,
            removed: 0,
            counts: PassCounts::default(),
        }
    }

    /// Queues the context whose removal the flush deferred, if any.
    fn queue_pending_removal(&mut self, cursor: &mut FlushCursor) {
        if let Some(index) = cursor.pending_removal.take() {
            if let Some((context, am)) = self.contexts.get_bucket(index) {
                self.telemetry.decrement_contexts(context, &am.values);
                self.contexts_remove_buf.push(context.clone());
            }
        }
    }

    /// Flushes the next context of a flush in progress, emitting the metrics of its closed buckets into `sink`, with
    /// histograms expanded by `hist_config`, except the records `filter` drops, which are counted instead.
    ///
    /// A drain or a discard removes the context it visits, and releases its units of `budget`, the budgets the state
    /// shares, if any. Returns `false`, without emitting anything, once every context has been visited.
    pub(super) fn flush_next<F>(
        &mut self, cursor: &mut FlushCursor, hist_config: &HistogramConfiguration, filter: Option<&dyn EmissionFilter>,
        budget: Option<&ContextBudget>, mut sink: F,
    ) -> bool
    where
        F: FnMut(Metric),
    {
        self.queue_pending_removal(cursor);

        let plan = &cursor.plan;
        let emission = Emission {
            bucket_width_secs: self.bucket_width_secs,
            hist_config,
            filter,
        };
        let num_buckets = self.contexts.num_buckets();
        while cursor.next_bucket < num_buckets {
            let index = cursor.next_bucket;
            cursor.next_bucket += 1;

            if plan.mode != FlushMode::Closed {
                let Ok(entry) = self.contexts.get_bucket_entry(index) else {
                    continue;
                };
                // A drain or a discard takes the context out of the table first, and gives back what it held, so that
                // nothing it emits can leave the state holding a context, or a budget unit, that it no longer counts.
                let ((context, mut am), _) = entry.remove();
                cursor.removed += 1;
                self.telemetry.decrement_contexts(&context, &am.values);
                if let Some(budget) = budget {
                    budget.release(1);
                }

                if plan.mode == FlushMode::All {
                    // A drain emits every timestamped point, whatever its bucket. Values are timestamped when they are
                    // inserted, except at bucket start 0, which no real clock reaches.
                    if let Some(values) = am.values.split_at_timestamp(plan.split_timestamp) {
                        self.telemetry.increment_flushed(&values);
                        let suppressed = emission.emit(&context, values, &am.metadata, &mut sink);
                        cursor.counts.suppressed.add(suppressed);
                    }
                } else if !am.values.is_empty() {
                    cursor.counts.discarded += 1;
                    cursor.counts.discarded_points += am.values.len();
                }
                return true;
            }

            let Some((context, am)) = self.contexts.get_bucket_mut(index) else {
                continue;
            };

            // Figure out if we should remove this metric or not if it has no values in open buckets.
            //
            // We have a special carve-out for counters here, which we have the ability to keep alive after they are
            // flushed, based on a configured expiration period. This allows us to continue emitting a zero value for
            // counters when they're idle, which can make them appear "live" in downstream systems, even when they're
            // not.
            //
            // This is useful for sparsely-updated counters.
            let should_expire_if_empty = match &am.values {
                MetricValues::Counter(..) => {
                    saluki_antithesis::always_le!(
                        am.last_seen,
                        u64::MAX - plan.counter_expire_secs,
                        "aggregate counter expiry add does not overflow",
                        { "last_seen": am.last_seen, "counter_expire_secs": plan.counter_expire_secs }
                    );
                    plan.counter_expire_secs != 0
                        && am.last_seen.saturating_add(plan.counter_expire_secs) < plan.current_time
                }
                _ => true,
            };

            // If we're dealing with a counter, we'll merge in our calculated set of zero values. We only merge in the
            // values that represent now-closed buckets.
            //
            // This is also safe to do even when there are real values in those buckets since adding zero to anything is
            // a no-op from the perspective of what we end up flushing, and it doesn't mess with the "last seen" time.
            if let MetricValues::Counter(..) = &mut am.values {
                let expires_at = am.last_seen.saturating_add(plan.counter_expire_secs);
                for (zv_bucket_start, zero_value) in &plan.zero_value_buckets {
                    if expires_at > *zv_bucket_start {
                        am.values.merge(zero_value.clone());
                    } else {
                        // Since zero-value buckets are in order, we can break early if this bucket is past the
                        // expiration cutoff of the counter. No other bucket will be within the expiration range.
                        break;
                    }
                }
            }

            // Finally, figure out if the current metric can be removed.
            //
            // For any metric with values that are in open buckets, we split off the values that are in closed buckets
            // and keep the metric alive. When all the values are in closed buckets, or there are no values, we'll
            // remove the metric if `should_remove_if_empty` is `true`.
            //
            // This means we'll always remove all-closed/empty non-counter metrics, and we _may_ remove all-closed/empty
            // counters.
            if let Some(closed_bucket_values) = am.values.split_at_timestamp(plan.split_timestamp) {
                self.telemetry.increment_flushed(&closed_bucket_values);

                // We got some closed bucket values, so flush those out.
                let suppressed = emission.emit(context, closed_bucket_values, &am.metadata, &mut sink);
                cursor.counts.suppressed.add(suppressed);
            }

            if am.values.is_empty() && should_expire_if_empty {
                cursor.pending_removal = Some(index);
            }
            return true;
        }
        false
    }

    /// Completes a flush: removes the contexts it emptied or expired, releasing their budget units, clears the breach
    /// flag if the state is below `limit`, and records the flush.
    ///
    /// A native flush is recorded as the last flush; after a drain or a discard, which removed every context as they
    /// visited it, the state is as if it had never flushed. The owner of `cursor` takes what the pass counted from it
    /// first.
    pub(super) fn finish_flush(&mut self, mut cursor: FlushCursor, limit: ContextLimit<'_>) {
        debug_assert!(
            cursor.next_bucket >= self.contexts.num_buckets(),
            "a flush finished before visiting every context"
        );
        self.queue_pending_removal(&mut cursor);

        // Remove any contexts that were marked as needing to be removed. A drain or a discard queues none of its own,
        // but may find the contexts an abandoned flush queued, which it has removed already.
        let queued = self.remove_queued();
        if let Some(budget) = limit.shared {
            budget.release(queued);
        }
        let contexts_delta = queued + cursor.removed;
        let contexts_len_after = self.contexts.len();

        let target_contexts_capacity = contexts_len_after.saturating_add(contexts_delta / 2);
        let hasher = &self.hasher;
        self.contexts
            .shrink_to(target_contexts_capacity, |(context, _)| hasher.hash_one(context));

        if self.context_limit_breached && limit.is_below(self.contexts.len()) {
            self.context_limit_breached = false;
        }
        if limit.shared.is_some() {
            // A state that admits by its own count is bounded only when it admits: a later call may pass a lower limit.
            self.check_bounds(&limit);
        }

        match cursor.plan.mode {
            FlushMode::Closed => self.last_flush = cursor.plan.current_time,
            FlushMode::All | FlushMode::Discard => self.forget_flushes(),
        }
    }

    /// Removes the contexts queued for removal, and returns how many it removed.
    ///
    /// A context can be queued twice, after an abandoned flush that the next flush visits again; it is removed once.
    fn remove_queued(&mut self) -> usize {
        let before = self.contexts.len();
        let hasher = &self.hasher;
        for context in self.contexts_remove_buf.drain(..) {
            let hash = hasher.hash_one(&context);
            if let Ok(entry) = self.contexts.find_entry(hash, |(existing, _)| *existing == context) {
                entry.remove();
            }
        }
        before - self.contexts.len()
    }

    /// Removes every context, as a state discarded by its owner: decrements the context gauges of the contexts not yet
    /// queued for removal, whose gauges were decremented when they were queued, and returns how many contexts it
    /// removed, which hold budget units.
    pub(super) fn release_all(&mut self) -> usize {
        let queued = self.remove_queued();
        for (context, aggregated) in &self.contexts {
            self.telemetry.decrement_contexts(context, &aggregated.values);
        }
        let held = self.contexts.len();
        self.contexts.clear();
        queued + held
    }
}

/// Returns `true` if merging `incoming` into `held` replaces `held` whole, as [`MetricValues::merge`] does: their kinds
/// differ, or both are rates over different intervals.
fn replaces(held: &MetricValues, incoming: &MetricValues) -> bool {
    match (held, incoming) {
        (MetricValues::Rate(_, held), MetricValues::Rate(_, incoming)) => held != incoming,
        (held, incoming) => std::mem::discriminant(held) != std::mem::discriminant(incoming),
    }
}

/// How a pass emits the values it takes from a context.
struct Emission<'a> {
    bucket_width_secs: NonZeroU64,
    hist_config: &'a HistogramConfiguration,
    filter: Option<&'a dyn EmissionFilter>,
}

impl Emission<'_> {
    /// Emits the records `values`, taken from `context`, flush into, or, if the filter drops every one of them, skips
    /// them and returns what it skipped.
    fn emit<F>(&self, context: &Context, values: MetricValues, metadata: &MetricMetadata, sink: &mut F) -> Suppressed
    where
        F: FnMut(Metric),
    {
        if let Some(filter) = self.filter {
            let suppressed = self.suppressed_by(filter, context, &values, metadata);
            if !suppressed.is_empty() {
                return suppressed;
            }
        }
        expand_flushed(
            context.clone(),
            values,
            metadata.clone(),
            self.bucket_width_secs,
            self.hist_config,
            sink,
        );
        Suppressed::default()
    }

    /// Returns the records `filter` drops of those `values` flush into, which are all of them or none.
    fn suppressed_by(
        &self, filter: &dyn EmissionFilter, context: &Context, values: &MetricValues, metadata: &MetricMetadata,
    ) -> Suppressed {
        // Every record a context's values flush into carries one point per timestamp held.
        let points = values.len() as u64;
        let (records, drops) = match OutputKind::of(values) {
            Some(kind) => (1, filter.drops(context, kind, metadata)),
            None => {
                let records =
                    self.hist_config.statistics().len() as u64 + u64::from(self.hist_config.copy_to_distribution());
                (records, records > 0 && filter.drops_histogram(context, metadata))
            }
        };
        if drops {
            Suppressed {
                records,
                points: records * points,
            }
        } else {
            Suppressed::default()
        }
    }
}

/// Emits the metrics one context's closed buckets flush into.
///
/// Counters leave as rates over the bucket width. Histograms expand: first into a distribution, when they are copied
/// to distributions, then into one series per statistic, in order. Every other kind leaves unchanged.
fn expand_flushed<F>(
    context: Context, mut values: MetricValues, metadata: MetricMetadata, bucket_width_secs: NonZeroU64,
    hist_config: &HistogramConfiguration, sink: &mut F,
) where
    F: FnMut(Metric),
{
    let bucket_width = Duration::from_secs(bucket_width_secs.get());

    match values {
        // If we're dealing with a histogram, we calculate a configured set of aggregates/percentiles from it, and emit
        // them as individual metrics.
        MetricValues::Histogram(ref mut points) => {
            // Convert histogram to distribution
            if hist_config.copy_to_distribution() {
                let sketch_points = points
                    .into_iter()
                    .map(|(ts, hist)| {
                        let mut sketch = DDSketch::default();
                        for sample in hist.samples() {
                            sketch.insert_n(sample.value.into_inner(), sample.weight.0 as u64);
                        }
                        (ts, sketch)
                    })
                    .collect::<SketchPoints>();
                let distribution_values = MetricValues::distribution(sketch_points);
                let metric_context = if !hist_config.copy_to_distribution_prefix().is_empty() {
                    context.with_name(format!(
                        "{}{}",
                        hist_config.copy_to_distribution_prefix(),
                        context.name()
                    ))
                } else {
                    context.clone()
                };
                let new_metric = Metric::from_parts(metric_context, distribution_values, metadata.clone());
                sink(new_metric);
            }
            // We collect our histogram points in their "summary" view, which sorts the underlying samples allowing
            // proper quantile queries to be answered, hence our "sorted" points. We do it this way because rather than
            // sort every time we insert, or cloning the points, we only sort when a summary view is constructed, which
            // requires mutable access to sort the samples in-place.
            let mut sorted_points = Vec::new();
            for (ts, h) in points {
                sorted_points.push((ts, h.summary_view()));
            }

            for statistic in hist_config.statistics() {
                let new_points = sorted_points
                    .iter()
                    .map(|(ts, hs)| (*ts, statistic.value_from_histogram(hs)))
                    .collect::<ScalarPoints>();

                let new_values = if statistic.is_rate_statistic() {
                    MetricValues::rate(new_points, bucket_width)
                } else {
                    MetricValues::gauge(new_points)
                };

                // Counts are dimensionless, so clear any unit inherited from the input histogram.
                let new_metadata = if matches!(statistic, HistogramStatistic::Count) {
                    metadata.clone().with_unit(MetaString::empty())
                } else {
                    metadata.clone()
                };

                let new_context = context.with_name(format!("{}.{}", context.name(), statistic.suffix()));
                let new_metric = Metric::from_parts(new_context, new_values, new_metadata);
                sink(new_metric);
            }
        }

        // If we're not dealing with a histogram, then all we need to worry about is converting counters to rates before
        // forwarding our single, aggregated metric.
        values => {
            let adjusted_values = counter_values_to_rate(values, bucket_width_secs);

            let metric = Metric::from_parts(context, adjusted_values, metadata);
            sink(metric);
        }
    }
}

fn counter_values_to_rate(values: MetricValues, interval_secs: NonZeroU64) -> MetricValues {
    match values {
        MetricValues::Counter(points) => MetricValues::rate(points, Duration::from_secs(interval_secs.get())),
        values => values,
    }
}

pub(super) const fn align_to_bucket_start(timestamp: u64, bucket_width_secs: NonZeroU64) -> u64 {
    timestamp - (timestamp % bucket_width_secs.get())
}

pub(super) const fn is_bucket_closed(
    current_time: u64, bucket_start: u64, bucket_width_secs: NonZeroU64, flush_open_buckets: bool,
) -> bool {
    // A bucket is considered "closed" if the current time is greater than the end of the bucket, or if
    // `flush_open_buckets` is `true`.
    //
    // Buckets represent a half-open interval, where the start is inclusive and the end is exclusive. This means that
    // for a bucket start of 10, and a width of 10, the bucket is 10 seconds "wide", and its start and end are 10 and
    // 20, with the 20 excluded, or [10, 20) in interval notation. Simply put, if we have a timestamp of 10, or anything
    // smaller than 20, we would consider it to fall within the bucket... but 20 or more would be outside of the bucket.
    //
    // We can also represent this visually:
    //
    // <--------- bucket 1 ----------> <--------- bucket 2 ----------> <--------- bucket 3 ---------->
    // [10 11 12 13 14 15 16 17 18 19] [20 21 22 23 24 25 26 27 28 29] [30 31 32 33 34 35 36 37 38 39]
    //
    // We can see that each bucket is 10 seconds wide (10 elements, one for each second), and that their ends are
    // effectively `start + width - 1`. This means that for any of these buckets to be considered "closed", the current
    // time has to be _greater_ than `start + width - 1`. For example, if the current time is 19, then no buckets are
    // closed, and if the current time is 29, then bucket 1 is closed but buckets 2 and 3 are still open, and if the
    // current time is 30, then both buckets 1 and 2 are closed, but bucket 3 is still open.
    (bucket_start + bucket_width_secs.get() - 1) < current_time || flush_open_buckets
}
