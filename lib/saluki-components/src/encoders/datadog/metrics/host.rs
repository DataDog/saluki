//! The multiplexed metrics encode host: one encoder component per delivery host, running one lease per output.
//!
//! Each output bound to a delivery host runs as a **lease** of that host's encode host: one encoder stream, with its
//! own builder task on the global pool, so compression stays as parallel as one native encoder per output made it.
//! Region exits address a lease by their buffers' header entry ([`EntryId`]), and every payload a lease builds carries
//! the recipients of its protocol variant ([`PayloadRecipients`][crate::forwarders::delivery::PayloadRecipients]).
//!
//! The host runs two kinds of task:
//!
//! - its **front task**, the component's own, reads the host's input, its control mailbox, and the payloads every lease
//!   builds. It looks up each buffer's lease by the buffer's header entry and moves the whole buffer into one of the
//!   lease's builder shards, doing no work per event; and it dispatches each payload to the delivery host, in the order
//!   the builders sent them;
//! - each **builder shard** of a lease is a request builder task on the global thread pool, the native encoder's, run
//!   with the lease's settings. It stamps every payload with the recipients the lease resolved for the payload's
//!   protocol variant, and, after its last payload, sends a [`HostPayload::LeaseClosed`] marker on the channel the
//!   payloads take.
//!
//! While the front task waits for room in a shard, or for the delivery host to take a payload, it keeps answering its
//! control mailbox and liveness probes, and while it waits for a shard, it keeps dispatching payloads: a prepare or a
//! drain never waits behind data, and a builder waiting to hand over a payload never deadlocks against it.
//!
//! A controller drives the host through its [control mailbox](saluki_core::execution::control) with
//! [`EncodeControl`] messages: every message names `(EntryId, LeaseKeyDigest)`, and a host ignores, and logs, a pair
//! that does not match its table.

use std::{
    collections::BTreeSet,
    fmt,
    future::Future,
    mem::size_of,
    num::NonZeroU8,
    pin::Pin,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use http::Request;
use saluki_common::{buf::FrozenChunkedBytesBuffer, collections::FastHashMap};
use saluki_core::{
    accounting::{MemoryBounds, MemoryBoundsBuilder},
    components::{
        encoders::{Encoder, EncoderBuilder, EncoderContext},
        BuildContext, ComponentType,
    },
    data_model::{
        event::{metric::Metric, Event, EventType},
        payload::{HttpPayload, Payload, PayloadMetadata, PayloadType},
        tags::SharedTagSet,
    },
    execution::{
        control::{ControlMailbox, ControlReceiver, ControlReply},
        EntryId,
    },
    health::Health,
    runtime,
    support::SubsystemIdentifier,
    topology::{EventsBuffer, PayloadsBuffer, PayloadsDispatcher},
};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_metrics::{Counter, MetricsBuilder};
use stringtheory::MetaString;
use tokio::{
    runtime::Handle,
    select,
    sync::mpsc::{self, error::TrySendError},
};
use tracing::{debug, error, warn};

use super::{run_request_builder, OnInputClose, PayloadSink, StreamBuilders};
use crate::common::datadog::{
    delivery::{EncodeLeaseSpec, EncoderSettings, LeaseKeyDigest, ResolvedRecipients},
    protocol::MetricsPayloadInfo,
};

#[cfg(test)]
mod tests;

/// The buffers each builder shard of a lease holds waiting to be encoded, as the native encoder's events channel does.
pub const LEASE_QUEUE_BUFFERS: usize = 8;

/// The payloads the builder shards of every lease of a host can hold waiting for the front task, as the native
/// encoder's payloads channel does.
const PAYLOAD_CHANNEL_PAYLOADS: usize = 8;

/// The name of the counter of points lost that a buffer naming no live lease adds to, tagged `component_id` (the host)
/// and `class` (`invalid`): the counter a plan's regions count their own losses in.
const PLAN_LOSS_TOTAL: &str = "plan_loss_total";

/// The prefix of the outputs a resources artifact declares, whose leases share one resource group per host.
const DECLARED_OUTPUT_PREFIX: &str = "resources_";

/// The resource group, below an encode host's own, whose children account the builders of the host's native outputs:
/// `<host>.leases.<output>`.
const NATIVE_LEASES_GROUP: &str = "leases";

/// The resource group, below an encode host's own, that accounts the builders of every declared output of the host:
/// `<host>.resources`.
const DECLARED_LEASES_GROUP: &str = "resources";

/// A control message for an encode host.
#[derive(Debug)]
pub enum EncodeControl {
    /// Adds a lease, whose builders and telemetry the controller built off the data path through the host's
    /// [`EncodeLeaseFactory`], and spawns its builder tasks.
    ///
    /// Nothing addresses the lease before the generation whose lease table names it commits. A prepare of a live lease
    /// the host already holds under the same key is acknowledged as done, so a prepare repeated after a controller
    /// restart changes nothing. A prepare whose entry holds a draining lease, of any key, or a live lease of another
    /// key, is rejected: an entry is reused only after its lease retired, and a draining lease takes no buffer.
    Prepare(LeasePrepare),

    /// Drains a lease that no generation uses any more: each builder flushes every open payload, sends it, and exits.
    ///
    /// The message is acknowledged once every payload of the lease is in the delivery host's input, so a recipient
    /// released after the acknowledgment still receives them. If the host stops first, it is acknowledged as stopped.
    /// A drain of a lease the host does not hold, because it already retired or never was prepared, is acknowledged at
    /// once; so is one whose entry holds a lease of another key, which is logged. A drain is how a controller removes a
    /// lease that an aborted preparation created: nothing addressed it, so it closes at once.
    ///
    /// The lease stops taking buffers when the host handles the message: a buffer naming it that arrives later is
    /// counted as lost. A controller therefore drains a lease only once every generation using it retired, which
    /// waits for the last buffer of each to be consumed.
    Drain {
        /// The lease's identifier.
        lease: EntryId,

        /// The lease's key.
        key: LeaseKeyDigest,
    },
}

/// One lease to prepare, with what the host's factory built for it.
#[derive(Debug)]
pub struct LeasePrepare {
    /// What the lease is.
    pub spec: EncodeLeaseSpec,

    /// What the factory built for it.
    pub built: BuiltLease,
}

/// What an encode host's [`EncodeLeaseFactory`] builds for one lease, off the data path: the request builders of each
/// of its builder shards, with their telemetry.
///
/// Opaque outside this crate: a controller only carries it from the factory to the host.
pub struct BuiltLease {
    shards: Vec<BuiltShard>,
}

/// What one builder shard of a lease runs.
enum BuiltShard {
    /// A request builder of the native encoder, with the lease's settings.
    Builders(Box<StreamBuilders>),

    /// A shard whose builder a test plays.
    #[cfg(test)]
    Probe(tokio::sync::oneshot::Sender<tests::ShardProbe>),
}

impl fmt::Debug for BuiltLease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BuiltLease")
            .field("shards", &self.shards.len())
            .finish_non_exhaustive()
    }
}

/// Builds what one lease of an encode host runs with, off the data path.
///
/// An encode host exposes its factory when it is built, so that a controller can build a lease's request builders in
/// its own task before it sends them to the host.
pub trait EncodeLeaseFactory: Send + Sync {
    /// Builds the request builders of one lease.
    ///
    /// # Errors
    ///
    /// Returns an error naming the lease's output if its settings cannot be built, such as an unknown compressor.
    fn build(&self, spec: &EncodeLeaseSpec) -> Result<BuiltLease, GenericError>;
}

/// What an encode host's builder tasks send to its front task, on one channel shared by every lease.
///
/// Each builder's sends are ordered on the channel, and the front task dispatches each one before it reads the next, so
/// once it has dispatched past every shard's [`LeaseClosed`][Self::LeaseClosed] marker, every payload of the lease is
/// in the delivery host's input.
// Nearly every value on the channel is a payload, and markers are rare: boxing payloads would add an allocation to each
// of them and save no space.
#[allow(clippy::large_enum_variant)]
pub enum HostPayload {
    /// A payload a lease built, stamped with the recipients of its protocol variant.
    Payload(PayloadsBuffer),

    /// The marker a builder shard of a lease sends after its last payload, when the lease drains.
    LeaseClosed {
        /// The lease.
        lease: EntryId,

        /// The builder shard.
        shard: u8,
    },
}

impl fmt::Debug for HostPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Payload(_) => f.debug_tuple("Payload").finish_non_exhaustive(),
            Self::LeaseClosed { lease, shard } => f
                .debug_struct("LeaseClosed")
                .field("lease", lease)
                .field("shard", shard)
                .finish(),
        }
    }
}

/// Builds the leases of a metrics encode host: for each builder shard, the request builders of one native encoder
/// stream with the lease's settings, whose telemetry reports under the lease's component identifier.
#[derive(Default)]
struct MetricsLeaseFactory {
    additional_tags: Option<SharedTagSet>,
}

impl EncodeLeaseFactory for MetricsLeaseFactory {
    fn build(&self, spec: &EncodeLeaseSpec) -> Result<BuiltLease, GenericError> {
        let metrics_builder = lease_metrics_builder(&spec.telemetry_id);
        let oversized_drops = metrics_builder.register_counter_with_tags(
            "component_events_dropped_total",
            ["intentional:false", "drop_reason:payload_too_large"],
        );

        let mut shards = Vec::with_capacity(usize::from(spec.builders.get()));
        for _ in 0..spec.builders.get() {
            let mut builders = StreamBuilders::build(&spec.settings, self.additional_tags.clone(), &metrics_builder)
                .with_error_context(|| {
                    format!(
                        "Failed to build the request builders of lease {} for output '{}'.",
                        spec.lease.get(),
                        spec.key.output
                    )
                })?;
            builders.oversized_drops = Some(oversized_drops.clone());
            shards.push(BuiltShard::Builders(Box::new(builders)));
        }

        Ok(BuiltLease { shards })
    }
}

/// Returns the builder of a lease's telemetry: the encoder telemetry of the output the lease runs, under its native
/// component identifier.
fn lease_metrics_builder(telemetry_id: &MetaString) -> MetricsBuilder {
    MetricsBuilder::default()
        .add_default_tag(("component_id", telemetry_id.to_string()))
        .add_default_tag(("component_type", ComponentType::Encoder.as_str()))
}

/// Returns the resource group a lease's builders are accounted in, below `host`, the host's identity (or, when empty,
/// the group's path relative to the host): `leases.<output>` for a native output, one per output of a bounded set, and
/// `resources` for every declared output, which share it.
///
/// Below the host's own group, the builders' memory and CPU stay attributed to the host, as a native encoder's request
/// builder was to the encoder, while each native output's builders are still reported apart.
fn lease_resource_group(host: SubsystemIdentifier, spec: &EncodeLeaseSpec) -> SubsystemIdentifier {
    if spec.telemetry_id.starts_with(DECLARED_OUTPUT_PREFIX) {
        host.child(DECLARED_LEASES_GROUP)
    } else {
        host.child(NATIVE_LEASES_GROUP).child(&*spec.telemetry_id)
    }
}

/// Returns the memory one lease holds at most: for each of its builder shards, the split re-encode buffers of its
/// request builders, which hold up to `max_metrics_per_payload` series and as many sketches, and the full buffers its
/// channel holds.
///
/// A controller admits this bound against its memory budget before it prepares the lease, and returns it when the lease
/// retires.
pub fn lease_bound_bytes(spec: &EncodeLeaseSpec) -> u64 {
    lease_settings_bound_bytes(&spec.settings, spec.builders)
}

/// Returns the memory a lease with `settings` and `builders` shards holds at most, as [`lease_bound_bytes`] does, for
/// a lease not prepared yet, such as one a controller admits before it builds it.
pub fn lease_settings_bound_bytes(settings: &EncoderSettings, builders: NonZeroU8) -> u64 {
    let shards = u64::from(builders.get());
    let reencode_buffers = (settings.max_metrics_per_payload as u64)
        .saturating_mul(2)
        .saturating_mul(size_of::<Metric>() as u64);
    let full_buffer = size_of::<EventsBuffer>() + EventsBuffer::with_capacity(0).capacity() * size_of::<Event>();
    let queued_buffers = (LEASE_QUEUE_BUFFERS * full_buffer) as u64;

    shards.saturating_mul(reencode_buffers.saturating_add(queued_buffers))
}

/// A metrics encode host: one encoder component that runs a lease per output of its delivery host.
///
/// The host starts with the leases it is configured with ([`with_lease`][Self::with_lease]), which are those of the
/// boot generation, and a controller adds and drains leases while it runs through its control mailbox, building each
/// one with the host's [lease factory][Self::lease_factory].
///
/// With one lease per native output and that output's native settings, the host produces the payload bodies and
/// metadata the native metrics encoder produces for the same input, except that a payload names its recipients
/// ([`PayloadRecipients`][crate::forwarders::delivery::PayloadRecipients]) where the native encoder's routing policy
/// names endpoints.
pub struct MetricsEncodeHostConfiguration {
    /// The host's control mailbox, taken when the host is built; `None` for a host without a controller.
    control: Option<Mutex<Option<ControlMailbox<EncodeControl>>>>,
    leases: Vec<EncodeLeaseSpec>,
    factory: Arc<MetricsLeaseFactory>,

    /// What sees the buffers the host routes, and whether the leases still encode them, for tests.
    #[cfg(any(test, feature = "test-util"))]
    observer: Option<(Arc<dyn LeaseObserver>, bool)>,

    /// What the host waits for before it reads its input, for tests.
    gate: InputGate,
}

/// A gate an encode host waits on before it reads its input, for tests: none in production builds.
#[cfg(any(test, feature = "test-util"))]
type InputGate = Option<Arc<tokio::sync::Semaphore>>;

/// A gate an encode host waits on before it reads its input, for tests: none in production builds.
#[cfg(not(any(test, feature = "test-util")))]
type InputGate = ();

/// Waits until the input gate opens, taking its permit, or never without one.
#[cfg(any(test, feature = "test-util"))]
async fn gate_opened(gate: &mut InputGate) {
    match gate {
        Some(semaphore) => {
            if let Ok(permit) = semaphore.acquire().await {
                permit.forget();
            }
            *gate = None;
        }
        None => std::future::pending().await,
    }
}

/// Waits until the input gate opens, taking its permit, or never without one.
#[cfg(not(any(test, feature = "test-util")))]
async fn gate_opened(_: &mut InputGate) {
    std::future::pending().await
}

/// Returns the input gate of a host its configuration builds: the configuration's, which tests open.
#[cfg(any(test, feature = "test-util"))]
fn built_gate(gate: &InputGate) -> InputGate {
    gate.clone()
}

/// Returns the input gate of a host its configuration builds: none in production builds.
#[cfg(not(any(test, feature = "test-util")))]
fn built_gate(_: &InputGate) -> InputGate {}

/// Returns whether the host reads its input: once its gate, if it has one, opened.
#[cfg(any(test, feature = "test-util"))]
fn gate_open(gate: &InputGate) -> bool {
    gate.is_none()
}

/// Returns whether the host reads its input: always, without a gate.
#[cfg(not(any(test, feature = "test-util")))]
fn gate_open(_: &InputGate) -> bool {
    true
}

/// Sees the buffers an encode host routes to its leases, by output, for tests (`test-util`).
#[cfg(any(test, feature = "test-util"))]
pub trait LeaseObserver: Send + Sync {
    /// Sees a buffer the host routes to the lease of `output`, before the lease encodes it.
    fn observe(&self, output: &str, buffer: &EventsBuffer);
}

impl MetricsEncodeHostConfiguration {
    /// Creates the configuration of an encode host that runs no lease and has no controller.
    pub fn new() -> Self {
        Self {
            control: None,
            leases: Vec::new(),
            factory: Arc::new(MetricsLeaseFactory::default()),
            #[cfg(any(test, feature = "test-util"))]
            observer: None,
            gate: InputGate::default(),
        }
    }

    /// Makes the host read nothing from its input until `gate` gives it a permit, for tests (`test-util`): what its
    /// upstream sends backs up, while the host answers its control mailbox and liveness probes.
    #[cfg(any(test, feature = "test-util"))]
    pub fn with_input_gate(mut self, gate: Arc<tokio::sync::Semaphore>) -> Self {
        self.gate = Some(gate);
        self
    }

    /// Shows every buffer the host routes to a lease to `observer`, by the lease's output, for tests (`test-util`).
    ///
    /// With `encode`, each lease then encodes the buffer as it would; without, the host drops it once observed, so
    /// that a test sees what each output receives and nothing is encoded.
    #[cfg(any(test, feature = "test-util"))]
    pub fn with_observer(mut self, observer: Arc<dyn LeaseObserver>, encode: bool) -> Self {
        self.observer = Some((observer, encode));
        self
    }

    /// Gives the host a control mailbox, through which a controller prepares and drains its leases.
    pub fn with_control_mailbox(mut self, mailbox: ControlMailbox<EncodeControl>) -> Self {
        self.control = Some(Mutex::new(Some(mailbox)));
        self
    }

    /// Adds a lease that the host runs from its start, such as a lease of the boot generation.
    ///
    /// The host builds it with its own factory when it is built, so a controller never prepares it.
    pub fn with_lease(mut self, spec: EncodeLeaseSpec) -> Self {
        self.leases.push(spec);
        self
    }

    /// Sets tags that every lease adds to every metric it encodes, as the native encoder's additional tags do.
    ///
    /// The tags belong to the host's factory: set them before taking the factory with
    /// [`lease_factory`][Self::lease_factory], which a factory taken earlier does not see.
    pub fn with_additional_tags(mut self, additional_tags: SharedTagSet) -> Self {
        self.factory = Arc::new(MetricsLeaseFactory {
            additional_tags: Some(additional_tags),
        });
        self
    }

    /// Returns the factory a controller builds the host's leases with, off the data path.
    pub fn lease_factory(&self) -> Arc<dyn EncodeLeaseFactory> {
        Arc::clone(&self.factory) as Arc<dyn EncodeLeaseFactory>
    }

    fn take_control(&self) -> Result<Option<ControlReceiver<EncodeControl>>, GenericError> {
        let Some(control) = &self.control else {
            return Ok(None);
        };
        let mailbox = control
            .lock()
            .map_err(|_| generic_error!("The encode host's control mailbox lock is poisoned."))?
            .take()
            .ok_or_else(|| {
                generic_error!(
                    "The control mailbox of this encode host was already used to build an encoder. An encode host \
                     configuration builds one encoder, since each encode host has its own mailbox."
                )
            })?;
        Ok(Some(mailbox.into_receiver_for("encode host")))
    }
}

impl Default for MetricsEncodeHostConfiguration {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl EncoderBuilder for MetricsEncodeHostConfiguration {
    fn input_event_type(&self) -> EventType {
        EventType::Metric
    }

    fn output_payload_type(&self) -> PayloadType {
        PayloadType::Http
    }

    async fn build(&self, _context: BuildContext) -> Result<Box<dyn Encoder + Send>, GenericError> {
        let boot = self
            .leases
            .iter()
            .map(|spec| {
                Ok(LeasePrepare {
                    spec: spec.clone(),
                    built: self.factory.build(spec)?,
                })
            })
            .collect::<Result<Vec<_>, GenericError>>()?;

        Ok(Box::new(MetricsEncodeHost {
            control: self.take_control()?,
            #[cfg(any(test, feature = "test-util"))]
            observer: self.observer.clone(),
            gate: built_gate(&self.gate),
            boot,
        }))
    }
}

impl MemoryBounds for MetricsEncodeHostConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        // The leases the host starts with count as the native encoders they replace do: each builder shard's events
        // channel, and the split re-encode buffers of its request builders. Those buffers count in the resource group
        // the lease's builders run in, below the host's, so that each group's bounds sit beside its usage. Leases
        // prepared later are admitted by the controller, with `lease_bound_bytes`.
        let shards = self
            .leases
            .iter()
            .map(|spec| usize::from(spec.builders.get()))
            .fold(0, usize::saturating_add);

        builder
            .minimum()
            .with_single_value::<MetricsEncodeHost>("component struct")
            .with_array::<HostPayload>("lease builders payloads channel", PAYLOAD_CHANNEL_PAYLOADS)
            .with_array::<EventsBuffer>(
                "lease builder events channels",
                shards.saturating_mul(LEASE_QUEUE_BUFFERS),
            );

        for spec in &self.leases {
            let reencode_metrics =
                usize::from(spec.builders.get()).saturating_mul(spec.settings.max_metrics_per_payload);
            let group_path = lease_resource_group(SubsystemIdentifier::from_segments(std::iter::empty::<&str>()), spec);
            builder
                .subcomponent(group_path.to_string())
                .firm()
                // The owned copies of the metrics the request builders encode, which they keep in case they have to
                // re-encode them while splitting a payload.
                .with_array::<Metric>("series metrics split re-encode buffers", reencode_metrics)
                .with_array::<Metric>("sketch metrics split re-encode buffers", reencode_metrics);
        }
    }
}

/// A built metrics encode host, which runs as an encoder component.
pub struct MetricsEncodeHost {
    control: Option<ControlReceiver<EncodeControl>>,
    boot: Vec<LeasePrepare>,
    #[cfg(any(test, feature = "test-util"))]
    observer: Option<(Arc<dyn LeaseObserver>, bool)>,
    gate: InputGate,
}

#[async_trait]
impl Encoder for MetricsEncodeHost {
    async fn run(self: Box<Self>, mut context: EncoderContext) -> Result<(), GenericError> {
        #[cfg(any(test, feature = "test-util"))]
        let Self {
            control,
            boot,
            observer,
            gate,
        } = *self;
        #[cfg(not(any(test, feature = "test-util")))]
        let Self { control, boot, gate } = *self;

        let host_id = context.component_context().component_id().to_string();
        let (payloads_tx, payloads) = mpsc::channel(PAYLOAD_CHANNEL_PAYLOADS);
        let (failures_tx, failures) = mpsc::unbounded_channel();
        let spawner = Spawner {
            payloads: Some(payloads_tx),
            failures: failures_tx,
            pool: context.topology_context().global_thread_pool().clone(),
            host: context.component_context().identity(),
        };
        let invalid_loss = MetricsBuilder::default().register_counter_with_tags(
            PLAN_LOSS_TOTAL,
            [("component_id", host_id), ("class", "invalid".to_string())],
        );
        let leases = Leases::new(spawner, invalid_loss);
        #[cfg(any(test, feature = "test-util"))]
        let leases = Leases { observer, ..leases };
        let mut host = Host {
            leases,
            health: context.take_health_handle(),
            control,
            payloads,
            failures,
            gate,
        };

        for prepare in boot {
            let lease = prepare.spec.lease.get();
            host.leases
                .prepare(prepare)
                .with_error_context(|| format!("Failed to start lease {} of the encode host.", lease))?;
        }

        host.health.mark_ready();
        // From here on the mailbox is read: whoever controls the host can start timing its messages.
        if let Some(control) = &host.control {
            control.mark_running();
        }
        debug!(leases = host.leases.len(), "Metrics encode host started.");

        host.run(&mut context).await
    }
}

/// A buffer on its way into a lease's shard that had no room for it when it arrived.
type PendingSend = Pin<Box<dyn Future<Output = Result<(), GenericError>> + Send>>;

/// The front task of an encode host.
struct Host {
    leases: Leases,
    health: Health,
    /// The control mailbox, `None` once every controller is gone or the host shuts down.
    control: Option<ControlReceiver<EncodeControl>>,
    payloads: mpsc::Receiver<HostPayload>,
    failures: mpsc::UnboundedReceiver<BuilderFailure>,
    gate: InputGate,
}

impl Host {
    async fn run(mut self, context: &mut EncoderContext) -> Result<(), GenericError> {
        let mut pending = None;
        loop {
            if let Some(send) = pending.take() {
                self.wait_for_room(context.dispatcher(), send).await?;
                continue;
            }

            select! {
                biased;

                _ = self.health.live() => {},
                Some(failure) = self.failures.recv() => return Err(failure.into_error()),
                maybe_message = next_control(&mut self.control) => self.handle_control(maybe_message),
                Some(payload) = self.payloads.recv() => self.forward(context.dispatcher(), payload).await?,
                _ = gate_opened(&mut self.gate), if !gate_open(&self.gate) => {},
                maybe_buffer = context.events().next(), if gate_open(&self.gate) => match maybe_buffer {
                    Some(buffer) => pending = self.leases.route(buffer)?,
                    None => break,
                },
            }
        }

        self.shut_down(context.dispatcher()).await
    }

    /// Waits until a lease's shard takes the buffer that had no room in it, meanwhile answering liveness probes and
    /// control messages, and dispatching what the builders send, so that the builder being waited for can make room.
    async fn wait_for_room(
        &mut self, dispatcher: &PayloadsDispatcher, mut send: PendingSend,
    ) -> Result<(), GenericError> {
        loop {
            select! {
                biased;

                result = &mut send => return result,
                _ = self.health.live() => {},
                Some(failure) = self.failures.recv() => return Err(failure.into_error()),
                maybe_message = next_control(&mut self.control) => self.handle_control(maybe_message),
                Some(payload) = self.payloads.recv() => self.forward(dispatcher, payload).await?,
            }
        }
    }

    /// Dispatches a payload to the delivery host, or retires what a lease's closing marker completes.
    async fn forward(&mut self, dispatcher: &PayloadsDispatcher, payload: HostPayload) -> Result<(), GenericError> {
        match payload {
            HostPayload::Payload(payload) => self.dispatch(dispatcher, payload).await,
            HostPayload::LeaseClosed { lease, shard } => {
                self.leases.shard_closed(lease, shard);
                Ok(())
            }
        }
    }

    /// Dispatches one payload to the delivery host, answering liveness probes and control messages while the delivery
    /// host has no room for it.
    async fn dispatch(&mut self, dispatcher: &PayloadsDispatcher, payload: Payload) -> Result<(), GenericError> {
        let send = dispatcher.dispatch(payload);
        tokio::pin!(send);
        loop {
            select! {
                biased;

                result = &mut send => {
                    if let Err(e) = result {
                        error!("Failed to dispatch payload: {}", e);
                    }
                    return Ok(());
                },
                _ = self.health.live() => {},
                Some(failure) = self.failures.recv() => return Err(failure.into_error()),
                maybe_message = next_control(&mut self.control) => self.handle_control(maybe_message),
            }
        }
    }

    fn handle_control(&mut self, maybe_message: Option<(EncodeControl, ControlReply)>) {
        let Some((message, reply)) = maybe_message else {
            // Every controller is gone, so no message arrives any more.
            self.control = None;
            return;
        };

        match message {
            EncodeControl::Prepare(prepare) => match self.leases.prepare(prepare) {
                Ok(()) => reply.ack(),
                Err(e) => reply.reject(e),
            },
            EncodeControl::Drain { lease, key } => self.leases.drain(lease, &key, reply),
        }
    }

    /// Closes every lease once the host's input ended, and dispatches what their builders flush until every builder
    /// exited.
    async fn shut_down(mut self, dispatcher: &PayloadsDispatcher) -> Result<(), GenericError> {
        // Messages still in the mailbox, and any sent from now on, are acknowledged as stopped.
        self.control = None;
        let leases = self.leases.close_all();
        debug!(leases, "Metrics encode host input closed. Flushing every lease.");

        // The channel closes once every builder exited, since the host dropped its own sender.
        loop {
            select! {
                biased;

                _ = self.health.live() => {},
                maybe_payload = self.payloads.recv() => match maybe_payload {
                    Some(HostPayload::Payload(payload)) => {
                        if let Err(e) = dispatcher.dispatch(payload).await {
                            error!("Failed to dispatch payload: {}", e);
                        }
                    }
                    Some(HostPayload::LeaseClosed { lease, shard }) => self.leases.shard_closed(lease, shard),
                    None => break,
                },
            }
        }

        if let Ok(failure) = self.failures.try_recv() {
            return Err(failure.into_error());
        }

        debug!("Metrics encode host stopped.");
        Ok(())
    }
}

/// Waits for the next control message, or forever once the mailbox is closed.
async fn next_control(control: &mut Option<ControlReceiver<EncodeControl>>) -> Option<(EncodeControl, ControlReply)> {
    match control {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}

/// The lease table of an encode host, by lease identifier.
///
/// A map rather than a vector indexed by identifier: the installer allocates identifiers in rotation, so over the life
/// of the process they reach every value, and a vector would grow to the largest one used.
struct Leases {
    held: FastHashMap<EntryId, Lease>,
    spawner: Spawner,
    invalid_loss: Counter,
    /// The entries a wiring fault was logged for, so that each is logged once.
    faults_logged: BTreeSet<Option<EntryId>>,
    /// What sees each buffer routed to a lease, and whether the lease still encodes it, for tests.
    #[cfg(any(test, feature = "test-util"))]
    observer: Option<(Arc<dyn LeaseObserver>, bool)>,
}

/// One lease of an encode host.
struct Lease {
    key: LeaseKeyDigest,
    /// The senders of the lease's builder shards while it is live, and none once it drains: dropping them closes each
    /// shard after the buffers it holds.
    shards: Vec<mpsc::Sender<EventsBuffer>>,
    /// Whether each builder shard is still open, until its closing marker arrives.
    open: Vec<bool>,
    /// The drains of the lease waiting for every shard to close.
    drains: Vec<ControlReply>,
    /// The shard the next buffer starts looking for room from, when the lease has more than one.
    next_shard: usize,
}

impl Lease {
    /// Returns whether the lease is draining: it takes no buffer any more, and retires once every shard closed.
    fn is_draining(&self) -> bool {
        self.shards.is_empty()
    }

    /// Returns the shard a buffer goes to: the one with the most room, the first such from a rotating start; or `None`
    /// if the lease is draining, so takes no buffer.
    fn pick_shard(&mut self) -> Option<usize> {
        let count = self.shards.len();
        match count {
            0 => return None,
            1 => return Some(0),
            _ => {}
        }

        let start = self.next_shard % count;
        self.next_shard = start + 1;
        let mut best = start;
        let mut best_room = self.shards[start].capacity();
        for offset in 1..count {
            let shard = (start + offset) % count;
            let room = self.shards[shard].capacity();
            if room > best_room {
                best = shard;
                best_room = room;
            }
        }
        Some(best)
    }
}

impl Leases {
    fn new(spawner: Spawner, invalid_loss: Counter) -> Self {
        Self {
            held: FastHashMap::default(),
            spawner,
            invalid_loss,
            faults_logged: BTreeSet::new(),
            #[cfg(any(test, feature = "test-util"))]
            observer: None,
        }
    }

    /// Returns the number of leases held, live or draining.
    fn len(&self) -> usize {
        self.held.len()
    }

    fn held(&mut self, lease: EntryId) -> Option<&mut Lease> {
        self.held.get_mut(&lease)
    }

    /// Adds a lease and spawns its builder shards.
    fn prepare(&mut self, prepare: LeasePrepare) -> Result<(), GenericError> {
        let LeasePrepare { spec, built } = prepare;
        if let Some(held) = self.held(spec.lease) {
            if held.is_draining() {
                // Acknowledging it would commit generations over a lease that drops every buffer they send it.
                return Err(generic_error!(
                    "Lease {} cannot be prepared for output '{}': this encode host still holds it for output '{}', \
                     and is draining it. A lease identifier is reused only after its lease retired.",
                    spec.lease.get(),
                    spec.key.output,
                    held.key.output
                ));
            }
            if held.key == spec.key {
                debug!(
                    lease = spec.lease.get(),
                    output = %spec.key.output,
                    "Lease already prepared."
                );
                return Ok(());
            }

            return Err(generic_error!(
                "Lease {} cannot be prepared for output '{}': this encode host still holds it for output '{}'. A \
                 lease identifier is reused only after its lease retired.",
                spec.lease.get(),
                spec.key.output,
                held.key.output
            ));
        }
        if built.shards.len() != usize::from(spec.builders.get()) {
            return Err(generic_error!(
                "Lease {} for output '{}' was built with {} builder shards, but its specification asks for {}. Build \
                 the lease from its specification with this encode host's lease factory.",
                spec.lease.get(),
                spec.key.output,
                built.shards.len(),
                spec.builders.get()
            ));
        }

        let mut shards = Vec::with_capacity(built.shards.len());
        for (shard, built) in (0..=u8::MAX).zip(built.shards) {
            shards.push(self.spawner.spawn(&spec, shard, built)?);
        }

        debug!(
            lease = spec.lease.get(),
            output = %spec.key.output,
            shards = shards.len(),
            "Lease prepared."
        );
        self.held.insert(
            spec.lease,
            Lease {
                key: spec.key,
                open: vec![true; shards.len()],
                shards,
                drains: Vec::new(),
                next_shard: 0,
            },
        );
        Ok(())
    }

    /// Moves a buffer into a shard of the lease its header names, or returns the send that waits for room in it.
    ///
    /// A buffer that names no live lease is counted as lost and dropped.
    fn route(&mut self, buffer: EventsBuffer) -> Result<Option<PendingSend>, GenericError> {
        let entry = buffer.header().entry;
        // A draining lease has no shard to pick.
        let Some((lease, shard)) = entry
            .and_then(|entry| self.held.get_mut(&entry))
            .and_then(|lease| lease.pick_shard().map(|shard| (lease, shard)))
        else {
            self.count_invalid(entry, &buffer);
            return Ok(None);
        };
        #[cfg(any(test, feature = "test-util"))]
        if let Some((observer, encode)) = &self.observer {
            observer.observe(&lease.key.output, &buffer);
            if !encode {
                return Ok(None);
            }
        }

        match lease.shards[shard].try_send(buffer) {
            Ok(()) => Ok(None),
            Err(TrySendError::Full(buffer)) => {
                let sender = lease.shards[shard].clone();
                let stopped = builder_stopped(entry, shard, &lease.key);
                Ok(Some(Box::pin(
                    async move { sender.send(buffer).await.map_err(|_| stopped) },
                )))
            }
            Err(TrySendError::Closed(_)) => Err(builder_stopped(entry, shard, &lease.key)),
        }
    }

    fn count_invalid(&mut self, entry: Option<EntryId>, buffer: &EventsBuffer) {
        let points = buffer
            .into_iter()
            .map(|event| event.try_as_metric().map_or(0, |metric| metric.values().len() as u64))
            .sum::<u64>();
        self.invalid_loss.increment(points);

        if self.faults_logged.insert(entry) {
            warn!(
                entry = ?entry.map(EntryId::get),
                events = buffer.len(),
                points,
                "Dropped a buffer whose entry names no live lease of this encode host. This is a wiring fault: a \
                 lease is prepared before any generation addresses it, and drained after the last one retired. Later \
                 buffers with this entry are counted as lost without being logged."
            );
        }
    }

    /// Starts draining a lease: its shards close after the buffers they hold, and the drain is acknowledged once
    /// every shard's closing marker was dispatched past.
    fn drain(&mut self, lease: EntryId, key: &LeaseKeyDigest, reply: ControlReply) {
        match self.held(lease) {
            Some(held) if held.key == *key => {
                if !held.is_draining() {
                    debug!(lease = lease.get(), output = %key.output, "Draining lease.");
                    held.shards.clear();
                }
                held.drains.push(reply);
            }
            Some(held) => {
                warn!(
                    lease = lease.get(),
                    output = %key.output,
                    held = %held.key.output,
                    "Ignoring the drain of a lease this encode host does not hold: its entry holds a lease of another \
                     key. The lease named retired already."
                );
                reply.ack();
            }
            None => {
                debug!(
                    lease = lease.get(),
                    output = %key.output,
                    "The lease to drain is not held, so it retired already."
                );
                reply.ack();
            }
        }
    }

    /// Records that a builder shard of a draining lease sent its last payload, and retires the lease once every shard
    /// did, acknowledging its drains.
    fn shard_closed(&mut self, lease: EntryId, shard: u8) {
        let Some(held) = self.held(lease) else {
            warn!(
                lease = lease.get(),
                shard, "A builder shard closed for a lease this encode host does not hold."
            );
            return;
        };
        debug_assert!(held.is_draining(), "a builder shard closes only once its lease drains");

        match held.open.get_mut(usize::from(shard)) {
            Some(open) => *open = false,
            None => warn!(
                lease = lease.get(),
                shard,
                output = %held.key.output,
                "A builder shard closed that its lease does not have."
            ),
        }
        if held.open.iter().any(|open| *open) {
            return;
        }

        if let Some(retired) = self.held.remove(&lease) {
            debug!(lease = lease.get(), output = %retired.key.output, "Lease drained and retired.");
            for reply in retired.drains {
                reply.ack();
            }
        }
    }

    /// Closes every lease, at shutdown, and returns how many there were: pending drains are dropped, which answers
    /// them as stopped, and the host's own payload sender is dropped, so the payload channel closes once every builder
    /// exited.
    fn close_all(&mut self) -> usize {
        self.spawner.payloads = None;
        for lease in self.held.values_mut() {
            lease.shards.clear();
            lease.drains.clear();
        }
        self.held.len()
    }
}

fn builder_stopped(entry: Option<EntryId>, shard: usize, key: &LeaseKeyDigest) -> GenericError {
    generic_error!(
        "Builder shard {} of lease {} for output '{}' stopped while the lease was live.",
        shard,
        entry.map_or(0, EntryId::get),
        key.output
    )
}

/// Spawns the builder shards of an encode host's leases.
struct Spawner {
    /// The sender the builders' payloads take to the front task, which the host drops when it shuts down.
    payloads: Option<mpsc::Sender<HostPayload>>,
    failures: mpsc::UnboundedSender<BuilderFailure>,
    pool: Handle,
    /// The host's identity, which is also its resource group: the leases' groups are below it.
    host: SubsystemIdentifier,
}

impl Spawner {
    /// Spawns one builder shard of a lease, and returns the sender of its channel.
    fn spawn(
        &self, spec: &EncodeLeaseSpec, shard: u8, built: BuiltShard,
    ) -> Result<mpsc::Sender<EventsBuffer>, GenericError> {
        let payloads = self
            .payloads
            .clone()
            .ok_or_else(|| generic_error!("The encode host is shutting down, so it prepares no lease."))?;
        let (sender, receiver) = mpsc::channel(LEASE_QUEUE_BUFFERS);
        let identity = ShardIdentity {
            lease: spec.lease,
            shard,
            output: spec.key.output.clone(),
        };
        let sink = LeaseSink {
            payloads,
            recipients: Arc::clone(&spec.recipients),
        };

        match built {
            BuiltShard::Builders(builders) => {
                let task = run_shard(identity, *builders, receiver, sink, self.failures.clone());
                runtime::worker("request_builder", task)
                    .on_runtime(self.pool.clone())
                    .with_resource_group(lease_resource_group(self.host.clone(), spec).to_string())
                    .spawn_child_or_detached();
            }
            #[cfg(test)]
            BuiltShard::Probe(probe) => {
                let exit = ShardExit {
                    identity: identity.clone(),
                    failures: Some(self.failures.clone()),
                };
                let _ = probe.send(tests::ShardProbe {
                    identity,
                    events: receiver,
                    sink,
                    exit,
                });
            }
        }

        Ok(sender)
    }
}

/// Which builder shard of which lease a builder task runs.
#[derive(Clone, Debug)]
struct ShardIdentity {
    lease: EntryId,
    shard: u8,
    output: MetaString,
}

/// A builder shard that stopped other than by closing: its error, or a panic.
struct BuilderFailure {
    identity: ShardIdentity,
    reason: String,
}

impl BuilderFailure {
    fn into_error(self) -> GenericError {
        generic_error!(
            "Builder shard {} of lease {} for output '{}' failed: {}",
            self.identity.shard,
            self.identity.lease.get(),
            self.identity.output,
            self.reason
        )
    }
}

/// Reports a builder shard's failure to the front task, unless the shard closed: when its task returns an error, or
/// is dropped without finishing, as when it panics.
struct ShardExit {
    identity: ShardIdentity,
    failures: Option<mpsc::UnboundedSender<BuilderFailure>>,
}

impl ShardExit {
    /// Records that the shard closed, so that nothing is reported.
    fn closed(&mut self) {
        self.failures = None;
    }

    /// Reports the shard's failure.
    fn failed(&mut self, reason: String) {
        if let Some(failures) = self.failures.take() {
            let _ = failures.send(BuilderFailure {
                identity: self.identity.clone(),
                reason,
            });
        }
    }
}

impl Drop for ShardExit {
    fn drop(&mut self) {
        self.failed("its task stopped before the shard closed: it panicked, or was cancelled".to_string());
    }
}

/// Runs one builder shard of a lease until its channel closes, then sends the shard's closing marker after its last
/// payload.
async fn run_shard(
    identity: ShardIdentity, builders: StreamBuilders, events: mpsc::Receiver<EventsBuffer>, sink: LeaseSink,
    failures: mpsc::UnboundedSender<BuilderFailure>,
) -> Result<(), GenericError> {
    let payloads = sink.payloads.clone();
    let mut exit = ShardExit {
        identity: identity.clone(),
        failures: Some(failures),
    };

    match run_request_builder(builders, events, sink, OnInputClose::FlushNow).await {
        Ok(()) => {
            exit.closed();
            // The front task is gone only once the host stopped, when nothing waits for the marker any more.
            let _ = payloads
                .send(HostPayload::LeaseClosed {
                    lease: identity.lease,
                    shard: identity.shard,
                })
                .await;
            Ok(())
        }
        Err(e) => {
            error!(
                lease = identity.lease.get(),
                shard = identity.shard,
                output = %identity.output,
                error = %e,
                "Request builder of an encode host lease failed."
            );
            exit.failed(format!("{:#}", e));
            Err(e)
        }
    }
}

/// Where a lease's builder shards send their payloads: the host's payload channel, with each payload stamped with the
/// recipients the lease resolved for its protocol variant.
struct LeaseSink {
    payloads: mpsc::Sender<HostPayload>,
    recipients: Arc<ResolvedRecipients>,
}

impl PayloadSink for LeaseSink {
    async fn send_payload(
        &mut self, mut metadata: PayloadMetadata, request: Request<FrozenChunkedBytesBuffer>,
        variant: MetricsPayloadInfo,
    ) -> Result<(), GenericError> {
        metadata.set(self.recipients.for_payload(variant).clone());
        self.payloads
            .send(HostPayload::Payload(Payload::Http(HttpPayload::new(metadata, request))))
            .await
            .map_err(|_| generic_error!("The encode host stopped before it took a payload."))
    }
}
