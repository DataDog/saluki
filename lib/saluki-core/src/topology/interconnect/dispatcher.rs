use std::{
    borrow::Cow,
    num::NonZeroU16,
    sync::{
        atomic::{AtomicBool, AtomicU16, Ordering},
        Arc,
    },
    time::Instant,
};

use futures::future::select_all;
use saluki_common::collections::FastHashMap;
use saluki_error::{generic_error, GenericError};
use saluki_metrics::{static_metrics, Counter, Histogram};
use tokio::sync::mpsc::{self, error::TrySendError, Permit};

use super::{consumer::ReceivedMetrics, Dispatchable};
#[cfg(feature = "measurement")]
use crate::probes;
use crate::{
    components::ComponentContext,
    execution::{AdmissionPoint, OwnerToken},
    topology::{ComponentId, EventsBuffer, LaneEdge, LaneRoute, OutputName},
};

// TODO: When we have support for additional static labels on a per-metric basis, add `discard_reason` to
// `events_discarded_total` metric to indicate that it's due to the destination component being disconnected.
#[static_metrics(prefix = component, labels(component_id, component_type, output))]
#[derive(Clone)]
struct DispatcherMetrics {
    events_sent_total: Counter,
    #[metric(level = trace)]
    send_latency_seconds: Histogram,
    events_discarded_total: Counter,
}

impl DispatcherMetrics {
    fn default_output(context: ComponentContext) -> Self {
        Self::with_output_name(context, "_default")
    }

    fn named_output(context: ComponentContext, output_name: &str) -> Self {
        Self::with_output_name(context, output_name)
    }

    fn with_output_name(context: ComponentContext, output_name: &str) -> Self {
        Self::new(context.component_id(), context.component_type().as_str(), output_name)
    }
}

/// A type that can be used as a buffer for dispatching items.
pub trait DispatchBuffer: Dispatchable + Default {
    /// Type of item that can be pushed into the buffer.
    type Item;

    /// Returns the number of items currently in the buffer.
    fn len(&self) -> usize;

    /// Returns `true` if the buffer is full.
    fn is_full(&self) -> bool;

    /// Attempts to push an item into the buffer.
    ///
    /// Returns `Some(item)` if the buffer is full and the item couldn't be pushed.
    fn try_push(&mut self, item: Self::Item) -> Option<Self::Item>;
}

/// The measurement probes of one output (see [`probes`][crate::probes]).
#[cfg(feature = "measurement")]
struct DispatchProbes {
    lock_wait: Arc<probes::Histogram>,
    lock_hold: Arc<probes::Histogram>,
    send_wait: Arc<probes::Histogram>,
    clone: Arc<probes::Histogram>,
    items: Arc<probes::Histogram>,
}

#[cfg(feature = "measurement")]
impl DispatchProbes {
    fn new(context: &ComponentContext, output_name: &str) -> Self {
        let subject = format!("{}.{}", context.component_id(), output_name);
        let histogram = |probe: &str| probes::histogram(&format!("{}/{}", probe, subject));
        Self {
            lock_wait: histogram("admission_lock_wait"),
            lock_hold: histogram("admission_lock_hold"),
            send_wait: histogram("dispatch_send_wait"),
            clone: histogram("dispatch_clone"),
            items: histogram("dispatch_items"),
        }
    }
}

/// A gate on the input of a component that only observes what it receives, such as a tap that collects statistics on
/// demand.
///
/// While the gate is closed, a dispatcher sending to the component skips it: it neither clones what it sends nor sends
/// it there, and counts what it skipped as received by the component instead, in the component's own
/// `component_events_received_total` and `component_events_received_size` series, which therefore keep the values
/// they would have had. The dispatcher's own series count the component as sent to, as before. While the gate is open,
/// the component receives everything, as any component does. A component opens its gate when it starts observing, and
/// closes it when it stops: what was sent while it was closed never reaches it, and what was sent before it closed may
/// still arrive, for the component to discard.
///
/// Clones share one gate. A gate starts closed.
#[derive(Clone, Debug, Default)]
pub struct InputGate {
    open: Arc<AtomicBool>,
}

impl InputGate {
    /// Creates a closed gate.
    pub fn closed() -> Self {
        Self::default()
    }

    /// Opens the gate: senders deliver to the component from their next send.
    pub fn open(&self) {
        self.open.store(true, Ordering::Release);
    }

    /// Closes the gate: senders skip the component from their next send.
    pub fn close(&self) {
        self.open.store(false, Ordering::Release);
    }

    /// Returns `true` if the gate is open.
    pub fn is_open(&self) -> bool {
        self.open.load(Ordering::Acquire)
    }
}

/// The gate of a sender's component, and the counters of what the component received, which a skipped send adds to.
struct GatedSender {
    gate: InputGate,
    received: ReceivedMetrics,
}

/// Returns the error of a send to a channel whose receiving component has stopped.
fn closed_output() -> GenericError {
    generic_error!("Failed to send to output.")
}

/// The lane of the one component an output routes to, as the route of a send resolved it (see
/// [`DispatchTarget::routed`]).
enum RoutedLane<'a> {
    /// This lane.
    Lane(usize),

    /// The first lane with room, in rotation from this counter, or from the output's own rotation without one.
    Spread(Option<&'a mut u16>),
}

/// The senders to one component that an output feeds: one per input lane of the component, and how a send picks one.
struct OutputSender<T> {
    /// One sender per lane, in lane order: one for a component without lanes.
    lanes: Vec<mpsc::Sender<T>>,

    /// How a send picks a lane: `Fixed(0)` for a component with one lane.
    edge: LaneEdge,

    /// Where the next spread send that brings no rotation counter of its own starts trying lanes.
    ///
    /// Several tasks may share a dispatcher; on an admission point, they update it under the point's lock, as they
    /// send, and otherwise a race between them only changes which lane with room a buffer goes to.
    rotation: AtomicU16,

    /// The component the senders feed, where it is known, so that heartbeats can go to some components only.
    component: Option<ComponentId>,

    /// The gate of the component's input, for a component that has one.
    gate: Option<GatedSender>,
}

impl<T> OutputSender<T>
where
    T: Dispatchable,
{
    /// Returns the senders to the lanes that receive an admission point's heartbeats on the output: the lane a fixed
    /// edge leads to, the same path as the point's batches, and every lane of a routed edge, since each lane follows
    /// the point's progress on its own.
    fn heartbeat_lanes(&self) -> &[mpsc::Sender<T>] {
        match self.edge {
            LaneEdge::Fixed(lane) => {
                let lane = usize::from(lane);
                &self.lanes[lane..=lane]
            }
            _ => &self.lanes,
        }
    }

    /// Returns the lane after `lane`, which a spread send leaves its rotation at.
    fn lane_after(&self, lane: usize) -> u16 {
        // There are at most `u16::MAX` lanes (checked when the senders are attached), so every lane index fits.
        u16::try_from((lane + 1) % self.lanes.len()).unwrap_or_default()
    }

    /// Returns the lane a spread send starts trying lanes at: `counter`, or the output's own rotation without one.
    fn spread_start(&self, counter: Option<&u16>) -> usize {
        let start = counter
            .copied()
            .unwrap_or_else(|| self.rotation.load(Ordering::Relaxed));
        usize::from(start) % self.lanes.len()
    }

    /// Leaves the rotation of a spread send at the lane after `lane`, which it sent to.
    fn advance(&self, counter: Option<&mut u16>, lane: usize) {
        let next = self.lane_after(lane);
        match counter {
            Some(counter) => *counter = next,
            None => self.rotation.store(next, Ordering::Relaxed),
        }
    }

    /// Sends `item` to the lane its route picked, or, on an edge that is not routed, to the edge's lane.
    ///
    /// A spread send goes to the first lane with room, in rotation, and waits only when every lane is full.
    async fn send(&self, item: T, routed: Option<RoutedLane<'_>>) -> Result<(), GenericError> {
        let lane = match (self.edge, routed) {
            (LaneEdge::Fixed(lane), _) => usize::from(lane),
            (_, Some(RoutedLane::Lane(lane))) => lane,
            (_, Some(RoutedLane::Spread(counter))) => return self.send_spread(item, counter).await,
            // The output resolves the route of its routed sender before every send, so this sends nowhere else.
            (_, None) => 0,
        };
        self.lanes[lane].send(item).await.map_err(|_| closed_output())
    }

    async fn send_spread(&self, item: T, mut counter: Option<&mut u16>) -> Result<(), GenericError> {
        let start = self.spread_start(counter.as_deref());
        let (lane, permit) = match self.try_reserve_spread(start)? {
            Some(reserved) => reserved,
            None => self.reserve_first().await?,
        };
        permit.send(item);
        self.advance(counter.take(), lane);
        Ok(())
    }

    /// Reserves room in the first lane that has room, trying lanes in rotation from `start`, or returns `None` if every
    /// lane is full.
    fn try_reserve_spread(&self, start: usize) -> Result<Option<(usize, Permit<'_, T>)>, GenericError> {
        let lanes = self.lanes.len();
        for offset in 0..lanes {
            let lane = (start + offset) % lanes;
            match self.lanes[lane].try_reserve() {
                Ok(permit) => return Ok(Some((lane, permit))),
                Err(TrySendError::Full(())) => {}
                Err(TrySendError::Closed(())) => return Err(closed_output()),
            }
        }
        Ok(None)
    }

    /// Waits for room in whichever lane has room first.
    async fn reserve_first(&self) -> Result<(usize, Permit<'_, T>), GenericError> {
        // Dropping the other reservations gives back whatever room they were granted meanwhile.
        let reservations = self
            .lanes
            .iter()
            .enumerate()
            .map(|(lane, sender)| Box::pin(async move { (lane, sender.reserve().await) }));
        let ((lane, reserved), _, _) = select_all(reservations).await;
        let permit = reserved.map_err(|_| closed_output())?;
        Ok((lane, permit))
    }

    /// Reserves room in the lane a send with `routed` goes to, without waiting: `None` if that lane is full, or, for a
    /// spread send, if every lane is.
    fn try_reserve(&self, routed: Option<&RoutedLane<'_>>) -> Result<Option<(usize, Permit<'_, T>)>, GenericError> {
        let lane = match (self.edge, routed) {
            (LaneEdge::Fixed(lane), _) => usize::from(lane),
            (_, Some(RoutedLane::Lane(lane))) => *lane,
            (_, Some(RoutedLane::Spread(counter))) => {
                return self.try_reserve_spread(self.spread_start(counter.as_deref()))
            }
            (_, None) => 0,
        };
        match self.lanes[lane].try_reserve() {
            Ok(permit) => Ok(Some((lane, permit))),
            Err(TrySendError::Full(())) => Ok(None),
            Err(TrySendError::Closed(())) => Err(closed_output()),
        }
    }
}

struct DispatchTarget<T> {
    output: OutputName,
    metrics: DispatcherMetrics,
    #[cfg(feature = "measurement")]
    probes: DispatchProbes,

    // One per component the output feeds, in the order they were attached.
    senders: Vec<OutputSender<T>>,

    // The index of the one sender whose lanes the output routes to by key, by affinity, or by spreading, if any. Each
    // send names one route, which only that sender follows; every other sender has one lane, or is fixed to one.
    routed: Option<usize>,
    admission: Option<Arc<AdmissionPoint>>,

    // Held while the dispatcher lives, so that the admission point sends no heartbeat once the component stops.
    _admission_owner: Option<OwnerToken>,
}

impl<T> DispatchTarget<T>
where
    T: Dispatchable,
{
    fn default_output(context: ComponentContext) -> Self {
        Self {
            output: OutputName::Default,
            #[cfg(feature = "measurement")]
            probes: DispatchProbes::new(&context, "_default"),
            metrics: DispatcherMetrics::default_output(context),
            senders: Vec::new(),
            routed: None,
            admission: None,
            _admission_owner: None,
        }
    }

    fn named_output(context: ComponentContext, output_name: Cow<'static, str>) -> Self {
        Self {
            #[cfg(feature = "measurement")]
            probes: DispatchProbes::new(&context, &output_name),
            metrics: DispatcherMetrics::named_output(context, &output_name),
            output: OutputName::Given(output_name),
            senders: Vec::new(),
            routed: None,
            admission: None,
            _admission_owner: None,
        }
    }

    /// Adds the senders to one component's lanes, which a send picks among by `edge`.
    fn add_sender(
        &mut self, lanes: Vec<mpsc::Sender<T>>, edge: LaneEdge, component: Option<ComponentId>,
        gate: Option<GatedSender>,
    ) -> Result<(), GenericError> {
        let feeds = || {
            component
                .as_ref()
                .map_or_else(|| "a component".to_string(), |component| format!("'{}'", component))
        };
        if lanes.is_empty() || lanes.len() > usize::from(u16::MAX) {
            return Err(generic_error!(
                "Cannot attach {} senders to output '{}' for {}: a component has 1 to {} input lanes, and each lane \
                 one sender.",
                lanes.len(),
                self.output,
                feeds(),
                u16::MAX
            ));
        }
        // An edge into a component with one lane leads to that lane, whatever it names.
        let edge = if lanes.len() == 1 && edge.is_routed() {
            LaneEdge::Fixed(0)
        } else {
            edge
        };
        if let LaneEdge::Fixed(lane) = edge {
            if usize::from(lane) >= lanes.len() {
                return Err(generic_error!(
                    "Cannot attach output '{}' to lane {} of {}, which has {} input lane(s). Attach it to a lane the \
                     component has.",
                    self.output,
                    lane,
                    feeds(),
                    lanes.len()
                ));
            }
        } else if let Some(routed) = self.routed {
            return Err(generic_error!(
                "Cannot route output '{}' to the lanes of {}: it already routes to the lanes of {}, and each send \
                 names one route. Give each component with lanes an output of its own.",
                self.output,
                feeds(),
                self.senders[routed].component.as_ref().map_or_else(
                    || "another component".to_string(),
                    |component| format!("'{}'", component)
                )
            ));
        } else {
            self.routed = Some(self.senders.len());
        }
        self.senders.push(OutputSender {
            lanes,
            edge,
            rotation: AtomicU16::new(0),
            component,
            gate,
        });
        Ok(())
    }

    /// Returns `true` if the sender at `index` delivers: it has no gate, or its gate is open.
    fn delivers(&self, index: usize) -> bool {
        self.senders[index]
            .gate
            .as_ref()
            .is_none_or(|gated| gated.gate.is_open())
    }

    /// Counts an item the sender at `index` skipped as received by its component, which must have a gate.
    fn count_skipped(&self, index: usize, item_count: usize) {
        if let Some(gated) = &self.senders[index].gate {
            gated.received.record(item_count);
        }
    }

    /// Resolves the lane that `route` names for the one sender the output routes to by key, by affinity, or by
    /// spreading, which is `None` if there is none: every other sender ignores the route, and a spread counter is left
    /// at 0, the lane after the only lane.
    ///
    /// A send that names no route is the send of a sender without lanes, whose only lane is lane 0.
    ///
    /// # Errors
    ///
    /// If the route does not fit the routing of the sender's edge, or names a lane its component does not have, an
    /// error is returned.
    fn resolve_route<'r>(&self, route: Option<LaneRoute<'r>>) -> Result<Option<RoutedLane<'r>>, GenericError> {
        let Some(index) = self.routed else {
            if let Some(LaneRoute::Spread(counter)) = route {
                *counter = 0;
            }
            return Ok(None);
        };
        let sender = &self.senders[index];
        let lanes = sender.lanes.len();
        let routed = match (sender.edge, route) {
            (LaneEdge::Keyed, Some(LaneRoute::Target(lane))) if usize::from(lane) < lanes => {
                RoutedLane::Lane(usize::from(lane))
            }
            (LaneEdge::Affinity, Some(LaneRoute::From(lane))) => RoutedLane::Lane(usize::from(lane) % lanes),
            (LaneEdge::Affinity, None) => RoutedLane::Lane(0),
            (LaneEdge::Spread, Some(LaneRoute::Spread(counter))) => RoutedLane::Spread(Some(counter)),
            (LaneEdge::Spread, None) => RoutedLane::Spread(None),
            (edge, route) => return Err(self.route_error(sender, edge, route.as_ref())),
        };
        Ok(Some(routed))
    }

    /// Returns the error of a send whose route does not fit the edge of the sender the output routes to.
    fn route_error(&self, sender: &OutputSender<T>, edge: LaneEdge, route: Option<&LaneRoute<'_>>) -> GenericError {
        let component = sender
            .component
            .as_ref()
            .map_or_else(|| "its component".to_string(), |component| format!("'{}'", component));
        let named = match route {
            None => "no route".to_string(),
            Some(LaneRoute::Target(lane)) => format!("a route to lane {}", lane),
            Some(LaneRoute::From(lane)) => format!("a route from lane {}", lane),
            Some(LaneRoute::Spread(_)) => "a spread route".to_string(),
        };
        if let (LaneEdge::Keyed, Some(LaneRoute::Target(lane))) = (edge, route) {
            return generic_error!(
                "Cannot send on output '{}' to lane {} of {}, which has {} input lanes. Route each send to a lane the \
                 component has.",
                self.output,
                lane,
                component,
                sender.lanes.len()
            );
        }
        let needed = match edge {
            LaneEdge::Keyed => "the lane the sender chose for it (`LaneRoute::Target`)",
            LaneEdge::Affinity => "the sender's own lane (`LaneRoute::From`), or no route",
            _ => "a rotation counter (`LaneRoute::Spread`), or no route",
        };
        generic_error!(
            "Cannot send on output '{}' with {}: the output routes to the lanes of {} by {} routing, so each send \
             names {}.",
            self.output,
            named,
            component,
            edge,
            needed
        )
    }

    async fn send(&self, mut item: T, route: Option<LaneRoute<'_>>) -> Result<(), GenericError> {
        // Resolve the route first, so that a send whose route does not fit the output stamps and sends nothing.
        let mut routed = self.resolve_route(route)?;

        // On an admission point, stamp the item and keep the point locked until the item has been sent to every
        // sender: every downstream channel then receives the point's items in stamp order, even when several tasks
        // share this dispatcher.
        let _admission = match &self.admission {
            Some(point) => {
                #[cfg(feature = "measurement")]
                let lock_wait = probes::Span::start(&self.probes.lock_wait);
                let mut guard = point.lock().await;
                #[cfg(feature = "measurement")]
                drop(lock_wait);
                if let Some(header) = item.header_mut() {
                    guard.stamp(header);
                }
                Some(guard)
            }
            None => None,
        };
        // Declared after the guard, so that it ends just before the guard is released.
        #[cfg(feature = "measurement")]
        let _lock_hold = self
            .admission
            .as_ref()
            .map(|_| probes::Span::start(&self.probes.lock_hold));

        if self.senders.is_empty() {
            self.discard(&item);
            return Ok(());
        }

        let start = Instant::now();
        let item_count = item.item_count();
        #[cfg(feature = "measurement")]
        self.probes.items.record(item_count as u64);

        // The last sender that delivers takes the item itself, and every other one that delivers a clone. A sender
        // whose component's gate is closed is skipped, and what it would have sent counted as received there. Each
        // gate is decided once: a sender after the last one found delivering is skipped even if its gate opens in the
        // meantime, and the last one takes the item even if its gate closes. Each sender sends to one lane of its
        // component: the lane its edge is fixed to, or, for the sender the output routes to, the lane the route picks.
        saluki_antithesis::always_gt!(self.senders.len(), 0, "dispatcher fanout has at least one sender");
        let last_delivering = (0..self.senders.len()).rev().find(|&index| self.delivers(index));
        let mut item = Some(item);
        for (index, sender) in self.senders.iter().enumerate() {
            let to_send = match last_delivering {
                Some(last) if index == last => item.take(),
                Some(last) if index < last && self.delivers(index) => {
                    #[cfg(feature = "measurement")]
                    let _clone = probes::Span::start(&self.probes.clone);
                    item.clone()
                }
                _ => None,
            };
            match to_send {
                Some(to_send) => {
                    #[cfg(feature = "measurement")]
                    let _send_wait = probes::Span::start(&self.probes.send_wait);
                    let lane = if self.routed == Some(index) {
                        routed.take()
                    } else {
                        None
                    };
                    sender.send(to_send, lane).await?
                }
                None => self.count_skipped(index, item_count),
            }
        }

        self.record_sent(start, item_count);
        Ok(())
    }

    /// Sends `item` as [`send`][Self::send] does if the lane it goes to has room now, and otherwise returns it, unsent.
    ///
    /// Only an output that feeds one component tries: on an output that feeds several, the item is always returned,
    /// since a caller that holds back its later items only for the lanes a returned item could go to would otherwise
    /// let them overtake it in a full component it cannot see. An item on an admission point is always returned too,
    /// since its stamp takes the point's lock, which a send may have to wait for.
    fn try_send(&self, item: T, route: Option<LaneRoute<'_>>) -> Result<Option<T>, GenericError> {
        let routed = self.resolve_route(route)?;
        if self.admission.is_some() || self.senders.len() > 1 {
            return Ok(Some(item));
        }
        let Some(sender) = self.senders.first() else {
            self.discard(&item);
            return Ok(None);
        };

        // The one sender is the routed one, if the output routes to any.
        let start = Instant::now();
        let item_count = item.item_count();
        if self.delivers(0) {
            let Some((lane, permit)) = sender.try_reserve(routed.as_ref())? else {
                return Ok(Some(item));
            };
            permit.send(item);
            if let Some(RoutedLane::Spread(counter)) = routed {
                sender.advance(counter, lane);
            }
        } else {
            self.count_skipped(0, item_count);
        }

        // A returned item is recorded when it is sent.
        #[cfg(feature = "measurement")]
        self.probes.items.record(item_count as u64);
        self.record_sent(start, item_count);
        Ok(None)
    }

    /// Counts an item sent on an output without senders as discarded.
    fn discard(&self, item: &T) {
        // Track discarded events when no senders are attached to this output
        let item_count = item.item_count() as u64;
        self.metrics.events_discarded_total().increment(item_count);
        // Anchor the legitimate zero-sender discard. A wired edge never reaches this branch, so this stays a
        // disconnected-output signal — not a silent-loss-on-a-wired-edge violation.
        saluki_antithesis::sometimes!(
            true,
            "events discarded on a zero-sender output",
            { "items": item_count }
        );
    }

    /// Records the latency of a send that started at `start`, and its `item_count` items as sent to every sender.
    fn record_sent(&self, start: Instant, item_count: usize) {
        let elapsed = start.elapsed();

        // TODO: We should consider splitting this out per-sender somehow. We would need to carry around the
        // destination component's ID, though, to properly associate it.
        self.metrics.send_latency_seconds().record(elapsed);

        let total_events_sent = (self.senders.len() * item_count) as u64;
        self.metrics.events_sent_total().increment(total_events_sent);
    }
}

/// A buffered dispatcher.
///
/// `BufferedDispatcher` provides an efficient and ergonomic interface to `Dispatcher` that allows for writing events
/// one-by-one into batches, which are then dispatched to the configured output as needed. This allows callers to focus
/// on the logic around what items to send, without needing to worry about the details of event buffer sizing or
/// flushing.
pub struct BufferedDispatcher<'a, T> {
    metrics: &'a DispatcherMetrics,
    flushed_len: usize,
    buffer: Option<T>,
    target: &'a DispatchTarget<T>,
}

impl<'a, T> BufferedDispatcher<'a, T> {
    fn new(target: &'a DispatchTarget<T>) -> Self {
        Self {
            metrics: &target.metrics,
            flushed_len: 0,
            buffer: None,
            target,
        }
    }
}

impl<T> BufferedDispatcher<'_, T>
where
    T: DispatchBuffer,
{
    async fn try_flush_buffer(&self, buffer: T) -> Result<(), GenericError> {
        let buffer_len = buffer.len();
        if buffer_len > 0 {
            self.target.send(buffer, None).await
        } else {
            Ok(())
        }
    }

    /// Pushes an item into the buffered dispatcher.
    ///
    /// # Errors
    ///
    /// If there is an error flushing items to the output, or if there is an error acquiring a new buffer, an error
    /// is returned.
    pub async fn push(&mut self, item: T::Item) -> Result<(), GenericError> {
        // If our current buffer is full, flush it before acquiring a new one.
        if let Some(old_buffer) = self.buffer.take_if(|b| b.is_full()) {
            self.try_flush_buffer(old_buffer).await?;
        }

        // Add the item to our current buffer.
        //
        // If our current buffer is empty, create a new one first. If the current buffer is full, return an error
        // because it should be impossible to get a new buffer that is full.
        let buffer = self.buffer.get_or_insert_default();
        if buffer.try_push(item).is_some() {
            return Err(generic_error!("Dispatch buffer already full after acquisition."));
        }

        self.flushed_len += 1;

        Ok(())
    }

    /// Consumes this buffered dispatcher and sends/flushes all input items to the underlying output.
    ///
    /// If flushing is successful, `Ok(flushed)` is returned, where `flushed` is the total number of items that
    /// have been flushed through this buffered dispatcher.
    ///
    /// # Errors
    ///
    /// If there is an error sending items to the output, an error is returned.
    pub async fn send_all<I>(mut self, items: I) -> Result<usize, GenericError>
    where
        I: IntoIterator<Item = T::Item>,
    {
        for item in items {
            self.push(item).await?;
        }

        self.flush().await
    }

    /// Consumes this buffered dispatcher, flushing any buffered items to the underlying output.
    ///
    /// If flushing is successful, `Ok(flushed)` is returned, where `flushed` is the total number of items that have
    /// been flushed through this buffered dispatcher.
    ///
    /// # Errors
    ///
    /// If there is an error sending items to the output, an error is returned.
    pub async fn flush(mut self) -> Result<usize, GenericError> {
        if let Some(old_buffer) = self.buffer.take() {
            self.try_flush_buffer(old_buffer).await?;
        }

        // We increment the "events sent" metric here because we want to count the number of buffered items, vs doing it in
        // `DispatchTarget::send` where all it knows is that it sent one item.
        self.metrics.events_sent_total().increment(self.flushed_len as u64);

        Ok(self.flushed_len)
    }
}

/// Dispatches items from one component to another.
///
/// [`Dispatcher`] provides an ergonomic interface for sending items to a downstream component. It has support for
/// multiple outputs (a default output, and additional "named" outputs) and provides telemetry around the number of
/// dispatched items as well as the latency of sending them.
pub struct Dispatcher<T>
where
    T: Dispatchable,
{
    context: ComponentContext,
    default: Option<DispatchTarget<T>>,
    targets: FastHashMap<Cow<'static, str>, DispatchTarget<T>>,
}

impl<T> Dispatcher<T>
where
    T: Dispatchable,
{
    /// Create a new `Dispatcher` for the given component context.
    pub fn new(context: ComponentContext) -> Self {
        Self {
            context,
            default: None,
            targets: FastHashMap::default(),
        }
    }

    /// Adds an output to the dispatcher.
    ///
    /// # Errors
    ///
    /// If the output already exists, an error is returned.
    pub fn add_output(&mut self, output_name: OutputName) -> Result<(), GenericError> {
        match output_name {
            OutputName::Default => {
                if self.default.is_some() {
                    return Err(generic_error!("Default output already exists."));
                }

                self.default = Some(DispatchTarget::default_output(self.context.clone()));
            }
            OutputName::Given(name) => {
                if self.targets.contains_key(&name) {
                    return Err(generic_error!("Output '{}' already exists.", name));
                }
                let target = DispatchTarget::named_output(self.context.clone(), name.clone());
                self.targets.insert(name, target);
            }
        }

        Ok(())
    }

    /// Attaches a sender to the given output.
    ///
    /// # Errors
    ///
    /// If the output doesn't exist, or already has an admission point, an error is returned.
    pub fn attach_sender_to_output(
        &mut self, output_name: &OutputName, sender: mpsc::Sender<T>,
    ) -> Result<(), GenericError> {
        self.attach_lanes(output_name, vec![sender], LaneEdge::Fixed(0), None, None)
    }

    /// Attaches a sender that feeds the component `component_id` to the given output.
    ///
    /// This is [`attach_sender_to_output`][Self::attach_sender_to_output] for a sender whose downstream component is
    /// known, which an admission point needs in order to send heartbeats only to some of the components its output
    /// feeds.
    ///
    /// # Errors
    ///
    /// If the output doesn't exist, or already has an admission point, an error is returned.
    pub fn attach_component_sender_to_output(
        &mut self, output_name: &OutputName, component_id: ComponentId, sender: mpsc::Sender<T>,
    ) -> Result<(), GenericError> {
        self.attach_lanes(output_name, vec![sender], LaneEdge::Fixed(0), Some(component_id), None)
    }

    /// Attaches the senders to the input lanes of the component `component_id`, one per lane in lane order, to the
    /// given output, which picks the lane of each send by `edge` (see [`LaneEdge`]).
    ///
    /// The output then sends each item to one lane of the component: the lane a fixed edge leads to, or the lane the
    /// route of a send picks ([`dispatch_named_to_lane`][Self::dispatch_named_to_lane]). With one sender, every item
    /// goes to it, whatever `edge` names, as
    /// [`attach_component_sender_to_output`][Self::attach_component_sender_to_output] attaches it. An admission point
    /// on the output sends its heartbeats to the lane a fixed edge leads to, or to every lane of a routed one.
    ///
    /// # Errors
    ///
    /// If the output doesn't exist or already has an admission point, if `lanes` is empty or holds more than `u16::MAX`
    /// senders, if `edge` is fixed to a lane the component does not have, or if `edge` routes to the lanes of a
    /// component with lanes and the output already routes to another one's, an error is returned. A send names one
    /// route, so an output routes by key, by affinity, or by spreading to the lanes of one component only.
    pub fn attach_component_lanes_to_output(
        &mut self, output_name: &OutputName, component_id: ComponentId, lanes: Vec<mpsc::Sender<T>>, edge: LaneEdge,
    ) -> Result<(), GenericError> {
        self.attach_lanes(output_name, lanes, edge, Some(component_id), None)
    }

    /// Attaches the senders to the input lanes of the component `component_id`, whose input has the gate `gate`, to
    /// the given output, as [`attach_component_lanes_to_output`][Self::attach_component_lanes_to_output] does: while
    /// the gate is closed, the output skips the component, and records what it skipped in `received`, the counters of
    /// what the component received. See [`InputGate`].
    ///
    /// # Errors
    ///
    /// The errors of [`attach_component_lanes_to_output`][Self::attach_component_lanes_to_output].
    pub(crate) fn attach_gated_lanes_to_output(
        &mut self, output_name: &OutputName, component_id: ComponentId, lanes: Vec<mpsc::Sender<T>>, edge: LaneEdge,
        gate: InputGate, received: ReceivedMetrics,
    ) -> Result<(), GenericError> {
        let gated = GatedSender { gate, received };
        self.attach_lanes(output_name, lanes, edge, Some(component_id), Some(gated))
    }

    fn attach_lanes(
        &mut self, output_name: &OutputName, lanes: Vec<mpsc::Sender<T>>, edge: LaneEdge,
        component_id: Option<ComponentId>, gate: Option<GatedSender>,
    ) -> Result<(), GenericError> {
        let target = self.get_output_mut(output_name)?;
        if target.admission.is_some() {
            // The admission point captured the output's senders for its heartbeats, so a later sender would miss them.
            return Err(generic_error!(
                "Cannot attach a sender to output '{}' after it became an admission point. Attach every sender \
                 before setting the admission point.",
                output_name
            ));
        }
        target.add_sender(lanes, edge, component_id, gate)
    }

    fn get_output_mut(&mut self, output_name: &OutputName) -> Result<&mut DispatchTarget<T>, GenericError> {
        match output_name {
            OutputName::Default => self
                .default
                .as_mut()
                .ok_or_else(|| generic_error!("No default output declared.")),
            OutputName::Given(name) => self
                .targets
                .get_mut(name)
                .ok_or_else(|| generic_error!("Output '{}' does not exist.", name)),
        }
    }

    fn get_default_output(&self) -> Result<&DispatchTarget<T>, GenericError> {
        self.default
            .as_ref()
            .ok_or_else(|| generic_error!("No default output declared."))
    }

    fn get_named_output(&self, name: &str) -> Result<&DispatchTarget<T>, GenericError> {
        self.targets
            .get(name)
            .ok_or_else(|| generic_error!("No output named '{}' declared.", name))
    }

    /// Returns `true` if the default output is connected to downstream components.
    pub fn is_default_output_connected(&self) -> bool {
        self.default.as_ref().is_some_and(|target| !target.senders.is_empty())
    }

    /// Returns `true` if the named output is connected to downstream components.
    pub fn is_named_output_connected(&self, name: &str) -> bool {
        self.targets.get(name).is_some_and(|target| !target.senders.is_empty())
    }

    /// Returns the routing of the edge by which the named output sends to the lanes of a component, by key, by
    /// affinity, or by spreading, with the number of lanes that component has: what the route of every send on the
    /// output must fit ([`dispatch_named_to_lane`][Self::dispatch_named_to_lane]).
    ///
    /// Returns `None` if the output routes to no component's lanes, so that it ignores the route of every send: each of
    /// its edges leads to a component with one lane, or is fixed to one lane. Returns `None`, too, if the dispatcher
    /// has no output of that name.
    pub fn lane_edge(&self, output_name: &str) -> Option<(LaneEdge, NonZeroU16)> {
        let target = self.targets.get(output_name)?;
        let sender = &target.senders[target.routed?];
        // Attaching senders checks that a component has 1 to `u16::MAX` lanes.
        let lanes = u16::try_from(sender.lanes.len()).ok().and_then(NonZeroU16::new)?;
        Some((sender.edge, lanes))
    }

    /// Dispatches the given item to the default output.
    ///
    /// # Errors
    ///
    /// If the default output isn't set, or there is an error sending to the default output, an error is returned.
    pub async fn dispatch(&self, item: T) -> Result<(), GenericError> {
        self.dispatch_inner(None, item).await
    }

    /// Dispatches the given items to the given named output.
    ///
    /// # Errors
    ///
    /// If a output of the given name isn't set, or there is an error sending to the output, an error is returned.
    pub async fn dispatch_named<N>(&self, output_name: N, item: T) -> Result<(), GenericError>
    where
        N: AsRef<str>,
    {
        self.dispatch_inner(Some(output_name.as_ref()), item).await
    }

    async fn dispatch_inner(&self, output_name: Option<&str>, item: T) -> Result<(), GenericError> {
        let target = match output_name {
            None => self.get_default_output()?,
            Some(name) => self.get_named_output(name)?,
        };

        target.send(item, None).await?;

        Ok(())
    }

    /// Sends `item` on the named output, to the lane of the output's target that `route` names, as the edge's
    /// [routing][crate::topology::LaneEdge] asks.
    ///
    /// Every component the output feeds receives the item once, in one of its lanes. An edge fixed to a lane, and an
    /// edge into a component with one lane, ignore `route`. Of the one component whose lanes the output routes to, if
    /// any: a keyed edge sends to the lane `route` names ([`LaneRoute::Target`]); an affinity edge to the sender's lane
    /// ([`LaneRoute::From`]) modulo the component's lanes; a spread edge ([`LaneRoute::Spread`]) to the first lane with
    /// room, trying the lanes in rotation from the counter and waiting only when every lane is full, and it leaves the
    /// counter at the lane after the one it sent to. Each lane receives the sends of one sender in the order they were
    /// made. A spread counter that no component's lanes follow is left at 0.
    ///
    /// # Errors
    ///
    /// If an output of the given name isn't set, if `route` does not fit the routing of the output's edge into a
    /// component with lanes, if it names a lane that component does not have, or if there is an error sending to the
    /// output, an error is returned. Nothing is sent when the route is refused.
    pub async fn dispatch_named_to_lane(
        &self, output: &str, route: LaneRoute<'_>, item: T,
    ) -> Result<(), GenericError> {
        self.get_named_output(output)?.send(item, Some(route)).await
    }

    /// Sends `item` on the named output, to the lane `route` names, as
    /// [`dispatch_named_to_lane`][Self::dispatch_named_to_lane] does, if every lane it goes to has room now, and
    /// otherwise returns it, unsent.
    ///
    /// Returns `Ok(None)` once the item is sent, and `Ok(Some(item))`, sending it nowhere, when the send would wait:
    /// when the lane it goes to is full (for a spread route, when every lane is), or when the output is an admission
    /// point, whose stamp takes the point's lock. An output that feeds more than one component, such as one that also
    /// feeds a tap, always returns the item: the caller cannot see which component is full, and every later item goes
    /// to every component the output feeds.
    ///
    /// A caller that sends several items before awaiting the ones returned, so that one full lane does not hold up the
    /// others, keeps each lane's order only if, once it holds a returned item, it holds every later item for a lane the
    /// returned one could go to, until it sent the returned one: the lane a keyed or affinity route names, every lane
    /// for a spread route, and the lane of an edge fixed to one or of a component with one lane, which every item goes
    /// to.
    ///
    /// # Errors
    ///
    /// The errors of [`dispatch_named_to_lane`][Self::dispatch_named_to_lane].
    pub fn try_dispatch_named_to_lane(
        &self, output: &str, route: LaneRoute<'_>, item: T,
    ) -> Result<Option<T>, GenericError> {
        self.get_named_output(output)?.try_send(item, Some(route))
    }
}

impl Dispatcher<EventsBuffer> {
    /// Makes the given output an admission point.
    ///
    /// From then on, every buffer sent on the output, including the buffers of buffered dispatchers, is stamped by
    /// `point` before it is sent, and the point can send heartbeats through the output's senders: through every sender,
    /// or, if the point has [heartbeat recipients][AdmissionPoint::heartbeat_recipients], through the senders attached
    /// for those components with [`attach_component_sender_to_output`][Self::attach_component_sender_to_output] or
    /// [`attach_component_lanes_to_output`][Self::attach_component_lanes_to_output]. A component with lanes receives
    /// the point's heartbeats in the lane a fixed edge leads to, or in every lane of a routed edge. Outputs without an
    /// admission point send buffers unchanged.
    ///
    /// Call this after attaching every sender to the output: the point captures the output's senders, holding them
    /// weakly.
    ///
    /// # Errors
    ///
    /// If the output doesn't exist, if it already has an admission point, if `point` is already attached to an
    /// output, or if one of the point's heartbeat recipients has no sender attached to the output, an error is
    /// returned.
    pub fn set_admission_point(
        &mut self, output_name: &OutputName, point: Arc<AdmissionPoint>,
    ) -> Result<(), GenericError> {
        let target = self.get_output_mut(output_name)?;
        if target.admission.is_some() {
            return Err(generic_error!(
                "Output '{}' already has an admission point. Each output can have at most one.",
                output_name
            ));
        }

        let recipients = point.heartbeat_recipients();
        if let Some(missing) = recipients.iter().find(|recipient| {
            !target
                .senders
                .iter()
                .any(|sender| sender.component.as_ref() == Some(*recipient))
        }) {
            return Err(generic_error!(
                "Admission point {} sends heartbeats to '{}', but output '{}' has no sender to it. Attach a sender for \
                 every heartbeat recipient before setting the admission point.",
                point.id().get(),
                missing,
                output_name
            ));
        }
        let heartbeat_senders = target
            .senders
            .iter()
            .filter(|sender| {
                recipients.is_empty()
                    || sender
                        .component
                        .as_ref()
                        .is_some_and(|component| recipients.contains(component))
            })
            .flat_map(OutputSender::heartbeat_lanes)
            .map(mpsc::Sender::downgrade)
            .collect();

        let owner = point.attach_senders(heartbeat_senders)?;
        target.admission = Some(point);
        target._admission_owner = Some(owner);

        Ok(())
    }
}

impl<T> Dispatcher<T>
where
    T: DispatchBuffer,
{
    /// Creates a buffered dispatcher for the default output.
    ///
    /// This should generally be used if the items being dispatched aren't already collected in a container, or exposed
    /// via an iterable type. It allows for efficiently buffering items one-by-one before dispatching them to the
    /// underlying output.
    ///
    /// # Errors
    ///
    /// If the default output hasn't been configured, an error will be returned.
    pub fn buffered(&self) -> Result<BufferedDispatcher<'_, T>, GenericError> {
        self.get_default_output().map(BufferedDispatcher::new)
    }

    /// Creates a buffered dispatcher for the given named output.
    ///
    /// This should generally be used if the items being dispatched aren't already collected in a container, or exposed
    /// via an iterable type. It allows for efficiently buffering items one-by-one before dispatching them to the
    /// underlying output.
    ///
    /// # Errors
    ///
    /// If the given named output hasn't been configured, an error will be returned.
    pub fn buffered_named<N>(&self, output_name: N) -> Result<BufferedDispatcher<'_, T>, GenericError>
    where
        N: AsRef<str>,
    {
        self.get_named_output(output_name.as_ref()).map(BufferedDispatcher::new)
    }

    /// Dispatches a single item to the default output.
    ///
    /// # Errors
    ///
    /// If the default output isn't set, or there is an error sending to the default output, an error is returned.
    pub async fn dispatch_one(&self, item: T::Item) -> Result<(), GenericError> {
        self.dispatch_one_inner(None, item).await
    }

    /// Dispatches a single item to the given named output.
    ///
    /// # Errors
    ///
    /// If an output of the given name isn't set, or there is an error sending to the output, an error is returned.
    pub async fn dispatch_one_named<N>(&self, output_name: N, item: T::Item) -> Result<(), GenericError>
    where
        N: AsRef<str>,
    {
        self.dispatch_one_inner(Some(output_name.as_ref()), item).await
    }

    async fn dispatch_one_inner(&self, output_name: Option<&str>, item: T::Item) -> Result<(), GenericError> {
        let target = match output_name {
            None => self.get_default_output()?,
            Some(name) => self.get_named_output(name)?,
        };

        let mut buffer = T::default();
        if buffer.try_push(item).is_some() {
            return Err(generic_error!("Default-constructed buffer rejected a single item."));
        }
        target.send(buffer, None).await
    }
}

#[cfg(test)]
mod tests {
    // TODO: Tests asserting we emit metrics, and the right metrics.

    use std::{ops::Deref, time::Duration};

    use futures::FutureExt as _;
    use metrics::{Key, Label};
    use metrics_util::{
        debugging::{DebugValue, DebuggingRecorder, Snapshotter},
        CompositeKey, MetricKind,
    };
    use ordered_float::OrderedFloat;
    use proptest::prelude::*;

    use super::*;
    use crate::execution::{
        AdmissionMode, AdmissionPointConfig, AdmissionPointId, AdmissionSchedule, AdmissionTime, BatchHeader, EntryId,
        GenerationId, GenerationToken, HeartbeatOutcome,
    };
    use crate::topology::interconnect::Consumer;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct SingleEvent<T>(T);

    impl<T: Clone + Copy> Dispatchable for SingleEvent<T> {
        fn item_count(&self) -> usize {
            1
        }
    }

    impl<T: Clone + Copy> From<T> for SingleEvent<T> {
        fn from(value: T) -> Self {
            Self(value)
        }
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct FixedUsizeVec<const N: usize> {
        data: [usize; N],
        len: usize,
    }

    impl<const N: usize> Default for FixedUsizeVec<N> {
        fn default() -> Self {
            Self { data: [0; N], len: 0 }
        }
    }

    impl<const N: usize> Deref for FixedUsizeVec<N> {
        type Target = [usize];

        fn deref(&self) -> &Self::Target {
            &self.data
        }
    }

    impl<const N: usize> Dispatchable for FixedUsizeVec<N> {
        fn item_count(&self) -> usize {
            self.len
        }
    }

    impl<const N: usize> DispatchBuffer for FixedUsizeVec<N> {
        type Item = usize;

        fn len(&self) -> usize {
            self.len
        }

        fn is_full(&self) -> bool {
            self.len == N
        }

        fn try_push(&mut self, item: Self::Item) -> Option<Self::Item> {
            if self.is_full() {
                Some(item)
            } else {
                self.data[self.len] = item;
                self.len += 1;
                None
            }
        }
    }

    fn unbuffered_dispatcher<T: Dispatchable>() -> Dispatcher<T> {
        let component_context = ComponentContext::test_source("dispatcher_test");
        Dispatcher::new(component_context)
    }

    fn buffered_dispatcher<T: DispatchBuffer>() -> Dispatcher<T> {
        unbuffered_dispatcher()
    }

    /// One output-kind case: the output to operate on, plus the `output` metric label it maps to.
    ///
    /// Every dispatcher operation must behave identically for the default output and for a named output, so the tests
    /// below iterate over these two cases rather than duplicating a default-vs-named function pair for each scenario.
    struct OutputCase {
        output: OutputName,
        metric_label: &'static str,
    }

    fn output_cases() -> [OutputCase; 2] {
        [
            OutputCase {
                output: OutputName::Default,
                metric_label: "_default",
            },
            OutputCase {
                output: OutputName::Given("special".into()),
                metric_label: "special",
            },
        ]
    }

    /// Declares `output` on the dispatcher and attaches the given senders to it.
    fn add_output_with_senders<T: Dispatchable, const N: usize>(
        dispatcher: &mut Dispatcher<T>, output: &OutputName, senders: [mpsc::Sender<T>; N],
    ) {
        dispatcher
            .add_output(output.clone())
            .expect("output should not be declared yet");
        for sender in senders {
            dispatcher
                .attach_sender_to_output(output, sender)
                .expect("output should exist after being declared");
        }
    }

    /// Dispatches `item` to `output`, choosing the default or named entry point as appropriate.
    async fn dispatch_to<T: Dispatchable>(
        dispatcher: &Dispatcher<T>, output: &OutputName, item: T,
    ) -> Result<(), GenericError> {
        match output {
            OutputName::Default => dispatcher.dispatch(item).await,
            OutputName::Given(name) => dispatcher.dispatch_named(name.as_ref(), item).await,
        }
    }

    /// Creates a buffered dispatcher for `output`, choosing the default or named entry point as appropriate.
    fn buffered_for<'a, T: DispatchBuffer>(
        dispatcher: &'a Dispatcher<T>, output: &OutputName,
    ) -> Result<BufferedDispatcher<'a, T>, GenericError> {
        match output {
            OutputName::Default => dispatcher.buffered(),
            OutputName::Given(name) => dispatcher.buffered_named(name.as_ref()),
        }
    }

    /// Dispatches a single item to `output`, choosing the default or named entry point as appropriate.
    async fn dispatch_one_to<T: DispatchBuffer>(
        dispatcher: &Dispatcher<T>, output: &OutputName, item: T::Item,
    ) -> Result<(), GenericError> {
        match output {
            OutputName::Default => dispatcher.dispatch_one(item).await,
            OutputName::Given(name) => dispatcher.dispatch_one_named(name.as_ref(), item).await,
        }
    }

    /// Returns whether `output` is connected, choosing the default or named query as appropriate.
    fn is_output_connected<T: Dispatchable>(dispatcher: &Dispatcher<T>, output: &OutputName) -> bool {
        match output {
            OutputName::Default => dispatcher.is_default_output_connected(),
            OutputName::Given(name) => dispatcher.is_named_output_connected(name.as_ref()),
        }
    }

    fn get_dispatcher_metric_ckey(
        kind: MetricKind, name: &'static str, output_name: &'static str, tags: &[(&'static str, &'static str)],
    ) -> CompositeKey {
        let mut labels = vec![
            Label::from_static_parts("component_id", "dispatcher_test"),
            Label::from_static_parts("component_type", "source"),
            Label::from_static_parts("output", output_name),
        ];

        for tag in tags {
            labels.push(Label::from_static_parts(tag.0, tag.1));
        }

        let key = Key::from_parts(name, labels);
        CompositeKey::new(kind, key)
    }

    fn get_output_metrics(snapshotter: &Snapshotter, output_name: &'static str) -> (u64, u64, Vec<OrderedFloat<f64>>) {
        let events_sent_key = get_dispatcher_metric_ckey(
            MetricKind::Counter,
            DispatcherMetrics::events_sent_total_name(),
            output_name,
            &[],
        );
        let events_discarded_key = get_dispatcher_metric_ckey(
            MetricKind::Counter,
            DispatcherMetrics::events_discarded_total_name(),
            output_name,
            &[],
        );
        let send_latency_key = get_dispatcher_metric_ckey(
            MetricKind::Histogram,
            DispatcherMetrics::send_latency_seconds_name(),
            output_name,
            &[],
        );

        // TODO: This API for querying the metrics really sucks... and we need something better.
        let current_metrics = snapshotter.snapshot().into_hashmap();
        let (_, _, events_sent) = current_metrics
            .get(&events_sent_key)
            .expect("should have events sent metric");
        let (_, _, events_discarded) = current_metrics
            .get(&events_discarded_key)
            .expect("should have events discarded metric");
        let (_, _, send_latency) = current_metrics
            .get(&send_latency_key)
            .expect("should have send latency metric");

        let events_sent = match events_sent {
            DebugValue::Counter(value) => *value,
            _ => panic!("unexpected metric type for events sent"),
        };

        let events_discarded = match events_discarded {
            DebugValue::Counter(value) => *value,
            _ => panic!("unexpected metric type for events discarded"),
        };

        let send_latency = match send_latency {
            DebugValue::Histogram(value) => value.clone(),
            _ => panic!("unexpected metric type for send latency"),
        };

        (events_sent, events_discarded, send_latency)
    }

    #[tokio::test]
    async fn dispatch_delivers_item_to_each_output_kind() {
        // Dispatching a single item to a wired output delivers it unchanged, for both the default and a named output.
        for case in output_cases() {
            let mut dispatcher = unbuffered_dispatcher::<SingleEvent<usize>>();
            let (tx, mut rx) = mpsc::channel(1);
            add_output_with_senders(&mut dispatcher, &case.output, [tx]);

            let input_item = 42.into();
            dispatch_to(&dispatcher, &case.output, input_item).await.unwrap();

            let output_item = rx.try_recv().expect("input item should have been dispatched");
            assert_eq!(output_item, input_item);
        }
    }

    #[tokio::test]
    async fn dispatch_fans_out_to_all_senders_for_each_output_kind() {
        // With multiple senders attached to an output, a dispatched item is delivered (cloned) to every sender.
        for case in output_cases() {
            let mut dispatcher = unbuffered_dispatcher::<SingleEvent<usize>>();
            let (tx1, mut rx1) = mpsc::channel(1);
            let (tx2, mut rx2) = mpsc::channel(1);
            add_output_with_senders(&mut dispatcher, &case.output, [tx1, tx2]);

            let input_item = 42.into();
            dispatch_to(&dispatcher, &case.output, input_item).await.unwrap();

            assert_eq!(rx1.try_recv().expect("first sender should receive"), input_item);
            assert_eq!(rx2.try_recv().expect("second sender should receive"), input_item);
        }
    }

    #[tokio::test]
    async fn dispatch_to_unconfigured_output_errors() {
        // Dispatching to an output that was never declared fails with the documented error for that output kind.
        for case in output_cases() {
            let dispatcher = unbuffered_dispatcher::<SingleEvent<()>>();
            let err = dispatch_to(&dispatcher, &case.output, ().into())
                .await
                .expect_err("dispatch to an unconfigured output must fail");
            let msg = err.to_string();
            match &case.output {
                OutputName::Default => assert_eq!(msg, "No default output declared."),
                OutputName::Given(_) => assert!(msg.contains("No output named"), "got: {msg}"),
            }
        }
    }

    #[tokio::test]
    async fn buffered_dispatch_flushes_partial_buffer() {
        // A single buffered push, once flushed, arrives as a one-element buffer on the wired output.
        for case in output_cases() {
            let mut dispatcher = buffered_dispatcher::<FixedUsizeVec<4>>();
            let (tx, mut rx) = mpsc::channel(1);
            add_output_with_senders(&mut dispatcher, &case.output, [tx]);

            let input_item = 42;
            let mut buffered = buffered_for(&dispatcher, &case.output).unwrap();
            buffered.push(input_item).await.unwrap();
            let flushed_len = buffered.flush().await.unwrap();
            assert_eq!(flushed_len, 1);

            let output_item = rx.try_recv().expect("input item should have been dispatched");
            assert_eq!(output_item.len(), 1);
            assert_eq!(output_item[0], input_item);
        }
    }

    #[tokio::test]
    async fn buffered_dispatch_flushes_full_buffer_during_push() {
        // Pushing more items than a single buffer can hold flushes the full buffer mid-push, so the items arrive as
        // successive buffers (four, then the remaining two).
        for case in output_cases() {
            let mut dispatcher = buffered_dispatcher::<FixedUsizeVec<4>>();
            let (tx, mut rx) = mpsc::channel(2);
            add_output_with_senders(&mut dispatcher, &case.output, [tx]);

            let input_items: Vec<usize> = vec![1, 2, 3, 4, 5, 6];
            let mut buffered = buffered_for(&dispatcher, &case.output).unwrap();
            for item in &input_items {
                buffered.push(*item).await.unwrap();
            }
            let flushed_len = buffered.flush().await.unwrap();
            assert_eq!(flushed_len, input_items.len());

            let first = rx.try_recv().expect("first buffer should have been dispatched");
            assert_eq!(first.len(), 4);
            assert_eq!(first[0..4], input_items[0..4]);

            let second = rx.try_recv().expect("second buffer should have been dispatched");
            assert_eq!(second.len(), 2);
            assert_eq!(second[0..2], input_items[4..6]);
        }
    }

    #[tokio::test]
    async fn buffered_dispatch_fans_out_partial_buffer() {
        // A buffered push flushed to an output with multiple senders is delivered to every sender.
        for case in output_cases() {
            let mut dispatcher = buffered_dispatcher::<FixedUsizeVec<4>>();
            let (tx1, mut rx1) = mpsc::channel(1);
            let (tx2, mut rx2) = mpsc::channel(1);
            add_output_with_senders(&mut dispatcher, &case.output, [tx1, tx2]);

            let input_item = 42;
            let mut buffered = buffered_for(&dispatcher, &case.output).unwrap();
            buffered.push(input_item).await.unwrap();
            let flushed_len = buffered.flush().await.unwrap();
            assert_eq!(flushed_len, 1);

            let out1 = rx1.try_recv().expect("first sender should receive");
            assert_eq!(out1.len(), 1);
            assert_eq!(out1[0], input_item);

            let out2 = rx2.try_recv().expect("second sender should receive");
            assert_eq!(out2.len(), 1);
            assert_eq!(out2[0], input_item);
        }
    }

    #[tokio::test]
    async fn dispatch_to_output_without_senders_succeeds() {
        // Dispatching to a declared output that has no senders attached succeeds (the item is discarded, not an error).
        for case in output_cases() {
            let mut dispatcher = unbuffered_dispatcher::<SingleEvent<u32>>();
            dispatcher
                .add_output(case.output.clone())
                .expect("should be able to declare the output");

            dispatch_to(&dispatcher, &case.output, 42.into())
                .await
                .expect("dispatch to a senderless output should succeed");
        }
    }

    #[tokio::test]
    async fn disconnected_output_records_discarded_events() {
        // Dispatching to a declared-but-senderless output records the item count as discarded (and none as sent, with
        // no send-latency samples), for both the default and a named output.
        for case in output_cases() {
            let recorder = DebuggingRecorder::new();
            let snapshotter = recorder.snapshotter();
            let output = case.output.clone();
            let dispatcher = metrics::with_local_recorder(&recorder, || {
                let mut dispatcher = buffered_dispatcher::<FixedUsizeVec<4>>();
                dispatcher
                    .add_output(output)
                    .expect("should not fail to declare the output");
                dispatcher
            });

            // Dispatch an item with a count of 1 to the senderless output; it is discarded rather than sent.
            let mut single_item = FixedUsizeVec::<4>::default();
            assert_eq!(None, single_item.try_push(42));
            let single_item_count = single_item.item_count() as u64;
            dispatch_to(&dispatcher, &case.output, single_item)
                .await
                .expect("should not fail to dispatch");

            let (events_sent, events_discarded, send_latencies) = get_output_metrics(&snapshotter, case.metric_label);
            assert_eq!(events_sent, 0);
            assert_eq!(events_discarded, single_item_count);
            assert!(send_latencies.is_empty());

            // Dispatch a second item, this time with a count of 3.
            let mut multiple_items = FixedUsizeVec::<4>::default();
            assert_eq!(None, multiple_items.try_push(42));
            assert_eq!(None, multiple_items.try_push(12345));
            assert_eq!(None, multiple_items.try_push(1337));
            let multiple_items_count = multiple_items.item_count() as u64;
            dispatch_to(&dispatcher, &case.output, multiple_items)
                .await
                .expect("should not fail to dispatch");

            let (events_sent, events_discarded, send_latencies) = get_output_metrics(&snapshotter, case.metric_label);
            assert_eq!(events_sent, 0);
            assert_eq!(events_discarded, multiple_items_count);
            assert!(send_latencies.is_empty());
        }
    }

    #[tokio::test]
    async fn output_connected_reflects_sender_attachment() {
        // The connected-query reports false before an output exists, false once declared without senders, and true
        // only after a sender is attached -- for both the default and a named output.
        for case in output_cases() {
            let mut dispatcher = unbuffered_dispatcher::<SingleEvent<u32>>();

            assert!(
                !is_output_connected(&dispatcher, &case.output),
                "an undeclared output must report as not connected"
            );

            dispatcher
                .add_output(case.output.clone())
                .expect("should be able to declare the output");
            assert!(
                !is_output_connected(&dispatcher, &case.output),
                "a declared output with no senders must report as not connected"
            );

            let (tx, _rx) = mpsc::channel(1);
            dispatcher
                .attach_sender_to_output(&case.output, tx)
                .expect("should be able to attach a sender");
            assert!(
                is_output_connected(&dispatcher, &case.output),
                "an output with a sender attached must report as connected"
            );

            // A query for a different, undeclared named output is always false.
            assert!(
                !dispatcher.is_named_output_connected("nonexistent_output"),
                "an unknown named output must report as not connected"
            );
        }
    }

    #[tokio::test]
    async fn dispatch_one_wraps_item_in_single_element_buffer() {
        // `dispatch_one`/`dispatch_one_named` wrap a single item into a one-element buffer on the wired output.
        for case in output_cases() {
            let mut dispatcher = buffered_dispatcher::<FixedUsizeVec<4>>();
            let (tx, mut rx) = mpsc::channel(1);
            add_output_with_senders(&mut dispatcher, &case.output, [tx]);

            let input_item = 42;
            dispatch_one_to(&dispatcher, &case.output, input_item).await.unwrap();

            let output_item = rx.try_recv().expect("input item should have been dispatched");
            assert_eq!(output_item.len(), 1);
            assert_eq!(output_item[0], input_item);
        }
    }

    #[tokio::test]
    async fn dispatch_one_to_unconfigured_output_errors() {
        // `dispatch_one`/`dispatch_one_named` to an output that was never declared fails with the documented error for
        // that output kind.
        for case in output_cases() {
            let dispatcher = buffered_dispatcher::<FixedUsizeVec<4>>();
            let err = dispatch_one_to(&dispatcher, &case.output, 42)
                .await
                .expect_err("dispatch_one to an unconfigured output must fail");
            let msg = err.to_string();
            match &case.output {
                OutputName::Default => assert_eq!(msg, "No default output declared."),
                OutputName::Given(_) => assert!(msg.contains("No output named"), "got: {msg}"),
            }
        }
    }

    #[tokio::test]
    async fn add_output_rejects_duplicate() {
        // `add_output`'s documented error: declaring the same output twice is rejected with an "already exists" error,
        // for both the default and a named output.
        for case in output_cases() {
            let mut dispatcher = unbuffered_dispatcher::<SingleEvent<u32>>();
            dispatcher
                .add_output(case.output.clone())
                .expect("first declaration should succeed");

            let err = dispatcher
                .add_output(case.output.clone())
                .expect_err("declaring the same output twice must fail");
            assert!(err.to_string().contains("already exists"), "got: {err}");
        }
    }

    #[tokio::test]
    async fn attach_sender_to_undeclared_output_errors() {
        // `attach_sender_to_output`'s documented error: attaching a sender to an output that hasn't been declared fails
        // with the documented error for that output kind.
        for case in output_cases() {
            let mut dispatcher = unbuffered_dispatcher::<SingleEvent<u32>>();
            let (tx, _rx) = mpsc::channel(1);
            let err = dispatcher
                .attach_sender_to_output(&case.output, tx)
                .expect_err("attaching to an undeclared output must fail");
            let msg = err.to_string();
            match &case.output {
                OutputName::Default => assert_eq!(msg, "No default output declared."),
                OutputName::Given(_) => assert!(msg.contains("does not exist"), "got: {msg}"),
            }
        }
    }

    fn admission_point(id: u32) -> Arc<AdmissionPoint> {
        let generation = Arc::new(GenerationToken::new(GenerationId::new(1)));
        let schedule = AdmissionSchedule::Steady(generation).into_handle();
        Arc::new(AdmissionPoint::new(AdmissionPointConfig::new(
            AdmissionPointId::new(id),
            AdmissionMode::Admit,
            schedule,
            Duration::from_secs(600),
        )))
    }

    #[tokio::test]
    async fn a_send_to_a_lane_of_a_target_with_one_lane_goes_to_that_lane_whatever_its_route() {
        let mut dispatcher = unbuffered_dispatcher::<EventsBuffer>();
        let output = OutputName::Given("exit".into());
        let (tx, mut rx) = mpsc::channel(4);
        add_output_with_senders(&mut dispatcher, &output, [tx]);

        let mut counter = 0;
        for route in [
            LaneRoute::Target(0),
            LaneRoute::From(3),
            LaneRoute::Spread(&mut counter),
        ] {
            dispatcher
                .dispatch_named_to_lane("exit", route, EventsBuffer::with_capacity(0))
                .await
                .unwrap();
        }
        assert_eq!(counter, 0, "a spread over one lane stays at lane 0");
        for _ in 0..3 {
            rx.try_recv().expect("every send reaches the one lane");
        }
        assert!(dispatcher
            .dispatch_named_to_lane("missing", LaneRoute::Target(0), EventsBuffer::with_capacity(0))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn fan_out_copies_the_header_to_every_sender() {
        // Without an admission point, buffers keep whatever header they carry: every copy of a stamped buffer carries
        // the same stamp, and unstamped buffers stay unstamped.
        let mut dispatcher = unbuffered_dispatcher::<EventsBuffer>();
        let (tx1, mut rx1) = mpsc::channel(2);
        let (tx2, mut rx2) = mpsc::channel(2);
        let (tx3, mut rx3) = mpsc::channel(2);
        add_output_with_senders(&mut dispatcher, &OutputName::Default, [tx1, tx2, tx3]);

        let generation = Arc::new(GenerationToken::new(GenerationId::new(4)));
        let header = BatchHeader {
            admitted_at: Some(AdmissionTime::from_unix_secs(30)),
            admission_point: Some(AdmissionPointId::new(2)),
            epoch: 1,
            generation: Some(Arc::clone(&generation)),
            entry: Some(EntryId::new(6)),
        };
        dispatcher
            .dispatch(EventsBuffer::with_capacity(0).with_header(header))
            .await
            .unwrap();
        dispatcher.dispatch(EventsBuffer::with_capacity(0)).await.unwrap();

        let mut stamped = Vec::new();
        for rx in [&mut rx1, &mut rx2, &mut rx3] {
            let copy = rx.try_recv().expect("every sender should receive the stamped buffer");
            assert_eq!(copy.header().admitted_at, Some(AdmissionTime::from_unix_secs(30)));
            assert_eq!(copy.header().admission_point, Some(AdmissionPointId::new(2)));
            assert_eq!(copy.header().epoch, 1);
            assert_eq!(copy.header().generation_id(), Some(GenerationId::new(4)));
            assert_eq!(copy.header().entry, Some(EntryId::new(6)));
            stamped.push(copy);

            let unstamped = rx.try_recv().expect("every sender should receive the unstamped buffer");
            assert!(!unstamped.header().is_stamped());
            assert_eq!(unstamped.header().generation_id(), None);
        }
        assert_eq!(Arc::strong_count(&generation), 4);
    }

    #[test]
    fn admission_points_attach_only_to_declared_outputs_without_one() {
        let mut dispatcher = unbuffered_dispatcher::<EventsBuffer>();

        let error = dispatcher
            .set_admission_point(&OutputName::Default, admission_point(1))
            .unwrap_err();
        assert_eq!(error.to_string(), "No default output declared.");

        let error = dispatcher
            .set_admission_point(&OutputName::Given("metrics".into()), admission_point(1))
            .unwrap_err();
        assert_eq!(error.to_string(), "Output 'metrics' does not exist.");

        dispatcher.add_output(OutputName::Given("metrics".into())).unwrap();
        dispatcher
            .set_admission_point(&OutputName::Given("metrics".into()), admission_point(1))
            .unwrap();
        let error = dispatcher
            .set_admission_point(&OutputName::Given("metrics".into()), admission_point(2))
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Output 'metrics' already has an admission point. Each output can have at most one."
        );
    }

    #[test]
    fn senders_cannot_be_attached_after_the_admission_point() {
        // The admission point captures the output's senders for its heartbeats, so a later sender would miss them.
        let mut dispatcher = unbuffered_dispatcher::<EventsBuffer>();
        let (tx1, _rx1) = mpsc::channel(1);
        add_output_with_senders(&mut dispatcher, &OutputName::Default, [tx1]);
        dispatcher
            .set_admission_point(&OutputName::Default, admission_point(1))
            .unwrap();

        let (tx2, _rx2) = mpsc::channel(1);
        let error = dispatcher
            .attach_sender_to_output(&OutputName::Default, tx2)
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Cannot attach a sender to output '_default' after it became an admission point. Attach every sender \
             before setting the admission point."
        );
    }

    #[cfg(feature = "measurement")]
    #[test]
    fn probes_record_the_lock_the_fan_out_and_the_fill_of_an_admission_point_output() {
        use crate::data_model::event::{
            metric::{context::Context, Metric},
            Event,
        };

        // The probes' state is process-wide: hold the lock outside the runtime, so that no guard is held across an
        // await.
        let _probes = probes::tests::PROBES_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        runtime.block_on(async {
            let mut dispatcher =
                Dispatcher::<EventsBuffer>::new(ComponentContext::test_source("dispatcher_probe_test"));
            let (tx1, mut rx1) = mpsc::channel(2);
            let (tx2, mut rx2) = mpsc::channel(2);
            add_output_with_senders(&mut dispatcher, &OutputName::Default, [tx1, tx2]);
            dispatcher
                .set_admission_point(&OutputName::Default, admission_point(9))
                .unwrap();
            let probe = |name: &str| probes::snapshot()[&format!("{}/dispatcher_probe_test._default", name)].clone();

            // Item counts are recorded whether or not timing is on; durations only while it is.
            probes::set_timing(false);
            let mut buffer = EventsBuffer::with_capacity(4);
            let metric = Metric::counter(Context::from_static_name("probed"), 1.0);
            assert!(buffer.try_push(Event::Metric(metric)).is_none());
            dispatcher.dispatch(buffer.clone()).await.unwrap();
            assert_eq!((probe("dispatch_items").count, probe("dispatch_items").sum), (1, 1));
            assert_eq!(probe("admission_lock_wait").count, 0);

            probes::set_timing(true);
            dispatcher.dispatch(buffer).await.unwrap();
            probes::set_timing(false);
            assert_eq!(probe("dispatch_items").count, 2);
            assert_eq!(probe("admission_lock_wait").count, 1);
            assert_eq!(probe("admission_lock_hold").count, 1);
            // One clone, for the first of two senders; one send wait for each sender.
            assert_eq!(probe("dispatch_clone").count, 1);
            assert_eq!(probe("dispatch_send_wait").count, 2);
            for rx in [&mut rx1, &mut rx2] {
                assert!(rx.try_recv().unwrap().header().is_stamped());
                assert!(rx.try_recv().unwrap().header().is_stamped());
            }
        });
    }

    #[cfg(feature = "measurement")]
    #[test]
    fn probes_record_no_clone_and_no_send_for_a_sender_whose_gate_is_closed() {
        use crate::data_model::event::{
            metric::{context::Context, Metric},
            Event,
        };

        let _probes = probes::tests::PROBES_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        runtime.block_on(async {
            let mut dispatcher =
                Dispatcher::<EventsBuffer>::new(ComponentContext::test_source("dispatcher_gate_probe_test"));
            dispatcher.add_output(OutputName::Default).unwrap();
            let (tap_tx, tap_rx) = mpsc::channel(2);
            let (owner_tx, mut owner_rx) = mpsc::channel(2);
            let mut tap = Consumer::new(ComponentContext::test_destination("tap"), tap_rx);
            let gate = InputGate::closed();
            // The tap first: open, it takes a clone and the owner the buffer itself.
            dispatcher
                .attach_gated_lanes_to_output(
                    &OutputName::Default,
                    ComponentId::try_from("tap").unwrap(),
                    vec![tap_tx],
                    LaneEdge::Fixed(0),
                    gate.clone(),
                    tap.received_metrics(),
                )
                .unwrap();
            dispatcher
                .attach_sender_to_output(&OutputName::Default, owner_tx)
                .unwrap();
            let probe =
                |name: &str| probes::snapshot()[&format!("{}/dispatcher_gate_probe_test._default", name)].clone();

            let mut buffer = EventsBuffer::with_capacity(4);
            let metric = Metric::counter(Context::from_static_name("probed"), 1.0);
            assert!(buffer.try_push(Event::Metric(metric)).is_none());
            probes::set_timing(true);
            dispatcher.dispatch(buffer.clone()).await.unwrap();
            let closed = (probe("dispatch_clone").count, probe("dispatch_send_wait").count);
            gate.open();
            dispatcher.dispatch(buffer).await.unwrap();
            probes::set_timing(false);

            // Closed: the owner's send only. Open: the tap's clone, and both sends.
            assert_eq!(closed, (0, 1));
            assert_eq!(probe("dispatch_clone").count, 1);
            assert_eq!(probe("dispatch_send_wait").count, 3);
            assert_eq!(probe("dispatch_items").count, 2);
            assert!(owner_rx.try_recv().is_ok() && owner_rx.try_recv().is_ok());
            assert_eq!(tap.next().await.map(|buffer| buffer.len()), Some(1));
        });
    }

    #[tokio::test]
    async fn dispatch_one_errors_when_buffer_cannot_hold_a_single_item() {
        // Defensive guard in `dispatch_one_inner`: if the default-constructed buffer can't accept even one item, it
        // returns a documented error rather than silently dropping the item. This branch is only reachable via a
        // degenerate zero-capacity buffer type; every real buffer (capacity >= 1) accepts the first item.
        let mut dispatcher = buffered_dispatcher::<FixedUsizeVec<0>>();
        let (tx, _rx) = mpsc::channel(1);
        add_output_with_senders(&mut dispatcher, &OutputName::Default, [tx]);

        let err = dispatcher
            .dispatch_one(42)
            .await
            .expect_err("a zero-capacity buffer must be rejected, not silently drop the item");
        assert!(err.to_string().contains("rejected a single item"), "got: {err}");
    }

    /// A buffer of `len` items, and an observer of how many times buffers were cloned.
    #[derive(Debug)]
    struct CountedClones {
        len: usize,
        clones: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Clone for CountedClones {
        fn clone(&self) -> Self {
            self.clones.fetch_add(1, Ordering::Relaxed);
            Self {
                len: self.len,
                clones: Arc::clone(&self.clones),
            }
        }
    }

    impl Dispatchable for CountedClones {
        fn item_count(&self) -> usize {
            self.len
        }
    }

    /// Returns the value of the counter `name` of the destination `id` in `recorder`.
    fn received_by(recorder: &saluki_metrics::test::TestRecorder, id: &'static str) -> Option<u64> {
        recorder.counter((
            "component_events_received_total",
            &[("component_id", id), ("component_type", "destination")][..],
        ))
    }

    #[tokio::test]
    async fn a_closed_gate_skips_its_component_without_cloning_and_counts_it_as_received() {
        let recorder = saluki_metrics::test::TestRecorder::default();
        let _local = metrics::set_default_local_recorder(&recorder);
        let clones = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let buffer = |len| CountedClones {
            len,
            clones: Arc::clone(&clones),
        };

        // Each case puts the gated tap in another position of the fan-out: the last delivering sender takes the item
        // itself, so a closed tap in last place must not take it from the others.
        for tap_first in [true, false] {
            let mut dispatcher = unbuffered_dispatcher::<CountedClones>();
            dispatcher.add_output(OutputName::Default).unwrap();
            let (owner_tx, mut owner_rx) = mpsc::channel(4);
            let (tap_tx, tap_rx) = mpsc::channel(4);
            let tap = Consumer::new(ComponentContext::test_destination("tap"), tap_rx);
            let gate = InputGate::closed();
            let attach_tap = |dispatcher: &mut Dispatcher<CountedClones>, tap_tx| {
                dispatcher
                    .attach_gated_lanes_to_output(
                        &OutputName::Default,
                        ComponentId::try_from("tap").unwrap(),
                        vec![tap_tx],
                        LaneEdge::Fixed(0),
                        gate.clone(),
                        tap.received_metrics(),
                    )
                    .unwrap();
            };
            if tap_first {
                attach_tap(&mut dispatcher, tap_tx);
                dispatcher
                    .attach_sender_to_output(&OutputName::Default, owner_tx)
                    .unwrap();
            } else {
                dispatcher
                    .attach_sender_to_output(&OutputName::Default, owner_tx)
                    .unwrap();
                attach_tap(&mut dispatcher, tap_tx);
            }
            let mut tap = tap;
            let received_before = received_by(&recorder, "tap").unwrap_or(0);
            clones.store(0, Ordering::Relaxed);

            // Closed: the owner gets the buffer itself, the tap nothing, and no buffer is cloned; the tap's received
            // count still grows by the buffer's items.
            dispatcher.dispatch(buffer(3)).await.unwrap();
            assert_eq!(owner_rx.recv().await.unwrap().len, 3);
            assert!(tap.next().now_or_never().is_none(), "the closed tap receives nothing");
            assert_eq!(clones.load(Ordering::Relaxed), 0, "nothing is cloned for a closed tap");
            assert_eq!(received_by(&recorder, "tap"), Some(received_before + 3));

            // Open: both receive, one of them a clone, and the tap's consumer counts what it receives, as for any
            // component.
            gate.open();
            dispatcher.dispatch(buffer(5)).await.unwrap();
            assert_eq!(owner_rx.recv().await.unwrap().len, 5);
            assert_eq!(tap.next().await.unwrap().len, 5);
            assert_eq!(clones.load(Ordering::Relaxed), 1);
            assert_eq!(received_by(&recorder, "tap"), Some(received_before + 8));

            // Closed again: skipped again.
            gate.close();
            dispatcher.dispatch(buffer(2)).await.unwrap();
            assert_eq!(owner_rx.recv().await.unwrap().len, 2);
            assert!(tap.next().now_or_never().is_none());
            assert_eq!(received_by(&recorder, "tap"), Some(received_before + 10));
        }
    }

    #[tokio::test]
    async fn the_dispatcher_counts_a_closed_tap_as_sent_to() {
        // The source's own series do not change with the gate: the tap counts as sent to, open or closed.
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let (dispatcher, gate, mut owner_rx) = metrics::with_local_recorder(&recorder, || {
            let mut dispatcher = unbuffered_dispatcher::<SingleEvent<u32>>();
            dispatcher.add_output(OutputName::Default).unwrap();
            let (owner_tx, owner_rx) = mpsc::channel(4);
            let (tap_tx, tap_rx) = mpsc::channel(4);
            let tap = Consumer::new(ComponentContext::test_destination("tap"), tap_rx);
            let gate = InputGate::closed();
            dispatcher
                .attach_sender_to_output(&OutputName::Default, owner_tx)
                .unwrap();
            dispatcher
                .attach_gated_lanes_to_output(
                    &OutputName::Default,
                    ComponentId::try_from("tap").unwrap(),
                    vec![tap_tx],
                    LaneEdge::Fixed(0),
                    gate.clone(),
                    tap.received_metrics(),
                )
                .unwrap();
            (dispatcher, gate, owner_rx)
        });
        dispatcher.dispatch(SingleEvent(1)).await.unwrap();
        assert_eq!(owner_rx.recv().await, Some(SingleEvent(1)));
        let (sent, _, _) = get_output_metrics(&snapshotter, "_default");
        assert_eq!(sent, 2, "one event to each of the two senders");
        drop((dispatcher, gate));
    }

    /// Returns a dispatcher whose output `routed` feeds the `lanes` lanes of the component `lanes` by `edge`, each lane
    /// a channel of `capacity` items, and the receivers of the lanes, in lane order.
    fn laned_dispatcher(
        edge: LaneEdge, lanes: usize, capacity: usize,
    ) -> (Dispatcher<SingleEvent<u32>>, Vec<mpsc::Receiver<SingleEvent<u32>>>) {
        let mut dispatcher = unbuffered_dispatcher::<SingleEvent<u32>>();
        let output = OutputName::Given("routed".into());
        dispatcher.add_output(output.clone()).unwrap();
        let (senders, receivers) = (0..lanes).map(|_| mpsc::channel(capacity)).unzip();
        dispatcher
            .attach_component_lanes_to_output(&output, ComponentId::try_from("lanes").unwrap(), senders, edge)
            .unwrap();
        (dispatcher, receivers)
    }

    /// Takes every item each lane holds now, in lane order.
    fn drain_lanes<T: Copy>(receivers: &mut [mpsc::Receiver<SingleEvent<T>>]) -> Vec<Vec<T>> {
        receivers
            .iter_mut()
            .map(|receiver| {
                let mut items = Vec::new();
                while let Ok(SingleEvent(item)) = receiver.try_recv() {
                    items.push(item);
                }
                items
            })
            .collect()
    }

    #[tokio::test]
    async fn a_fixed_edge_sends_every_item_to_its_lane_whatever_the_route() {
        let (dispatcher, mut lanes) = laned_dispatcher(LaneEdge::Fixed(2), 3, 4);
        let mut counter = 5;
        dispatcher.dispatch_named("routed", SingleEvent(1)).await.unwrap();
        for (route, value) in [
            (LaneRoute::Target(0), 2),
            (LaneRoute::From(1), 3),
            (LaneRoute::Spread(&mut counter), 4),
        ] {
            dispatcher
                .dispatch_named_to_lane("routed", route, SingleEvent(value))
                .await
                .unwrap();
        }
        assert_eq!(counter, 0, "no component's lanes follow the counter");
        assert_eq!(drain_lanes(&mut lanes), vec![vec![], vec![], vec![1, 2, 3, 4]]);
    }

    #[tokio::test]
    async fn a_keyed_edge_sends_to_the_lane_the_route_names_and_refuses_any_other_route() {
        let (dispatcher, mut lanes) = laned_dispatcher(LaneEdge::Keyed, 3, 4);
        for (lane, value) in [(2, 1), (0, 2), (2, 3)] {
            dispatcher
                .dispatch_named_to_lane("routed", LaneRoute::Target(lane), SingleEvent(value))
                .await
                .unwrap();
        }
        assert_eq!(drain_lanes(&mut lanes), vec![vec![2], vec![], vec![1, 3]]);

        let error = dispatcher
            .dispatch_named_to_lane("routed", LaneRoute::Target(3), SingleEvent(4))
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Cannot send on output 'routed' to lane 3 of 'lanes', which has 3 input lanes. Route each send to a lane \
             the component has."
        );
        // A dispatch that names no lane cannot follow keyed routing.
        let error = dispatcher.dispatch_named("routed", SingleEvent(5)).await.unwrap_err();
        assert_eq!(
            error.to_string(),
            "Cannot send on output 'routed' with no route: the output routes to the lanes of 'lanes' by keyed routing, \
             so each send names the lane the sender chose for it (`LaneRoute::Target`)."
        );
        let mut counter = 0;
        assert!(dispatcher
            .dispatch_named_to_lane("routed", LaneRoute::Spread(&mut counter), SingleEvent(6))
            .await
            .is_err());
        assert!(dispatcher
            .dispatch_named_to_lane("routed", LaneRoute::From(0), SingleEvent(7))
            .await
            .is_err());
        assert!(dispatcher
            .try_dispatch_named_to_lane("routed", LaneRoute::From(0), SingleEvent(8))
            .is_err());
        assert_eq!(
            drain_lanes(&mut lanes),
            vec![Vec::<u32>::new(); 3],
            "a refused send sends nothing"
        );
    }

    #[tokio::test]
    async fn an_affinity_edge_sends_to_the_senders_lane_modulo_the_lanes() {
        let (dispatcher, mut lanes) = laned_dispatcher(LaneEdge::Affinity, 3, 4);
        for (lane, value) in [(0, 1), (4, 2), (2, 3)] {
            dispatcher
                .dispatch_named_to_lane("routed", LaneRoute::From(lane), SingleEvent(value))
                .await
                .unwrap();
        }
        // A sender without lanes is lane 0.
        dispatcher.dispatch_named("routed", SingleEvent(4)).await.unwrap();
        assert_eq!(drain_lanes(&mut lanes), vec![vec![1, 4], vec![2], vec![3]]);

        let error = dispatcher
            .dispatch_named_to_lane("routed", LaneRoute::Target(1), SingleEvent(5))
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Cannot send on output 'routed' with a route to lane 1: the output routes to the lanes of 'lanes' by \
             affinity routing, so each send names the sender's own lane (`LaneRoute::From`), or no route."
        );
    }

    #[tokio::test]
    async fn a_spread_edge_sends_to_the_first_lane_with_room_and_waits_only_when_every_lane_is_full() {
        let (dispatcher, mut lanes) = laned_dispatcher(LaneEdge::Spread, 3, 1);
        let spread = |counter: &mut u16, value| {
            dispatcher
                .dispatch_named_to_lane("routed", LaneRoute::Spread(counter), SingleEvent(value))
                .now_or_never()
                .expect("a lane has room, so the send does not wait")
                .unwrap()
        };

        // From the counter, in rotation, and the counter is left at the lane after the one sent to.
        let mut counter = 1;
        spread(&mut counter, 1);
        assert_eq!(counter, 2);
        // A full lane is passed over for the next one with room.
        counter = 1;
        spread(&mut counter, 2);
        assert_eq!(counter, 0);
        spread(&mut counter, 3);
        assert_eq!(counter, 1);

        // Every lane is full: the send waits, and goes to the first lane that has room.
        let mut send = tokio_test::task::spawn(dispatcher.dispatch_named_to_lane(
            "routed",
            LaneRoute::Spread(&mut counter),
            SingleEvent(4),
        ));
        assert!(send.poll().is_pending(), "every lane is full");
        assert_eq!(lanes[2].recv().await, Some(SingleEvent(2)));
        assert!(send.is_woken());
        assert!(matches!(send.poll(), std::task::Poll::Ready(Ok(()))));
        drop(send);
        assert_eq!(counter, 0);
        assert_eq!(drain_lanes(&mut lanes), vec![vec![3], vec![1], vec![4]]);

        // A route of another kind does not fit.
        assert!(dispatcher
            .dispatch_named_to_lane("routed", LaneRoute::From(0), SingleEvent(5))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn a_dispatch_without_a_route_spreads_from_the_outputs_own_rotation() {
        let (dispatcher, mut lanes) = laned_dispatcher(LaneEdge::Spread, 2, 4);
        for value in 0..5 {
            dispatcher.dispatch_named("routed", SingleEvent(value)).await.unwrap();
        }
        assert_eq!(drain_lanes(&mut lanes), vec![vec![0, 2, 4], vec![1, 3]]);
    }

    #[tokio::test]
    async fn a_try_dispatch_sends_only_when_its_lane_has_room_and_only_on_an_output_that_feeds_one_component() {
        // `routed` feeds two keyed lanes, each a channel of one item.
        let (dispatcher, mut lanes) = laned_dispatcher(LaneEdge::Keyed, 2, 1);
        let try_send =
            |lane, value| dispatcher.try_dispatch_named_to_lane("routed", LaneRoute::Target(lane), SingleEvent(value));
        assert_eq!(try_send(0, 1).unwrap(), None);
        // The lane is full: the item is returned, and no lane receives it.
        assert_eq!(try_send(0, 2).unwrap(), Some(SingleEvent(2)));
        assert_eq!(try_send(1, 3).unwrap(), None);
        assert_eq!(drain_lanes(&mut lanes), vec![vec![1], vec![3]]);

        // Once `routed` also feeds a tap, every item is returned, even with room in every channel.
        let mut dispatcher = unbuffered_dispatcher::<SingleEvent<u32>>();
        let output = OutputName::Given("routed".into());
        dispatcher.add_output(output.clone()).unwrap();
        let (senders, receivers): (Vec<_>, Vec<_>) = (0..2).map(|_| mpsc::channel(4)).unzip();
        let mut lanes = receivers;
        dispatcher
            .attach_component_lanes_to_output(
                &output,
                ComponentId::try_from("lanes").unwrap(),
                senders,
                LaneEdge::Keyed,
            )
            .unwrap();
        let (tap_tx, tap_rx) = mpsc::channel(4);
        let mut tap = [tap_rx];
        dispatcher
            .attach_component_sender_to_output(&output, ComponentId::try_from("tap").unwrap(), tap_tx)
            .unwrap();
        let returned = dispatcher
            .try_dispatch_named_to_lane("routed", LaneRoute::Target(0), SingleEvent(4))
            .unwrap();
        assert_eq!(returned, Some(SingleEvent(4)));
        assert_eq!(drain_lanes(&mut lanes), vec![Vec::<u32>::new(); 2]);
        assert_eq!(drain_lanes(&mut tap), vec![Vec::<u32>::new()]);

        // A spread try leaves its counter where it was when every lane is full.
        let (dispatcher, mut lanes) = laned_dispatcher(LaneEdge::Spread, 2, 1);
        let mut counter = 0;
        for (value, sent, after) in [(1, true, 1), (2, true, 0), (3, false, 0)] {
            let returned = dispatcher
                .try_dispatch_named_to_lane("routed", LaneRoute::Spread(&mut counter), SingleEvent(value))
                .unwrap();
            assert_eq!(returned.is_none(), sent, "{}", value);
            assert_eq!(counter, after, "{}", value);
        }
        assert_eq!(drain_lanes(&mut lanes), vec![vec![1], vec![2]]);
    }

    #[tokio::test]
    async fn a_caller_holding_returned_items_by_lane_keeps_the_order_of_every_component_the_output_feeds() {
        // `routed` feeds two keyed lanes, each a channel of four items, and a tap with one lane, a channel of one item.
        // The caller keeps the rule of `try_dispatch_named_to_lane`: once it holds a returned item, it holds every
        // later item for a lane the returned one could go to, and once its tries are done it sends what it holds, in
        // order.
        let mut dispatcher = unbuffered_dispatcher::<SingleEvent<u32>>();
        let output = OutputName::Given("routed".into());
        dispatcher.add_output(output.clone()).unwrap();
        let (senders, receivers): (Vec<_>, Vec<_>) = (0..2).map(|_| mpsc::channel(4)).unzip();
        let mut lanes = receivers;
        dispatcher
            .attach_component_lanes_to_output(
                &output,
                ComponentId::try_from("lanes").unwrap(),
                senders,
                LaneEdge::Keyed,
            )
            .unwrap();
        let (tap_tx, tap_rx) = mpsc::channel(1);
        let mut tap = [tap_rx];
        dispatcher
            .attach_component_sender_to_output(&output, ComponentId::try_from("tap").unwrap(), tap_tx)
            .unwrap();

        let mut tapped = Vec::new();
        let mut held = Vec::new();
        for (lane, value) in [(0, 1), (0, 2), (1, 3)] {
            // The tap takes what it holds before the last item, so it has room for that one.
            if value == 3 {
                tapped.extend(drain_lanes(&mut tap).remove(0));
            }
            if held.iter().any(|(held_lane, _)| *held_lane == lane) {
                held.push((lane, SingleEvent(value)));
                continue;
            }
            let returned = dispatcher
                .try_dispatch_named_to_lane("routed", LaneRoute::Target(lane), SingleEvent(value))
                .unwrap();
            if let Some(item) = returned {
                held.push((lane, item));
            }
        }
        for (lane, item) in held {
            tapped.extend(drain_lanes(&mut tap).remove(0));
            dispatcher
                .dispatch_named_to_lane("routed", LaneRoute::Target(lane), item)
                .await
                .unwrap();
        }
        tapped.extend(drain_lanes(&mut tap).remove(0));

        assert_eq!(
            tapped,
            vec![1, 2, 3],
            "the tap receives the items in the order they were sent"
        );
        assert_eq!(drain_lanes(&mut lanes), vec![vec![1, 2], vec![3]]);
    }

    #[tokio::test]
    async fn a_try_dispatch_on_an_admission_point_returns_the_buffer_unstamped() {
        let mut dispatcher = unbuffered_dispatcher::<EventsBuffer>();
        let output = OutputName::Given("exit".into());
        let (tx, mut rx) = mpsc::channel(4);
        add_output_with_senders(&mut dispatcher, &output, [tx]);
        dispatcher.set_admission_point(&output, admission_point(1)).unwrap();

        let returned = dispatcher
            .try_dispatch_named_to_lane("exit", LaneRoute::Target(0), EventsBuffer::with_capacity(0))
            .unwrap()
            .expect("an admission point's stamp may wait for its lock");
        assert!(!returned.header().is_stamped());
        assert!(rx.try_recv().is_err());
        dispatcher
            .dispatch_named_to_lane("exit", LaneRoute::Target(0), returned)
            .await
            .unwrap();
        assert!(rx.try_recv().unwrap().header().is_stamped());
    }

    #[test]
    fn an_output_routes_to_the_lanes_of_one_component_only() {
        let mut dispatcher = unbuffered_dispatcher::<SingleEvent<u32>>();
        let output = OutputName::Given("routed".into());
        dispatcher.add_output(output.clone()).unwrap();
        let lanes = |count| (0..count).map(|_| mpsc::channel(1).0).collect::<Vec<_>>();
        let mut attach = |component: &str, senders, edge| {
            dispatcher
                .attach_component_lanes_to_output(&output, ComponentId::try_from(component).unwrap(), senders, edge)
                .map_err(|error| error.to_string())
        };

        attach("first", lanes(2), LaneEdge::Spread).unwrap();
        // Lanes the output is fixed to, and components with one lane, ignore the route.
        attach("fixed", lanes(2), LaneEdge::Fixed(1)).unwrap();
        attach("tap", lanes(1), LaneEdge::Keyed).unwrap();
        assert_eq!(
            attach("second", lanes(3), LaneEdge::Keyed).unwrap_err(),
            "Cannot route output 'routed' to the lanes of 'second': it already routes to the lanes of 'first', and \
             each send names one route. Give each component with lanes an output of its own."
        );
        assert_eq!(
            attach("short", lanes(2), LaneEdge::Fixed(2)).unwrap_err(),
            "Cannot attach output 'routed' to lane 2 of 'short', which has 2 input lane(s). Attach it to a lane the \
             component has."
        );
        assert_eq!(
            attach("none", lanes(0), LaneEdge::Spread).unwrap_err(),
            "Cannot attach 0 senders to output 'routed' for 'none': a component has 1 to 65535 input lanes, and each \
             lane one sender."
        );
    }

    #[test]
    fn an_output_reports_the_edge_it_routes_to_lanes_by_and_none_when_it_routes_to_no_lanes() {
        let mut dispatcher = unbuffered_dispatcher::<SingleEvent<u32>>();
        let lanes = |count| (0..count).map(|_| mpsc::channel(1).0).collect::<Vec<_>>();
        for name in ["keyed", "fixed", "unconnected"] {
            dispatcher.add_output(OutputName::Given(name.into())).unwrap();
        }
        let mut attach = |output: &'static str, component: &str, senders, edge| {
            dispatcher
                .attach_component_lanes_to_output(
                    &OutputName::Given(output.into()),
                    ComponentId::try_from(component).unwrap(),
                    senders,
                    edge,
                )
                .unwrap()
        };
        attach("keyed", "tap", lanes(1), LaneEdge::Spread);
        attach("keyed", "state", lanes(3), LaneEdge::Keyed);
        attach("fixed", "lanes", lanes(2), LaneEdge::Fixed(1));
        attach("fixed", "one", lanes(1), LaneEdge::Affinity);

        assert_eq!(
            dispatcher.lane_edge("keyed"),
            Some((LaneEdge::Keyed, NonZeroU16::new(3).unwrap()))
        );
        // Edges fixed to a lane, and edges into a component with one lane, route nothing.
        assert_eq!(dispatcher.lane_edge("fixed"), None);
        assert_eq!(dispatcher.lane_edge("unconnected"), None);
        assert_eq!(dispatcher.lane_edge("missing"), None);
    }

    #[tokio::test]
    async fn heartbeats_reach_the_lane_a_fixed_edge_leads_to_and_every_lane_of_a_routed_edge() {
        let mut dispatcher = unbuffered_dispatcher::<EventsBuffer>();
        dispatcher.add_output(OutputName::Default).unwrap();
        let mut attach = |component: &str, lanes: usize, edge| {
            let (senders, receivers): (Vec<_>, Vec<_>) = (0..lanes).map(|_| mpsc::channel(4)).unzip();
            dispatcher
                .attach_component_lanes_to_output(
                    &OutputName::Default,
                    ComponentId::try_from(component).unwrap(),
                    senders,
                    edge,
                )
                .unwrap();
            receivers
        };
        let mut fixed = attach("fixed", 2, LaneEdge::Fixed(1));
        let mut spread = attach("spread", 3, LaneEdge::Spread);
        let mut tap = attach("tap", 1, LaneEdge::Fixed(0));
        let point = admission_point(1);
        dispatcher
            .set_admission_point(&OutputName::Default, Arc::clone(&point))
            .unwrap();

        let received = |lanes: &mut [mpsc::Receiver<EventsBuffer>]| {
            lanes
                .iter_mut()
                .map(|lane| {
                    let mut stamped = 0;
                    while let Ok(buffer) = lane.try_recv() {
                        assert!(buffer.header().is_stamped());
                        stamped += 1;
                    }
                    stamped
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(point.heartbeat().await, HeartbeatOutcome::Sent);
        assert_eq!(received(&mut fixed), vec![0, 1]);
        assert_eq!(received(&mut spread), vec![1, 1, 1]);
        assert_eq!(received(&mut tap), vec![1]);

        // Data goes to one lane of each component, stamped.
        dispatcher.dispatch(EventsBuffer::with_capacity(0)).await.unwrap();
        assert_eq!(received(&mut fixed), vec![0, 1]);
        assert_eq!(received(&mut spread), vec![1, 0, 0]);
        assert_eq!(received(&mut tap), vec![1]);
    }

    /// One step of the spread property: a send through `try_dispatch_named_to_lane` or, when `wait` is set and a lane
    /// has room, through `dispatch_named_to_lane`; or a receive from a lane.
    #[derive(Clone, Debug)]
    enum SpreadStep {
        Send { wait: bool },
        Receive { lane: usize },
    }

    fn spread_step() -> impl Strategy<Value = SpreadStep> {
        prop_oneof![
            3 => any::<bool>().prop_map(|wait| SpreadStep::Send { wait }),
            2 => (0usize..8).prop_map(|lane| SpreadStep::Receive { lane }),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        #[test]
        fn property_test_spread_sends_go_to_the_first_lane_with_room_and_keep_each_lanes_order(
            lane_count in 2usize..6,
            capacity in 1usize..4,
            start in 0u16..8,
            steps in prop::collection::vec(spread_step(), 1..60),
        ) {
            let (dispatcher, mut lanes) = laned_dispatcher(LaneEdge::Spread, lane_count, capacity);
            // The lanes as they should be: what each holds, in order.
            let mut expected: Vec<std::collections::VecDeque<u32>> = vec![Default::default(); lane_count];
            let mut counter = start;
            let mut model_counter = usize::from(start) % lane_count;
            for (value, step) in (0u32..).zip(steps) {
                match step {
                    SpreadStep::Send { wait } => {
                        let with_room = (0..lane_count)
                            .map(|offset| (model_counter + offset) % lane_count)
                            .find(|lane| expected[*lane].len() < capacity);
                        let returned = if wait && with_room.is_some() {
                            dispatcher
                                .dispatch_named_to_lane("routed", LaneRoute::Spread(&mut counter), SingleEvent(value))
                                .now_or_never()
                                .expect("a send with a lane that has room does not wait")
                                .unwrap();
                            None
                        } else {
                            let route = LaneRoute::Spread(&mut counter);
                            dispatcher
                                .try_dispatch_named_to_lane("routed", route, SingleEvent(value))
                                .unwrap()
                        };
                        match with_room {
                            Some(lane) => {
                                prop_assert_eq!(returned, None);
                                expected[lane].push_back(value);
                                model_counter = (lane + 1) % lane_count;
                            }
                            None => prop_assert_eq!(returned, Some(SingleEvent(value))),
                        }
                        prop_assert_eq!(usize::from(counter), model_counter);
                    }
                    SpreadStep::Receive { lane } => {
                        let lane = lane % lane_count;
                        let received = lanes[lane].try_recv().ok().map(|SingleEvent(value)| value);
                        prop_assert_eq!(received, expected[lane].pop_front());
                    }
                }
            }
            let remaining = expected.into_iter().map(Vec::from).collect::<Vec<_>>();
            prop_assert_eq!(drain_lanes(&mut lanes), remaining);
        }
    }
}
