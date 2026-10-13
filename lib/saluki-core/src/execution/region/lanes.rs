//! Lanes: several tasks inside one region component, each running the region's program on an input of its own.
//!
//! A region with lanes is still one component: one component ID, one health registration, and one control mailbox. Its
//! host runs one **lane** task per lane, each with its own driver, its own input channel, and its own resource group,
//! and itself becomes the lanes' **coordinator**: it owns the mailbox, splits each control message into one per lane
//! ([`LaneSplit`]), sends each lane its share through the lane's own control queue, and acknowledges the message once
//! every lane accepted it, or at the first rejection; it takes each liveness probe and forwards it to every lane,
//! responding once all answered. This module holds the pieces the coordinator and its lanes share: what a lane is told
//! about itself ([`LaneContext`]), how a message splits, and the link between the coordinator and one lane
//! ([`lane_link`]).
//!
//! With one lane, a region host runs as it always has, without a coordinator.

use std::{
    fmt,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use tokio::sync::{mpsc, oneshot};

use crate::execution::control::{control_mailbox, ControlAck, ControlReceiver, ControlSender};

/// Which lane of its region a driver runs, and how many lanes the region has.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct LaneContext {
    /// The lane's index, below `count`.
    pub index: u16,

    /// The number of lanes of the region, at least one.
    pub count: u16,
}

impl LaneContext {
    /// The only lane of a region without lanes.
    pub const ONE: Self = Self { index: 0, count: 1 };

    /// Returns `true` if the region has only this lane.
    pub const fn is_only(&self) -> bool {
        self.count <= 1
    }
}

impl Default for LaneContext {
    fn default() -> Self {
        Self::ONE
    }
}

impl fmt::Display for LaneContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "lane {} of {}", self.index, self.count)
    }
}

/// A control message that a region with lanes splits into one message per lane.
///
/// The coordinator splits each message it receives and sends lane `i` the `i`-th part. A message whose parts differ per
/// lane, such as a preparation that carries one set of kernels per lane, splits into those parts; one that is the same
/// for every lane, such as an abort, splits into copies; and one that addresses a single lane does not split at all,
/// and names its lane ([`lane`][Self::lane]).
pub trait LaneSplit: Sized {
    /// Splits the message into exactly `lanes` messages, one per lane, in lane order.
    ///
    /// # Errors
    ///
    /// If the message cannot be split into `lanes` parts, such as a message for one lane, or one whose parts are for
    /// another number of lanes, it is returned unchanged; the coordinator then handles it as a whole: it sends it to
    /// the lane it names, and rejects it if it names none.
    fn split(self, lanes: u16) -> Result<Vec<Self>, Self>;

    /// Returns the one lane a message that does not split is for, if it names one.
    ///
    /// The default names none, so that a message that does not split is rejected.
    fn lane(&self) -> Option<u16> {
        None
    }
}

/// Creates the link between a region's coordinator and one of its lanes: the lane's control queue, the channel the
/// coordinator forwards liveness probes on, and the lane's report that it finished.
///
/// The coordinator keeps the [`LaneLink`], and the lane task the [`LaneEnd`].
pub fn lane_link<C>(lane: LaneContext) -> (LaneLink<C>, LaneEnd<C>) {
    let (control, mailbox) = control_mailbox();
    // One probe at a time: the coordinator forwards the next one only after the lane answered the last.
    let (probes, probe_rx) = mpsc::channel(1);
    let finished = Arc::new(AtomicBool::new(false));
    (
        LaneLink {
            lane,
            control,
            probes,
            finished: Arc::clone(&finished),
        },
        LaneEnd {
            lane,
            control: mailbox.into_receiver_for("region lane"),
            probes: LaneProbes { probes: probe_rx },
            finish: LaneFinish { finished },
        },
    )
}

/// The coordinator's side of the link to one lane.
pub struct LaneLink<C> {
    lane: LaneContext,
    control: ControlSender<C>,
    probes: mpsc::Sender<LaneProbe>,
    finished: Arc<AtomicBool>,
}

impl<C> LaneLink<C> {
    /// Returns the lane this link leads to.
    pub fn lane(&self) -> LaneContext {
        self.lane
    }

    /// Sends a control message to the lane's control queue, without waiting for the lane, and returns its
    /// acknowledgment.
    pub fn send(&self, message: C) -> ControlAck {
        self.control.send(message)
    }

    /// Returns `true` once the lane task has started reading its control queue.
    pub fn is_running(&self) -> bool {
        self.control.is_running()
    }

    /// Waits until the lane task has started reading its control queue.
    ///
    /// Returns `true` once it has, or `false` if the lane's end was dropped without the lane ever reading it, as when
    /// its task failed before it started.
    pub async fn running(&self) -> bool {
        self.control.running().await
    }

    /// Forwards one liveness probe to the lane and waits for its answer.
    ///
    /// Returns `true` once the lane answered, and `false` if the lane task has stopped, so that it can no longer
    /// answer.
    pub async fn probe(&self) -> bool {
        let (reply, answered) = oneshot::channel();
        if self.probes.send(LaneProbe { reply }).await.is_err() {
            return false;
        }
        answered.await.is_ok()
    }

    /// Returns `true` if the lane reported that it finished: its input ended and it completed its work.
    ///
    /// A lane reports it before its control queue and its probes close, so once either is seen closed, this tells a
    /// lane that finished from one that failed or was stopped.
    pub fn has_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }
}

impl<C> fmt::Debug for LaneLink<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LaneLink")
            .field("lane", &self.lane)
            .field("control", &self.control)
            .field("finished", &self.has_finished())
            .finish_non_exhaustive()
    }
}

/// The lane task's side of its link to the coordinator.
pub struct LaneEnd<C> {
    lane: LaneContext,
    control: ControlReceiver<C>,
    probes: LaneProbes,
    finish: LaneFinish,
}

impl<C> LaneEnd<C> {
    /// Returns the lane this end belongs to.
    pub fn lane(&self) -> LaneContext {
        self.lane
    }

    /// Splits the end into the lane's control queue, its probes, and its report that it finished, so that a lane can
    /// wait on its control queue and its probes at once, and report that it finished before it drops them.
    pub fn into_parts(self) -> (ControlReceiver<C>, LaneProbes, LaneFinish) {
        (self.control, self.probes, self.finish)
    }
}

/// A lane's report to its coordinator that it finished: its input ended and it completed its work.
///
/// The lane reports it before it drops its control queue and its probes, so that the coordinator, once it sees them
/// closed, knows the lane finished rather than failed: it then counts the lane as answering every later probe, and
/// sends it no part of a later control message. A lane that fails, or whose task ends otherwise, never reports it.
#[derive(Debug)]
pub struct LaneFinish {
    finished: Arc<AtomicBool>,
}

impl LaneFinish {
    /// Reports that the lane finished.
    pub fn report(self) {
        self.finished.store(true, Ordering::Release);
    }
}

impl<C> fmt::Debug for LaneEnd<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LaneEnd")
            .field("lane", &self.lane)
            .field("control", &self.control)
            .finish_non_exhaustive()
    }
}

/// The liveness probes the coordinator forwards to one lane.
#[derive(Debug)]
pub struct LaneProbes {
    probes: mpsc::Receiver<LaneProbe>,
}

impl LaneProbes {
    /// Waits for the next forwarded probe, which the lane answers with [`LaneProbe::respond`].
    ///
    /// Waits forever once the coordinator is gone, so that a lane can keep this in its `select!` loop.
    ///
    /// # Cancel safety
    ///
    /// This method is cancel-safe.
    pub async fn next(&mut self) -> LaneProbe {
        match self.probes.recv().await {
            Some(probe) => probe,
            None => std::future::pending().await,
        }
    }
}

/// One liveness probe a coordinator forwarded to a lane.
#[must_use = "a dropped probe reads as a lane that stopped"]
#[derive(Debug)]
pub struct LaneProbe {
    reply: oneshot::Sender<()>,
}

impl LaneProbe {
    /// Answers the probe: the lane is live.
    pub fn respond(self) {
        // The coordinator may have stopped waiting, which is fine.
        let _ = self.reply.send(());
    }
}

/// What the host of one lane of a region measures of its own work, for the lane's telemetry.
///
/// A region host given lane statistics ([`RegionHostBuilder::with_lane_stats`]) updates one per lane as the lane works,
/// from the lane's own task, so its writes are uncontended; a telemetry task reads them, typically once a second, and
/// publishes them per lane. Totals only grow, so a reader that publishes deltas subtracts what it read last. Each value
/// is read on its own, without a lock: one read can see a batch in one total and not yet in another.
///
/// What only the region's driver knows, such as how many buffers a keyed exit routed, is the driver's to count.
///
/// [`RegionHostBuilder::with_lane_stats`]: crate::execution::RegionHostBuilder::with_lane_stats
#[derive(Debug, Default)]
pub struct LaneStats {
    batches: AtomicU64,
    events: AtomicU64,
    busy_nanos: AtomicU64,
    input_depth: AtomicU64,
    stamp_wait_nanos: AtomicU64,
}

impl LaneStats {
    /// Creates statistics of a lane that has done nothing yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the number of batches the lane received on its input and processed, header-only ones included.
    ///
    /// The heartbeats the lane's own admission point stamps are not received, so they are not counted.
    pub fn batches(&self) -> u64 {
        self.batches.load(Ordering::Relaxed)
    }

    /// Returns the number of events in the batches the lane received on its input.
    pub fn events(&self) -> u64 {
        self.events.load(Ordering::Relaxed)
    }

    /// Returns how long the lane spent in calls into its driver, in all, each measured from the instant of the call's
    /// turn to its return.
    pub fn busy(&self) -> Duration {
        Duration::from_nanos(self.busy_nanos.load(Ordering::Relaxed))
    }

    /// Returns the number of batches that waited on the lane's input when the lane last ended a turn of input.
    pub fn input_depth(&self) -> u64 {
        self.input_depth.load(Ordering::Relaxed)
    }

    /// Returns how long the stamps of the lane's admission point waited for the point's lock, in all, as of the end of
    /// the lane's last turn of input. Only an installer's transaction holds the lock besides the lane's own stamps, so
    /// this grows only during transactions. Zero for a lane without an admission point.
    pub fn stamp_wait(&self) -> Duration {
        Duration::from_nanos(self.stamp_wait_nanos.load(Ordering::Relaxed))
    }

    /// Counts a batch of `events` events received on the lane's input.
    pub(super) fn record_batch(&self, events: u64) {
        self.batches.fetch_add(1, Ordering::Relaxed);
        self.events.fetch_add(events, Ordering::Relaxed);
    }

    /// Counts the time of one call into the driver.
    pub(super) fn record_busy(&self, busy: Duration) {
        self.busy_nanos.fetch_add(nanos(busy), Ordering::Relaxed);
    }

    /// Records the end of a turn of input: how many batches wait on the input, and the total stamp wait so far.
    pub(super) fn record_turn(&self, input_depth: usize, stamp_wait: Duration) {
        self.input_depth
            .store(u64::try_from(input_depth).unwrap_or(u64::MAX), Ordering::Relaxed);
        self.stamp_wait_nanos.store(nanos(stamp_wait), Ordering::Relaxed);
    }
}

/// Returns a duration in nanoseconds, saturating at `u64::MAX` (about 584 years).
fn nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use saluki_error::generic_error;

    use super::*;

    #[tokio::test]
    async fn a_lane_link_carries_control_messages_and_their_acknowledgments() {
        let lane = LaneContext { index: 1, count: 2 };
        let (link, end) = lane_link::<u32>(lane);
        assert_eq!(link.lane(), lane);
        assert_eq!(end.lane(), lane);
        let (mut control, _probes, _finish) = end.into_parts();
        assert!(!link.is_running());
        control.mark_running();
        assert!(link.is_running());

        let accepted = link.send(7);
        let rejected = link.send(8);
        assert!(!link.has_finished());
        let (message, reply) = control.recv().await.unwrap();
        assert_eq!(message, 7);
        reply.ack();
        let (message, reply) = control.recv().await.unwrap();
        assert_eq!(message, 8);
        reply.reject(generic_error!("lane 1 refuses"));

        assert!(accepted.await.is_ok());
        let error = rejected.await.unwrap_err().to_string();
        assert!(
            error.contains("region lane") && error.contains("lane 1 refuses"),
            "{}",
            error
        );
    }

    #[tokio::test]
    async fn a_forwarded_probe_returns_once_the_lane_answers_and_fails_once_the_lane_stopped() {
        let (link, end) = lane_link::<()>(LaneContext { index: 0, count: 2 });
        let (_control, mut probes, _finish) = end.into_parts();
        let lane = tokio::spawn(async move {
            probes.next().await.respond();
            // The second probe is dropped unanswered, as by a lane that stops.
            drop(probes.next().await);
        });
        assert!(link.probe().await);
        assert!(!link.probe().await);
        lane.await.unwrap();
        assert!(!link.probe().await);
    }

    #[tokio::test]
    async fn a_lane_that_reports_its_finish_before_it_stops_reads_as_finished_once_its_link_sees_it_stopped() {
        let (link, end) = lane_link::<()>(LaneContext { index: 0, count: 2 });
        let (control, probes, finish) = end.into_parts();
        assert!(!link.has_finished());
        finish.report();
        drop((control, probes));
        assert!(!link.probe().await);
        assert!(link.has_finished());

        // A lane that stops without reporting, as one that fails, does not read as finished.
        let (link, end) = lane_link::<()>(LaneContext { index: 1, count: 2 });
        drop(end);
        assert!(!link.probe().await);
        assert!(!link.has_finished());
    }

    #[test]
    fn lane_stats_count_batches_events_and_busy_time_and_keep_the_latest_depth_and_stamp_wait() {
        let stats = LaneStats::new();
        stats.record_batch(3);
        stats.record_batch(0);
        stats.record_busy(Duration::from_micros(5));
        stats.record_busy(Duration::from_micros(7));
        stats.record_turn(4, Duration::from_millis(2));
        stats.record_turn(1, Duration::from_millis(3));
        assert_eq!((stats.batches(), stats.events()), (2, 3));
        assert_eq!(stats.busy(), Duration::from_micros(12));
        assert_eq!((stats.input_depth(), stats.stamp_wait()), (1, Duration::from_millis(3)));
    }

    #[test]
    fn the_only_lane_is_the_default() {
        assert_eq!(LaneContext::default(), LaneContext::ONE);
        assert!(LaneContext::ONE.is_only());
        assert!(!LaneContext { index: 0, count: 2 }.is_only());
        assert_eq!(LaneContext { index: 1, count: 4 }.to_string(), "lane 1 of 4");
    }
}
