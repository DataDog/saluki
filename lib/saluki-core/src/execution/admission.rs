//! Admission points and schedules.

use std::{
    sync::{Arc, OnceLock, Weak},
    time::Duration,
};

use arc_swap::ArcSwap;
use saluki_error::{generic_error, GenericError};
use tokio::{
    sync::{mpsc, Mutex, MutexGuard},
    time::Instant,
};
use tracing::warn;

use super::{
    clock::{AdmissionClock, ClockStamp, SystemWallClock, WallClock},
    generation::{GenerationId, GenerationRef},
    header::{AdmissionPointId, AdmissionTime, BatchHeader},
};
use crate::topology::EventsBuffer;

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
    wall_clock: Arc<dyn WallClock>,
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
            wall_clock: Arc::new(SystemWallClock),
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

    /// Makes the point's clock read the given wall clock instead of the operating system's.
    pub fn with_wall_clock<W>(mut self, wall_clock: W) -> Self
    where
        W: WallClock + 'static,
    {
        self.wall_clock = Arc::new(wall_clock);
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
}

/// The result of sending a heartbeat.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use]
pub enum HeartbeatOutcome {
    /// The heartbeat reached at least one downstream channel.
    Sent,

    /// The heartbeat reached no downstream channel, because the output has none, the component that owns the output
    /// has stopped, or every downstream component has stopped.
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

    /// Whether the point has stamped a batch with the `new` generation of the cut the schedule holds.
    ///
    /// A point moves to `new` either by stamping an admission time at or after the cut, or because its clock was reset
    /// after it first saw a cut to `new`. Either way the batch was admitted to `new`, so cancelling the cut, or
    /// replacing it with one whose `at` is later than that batch, would move the point back to `old`. Always `false`
    /// under a steady schedule.
    pub admitted_to_new: bool,
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

    /// Returns what the point has stamped, relative to `schedule`.
    fn status(&self, schedule: &AdmissionSchedule) -> AdmissionPointStatus {
        let admitted_to_new = match (schedule, &self.observed) {
            (AdmissionSchedule::Cut { new, .. }, Some(observed)) => {
                observed.new == new.id() && observed.admitted_to_new
            }
            _ => false,
        };
        AdmissionPointStatus {
            last_admitted_at: self.clock.latest(),
            epoch: self.clock.epoch(),
            admitted_to_new,
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
/// An idle admission point can send **heartbeats**: header-only batches that carry an admission time, epoch, and
/// admission point, but no generation, so downstream state can tell that nothing earlier is still on its way.
/// Heartbeats go through the output's own downstream channels, under the same mutex, so their order relative to data
/// is the order in which they were stamped. The point holds those channels only weakly, so it never keeps a channel
/// open, and it sends no heartbeat once the component that owns the output has stopped, even while other components
/// still send to the same channels.
#[derive(Debug)]
pub struct AdmissionPoint {
    id: AdmissionPointId,
    mode: AdmissionMode,
    heartbeat: Option<Duration>,
    schedule: ScheduleHandle,
    state: Mutex<PointState>,
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
            wall_clock,
        } = config;

        Self {
            id,
            mode,
            heartbeat,
            schedule,
            state: Mutex::new(PointState {
                clock: AdmissionClock::with_wall_clock(clock_reset_threshold, wall_clock),
                observed: None,
                last_activity: Instant::now(),
            }),
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

    /// Returns what this point has stamped, relative to the schedule it stamps from.
    ///
    /// It waits for any dispatch or heartbeat in progress on the point to finish.
    pub async fn status(&self) -> AdmissionPointStatus {
        let state = self.state.lock().await;
        state.status(&self.schedule.load())
    }

    /// Attaches the downstream channels of the output this point belongs to, and returns the token the output's
    /// dispatcher holds while its component runs.
    ///
    /// The point keeps only weak references to the channels, so it never keeps a channel open by itself, and sends no
    /// heartbeat once the token is dropped.
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

    /// Sends a heartbeat to every downstream channel of this point's output.
    ///
    /// The heartbeat is an empty batch whose header carries a fresh admission time, the clock's epoch, and this point's
    /// identifier, but no generation. It waits for room in each channel in turn, exactly as data does, and holds the
    /// point's mutex while doing so.
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
                let (stamp, _) = state.advance(self.id, &schedule);
                BatchHeader {
                    admitted_at: Some(stamp.admitted_at),
                    admission_point: Some(self.id),
                    epoch: stamp.epoch,
                    generation: None,
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
            let due = heartbeat_due(self.state.lock().await.last_activity, period);
            tokio::time::sleep_until(due).await;

            let mut state = self.state.lock().await;
            if heartbeat_due(state.last_activity, period) > Instant::now() {
                // Something was stamped while we slept, so the point is not idle.
                continue;
            }

            if self.heartbeat_locked(&mut state).await == HeartbeatOutcome::Closed {
                return;
            }
        }
    }
}

/// Returns when a heartbeat is due: one period after the last stamp.
///
/// A period too long to add to the last stamp's instant is due in about 30 years, which is never in practice.
fn heartbeat_due(last_activity: Instant, period: Duration) -> Instant {
    const FAR_FUTURE: Duration = Duration::from_secs(86_400 * 365 * 30);
    last_activity
        .checked_add(period)
        .or_else(|| last_activity.checked_add(FAR_FUTURE))
        .unwrap_or(last_activity)
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
        let (stamp, epoch_at_publish) = self.state.advance(point.id, &schedule);
        let generation = schedule.generation_for(stamp.admitted_at, stamp.epoch, epoch_at_publish);
        if let AdmissionSchedule::Cut { new, .. } = &**schedule {
            if Arc::ptr_eq(generation, new) {
                self.state.observe_cut(new).admitted_to_new = true;
            }
        }

        header.admitted_at = Some(stamp.admitted_at);
        header.admission_point = Some(point.id);
        header.epoch = stamp.epoch;
        header.generation = Some(Arc::clone(generation));
        if point.mode == AdmissionMode::Readmit {
            header.entry = None;
        }
    }
}

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
            generation::{GenerationId, GenerationToken, RetireNotifier},
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

        fn heartbeat(admitted_at: AdmissionTime, point: u32, epoch: u32) -> Self {
            Self {
                admitted_at: Some(admitted_at),
                admission_point: Some(point),
                epoch,
                generation: None,
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
        assert_eq!(drain(&mut receivers[0]), vec![Received::heartbeat(secs(3_603), 1, 1)]);

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
        let status = |last: Option<u64>, epoch, admitted_to_new| AdmissionPointStatus {
            last_admitted_at: last.map(secs),
            epoch,
            admitted_to_new,
        };
        let schedule = AdmissionSchedule::Steady(generation(1)).into_handle();
        let wall = ManualWallClock::default();
        let point = point(1, AdmissionMode::Admit, &schedule, &wall);
        let (dispatcher, mut receivers) = wired(&point, 1, 8);
        let receiver = &mut receivers[0];
        assert_eq!(point.status().await, status(None, 0, false));

        send_at(&dispatcher, receiver, &wall, secs(7_203), batch(1)).await;
        assert_eq!(point.status().await, status(Some(7_203), 0, false));

        // Under the cut, the point admits to the old generation until time reaches it.
        let cut = || AdmissionSchedule::Cut {
            old: generation(1),
            new: generation(2),
            at: secs(8_000),
        };
        schedule.store(Arc::new(cut()));
        send_at(&dispatcher, receiver, &wall, secs(7_210), batch(1)).await;
        assert_eq!(point.status().await, status(Some(7_210), 0, false));

        // A heartbeat that finds a reset admits nothing to the new generation; the next batch does.
        wall.set(secs(3_610));
        assert_eq!(point.heartbeat().await, HeartbeatOutcome::Sent);
        assert_eq!(point.status().await, status(Some(3_610), 1, false));
        send_at(&dispatcher, receiver, &wall, secs(3_611), batch(1)).await;
        assert_eq!(point.status().await, status(Some(3_611), 1, true));

        // Republishing the cut keeps that; a steady schedule, or a cut to another generation, has nothing admitted.
        schedule.store(Arc::new(cut()));
        assert_eq!(point.status().await, status(Some(3_611), 1, true));
        schedule.store(Arc::new(AdmissionSchedule::Steady(generation(2))));
        assert_eq!(point.status().await, status(Some(3_611), 1, false));
        schedule.store(Arc::new(AdmissionSchedule::Cut {
            old: generation(2),
            new: generation(3),
            at: secs(3_620),
        }));
        assert_eq!(point.status().await, status(Some(3_611), 1, false));

        // Reaching the cut by time admits to the new generation too.
        send_at(&dispatcher, receiver, &wall, secs(3_620), batch(1)).await;
        assert_eq!(point.status().await, status(Some(3_620), 1, true));
    }

    #[tokio::test]
    async fn heartbeats_are_ordered_with_data_and_carry_no_generation() {
        let schedule = AdmissionSchedule::Steady(generation(1)).into_handle();
        let wall = ManualWallClock::default();
        let point = point(6, AdmissionMode::Admit, &schedule, &wall);
        let (dispatcher, mut receivers) = wired(&point, 2, 4);

        wall.set(secs(10));
        dispatcher.dispatch(batch(2)).await.unwrap();
        wall.set(secs(11));
        assert_eq!(point.heartbeat().await, HeartbeatOutcome::Sent);
        wall.set(secs(12));
        dispatcher.dispatch(batch(1)).await.unwrap();

        for receiver in &mut receivers {
            assert_eq!(
                drain(receiver),
                vec![
                    Received::data(secs(10), 6, 0, 1, 2),
                    Received::heartbeat(secs(11), 6, 0),
                    Received::data(secs(12), 6, 0, 1, 1),
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
                        received.push((header.admitted_at.unwrap(), header.generation.is_some()));
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
                        dispatcher.dispatch(EventsBuffer::with_capacity(0)).await.unwrap();
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
        assert_eq!(drain(&mut receivers[0]), vec![Received::heartbeat(secs(101), 1, 0)]);
        wall.set(secs(102));
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(drain(&mut receivers[0]), vec![Received::heartbeat(secs(102), 1, 0)]);

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
        assert_eq!(drain(&mut receivers[0]), vec![Received::heartbeat(secs(100), 1, 0)]);

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
        assert_eq!(drain(&mut receiver), vec![Received::heartbeat(secs(100), 1, 0)]);

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
