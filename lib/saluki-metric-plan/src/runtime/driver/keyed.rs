//! Keyed exits: the repartition edge into a state owner that runs on several lanes.
//!
//! At an exit whose target is an isolation slot on several lanes ([`ExitRouting::Keyed`][crate::compile::ExitRouting]),
//! each event goes, by its partition key through the frame's key groups, to the lane that owns it. Events keep their
//! order within a lane, and each lane receives the buffers and progress of every upstream point in stamp order, so a
//! state lane's watermark tracker sees what a state owner on one lane sees, point by point.
//!
//! # Coalescing
//!
//! Splitting every call's events over the lanes would make each state lane pay its per-buffer costs once per call and
//! lane, so the exit keeps one **open** routed buffer per target lane, across calls and across idle input. Events of a
//! call append to their lane's open buffer when its key is the call's: equal generation, epoch, entry, admission point,
//! and admission second. The buffer's header takes the generation once, when it opens, and the admission time of every
//! call that appends to it, so it carries the latest stamp of its events, all of one second. Otherwise the open buffer
//! is sent first and a new one opened, so a buffer never overtakes an earlier one. An open buffer is sent:
//!
//! - when it holds its capacity, `clamp(ROUTE_BUFFER_EVENTS / lanes, 256, 1024)` events;
//! - after a call whose key it does not continue: its second, generation, epoch, or point moved on;
//! - after a call whose admission time is [`PROGRESS_PERIOD`] or more past its own, so that a lane's view of the point
//!   never lags by more than the progress throttle while the second lasts;
//! - after a header-only call, a heartbeat or forwarded progress: every open buffer;
//! - at the end of a turn once it was opened [`COALESCE_HOLD`] ago ([`KeyedExit::end_turn`]), and at finish.
//!
//! A call whose events all go to one lane moves its buffer whole when a routed buffer holds a whole events buffer: it
//! becomes the lane's open buffer when none is open, appends to it when its key is equal, and otherwise follows it.
//! With smaller routed buffers its events are copied, as those of a buffer split over several lanes are, so that no
//! buffer the exit holds or sends has room for more events than a routed buffer, which is what the region's memory
//! bounds and the state lanes' inputs count.
//!
//! Coalescing changes no output: aggregation reads only the admission second, which every event of a routed buffer
//! shares with its own stamp, the generation and the epoch are part of the key, and the header's admission time is a
//! real stamp of the point that is never earlier than an event's own and never later than what the lane receives next.
//!
//! # Progress
//!
//! Progress is judged per target lane against what was actually sent to it, since a sent buffer can carry an older
//! header than the call's. After a call's sends, each target lane with no open buffer receives a header-only copy of
//! the call's header, without its entry, when the call was header-only and its stamp is later than the last one the
//! lane received from the point, when the point's epoch or generation differs from that one's, when its admission
//! second is later, or when its admission time is [`PROGRESS_PERIOD`] or more past it. A generation change therefore
//! reaches every lane at once, so a cut's crossing never waits for the throttle, and so does a new second: every lane
//! with no open buffer holds each point's latest generation, epoch, and admission second, the whole second its tick
//! gate reads, whichever lanes the point's events went to. A lane with an open buffer gets no copy: the buffer carries
//! the header of its last append, which has the point's latest generation, epoch, and second, and goes out within the
//! throttle or the hold.
//!
//! # Epochs
//!
//! When a call's header is in a newer epoch than the previous header the region handled from its point, the exit first
//! sends every open buffer, then a header-only copy of that previous header to every lane whose last header from the
//! point is another, and only then routes the call. No lane therefore receives a newer epoch before the point's final
//! latest stamp in the older one, which a state lane's reset window drains at. At finish the exit likewise sends every
//! open buffer, then each point's last header to every lane whose last header from the point is another, so that every
//! state lane holds each point's true last stamp when its input closes.
//!
//! # Calls
//!
//! What a call writes to the exit is staged while its program runs ([`KeyedExit::stage`]) and routed once the program
//! has run ([`KeyedExit::complete_call`]): a call that panics changes nothing here, and sends nothing an earlier call
//! left open, whose events the host would otherwise discard with the panicking call's output.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use saluki_core::{
    data_model::event::Event,
    execution::{AdmissionPointId, AdmissionTime, BatchHeader, EntryId, ExitId, ExitSink, GenerationId},
    topology::EventsBuffer,
};

pub(crate) use crate::limits::routed_buffer_events;
use crate::{
    compile::KeyGroups,
    limits::values::{COALESCE_HOLD, PROGRESS_PERIOD},
    runtime::state::rules::epoch_newer,
};

/// What a header says about its point's progress: the generation, epoch, and admission time of a stamped header.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SentMark {
    generation: Option<GenerationId>,
    epoch: u32,
    admitted_at: AdmissionTime,
}

impl SentMark {
    /// Returns the mark of a header, or `None` if it carries no admission time.
    pub fn of(header: &BatchHeader) -> Option<Self> {
        header.admitted_at.map(|admitted_at| Self {
            generation: header.generation_id(),
            epoch: header.epoch,
            admitted_at,
        })
    }

    /// Returns the generation the header was stamped with.
    pub fn generation(&self) -> Option<GenerationId> {
        self.generation
    }

    /// Returns the epoch of the point's clock the header was stamped in.
    pub fn epoch(&self) -> u32 {
        self.epoch
    }

    /// Returns the admission time.
    pub fn admitted_at(&self) -> AdmissionTime {
        self.admitted_at
    }
}

/// What events of a routed buffer share: a buffer only takes events of calls with equal keys.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CoalesceKey {
    generation: Option<GenerationId>,
    epoch: u32,
    /// The entry the exit writes, the consumer's, not the region input's.
    entry: Option<EntryId>,
    point: Option<AdmissionPointId>,
    second: Option<u64>,
}

impl CoalesceKey {
    /// Returns the key of a call with header `header` whose events the exit writes with `entry`.
    fn of(header: &BatchHeader, entry: Option<EntryId>) -> Self {
        Self {
            generation: header.generation_id(),
            epoch: header.epoch,
            entry,
            point: header.admission_point,
            second: header.admitted_at.map(AdmissionTime::as_unix_secs),
        }
    }

    /// Returns `true` if a call with header `header` has this key, whatever entry it writes: one generation's program
    /// writes one entry to an exit, so equal generations write equal entries.
    fn continues(&self, header: &BatchHeader) -> bool {
        self.generation == header.generation_id()
            && self.epoch == header.epoch
            && self.point == header.admission_point
            && self.second == header.admitted_at.map(AdmissionTime::as_unix_secs)
    }
}

/// A routed buffer a keyed exit holds open for one target lane.
struct OpenBuffer {
    /// The events, and the header of the latest call that appended to them, with the exit's entry.
    buffer: EventsBuffer,
    key: CoalesceKey,
    /// The position of the buffer's point among the exit's points, if the exit forwards its progress.
    point: Option<usize>,
    /// When the buffer opened, on the host's clock.
    opened_at: Instant,
}

impl OpenBuffer {
    /// Returns `true` if the buffer is to be sent after a call with header `header`: one whose key it does not
    /// continue, or whose admission time is the progress throttle or more past its own.
    fn moved_on(&self, header: &BatchHeader) -> bool {
        if !self.key.continues(header) {
            return true;
        }
        match (self.buffer.header().admitted_at, header.admitted_at) {
            (Some(own), Some(call)) => call >= own.saturating_add(PROGRESS_PERIOD),
            _ => false,
        }
    }
}

/// What the events of one buffer a call wrote share as they are routed.
struct Append<'h> {
    /// The call's header.
    header: &'h BatchHeader,
    key: CoalesceKey,
    /// The position of the header's point among the exit's points, if the exit forwards its progress.
    point: Option<usize>,
    /// The host's clock.
    now: Instant,
}

impl Append<'_> {
    /// Returns the header a buffer the call opens carries: the call's, with the exit's entry.
    fn stamp(&self) -> BatchHeader {
        let mut stamped = self.header.clone();
        stamped.entry = self.key.entry;
        stamped
    }
}

/// What a keyed exit sent since its counts were last taken.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct RoutedCounts {
    /// The routed buffers, which carry events.
    pub buffers: u64,
    /// The events in the routed buffers.
    pub events: u64,
    /// The header-only buffers, which carry a point's progress.
    pub progress: u64,
}

/// What a keyed exit knows of one admission point whose progress it forwards.
struct PointProgress {
    point: AdmissionPointId,
    /// The header of the last call from the point the region completed, without its entry: the point's latest stamp,
    /// which every lane receives before anything of a newer epoch, and at finish.
    last_call: Option<BatchHeader>,
    /// Per target lane, the mark of the last header from the point actually sent to it, by a routed buffer or as
    /// progress.
    sent: Vec<Option<SentMark>>,
}

/// The routing state of one keyed exit of a region: its open routed buffers, and the progress it sent to each lane.
pub(super) struct KeyedExit {
    exit: ExitId,
    groups: Arc<KeyGroups>,
    /// The events a routed buffer holds before it is sent.
    capacity: usize,
    /// The longest a routed buffer is held open.
    hold: Duration,
    /// Each target lane's open buffer.
    open: Vec<Option<OpenBuffer>>,
    /// The admission points whose progress the exit forwards.
    points: Vec<PointProgress>,
    /// What the running call wrote to the exit, with the entry it wrote, routed once its program has run.
    staged: Vec<(Option<EntryId>, EventsBuffer)>,
    /// The lane of each event of the buffer being routed, reused across buffers.
    lanes: Vec<u16>,
    /// What the exit sent since its counts were last taken.
    counts: RoutedCounts,
}

impl KeyedExit {
    /// Creates the routing state of `exit`, which routes through `groups` and forwards the progress of `points`.
    pub fn new(exit: ExitId, groups: Arc<KeyGroups>, points: Vec<AdmissionPointId>) -> Self {
        let lanes = usize::from(groups.lanes());
        Self {
            exit,
            capacity: routed_buffer_events(groups.lanes()),
            hold: COALESCE_HOLD,
            open: (0..lanes).map(|_| None).collect(),
            points: points
                .into_iter()
                .map(|point| PointProgress {
                    point,
                    last_call: None,
                    sent: vec![None; lanes],
                })
                .collect(),
            staged: Vec::new(),
            lanes: Vec::new(),
            counts: RoutedCounts::default(),
            groups,
        }
    }

    /// Sets the number of events a routed buffer holds before it is sent, at least one and at most an events buffer's
    /// capacity, in place of the one the lane count gives.
    #[cfg(test)]
    fn with_capacity(mut self, capacity: usize) -> Self {
        self.capacity = capacity.clamp(1, EventsBuffer::with_capacity(0).capacity());
        self
    }

    /// Returns the exit.
    pub fn exit(&self) -> ExitId {
        self.exit
    }

    /// Returns the number of events a routed buffer holds before it is sent.
    #[cfg(test)]
    fn capacity(&self) -> usize {
        self.capacity
    }

    /// Returns the open buffer of `lane`, if any.
    #[cfg(test)]
    fn open_buffer(&self, lane: u16) -> Option<&EventsBuffer> {
        self.open[usize::from(lane)].as_ref().map(|open| &open.buffer)
    }

    /// Returns the number of events each target lane's open buffer holds, in lane order.
    #[cfg(test)]
    fn open_events(&self) -> Vec<usize> {
        self.open
            .iter()
            .map(|open| open.as_ref().map_or(0, |open| open.buffer.len()))
            .collect()
    }

    /// Returns what the exit sent since this was last called, and starts counting again.
    pub fn take_counts(&mut self) -> RoutedCounts {
        std::mem::take(&mut self.counts)
    }

    /// Keeps a buffer the running call wrote to the exit, with the entry it wrote, until the call's program has run.
    pub fn stage(&mut self, entry: Option<EntryId>, buffer: EventsBuffer) {
        self.staged.push((entry, buffer));
    }

    /// Drops what the running call wrote to the exit, after a panic ended it: its events are lost with the call.
    pub fn abandon_call(&mut self) {
        self.staged.clear();
    }

    /// Completes a call with header `header` once its program has run: sends the final header of the point's older
    /// epoch to every lane that lacks it, routes what the call wrote to the lanes owning each event's key, sends the
    /// open buffers the call moved on from, and forwards the call's progress to every lane with no open buffer that
    /// needs it. `header_only` is whether the call's input was header-only, and `now` the host's clock.
    pub fn complete_call(&mut self, header: &BatchHeader, header_only: bool, now: Instant, exits: &mut ExitSink) {
        let point = self.point_of(header);

        // Nothing of a newer epoch goes out before the point's final header in the older one.
        if let Some(index) = point {
            let newer = self.points[index]
                .last_call
                .as_ref()
                .is_some_and(|last| epoch_newer(header.epoch, last.epoch));
            if newer {
                self.send_open(exits);
                self.send_last_call(index, exits);
            }
        }

        let mut staged = std::mem::take(&mut self.staged);
        for (entry, buffer) in staged.drain(..) {
            self.route(header, entry, buffer, point, now, exits);
        }
        self.staged = staged;

        // An open buffer the call moved on from goes out now, and after header-only input every open buffer does.
        for lane in 0..self.lanes() {
            let moved_on = self.open[usize::from(lane)]
                .as_ref()
                .is_some_and(|open| header_only || open.moved_on(header));
            if moved_on {
                self.send(lane, exits);
            }
        }

        let Some(index) = point else {
            return;
        };
        // `point_of` finds only stamped headers.
        let Some(mark) = SentMark::of(header) else {
            return;
        };
        let mut progress = header.clone();
        progress.entry = None;
        for lane in 0..self.lanes() {
            if self.open[usize::from(lane)].is_some() {
                continue;
            }
            let sent = &mut self.points[index].sent[usize::from(lane)];
            if progress_due(*sent, mark, header_only) {
                exits.emit_to_lane(
                    self.exit,
                    lane,
                    EventsBuffer::with_capacity(0).with_header(progress.clone()),
                );
                *sent = Some(mark);
                self.counts.progress += 1;
            }
        }
        // A header of an epoch older than the point's last, which only a wiring fault produces and every state lane
        // counts as such, is never the point's final header of an epoch.
        let last_call = &mut self.points[index].last_call;
        if last_call
            .as_ref()
            .is_none_or(|last| !epoch_newer(last.epoch, mark.epoch))
        {
            *last_call = Some(progress);
        }
    }

    /// Ends a turn of input: sends the open buffers whose hold expired by `now`, and returns when the next one expires,
    /// if any is still open. The hold runs whether input is idle or not.
    pub fn end_turn(&mut self, now: Instant, exits: &mut ExitSink) -> Option<Instant> {
        let mut next: Option<Instant> = None;
        for lane in 0..self.lanes() {
            let Some(open) = &self.open[usize::from(lane)] else {
                continue;
            };
            let expires = open.opened_at + self.hold;
            if expires <= now {
                self.send(lane, exits);
            } else {
                next = Some(next.map_or(expires, |next| next.min(expires)));
            }
        }
        next
    }

    /// Finishes the exit when the region's input ends: sends every open buffer, then each point's last header to every
    /// lane whose last header from the point is another. Finishing again sends nothing more.
    pub fn finish(&mut self, exits: &mut ExitSink) {
        self.send_open(exits);
        for index in 0..self.points.len() {
            self.send_last_call(index, exits);
        }
    }

    /// Returns the number of lanes the exit routes to.
    fn lanes(&self) -> u16 {
        self.groups.lanes()
    }

    /// Returns the position among the exit's points of the point that stamped `header`, if the exit forwards its
    /// progress and the header is stamped.
    fn point_of(&self, header: &BatchHeader) -> Option<usize> {
        header.admitted_at?;
        let point = header.admission_point?;
        self.points.iter().position(|progress| progress.point == point)
    }

    /// Routes the events of `buffer`, which the call with header `header` wrote with `entry`, into the open buffers of
    /// the lanes that own their keys. `point` is the position of the header's point among the exit's points.
    fn route(
        &mut self, header: &BatchHeader, entry: Option<EntryId>, buffer: EventsBuffer, point: Option<usize>,
        now: Instant, exits: &mut ExitSink,
    ) {
        let call = Append {
            header,
            key: CoalesceKey::of(header, entry),
            point,
            now,
        };
        // The first pass finds each event's lane, and whether they all go to one.
        let mut lanes = std::mem::take(&mut self.lanes);
        lanes.clear();
        lanes.extend((&buffer).into_iter().map(|event| lane_of(&self.groups, event)));
        if let Some(&first) = lanes.first() {
            if lanes.iter().all(|&lane| lane == first) {
                self.move_whole(first, &call, buffer, exits);
            } else {
                for (event, &lane) in buffer.into_iter().zip(&lanes) {
                    self.push(lane, &call, event, exits);
                }
            }
        }
        self.lanes = lanes;
    }

    /// Moves a buffer whose events all go to `lane`: it becomes the lane's open buffer when none is open, appends to
    /// the open one when its key is equal, and otherwise follows it, which is sent first.
    ///
    /// The buffer itself moves only when it has storage for at most a routed buffer's events. No buffer the exit holds
    /// open or sends may have room for more events than a routed buffer, which is what the region's memory bounds and
    /// the state lane's input count (and what that input's consumers check in debug builds). A region's buffer usually
    /// has room for a whole batch, so with routed buffers smaller than a batch its events are copied into routed
    /// buffers, as those of a buffer split over several lanes are.
    fn move_whole(&mut self, lane: u16, call: &Append<'_>, mut buffer: EventsBuffer, exits: &mut ExitSink) {
        let index = usize::from(lane);
        let appends = self.open[index].as_ref().is_some_and(|open| open.key == call.key);
        if appends || buffer.allocated_capacity() > self.capacity {
            for event in buffer {
                self.push(lane, call, event, exits);
            }
            return;
        }
        self.send(lane, exits);
        buffer.set_header(call.stamp());
        let full = buffer.len() >= self.capacity;
        self.open[index] = Some(OpenBuffer {
            buffer,
            key: call.key,
            point: call.point,
            opened_at: call.now,
        });
        if full {
            self.send(lane, exits);
        }
    }

    /// Appends one event of a call to the open buffer of `lane`, sending the open buffer first if its key is another,
    /// and sending it once it is full.
    ///
    /// An open buffer that moved whole may have storage for fewer events than a routed buffer holds; once that storage
    /// is full, it is sent rather than grown, since growing could leave it room for more events than a routed buffer.
    fn push(&mut self, lane: u16, call: &Append<'_>, event: Event, exits: &mut ExitSink) {
        let index = usize::from(lane);
        if self.open[index]
            .as_ref()
            .is_some_and(|open| open.key != call.key || open.buffer.len() == open.buffer.allocated_capacity())
        {
            self.send(lane, exits);
        }
        let capacity = self.capacity;
        let open = self.open[index].get_or_insert_with(|| OpenBuffer {
            buffer: EventsBuffer::with_capacity(capacity).with_header(call.stamp()),
            key: call.key,
            point: call.point,
            opened_at: call.now,
        });
        // The key holds the generation, so an append copies only the admission time, the latest stamp of the buffer.
        open.buffer.header_mut().admitted_at = call.header.admitted_at;
        let rejected = open.buffer.try_push(event);
        debug_assert!(
            rejected.is_none(),
            "an open buffer below its capacity has room for an event"
        );
        if open.buffer.len() >= capacity {
            self.send(lane, exits);
        }
    }

    /// Sends the open buffer of `lane`, if any, and records its header as the last one the lane received from its
    /// point.
    fn send(&mut self, lane: u16, exits: &mut ExitSink) {
        let Some(open) = self.open[usize::from(lane)].take() else {
            return;
        };
        if let Some(point) = open.point {
            self.points[point].sent[usize::from(lane)] = SentMark::of(open.buffer.header());
        }
        self.counts.buffers += 1;
        self.counts.events += open.buffer.len() as u64;
        exits.emit_to_lane(self.exit, lane, open.buffer);
    }

    /// Sends every open buffer.
    fn send_open(&mut self, exits: &mut ExitSink) {
        for lane in 0..self.lanes() {
            self.send(lane, exits);
        }
    }

    /// Sends a header-only copy of the last call's header from the point at `index` to every lane whose last header
    /// from the point is another.
    fn send_last_call(&mut self, index: usize, exits: &mut ExitSink) {
        let exit = self.exit;
        let progress = &mut self.points[index];
        let Some(last) = &progress.last_call else {
            return;
        };
        let mark = SentMark::of(last);
        for (lane, sent) in progress.sent.iter_mut().enumerate() {
            if *sent != mark {
                // Lanes are numbered below the key groups' lane count, a `u16`.
                exits.emit_to_lane(
                    exit,
                    lane as u16,
                    EventsBuffer::with_capacity(0).with_header(last.clone()),
                );
                *sent = mark;
                self.counts.progress += 1;
            }
        }
    }
}

/// Returns `true` if a lane whose last header from a point is `sent` needs a header-only copy of a call's header from
/// it, `mark`: when the call was header-only and its stamp is later, when its epoch or generation is another, when its
/// admission second is later, or when its admission time is the progress throttle or more past the last.
fn progress_due(sent: Option<SentMark>, mark: SentMark, header_only: bool) -> bool {
    let Some(sent) = sent else {
        return true;
    };
    (header_only && mark.admitted_at > sent.admitted_at)
        || mark.epoch != sent.epoch
        || mark.generation != sent.generation
        || mark.admitted_at.as_unix_secs() > sent.admitted_at.as_unix_secs()
        || mark.admitted_at >= sent.admitted_at.saturating_add(PROGRESS_PERIOD)
}

/// Returns the lane an event goes to: the lane owning its context key, and lane 0 for an event that is not a metric,
/// which has no context.
fn lane_of(groups: &KeyGroups, event: &Event) -> u16 {
    event
        .try_as_metric()
        .map_or(0, |metric| groups.lane_of(metric.context().key().as_u64()))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use proptest::prelude::*;
    use saluki_core::{
        data_model::{
            event::metric::{context::Context, Metric},
            tags::TagSet,
        },
        execution::{GenerationRef, GenerationToken},
    };

    use super::*;
    use crate::runtime::state::rules::{Observation, WatermarkTracker};

    const EXIT: ExitId = ExitId::new(0);

    /// The entry the exit writes in most tests.
    const ENTRY: Option<EntryId> = Some(EntryId::new(3));

    /// What the tests compare of a sent buffer: its lane, its stamp in milliseconds, its epoch, its generation, its
    /// entry, and its events' names.
    type Summary<'a> = (u16, u64, u32, u64, Option<EntryId>, Vec<&'a str>);

    fn generation(id: u64) -> GenerationRef {
        Arc::new(GenerationToken::new(GenerationId::new(id)))
    }

    fn counter(name: &str) -> Event {
        Event::Metric(Metric::counter(
            Context::from_parts(name.to_string(), TagSet::default()),
            1.0,
        ))
    }

    fn key_of(name: &str) -> u64 {
        Context::from_parts(name.to_string(), TagSet::default()).key().as_u64()
    }

    fn millis(millis: u64) -> AdmissionTime {
        AdmissionTime::from_unix_nanos(millis * 1_000_000)
    }

    /// A region input header: stamped by `point` at `at` milliseconds in `epoch`, of `generation`, with no entry.
    fn header(generation: &GenerationRef, point: u32, epoch: u32, at: u64) -> BatchHeader {
        BatchHeader {
            admitted_at: Some(millis(at)),
            admission_point: Some(AdmissionPointId::new(point)),
            epoch,
            generation: Some(Arc::clone(generation)),
            entry: None,
        }
    }

    fn buffer(names: &[String]) -> EventsBuffer {
        let mut buffer = EventsBuffer::default();
        for name in names {
            assert!(buffer.try_push(counter(name)).is_none());
        }
        buffer
    }

    /// The first `count` names whose contexts the key groups give to `lane`.
    fn names_on(groups: &KeyGroups, lane: u16, count: usize) -> Vec<String> {
        (0..)
            .map(|index| format!("m{}", index))
            .filter(|name| groups.lane_of(key_of(name)) == lane)
            .take(count)
            .collect()
    }

    /// One buffer the exit sent: the lane it went to, its header, and its events' names.
    #[derive(Debug)]
    struct Sent {
        lane: u16,
        header: BatchHeader,
        names: Vec<String>,
    }

    impl Sent {
        fn summary(&self) -> Summary<'_> {
            (
                self.lane,
                self.header.admitted_at.unwrap().as_unix_nanos() / 1_000_000,
                self.header.epoch,
                self.header.generation_id().unwrap().get(),
                self.header.entry,
                self.names.iter().map(String::as_str).collect(),
            )
        }
    }

    fn sent(exits: &mut ExitSink) -> Vec<Sent> {
        exits
            .drain_routed()
            .map(|(exit, lane, buffer)| {
                assert_eq!(exit, EXIT);
                Sent {
                    lane: lane.expect("a keyed exit names the lane of every buffer"),
                    header: buffer.header().clone(),
                    names: buffer
                        .into_iter()
                        .map(|event| event.try_as_metric().unwrap().context().name().to_string())
                        .collect(),
                }
            })
            .collect()
    }

    fn summaries(sent: &[Sent]) -> Vec<Summary<'_>> {
        sent.iter().map(Sent::summary).collect()
    }

    /// Runs one call with header `header` that wrote `buffers` to the exit with `entry`, at `now`, and returns what the
    /// exit sent.
    fn call(
        exit: &mut KeyedExit, header: &BatchHeader, entry: Option<EntryId>, buffers: &[Vec<String>], header_only: bool,
        now: Instant,
    ) -> Vec<Sent> {
        let mut exits = ExitSink::new(1);
        for names in buffers {
            exit.stage(entry, buffer(names));
        }
        exit.complete_call(header, header_only, now, &mut exits);
        sent(&mut exits)
    }

    fn end_turn(exit: &mut KeyedExit, now: Instant) -> (Option<Instant>, Vec<Sent>) {
        let mut exits = ExitSink::new(1);
        let next = exit.end_turn(now, &mut exits);
        (next, sent(&mut exits))
    }

    /// A keyed exit over two lanes forwarding point 0's progress, and names for each lane.
    fn two_lanes() -> (KeyedExit, Vec<String>, Vec<String>) {
        let groups = Arc::new(KeyGroups::new(2));
        let exit = KeyedExit::new(EXIT, Arc::clone(&groups), vec![AdmissionPointId::new(0)]);
        (exit, names_on(&groups, 0, 8), names_on(&groups, 1, 8))
    }

    #[test]
    fn a_routed_buffer_holds_its_share_of_the_route_buffer_events_between_256_and_1024() {
        for (lanes, events) in [
            (1, 1_024),
            (2, 1_024),
            (3, 682),
            (4, 512),
            (8, 256),
            (16, 256),
            (64, 256),
        ] {
            assert_eq!(routed_buffer_events(lanes), events, "{} lanes", lanes);
            let exit = KeyedExit::new(EXIT, Arc::new(KeyGroups::new(lanes)), Vec::new());
            assert_eq!(exit.capacity(), events);
        }
    }

    #[test]
    fn a_lanes_events_coalesce_across_the_calls_of_one_second_until_the_second_moves_on_or_the_hold_expires() {
        let (mut exit, a, b) = two_lanes();
        let g = generation(1);
        let start = Instant::now();

        // Both lanes hold an open buffer, so nothing goes out, not even progress.
        let both = vec![a[0].clone(), b[0].clone()];
        let out = call(&mut exit, &header(&g, 0, 0, 1_000_100), ENTRY, &[both], false, start);
        assert!(out.is_empty(), "{:?}", out);
        let both = vec![a[1].clone(), b[1].clone()];
        let out = call(&mut exit, &header(&g, 0, 0, 1_000_150), ENTRY, &[both], false, start);
        assert!(out.is_empty(), "{:?}", out);
        assert_eq!(exit.open_events(), vec![2, 2]);

        // The next second: lane 0's buffer goes out before the new one opens; lane 1's goes out once the call moved
        // on, carrying the latest stamp that appended to it, and lane 1 then learns the new second as progress.
        let out = call(
            &mut exit,
            &header(&g, 0, 0, 1_001_000),
            ENTRY,
            &[vec![a[2].clone()]],
            false,
            start,
        );
        assert_eq!(
            summaries(&out),
            vec![
                (0, 1_000_150, 0, 1, ENTRY, vec![a[0].as_str(), a[1].as_str()]),
                (1, 1_000_150, 0, 1, ENTRY, vec![b[0].as_str(), b[1].as_str()]),
                (1, 1_001_000, 0, 1, None, vec![]),
            ]
        );
        assert_eq!(exit.open_events(), vec![1, 0]);

        // The hold runs from when the buffer opened, whatever arrives meanwhile.
        let (next, out) = end_turn(&mut exit, start + Duration::from_millis(1));
        assert!(out.is_empty(), "{:?}", out);
        assert_eq!(next, Some(start + COALESCE_HOLD));
        let (next, out) = end_turn(&mut exit, start + COALESCE_HOLD);
        assert_eq!(next, None);
        assert_eq!(summaries(&out), vec![(0, 1_001_000, 0, 1, ENTRY, vec![a[2].as_str()])]);
    }

    #[test]
    fn an_open_buffer_of_the_previous_second_goes_out_before_a_generation_change_reaches_its_lane_without_data() {
        let (mut exit, a, b) = two_lanes();
        let (old, new) = (generation(1), generation(2));
        let start = Instant::now();

        let out = call(
            &mut exit,
            &header(&old, 0, 0, 1_000_900),
            ENTRY,
            &[vec![b[0].clone()]],
            false,
            start,
        );
        assert_eq!(
            summaries(&out),
            vec![(0, 1_000_900, 0, 1, None, vec![])],
            "lane 0 learns of the point"
        );
        // The new generation writes to lane 0 only: lane 1 receives its open buffer, then the new header at once.
        let out = call(
            &mut exit,
            &header(&new, 0, 0, 1_001_000),
            ENTRY,
            &[vec![a[0].clone()]],
            false,
            start,
        );
        assert_eq!(
            summaries(&out),
            vec![
                (1, 1_000_900, 0, 1, ENTRY, vec![b[0].as_str()]),
                (1, 1_001_000, 0, 2, None, vec![]),
            ]
        );

        // Within one second, a generation change alone does the same.
        let (mut exit, _, b) = two_lanes();
        call(
            &mut exit,
            &header(&old, 0, 0, 1_002_100),
            ENTRY,
            &[vec![b[1].clone()]],
            false,
            start,
        );
        let out = call(&mut exit, &header(&new, 0, 0, 1_002_150), ENTRY, &[], false, start);
        assert_eq!(
            summaries(&out),
            vec![
                (1, 1_002_100, 0, 1, ENTRY, vec![b[1].as_str()]),
                (0, 1_002_150, 0, 2, None, vec![]),
                (1, 1_002_150, 0, 2, None, vec![]),
            ]
        );
    }

    #[test]
    fn no_lane_receives_a_newer_epoch_before_the_points_final_header_in_the_older_one() {
        let (exit, a, _) = two_lanes();
        let mut exit = exit.with_capacity(2);
        let g = generation(1);
        let start = Instant::now();

        let out = call(
            &mut exit,
            &header(&g, 0, 0, 1_000_100),
            ENTRY,
            &[vec![a[0].clone()]],
            false,
            start,
        );
        assert_eq!(summaries(&out), vec![(1, 1_000_100, 0, 1, None, vec![])]);
        // A call whose events all left by other exits: lane 0's buffer stays open, and lane 1 is within the throttle.
        let out = call(&mut exit, &header(&g, 0, 0, 1_000_150), ENTRY, &[], false, start);
        assert!(out.is_empty(), "{:?}", out);

        // The clock resets, and the first call of the new epoch fills a buffer for lane 0: both lanes first receive
        // what was open and the final header of epoch 0, then the new epoch.
        let first = vec![a[1].clone(), a[2].clone()];
        let out = call(&mut exit, &header(&g, 0, 1, 600_000), ENTRY, &[first], false, start);
        assert_eq!(
            summaries(&out),
            vec![
                (0, 1_000_100, 0, 1, ENTRY, vec![a[0].as_str()]),
                (0, 1_000_150, 0, 1, None, vec![]),
                (1, 1_000_150, 0, 1, None, vec![]),
                (0, 600_000, 1, 1, ENTRY, vec![a[1].as_str(), a[2].as_str()]),
                (1, 600_000, 1, 1, None, vec![]),
            ]
        );
    }

    #[test]
    fn a_header_only_call_sends_every_open_buffer_and_then_its_progress_where_it_advances() {
        let (mut exit, a, b) = two_lanes();
        let g = generation(1);
        let start = Instant::now();

        let both = vec![a[0].clone(), b[0].clone()];
        call(&mut exit, &header(&g, 0, 0, 1_000_100), ENTRY, &[both], false, start);
        let out = call(&mut exit, &header(&g, 0, 0, 1_000_120), None, &[], true, start);
        assert_eq!(
            summaries(&out),
            vec![
                (0, 1_000_100, 0, 1, ENTRY, vec![a[0].as_str()]),
                (1, 1_000_100, 0, 1, ENTRY, vec![b[0].as_str()]),
                (0, 1_000_120, 0, 1, None, vec![]),
                (1, 1_000_120, 0, 1, None, vec![]),
            ]
        );
        // A heartbeat that does not advance tells no lane anything.
        let out = call(&mut exit, &header(&g, 0, 0, 1_000_120), None, &[], true, start);
        assert!(out.is_empty(), "{:?}", out);
    }

    #[test]
    fn an_open_buffer_goes_out_once_its_point_moved_a_progress_period_past_it_within_the_second() {
        let (mut exit, a, b) = two_lanes();
        let g = generation(1);
        let start = Instant::now();

        let both = vec![a[0].clone(), b[0].clone()];
        call(&mut exit, &header(&g, 0, 0, 1_000_100), ENTRY, &[both], false, start);
        // Lane 0 keeps appending; lane 1's buffer falls a progress period behind within the second, and goes out,
        // followed by the call's progress.
        let out = call(
            &mut exit,
            &header(&g, 0, 0, 1_000_150),
            ENTRY,
            &[vec![a[1].clone()]],
            false,
            start,
        );
        assert!(out.is_empty(), "{:?}", out);
        let out = call(
            &mut exit,
            &header(&g, 0, 0, 1_000_200),
            ENTRY,
            &[vec![a[2].clone()]],
            false,
            start,
        );
        assert_eq!(
            summaries(&out),
            vec![
                (1, 1_000_100, 0, 1, ENTRY, vec![b[0].as_str()]),
                (1, 1_000_200, 0, 1, None, vec![]),
            ]
        );
        assert_eq!(exit.open_events(), vec![3, 0]);
    }

    /// The address of a buffer's first event, which a moved buffer keeps and a copy does not.
    fn first_event(buffer: &EventsBuffer) -> *const Event {
        buffer.into_iter().next().expect("a buffer with events")
    }

    #[test]
    fn a_buffer_bound_for_one_lane_moves_whole_only_when_a_routed_buffer_holds_a_whole_events_buffer() {
        let g = generation(1);
        let start = Instant::now();
        let mut exits = ExitSink::new(1);

        // At two lanes a routed buffer holds a whole events buffer: a buffer whose events all go to one lane becomes
        // the open buffer as it is.
        let (mut exit, a, _) = two_lanes();
        let three = buffer(&a[..3]);
        let moved = first_event(&three);
        exit.stage(ENTRY, three);
        exit.complete_call(&header(&g, 0, 0, 1_000_100), false, start, &mut exits);
        assert_eq!(summaries(&sent(&mut exits)), vec![(1, 1_000_100, 0, 1, None, vec![])]);
        assert_eq!(
            exit.open_buffer(0).map(first_event),
            Some(moved),
            "the buffer itself is held"
        );
        // One that fills a routed buffer goes out as it is, after the open one, which has another entry.
        let full = buffer(&names_on(&KeyGroups::new(2), 0, 1_024));
        let moved = first_event(&full);
        exit.stage(Some(EntryId::new(4)), full);
        exit.complete_call(&header(&g, 0, 0, 1_000_110), false, start, &mut exits);
        let out = exits.drain_routed().map(|(_, _, buffer)| buffer).collect::<Vec<_>>();
        assert_eq!(out.iter().map(EventsBuffer::len).collect::<Vec<_>>(), vec![3, 1_024]);
        assert_eq!(first_event(&out[1]), moved, "the buffer itself goes out");
        assert_eq!(exit.open_events(), vec![0, 0]);

        // Smaller routed buffers never hold or send a region's buffer, which has room for a whole batch: its events
        // are copied into routed buffers, which go out once full.
        let (exit, a, _) = two_lanes();
        let mut exit = exit.with_capacity(4);
        let three = buffer(&a[..3]);
        let staged = first_event(&three);
        exit.stage(ENTRY, three);
        exit.complete_call(&header(&g, 0, 0, 1_000_100), false, start, &mut exits);
        assert_eq!(summaries(&sent(&mut exits)), vec![(1, 1_000_100, 0, 1, None, vec![])]);
        assert_eq!(exit.open_events(), vec![3, 0]);
        assert_ne!(exit.open_buffer(0).map(first_event), Some(staged), "a copy is held");
        // A buffer written with another entry follows the open one, which goes out first, even one that fills a
        // routed buffer by itself.
        let other = Some(EntryId::new(4));
        let five = buffer(&a[3..8]);
        let staged = first_event(&five);
        exit.stage(other, five);
        exit.complete_call(&header(&g, 0, 0, 1_000_110), false, start, &mut exits);
        let out = exits.drain_routed().map(|(_, _, buffer)| buffer).collect::<Vec<_>>();
        assert_eq!(out.iter().map(EventsBuffer::len).collect::<Vec<_>>(), vec![3, 4]);
        assert_eq!(out[1].header().entry, other);
        assert_ne!(first_event(&out[1]), staged, "a copy goes out");
        assert_eq!(exit.open_events(), vec![1, 0]);
        // An open buffer of the same key takes events until it is full.
        let out = call(
            &mut exit,
            &header(&g, 0, 0, 1_000_120),
            other,
            &[a[..3].to_vec()],
            false,
            start,
        );
        assert_eq!(
            summaries(&out),
            vec![(
                0,
                1_000_120,
                0,
                1,
                other,
                vec![a[7].as_str(), a[0].as_str(), a[1].as_str(), a[2].as_str()]
            )]
        );
        assert_eq!(exit.open_events(), vec![0, 0]);
    }

    #[test]
    fn a_buffer_moves_whole_only_with_storage_for_at_most_a_routed_buffer_and_is_sent_rather_than_grown() {
        let g = generation(1);
        let start = Instant::now();
        let mut exits = ExitSink::new(1);
        let (exit, a, _) = two_lanes();
        let mut exit = exit.with_capacity(4);
        let allocated = |names: &[String], storage: usize| {
            let mut buffer = EventsBuffer::with_capacity(storage);
            for name in names {
                assert!(buffer.try_push(counter(name)).is_none());
            }
            assert_eq!(buffer.allocated_capacity(), storage);
            buffer
        };

        // A buffer with storage for two events, at most a routed buffer's four, is held as it is.
        let two = allocated(&a[..2], 2);
        let moved = first_event(&two);
        exit.stage(ENTRY, two);
        exit.complete_call(&header(&g, 0, 0, 1_000_100), false, start, &mut exits);
        assert_eq!(summaries(&sent(&mut exits)), vec![(1, 1_000_100, 0, 1, None, vec![])]);
        assert_eq!(
            exit.open_buffer(0).map(first_event),
            Some(moved),
            "the buffer itself is held"
        );
        // Its storage is full: the next event of its key sends it rather than growing it, and opens a routed buffer.
        let one = allocated(&a[2..3], 1);
        exit.stage(ENTRY, one);
        exit.complete_call(&header(&g, 0, 0, 1_000_110), false, start, &mut exits);
        let out = exits.drain_routed().map(|(_, _, buffer)| buffer).collect::<Vec<_>>();
        assert_eq!(out.iter().map(EventsBuffer::len).collect::<Vec<_>>(), vec![2]);
        assert_eq!(first_event(&out[0]), moved, "the held buffer goes out as it is");
        assert_eq!(exit.open_events(), vec![1, 0]);

        // One with storage for five events, more than a routed buffer's, is copied, though it holds only three.
        let three = allocated(&a[3..6], 5);
        let staged = first_event(&three);
        exit.stage(ENTRY, three);
        exit.complete_call(&header(&g, 0, 0, 1_000_120), false, start, &mut exits);
        let out = exits.drain_routed().map(|(_, _, buffer)| buffer).collect::<Vec<_>>();
        assert_eq!(out.iter().map(EventsBuffer::len).collect::<Vec<_>>(), vec![4]);
        assert_ne!(first_event(&out[0]), staged, "a copy goes out");
        assert_eq!(exit.open_events(), vec![0, 0]);

        // Whatever moved or was copied, nothing the exit held or sent had room for more than a routed buffer's events.
        assert!(out.iter().all(|buffer| buffer.allocated_capacity() <= exit.capacity()));
    }

    #[test]
    fn a_lane_that_receives_none_of_a_new_seconds_events_learns_of_the_second_at_once() {
        let (mut exit, a, b) = two_lanes();
        let g = generation(1);
        let start = Instant::now();

        let out = call(&mut exit, &header(&g, 0, 0, 9_990), None, &[], true, start);
        assert_eq!(
            summaries(&out),
            vec![(0, 9_990, 0, 1, None, vec![]), (1, 9_990, 0, 1, None, vec![])]
        );
        // Second 10's first events go to lane 0, which holds them: lane 1 learns of the second at once, within the
        // throttle, so its tick gate reaches second 10 as a state owner on one lane, which receives the events, does.
        let out = call(
            &mut exit,
            &header(&g, 0, 0, 10_020),
            ENTRY,
            &[vec![a[0].clone()]],
            false,
            start,
        );
        assert_eq!(summaries(&out), vec![(1, 10_020, 0, 1, None, vec![])]);
        // Within the second and the throttle, lane 1 learns nothing more.
        let out = call(
            &mut exit,
            &header(&g, 0, 0, 10_050),
            ENTRY,
            &[vec![a[1].clone()]],
            false,
            start,
        );
        assert!(out.is_empty(), "{:?}", out);
        let (_, out) = end_turn(&mut exit, start + COALESCE_HOLD);
        assert_eq!(
            summaries(&out),
            vec![(0, 10_050, 0, 1, ENTRY, vec![a[0].as_str(), a[1].as_str()])]
        );

        // A lane whose open buffer of the previous second goes out learns of the new second right after it.
        let out = call(
            &mut exit,
            &header(&g, 0, 0, 10_980),
            ENTRY,
            &[vec![b[0].clone()]],
            false,
            start,
        );
        assert_eq!(summaries(&out), vec![(0, 10_980, 0, 1, None, vec![])]);
        let out = call(
            &mut exit,
            &header(&g, 0, 0, 11_010),
            ENTRY,
            &[vec![a[2].clone()]],
            false,
            start,
        );
        assert_eq!(
            summaries(&out),
            vec![
                (1, 10_980, 0, 1, ENTRY, vec![b[0].as_str()]),
                (1, 11_010, 0, 1, None, vec![])
            ]
        );
    }

    #[test]
    fn finishing_sends_what_is_open_and_then_each_points_last_header_to_the_lanes_without_it() {
        let groups = Arc::new(KeyGroups::new(2));
        let points = vec![AdmissionPointId::new(0), AdmissionPointId::new(1)];
        let mut exit = KeyedExit::new(EXIT, Arc::clone(&groups), points);
        let (a, b) = (names_on(&groups, 0, 2), names_on(&groups, 1, 2));
        let g = generation(1);
        let start = Instant::now();

        // Point 1's only call reaches lane 1 as data, then lane 0 as progress once point 0 moved lane 1's buffer on.
        call(
            &mut exit,
            &header(&g, 1, 0, 2_000_000),
            ENTRY,
            &[vec![b[1].clone()]],
            false,
            start,
        );
        let both = vec![a[0].clone(), b[0].clone()];
        call(&mut exit, &header(&g, 0, 0, 1_000_100), ENTRY, &[both], false, start);
        call(&mut exit, &header(&g, 0, 0, 1_000_150), ENTRY, &[], false, start);
        let mut exits = ExitSink::new(1);
        exit.finish(&mut exits);
        let out = sent(&mut exits);
        assert_eq!(
            summaries(&out),
            vec![
                (0, 1_000_100, 0, 1, ENTRY, vec![a[0].as_str()]),
                (1, 1_000_100, 0, 1, ENTRY, vec![b[0].as_str()]),
                (0, 1_000_150, 0, 1, None, vec![]),
                (1, 1_000_150, 0, 1, None, vec![]),
            ]
        );
        assert!(out
            .iter()
            .all(|sent| sent.header.admission_point == Some(AdmissionPointId::new(0))));
        exit.finish(&mut exits);
        assert!(exits.is_empty(), "finishing again sends nothing");
    }

    #[test]
    fn a_call_that_panicked_leaves_what_earlier_calls_left_open() {
        let (mut exit, a, _) = two_lanes();
        let g = generation(1);
        let start = Instant::now();

        call(
            &mut exit,
            &header(&g, 0, 0, 1_000_100),
            ENTRY,
            &[vec![a[0].clone()]],
            false,
            start,
        );
        exit.stage(ENTRY, buffer(&a[1..3]));
        exit.abandon_call();
        assert_eq!(exit.open_events(), vec![1, 0]);
        let out = call(
            &mut exit,
            &header(&g, 0, 0, 1_000_150),
            ENTRY,
            &[vec![a[3].clone()]],
            false,
            start,
        );
        assert!(out.is_empty(), "{:?}", out);
        let mut exits = ExitSink::new(1);
        exit.finish(&mut exits);
        assert_eq!(
            summaries(&sent(&mut exits)),
            vec![
                (0, 1_000_150, 0, 1, ENTRY, vec![a[0].as_str(), a[3].as_str()]),
                (1, 1_000_150, 0, 1, None, vec![]),
            ]
        );
    }

    /// What aggregation and the tracker read of an event from its routed buffer's header: the generation, the epoch,
    /// the entry, the point, and the admission second.
    type Attrs = (
        Option<GenerationId>,
        u32,
        Option<EntryId>,
        Option<AdmissionPointId>,
        Option<u64>,
    );

    fn attrs(header: &BatchHeader, entry: Option<EntryId>) -> Attrs {
        (
            header.generation_id(),
            header.epoch,
            entry,
            header.admission_point,
            header.admitted_at.map(AdmissionTime::as_unix_secs),
        )
    }

    /// What every state lane received from a keyed exit, in order, as a state lane's watermark tracker observes it.
    struct Lanes {
        /// Per lane, every event received, in order, with what its buffer's header says of it.
        events: Vec<Vec<(String, Attrs)>>,
        /// Per lane and point, the last header received from the point.
        view: Vec<Vec<Option<SentMark>>>,
        /// Per lane, the watermark tracker of a state lane, which observes every header the lane receives.
        trackers: Vec<WatermarkTracker>,
        /// Per lane, every epoch switch its tracker observed, in order.
        switches: Vec<Vec<Observation>>,
    }

    impl Lanes {
        fn new(lanes: usize, points: usize) -> Self {
            let tracker = WatermarkTracker::new((0..points).map(|point| AdmissionPointId::new(point as u32)));
            Self {
                events: vec![Vec::new(); lanes],
                view: vec![vec![None; points]; lanes],
                trackers: vec![tracker; lanes],
                switches: vec![Vec::new(); lanes],
            }
        }

        /// Delivers a buffer with header `header` and the events `names` to `lane`: each point's stamps never decrease
        /// within an epoch at a lane, its epochs never go back, a lane leaves an epoch only after the point's final
        /// header in it, of `finals`, and the lane's tracker finds no fault.
        fn deliver(
            &mut self, lane: usize, header: &BatchHeader, names: Vec<String>, finals: &BTreeMap<(usize, u32), SentMark>,
        ) -> Result<(), TestCaseError> {
            let point = header.admission_point.unwrap().get() as usize;
            let mark = SentMark::of(header).unwrap();
            if let Some(last) = self.view[lane][point] {
                if last.epoch == mark.epoch {
                    prop_assert!(
                        mark.admitted_at >= last.admitted_at,
                        "lane {} received {:?} from point {} after {:?}",
                        lane,
                        mark,
                        point,
                        last
                    );
                } else {
                    prop_assert!(epoch_newer(mark.epoch, last.epoch), "an older epoch after a newer one");
                    prop_assert_eq!(
                        Some(&last),
                        finals.get(&(point, last.epoch)),
                        "lane {} left epoch {} of point {} before its final header",
                        lane,
                        last.epoch,
                        point
                    );
                }
            }
            match self.trackers[lane].observe(header) {
                Observation::Ok => {}
                Observation::Fault => prop_assert!(false, "lane {}'s tracker found a fault in {:?}", lane, header),
                switched => self.switches[lane].push(switched),
            }
            self.view[lane][point] = Some(mark);
            for name in names {
                self.events[lane].push((name, attrs(header, header.entry)));
            }
            Ok(())
        }

        /// Delivers what a keyed exit sent, in order.
        fn receive(&mut self, sent: Vec<Sent>, finals: &BTreeMap<(usize, u32), SentMark>) -> Result<(), TestCaseError> {
            for Sent { lane, header, names } in sent {
                self.deliver(usize::from(lane), &header, names, finals)?;
            }
            Ok(())
        }
    }

    /// A keyed exit that sends what each call routes at once, with the same progress and final-header rules, written
    /// apart from the exit under test: what every state lane receives without coalescing.
    struct Uncoalesced {
        groups: Arc<KeyGroups>,
        lanes: Lanes,
        /// Per point, the header of its last completed call, without its entry.
        last_call: Vec<Option<BatchHeader>>,
        /// The final header of each point's epoch, by point and epoch, once the point moved past the epoch.
        finals: BTreeMap<(usize, u32), SentMark>,
    }

    impl Uncoalesced {
        fn new(groups: Arc<KeyGroups>, points: usize) -> Self {
            let lanes = usize::from(groups.lanes());
            Self {
                groups,
                lanes: Lanes::new(lanes, points),
                last_call: vec![None; points],
                finals: BTreeMap::new(),
            }
        }

        /// Returns the marks of the points' last completed calls.
        fn last_marks(&self) -> Vec<Option<SentMark>> {
            self.last_call
                .iter()
                .map(|last| last.as_ref().and_then(SentMark::of))
                .collect()
        }

        fn call(
            &mut self, point: usize, header: &BatchHeader, entry: Option<EntryId>, buffers: &[Vec<String>],
            header_only: bool,
        ) -> Result<(), TestCaseError> {
            let lanes = self.lanes.events.len();
            let mark = SentMark::of(header).unwrap();
            if let Some(last) = self.last_call[point].clone() {
                let final_mark = SentMark::of(&last).unwrap();
                if epoch_newer(mark.epoch, final_mark.epoch) {
                    self.finals.insert((point, final_mark.epoch), final_mark);
                    for lane in 0..lanes {
                        if self.lanes.view[lane][point] != Some(final_mark) {
                            self.lanes.deliver(lane, &last, Vec::new(), &self.finals)?;
                        }
                    }
                }
            }
            // Each lane receives the call's events for it in one buffer, and otherwise its progress when it is due.
            let mut routed = vec![Vec::new(); lanes];
            for name in buffers.iter().flatten() {
                routed[usize::from(self.groups.lane_of(key_of(name)))].push(name.clone());
            }
            let mut progress = header.clone();
            progress.entry = None;
            for (lane, names) in routed.into_iter().enumerate() {
                if !names.is_empty() {
                    let mut stamped = header.clone();
                    stamped.entry = entry;
                    self.lanes.deliver(lane, &stamped, names, &self.finals)?;
                    continue;
                }
                let due = self.lanes.view[lane][point].is_none_or(|last| {
                    (header_only && mark.admitted_at > last.admitted_at)
                        || mark.epoch != last.epoch
                        || mark.generation != last.generation
                        || mark.admitted_at.as_unix_secs() > last.admitted_at.as_unix_secs()
                        || mark.admitted_at.as_unix_nanos() >= last.admitted_at.as_unix_nanos() + 100_000_000
                });
                if due {
                    self.lanes.deliver(lane, &progress, Vec::new(), &self.finals)?;
                }
            }
            self.last_call[point] = Some(progress);
            Ok(())
        }

        fn finish(&mut self) -> Result<(), TestCaseError> {
            for lane in 0..self.lanes.events.len() {
                for (point, last) in self.last_call.iter().enumerate() {
                    let Some(last) = last else {
                        continue;
                    };
                    if self.lanes.view[lane][point] != SentMark::of(last) {
                        self.lanes.deliver(lane, last, Vec::new(), &self.finals)?;
                    }
                }
            }
            Ok(())
        }
    }

    /// One call in a generated stream: from `point`, after the host's clock moved `wait` microseconds, with the point's
    /// clock `advance` milliseconds on, possibly reset into its next epoch or moved to its next generation, writing
    /// `buffers` of contexts by index with entry `entry`.
    #[derive(Clone, Debug)]
    struct Step {
        point: usize,
        advance: u64,
        reset: bool,
        next_generation: bool,
        entry: u8,
        buffers: Vec<Vec<u8>>,
        /// For a call that wrote nothing: whether it was header-only, rather than one whose events left elsewhere.
        header_only: bool,
        /// Whether the call's program panicked after it wrote to the exit.
        panics: bool,
        wait: u64,
        /// Whether the host ends its turn after the call.
        end_turn: bool,
        /// How long input then stays idle, in microseconds, after which the host's timer ends the turn again.
        idle: u64,
    }

    fn arb_step() -> impl Strategy<Value = Step> {
        (
            (
                0usize..3,
                prop_oneof![Just(0u64), 0u64..30, 0u64..400],
                prop::bool::weighted(0.05),
                prop::bool::weighted(0.1),
                0u8..3,
            ),
            (
                prop::collection::vec(prop::collection::vec(0u8..24, 1..6), 0..3),
                any::<bool>(),
                prop::bool::weighted(0.05),
            ),
            (
                prop_oneof![0u64..1_000, 0u64..8_000],
                any::<bool>(),
                prop_oneof![Just(0u64), 0u64..10_000],
            ),
        )
            .prop_map(
                |(
                    (point, advance, reset, next_generation, entry),
                    (buffers, header_only, panics),
                    (wait, end_turn, idle),
                )| {
                    Step {
                        point,
                        advance,
                        reset,
                        next_generation,
                        entry,
                        buffers,
                        header_only,
                        panics,
                        wait,
                        end_turn,
                        idle,
                    }
                },
            )
    }

    /// Checks the exit against the one that does not coalesce after each of its calls and turns: a lane with no open
    /// buffer has received exactly the same events, with the same attributes, and the same last header from every
    /// point, its tracker is the same and observed the same epoch switches, and it holds every point's latest
    /// generation, epoch, and admission second, whichever lanes the point's events went to; a lane with one has
    /// received all but the events the buffer holds, fewer than its capacity; and while a buffer is open, the end of a
    /// turn names when its hold expires, which is later.
    fn check(
        exit: &KeyedExit, received: &Lanes, uncoalesced: &Uncoalesced, next: Option<Option<Instant>>, now: Instant,
    ) -> Result<(), TestCaseError> {
        let open = exit.open_events();
        for (lane, held) in open.iter().enumerate() {
            prop_assert!(*held < exit.capacity(), "lane {} holds a full buffer", lane);
            let (got, all) = (&received.events[lane], &uncoalesced.lanes.events[lane]);
            prop_assert_eq!(
                got.len() + held,
                all.len(),
                "lane {} holds what it has not received",
                lane
            );
            prop_assert_eq!(&got[..], &all[..got.len()], "lane {} received other events", lane);
            if *held > 0 {
                continue;
            }
            prop_assert_eq!(
                &received.view[lane],
                &uncoalesced.lanes.view[lane],
                "lane {}'s view",
                lane
            );
            prop_assert_eq!(
                &received.trackers[lane],
                &uncoalesced.lanes.trackers[lane],
                "lane {}'s tracker",
                lane
            );
            prop_assert_eq!(
                &received.switches[lane],
                &uncoalesced.lanes.switches[lane],
                "lane {}'s epoch switches",
                lane
            );
            for (point, last) in uncoalesced.last_call.iter().enumerate() {
                let Some(last) = last else {
                    continue;
                };
                let progress = received.trackers[lane].progress(AdmissionPointId::new(point as u32));
                prop_assert_eq!(
                    progress.map(|progress| (progress.max_generation, progress.epoch, progress.latest.as_unix_secs())),
                    Some((
                        last.generation_id(),
                        last.epoch,
                        last.admitted_at.unwrap().as_unix_secs()
                    )),
                    "lane {} is behind point {}'s latest second",
                    lane,
                    point
                );
            }
        }
        if let Some(next) = next {
            let held = open.iter().any(|held| *held > 0);
            prop_assert_eq!(
                next.is_some(),
                held,
                "the end of a turn names a deadline while a buffer is open"
            );
            prop_assert!(
                next.is_none_or(|next| next > now),
                "an expired hold is sent at the end of its turn"
            );
        }
        Ok(())
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        #[test]
        fn property_test_coalescing_is_exact(
            lanes in 1u16..=4,
            points in 1usize..=3,
            capacity in 1usize..=6,
            steps in prop::collection::vec(arb_step(), 1..60),
        ) {
            let groups = Arc::new(KeyGroups::new(lanes));
            let ids = (0..points).map(|point| AdmissionPointId::new(point as u32)).collect::<Vec<_>>();
            let mut exit = KeyedExit::new(EXIT, Arc::clone(&groups), ids).with_capacity(capacity);
            let mut uncoalesced = Uncoalesced::new(Arc::clone(&groups), points);
            let mut received = Lanes::new(usize::from(lanes), points);
            let generations = (1..=4).map(generation).collect::<Vec<_>>();
            let names = (0..24).map(|index| format!("c{}", index)).collect::<Vec<_>>();
            // Each point's clock: its generation, its epoch, and its latest stamp in milliseconds.
            let mut clocks = vec![(0usize, 0u32, 100_000_000u64); points];
            let mut now = Instant::now();

            for step in steps {
                let point = step.point % points;
                let clock = &mut clocks[point];
                if step.next_generation {
                    clock.0 = (clock.0 + 1).min(generations.len() - 1);
                }
                if step.reset {
                    // A reset moves the point's clock back by more than the reset threshold, into its next epoch.
                    clock.1 += 1;
                    clock.2 -= 400_000;
                }
                clock.2 += step.advance;
                let header = header(&generations[clock.0], point as u32, clock.1, clock.2);
                let entry = match step.entry {
                    0 => None,
                    entry => Some(EntryId::new(u16::from(entry))),
                };
                let buffers = step
                    .buffers
                    .iter()
                    .map(|buffer| buffer.iter().map(|name| names[usize::from(*name)].clone()).collect::<Vec<_>>())
                    .collect::<Vec<_>>();
                let header_only = buffers.is_empty() && step.header_only;
                now += Duration::from_micros(step.wait);

                if step.panics {
                    for names in &buffers {
                        exit.stage(entry, buffer(names));
                    }
                    exit.abandon_call();
                } else {
                    let out = call(&mut exit, &header, entry, &buffers, header_only, now);
                    uncoalesced.call(point, &header, entry, &buffers, header_only)?;
                    received.receive(out, &uncoalesced.finals)?;
                }
                check(&exit, &received, &uncoalesced, None, now)?;
                if step.end_turn {
                    let (next, out) = end_turn(&mut exit, now);
                    received.receive(out, &uncoalesced.finals)?;
                    check(&exit, &received, &uncoalesced, Some(next), now)?;
                }
                if step.idle > 0 {
                    now += Duration::from_micros(step.idle);
                    let (next, out) = end_turn(&mut exit, now);
                    received.receive(out, &uncoalesced.finals)?;
                    check(&exit, &received, &uncoalesced, Some(next), now)?;
                }
            }

            let mut exits = ExitSink::new(1);
            exit.finish(&mut exits);
            uncoalesced.finish()?;
            received.receive(sent(&mut exits), &uncoalesced.finals)?;
            prop_assert_eq!(exit.open_events(), vec![0; usize::from(lanes)]);
            check(&exit, &received, &uncoalesced, None, now)?;
            prop_assert_eq!(&received.events, &uncoalesced.lanes.events);
            // Every lane holds each point's last stamp when its input closes.
            for view in &received.view {
                prop_assert_eq!(view, &uncoalesced.last_marks());
            }
        }

        #[test]
        fn property_test_keyed_exit_routes_each_context_to_one_lane(
            lanes in 1u16..=64,
            capacity in 1usize..=1_024,
            calls in prop::collection::vec(prop::collection::vec("[a-z0-9]{1,12}", 1..40), 1..8),
        ) {
            let groups = Arc::new(KeyGroups::new(lanes));
            // The table is contiguous: each lane owns one range of groups, in lane order, and every lane owns some.
            for group in 0..256 {
                prop_assert_eq!(usize::from(groups.owner(group)), group * usize::from(lanes) / 256);
            }
            prop_assert_eq!(groups.owner(0), 0);
            prop_assert_eq!(groups.owner(255), lanes - 1);
            prop_assert!((1..256).all(|group| groups.owner(group) - groups.owner(group - 1) <= 1));

            let mut exit =
                KeyedExit::new(EXIT, Arc::clone(&groups), vec![AdmissionPointId::new(0)]).with_capacity(capacity);
            let g = generation(1);
            let mut exits = ExitSink::new(1);
            let mut delivered = Vec::new();
            for (index, names) in calls.iter().enumerate() {
                let header = header(&g, 0, 0, 1_000_000 + 400 * index as u64);
                exit.stage(ENTRY, buffer(names));
                exit.complete_call(&header, false, Instant::now(), &mut exits);
                delivered.extend(sent(&mut exits));
            }
            exit.finish(&mut exits);
            delivered.extend(sent(&mut exits));

            let mut lane_of = BTreeMap::new();
            let mut per_lane = vec![Vec::new(); usize::from(lanes)];
            for sent in delivered {
                for name in sent.names {
                    prop_assert_eq!(sent.lane, groups.lane_of(key_of(&name)), "{} reached another lane", name);
                    prop_assert_eq!(*lane_of.entry(name.clone()).or_insert(sent.lane), sent.lane);
                    per_lane[usize::from(sent.lane)].push(name);
                }
            }
            // Each lane receives its events in the order the calls wrote them.
            for (lane, received) in per_lane.iter().enumerate() {
                let expected = calls
                    .iter()
                    .flatten()
                    .filter(|name| usize::from(groups.lane_of(key_of(name))) == lane)
                    .cloned()
                    .collect::<Vec<_>>();
                prop_assert_eq!(received, &expected, "lane {}", lane);
            }
        }
    }
}
