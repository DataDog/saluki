//! Arrival replays: one trace, in the order a partitioned execution's state lanes saw their input, which the
//! interpreter can run as its lane-less oracle.
//!
//! A harness that runs a frame with several state lanes records, per state lane, everything the lane observed: each
//! routed buffer it admitted, with the header it carried, each header-only observation, and each tick it fired, with
//! its epoch, ordinal, and flush time. [`ArrivalReplay`] merges those logs into one trace that the reference
//! interpreter runs [in arrival order][super::Interpreter::in_arrival_order]:
//!
//! - **Epoch by epoch.** Every observation of an older epoch of the source clock goes before every observation of a
//!   newer one, so the interpreter, which infers resets from jumps in admission times, sees one reset per jump, never
//!   an old-epoch admission after a newer one read as a second jump. The old-epoch instances and the new epoch's fire
//!   no tick at once: after a reset's mark no lane fires an old-epoch tick past its cutoff, so every old-epoch tick
//!   goes before the reset, and a new-epoch tick fires only once every point switched, which the replay's prefix then
//!   shows.
//! - **Tick by tick.** Within an epoch, per tick, every lane's observations before its own firing of the tick go before
//!   one `advance` to the tick's flush time, and those after it go after. Each lane fired at a watermark that reached
//!   the flush time, so the replayed prefix brings the interpreter's watermark there too, and its tick fires at once.
//! - **Lane by lane.** Between two ticks, each lane's observations keep the lane's own order, one lane after another:
//!   lanes hold disjoint contexts, so only each lane's order decides what a state owner holds. Each routed buffer is an
//!   `admit` of its own, at the header the lane saw: one admission second, one bucket. A header-only observation is a
//!   `heartbeat` at that header.
//!
//! A replay is valid only when every lane fired each tick at the same flush time and took its idle-counter start from
//! the shared log. A harness reports every case in which a lane's timing decided an outcome ([`exclude`]), such as a
//! grace expiry, a straggler, an unresolved previous tick, or a reset overflow; the replay itself excludes lanes that
//! fired one tick at different flush times. A lane that did not fire a tick another lane fired, because it had not
//! heard from every point yet or its reset window was open, needs no exclusion: what it saw after its own last tick
//! goes after the other lanes' tick, so the interpreter, like the lane, flushes it at a later tick or drain.
//!
//! [`exclude`]: ArrivalReplay::exclude

use std::collections::BTreeMap;

use saluki_core::{data_model::event::metric::Metric, execution::AdmissionTime};

use super::{Trace, TraceEvent};
use crate::{ir::NodeId, runtime::state::rules::epoch_newer};

/// One thing a state lane observed, as a harness records it.
#[derive(Clone, Debug)]
pub enum LaneObservation {
    /// The lane admitted a routed buffer, stamped at lane `lane` of `source` at `admitted_at` in `epoch`.
    Admitted {
        /// The source that stamped the buffer.
        source: NodeId,

        /// The lane of the source.
        lane: u16,

        /// The buffer's admission time.
        admitted_at: AdmissionTime,

        /// The epoch of the source's clock in the buffer's header.
        epoch: u32,

        /// The buffer's metrics.
        batch: Vec<Metric>,
    },

    /// The lane observed a header-only buffer of lane `lane` of `source` at `admitted_at` in `epoch`.
    Progress {
        /// The source that stamped the header.
        source: NodeId,

        /// The lane of the source.
        lane: u16,

        /// The header's admission time.
        admitted_at: AdmissionTime,

        /// The epoch of the source's clock in the header.
        epoch: u32,
    },

    /// The lane fired the tick of ordinal `ordinal` of the lineage instance of `epoch`, at flush time `flush`.
    Ticked {
        /// The epoch of the lineage instance the tick belongs to.
        epoch: u32,

        /// The tick's ordinal within that instance.
        ordinal: u64,

        /// The tick's flush time.
        flush: AdmissionTime,
    },
}

impl LaneObservation {
    fn epoch(&self) -> u32 {
        match self {
            Self::Admitted { epoch, .. } | Self::Progress { epoch, .. } | Self::Ticked { epoch, .. } => *epoch,
        }
    }
}

/// Why a recorded execution has no valid arrival replay, so that its case is counted and excluded.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplayExcluded {
    /// What made the replay invalid.
    pub reason: String,
}

/// The logs of every state lane of one recorded execution, merged into one trace on demand.
///
/// The replay supports one tick schedule per slot, as the interpreter, whose `advance` ticks every state owner, does:
/// the lineages of the lanes' slot tick together, such as one lineage.
#[derive(Clone, Debug, Default)]
pub struct ArrivalReplay {
    lanes: Vec<Vec<LaneObservation>>,
    prelude: Vec<TraceEvent>,
    exclusions: Vec<String>,
    closed: bool,
}

impl ArrivalReplay {
    /// Creates a replay of `lanes` state lanes, none of which observed anything yet.
    pub fn new(lanes: u16) -> Self {
        Self {
            lanes: (0..lanes).map(|_| Vec::new()).collect(),
            ..Self::default()
        }
    }

    /// Returns this replay starting with `events`, before anything the lanes observed: the boot plan's installation.
    pub fn with_prelude<I>(mut self, events: I) -> Self
    where
        I: IntoIterator<Item = TraceEvent>,
    {
        self.prelude.extend(events);
        self
    }

    /// Records what state lane `lane` observed, after everything it observed before.
    ///
    /// # Panics
    ///
    /// Panics if the replay has no lane `lane`, which is a bug in the harness.
    pub fn record(&mut self, lane: u16, observation: LaneObservation) {
        self.lanes[usize::from(lane)].push(observation);
    }

    /// Returns what state lane `lane` observed, in order.
    pub fn observations(&self, lane: u16) -> &[LaneObservation] {
        self.lanes.get(usize::from(lane)).map_or(&[], Vec::as_slice)
    }

    /// Records that a lane's timing decided an outcome, as the harness learned from the lanes' divergence counters:
    /// the execution has no valid replay.
    pub fn exclude(&mut self, reason: impl Into<String>) {
        self.exclusions.push(reason.into());
    }

    /// Records that the execution's input ended: the replay ends with `close`.
    pub fn close(&mut self) {
        self.closed = true;
    }

    /// Merges the lanes' logs into one trace, or says why the execution has no valid replay.
    ///
    /// # Errors
    ///
    /// Returns why the replay is excluded: an exclusion the harness recorded, a lane that fired a tick twice or out of
    /// order, lanes that fired one tick at different flush times, or flush times that move back within an epoch.
    pub fn build(&self) -> Result<Trace, ReplayExcluded> {
        let excluded = |reason: String| Err(ReplayExcluded { reason });
        if let Some(reason) = self.exclusions.first() {
            return excluded(reason.clone());
        }

        // The epochs, oldest first: one clock's epochs only move forward, though a lane may observe a newer one before
        // an older one's last observation.
        let mut epochs = Vec::<u32>::new();
        for observation in self.lanes.iter().flatten() {
            let epoch = observation.epoch();
            if !epochs.contains(&epoch) {
                let at = epochs
                    .iter()
                    .position(|known| epoch_newer(*known, epoch))
                    .unwrap_or(epochs.len());
                epochs.insert(at, epoch);
            }
        }

        // Every tick, with its flush time, by epoch and ordinal; each lane fires each at most once, in order.
        let mut ticks = BTreeMap::<(usize, u64), AdmissionTime>::new();
        let rank = |epoch: u32| epochs.iter().position(|known| *known == epoch).unwrap_or(0);
        for (lane, observations) in self.lanes.iter().enumerate() {
            let mut previous = None;
            for observation in observations {
                let LaneObservation::Ticked { epoch, ordinal, flush } = observation else {
                    continue;
                };
                let key = (rank(*epoch), *ordinal);
                if previous.is_some_and(|previous| previous >= key) {
                    return excluded(format!(
                        "state lane {} fired tick {} of epoch {} after a later tick or twice",
                        lane, ordinal, epoch
                    ));
                }
                previous = Some(key);
                match ticks.get(&key) {
                    Some(known) if known != flush => {
                        return excluded(format!(
                            "state lanes fired tick {} of epoch {} at different flush times, {} and {}",
                            ordinal, epoch, known, flush
                        ))
                    }
                    _ => {
                        ticks.insert(key, *flush);
                    }
                }
            }
        }
        for (rank, epoch) in epochs.iter().enumerate() {
            let flushes = ticks.range((rank, 0)..=(rank, u64::MAX)).map(|(_, flush)| *flush);
            if flushes
                .clone()
                .zip(flushes.skip(1))
                .any(|(earlier, later)| later < earlier)
            {
                return excluded(format!("flush times move back within epoch {}", epoch));
            }
        }

        let mut trace = self.prelude.iter().cloned().collect::<Trace>();
        for (rank, &epoch) in epochs.iter().enumerate() {
            let epoch_ticks = ticks
                .range((rank, 0)..=(rank, u64::MAX))
                .map(|(&(_, ordinal), &flush)| (ordinal, flush))
                .collect::<Vec<_>>();
            // Each lane's observations of this epoch, cut at its ticks: segment `i` precedes the epoch's `i`-th tick.
            // What a lane saw after its last tick goes before the next tick it fired, or after the epoch's last tick
            // if it fired none: a tick it did not fire flushes none of it, in the lane or in the interpreter.
            let segments = self
                .lanes
                .iter()
                .map(|observations| {
                    let mut segments = vec![Vec::new(); epoch_ticks.len() + 1];
                    let mut pending = Vec::new();
                    for observation in observations.iter().filter(|observation| observation.epoch() == epoch) {
                        match observation {
                            LaneObservation::Ticked { ordinal, .. } => {
                                if let Some(index) = epoch_ticks.iter().position(|(known, _)| known == ordinal) {
                                    segments[index].append(&mut pending);
                                }
                            }
                            other => pending.push(other.clone()),
                        }
                    }
                    segments[epoch_ticks.len()].append(&mut pending);
                    segments
                })
                .collect::<Vec<_>>();
            for index in 0..=epoch_ticks.len() {
                for lane in &segments {
                    trace.extend(lane[index].iter().filter_map(replayed));
                }
                if let Some((_, flush)) = epoch_ticks.get(index) {
                    trace.push(TraceEvent::Advance { clock: *flush });
                }
            }
        }
        if self.closed {
            trace.push(TraceEvent::Close);
        }
        Ok(trace)
    }
}

/// Returns the trace event that replays one observation: an `admit` of a routed buffer, or a `heartbeat` of a header.
fn replayed(observation: &LaneObservation) -> Option<TraceEvent> {
    match observation {
        LaneObservation::Admitted {
            source,
            lane,
            admitted_at,
            batch,
            ..
        } if !batch.is_empty() => Some(TraceEvent::Admit {
            source: source.clone(),
            lane: *lane,
            admitted_at: *admitted_at,
            batch: batch.clone(),
        }),
        LaneObservation::Admitted {
            source,
            lane,
            admitted_at,
            ..
        }
        | LaneObservation::Progress {
            source,
            lane,
            admitted_at,
            ..
        } => Some(TraceEvent::Heartbeat {
            source: source.clone(),
            lane: *lane,
            admitted_at: *admitted_at,
        }),
        LaneObservation::Ticked { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use saluki_core::data_model::event::metric::{context::Context, Metric};

    use super::*;
    use crate::interpret::{
        transition_tests::{aggregated, catalog, delivered, expected, ms, t, Delivered},
        Interpreter,
    };
    use crate::operators::OperatorRegistry;

    fn src() -> NodeId {
        "src".parse().unwrap()
    }

    fn data(lane: u16, at: AdmissionTime, epoch: u32, batch: Vec<Metric>) -> LaneObservation {
        LaneObservation::Admitted {
            source: src(),
            lane,
            admitted_at: at,
            epoch,
            batch,
        }
    }

    fn progress(lane: u16, at: AdmissionTime, epoch: u32) -> LaneObservation {
        LaneObservation::Progress {
            source: src(),
            lane,
            admitted_at: at,
            epoch,
        }
    }

    fn tick(epoch: u32, ordinal: u64, flush: AdmissionTime) -> LaneObservation {
        LaneObservation::Ticked { epoch, ordinal, flush }
    }

    fn counter(name: &'static str, value: f64) -> Metric {
        Metric::counter(Context::from_static_name(name), value)
    }

    fn gauge(name: &'static str, value: f64) -> Metric {
        Metric::gauge(Context::from_static_name(name), value)
    }

    /// Two state lanes of `src -> agg -> emit a` over two producer lanes, with the boot plan's installation first.
    fn replay() -> ArrivalReplay {
        ArrivalReplay::new(2).with_prelude(Trace::new().install(aggregated("10s", "a")).events().iter().cloned())
    }

    /// Runs a replay through the interpreter, with `src` on two lanes, in arrival order, and returns what `a` received,
    /// sorted.
    fn replayed_output(replay: &ArrivalReplay) -> Vec<Delivered> {
        let registry = OperatorRegistry::native();
        let catalog = catalog();
        let trace = replay.build().expect("the replay is valid");
        let run = Interpreter::new(&registry, &catalog)
            .with_source_lanes(src(), 2)
            .in_arrival_order()
            .run(&trace)
            .unwrap();
        let mut records = delivered(&run, "a");
        records.sort();
        records
    }

    fn sorted(records: &[(u64, u64, &str)]) -> Vec<Delivered> {
        let mut records = expected(records);
        records.sort();
        records
    }

    #[test]
    fn a_replay_keeps_what_each_lane_observed_in_order() {
        let mut replay = ArrivalReplay::new(2);
        replay.record(1, progress(0, AdmissionTime::from_unix_secs(10), 0));
        replay.record(1, tick(0, 1, AdmissionTime::from_unix_secs(10)));
        assert!(replay.observations(0).is_empty());
        assert_eq!(replay.observations(1).len(), 2);
        assert!(replay.observations(2).is_empty());
    }

    #[test]
    fn each_lanes_observations_before_its_firing_of_a_tick_go_before_the_tick_and_the_rest_after() {
        // State lane 0 holds `g`, state lane 1 holds `c`. Lane 0 fires the tick at 111 before it admits producer lane
        // 1's `g` at 111.5, whose bucket [110, 120) is open at the tick anyway; lane 1 admits its `c` from producer
        // lane 0 at 110.2 before it fires the tick. The replay keeps each state lane's order, one lane after the other,
        // so producer lane 0's stamps go 101, 110.2, 111, and then 104, as state lane 1 saw them: an arrival order.
        let mut replay = replay();
        replay.record(0, data(0, t(101), 0, vec![gauge("g", 1.0)]));
        replay.record(0, data(1, t(102), 0, vec![gauge("g", 2.0)]));
        replay.record(0, progress(0, ms(110_200), 0));
        replay.record(0, data(1, ms(110_500), 0, vec![gauge("g", 3.0)]));
        replay.record(1, data(1, t(103), 0, vec![counter("c", 1.0)]));
        replay.record(1, data(0, t(104), 0, vec![counter("c", 1.0)]));
        replay.record(1, data(0, ms(110_200), 0, vec![counter("c", 5.0)]));
        replay.record(1, progress(1, ms(110_500), 0));
        for lane in 0..2 {
            replay.record(lane, progress(0, t(111), 0));
            replay.record(lane, progress(1, t(111), 0));
        }
        replay.record(1, tick(0, 1, t(111)));
        replay.record(0, tick(0, 1, t(111)));
        replay.record(0, data(1, ms(111_500), 0, vec![gauge("g", 4.0)]));
        for lane in 0..2 {
            replay.record(lane, progress(0, t(121), 0));
            replay.record(lane, progress(1, t(121), 0));
            replay.record(lane, tick(0, 2, t(121)));
        }

        let trace = replay.build().unwrap();
        let shape = trace
            .events()
            .iter()
            .map(|event| match event {
                TraceEvent::Install { .. } => "install".to_string(),
                TraceEvent::Admit { lane, admitted_at, .. } => {
                    format!(
                        "admit {} {}",
                        lane,
                        admitted_at.as_unix_nanos() / 1_000_000 - super::super::transition_tests::B * 1_000
                    )
                }
                TraceEvent::Heartbeat { lane, admitted_at, .. } => format!(
                    "heartbeat {} {}",
                    lane,
                    admitted_at.as_unix_nanos() / 1_000_000 - super::super::transition_tests::B * 1_000
                ),
                TraceEvent::Advance { clock } => {
                    format!(
                        "advance {}",
                        clock.as_unix_nanos() / 1_000_000 - super::super::transition_tests::B * 1_000
                    )
                }
                other => other.name().to_string(),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            shape,
            [
                "install",
                // State lane 0, then state lane 1, up to each one's firing of the first tick.
                "admit 0 101000",
                "admit 1 102000",
                "heartbeat 0 110200",
                "admit 1 110500",
                "heartbeat 0 111000",
                "heartbeat 1 111000",
                "admit 1 103000",
                "admit 0 104000",
                "admit 0 110200",
                "heartbeat 1 110500",
                "heartbeat 0 111000",
                "heartbeat 1 111000",
                "advance 111000",
                "admit 1 111500",
                "heartbeat 0 121000",
                "heartbeat 1 121000",
                "heartbeat 0 121000",
                "heartbeat 1 121000",
                "advance 121000",
            ]
        );
        // The interpreter on the replay: `g`'s last arrival in [100, 110) is 2, its [110, 120) holds 3 then 4.
        assert_eq!(
            replayed_output(&replay),
            sorted(&[
                (1, 111_000, "c rate/10s [100:2]"),
                (1, 111_000, "g gauge [100:2]"),
                (1, 121_000, "c rate/10s [110:5]"),
                (1, 121_000, "g gauge [110:4]"),
            ])
        );
    }

    #[test]
    fn a_reset_goes_after_every_old_epoch_observation_and_before_every_newer_one() {
        // Producer lane 0 resets at 3,601. State lane 0 sees its new-epoch data before producer lane 1's last old-epoch
        // buffer, at 7,186; state lane 1 sees the new-epoch header first too. The replay states every old-epoch
        // observation before every new-epoch one, so the interpreter sees one reset, and the old epoch drains at the
        // earlier of the producer lanes' final old-epoch times, 7,190 and 7,186.
        let mut replay = replay();
        replay.record(0, data(0, t(7_185), 0, vec![counter("c", 1.0)]));
        replay.record(1, progress(0, t(7_190), 0));
        replay.record(0, progress(0, t(7_190), 0));
        replay.record(0, data(0, t(3_601), 1, vec![counter("c", 2.0)]));
        replay.record(1, progress(0, t(3_601), 1));
        replay.record(0, data(1, t(7_186), 0, vec![counter("c", 1.0)]));
        replay.record(1, progress(1, t(7_186), 0));
        replay.record(0, progress(1, t(3_602), 1));
        replay.record(1, progress(1, t(3_602), 1));
        for lane in 0..2 {
            replay.record(lane, progress(0, t(3_615), 1));
            replay.record(lane, progress(1, t(3_615), 1));
            replay.record(lane, tick(1, 1, t(3_615)));
        }
        let events = replay.build().unwrap();
        let epochs = events
            .events()
            .iter()
            .filter_map(|event| match event {
                TraceEvent::Admit { admitted_at, .. } | TraceEvent::Heartbeat { admitted_at, .. } => {
                    Some(admitted_at.as_unix_secs() > t(5_000).as_unix_secs())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let first_new = epochs.iter().position(|old| !old).unwrap();
        assert!(epochs[..first_new].iter().all(|old| *old), "{:?}", epochs);
        assert!(epochs[first_new..].iter().all(|old| !old), "{:?}", epochs);
        assert_eq!(
            replayed_output(&replay),
            sorted(&[
                (1, 7_186_000, "c rate/10s [7180:2]"),
                (1, 3_615_000, "c rate/10s [3600:2]"),
            ])
        );
    }

    #[test]
    fn ticks_without_a_shared_flush_time_exclude_the_replay_and_a_lane_flushes_what_it_saw_at_the_next_tick_it_fired() {
        let mut replay = replay();
        replay.record(0, tick(0, 1, t(111)));
        replay.record(1, tick(0, 1, t(112)));
        assert!(replay.build().unwrap_err().reason.contains("different flush times"));

        // A lane that did not fire a tick another lane fired, holding data or not, goes after the tick: like the lane,
        // the interpreter flushes what it saw at a later tick or drain.
        let mut replay = self::replay();
        replay.record(1, data(0, t(101), 0, vec![counter("c", 1.0)]));
        replay.record(0, progress(0, t(111), 0));
        replay.record(0, tick(0, 1, t(111)));
        let names = replay
            .build()
            .unwrap()
            .events()
            .iter()
            .map(TraceEvent::name)
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["install", "heartbeat", "advance", "admit"]);
        // A lane that missed a tick and fired the next places what it saw since its last tick before the next.
        let mut replay = self::replay();
        replay.record(0, data(0, t(101), 0, vec![counter("c", 1.0)]));
        replay.record(0, tick(0, 1, t(111)));
        replay.record(1, data(1, t(105), 0, vec![counter("d", 1.0)]));
        replay.record(1, tick(0, 2, t(121)));
        replay.record(0, tick(0, 2, t(121)));
        let names = replay
            .build()
            .unwrap()
            .events()
            .iter()
            .map(TraceEvent::name)
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["install", "admit", "advance", "admit", "advance"]);

        let mut replay = self::replay();
        replay.record(0, tick(0, 2, t(111)));
        replay.record(0, tick(0, 1, t(121)));
        assert!(replay
            .build()
            .unwrap_err()
            .reason
            .contains("after a later tick or twice"));

        let mut replay = self::replay();
        replay.record(0, tick(0, 1, t(121)));
        replay.record(1, tick(0, 2, t(111)));
        assert!(replay.build().unwrap_err().reason.contains("flush times move back"));

        let mut replay = self::replay();
        replay.exclude("the grace of tick 3 expired at state lane 1");
        assert_eq!(
            replay.build().unwrap_err(),
            ReplayExcluded {
                reason: "the grace of tick 3 expired at state lane 1".to_string()
            }
        );
    }

    #[test]
    fn a_closed_execution_replays_its_close_last() {
        let mut replay = replay();
        replay.record(0, data(0, t(101), 0, vec![counter("c", 1.0)]));
        replay.record(1, progress(1, t(101), 0));
        replay.close();
        let trace = replay.build().unwrap();
        assert!(matches!(trace.events().last(), Some(TraceEvent::Close)));
        assert_eq!(trace.events().len(), 4);
    }
}
