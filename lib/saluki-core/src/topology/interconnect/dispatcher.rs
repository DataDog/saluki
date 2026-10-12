use std::{
    borrow::Cow,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Instant,
};

use saluki_common::collections::FastHashMap;
use saluki_error::{generic_error, GenericError};
use saluki_metrics::{static_metrics, Counter, Histogram};
use tokio::sync::mpsc;

use super::{consumer::ReceivedMetrics, Dispatchable};
#[cfg(feature = "measurement")]
use crate::probes;
use crate::{
    components::ComponentContext,
    execution::{AdmissionPoint, OwnerToken},
    topology::{ComponentId, EventsBuffer, OutputName},
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

struct DispatchTarget<T> {
    metrics: DispatcherMetrics,
    #[cfg(feature = "measurement")]
    probes: DispatchProbes,
    senders: Vec<mpsc::Sender<T>>,

    // The component each sender feeds, where it is known, so that heartbeats can go to some of them only.
    sender_components: Vec<Option<ComponentId>>,

    // The gate of each sender's component, for the components that have one.
    sender_gates: Vec<Option<GatedSender>>,
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
            #[cfg(feature = "measurement")]
            probes: DispatchProbes::new(&context, "_default"),
            metrics: DispatcherMetrics::default_output(context),
            senders: Vec::new(),
            sender_components: Vec::new(),
            sender_gates: Vec::new(),
            admission: None,
            _admission_owner: None,
        }
    }

    fn named_output(context: ComponentContext, output_name: &str) -> Self {
        Self {
            #[cfg(feature = "measurement")]
            probes: DispatchProbes::new(&context, output_name),
            metrics: DispatcherMetrics::named_output(context, output_name),
            senders: Vec::new(),
            sender_components: Vec::new(),
            sender_gates: Vec::new(),
            admission: None,
            _admission_owner: None,
        }
    }

    fn add_sender(&mut self, sender: mpsc::Sender<T>, component: Option<ComponentId>, gate: Option<GatedSender>) {
        self.senders.push(sender);
        self.sender_components.push(component);
        self.sender_gates.push(gate);
    }

    /// Returns `true` if the sender at `index` delivers: it has no gate, or its gate is open.
    fn delivers(&self, index: usize) -> bool {
        self.sender_gates[index]
            .as_ref()
            .is_none_or(|gated| gated.gate.is_open())
    }

    /// Counts an item the sender at `index` skipped as received by its component, which must have a gate.
    fn count_skipped(&self, index: usize, item_count: usize) {
        if let Some(gated) = &self.sender_gates[index] {
            gated.received.record(item_count);
        }
    }

    async fn send(&self, mut item: T) -> Result<(), GenericError> {
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
            return Ok(());
        }

        let start = Instant::now();
        let item_count = item.item_count();
        #[cfg(feature = "measurement")]
        self.probes.items.record(item_count as u64);

        // The last sender that delivers takes the item itself, and every other one that delivers a clone. A sender
        // whose component's gate is closed is skipped, and what it would have sent counted as received there. Each
        // gate is decided once: a sender after the last one found delivering is skipped even if its gate opens in the
        // meantime, and the last one takes the item even if its gate closes.
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
                    sender
                        .send(to_send)
                        .await
                        .map_err(|_| generic_error!("Failed to send to output."))?
                }
                None => self.count_skipped(index, item_count),
            }
        }

        let elapsed = start.elapsed();

        // TODO: We should consider splitting this out per-sender somehow. We would need to carry around the
        // destination component's ID, though, to properly associate it.
        self.metrics.send_latency_seconds().record(elapsed);

        let total_events_sent = (self.senders.len() * item_count) as u64;
        self.metrics.events_sent_total().increment(total_events_sent);

        Ok(())
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
            self.target.send(buffer).await
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
                let target = DispatchTarget::named_output(self.context.clone(), &name);
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
        self.attach_sender(output_name, sender, None, None)
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
        self.attach_sender(output_name, sender, Some(component_id), None)
    }

    /// Attaches a sender that feeds the component `component_id`, whose input has the gate `gate`, to the given
    /// output: while the gate is closed, the output skips the sender, and records what it skipped in `received`, the
    /// counters of what the component received. See [`InputGate`].
    ///
    /// # Errors
    ///
    /// If the output doesn't exist, or already has an admission point, an error is returned.
    pub(crate) fn attach_gated_sender_to_output(
        &mut self, output_name: &OutputName, component_id: ComponentId, sender: mpsc::Sender<T>, gate: InputGate,
        received: ReceivedMetrics,
    ) -> Result<(), GenericError> {
        let gated = GatedSender { gate, received };
        self.attach_sender(output_name, sender, Some(component_id), Some(gated))
    }

    fn attach_sender(
        &mut self, output_name: &OutputName, sender: mpsc::Sender<T>, component_id: Option<ComponentId>,
        gate: Option<GatedSender>,
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
        target.add_sender(sender, component_id, gate);

        Ok(())
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

        target.send(item).await?;

        Ok(())
    }
}

impl Dispatcher<EventsBuffer> {
    /// Makes the given output an admission point.
    ///
    /// From then on, every buffer sent on the output, including the buffers of buffered dispatchers, is stamped by
    /// `point` before it is sent, and the point can send heartbeats through the output's senders: through every
    /// sender, or, if the point has [heartbeat recipients][AdmissionPoint::heartbeat_recipients], through the senders
    /// attached for those components with
    /// [`attach_component_sender_to_output`][Self::attach_component_sender_to_output]. Outputs without an admission
    /// point send buffers unchanged.
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
        if let Some(missing) = recipients
            .iter()
            .find(|recipient| !target.sender_components.iter().flatten().any(|fed| fed == *recipient))
        {
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
            .zip(&target.sender_components)
            .filter(|(_, component)| {
                recipients.is_empty()
                    || component
                        .as_ref()
                        .is_some_and(|component| recipients.contains(component))
            })
            .map(|(sender, _)| sender.downgrade())
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
        target.send(buffer).await
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

    use super::*;
    use crate::execution::{
        AdmissionMode, AdmissionPointConfig, AdmissionPointId, AdmissionSchedule, AdmissionTime, BatchHeader, EntryId,
        GenerationId, GenerationToken,
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
                .attach_gated_sender_to_output(
                    &OutputName::Default,
                    ComponentId::try_from("tap").unwrap(),
                    tap_tx,
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
                    .attach_gated_sender_to_output(
                        &OutputName::Default,
                        ComponentId::try_from("tap").unwrap(),
                        tap_tx,
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
                .attach_gated_sender_to_output(
                    &OutputName::Default,
                    ComponentId::try_from("tap").unwrap(),
                    tap_tx,
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
}
