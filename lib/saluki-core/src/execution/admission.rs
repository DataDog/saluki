//! Admission points and schedules.

use std::{
    collections::{btree_map::Entry, BTreeMap},
    fmt,
    sync::{Arc, Mutex as StdMutex, MutexGuard as StdMutexGuard, OnceLock, PoisonError, Weak},
    time::Duration,
};

use arc_swap::ArcSwap;
use saluki_error::{generic_error, GenericError};
use smallvec::SmallVec;
use tokio::{
    sync::{mpsc, Mutex, MutexGuard, OwnedMutexGuard},
    time::Instant,
};
use tracing::warn;

use super::{
    clock::{AdmissionClock, ClockStamp, SystemWallClock, WallClock},
    generation::{GenerationId, GenerationRef},
    header::{AdmissionPointId, AdmissionTime, BatchHeader},
};
use crate::topology::{ComponentId, EventsBuffer};

/// The schedule that assigns generations to newly admitted batches.
///
/// Every admission point reads one shared schedule each time it stamps a batch, and stamps the generation the schedule
/// chooses for that batch's admission time. Installing a generation is one store into the schedule's
/// [handle][ScheduleHandle]; batches admitted earlier keep the generation they were stamped with.
#[derive(Clone, Debug)]
pub enum AdmissionSchedule {
    /// Every batch gets this generation.
    Steady(GenerationRef),

    /// Ownership moves from `old` to `new` at the admission time `at`.
    ///
    /// A batch gets `new` if its admission time is at least `at`, or if its admission point's clock was reset after
    /// the point first saw a cut to `new`, since a reset before the cut counts as reaching it. Every other batch gets
    /// `old`. The choice depends only on the stamp, so queueing delays cannot change it.
    ///
    /// A point remembers the epoch at which it first saw a cut by the identifier of the cut's `new` generation, not by
    /// the schedule value: publishing another cut to the same `new` generation, even one with a different `at`, keeps
    /// what the point recorded, so a point that a clock reset moved to `new` stays there.
    Cut {
        /// The generation that owns batches admitted before the cut.
        old: GenerationRef,

        /// The generation that owns batches admitted at or after the cut.
        new: GenerationRef,

        /// The admission time of the cut.
        at: AdmissionTime,
    },
}

impl AdmissionSchedule {
    /// Returns the generation this schedule assigns to a batch.
    ///
    /// `admitted_at` and `epoch` are the batch's stamp. `epoch_at_publish` is the epoch its admission point's clock had
    /// when the point first saw a cut to this schedule's `new` generation: a different epoch means the clock was reset
    /// since then. It is ignored for a steady schedule.
    pub fn generation_for(&self, admitted_at: AdmissionTime, epoch: u32, epoch_at_publish: u32) -> &GenerationRef {
        match self {
            Self::Steady(generation) => generation,
            Self::Cut { old, new, at } => {
                // Epochs only move forward, and wrap, so any difference means the clock was reset after publication.
                if admitted_at >= *at || epoch != epoch_at_publish {
                    new
                } else {
                    old
                }
            }
        }
    }

    /// Wraps this schedule in a new shared handle.
    pub fn into_handle(self) -> ScheduleHandle {
        Arc::new(ArcSwap::from_pointee(self))
    }
}

/// A shared, atomically replaceable admission schedule.
///
/// One handle serves every admission point of a topology. Publishing a new schedule is one `store` into the handle:
/// each admission point reads the new schedule the next time it stamps a batch.
pub type ScheduleHandle = Arc<ArcSwap<AdmissionSchedule>>;

/// The shortest heartbeat period a topology accepts for an admission point: 1 millisecond.
pub const MIN_HEARTBEAT_PERIOD: Duration = Duration::from_millis(1);

/// The longest heartbeat period a topology accepts for an admission point: 1 hour.
pub const MAX_HEARTBEAT_PERIOD: Duration = Duration::from_secs(3_600);

/// How an admission point treats the headers of the batches it sends.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AdmissionMode {
    /// Stamps batches that are not stamped yet, and sends already stamped batches as they are.
    ///
    /// This is the mode of a source: the batches it creates start unstamped.
    Admit,

    /// Stamps every batch, overwriting any admission time, admission point, epoch, and generation, and clearing the
    /// entry.
    ///
    /// This is the mode of a component whose processing the plan cannot see into: nothing stamped upstream of it
    /// crosses it, so the parts of the plan on either side of it switch generations independently.
    Readmit,
}

/// The configuration of an admission point.
#[derive(Clone, Debug)]
pub struct AdmissionPointConfig {
    id: AdmissionPointId,
    mode: AdmissionMode,
    schedule: ScheduleHandle,
    clock_reset_threshold: Duration,
    heartbeat: Option<Duration>,
    heartbeat_recipients: Vec<ComponentId>,
    wall_clock: Arc<dyn WallClock>,
    registry: Option<AdmissionRegistry>,
}

impl AdmissionPointConfig {
    /// Creates the configuration of an admission point that reads the operating system's wall clock and sends no
    /// periodic heartbeats.
    ///
    /// `id` must be unique within a topology. `schedule` chooses the generation of every batch the point stamps.
    /// `clock_reset_threshold` is the largest backward wall-clock jump the point's clock absorbs; a larger jump resets
    /// the clock (see [`AdmissionClock`]). It should be at least the largest bucket width of any state the point feeds.
    pub fn new(
        id: AdmissionPointId, mode: AdmissionMode, schedule: ScheduleHandle, clock_reset_threshold: Duration,
    ) -> Self {
        Self {
            id,
            mode,
            schedule,
            clock_reset_threshold,
            heartbeat: None,
            heartbeat_recipients: Vec::new(),
            wall_clock: Arc::new(SystemWallClock),
            registry: None,
        }
    }

    /// Enables periodic heartbeats: whenever the point has stamped nothing for `period`, it sends a heartbeat.
    ///
    /// Heartbeats are disabled by default. A topology rejects a period shorter than [`MIN_HEARTBEAT_PERIOD`] (1 ms)
    /// or longer than [`MAX_HEARTBEAT_PERIOD`] (1 hour). A period no longer than the shortest flush interval of any
    /// state the point feeds keeps idle points from delaying flushes.
    pub fn with_heartbeat(mut self, period: Duration) -> Self {
        self.heartbeat = Some(period);
        self
    }

    /// Sends the point's heartbeats only to the given components.
    ///
    /// Every component listed must be fed directly by the point's output: a topology whose output does not feed a
    /// listed component fails to build. Data is sent to every component the output feeds either way; only heartbeats
    /// are restricted, so components that keep no time-bucketed state downstream of the point, such as taps that
    /// observe its traffic, receive no header-only batches. An empty list, the default, sends heartbeats to every
    /// component the output feeds.
    pub fn with_heartbeat_recipients<I>(mut self, components: I) -> Self
    where
        I: IntoIterator<Item = ComponentId>,
    {
        self.heartbeat_recipients = components.into_iter().collect();
        self
    }

    /// Makes the point's clock read the given wall clock instead of the operating system's.
    pub fn with_wall_clock<W>(mut self, wall_clock: W) -> Self
    where
        W: WallClock + 'static,
    {
        self.wall_clock = Arc::new(wall_clock);
        self
    }

    /// Registers the point, once a topology builds it, in `registry`, through which an installer reads and locks
    /// several points at once.
    ///
    /// A topology fails to start if another live point of the registry already has the point's identifier.
    pub fn with_registry(mut self, registry: AdmissionRegistry) -> Self {
        self.registry = Some(registry);
        self
    }

    /// Returns the identifier of the admission point.
    pub fn id(&self) -> AdmissionPointId {
        self.id
    }

    /// Returns the admission point's mode.
    pub fn mode(&self) -> AdmissionMode {
        self.mode
    }

    /// Returns the heartbeat period, if periodic heartbeats are enabled.
    pub fn heartbeat(&self) -> Option<Duration> {
        self.heartbeat
    }

    /// Returns the components the point sends heartbeats to; empty means every component its output feeds.
    pub fn heartbeat_recipients(&self) -> &[ComponentId] {
        &self.heartbeat_recipients
    }

    /// Returns the registry the point registers in, if any.
    pub fn registry(&self) -> Option<&AdmissionRegistry> {
        self.registry.as_ref()
    }
}

/// The result of sending a heartbeat.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use]
pub enum HeartbeatOutcome {
    /// The heartbeat reached at least one downstream channel.
    Sent,

    /// The heartbeat reached no downstream channel, because the output has none that receives heartbeats, the
    /// component that owns the output has stopped, or every downstream component that receives heartbeats has stopped.
    ///
    /// In the first two cases nothing was stamped. Other components may still send to the same downstream channels,
    /// since several outputs can feed one component: a point stops once its own output's component stops.
    Closed,
}

/// What an admission point has stamped, as an installer needs to know it to schedule and check a cut.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdmissionPointStatus {
    /// The admission time of the point's latest stamp, of data or of a heartbeat, or `None` if it has stamped
    /// nothing.
    pub last_admitted_at: Option<AdmissionTime>,

    /// The epoch of the point's clock.
    pub epoch: u32,

    /// Whether the point has stamped a batch, of data or a heartbeat, with the `new` generation of the cut the schedule
    /// holds.
    ///
    /// A point moves to `new` either by stamping an admission time at or after the cut, or because its clock was reset
    /// after it first saw a cut to `new`. Either way the batch was admitted to `new`, so cancelling the cut, or
    /// replacing it with one whose `at` is later than that batch, would move the point back to `old`. Always `false`
    /// under a steady schedule.
    pub admitted_to_new: bool,

    /// Whether every later stamp of the point gets the `new` generation of the cut the schedule holds.
    ///
    /// That is so once the point has seen the cut and either its latest admission time is at or after the cut's `at`,
    /// since admission times never decrease within an epoch, or its clock was reset after it first saw the cut. Always
    /// `false` under a steady schedule, and for a point that has stamped nothing since the cut was published.
    pub passed_cut: bool,

    /// The point's wall clock when the status was read, which the clock stamps the next admission time from unless it
    /// is absorbing a backward jump.
    pub wall_now: AdmissionTime,
}

/// A cut an admission point has seen, identified by its `new` generation.
#[derive(Debug)]
struct ObservedCut {
    // Generation identifiers are never reused, so the identifier stands for the generation without keeping it alive.
    new: GenerationId,

    // The clock's epoch when the point first saw a cut to `new`.
    epoch_at_publish: u32,

    // Whether the point has stamped a batch with `new`.
    admitted_to_new: bool,
}

/// The state an admission point guards with its mutex.
#[derive(Debug)]
struct PointState {
    clock: AdmissionClock,
    observed: Option<ObservedCut>,
    last_activity: Instant,
}

impl PointState {
    /// Returns what the point recorded about the cut to `new`, recording the clock's current epoch if the point has
    /// not seen a cut to `new` before.
    ///
    /// Only a cut to another generation replaces the record, so publishing an equal cut again, or a cut to the same
    /// generation at another time, keeps the epoch the point first saw.
    fn observe_cut(&mut self, new: &GenerationRef) -> &mut ObservedCut {
        let epoch = self.clock.epoch();
        let observed = self.observed.get_or_insert(ObservedCut {
            new: new.id(),
            epoch_at_publish: epoch,
            admitted_to_new: false,
        });
        if observed.new != new.id() {
            *observed = ObservedCut {
                new: new.id(),
                epoch_at_publish: epoch,
                admitted_to_new: false,
            };
        }
        observed
    }

    /// Returns the epoch at which the point first saw `schedule`'s cut, recording it now if it has not seen it before.
    ///
    /// A steady schedule needs no epoch; the clock's current epoch is returned for it.
    fn epoch_at_publish(&mut self, schedule: &AdmissionSchedule) -> u32 {
        match schedule {
            AdmissionSchedule::Steady(_) => self.clock.epoch(),
            AdmissionSchedule::Cut { new, .. } => self.observe_cut(new).epoch_at_publish,
        }
    }

    /// Takes the next stamp under `schedule`, returning it together with the epoch at which the point first saw the
    /// schedule's cut.
    fn advance(&mut self, id: AdmissionPointId, schedule: &AdmissionSchedule) -> (ClockStamp, u32) {
        // Read the schedule before the clock, so that a reset detected by this very stamp counts as happening after
        // the schedule was published.
        let epoch_at_publish = self.epoch_at_publish(schedule);
        let previous = self.clock.latest();
        let stamp = self.clock.stamp();
        if stamp.reset {
            warn!(
                admission_point = id.get(),
                previous = %previous.unwrap_or(AdmissionTime::UNIX_EPOCH),
                restarted_at = %stamp.admitted_at,
                epoch = stamp.epoch,
                "Wall clock moved backward by more than the clock-reset threshold. Admission clock restarted in a new \
                 epoch."
            );
        }
        self.last_activity = Instant::now();
        (stamp, epoch_at_publish)
    }

    /// Takes the next stamp under `schedule`, returning it with the generation the schedule assigns to it.
    ///
    /// A stamp that gets the `new` generation of the schedule's cut records that the point admitted to it, whether it
    /// stamps data or a heartbeat.
    fn stamp<'s>(&mut self, id: AdmissionPointId, schedule: &'s AdmissionSchedule) -> (ClockStamp, &'s GenerationRef) {
        let (stamp, epoch_at_publish) = self.advance(id, schedule);
        let generation = schedule.generation_for(stamp.admitted_at, stamp.epoch, epoch_at_publish);
        if let AdmissionSchedule::Cut { new, .. } = schedule {
            if Arc::ptr_eq(generation, new) {
                self.observe_cut(new).admitted_to_new = true;
            }
        }
        (stamp, generation)
    }

    /// Returns what the point has stamped, relative to `schedule`.
    fn status(&self, schedule: &AdmissionSchedule) -> AdmissionPointStatus {
        let latest = self.clock.latest();
        let epoch = self.clock.epoch();
        let (admitted_to_new, passed_cut) = match (schedule, &self.observed) {
            (AdmissionSchedule::Cut { new, at, .. }, Some(observed)) if observed.new == new.id() => {
                let passed = latest.is_some_and(|latest| latest >= *at) || epoch != observed.epoch_at_publish;
                (observed.admitted_to_new, passed)
            }
            _ => (false, false),
        };
        AdmissionPointStatus {
            last_admitted_at: latest,
            epoch,
            admitted_to_new,
            passed_cut,
            wall_now: self.clock.wall_now(),
        }
    }
}

/// The downstream channels of the output an admission point belongs to, and how it knows that the output's component
/// has stopped.
#[derive(Debug)]
struct Attachment {
    senders: Box<[mpsc::WeakSender<EventsBuffer>]>,
    owner: Weak<()>,
}

/// A token the dispatcher of an admission point's output holds for as long as the output's component runs.
///
/// Dropping it, with the dispatcher, tells the point that the component has stopped. The downstream channels cannot
/// tell it: other components may hold senders to the same channels, keeping them open.
#[derive(Debug)]
pub(crate) struct OwnerToken {
    _alive: Arc<()>,
}

/// A place where batches enter plan-driven processing: one output of one component.
///
/// An admission point stamps the [header][BatchHeader] of every batch sent on its output with an admission time from
/// its own [clock][AdmissionClock], its identifier, the clock's epoch, and the generation its
/// [schedule][AdmissionSchedule] chooses. Its [mode][AdmissionMode] decides whether batches that are already stamped
/// keep their stamp.
///
/// Stamping happens inside the output's dispatcher, under a mutex that is held across the stamp and the whole
/// sequential fan-out to the output's downstream channels. Every downstream channel therefore receives the point's
/// batches in admission-time order, even when several tasks send on the output concurrently. The mutex cannot
/// deadlock, because a topology is acyclic.
///
/// An idle admission point can send **heartbeats**: header-only batches stamped like data, with an admission time,
/// epoch, admission point, and the generation the schedule assigns to that admission time, so that downstream state
/// can tell that nothing earlier is still on its way, and to which generation the point admits. Heartbeats go through
/// the output's own downstream channels, under the same mutex, so their order relative to data is the order in which
/// they were stamped; a point configured with [heartbeat recipients][AdmissionPointConfig::with_heartbeat_recipients]
/// sends them only on the channels of those components. The point holds those channels only weakly, so it never keeps
/// a channel open, and it sends no heartbeat once the component that owns the output has stopped, even while other
/// components still send to the same channels.
///
/// The point's state lives behind a shared mutex, so that an [`AdmissionRegistry`] transaction can hold the locks of
/// several points at once, and stamping stops at each of them until the transaction ends. A transaction waits for each
/// lock at most for its timeout, so it cannot deadlock with dispatches either.
#[derive(Debug)]
pub struct AdmissionPoint {
    id: AdmissionPointId,
    mode: AdmissionMode,
    heartbeat: Option<Duration>,
    heartbeat_recipients: Box<[ComponentId]>,
    schedule: ScheduleHandle,
    state: Arc<Mutex<PointState>>,
    attachment: OnceLock<Attachment>,
}

impl AdmissionPoint {
    /// Creates an admission point from its configuration.
    ///
    /// The point is not attached to any output yet: a dispatcher attaches it to one of its outputs.
    pub fn new(config: AdmissionPointConfig) -> Self {
        let AdmissionPointConfig {
            id,
            mode,
            schedule,
            clock_reset_threshold,
            heartbeat,
            heartbeat_recipients,
            wall_clock,
            // The topology that builds the point registers it.
            registry: _,
        } = config;

        Self {
            id,
            mode,
            heartbeat,
            heartbeat_recipients: heartbeat_recipients.into(),
            schedule,
            state: Arc::new(Mutex::new(PointState {
                clock: AdmissionClock::with_wall_clock(clock_reset_threshold, wall_clock),
                observed: None,
                last_activity: Instant::now(),
            })),
            attachment: OnceLock::new(),
        }
    }

    /// Returns the identifier of this admission point.
    pub fn id(&self) -> AdmissionPointId {
        self.id
    }

    /// Returns the mode of this admission point.
    pub fn mode(&self) -> AdmissionMode {
        self.mode
    }

    /// Returns the heartbeat period, if periodic heartbeats are enabled.
    pub fn heartbeat_period(&self) -> Option<Duration> {
        self.heartbeat
    }

    /// Returns the components the point sends heartbeats to; empty means every component its output feeds.
    pub fn heartbeat_recipients(&self) -> &[ComponentId] {
        &self.heartbeat_recipients
    }

    /// Returns what this point has stamped, relative to the schedule it stamps from.
    ///
    /// It waits for any dispatch or heartbeat in progress on the point to finish. See
    /// [`AdmissionRegistry::status_within`] for a read that waits at most a given time.
    pub async fn status(&self) -> AdmissionPointStatus {
        let state = self.state.lock().await;
        state.status(&self.schedule.load())
    }

    /// Attaches the downstream channels the point sends heartbeats on, and returns the token the output's dispatcher
    /// holds while its component runs.
    ///
    /// These are the channels of the output this point belongs to that lead to its heartbeat recipients, or all of
    /// them if it has none. The point keeps only weak references to the channels, so it never keeps a channel open by
    /// itself, and sends no heartbeat once the token is dropped.
    ///
    /// # Errors
    ///
    /// If the point is already attached to an output, an error is returned: an admission point belongs to exactly one
    /// output.
    pub(crate) fn attach_senders(
        &self, senders: Box<[mpsc::WeakSender<EventsBuffer>]>,
    ) -> Result<OwnerToken, GenericError> {
        let alive = Arc::new(());
        let attachment = Attachment {
            senders,
            owner: Arc::downgrade(&alive),
        };
        self.attachment.set(attachment).map_err(|_| {
            generic_error!(
                "Admission point {} is already attached to an output. Give every output its own admission point.",
                self.id.get()
            )
        })?;
        Ok(OwnerToken { _alive: alive })
    }

    /// Locks this admission point for one dispatch.
    ///
    /// The returned guard stamps headers. Hold it until the stamped batch has been sent to every downstream channel.
    pub(crate) async fn lock(&self) -> AdmissionGuard<'_> {
        AdmissionGuard {
            point: self,
            state: self.state.lock().await,
        }
    }

    /// Sends a heartbeat to every downstream channel of this point's output that receives heartbeats.
    ///
    /// The heartbeat is an empty batch whose header carries a fresh admission time, the clock's epoch, this point's
    /// identifier, and the generation the schedule assigns to that admission time, exactly as a batch of data would. A
    /// heartbeat that gets the `new` generation of a cut counts as admitted to it (see
    /// [`AdmissionPointStatus::admitted_to_new`]). It waits for room in each channel in turn, exactly as data does, and
    /// holds the point's mutex while doing so.
    pub async fn heartbeat(&self) -> HeartbeatOutcome {
        let mut state = self.state.lock().await;
        self.heartbeat_locked(&mut state).await
    }

    async fn heartbeat_locked(&self, state: &mut PointState) -> HeartbeatOutcome {
        let Some(attachment) = self.attachment.get() else {
            return HeartbeatOutcome::Closed;
        };

        // The output's dispatcher holds the token while the component that owns the output runs. Its channels cannot
        // tell: other components may keep them open.
        if attachment.owner.strong_count() == 0 {
            return HeartbeatOutcome::Closed;
        }

        let mut header = None;
        let mut delivered = false;
        for weak in attachment.senders.iter() {
            // Upgrading fails once every sender to that channel is gone: every component feeding it has stopped.
            let Some(sender) = weak.upgrade() else {
                continue;
            };

            // Stamp lazily, so that a point whose channels are all gone stamps nothing.
            let header = header.get_or_insert_with(|| {
                let schedule = self.schedule.load();
                let (stamp, generation) = state.stamp(self.id, &schedule);
                BatchHeader {
                    admitted_at: Some(stamp.admitted_at),
                    admission_point: Some(self.id),
                    epoch: stamp.epoch,
                    generation: Some(Arc::clone(generation)),
                    entry: None,
                }
            });

            let heartbeat = EventsBuffer::with_capacity(0).with_header(header.clone());
            if sender.send(heartbeat).await.is_ok() {
                delivered = true;
            }
        }

        if delivered {
            HeartbeatOutcome::Sent
        } else {
            HeartbeatOutcome::Closed
        }
    }

    /// Sends a heartbeat whenever this point has stamped nothing for one heartbeat period, until a heartbeat reaches no
    /// channel.
    ///
    /// Returns immediately if periodic heartbeats are disabled. Otherwise it returns once a heartbeat reaches no
    /// channel: when the component that owns the output stops (its dispatcher drops), even if other components keep
    /// the downstream channels open; when every downstream component stops; or when the output has no downstream
    /// channels at all. Detecting that takes up to one period. The driver holds no strong sender between heartbeats,
    /// so it never keeps a channel open.
    pub async fn run_heartbeats(&self) {
        let Some(period) = self.heartbeat.filter(|period| !period.is_zero()) else {
            return;
        };

        loop {
            let due = instant_after(self.state.lock().await.last_activity, period);
            tokio::time::sleep_until(due).await;

            let mut state = self.state.lock().await;
            if instant_after(state.last_activity, period) > Instant::now() {
                // Something was stamped while we slept, so the point is not idle.
                continue;
            }

            if self.heartbeat_locked(&mut state).await == HeartbeatOutcome::Closed {
                return;
            }
        }
    }
}

/// Returns the instant `duration` after `start`, such as when a heartbeat is due: one period after the last stamp.
///
/// A duration too long to add to `start` ends in about 30 years, which is never in practice.
fn instant_after(start: Instant, duration: Duration) -> Instant {
    const FAR_FUTURE: Duration = Duration::from_secs(86_400 * 365 * 30);
    start
        .checked_add(duration)
        .or_else(|| start.checked_add(FAR_FUTURE))
        .unwrap_or(start)
}

/// Exclusive access to an admission point for the duration of one dispatch.
pub(crate) struct AdmissionGuard<'a> {
    point: &'a AdmissionPoint,
    state: MutexGuard<'a, PointState>,
}

impl AdmissionGuard<'_> {
    /// Stamps `header` according to the point's mode.
    pub(crate) fn stamp(&mut self, header: &mut BatchHeader) {
        let point = self.point;
        if point.mode == AdmissionMode::Admit && header.is_stamped() {
            return;
        }

        let schedule = point.schedule.load();
        let (stamp, generation) = self.state.stamp(point.id, &schedule);

        header.admitted_at = Some(stamp.admitted_at);
        header.admission_point = Some(point.id);
        header.epoch = stamp.epoch;
        header.generation = Some(Arc::clone(generation));
        if point.mode == AdmissionMode::Readmit {
            header.entry = None;
        }
    }
}

/// The admission points of a topology, by identifier.
///
/// An installer uses the registry to read the status of every point without blocking on a busy one, and to change the
/// schedule in a [transaction][Self::transaction] that holds several points' locks at once, so that no batch is
/// stamped at those points while it decides where a cut goes. Points register when a topology builds them from a
/// configuration that names the registry (see [`AdmissionPointConfig::with_registry`]), and a point built directly,
/// as a test harness builds one, registers with [`register`][Self::register]. Clones share the same registrations.
///
/// The registry holds its points weakly: a point is registered for as long as its topology keeps it, and once the
/// output it belongs to is gone, its identifier can register again.
#[derive(Clone, Debug, Default)]
pub struct AdmissionRegistry {
    points: Arc<StdMutex<BTreeMap<AdmissionPointId, Weak<AdmissionPoint>>>>,
}

impl AdmissionRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Locks the table of registered points.
    ///
    /// Every change to the table is a single insertion or removal, so the table is consistent even if a thread
    /// panicked while holding the lock.
    fn table(&self) -> StdMutexGuard<'_, BTreeMap<AdmissionPointId, Weak<AdmissionPoint>>> {
        self.points.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Registers `point` under its identifier.
    ///
    /// Registering a point that is already registered does nothing.
    ///
    /// # Errors
    ///
    /// If another live point is registered with the same identifier, an error is returned and the registry is
    /// unchanged.
    pub fn register(&self, point: &Arc<AdmissionPoint>) -> Result<(), GenericError> {
        let mut table = self.table();
        table.retain(|_, registered| registered.strong_count() > 0);
        match table.entry(point.id()) {
            Entry::Occupied(entry) if entry.get().upgrade().is_some_and(|known| Arc::ptr_eq(&known, point)) => Ok(()),
            Entry::Occupied(_) => Err(generic_error!(
                "Admission point {} is already registered. Give every admission point of one registry a unique ID.",
                point.id().get()
            )),
            Entry::Vacant(entry) => {
                entry.insert(Arc::downgrade(point));
                Ok(())
            }
        }
    }

    /// Returns the registered admission point with the given identifier, if any.
    pub fn get(&self, id: AdmissionPointId) -> Option<Arc<AdmissionPoint>> {
        self.table().get(&id).and_then(Weak::upgrade)
    }

    /// Returns the identifiers of every registered admission point, in increasing order.
    pub fn ids(&self) -> Vec<AdmissionPointId> {
        self.table()
            .iter()
            .filter(|(_, point)| point.strong_count() > 0)
            .map(|(id, _)| *id)
            .collect()
    }

    /// Returns the status of the given admission point, waiting at most `timeout` for a dispatch in progress on it.
    ///
    /// Returns `None` if the point is not registered, or if its lock was not free within `timeout`, which happens while
    /// its output waits for room downstream: a caller that polls several points sees the others without waiting for
    /// the busy one.
    pub async fn status_within(&self, id: AdmissionPointId, timeout: Duration) -> Option<AdmissionPointStatus> {
        let point = self.get(id)?;
        let state = tokio::time::timeout(timeout, point.state.lock()).await.ok()?;
        Some(state.status(&point.schedule.load()))
    }

    /// Locks the given admission points, in the given order, and returns a transaction that holds every lock.
    ///
    /// While the transaction holds a point's lock, the point stamps nothing, so the statuses the transaction reports
    /// stay true until it publishes a schedule or is abandoned. Locks are taken one at a time in `lock_order`, each
    /// waiting at most for what remains of `timeout`; a caller chooses the order so that it never holds a lock while
    /// waiting on a point that may stay busy.
    ///
    /// A point's lock is held for the whole fan-out of each batch it sends, so waiting for it means waiting until the
    /// point's current batch has reached every downstream channel. Waiters are served in order, so a busy point keeps
    /// a transaction waiting only for the batches already waiting for its lock. Two transactions that take
    /// overlapping locks in opposite orders can wait on each other until one times out, which releases its locks and
    /// lets the other proceed.
    ///
    /// # Errors
    ///
    /// If a point is not registered, [`TransactionError::UnknownPoint`] is returned, and if a point is listed more
    /// than once, [`TransactionError::DuplicatePoint`]; either way before any lock is taken. If the locks are not all
    /// taken within `timeout`, every lock taken so far is released and [`TransactionError::Timeout`] is returned.
    pub async fn transaction(
        &self, lock_order: &[AdmissionPointId], timeout: Duration,
    ) -> Result<ScheduleTransaction, TransactionError> {
        let deadline = instant_after(Instant::now(), timeout);

        // Resolve every point before locking any, so that a transaction that cannot succeed holds nothing.
        let mut points = Vec::with_capacity(lock_order.len());
        for (index, &id) in lock_order.iter().enumerate() {
            if lock_order[..index].contains(&id) {
                return Err(TransactionError::DuplicatePoint { point: id });
            }
            points.push(self.get(id).ok_or(TransactionError::UnknownPoint { point: id })?);
        }

        let mut locked = Vec::with_capacity(points.len());
        for point in points {
            match tokio::time::timeout_at(deadline, Arc::clone(&point.state).lock_owned()).await {
                Ok(state) => locked.push(LockedPoint { point, state }),
                // Returning drops every guard taken so far, which releases those locks.
                Err(_) => {
                    return Err(TransactionError::Timeout {
                        point: point.id(),
                        timeout,
                    })
                }
            }
        }
        Ok(ScheduleTransaction { locked })
    }
}

/// One admission point a transaction holds locked.
#[derive(Debug)]
struct LockedPoint {
    point: Arc<AdmissionPoint>,
    state: OwnedMutexGuard<PointState>,
}

/// A change of the admission schedule made while holding the locks of several admission points.
///
/// Created by [`AdmissionRegistry::transaction`]. Dropping it, or calling [`abandon`][Self::abandon], releases every
/// lock without changing anything. Every locked point stalls the output it belongs to until then, so hold a transaction
/// only for as long as deciding what to publish takes, and never across an `await`.
#[derive(Debug)]
pub struct ScheduleTransaction {
    locked: Vec<LockedPoint>,
}

impl ScheduleTransaction {
    /// Returns the status of every locked admission point, in lock order.
    ///
    /// Nothing is stamped at a locked point, so everything in a status but its wall-clock reading stays as reported
    /// until the transaction ends.
    pub fn statuses(&self) -> Vec<(AdmissionPointId, AdmissionPointStatus)> {
        self.locked
            .iter()
            .map(|locked| (locked.point.id(), locked.state.status(&locked.point.schedule.load())))
            .collect()
    }

    /// Publishes `schedule` while every lock is held, then releases the locks.
    ///
    /// The schedule is stored into the schedule handle of every locked point; the points of one topology share one
    /// handle. The next stamp of every locked point reads the new schedule, and so does the next stamp of any other
    /// point that reads the same handle. A transaction that locks no point has no handle to store into, and publishes
    /// nothing.
    pub fn publish(self, schedule: AdmissionSchedule) {
        let schedule = Arc::new(schedule);
        let mut published: SmallVec<[&ScheduleHandle; 1]> = SmallVec::new();
        for locked in &self.locked {
            let handle = &locked.point.schedule;
            if !published.iter().any(|done| Arc::ptr_eq(done, handle)) {
                handle.store(Arc::clone(&schedule));
                published.push(handle);
            }
        }
        // The locks are released only after every store, when the transaction drops on return.
    }

    /// Releases every lock without changing the schedule.
    pub fn abandon(self) {}
}

/// Why an [`AdmissionRegistry::transaction`] could not take its locks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TransactionError {
    /// A point's lock was not free in time, because its output kept waiting for room downstream.
    Timeout {
        /// The point whose lock was not free.
        point: AdmissionPointId,

        /// How long the transaction waited for all of its locks.
        timeout: Duration,
    },

    /// A point the transaction names is not registered.
    UnknownPoint {
        /// The point that is not registered.
        point: AdmissionPointId,
    },

    /// A point is listed more than once in the transaction's lock order.
    DuplicatePoint {
        /// The point listed more than once.
        point: AdmissionPointId,
    },
}

impl fmt::Display for TransactionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout { point, timeout } => write!(
                f,
                "Timed out after {:?} waiting for the lock of admission point {}, whose output is waiting for room \
                 downstream. Retry once downstream drains.",
                timeout,
                point.get()
            ),
            Self::UnknownPoint { point } => write!(
                f,
                "Admission point {} is not registered. Configure every point a transaction names with the registry.",
                point.get()
            ),
            Self::DuplicatePoint { point } => write!(
                f,
                "Admission point {} is listed more than once in the transaction's lock order. List each point once.",
                point.get()
            ),
        }
    }
}

impl std::error::Error for TransactionError {}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicU64, Ordering},
        Mutex as StdMutex,
    };

    use futures::FutureExt as _;

    use super::*;
    use crate::{
        components::ComponentContext,
        data_model::event::{metric::Metric, Event},
        execution::{
            clock::ManualWallClock,
            generation::{CutBoundary, GenerationId, GenerationToken, RetireNotifier},
            header::EntryId,
        },
        topology::{interconnect::Dispatcher, OutputName},
    };

    const THRESHOLD: Duration = Duration::from_secs(600);

    fn generation(id: u64) -> GenerationRef {
        Arc::new(GenerationToken::new(GenerationId::new(id)))
    }

    fn secs(secs: u64) -> AdmissionTime {
        AdmissionTime::from_unix_secs(secs)
    }

    fn point_with<W: WallClock + 'static>(
        id: u32, mode: AdmissionMode, schedule: &ScheduleHandle, wall: W, heartbeat: Option<Duration>,
    ) -> Arc<AdmissionPoint> {
        let mut config = AdmissionPointConfig::new(AdmissionPointId::new(id), mode, Arc::clone(schedule), THRESHOLD)
            .with_wall_clock(wall);
        if let Some(period) = heartbeat {
            config = config.with_heartbeat(period);
        }
        Arc::new(AdmissionPoint::new(config))
    }

    fn point(id: u32, mode: AdmissionMode, schedule: &ScheduleHandle, wall: &ManualWallClock) -> Arc<AdmissionPoint> {
        point_with(id, mode, schedule, wall.clone(), None)
    }

    /// Returns a dispatcher whose default output is `point`, connected to `receivers` channels of `capacity` each.
    fn wired(
        point: &Arc<AdmissionPoint>, receivers: usize, capacity: usize,
    ) -> (Dispatcher<EventsBuffer>, Vec<mpsc::Receiver<EventsBuffer>>) {
        let mut dispatcher = Dispatcher::new(ComponentContext::test_source("admission_test"));
        dispatcher.add_output(OutputName::Default).unwrap();
        let mut channels = Vec::new();
        for _ in 0..receivers {
            let (sender, receiver) = mpsc::channel(capacity);
            dispatcher
                .attach_sender_to_output(&OutputName::Default, sender)
                .unwrap();
            channels.push(receiver);
        }
        dispatcher
            .set_admission_point(&OutputName::Default, Arc::clone(point))
            .unwrap();
        (dispatcher, channels)
    }

    fn batch(events: usize) -> EventsBuffer {
        let mut batch = EventsBuffer::with_capacity(events);
        for value in 0..events {
            assert!(batch
                .try_push(Event::Metric(Metric::counter("m", value as f64)))
                .is_none());
        }
        batch
    }

    /// A comparable summary of a received buffer: its stamp, entry, and event count.
    #[derive(Debug, Eq, PartialEq)]
    struct Received {
        admitted_at: Option<AdmissionTime>,
        admission_point: Option<u32>,
        epoch: u32,
        generation: Option<u64>,
        entry: Option<u16>,
        events: usize,
    }

    impl Received {
        fn of(buffer: &EventsBuffer) -> Self {
            let header = buffer.header();
            Self {
                admitted_at: header.admitted_at,
                admission_point: header.admission_point.map(AdmissionPointId::get),
                epoch: header.epoch,
                generation: header.generation_id().map(GenerationId::get),
                entry: header.entry.map(EntryId::get),
                events: buffer.len(),
            }
        }

        fn data(admitted_at: AdmissionTime, point: u32, epoch: u32, generation: u64, events: usize) -> Self {
            Self {
                admitted_at: Some(admitted_at),
                admission_point: Some(point),
                epoch,
                generation: Some(generation),
                entry: None,
                events,
            }
        }

        fn heartbeat(admitted_at: AdmissionTime, point: u32, epoch: u32, generation: u64) -> Self {
            Self {
                admitted_at: Some(admitted_at),
                admission_point: Some(point),
                epoch,
                generation: Some(generation),
                entry: None,
                events: 0,
            }
        }
    }

    /// Dispatches `buffer` with the wall clock at `at`, and returns what the receiver got.
    async fn send_at(
        dispatcher: &Dispatcher<EventsBuffer>, receiver: &mut mpsc::Receiver<EventsBuffer>, wall: &ManualWallClock,
        at: AdmissionTime, buffer: EventsBuffer,
    ) -> Received {
        wall.set(at);
        dispatcher.dispatch(buffer).await.unwrap();
        Received::of(&receiver.try_recv().expect("the batch should have been delivered"))
    }

    fn pre_stamped(generation: &GenerationRef) -> EventsBuffer {
        batch(1).with_header(BatchHeader {
            admitted_at: Some(secs(5)),
            admission_point: Some(AdmissionPointId::new(9)),
            epoch: 3,
            generation: Some(Arc::clone(generation)),
            entry: Some(EntryId::new(2)),
        })
    }

    fn drain(receiver: &mut mpsc::Receiver<EventsBuffer>) -> Vec<Received> {
        std::iter::from_fn(|| receiver.try_recv().ok())
            .map(|buffer| Received::of(&buffer))
            .collect()
    }

    #[test]
    fn the_schedule_chooses_generations_by_admission_time_and_epoch() {
        let steady = AdmissionSchedule::Steady(generation(1));
        let cut = AdmissionSchedule::Cut {
            old: generation(1),
            new: generation(2),
            at: secs(100),
        };

        // (schedule, t_a, epoch of the stamp, epoch when the point first saw the schedule, expected generation)
        let cases = [
            (&steady, secs(0), 0, 0, 1),
            (&steady, secs(1_000), 5, 0, 1),
            (&cut, secs(99), 0, 0, 1),
            (&cut, AdmissionTime::from_unix_nanos(99_999_999_999), 0, 0, 1),
            (&cut, secs(100), 0, 0, 2),
            (&cut, secs(101), 0, 0, 2),
            // A reset after the point saw the cut counts as reaching it, however small t_a is.
            (&cut, secs(0), 1, 0, 2),
            (&cut, secs(99), 4, 3, 2),
            // A reset before the point saw the cut does not.
            (&cut, secs(99), 3, 3, 1),
            (&cut, secs(150), 1, 1, 2),
            // Epochs wrap, so any difference counts as a reset.
            (&cut, secs(99), 0, u32::MAX, 2),
        ];
        for (schedule, admitted_at, epoch, epoch_at_publish, expected) in cases {
            let chosen = schedule.generation_for(admitted_at, epoch, epoch_at_publish).id().get();
            assert_eq!(
                chosen, expected,
                "{schedule:?} at {admitted_at} in epoch {epoch} (published in {epoch_at_publish})"
            );
        }
    }

    #[tokio::test]
    async fn admit_stamps_unstamped_batches_and_keeps_stamped_ones() {
        let schedule = AdmissionSchedule::Steady(generation(1)).into_handle();
        let wall = ManualWallClock::default();
        let point = point(1, AdmissionMode::Admit, &schedule, &wall);
        let (dispatcher, mut receivers) = wired(&point, 1, 4);
        let receiver = &mut receivers[0];

        assert_eq!(
            send_at(&dispatcher, receiver, &wall, secs(100), batch(2)).await,
            Received::data(secs(100), 1, 0, 1, 2)
        );

        // A batch that is already stamped passes through as it is, entry included.
        assert_eq!(
            send_at(&dispatcher, receiver, &wall, secs(101), pre_stamped(&generation(7))).await,
            Received {
                admitted_at: Some(secs(5)),
                admission_point: Some(9),
                epoch: 3,
                generation: Some(7),
                entry: Some(2),
                events: 1,
            }
        );

        assert_eq!(
            send_at(&dispatcher, receiver, &wall, secs(102), batch(0)).await,
            Received::data(secs(102), 1, 0, 1, 0)
        );
    }

    #[tokio::test]
    async fn readmit_overwrites_every_stamp_and_clears_the_entry() {
        let schedule = AdmissionSchedule::Steady(generation(1)).into_handle();
        let wall = ManualWallClock::default();
        let point = point(2, AdmissionMode::Readmit, &schedule, &wall);
        let (dispatcher, mut receivers) = wired(&point, 1, 4);
        let receiver = &mut receivers[0];

        assert_eq!(
            send_at(&dispatcher, receiver, &wall, secs(100), pre_stamped(&generation(7))).await,
            Received::data(secs(100), 2, 0, 1, 1)
        );
        assert_eq!(
            send_at(&dispatcher, receiver, &wall, secs(101), batch(3)).await,
            Received::data(secs(101), 2, 0, 1, 3)
        );
    }

    #[tokio::test]
    async fn every_channel_of_an_output_receives_the_same_stamp() {
        let generation_one = generation(1);
        let schedule = AdmissionSchedule::Steady(Arc::clone(&generation_one)).into_handle();
        let wall = ManualWallClock::new(secs(42));
        let point = point(3, AdmissionMode::Admit, &schedule, &wall);
        let (dispatcher, mut receivers) = wired(&point, 3, 1);

        dispatcher.dispatch(batch(2)).await.unwrap();
        let received = receivers
            .iter_mut()
            .map(|receiver| receiver.try_recv().unwrap())
            .collect::<Vec<_>>();
        for buffer in &received {
            assert_eq!(Received::of(buffer), Received::data(secs(42), 3, 0, 1, 2));
        }

        // Each copy holds one reference to the generation, beside the schedule's and ours.
        assert_eq!(Arc::strong_count(&generation_one), 5);
    }

    #[tokio::test]
    async fn buffered_dispatches_are_stamped_when_flushed() {
        let schedule = AdmissionSchedule::Steady(generation(1)).into_handle();
        let wall = ManualWallClock::new(secs(10));
        let point = point(4, AdmissionMode::Admit, &schedule, &wall);
        let (dispatcher, mut receivers) = wired(&point, 1, 4);

        let mut buffered = dispatcher.buffered().unwrap();
        for value in 0..1_025 {
            if value == 1_000 {
                wall.set(secs(11));
            }
            buffered
                .push(Event::Metric(Metric::counter("m", value as f64)))
                .await
                .unwrap();
        }
        wall.set(secs(12));
        buffered.flush().await.unwrap();

        // The full buffer is sent while the 1,025th event is pushed, when the clock reads 11.
        assert_eq!(
            drain(&mut receivers[0]),
            vec![
                Received::data(secs(11), 4, 0, 1, 1_024),
                Received::data(secs(12), 4, 0, 1, 1)
            ]
        );
    }

    #[tokio::test]
    async fn batches_follow_a_published_cut() {
        let schedule = AdmissionSchedule::Steady(generation(1)).into_handle();
        let wall = ManualWallClock::default();
        let point = point(1, AdmissionMode::Admit, &schedule, &wall);
        let (dispatcher, mut receivers) = wired(&point, 1, 4);
        let receiver = &mut receivers[0];

        assert_eq!(
            send_at(&dispatcher, receiver, &wall, secs(95), batch(1))
                .await
                .generation,
            Some(1)
        );

        schedule.store(Arc::new(AdmissionSchedule::Cut {
            old: generation(1),
            new: generation(2),
            at: secs(100),
        }));
        assert_eq!(
            send_at(&dispatcher, receiver, &wall, secs(99), batch(1))
                .await
                .generation,
            Some(1)
        );
        assert_eq!(
            send_at(&dispatcher, receiver, &wall, secs(100), batch(1))
                .await
                .generation,
            Some(2)
        );

        // A small backward jump is absorbed, so the batch is still admitted at 100 and stays with the new generation.
        assert_eq!(
            send_at(&dispatcher, receiver, &wall, secs(98), batch(1)).await,
            Received::data(secs(100), 1, 0, 2, 1)
        );

        schedule.store(Arc::new(AdmissionSchedule::Steady(generation(2))));
        assert_eq!(
            send_at(&dispatcher, receiver, &wall, secs(101), batch(1))
                .await
                .generation,
            Some(2)
        );
    }

    #[tokio::test]
    async fn a_clock_reset_after_a_cut_is_published_counts_as_reaching_the_cut() {
        let schedule = AdmissionSchedule::Steady(generation(1)).into_handle();
        let wall = ManualWallClock::default();
        let point = point(1, AdmissionMode::Admit, &schedule, &wall);
        let (dispatcher, mut receivers) = wired(&point, 1, 4);
        let receiver = &mut receivers[0];

        assert_eq!(
            send_at(&dispatcher, receiver, &wall, secs(7_203), batch(1)).await,
            Received::data(secs(7_203), 1, 0, 1, 1)
        );

        schedule.store(Arc::new(AdmissionSchedule::Cut {
            old: generation(1),
            new: generation(2),
            at: secs(8_000),
        }));
        assert_eq!(
            send_at(&dispatcher, receiver, &wall, secs(7_210), batch(1)).await,
            Received::data(secs(7_210), 1, 0, 1, 1)
        );

        // The wall clock jumps back an hour: the clock resets into epoch 1, and the reset counts as reaching the cut.
        assert_eq!(
            send_at(&dispatcher, receiver, &wall, secs(3_610), batch(1)).await,
            Received::data(secs(3_610), 1, 1, 2, 1)
        );
        assert_eq!(
            send_at(&dispatcher, receiver, &wall, secs(3_611), batch(1)).await,
            Received::data(secs(3_611), 1, 1, 2, 1)
        );
    }

    #[tokio::test]
    async fn a_clock_reset_before_a_cut_is_published_does_not_reach_it() {
        let schedule = AdmissionSchedule::Steady(generation(1)).into_handle();
        let wall = ManualWallClock::default();
        let point = point(1, AdmissionMode::Admit, &schedule, &wall);
        let (dispatcher, mut receivers) = wired(&point, 1, 4);
        let receiver = &mut receivers[0];

        assert_eq!(
            send_at(&dispatcher, receiver, &wall, secs(7_203), batch(1)).await,
            Received::data(secs(7_203), 1, 0, 1, 1)
        );
        assert_eq!(
            send_at(&dispatcher, receiver, &wall, secs(3_603), batch(1)).await,
            Received::data(secs(3_603), 1, 1, 1, 1)
        );

        schedule.store(Arc::new(AdmissionSchedule::Cut {
            old: generation(1),
            new: generation(2),
            at: secs(4_000),
        }));
        assert_eq!(
            send_at(&dispatcher, receiver, &wall, secs(3_700), batch(1)).await,
            Received::data(secs(3_700), 1, 1, 1, 1)
        );
        assert_eq!(
            send_at(&dispatcher, receiver, &wall, secs(4_000), batch(1)).await,
            Received::data(secs(4_000), 1, 1, 2, 1)
        );
    }

    #[tokio::test]
    async fn a_reset_found_by_a_heartbeat_moves_later_batches_to_the_new_generation() {
        let schedule = AdmissionSchedule::Steady(generation(1)).into_handle();
        let wall = ManualWallClock::default();
        let point = point(1, AdmissionMode::Admit, &schedule, &wall);
        let (dispatcher, mut receivers) = wired(&point, 1, 4);

        send_at(&dispatcher, &mut receivers[0], &wall, secs(7_203), batch(1)).await;
        schedule.store(Arc::new(AdmissionSchedule::Cut {
            old: generation(1),
            new: generation(2),
            at: secs(8_000),
        }));

        // The first stamp after publication is a heartbeat, and it is the one that finds the reset.
        wall.set(secs(3_603));
        assert_eq!(point.heartbeat().await, HeartbeatOutcome::Sent);
        assert_eq!(
            drain(&mut receivers[0]),
            vec![Received::heartbeat(secs(3_603), 1, 1, 2)]
        );

        assert_eq!(
            send_at(&dispatcher, &mut receivers[0], &wall, secs(3_604), batch(1)).await,
            Received::data(secs(3_604), 1, 1, 2, 1)
        );
    }

    #[tokio::test]
    async fn republishing_a_cut_keeps_a_point_that_a_reset_moved_to_the_new_generation_there() {
        let old = generation(1);
        let new = generation(2);
        let cut = |at| AdmissionSchedule::Cut {
            old: Arc::clone(&old),
            new: Arc::clone(&new),
            at: secs(at),
        };
        let schedule = cut(5_100).into_handle();
        let wall = ManualWallClock::default();
        let point = point(1, AdmissionMode::Admit, &schedule, &wall);
        let (dispatcher, mut receivers) = wired(&point, 1, 4);
        let receiver = &mut receivers[0];

        assert_eq!(
            send_at(&dispatcher, receiver, &wall, secs(5_000), batch(1)).await,
            Received::data(secs(5_000), 1, 0, 1, 1)
        );
        // A reset after the point first saw the cut moves it to the new generation.
        assert_eq!(
            send_at(&dispatcher, receiver, &wall, secs(1_000), batch(1)).await,
            Received::data(secs(1_000), 1, 1, 2, 1)
        );

        // Publishing the same cut again, as a new value, and then a cut to the same generation at another time, keeps
        // the point where the reset moved it: the same stamp gets the same generation.
        schedule.store(Arc::new(cut(5_100)));
        assert_eq!(
            send_at(&dispatcher, receiver, &wall, secs(1_001), batch(1)).await,
            Received::data(secs(1_001), 1, 1, 2, 1)
        );
        schedule.store(Arc::new(cut(6_000)));
        assert_eq!(
            send_at(&dispatcher, receiver, &wall, secs(1_002), batch(1)).await,
            Received::data(secs(1_002), 1, 1, 2, 1)
        );

        // A cut to another generation is a new cut: the point first sees it in epoch 1, so only time reaches it.
        schedule.store(Arc::new(AdmissionSchedule::Cut {
            old: Arc::clone(&new),
            new: generation(3),
            at: secs(1_010),
        }));
        assert_eq!(
            send_at(&dispatcher, receiver, &wall, secs(1_003), batch(1)).await,
            Received::data(secs(1_003), 1, 1, 2, 1)
        );
        assert_eq!(
            send_at(&dispatcher, receiver, &wall, secs(1_010), batch(1)).await,
            Received::data(secs(1_010), 1, 1, 3, 1)
        );
    }

    #[tokio::test]
    async fn the_status_tells_whether_a_point_admitted_to_the_cuts_new_generation() {
        // The status of a point whose wall clock reads what it last stamped: (latest, epoch, admitted, passed).
        let status = |last: Option<u64>, epoch, admitted_to_new, passed_cut| AdmissionPointStatus {
            last_admitted_at: last.map(secs),
            epoch,
            admitted_to_new,
            passed_cut,
            wall_now: secs(last.unwrap_or(0)),
        };
        let schedule = AdmissionSchedule::Steady(generation(1)).into_handle();
        let wall = ManualWallClock::default();
        let point = point(1, AdmissionMode::Admit, &schedule, &wall);
        let (dispatcher, mut receivers) = wired(&point, 1, 8);
        let receiver = &mut receivers[0];
        assert_eq!(point.status().await, status(None, 0, false, false));

        send_at(&dispatcher, receiver, &wall, secs(7_203), batch(1)).await;
        assert_eq!(point.status().await, status(Some(7_203), 0, false, false));

        // Under the cut, the point admits to the old generation until time reaches it.
        let cut = || AdmissionSchedule::Cut {
            old: generation(1),
            new: generation(2),
            at: secs(8_000),
        };
        schedule.store(Arc::new(cut()));
        // Before its next stamp the point has not seen the cut, so it has not passed it.
        assert_eq!(point.status().await, status(Some(7_203), 0, false, false));
        send_at(&dispatcher, receiver, &wall, secs(7_210), batch(1)).await;
        assert_eq!(point.status().await, status(Some(7_210), 0, false, false));

        // A heartbeat that finds a reset is stamped with the new generation, like data, so the point admitted to it and
        // passed the cut; the next batch is admitted to it too.
        wall.set(secs(3_610));
        assert_eq!(point.heartbeat().await, HeartbeatOutcome::Sent);
        assert_eq!(point.status().await, status(Some(3_610), 1, true, true));
        send_at(&dispatcher, receiver, &wall, secs(3_611), batch(1)).await;
        assert_eq!(point.status().await, status(Some(3_611), 1, true, true));

        // Republishing the cut keeps that; a steady schedule, or a cut to another generation, has nothing admitted.
        schedule.store(Arc::new(cut()));
        assert_eq!(point.status().await, status(Some(3_611), 1, true, true));
        schedule.store(Arc::new(AdmissionSchedule::Steady(generation(2))));
        assert_eq!(point.status().await, status(Some(3_611), 1, false, false));
        schedule.store(Arc::new(AdmissionSchedule::Cut {
            old: generation(2),
            new: generation(3),
            at: secs(3_620),
        }));
        assert_eq!(point.status().await, status(Some(3_611), 1, false, false));
        send_at(&dispatcher, receiver, &wall, secs(3_615), batch(1)).await;
        assert_eq!(point.status().await, status(Some(3_615), 1, false, false));

        // Reaching the cut by time admits to the new generation too, and passes it.
        send_at(&dispatcher, receiver, &wall, secs(3_620), batch(1)).await;
        assert_eq!(point.status().await, status(Some(3_620), 1, true, true));
    }

    #[tokio::test]
    async fn the_status_reads_the_points_own_wall_clock() {
        let schedule = AdmissionSchedule::Steady(generation(1)).into_handle();
        let wall = ManualWallClock::new(secs(500));
        let point = point(1, AdmissionMode::Admit, &schedule, &wall);
        let (dispatcher, mut receivers) = wired(&point, 1, 8);

        send_at(&dispatcher, &mut receivers[0], &wall, secs(500), batch(1)).await;
        // The wall clock steps back by less than the reset threshold: the clock keeps its latest stamp, and the status
        // shows how far ahead of the wall clock that is.
        wall.set(secs(450));
        let status = point.status().await;
        assert_eq!(status.last_admitted_at, Some(secs(500)));
        assert_eq!(status.wall_now, secs(450));
        assert_eq!(status.epoch, 0);
    }

    fn id(id: u32) -> AdmissionPointId {
        AdmissionPointId::new(id)
    }

    /// Creates a point and registers it in `registry`.
    fn registered(
        registry: &AdmissionRegistry, id: u32, schedule: &ScheduleHandle, wall: &ManualWallClock,
    ) -> Arc<AdmissionPoint> {
        let point = point(id, AdmissionMode::Admit, schedule, wall);
        registry.register(&point).unwrap();
        point
    }

    /// Waits until every other task is idle. Requires paused time, under which the runtime advances the clock only
    /// once no task can make progress.
    async fn settle() {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    /// Returns whether the point's lock is free right now.
    async fn is_free(registry: &AdmissionRegistry, point: u32) -> bool {
        registry.status_within(id(point), Duration::ZERO).await.is_some()
    }

    /// A point whose output waits for room downstream while it holds the point's lock, as a source's does when its
    /// downstream is full.
    struct Busy {
        receiver: mpsc::Receiver<EventsBuffer>,
        blocked: tokio::task::JoinHandle<()>,
        _dispatcher: Arc<Dispatcher<EventsBuffer>>,
    }

    impl Busy {
        /// Fills the point's only channel and starts a dispatch that waits for room in it. Requires paused time.
        async fn new(point: &Arc<AdmissionPoint>) -> Self {
            let (dispatcher, mut receivers) = wired(point, 1, 1);
            let dispatcher = Arc::new(dispatcher);
            dispatcher.dispatch(batch(1)).await.unwrap();
            let blocked = tokio::spawn({
                let dispatcher = Arc::clone(&dispatcher);
                async move { dispatcher.dispatch(batch(1)).await.unwrap() }
            });
            settle().await;
            assert!(!blocked.is_finished(), "the dispatch should wait for room downstream");
            Self {
                receiver: receivers.pop().unwrap(),
                blocked,
                _dispatcher: dispatcher,
            }
        }

        /// Makes room downstream, so that the waiting dispatch completes and releases the point's lock.
        async fn release(mut self) {
            self.receiver.recv().await.unwrap();
            self.blocked.await.unwrap();
        }
    }

    #[test]
    fn the_registry_finds_the_points_registered_in_it_for_as_long_as_they_live() {
        let schedule = AdmissionSchedule::Steady(generation(1)).into_handle();
        let wall = ManualWallClock::default();
        let registry = AdmissionRegistry::new();
        assert!(registry.ids().is_empty());
        assert!(registry.get(id(1)).is_none());

        // Clones share their registrations.
        let second = point(2, AdmissionMode::Admit, &schedule, &wall);
        let first = point(1, AdmissionMode::Readmit, &schedule, &wall);
        registry.register(&second).unwrap();
        registry.clone().register(&first).unwrap();
        assert_eq!(registry.ids(), vec![id(1), id(2)]);
        assert!(Arc::ptr_eq(&registry.get(id(1)).unwrap(), &first));

        // Registering a point again does nothing; another point with a registered identifier is refused.
        registry.register(&first).unwrap();
        let impostor = point(1, AdmissionMode::Admit, &schedule, &wall);
        assert_eq!(
            registry.register(&impostor).unwrap_err().to_string(),
            "Admission point 1 is already registered. Give every admission point of one registry a unique ID."
        );
        assert!(Arc::ptr_eq(&registry.get(id(1)).unwrap(), &first));

        // The registry holds points weakly: once a point is gone, so is its registration, and its identifier is free.
        drop(first);
        assert_eq!(registry.ids(), vec![id(2)]);
        assert!(registry.get(id(1)).is_none());
        registry.register(&impostor).unwrap();
        assert_eq!(registry.ids(), vec![id(1), id(2)]);
    }

    #[tokio::test(start_paused = true)]
    async fn status_reads_wait_at_most_their_timeout_for_a_busy_point() {
        let schedule = AdmissionSchedule::Steady(generation(1)).into_handle();
        let wall = ManualWallClock::new(secs(100));
        let registry = AdmissionRegistry::new();
        let free = registered(&registry, 1, &schedule, &wall);
        let blocked = registered(&registry, 2, &schedule, &wall);
        let (dispatcher, mut receivers) = wired(&free, 1, 4);
        send_at(&dispatcher, &mut receivers[0], &wall, secs(100), batch(1)).await;
        let busy = Busy::new(&blocked).await;

        let status = registry.status_within(id(1), Duration::from_millis(10)).await;
        assert_eq!(status.map(|status| status.last_admitted_at), Some(Some(secs(100))));

        // A point whose output waits downstream reports nothing within the timeout, and neither does an unknown one.
        let started = Instant::now();
        assert_eq!(registry.status_within(id(2), Duration::from_millis(10)).await, None);
        assert_eq!(started.elapsed(), Duration::from_millis(10));
        assert_eq!(registry.status_within(id(3), Duration::from_millis(10)).await, None);

        busy.release().await;
        let status = registry.status_within(id(2), Duration::from_millis(10)).await;
        assert_eq!(status.map(|status| status.last_admitted_at), Some(Some(secs(100))));
    }

    #[tokio::test(start_paused = true)]
    async fn arming_in_a_transaction_closes_the_race_that_publishing_and_then_rechecking_misses() {
        // Arming reads the point's latest admission time, 95, and publishes a cut at 100. A batch stamped between the
        // read and the store, at 105, gets the old generation although it was admitted after the cut.
        let old = generation(1);
        let new = generation(2);
        let cut = || AdmissionSchedule::Cut {
            old: Arc::clone(&old),
            new: Arc::clone(&new),
            at: secs(100),
        };

        // Publishing and then rechecking: the race happens, and the recheck cannot see it, since the point admitted
        // nothing to the new generation.
        let schedule = AdmissionSchedule::Steady(Arc::clone(&old)).into_handle();
        let wall = ManualWallClock::default();
        let point = point(1, AdmissionMode::Admit, &schedule, &wall);
        let (dispatcher, mut receivers) = wired(&point, 1, 4);
        send_at(&dispatcher, &mut receivers[0], &wall, secs(95), batch(1)).await;
        assert_eq!(point.status().await.last_admitted_at, Some(secs(95)));
        let raced = send_at(&dispatcher, &mut receivers[0], &wall, secs(105), batch(1)).await;
        schedule.store(Arc::new(cut()));
        assert_eq!(raced, Received::data(secs(105), 1, 0, 1, 1));
        assert!(!point.status().await.admitted_to_new);

        // In a transaction, the same batch waits until the cut is published, and gets the new generation.
        let schedule = AdmissionSchedule::Steady(Arc::clone(&old)).into_handle();
        let registry = AdmissionRegistry::new();
        let point = registered(&registry, 1, &schedule, &wall);
        let (dispatcher, mut receivers) = wired(&point, 1, 4);
        let dispatcher = Arc::new(dispatcher);
        send_at(&dispatcher, &mut receivers[0], &wall, secs(95), batch(1)).await;

        let transaction = registry.transaction(&[id(1)], Duration::from_secs(1)).await.unwrap();
        assert_eq!(
            transaction.statuses(),
            vec![(
                id(1),
                AdmissionPointStatus {
                    last_admitted_at: Some(secs(95)),
                    epoch: 0,
                    admitted_to_new: false,
                    passed_cut: false,
                    wall_now: secs(95),
                }
            )]
        );
        wall.set(secs(105));
        let raced = tokio::spawn({
            let dispatcher = Arc::clone(&dispatcher);
            async move { dispatcher.dispatch(batch(1)).await }
        });
        settle().await;
        assert!(
            !raced.is_finished(),
            "nothing is stamped while the transaction holds the point"
        );
        assert!(receivers[0].try_recv().is_err());

        transaction.publish(cut());
        raced.await.unwrap().unwrap();
        assert_eq!(
            Received::of(&receivers[0].try_recv().unwrap()),
            Received::data(secs(105), 1, 0, 2, 1)
        );
        assert!(point.status().await.admitted_to_new);
    }

    #[tokio::test(start_paused = true)]
    async fn an_abandoned_transaction_releases_its_points_and_changes_nothing() {
        let schedule = AdmissionSchedule::Steady(generation(1)).into_handle();
        let wall = ManualWallClock::new(secs(100));
        let registry = AdmissionRegistry::new();
        let point = registered(&registry, 1, &schedule, &wall);
        let (dispatcher, mut receivers) = wired(&point, 1, 4);
        let dispatcher = Arc::new(dispatcher);

        let transaction = registry.transaction(&[id(1)], Duration::from_secs(1)).await.unwrap();
        let waiting = tokio::spawn({
            let dispatcher = Arc::clone(&dispatcher);
            async move { dispatcher.dispatch(batch(1)).await }
        });
        settle().await;
        assert!(!waiting.is_finished());

        transaction.abandon();
        waiting.await.unwrap().unwrap();
        assert_eq!(
            Received::of(&receivers[0].try_recv().unwrap()),
            Received::data(secs(100), 1, 0, 1, 1)
        );
        assert!(is_free(&registry, 1).await);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_stamping_never_admits_to_a_generation_outside_its_armed_cuts() {
        // Senders stamp batches as fast as they can on the system clock while cuts are armed one after another in
        // transactions, each one microsecond after the point's latest stamp or wall clock, and finalized once the point
        // passed them. Every batch of a generation must be admitted at or after the cut that installed it and before
        // the cut that replaced it. Reading the point and publishing outside a transaction fails this as soon as a few
        // microseconds pass between the two: a batch stamped meanwhile gets the old generation at or after the cut.
        const SENDERS: usize = 4;
        const CUTS: usize = 20;
        let generations = (0..=CUTS as u64).map(generation).collect::<Vec<_>>();
        let schedule = AdmissionSchedule::Steady(Arc::clone(&generations[0])).into_handle();
        let point = point_with(1, AdmissionMode::Admit, &schedule, SystemWallClock, None);
        let registry = AdmissionRegistry::new();
        registry.register(&point).unwrap();
        let (dispatcher, receivers) = wired(&point, 2, 1);
        let dispatcher = Arc::new(dispatcher);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let consumers = receivers
            .into_iter()
            .map(|mut receiver| {
                tokio::spawn(async move {
                    let mut received = Vec::new();
                    while let Some(buffer) = receiver.recv().await {
                        let header = buffer.header();
                        received.push((header.admitted_at.unwrap(), header.generation_id().unwrap().get()));
                    }
                    received
                })
            })
            .collect::<Vec<_>>();
        let senders = (0..SENDERS)
            .map(|_| {
                let dispatcher = Arc::clone(&dispatcher);
                let stop = Arc::clone(&stop);
                tokio::spawn(async move {
                    while !stop.load(Ordering::Relaxed) {
                        dispatcher.dispatch(batch(1)).await.unwrap();
                    }
                })
            })
            .collect::<Vec<_>>();

        let mut cuts = Vec::new();
        for next in 1..=CUTS {
            let transaction = registry.transaction(&[id(1)], Duration::from_secs(5)).await.unwrap();
            let (_, status) = transaction.statuses()[0];
            let now = status
                .last_admitted_at
                .unwrap_or(AdmissionTime::UNIX_EPOCH)
                .max(status.wall_now);
            let at = now.saturating_add(Duration::from_micros(1));
            let boundary = CutBoundary {
                at,
                replaces: generations[next - 1].id(),
                epochs: SmallVec::from_slice(&[(id(1), status.epoch)]),
            };
            generations[next].set_cut(boundary).unwrap();
            transaction.publish(AdmissionSchedule::Cut {
                old: Arc::clone(&generations[next - 1]),
                new: Arc::clone(&generations[next]),
                at,
            });
            cuts.push(at);

            // Once the point passed the cut, every later stamp gets the new generation under either schedule.
            while !registry
                .status_within(id(1), Duration::from_secs(1))
                .await
                .is_some_and(|status| status.passed_cut)
            {
                tokio::task::yield_now().await;
            }
            schedule.store(Arc::new(AdmissionSchedule::Steady(Arc::clone(&generations[next]))));
        }

        stop.store(true, Ordering::Relaxed);
        for sender in senders {
            sender.await.unwrap();
        }
        drop(dispatcher);
        let mut sequences = Vec::new();
        for consumer in consumers {
            sequences.push(consumer.await.unwrap());
        }

        for sequence in &sequences {
            assert!(
                sequence.len() > CUTS,
                "the senders should have stamped batches throughout"
            );
            assert_eq!(
                sequence.last().map(|(_, generation)| *generation),
                Some(CUTS as u64),
                "the last batches belong to the last generation"
            );
            for (admitted_at, generation) in sequence {
                let index = *generation as usize;
                let from = index.checked_sub(1).map_or(AdmissionTime::UNIX_EPOCH, |cut| cuts[cut]);
                let until = cuts.get(index).copied();
                assert!(
                    *admitted_at >= from && until.is_none_or(|until| *admitted_at < until),
                    "generation {} got a batch admitted at {}, outside [{}, {:?})",
                    generation,
                    admitted_at,
                    from,
                    until
                );
            }
        }
        assert_eq!(sequences[0], sequences[1]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_transaction_that_times_out_releases_every_lock_it_took() {
        let schedule = AdmissionSchedule::Steady(generation(1)).into_handle();
        let wall = ManualWallClock::new(secs(100));
        let registry = AdmissionRegistry::new();
        let free = registered(&registry, 1, &schedule, &wall);
        let blocked = registered(&registry, 2, &schedule, &wall);
        let (dispatcher, mut receivers) = wired(&free, 1, 4);
        let busy = Busy::new(&blocked).await;

        let started = Instant::now();
        let refused = registry
            .transaction(&[id(1), id(2)], Duration::from_millis(100))
            .await
            .unwrap_err();
        assert_eq!(
            refused,
            TransactionError::Timeout {
                point: id(2),
                timeout: Duration::from_millis(100),
            }
        );
        assert_eq!(started.elapsed(), Duration::from_millis(100));
        assert_eq!(
            refused.to_string(),
            "Timed out after 100ms waiting for the lock of admission point 2, whose output is waiting for room \
             downstream. Retry once downstream drains."
        );

        // The first point, locked while the transaction waited for the second, is free again and stamps at once.
        assert!(is_free(&registry, 1).await);
        assert_eq!(
            send_at(&dispatcher, &mut receivers[0], &wall, secs(101), batch(1)).await,
            Received::data(secs(101), 1, 0, 1, 1)
        );

        // Once the busy point drains, the same transaction takes both locks.
        busy.release().await;
        let transaction = registry
            .transaction(&[id(1), id(2)], Duration::from_millis(100))
            .await
            .unwrap();
        assert_eq!(
            transaction
                .statuses()
                .iter()
                .map(|(point, _)| *point)
                .collect::<Vec<_>>(),
            vec![id(1), id(2)]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_transaction_takes_its_locks_one_at_a_time_in_the_order_given() {
        let schedule = AdmissionSchedule::Steady(generation(1)).into_handle();
        let wall = ManualWallClock::new(secs(100));
        let registry = AdmissionRegistry::new();
        // The registry holds points weakly, as the dispatchers of a running topology would hold them strongly.
        let points = [1, 2, 3].map(|point| registered(&registry, point, &schedule, &wall));
        let busy = Busy::new(&points[1]).await;

        let transaction = tokio::spawn({
            let registry = registry.clone();
            async move {
                registry
                    .transaction(&[id(3), id(2), id(1)], Duration::from_secs(10))
                    .await
            }
        });
        settle().await;

        // Waiting for the busy point, the transaction holds the point before it and not yet the one after it.
        assert!(!transaction.is_finished());
        assert!(!is_free(&registry, 3).await);
        assert!(is_free(&registry, 1).await);

        busy.release().await;
        let transaction = transaction.await.unwrap().unwrap();
        assert_eq!(
            transaction
                .statuses()
                .iter()
                .map(|(point, _)| *point)
                .collect::<Vec<_>>(),
            vec![id(3), id(2), id(1)]
        );
        for point in [1, 2, 3] {
            assert!(!is_free(&registry, point).await);
        }
        drop(transaction);
        for point in [1, 2, 3] {
            assert!(is_free(&registry, point).await);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn transactions_that_lock_in_opposite_orders_never_deadlock() {
        let schedule = AdmissionSchedule::Steady(generation(1)).into_handle();
        let wall = ManualWallClock::new(secs(100));
        let registry = AdmissionRegistry::new();
        let _points = [1, 2].map(|point| registered(&registry, point, &schedule, &wall));

        // While point 1 is held, `forward` waits for it, and `backward` takes point 2 and then waits for point 1
        // behind `forward`. Once point 1 is free, `forward` takes it and waits for point 2, which `backward` holds
        // while it waits for point 1: each waits for the other.
        let holder = registry.transaction(&[id(1)], Duration::ZERO).await.unwrap();
        let forward = tokio::spawn({
            let registry = registry.clone();
            async move { registry.transaction(&[id(1), id(2)], Duration::from_millis(100)).await }
        });
        settle().await;
        let backward = tokio::spawn({
            let registry = registry.clone();
            async move { registry.transaction(&[id(2), id(1)], Duration::from_secs(10)).await }
        });
        settle().await;
        holder.abandon();
        settle().await;
        assert!(!forward.is_finished() && !backward.is_finished());

        // The shorter wait times out and releases point 1, which lets the other transaction complete.
        assert_eq!(
            forward.await.unwrap().unwrap_err(),
            TransactionError::Timeout {
                point: id(2),
                timeout: Duration::from_millis(100),
            }
        );
        let backward = backward.await.unwrap().unwrap();
        assert_eq!(
            backward.statuses().iter().map(|(point, _)| *point).collect::<Vec<_>>(),
            vec![id(2), id(1)]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn locking_a_busy_unaffected_point_first_keeps_the_affected_point_stamping_while_the_transaction_waits() {
        // Point 1 is affected by a cut; point 2 is not, and its output is blocked on a full channel.
        struct Case {
            lock_order: &'static [u32],
            affected_stamps_while_waiting: bool,
        }
        let cases = [
            // Arming locks only the affected point, and completes at once.
            Case {
                lock_order: &[1],
                affected_stamps_while_waiting: true,
            },
            // Cancelling locks the unaffected point first: the affected one is not held while the transaction waits.
            Case {
                lock_order: &[2, 1],
                affected_stamps_while_waiting: true,
            },
            // The other order would hold the affected point, and stall its source, for as long as the wait lasts.
            Case {
                lock_order: &[1, 2],
                affected_stamps_while_waiting: false,
            },
        ];

        for case in cases {
            let schedule = AdmissionSchedule::Steady(generation(1)).into_handle();
            let wall = ManualWallClock::new(secs(100));
            let registry = AdmissionRegistry::new();
            let affected = registered(&registry, 1, &schedule, &wall);
            let unaffected = registered(&registry, 2, &schedule, &wall);
            let (dispatcher, mut receivers) = wired(&affected, 1, 4);
            let dispatcher = Arc::new(dispatcher);
            let busy = Busy::new(&unaffected).await;

            let started = Instant::now();
            let transaction = tokio::spawn({
                let registry = registry.clone();
                let lock_order = case.lock_order.iter().copied().map(id).collect::<Vec<_>>();
                async move { registry.transaction(&lock_order, Duration::from_secs(1)).await }
            });
            settle().await;

            if case.lock_order.contains(&2) {
                assert!(
                    !transaction.is_finished(),
                    "{:?}: waits for the busy point",
                    case.lock_order
                );
                let stamped = tokio::spawn({
                    let dispatcher = Arc::clone(&dispatcher);
                    async move { dispatcher.dispatch(batch(1)).await }
                });
                settle().await;
                assert_eq!(
                    stamped.is_finished(),
                    case.affected_stamps_while_waiting,
                    "{:?}",
                    case.lock_order
                );
                busy.release().await;
                let transaction = transaction.await.unwrap().unwrap();
                drop(transaction);
                stamped.await.unwrap().unwrap();
            } else {
                let transaction = transaction.await.unwrap().unwrap();
                assert!(started.elapsed() < Duration::from_millis(10), "arming does not wait");
                assert_eq!(registry.status_within(id(2), Duration::from_millis(10)).await, None);
                drop(transaction);
                busy.release().await;
                dispatcher.dispatch(batch(1)).await.unwrap();
            }
            assert_eq!(receivers[0].try_recv().unwrap().len(), 1, "{:?}", case.lock_order);
        }
    }

    #[tokio::test]
    async fn a_transaction_that_names_an_unknown_or_repeated_point_takes_no_lock() {
        let schedule = AdmissionSchedule::Steady(generation(1)).into_handle();
        let wall = ManualWallClock::default();
        let registry = AdmissionRegistry::new();
        let _point = registered(&registry, 1, &schedule, &wall);

        let cases = [
            (vec![id(1), id(7)], TransactionError::UnknownPoint { point: id(7) }),
            (vec![id(1), id(1)], TransactionError::DuplicatePoint { point: id(1) }),
        ];
        for (lock_order, expected) in cases {
            let refused = registry.transaction(&lock_order, Duration::from_secs(1)).await;
            assert_eq!(refused.unwrap_err(), expected);
            assert!(is_free(&registry, 1).await, "{:?}", lock_order);
        }
        assert_eq!(
            TransactionError::UnknownPoint { point: id(7) }.to_string(),
            "Admission point 7 is not registered. Configure every point a transaction names with the registry."
        );
        assert_eq!(
            TransactionError::DuplicatePoint { point: id(1) }.to_string(),
            "Admission point 1 is listed more than once in the transaction's lock order. List each point once."
        );

        // A transaction over no point holds nothing and publishes nothing.
        let empty = registry.transaction(&[], Duration::ZERO).await.unwrap();
        assert!(empty.statuses().is_empty());
        empty.publish(AdmissionSchedule::Steady(generation(2)));
        assert_eq!(schedule.load().generation_for(secs(0), 0, 0).id(), GenerationId::new(1));
    }

    #[tokio::test]
    async fn a_point_passes_a_cut_by_time_by_reset_or_by_heartbeat() {
        #[derive(Clone, Copy, Debug)]
        enum Stamp {
            Data(u64),
            Heartbeat(u64),
        }
        use Stamp::{Data, Heartbeat};

        // After a batch at 7,000 under a steady schedule, a cut at 7,100 is published, and the point stamps the given
        // sequence: (what the point stamps, admitted to the new generation, passed the cut).
        let cases: [(&[Stamp], bool, bool); 9] = [
            (&[], false, false),
            (&[Data(7_099)], false, false),
            (&[Heartbeat(7_099)], false, false),
            (&[Data(7_100)], true, true),
            (&[Heartbeat(7_100)], true, true),
            (&[Heartbeat(7_200), Data(7_201)], true, true),
            // A backward jump within the threshold is absorbed: the clock stays at 7,050, before the cut.
            (&[Data(7_050), Data(6_900)], false, false),
            // A larger one resets the clock after the point saw the cut, which reaches it, found by data or by a
            // heartbeat.
            (&[Data(7_050), Data(6_000)], true, true),
            (&[Heartbeat(6_000)], true, true),
        ];

        for (stamps, admitted_to_new, passed_cut) in cases {
            let schedule = AdmissionSchedule::Steady(generation(1)).into_handle();
            let wall = ManualWallClock::default();
            let point = point(1, AdmissionMode::Admit, &schedule, &wall);
            let (dispatcher, mut receivers) = wired(&point, 1, 8);
            send_at(&dispatcher, &mut receivers[0], &wall, secs(7_000), batch(1)).await;
            schedule.store(Arc::new(AdmissionSchedule::Cut {
                old: generation(1),
                new: generation(2),
                at: secs(7_100),
            }));

            for stamp in stamps {
                match *stamp {
                    Data(at) => {
                        send_at(&dispatcher, &mut receivers[0], &wall, secs(at), batch(1)).await;
                    }
                    Heartbeat(at) => {
                        wall.set(secs(at));
                        assert_eq!(point.heartbeat().await, HeartbeatOutcome::Sent);
                    }
                }
            }
            let status = point.status().await;
            assert_eq!(
                (status.admitted_to_new, status.passed_cut),
                (admitted_to_new, passed_cut),
                "{:?}",
                stamps
            );
        }
    }

    #[tokio::test]
    async fn the_epoch_a_transaction_reports_is_the_one_the_point_records_when_it_next_stamps() {
        // A point whose clock reset once is in epoch 1 when a cut at 4,000 is armed. Its next stamp records that epoch
        // as the one it saw the cut in, which is what the new generation's boundary holds, also when that very stamp
        // resets the clock again.
        let cases = [
            (secs(3_700), Received::data(secs(3_700), 1, 1, 1, 1)),
            (secs(100), Received::data(secs(100), 1, 2, 2, 1)),
        ];
        for (next_stamp, expected) in cases {
            let old = generation(1);
            let new = generation(2);
            let schedule = AdmissionSchedule::Steady(Arc::clone(&old)).into_handle();
            let wall = ManualWallClock::default();
            let registry = AdmissionRegistry::new();
            let point = registered(&registry, 1, &schedule, &wall);
            let (dispatcher, mut receivers) = wired(&point, 1, 8);
            send_at(&dispatcher, &mut receivers[0], &wall, secs(7_203), batch(1)).await;
            send_at(&dispatcher, &mut receivers[0], &wall, secs(3_603), batch(1)).await;

            let transaction = registry.transaction(&[id(1)], Duration::from_secs(1)).await.unwrap();
            let (_, status) = transaction.statuses()[0];
            assert_eq!(status.epoch, 1);
            new.set_cut(CutBoundary {
                at: secs(4_000),
                replaces: old.id(),
                epochs: SmallVec::from_slice(&[(id(1), status.epoch)]),
            })
            .unwrap();
            transaction.publish(AdmissionSchedule::Cut {
                old: Arc::clone(&old),
                new: Arc::clone(&new),
                at: secs(4_000),
            });

            assert_eq!(
                send_at(&dispatcher, &mut receivers[0], &wall, next_stamp, batch(1)).await,
                expected
            );
            let recorded = point
                .state
                .lock()
                .await
                .observed
                .as_ref()
                .map(|observed| observed.epoch_at_publish);
            assert_eq!(
                recorded,
                new.cut().unwrap().epoch_of(id(1)),
                "next stamp at {}",
                next_stamp
            );
        }
    }

    #[tokio::test]
    async fn heartbeats_go_only_to_their_recipients_while_data_goes_to_every_component() {
        let schedule = AdmissionSchedule::Steady(generation(1)).into_handle();
        let wall = ManualWallClock::default();
        let owner = ComponentId::try_from("owner").unwrap();
        let tap = ComponentId::try_from("tap").unwrap();
        let config = AdmissionPointConfig::new(id(1), AdmissionMode::Admit, Arc::clone(&schedule), THRESHOLD)
            .with_wall_clock(wall.clone())
            .with_heartbeat_recipients([owner.clone()]);
        let point = Arc::new(AdmissionPoint::new(config.clone()));
        assert_eq!(point.heartbeat_recipients(), std::slice::from_ref(&owner));

        let mut dispatcher = Dispatcher::new(ComponentContext::test_source("admission_test"));
        dispatcher.add_output(OutputName::Default).unwrap();
        let mut receivers = Vec::new();
        for component in [&tap, &owner] {
            let (sender, receiver) = mpsc::channel(4);
            dispatcher
                .attach_component_sender_to_output(&OutputName::Default, component.clone(), sender)
                .unwrap();
            receivers.push(receiver);
        }
        dispatcher
            .set_admission_point(&OutputName::Default, Arc::clone(&point))
            .unwrap();

        wall.set(secs(10));
        dispatcher.dispatch(batch(1)).await.unwrap();
        wall.set(secs(11));
        assert_eq!(point.heartbeat().await, HeartbeatOutcome::Sent);
        assert_eq!(drain(&mut receivers[0]), vec![Received::data(secs(10), 1, 0, 1, 1)]);
        assert_eq!(
            drain(&mut receivers[1]),
            vec![
                Received::data(secs(10), 1, 0, 1, 1),
                Received::heartbeat(secs(11), 1, 0, 1)
            ]
        );

        // A recipient that the output has no sender to is refused, whether the output has no sender for it or only
        // senders whose component is unknown.
        for known in [true, false] {
            let mut dispatcher = Dispatcher::new(ComponentContext::test_source("admission_test"));
            dispatcher.add_output(OutputName::Default).unwrap();
            let (sender, _receiver) = mpsc::channel(4);
            if known {
                dispatcher
                    .attach_component_sender_to_output(&OutputName::Default, tap.clone(), sender)
                    .unwrap();
            } else {
                dispatcher
                    .attach_sender_to_output(&OutputName::Default, sender)
                    .unwrap();
            }
            let error = dispatcher
                .set_admission_point(&OutputName::Default, Arc::new(AdmissionPoint::new(config.clone())))
                .unwrap_err();
            assert_eq!(
                error.to_string(),
                "Admission point 1 sends heartbeats to 'owner', but output '_default' has no sender to it. Attach a \
                 sender for every heartbeat recipient before setting the admission point."
            );
        }
    }

    #[test]
    fn a_configuration_records_its_registry_and_heartbeat_recipients() {
        let schedule = AdmissionSchedule::Steady(generation(1)).into_handle();
        let config = AdmissionPointConfig::new(AdmissionPointId::new(1), AdmissionMode::Admit, schedule, THRESHOLD);
        assert!(config.registry().is_none());
        assert!(config.heartbeat_recipients().is_empty());

        let recipients = [
            ComponentId::try_from("state_owner").unwrap(),
            ComponentId::try_from("another_owner").unwrap(),
        ];
        let config = config
            .with_registry(AdmissionRegistry::new())
            .with_heartbeat_recipients(recipients.clone());
        assert!(config.registry().is_some());
        assert_eq!(config.heartbeat_recipients(), &recipients);
    }

    #[tokio::test]
    async fn heartbeats_are_ordered_with_data_and_carry_the_generation_the_schedule_assigns() {
        let schedule = AdmissionSchedule::Steady(generation(1)).into_handle();
        let wall = ManualWallClock::default();
        let point = point(6, AdmissionMode::Admit, &schedule, &wall);
        let (dispatcher, mut receivers) = wired(&point, 2, 8);

        wall.set(secs(10));
        dispatcher.dispatch(batch(2)).await.unwrap();
        wall.set(secs(11));
        assert_eq!(point.heartbeat().await, HeartbeatOutcome::Sent);
        wall.set(secs(12));
        dispatcher.dispatch(batch(1)).await.unwrap();

        // Under a cut, a heartbeat gets the old generation before the cut and the new one from it, as data does, and
        // a heartbeat at the cut is what admits the point to the new generation.
        schedule.store(Arc::new(AdmissionSchedule::Cut {
            old: generation(1),
            new: generation(2),
            at: secs(20),
        }));
        wall.set(secs(19));
        assert_eq!(point.heartbeat().await, HeartbeatOutcome::Sent);
        assert!(!point.status().await.admitted_to_new);
        wall.set(secs(20));
        assert_eq!(point.heartbeat().await, HeartbeatOutcome::Sent);
        let status = point.status().await;
        assert!(status.admitted_to_new && status.passed_cut);
        wall.set(secs(21));
        dispatcher.dispatch(batch(1)).await.unwrap();

        for receiver in &mut receivers {
            assert_eq!(
                drain(receiver),
                vec![
                    Received::data(secs(10), 6, 0, 1, 2),
                    Received::heartbeat(secs(11), 6, 0, 1),
                    Received::data(secs(12), 6, 0, 1, 1),
                    Received::heartbeat(secs(19), 6, 0, 1),
                    Received::heartbeat(secs(20), 6, 0, 2),
                    Received::data(secs(21), 6, 0, 2, 1),
                ]
            );
        }
    }

    #[tokio::test]
    async fn heartbeats_reach_no_channel_once_the_senders_or_receivers_are_gone() {
        let schedule = AdmissionSchedule::Steady(generation(1)).into_handle();
        let wall = ManualWallClock::new(secs(10));

        // A point that is not attached to any output has nowhere to send.
        let detached = point(1, AdmissionMode::Admit, &schedule, &wall);
        assert_eq!(detached.heartbeat().await, HeartbeatOutcome::Closed);

        // Once the dispatcher, which owns the output's senders, is gone, nothing is stamped or sent.
        let stopped = point(2, AdmissionMode::Admit, &schedule, &wall);
        let (dispatcher, mut receivers) = wired(&stopped, 1, 4);
        drop(dispatcher);
        assert_eq!(stopped.heartbeat().await, HeartbeatOutcome::Closed);
        assert_eq!(stopped.state.lock().await.clock.latest(), None);
        assert!(
            receivers[0].recv().await.is_none(),
            "the point must not keep the channel open"
        );

        // When every receiver is gone, the heartbeat is stamped but delivered nowhere.
        let orphaned = point(3, AdmissionMode::Admit, &schedule, &wall);
        let (_dispatcher, receivers) = wired(&orphaned, 2, 4);
        drop(receivers);
        assert_eq!(orphaned.heartbeat().await, HeartbeatOutcome::Closed);

        // Several outputs can feed one component, so another component may keep the channel open: once the point's
        // own dispatcher is gone, nothing is stamped or sent all the same.
        let shared = point(4, AdmissionMode::Admit, &schedule, &wall);
        let (sender, mut receiver) = mpsc::channel(4);
        let sibling = sender.clone();
        let mut dispatcher = Dispatcher::new(ComponentContext::test_source("admission_test"));
        dispatcher.add_output(OutputName::Default).unwrap();
        dispatcher
            .attach_sender_to_output(&OutputName::Default, sender)
            .unwrap();
        dispatcher
            .set_admission_point(&OutputName::Default, Arc::clone(&shared))
            .unwrap();
        drop(dispatcher);
        assert_eq!(shared.heartbeat().await, HeartbeatOutcome::Closed);
        assert_eq!(shared.state.lock().await.clock.latest(), None);
        assert!(receiver.try_recv().is_err(), "nothing was sent");
        drop(sibling);
        assert!(
            receiver.recv().await.is_none(),
            "the point must not keep the channel open"
        );
    }

    #[test]
    fn an_admission_point_belongs_to_one_output() {
        let schedule = AdmissionSchedule::Steady(generation(1)).into_handle();
        let point = point(5, AdmissionMode::Admit, &schedule, &ManualWallClock::default());
        let (_dispatcher, _receivers) = wired(&point, 1, 1);

        let mut other = Dispatcher::<EventsBuffer>::new(ComponentContext::test_source("other"));
        other.add_output(OutputName::Default).unwrap();
        let error = other.set_admission_point(&OutputName::Default, point).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Admission point 5 is already attached to an output. Give every output its own admission point."
        );
    }

    #[tokio::test]
    async fn replaced_schedules_do_not_keep_their_generations_alive() {
        let retired = Arc::new(StdMutex::new(Vec::new()));
        let notifier = {
            let retired = Arc::clone(&retired);
            RetireNotifier::new(move |id| retired.lock().unwrap().push(id))
        };
        let first = Arc::new(GenerationToken::with_retire_notifier(GenerationId::new(1), notifier));
        let schedule = AdmissionSchedule::Steady(first).into_handle();
        let wall = ManualWallClock::new(secs(1));
        let point = point(1, AdmissionMode::Admit, &schedule, &wall);
        let (dispatcher, mut receivers) = wired(&point, 1, 4);

        dispatcher.dispatch(batch(1)).await.unwrap();
        schedule.store(Arc::new(AdmissionSchedule::Steady(generation(2))));

        // The point last read the old schedule, but holds it only weakly: the batch in flight is the last holder.
        let in_flight = receivers[0].try_recv().unwrap();
        assert!(retired.lock().unwrap().is_empty());
        drop(in_flight);
        assert_eq!(*retired.lock().unwrap(), vec![GenerationId::new(1)]);
    }

    /// A wall clock that moves forward by one nanosecond every time it is read.
    #[derive(Debug, Default)]
    struct TickingWallClock {
        nanos: AtomicU64,
    }

    impl WallClock for TickingWallClock {
        fn now(&self) -> AdmissionTime {
            AdmissionTime::from_unix_nanos(self.nanos.fetch_add(1, Ordering::SeqCst) + 1)
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_senders_deliver_in_admission_time_order_on_every_channel() {
        const TASKS: usize = 8;
        const BATCHES_PER_TASK: usize = 50;
        const HEARTBEATS: usize = 50;

        let schedule = AdmissionSchedule::Steady(generation(1)).into_handle();
        let point = point_with(1, AdmissionMode::Admit, &schedule, TickingWallClock::default(), None);

        // Channels of capacity one make sends wait often, which interleaves the tasks' fan-outs.
        let (dispatcher, receivers) = wired(&point, 3, 1);
        let dispatcher = Arc::new(dispatcher);

        let consumers = receivers
            .into_iter()
            .map(|mut receiver| {
                tokio::spawn(async move {
                    let mut received = Vec::new();
                    while let Some(buffer) = receiver.recv().await {
                        let header = buffer.header();
                        received.push((header.admitted_at.unwrap(), !buffer.is_empty()));
                    }
                    received
                })
            })
            .collect::<Vec<_>>();

        let mut senders = (0..TASKS)
            .map(|_| {
                let dispatcher = Arc::clone(&dispatcher);
                tokio::spawn(async move {
                    for _ in 0..BATCHES_PER_TASK {
                        dispatcher.dispatch(batch(1)).await.unwrap();
                    }
                })
            })
            .collect::<Vec<_>>();
        senders.push({
            let point = Arc::clone(&point);
            tokio::spawn(async move {
                for _ in 0..HEARTBEATS {
                    assert_eq!(point.heartbeat().await, HeartbeatOutcome::Sent);
                    tokio::task::yield_now().await;
                }
            })
        });

        for sender in senders {
            sender.await.unwrap();
        }
        // Dropping the last dispatcher closes the channels, which ends the consumers.
        drop(dispatcher);

        let mut sequences = Vec::new();
        for consumer in consumers {
            sequences.push(consumer.await.unwrap());
        }

        let expected_len = TASKS * BATCHES_PER_TASK + HEARTBEATS;
        for sequence in &sequences {
            assert_eq!(sequence.len(), expected_len);
            assert!(
                sequence.windows(2).all(|pair| pair[0].0 < pair[1].0),
                "every channel must receive admission times in increasing order"
            );
            assert_eq!(sequence.iter().filter(|(_, data)| !data).count(), HEARTBEATS);
        }
        assert_eq!(sequences[0], sequences[1]);
        assert_eq!(sequences[0], sequences[2]);
    }

    #[tokio::test(start_paused = true)]
    async fn the_heartbeat_driver_sends_only_while_the_point_is_idle() {
        let schedule = AdmissionSchedule::Steady(generation(1)).into_handle();
        let wall = ManualWallClock::new(secs(100));
        let point = point_with(
            1,
            AdmissionMode::Admit,
            &schedule,
            wall.clone(),
            Some(Duration::from_secs(1)),
        );
        let (dispatcher, mut receivers) = wired(&point, 1, 16);
        let driver = tokio::spawn({
            let point = Arc::clone(&point);
            async move { point.run_heartbeats().await }
        });

        // Data at 0.5 s keeps the point busy until 1.5 s, so the driver's first check at 1 s sends nothing.
        tokio::time::sleep(Duration::from_millis(500)).await;
        dispatcher.dispatch(batch(1)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert_eq!(drain(&mut receivers[0]), vec![Received::data(secs(100), 1, 0, 1, 1)]);

        // At 1.5 s the point has been idle for a full period, and again at 2.5 s.
        wall.set(secs(101));
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(drain(&mut receivers[0]), vec![Received::heartbeat(secs(101), 1, 0, 1)]);
        wall.set(secs(102));
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(drain(&mut receivers[0]), vec![Received::heartbeat(secs(102), 1, 0, 1)]);

        drop(dispatcher);
        tokio::time::timeout(Duration::from_secs(5), driver)
            .await
            .expect("the driver should stop once the senders are gone")
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn the_heartbeat_driver_stops_when_the_senders_drop_and_keeps_no_channel_open() {
        let schedule = AdmissionSchedule::Steady(generation(1)).into_handle();
        let wall = ManualWallClock::new(secs(100));
        let point = point_with(
            1,
            AdmissionMode::Admit,
            &schedule,
            wall.clone(),
            Some(Duration::from_secs(1)),
        );
        let (dispatcher, mut receivers) = wired(&point, 1, 16);
        let driver = tokio::spawn({
            let point = Arc::clone(&point);
            async move { point.run_heartbeats().await }
        });

        tokio::time::sleep(Duration::from_millis(1_100)).await;
        assert_eq!(drain(&mut receivers[0]), vec![Received::heartbeat(secs(100), 1, 0, 1)]);

        // With the component's senders gone, the channel closes at once, while the driver is still waiting.
        drop(dispatcher);
        assert!(receivers[0].recv().await.is_none());
        assert!(!driver.is_finished());

        // The driver notices at its next heartbeat and stops.
        tokio::time::timeout(Duration::from_secs(5), driver)
            .await
            .expect("the driver should stop once the senders are gone")
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn the_heartbeat_driver_stops_with_its_component_while_another_feeds_the_same_channel() {
        let schedule = AdmissionSchedule::Steady(generation(1)).into_handle();
        let wall = ManualWallClock::new(secs(100));
        let point = point_with(
            1,
            AdmissionMode::Admit,
            &schedule,
            wall.clone(),
            Some(Duration::from_secs(1)),
        );

        // The downstream component's channel, which a sibling upstream component also sends to, as in a fan-in.
        let (sender, mut receiver) = mpsc::channel(16);
        let sibling = sender.clone();
        let mut dispatcher = Dispatcher::new(ComponentContext::test_source("admission_test"));
        dispatcher.add_output(OutputName::Default).unwrap();
        dispatcher
            .attach_sender_to_output(&OutputName::Default, sender)
            .unwrap();
        dispatcher
            .set_admission_point(&OutputName::Default, Arc::clone(&point))
            .unwrap();
        let driver = tokio::spawn({
            let point = Arc::clone(&point);
            async move { point.run_heartbeats().await }
        });

        tokio::time::sleep(Duration::from_millis(1_100)).await;
        assert_eq!(drain(&mut receiver), vec![Received::heartbeat(secs(100), 1, 0, 1)]);

        // The point's component stops; the sibling keeps the channel open.
        drop(dispatcher);
        wall.set(secs(101));
        tokio::time::timeout(Duration::from_secs(5), driver)
            .await
            .expect("the driver should stop once its component is gone")
            .unwrap();
        assert_eq!(
            drain(&mut receiver),
            vec![],
            "no heartbeat is sent for a stopped component"
        );

        drop(sibling);
        assert!(
            receiver.recv().await.is_none(),
            "the point must not keep the channel open"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_heartbeat_period_too_long_to_schedule_never_comes_due() {
        // A topology rejects such a period; a point built directly must still not panic.
        let schedule = AdmissionSchedule::Steady(generation(1)).into_handle();
        let wall = ManualWallClock::new(secs(100));
        let point = point_with(
            1,
            AdmissionMode::Admit,
            &schedule,
            wall.clone(),
            Some(Duration::from_secs(u64::MAX)),
        );
        let (_dispatcher, mut receivers) = wired(&point, 1, 4);
        let driver = tokio::spawn({
            let point = Arc::clone(&point);
            async move { point.run_heartbeats().await }
        });

        tokio::time::sleep(Duration::from_secs(86_400)).await;
        assert!(!driver.is_finished(), "the driver neither panics nor stops");
        assert_eq!(drain(&mut receivers[0]), vec![]);
        driver.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn the_heartbeat_driver_stops_on_outputs_without_channels_and_is_off_by_default() {
        let schedule = AdmissionSchedule::Steady(generation(1)).into_handle();
        let wall = ManualWallClock::new(secs(100));

        let unconnected = point_with(
            1,
            AdmissionMode::Admit,
            &schedule,
            wall.clone(),
            Some(Duration::from_secs(1)),
        );
        let (_dispatcher, _receivers) = wired(&unconnected, 0, 1);
        tokio::time::timeout(Duration::from_secs(5), unconnected.run_heartbeats())
            .await
            .expect("a driver with no channels should stop after one period");

        let disabled = point(2, AdmissionMode::Admit, &schedule, &wall);
        assert_eq!(disabled.heartbeat_period(), None);
        assert_eq!(disabled.run_heartbeats().now_or_never(), Some(()));
    }
}
