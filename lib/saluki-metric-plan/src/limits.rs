//! Resource and timing limits.
//!
//! [`LIMITS`] lists every limit that plans are validated or run against, with its default and where its value comes
//! from. Limits on what an artifact may contain, such as its size or node count, apply only to nodes with artifact
//! provenance, so that large native configurations stay valid.
//!
//! Configured limits, such as the aggregation context limit, are global: they mean the same at any degree of
//! parallelism. Scheduling limits, such as the batch size, are local to one execution unit.
//!
//! Code that enforces a limit reads its typed value from [`values`]; the table describes the same values.

use std::time::Duration;

/// The value of every limit, typed for the code that enforces it.
///
/// The [table][crate::limits::LIMITS] is built from these, so the two cannot disagree.
pub mod values {
    use std::time::Duration;

    /// The default number of contexts aggregation holds.
    pub const AGGREGATION_CONTEXT_LIMIT: u64 = 1_000_000;

    /// The minimum delay between preparing a cut and the cut taking effect.
    pub const CUT_MARGIN: Duration = Duration::from_secs(1);

    /// The maximum serialized size of an artifact, in bytes.
    pub const ARTIFACT_SIZE: u64 = 256 * 1024;

    /// The maximum number of nodes in an artifact.
    pub const ARTIFACT_NODES: u64 = 256;

    /// The maximum number of predicate leaves in an artifact.
    pub const PREDICATE_LEAVES: u64 = 1024;

    /// The maximum number of output ports of one copying node in an artifact.
    pub const TEE_FAN_OUT: u64 = 32;

    /// The maximum number of node visits one input record can cause.
    pub const WORK_PER_INPUT: u64 = 4096;

    /// The maximum memory an artifact's nodes declare, in bytes.
    pub const ARTIFACT_MEMORY: u64 = 64 * 1024 * 1024;

    /// The time allowed for every region to prepare a generation.
    pub const PREPARE_TIMEOUT: Duration = Duration::from_secs(10);

    /// The time a retiring generation's outputs have to carry out their closure behavior.
    pub const DRAIN_DEADLINE: Duration = Duration::from_secs(30);

    /// The maximum number of generations alive at once.
    pub const OVERLAP_BUDGET: u64 = 4;

    /// The maximum number of recipients one resources artifact declares.
    ///
    /// Decision log: new with live resources. A resources artifact adds at most this many recipient queues, each within
    /// its host's native bounds and admitted against the memory limit.
    pub const MAX_DECLARED_RECIPIENTS: u64 = 16;

    /// The maximum number of outputs one resources artifact declares.
    ///
    /// Decision log: new with live resources. Each declared output runs as one lease, so this bounds the leases a
    /// resources artifact adds, before overlap.
    pub const MAX_DECLARED_OUTPUTS: u64 = 16;

    /// The maximum serialized size of a resources artifact, in bytes.
    pub const RESOURCES_ARTIFACT_SIZE: u64 = 64 * 1024;

    /// The number of whole buffers queued for one builder shard of a lease in an encode host.
    ///
    /// Decision log: new with live resources. It is the native encoder's own events channel, so one output's lease
    /// holds what the native encoder held.
    pub const LEASE_QUEUE_BUFFERS: u64 = 8;

    /// The maximum number of recipients of one delivery host that drain at once.
    ///
    /// Decision log: new with live resources. One more drain closes the oldest draining recipient early with its
    /// closure, so a flapping configuration cannot grow drains without bound.
    pub const MAX_DRAINING_RECIPIENTS: u64 = 16;

    /// The longest an idle admission point waits before sending a heartbeat.
    ///
    /// Decision log: lowered from 5 seconds to 1 second when aggregation became a kernel. Heartbeats bound how long an
    /// idle source holds a state owner's watermark back, so the period bounds how late a flush tick closes buckets
    /// when nothing arrives; 1 second keeps that lag within the tick grace. On producer lanes a state lane's watermark
    /// is the minimum over several points, so one idle lane would hold back the ticks of busy ones by up to this
    /// period while data arrives: their points also heartbeat [`PROGRESS_PERIOD`] into each second they stamp nothing
    /// in, which bounds that lag by the progress throttle.
    pub const HEARTBEAT_PERIOD: Duration = Duration::from_secs(1);

    /// The backward wall-clock jump that resets an admission clock.
    ///
    /// Decision log: lowered from 600 seconds to 300 seconds, the specification's minimum (at least the largest bucket
    /// width), when aggregation became a kernel. A smaller backward jump is absorbed: admission times stay at the
    /// point's latest stamp until the wall clock catches up, so input over that time collapses into one bucket and
    /// nothing closes; the lower threshold halves the longest such freeze.
    pub const CLOCK_RESET_THRESHOLD: Duration = Duration::from_secs(300);

    /// The most admission time a region lets pass between the progress it forwards to a state owner for one admission
    /// point, when its input from that point leaves it without output.
    ///
    /// Decision log: new when aggregation became a kernel. It bounds how far a state owner's watermark lags behind
    /// traffic that a region filters out entirely, at about ten header-only batches per second per point.
    pub const PROGRESS_PERIOD: Duration = Duration::from_millis(100);

    /// The longest a flush tick waits for its state owner's watermark to reach the tick's wall-clock second.
    ///
    /// Decision log: new when aggregation became a kernel. A tick whose watermark lags the wall clock waits, while
    /// input raises the watermark, so that the flush closes the buckets native aggregation would close at that tick;
    /// once the grace ends it flushes at the watermark instead. Twice the heartbeat period, so that idle sources never
    /// delay a tick.
    pub const TICK_GRACE: Duration = Duration::from_secs(2);

    /// The most events a state owner emits in one step of a drain before it yields.
    pub const DRAIN_STEP_EVENTS: u64 = 1_024;

    /// The most state entries a state owner visits in one step of a drain before it yields.
    pub const DRAIN_STEP_CONTEXTS: u64 = 4_096;

    /// The period at which the installer polls the admission points while a cut is in flight.
    pub const CUT_POLL: Duration = Duration::from_millis(100);

    /// The longest the installer waits for the locks of the admission points a cut is armed or cancelled over.
    pub const CUT_LOCK_TIMEOUT: Duration = Duration::from_secs(1);

    /// The delay before the installer tries again to arm a cut it could not arm.
    pub const CUT_ARM_RETRY: Duration = Duration::from_secs(1);

    /// The most consecutive lock timeouts arming an artifact's cut before the artifact is rejected.
    ///
    /// Decision log: new when aggregation became a kernel. An artifact whose cut cannot be armed after this many
    /// attempts is rejected, and the active generation keeps running; a rollback keeps trying.
    pub const CUT_ARM_ATTEMPTS: u64 = 10;

    /// The largest least common multiple of the bucket widths a cut changes between.
    ///
    /// Decision log: new when aggregation became a kernel. A cut takes effect at a common boundary of the widths it
    /// changes between, so this bounds how long a width change waits; a change beyond it, such as from 120 to 180
    /// seconds, is rejected, and goes through an intermediate width of at most 60 seconds.
    pub const MAX_TRANSITION_LCM: Duration =
        Duration::from_secs(saluki_metric_kernels::params::MAX_TRANSITION_LCM_SECS);

    /// The maximum serialized size of one blob, in bytes.
    pub const BLOB_SIZE: u64 = 64 * 1024;

    /// The number of entries in each decision's memo.
    pub const DECISION_MEMO_ENTRIES: u64 = 4096;

    /// The maximum number of events in one batch: the capacity of an events buffer.
    pub const BATCH_SIZE: usize = 1024;

    /// The maximum number of batches a region processes between checks for control messages and timers.
    pub const WORK_PER_TURN: u64 = 8;

    /// The relative tolerance for comparing floating-point sums under reordering.
    pub const FLOAT_TOLERANCE: f64 = 1e-9;

    /// The number of key groups a keyed exit routes partition keys through to the lanes of a state owner.
    ///
    /// Decision log: new with partitioned execution. A key's group is its high byte, and each group belongs to one
    /// lane, in contiguous ranges fixed at boot; the indirection costs one table load per event and leaves room to move
    /// a group between lanes later without changing the key's group.
    pub const KEY_GROUPS: u64 = 256;

    /// The most lanes one slot runs on.
    ///
    /// Decision log: new with partitioned execution. Lanes are tasks with input channels, resource groups, and per-lane
    /// series, created at boot, so their number is bounded; an explicit count above it is capped.
    pub const MAX_LANES: u64 = 64;

    /// The most producer lanes, the lanes of the slot that admission points with lanes feed, that lane counts default
    /// to.
    ///
    /// Decision log: new with partitioned execution. Nothing above eight workers has been measured, so 16 is
    /// provisional until a gate host runs 16 workers; a gate run at 32 workers raises this to `MAX_LANES`.
    pub const PRODUCER_LANES_MAX: u64 = 16;

    /// The events a keyed exit's routed buffers hold in all, across the lanes it routes to: each routed buffer holds
    /// `clamp(ROUTE_BUFFER_EVENTS / lanes, 256, 1024)` events.
    ///
    /// Decision log: new with partitioned execution. It bounds the events a producer lane holds in open routed buffers
    /// at 2,048 at any automatic lane count, while keeping routed buffers large enough that per-buffer costs at the
    /// state lanes stay small.
    pub const ROUTE_BUFFER_EVENTS: u64 = 2_048;

    /// The longest a keyed exit holds an open routed buffer for more events before it sends it.
    ///
    /// Decision log: new with partitioned execution. Holding routed buffers across batches and idle input lets them
    /// fill at the decoders' rate; a buffer can therefore reach a state lane up to this much later relative to a tick's
    /// deadline, which only the state owner's flush output observes, at its ticks.
    pub const COALESCE_HOLD: Duration = Duration::from_millis(5);

    /// The most recent ticks of a lineage whose instants and flush times its lanes share.
    ///
    /// Decision log: new with partitioned execution. A lane more than this many ticks behind the others is a straggler:
    /// it uses its own flush time, and is counted.
    pub const TICK_RECORD_DEPTH: u64 = 4;

    /// The most earlier clock epochs a state lane keeps open for one admission point, waiting for every other point to
    /// switch, before it closes the oldest at once.
    ///
    /// Decision log: new with partitioned execution. Five resets before every point switched need a clock flapping by
    /// more than the reset threshold that often; closing the oldest bounds the instances a lineage holds.
    pub const MAX_OPEN_EPOCHS: u64 = 4;

    /// How much later within a second the lanes of a state owner tick, one after another: lane `j` ticks
    /// `j * stagger / lanes` after lane 0.
    ///
    /// Decision log: new with partitioned execution, off by default. Staggering spreads a tick's flush storm over the
    /// stagger, and widens the tick timing that equivalence excludes by as much.
    pub const LANE_FLUSH_STAGGER: Duration = Duration::ZERO;

    /// The most context budget units a state lane's batch takes in one block, for the new contexts of a state owner on
    /// several lanes: `saluki-core`'s `BudgetReservation::MAX_BLOCK`.
    ///
    /// Decision log: new with partitioned execution. Under churn, one bounded acquisition of each budget per new
    /// context took most of a state lane's time on two lanes or more (104 ns per new context per lane at two lanes, 851
    /// at eight, measured on a re-insertion burst of a million contexts); blocks of up to 64, which a lane starts at
    /// what its previous batch admitted, take one acquisition of each budget per 64 new contexts. Units a batch
    /// reserved and has not used, fewer than 64, count as held until it ends, so a new context of another lane can be
    /// refused for them, which is capacity behavior. A state owner on one lane takes one unit per block.
    pub const CONTEXT_RESERVATION_BLOCK: u64 = 64;
}

/// The value of a limit.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LimitValue {
    /// A number of items, such as nodes or entries.
    Count(u64),

    /// A number of bytes.
    Bytes(u64),

    /// A length of time.
    Duration(Duration),

    /// A set of allowed lengths of time.
    Durations(&'static [Duration]),

    /// A relative tolerance.
    Ratio(f64),
}

/// Where a limit's value comes from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LimitSource {
    /// The limit is fixed at its default.
    Fixed,

    /// The limit is a parameter of an operator, so each node sets it, and lowering binds it to native configuration in
    /// the default program. The table gives the native default.
    Parameter {
        /// The operator.
        operator: &'static str,

        /// The parameter.
        parameter: &'static str,
    },
}

/// One limit: its name, its default, and where its value comes from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LimitSpec {
    /// The limit's name.
    pub name: &'static str,

    /// The default value.
    pub default: LimitValue,

    /// Where the limit's value comes from.
    pub source: LimitSource,

    /// What the limit bounds, and what happens at the limit.
    pub description: &'static str,
}

/// The bucket widths that aggregation may use: every divisor of 60 seconds, and 120, 180, and 300 seconds.
///
/// These are the kernels crate's
/// [`ALLOWED_BUCKET_WIDTHS_SECS`][saluki_metric_kernels::params::ALLOWED_BUCKET_WIDTHS_SECS] as durations. One plan
/// uses at most one width above 60 seconds, so the least common multiple of a plan's widths is the larger of 60 seconds
/// and its largest width.
pub const ALLOWED_BUCKET_WIDTHS: &[Duration] = &BUCKET_WIDTH_DURATIONS;

const BUCKET_WIDTH_COUNT: usize = saluki_metric_kernels::params::ALLOWED_BUCKET_WIDTHS_SECS.len();

const BUCKET_WIDTH_DURATIONS: [Duration; BUCKET_WIDTH_COUNT] = {
    let secs = saluki_metric_kernels::params::ALLOWED_BUCKET_WIDTHS_SECS;
    let mut widths = [Duration::ZERO; BUCKET_WIDTH_COUNT];
    let mut index = 0;
    while index < BUCKET_WIDTH_COUNT {
        widths[index] = Duration::from_secs(secs[index]);
        index += 1;
    }
    widths
};

/// Returns `true` if `width` is an allowed bucket width: a whole number of seconds the kernels crate allows.
pub fn is_allowed_bucket_width(width: Duration) -> bool {
    width.subsec_nanos() == 0 && saluki_metric_kernels::params::is_allowed_bucket_width(width.as_secs())
}

/// The fewest events a keyed exit's routed buffer holds before it is sent, at any number of lanes.
const ROUTED_BUFFER_EVENTS_MIN: usize = 256;

/// Returns the number of events a keyed exit's routed buffer holds before it is sent, when the exit routes to `lanes`
/// lanes: [`ROUTE_BUFFER_EVENTS`][values::ROUTE_BUFFER_EVENTS] divided over the lanes, between 256 and
/// [`BATCH_SIZE`][values::BATCH_SIZE].
///
/// Every part that sizes something by routed buffers reads it here: the keyed exits that fill them, the bounds of the
/// open ones, and the capacity of the lane inputs they are sent to, whose consumers check every buffer they receive
/// against it.
pub fn routed_buffer_events(lanes: u16) -> usize {
    // The limit is a count of events, which every platform's memory addresses.
    let total = usize::try_from(values::ROUTE_BUFFER_EVENTS).unwrap_or(usize::MAX);
    (total / usize::from(lanes.max(1))).clamp(ROUTED_BUFFER_EVENTS_MIN, values::BATCH_SIZE)
}

/// The maximum number of contexts held in aggregation state.
pub const AGGREGATION_CONTEXT_LIMIT: LimitSpec = LimitSpec {
    name: "aggregation_context_limit",
    default: LimitValue::Count(values::AGGREGATION_CONTEXT_LIMIT),
    source: LimitSource::Parameter {
        operator: "aggregate",
        parameter: "context_limit",
    },
    description: "Maximum number of contexts held in aggregation state, including idle counters. When full, new \
                  contexts are dropped and counted as capacity loss until a flush brings the count below the limit. \
                  During a cut, both generations count against this one budget.",
};

/// The bucket widths aggregation may use.
///
/// Native configuration rejects any other aggregation window at startup.
///
/// # Missing
///
/// Validation does not enforce the restriction on plans yet: it will reject an `aggregate` or `timestamp_split` window
/// that is not an allowed width, a plan with more than one width above 60 seconds, and a `timestamp_split` whose
/// window differs from its aggregation's. Until then a plan's aggregation accepts any whole number of seconds.
pub const BUCKET_WIDTHS: LimitSpec = LimitSpec {
    name: "bucket_widths",
    default: LimitValue::Durations(ALLOWED_BUCKET_WIDTHS),
    source: LimitSource::Fixed,
    description:
        "Allowed aggregation bucket widths: every divisor of 60 seconds, and 120, 180, and 300 seconds. A plan \
                  uses at most one width above 60 seconds. Native configuration rejects any other aggregation window \
                  at startup.",
};

/// The minimum delay between preparing a cut and the cut taking effect.
pub const CUT_MARGIN: LimitSpec = LimitSpec {
    name: "cut_margin",
    default: LimitValue::Duration(values::CUT_MARGIN),
    source: LimitSource::Fixed,
    description: "Minimum time between the end of preparation and the cut time, which is the first common bucket \
                  boundary at least this far in the future.",
};

/// The maximum serialized size of an artifact, including its blobs.
pub const ARTIFACT_SIZE: LimitSpec = LimitSpec {
    name: "artifact_size",
    default: LimitValue::Bytes(values::ARTIFACT_SIZE),
    source: LimitSource::Fixed,
    description: "Maximum size of an artifact's JSON, including its blobs. Larger artifacts are rejected.",
};

/// The maximum number of nodes in an artifact.
pub const ARTIFACT_NODES: LimitSpec = LimitSpec {
    name: "artifact_nodes",
    default: LimitValue::Count(values::ARTIFACT_NODES),
    source: LimitSource::Fixed,
    description: "Maximum number of nodes in an artifact. Telemetry is reported per node, so this also bounds the \
                  telemetry cardinality an artifact author controls.",
};

/// The maximum number of predicate leaves in an artifact.
pub const PREDICATE_LEAVES: LimitSpec = LimitSpec {
    name: "predicate_leaves",
    default: LimitValue::Count(values::PREDICATE_LEAVES),
    source: LimitSource::Fixed,
    description: "Maximum number of predicate leaves, across all predicates of an artifact.",
};

/// The maximum number of output ports of one `tee`.
pub const TEE_FAN_OUT: LimitSpec = LimitSpec {
    name: "tee_fan_out",
    default: LimitValue::Count(values::TEE_FAN_OUT),
    source: LimitSource::Fixed,
    description: "Maximum number of output ports of one tee in an artifact.",
};

/// The maximum number of node visits one input can cause in the worst case.
pub const WORK_PER_INPUT: LimitSpec = LimitSpec {
    name: "work_per_input",
    default: LimitValue::Count(values::WORK_PER_INPUT),
    source: LimitSource::Fixed,
    description: "Maximum number of node visits that one input record can cause in the worst case, counting every \
                  path it can take and the declared expansion of every operator on them.",
};

/// The maximum memory an artifact's nodes declare in their memory bounds.
pub const ARTIFACT_MEMORY: LimitSpec = LimitSpec {
    name: "artifact_memory",
    default: LimitValue::Bytes(values::ARTIFACT_MEMORY),
    source: LimitSource::Fixed,
    description: "Maximum memory that an artifact's nodes declare in their memory bounds, what their kernels allocate \
                  up front and at most together, such as a mapper's string interner and caches. A kernel is built \
                  before the artifact runs, so an artifact that declares more is rejected. The state a state owner \
                  keeps in entries drawn from the host's state budget, such as aggregation's contexts, is not \
                  counted: the budget bounds it, and the host's bounds hold it at the budget's capacity.",
};

/// The time allowed for every region to prepare a new generation.
pub const PREPARE_TIMEOUT: LimitSpec = LimitSpec {
    name: "prepare_timeout",
    default: LimitValue::Duration(values::PREPARE_TIMEOUT),
    source: LimitSource::Fixed,
    description: "Time allowed for every region to acknowledge a prepared generation. On timeout the installation is \
                  aborted and the active generation keeps running.",
};

/// The time a retiring generation's outputs have to carry out their closure behavior.
pub const DRAIN_DEADLINE: LimitSpec = LimitSpec {
    name: "drain_deadline",
    default: LimitValue::Duration(values::DRAIN_DEADLINE),
    source: LimitSource::Fixed,
    description: "Time a retiring generation's outputs and recipients have to attempt their queued work before it is \
                  handled as at process shutdown. This changes behavior, not only tuning.",
};

/// The maximum number of generations alive at once.
pub const OVERLAP_BUDGET: LimitSpec = LimitSpec {
    name: "overlap_budget",
    default: LimitValue::Count(values::OVERLAP_BUDGET),
    source: LimitSource::Fixed,
    description: "Maximum number of generations alive at once: the active one, the last-known-good one, and two \
                  draining. A candidate beyond the budget waits, as pending, until a generation retires. This changes \
                  behavior, not only tuning.",
};

/// The maximum number of recipients one resources artifact declares.
pub const MAX_DECLARED_RECIPIENTS: LimitSpec = LimitSpec {
    name: "max_declared_recipients",
    default: LimitValue::Count(values::MAX_DECLARED_RECIPIENTS),
    source: LimitSource::Fixed,
    description: "Maximum number of recipients a resources artifact declares. A resources artifact with more is \
                  rejected.",
};

/// The maximum number of outputs one resources artifact declares.
pub const MAX_DECLARED_OUTPUTS: LimitSpec = LimitSpec {
    name: "max_declared_outputs",
    default: LimitValue::Count(values::MAX_DECLARED_OUTPUTS),
    source: LimitSource::Fixed,
    description: "Maximum number of outputs a resources artifact declares. A resources artifact with more is \
                  rejected.",
};

/// The maximum serialized size of a resources artifact.
pub const RESOURCES_ARTIFACT_SIZE: LimitSpec = LimitSpec {
    name: "resources_artifact_size",
    default: LimitValue::Bytes(values::RESOURCES_ARTIFACT_SIZE),
    source: LimitSource::Fixed,
    description: "Maximum size of a resources artifact's JSON. Larger resources artifacts are rejected.",
};

/// The number of whole buffers queued for one builder shard of a lease.
pub const LEASE_QUEUE_BUFFERS: LimitSpec = LimitSpec {
    name: "lease_queue_buffers",
    default: LimitValue::Count(values::LEASE_QUEUE_BUFFERS),
    source: LimitSource::Fixed,
    description: "Number of whole buffers queued for each builder of an output's encoder stream, beside the encode \
                  host's shared input. When one stream's queue is full, every stream of the host waits.",
};

/// The maximum number of recipients of one delivery host that drain at once.
pub const MAX_DRAINING_RECIPIENTS: LimitSpec = LimitSpec {
    name: "max_draining_recipients",
    default: LimitValue::Count(values::MAX_DRAINING_RECIPIENTS),
    source: LimitSource::Fixed,
    description: "Maximum number of retiring recipients of one delivery host that drain their queued work at once. \
                  One more closes the oldest draining recipient early, with its closure behavior.",
};

/// The longest an idle admission point waits before sending a heartbeat.
pub const HEARTBEAT_PERIOD: LimitSpec = LimitSpec {
    name: "heartbeat_period",
    default: LimitValue::Duration(values::HEARTBEAT_PERIOD),
    source: LimitSource::Fixed,
    description: "Longest an idle admission point waits before sending a heartbeat. The effective period is the \
                  smaller of this and the smallest flush interval of the state owners downstream, so idle sources \
                  never delay a flush.",
};

/// The backward wall-clock jump that resets an admission clock.
pub const CLOCK_RESET_THRESHOLD: LimitSpec = LimitSpec {
    name: "clock_reset_threshold",
    default: LimitValue::Duration(values::CLOCK_RESET_THRESHOLD),
    source: LimitSource::Fixed,
    description: "Backward wall-clock jump beyond which an admission clock resets: every state owner it feeds closes \
                  its open buckets, then the clock restarts from the wall clock. Smaller jumps are absorbed: admission \
                  times stay at the latest stamp until the wall clock catches up. At least the largest bucket width. \
                  This changes behavior, not only tuning.",
};

/// The most admission time a region lets pass between the progress it forwards to a state owner for one point.
pub const PROGRESS_PERIOD: LimitSpec = LimitSpec {
    name: "progress_period",
    default: LimitValue::Duration(values::PROGRESS_PERIOD),
    source: LimitSource::Fixed,
    description: "Most admission time a region lets pass, per admission point, before it forwards a header-only \
                  progress batch to a state owner downstream when its input from that point leaves no output, so \
                  that filtered traffic still advances the state owner's watermark.",
};

/// The longest a flush tick waits for its state owner's watermark to reach the tick's wall-clock second.
pub const TICK_GRACE: LimitSpec = LimitSpec {
    name: "tick_grace",
    default: LimitValue::Duration(values::TICK_GRACE),
    source: LimitSource::Fixed,
    description: "Longest a flush tick waits for its state owner's watermark to reach the tick's wall-clock second, \
                  so that the flush closes the buckets native aggregation closes at that tick; afterwards it flushes \
                  at the watermark. Twice the heartbeat period. This changes behavior, not only tuning.",
};

/// The most events a state owner emits in one step of a drain.
pub const DRAIN_STEP_EVENTS: LimitSpec = LimitSpec {
    name: "drain_step_events",
    default: LimitValue::Count(values::DRAIN_STEP_EVENTS),
    source: LimitSource::Fixed,
    description: "Most events a state owner emits in one step of a flush or drain before it yields, so that a large \
                  flush never holds its region or its memory for long. One state entry's records are never split.",
};

/// The most state entries a state owner visits in one step of a drain.
pub const DRAIN_STEP_CONTEXTS: LimitSpec = LimitSpec {
    name: "drain_step_contexts",
    default: LimitValue::Count(values::DRAIN_STEP_CONTEXTS),
    source: LimitSource::Fixed,
    description: "Most state entries, such as aggregation contexts, a state owner visits in one step of a flush or \
                  drain before it yields.",
};

/// The period at which the installer polls the admission points while a cut is in flight.
pub const CUT_POLL: LimitSpec = LimitSpec {
    name: "cut_poll",
    default: LimitValue::Duration(values::CUT_POLL),
    source: LimitSource::Fixed,
    description: "Period at which the installer polls every admission point while a cut is in flight, to learn when \
                  the cut becomes irrevocable and when every affected point has passed it.",
};

/// The longest the installer waits for the locks of the admission points a cut is armed or cancelled over.
pub const CUT_LOCK_TIMEOUT: LimitSpec = LimitSpec {
    name: "cut_lock_timeout",
    default: LimitValue::Duration(values::CUT_LOCK_TIMEOUT),
    source: LimitSource::Fixed,
    description: "Longest the installer waits for the locks of the admission points it arms or cancels a cut over. \
                  Arming that times out is tried again; cancelling that times out is refused, and the cut proceeds.",
};

/// The delay before the installer tries again to arm a cut it could not arm.
pub const CUT_ARM_RETRY: LimitSpec = LimitSpec {
    name: "cut_arm_retry",
    default: LimitValue::Duration(values::CUT_ARM_RETRY),
    source: LimitSource::Fixed,
    description: "Delay before the installer tries again to arm a cut whose locks timed out, or whose admission \
                  clocks run ahead of the wall clock.",
};

/// The most consecutive lock timeouts arming an artifact's cut before the artifact is rejected.
pub const CUT_ARM_ATTEMPTS: LimitSpec = LimitSpec {
    name: "cut_arm_attempts",
    default: LimitValue::Count(values::CUT_ARM_ATTEMPTS),
    source: LimitSource::Fixed,
    description: "Most consecutive lock timeouts arming an artifact's cut before the artifact is rejected and the \
                  active generation keeps running; a rollback keeps trying. This changes behavior, not only tuning.",
};

/// The largest least common multiple of the bucket widths a cut changes between.
pub const MAX_TRANSITION_LCM: LimitSpec = LimitSpec {
    name: "max_transition_lcm",
    default: LimitValue::Duration(values::MAX_TRANSITION_LCM),
    source: LimitSource::Fixed,
    description: "Largest least common multiple of the bucket widths one cut changes between, which bounds how long a \
                  width change waits for a common boundary. A larger change is rejected; it goes through an \
                  intermediate width of at most 60 seconds. This changes behavior, not only tuning.",
};

/// The maximum size of one blob.
pub const BLOB_SIZE: LimitSpec = LimitSpec {
    name: "blob_size",
    default: LimitValue::Bytes(values::BLOB_SIZE),
    source: LimitSource::Fixed,
    description: "Maximum size of one blob's JSON. Blobs also count toward the artifact size limit.",
};

/// The number of entries in each decision's memo.
pub const DECISION_MEMO_ENTRIES: LimitSpec = LimitSpec {
    name: "decision_memo_entries",
    default: LimitValue::Count(values::DECISION_MEMO_ENTRIES),
    source: LimitSource::Fixed,
    description: "Number of entries in the memo of each routing decision, keyed by context and kind. The memo has a \
                  fixed capacity and evicts without allocating.",
};

/// The maximum number of events in one batch.
pub const BATCH_SIZE: LimitSpec = LimitSpec {
    name: "batch_size",
    default: LimitValue::Count(values::BATCH_SIZE as u64),
    source: LimitSource::Fixed,
    description: "Maximum number of events in one batch, which is the unit of stateless work.",
};

/// The maximum number of batches a region processes between checks for control messages and timers.
pub const WORK_PER_TURN: LimitSpec = LimitSpec {
    name: "work_per_turn",
    default: LimitValue::Count(values::WORK_PER_TURN),
    source: LimitSource::Fixed,
    description: "Maximum number of batches a region processes before checking for control messages and timers, so \
                  that heavy input cannot starve them.",
};

/// The relative tolerance for comparing floating-point sums under reordering.
pub const FLOAT_TOLERANCE: LimitSpec = LimitSpec {
    name: "float_tolerance",
    default: LimitValue::Ratio(values::FLOAT_TOLERANCE),
    source: LimitSource::Fixed,
    description: "Relative tolerance when checking equivalence of floating-point sums whose inputs may be reordered, \
                  as under partitioned execution.",
};

/// The number of key groups a keyed exit routes partition keys through.
pub const KEY_GROUPS: LimitSpec = LimitSpec {
    name: "key_groups",
    default: LimitValue::Count(values::KEY_GROUPS),
    source: LimitSource::Fixed,
    description: "Number of key groups a state owner's partition keys are routed through to its lanes: a key's group \
                  is its high byte, and each group belongs to one lane for the life of the process.",
};

/// The most lanes one slot runs on.
pub const MAX_LANES: LimitSpec = LimitSpec {
    name: "max_lanes",
    default: LimitValue::Count(values::MAX_LANES),
    source: LimitSource::Fixed,
    description: "Maximum number of lanes one slot runs on. A configured lane count above it is capped.",
};

/// The most producer lanes the lane counts default to.
pub const PRODUCER_LANES_MAX: LimitSpec = LimitSpec {
    name: "producer_lanes_max",
    default: LimitValue::Count(values::PRODUCER_LANES_MAX),
    source: LimitSource::Fixed,
    description: "Maximum number of producer lanes, one per decoder, that the lane counts default to. More decoders \
                  share lanes.",
};

/// The events a keyed exit's routed buffers hold in all, across its target lanes.
pub const ROUTE_BUFFER_EVENTS: LimitSpec = LimitSpec {
    name: "route_buffer_events",
    default: LimitValue::Count(values::ROUTE_BUFFER_EVENTS),
    source: LimitSource::Fixed,
    description: "Events a keyed exit's open routed buffers hold in all: each target lane's buffer holds this many \
                  divided by the lanes, between 256 and 1,024 events.",
};

/// The longest a keyed exit holds an open routed buffer.
pub const COALESCE_HOLD: LimitSpec = LimitSpec {
    name: "coalesce_hold",
    default: LimitValue::Duration(values::COALESCE_HOLD),
    source: LimitSource::Fixed,
    description: "Longest a keyed exit holds an open routed buffer for more events before it sends it to its state \
                  lane.",
};

/// The most recent ticks of a lineage whose instants and flush times its lanes share.
pub const TICK_RECORD_DEPTH: LimitSpec = LimitSpec {
    name: "tick_record_depth",
    default: LimitValue::Count(values::TICK_RECORD_DEPTH),
    source: LimitSource::Fixed,
    description: "Number of recent ticks of a lineage whose instants and flush times its lanes share; a lane further \
                  behind uses its own flush time, and is counted.",
};

/// The most earlier clock epochs a state lane keeps open for one admission point.
pub const MAX_OPEN_EPOCHS: LimitSpec = LimitSpec {
    name: "max_open_epochs",
    default: LimitValue::Count(values::MAX_OPEN_EPOCHS),
    source: LimitSource::Fixed,
    description: "Maximum number of earlier clock epochs a state lane keeps open for one admission point, waiting for \
                  every other point to switch, before it closes the oldest at once.",
};

/// How much later within a second the lanes of a state owner tick, one after another.
pub const LANE_FLUSH_STAGGER: LimitSpec = LimitSpec {
    name: "lane_flush_stagger",
    default: LimitValue::Duration(values::LANE_FLUSH_STAGGER),
    source: LimitSource::Fixed,
    description: "How much later within a second the lanes of a state owner tick, one after another; zero ticks every \
                  lane together.",
};

/// The most context budget units a state lane's batch takes in one block.
pub const CONTEXT_RESERVATION_BLOCK: LimitSpec = LimitSpec {
    name: "context_reservation_block",
    default: LimitValue::Count(values::CONTEXT_RESERVATION_BLOCK),
    source: LimitSource::Fixed,
    description: "Most context budget units a state lane's batch takes at once for its new contexts, when its state \
                  owner runs on several lanes; units it has not used count as held until the batch ends, and can \
                  refuse another lane's new context.",
};

/// Every limit.
pub const LIMITS: &[LimitSpec] = &[
    AGGREGATION_CONTEXT_LIMIT,
    BUCKET_WIDTHS,
    CUT_MARGIN,
    ARTIFACT_SIZE,
    ARTIFACT_NODES,
    PREDICATE_LEAVES,
    TEE_FAN_OUT,
    WORK_PER_INPUT,
    ARTIFACT_MEMORY,
    PREPARE_TIMEOUT,
    DRAIN_DEADLINE,
    OVERLAP_BUDGET,
    MAX_DECLARED_RECIPIENTS,
    MAX_DECLARED_OUTPUTS,
    RESOURCES_ARTIFACT_SIZE,
    LEASE_QUEUE_BUFFERS,
    MAX_DRAINING_RECIPIENTS,
    HEARTBEAT_PERIOD,
    CLOCK_RESET_THRESHOLD,
    PROGRESS_PERIOD,
    TICK_GRACE,
    DRAIN_STEP_EVENTS,
    DRAIN_STEP_CONTEXTS,
    CUT_POLL,
    CUT_LOCK_TIMEOUT,
    CUT_ARM_RETRY,
    CUT_ARM_ATTEMPTS,
    MAX_TRANSITION_LCM,
    BLOB_SIZE,
    DECISION_MEMO_ENTRIES,
    BATCH_SIZE,
    WORK_PER_TURN,
    FLOAT_TOLERANCE,
    KEY_GROUPS,
    MAX_LANES,
    PRODUCER_LANES_MAX,
    ROUTE_BUFFER_EVENTS,
    COALESCE_HOLD,
    TICK_RECORD_DEPTH,
    MAX_OPEN_EPOCHS,
    LANE_FLUSH_STAGGER,
    CONTEXT_RESERVATION_BLOCK,
];

/// Returns the limit with the given name, if there is one.
pub fn limit(name: &str) -> Option<&'static LimitSpec> {
    LIMITS.iter().find(|spec| spec.name == name)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use saluki_core::topology::EventsBuffer;

    use super::*;

    fn duration(spec: &LimitSpec) -> Duration {
        match spec.default {
            LimitValue::Duration(duration) => duration,
            other => panic!("{} is not a duration: {:?}", spec.name, other),
        }
    }

    fn count(spec: &LimitSpec) -> u64 {
        match spec.default {
            LimitValue::Count(count) | LimitValue::Bytes(count) => count,
            other => panic!("{} is not a count: {:?}", spec.name, other),
        }
    }

    #[test]
    fn limits_are_named_uniquely_and_described() {
        let names = LIMITS.iter().map(|spec| spec.name).collect::<HashSet<_>>();
        assert_eq!(names.len(), LIMITS.len(), "limit names are unique");
        for spec in LIMITS {
            assert!(crate::ir::is_identifier(spec.name), "{} is an identifier", spec.name);
            assert!(
                spec.description.ends_with('.'),
                "{} has a complete description",
                spec.name
            );
            assert_eq!(limit(spec.name), Some(spec));
        }
        assert_eq!(limit("no_such_limit"), None);
    }

    #[test]
    fn known_values_match_the_specification() {
        assert_eq!(count(&AGGREGATION_CONTEXT_LIMIT), 1_000_000);
        assert_eq!(duration(&CUT_MARGIN), Duration::from_secs(1));
        assert_eq!(count(&ARTIFACT_SIZE), 262_144);
        assert_eq!(count(&ARTIFACT_NODES), 256);
        assert_eq!(count(&PREDICATE_LEAVES), 1024);
        assert_eq!(count(&TEE_FAN_OUT), 32);
        assert_eq!(count(&WORK_PER_INPUT), 4096);
        assert_eq!(count(&ARTIFACT_MEMORY), 67_108_864);
        assert_eq!(duration(&PREPARE_TIMEOUT), Duration::from_secs(10));
        assert_eq!(duration(&DRAIN_DEADLINE), Duration::from_secs(30));
        assert_eq!(count(&OVERLAP_BUDGET), 4);
        assert_eq!(count(&MAX_DECLARED_RECIPIENTS), 16);
        assert_eq!(count(&MAX_DECLARED_OUTPUTS), 16);
        assert_eq!(count(&RESOURCES_ARTIFACT_SIZE), 65_536);
        assert_eq!(count(&LEASE_QUEUE_BUFFERS), 8);
        assert_eq!(count(&MAX_DRAINING_RECIPIENTS), 16);
        assert_eq!(count(&BLOB_SIZE), 65_536);
        assert_eq!(count(&DECISION_MEMO_ENTRIES), 4096);
        assert_eq!(count(&WORK_PER_TURN), 8);
        assert_eq!(FLOAT_TOLERANCE.default, LimitValue::Ratio(1e-9));
        assert_eq!(duration(&HEARTBEAT_PERIOD), Duration::from_secs(1));
        assert_eq!(duration(&CLOCK_RESET_THRESHOLD), Duration::from_secs(300));
        assert_eq!(duration(&PROGRESS_PERIOD), Duration::from_millis(100));
        assert_eq!(duration(&TICK_GRACE), Duration::from_secs(2));
        assert_eq!(count(&DRAIN_STEP_EVENTS), 1024);
        assert_eq!(count(&DRAIN_STEP_CONTEXTS), 4096);
        assert_eq!(duration(&CUT_POLL), Duration::from_millis(100));
        assert_eq!(duration(&CUT_LOCK_TIMEOUT), Duration::from_secs(1));
        assert_eq!(duration(&CUT_ARM_RETRY), Duration::from_secs(1));
        assert_eq!(count(&CUT_ARM_ATTEMPTS), 10);
        assert_eq!(duration(&MAX_TRANSITION_LCM), Duration::from_secs(300));
        assert_eq!(count(&KEY_GROUPS), 256);
        assert_eq!(count(&MAX_LANES), 64);
        assert_eq!(count(&PRODUCER_LANES_MAX), 16);
        assert_eq!(count(&ROUTE_BUFFER_EVENTS), 2_048);
        assert_eq!(duration(&COALESCE_HOLD), Duration::from_millis(5));
        assert_eq!(count(&TICK_RECORD_DEPTH), 4);
        assert_eq!(count(&MAX_OPEN_EPOCHS), 4);
        assert_eq!(duration(&LANE_FLUSH_STAGGER), Duration::ZERO);
        assert_eq!(count(&CONTEXT_RESERVATION_BLOCK), 64);
    }

    #[test]
    fn only_the_context_limit_comes_from_an_operator_parameter() {
        for spec in LIMITS {
            let expected = if spec.name == AGGREGATION_CONTEXT_LIMIT.name {
                LimitSource::Parameter {
                    operator: "aggregate",
                    parameter: "context_limit",
                }
            } else {
                LimitSource::Fixed
            };
            assert_eq!(spec.source, expected, "{}", spec.name);
        }
        // The parameter exists.
        let aggregate = crate::operators::OperatorRegistry::native().get("aggregate").unwrap();
        assert!(aggregate.params().iter().any(|param| param.name == "context_limit"));
    }

    #[test]
    fn typed_values_are_the_tables_defaults() {
        assert_eq!(count(&BATCH_SIZE), values::BATCH_SIZE as u64);
        assert_eq!(values::BATCH_SIZE, EventsBuffer::default().capacity());
        assert_eq!(count(&WORK_PER_INPUT), values::WORK_PER_INPUT);
        assert_eq!(duration(&PREPARE_TIMEOUT), values::PREPARE_TIMEOUT);
        assert_eq!(FLOAT_TOLERANCE.default, LimitValue::Ratio(values::FLOAT_TOLERANCE));
        // The reservation block is enforced in `saluki-core`, which knows nothing of plans.
        assert_eq!(
            values::CONTEXT_RESERVATION_BLOCK,
            saluki_core::execution::BudgetReservation::MAX_BLOCK
        );
        assert_eq!(count(&CONTEXT_RESERVATION_BLOCK), values::CONTEXT_RESERVATION_BLOCK);
    }

    #[test]
    fn limits_respect_the_relations_the_specification_requires() {
        // The clock-reset threshold is at least the largest bucket width.
        let largest_width = ALLOWED_BUCKET_WIDTHS.iter().max().copied().unwrap();
        assert!(duration(&CLOCK_RESET_THRESHOLD) >= largest_width);

        // The heartbeat period is at most the native flush interval of 15 seconds.
        assert!(duration(&HEARTBEAT_PERIOD) <= Duration::from_secs(15));

        // The tick grace is twice the heartbeat period, so that an idle source never delays a flush tick.
        assert_eq!(duration(&TICK_GRACE), duration(&HEARTBEAT_PERIOD) * 2);

        // Progress is forwarded more often than heartbeats arrive.
        assert!(duration(&PROGRESS_PERIOD) < duration(&HEARTBEAT_PERIOD));

        // A cut waits at most the largest bucket width for its boundary, which the reset threshold covers.
        assert_eq!(duration(&MAX_TRANSITION_LCM), largest_width);
        assert!(duration(&CLOCK_RESET_THRESHOLD) >= duration(&MAX_TRANSITION_LCM));

        // One drain step emits at most one batch, and visits more contexts than it emits records.
        assert_eq!(count(&DRAIN_STEP_EVENTS), count(&BATCH_SIZE));
        assert!(count(&DRAIN_STEP_CONTEXTS) >= count(&DRAIN_STEP_EVENTS));

        // A blob fits in an artifact.
        assert!(count(&BLOB_SIZE) <= count(&ARTIFACT_SIZE));

        // The batch size is the capacity of the native event buffer.
        assert_eq!(count(&BATCH_SIZE), EventsBuffer::default().capacity() as u64);
    }

    #[test]
    fn bucket_widths_are_divisors_of_a_minute_or_a_few_longer_widths() {
        for secs in 1..=600 {
            let expected = (60 % secs == 0) || [120, 180, 300].contains(&secs);
            assert_eq!(
                is_allowed_bucket_width(Duration::from_secs(secs)),
                expected,
                "{} seconds",
                secs
            );
        }
        assert!(!is_allowed_bucket_width(Duration::from_millis(1500)));
        assert!(!is_allowed_bucket_width(Duration::from_millis(60_001)));
        assert!(!is_allowed_bucket_width(Duration::ZERO));
        assert_eq!(BUCKET_WIDTHS.default, LimitValue::Durations(ALLOWED_BUCKET_WIDTHS));

        // The table's widths are the kernels crate's, in its order.
        let secs = ALLOWED_BUCKET_WIDTHS.iter().map(Duration::as_secs).collect::<Vec<_>>();
        assert_eq!(secs, saluki_metric_kernels::params::ALLOWED_BUCKET_WIDTHS_SECS);
    }

    #[test]
    fn routed_buffers_divide_the_route_buffer_events_over_the_lanes_between_256_and_a_full_batch() {
        // 2,048 events over the lanes: whole batches at one or two lanes, halves at four, the floor from eight on.
        for (lanes, events) in [
            (0, 1_024),
            (1, 1_024),
            (2, 1_024),
            (3, 682),
            (4, 512),
            (8, 256),
            (64, 256),
        ] {
            assert_eq!(routed_buffer_events(lanes), events, "{} lanes", lanes);
        }
        // A routed buffer never holds more than an events buffer does.
        assert!(routed_buffer_events(1) <= EventsBuffer::default().capacity());
    }
}
