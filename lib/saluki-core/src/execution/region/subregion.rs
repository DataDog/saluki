//! Sub-regions: tasks inside one region component, each running a region of the program of its own, started and
//! retired by control messages.
//!
//! A region component runs its own lanes, which read the component's input, and may run **sub-regions** beside them. A
//! sub-region is a task like a lane: it has its own driver, its own input channel, its own control queue, and its own
//! resource group (`<component>.<name>`), shares the component's dispatcher, and is coordinated by the component's task.
//! Unlike a lane, it is not fed by the topology but by the other regions of its component, through their **internal
//! exits** ([`ExitSink::sub_region_exit`][super::ExitSink::sub_region_exit]), and it is not built with the component
//! but started by a control message, and retired once nothing routes to it.
//!
//! # Starting
//!
//! A control message for a region with sub-regions splits into the part for the component's own lanes and one part per
//! sub-region it addresses ([`SubRegionSplit`]). A part that may start a sub-region carries its [`SubRegionSpec`]: if
//! no sub-region with its key runs, the coordinator builds one, with the factory given to
//! [`RegionHostBuilder::with_sub_regions`][super::RegionHostBuilder::with_sub_regions] and the part itself, before it
//! sends any part of the message, and then sends the part as the sub-region's first control message. A driver that
//! cannot be built rejects the message as a whole, with no part sent. The message is acknowledged once every part was,
//! so a sub-region a message starts runs, and has applied its part, by the time the message is acknowledged.
//!
//! # Feeding and retiring
//!
//! A sub-region runs while anything holds a sender to its input. Each region of the component holds one for each
//! sub-region its driver lists ([`RegionDriver::sub_region_routes`][super::RegionDriver::sub_region_routes]), which it
//! connects to after each control message, before acknowledging it, and the coordinator holds one for every running
//! sub-region from when it routes a message until the message is acknowledged. Once no driver lists a sub-region, and
//! no message is being acknowledged, its input ends: it processes what its input still holds, finishes under
//! [`DrainPolicy::Retire`], and stops. A message that addresses its key from then on starts a new sub-region; the one
//! retiring takes no more messages.
//!
//! # Shutting down and failing
//!
//! When the component's input ends, its lanes finish, and drop their senders once they did: each sub-region's input
//! then ends once every region that sends to it finished, so sub-regions finish in the order their regions feed each
//! other, each under [`DrainPolicy::Shutdown`], and the component stops once every one stopped. A sub-region that fails
//! fails the component, as a lane does; the other lanes and sub-regions then stop without finishing.

use std::{
    collections::BTreeMap,
    fmt,
    num::NonZeroUsize,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc, Mutex, PoisonError,
    },
};

use futures::{future::BoxFuture, stream::FuturesUnordered, FutureExt as _, StreamExt as _};
use saluki_error::{generic_error, GenericError};
use tokio::{
    select,
    sync::{mpsc, oneshot},
};
use tracing::{debug, warn};

use super::{
    driver::{internal_exit, RegionDriver},
    host::{join_parts, panic_message, HostTelemetry, Lane, LaneInput, PartTarget},
    lanes::{lane_link, LaneLink, LaneProber},
};
use crate::{
    components::ComponentContext,
    execution::{
        control::{control_mailbox, ControlAck, ControlReceiver, ControlReply, ControlSender},
        Budget, DrainPolicy,
    },
    runtime,
    support::SubsystemIdentifier,
    topology::{EventsBuffer, EventsDispatcher},
};

/// The name of every sub-region task, a category label for task telemetry; each sub-region's resource group tells
/// sub-regions apart.
const SUB_REGION_TASK_NAME: &str = "region_sub_region";

/// Identifies a sub-region within its component.
///
/// Whoever controls the region allocates keys, one per region of its program, and keeps a key while any live
/// generation has that region. The internal exit of sub-region `k` is the region's exit `exit_count + k`, so a key
/// must leave that below 65,536.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SubRegionKey(u16);

impl SubRegionKey {
    /// Creates a sub-region key.
    pub const fn new(key: u16) -> Self {
        Self(key)
    }

    /// Returns the key's number.
    pub const fn get(self) -> u16 {
        self.0
    }
}

impl fmt::Display for SubRegionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// What a region host needs to start a sub-region.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubRegionSpec {
    /// The sub-region's key.
    pub key: SubRegionKey,

    /// The sub-region's name, which names its resource group below the component's, and appears in its logs and in the
    /// errors of its control messages.
    pub name: String,

    /// The number of buffers its input channel holds, each of at most an events buffer's capacity. The region's
    /// memory bounds do not count them: whoever starts a sub-region accounts for them, with every exit's in-flight
    /// buffer of its task, against a budget of its own.
    pub input_buffers: NonZeroUsize,
}

/// The part of a control message for one sub-region.
#[derive(Debug)]
pub enum SubRegionPart<C> {
    /// For the sub-region the spec describes, which the coordinator starts first, from this part, if no sub-region with
    /// the spec's key runs.
    Start(SubRegionSpec, C),

    /// For the sub-region with this key, if one runs. A part for a sub-region that does not run is dropped.
    Running(SubRegionKey, C),
}

/// A control message split for a region with sub-regions: the part for the component's own lanes, and the parts for
/// its sub-regions.
#[derive(Debug)]
pub struct RegionParts<C> {
    /// The part for the component's own lanes, which a region with lanes splits further ([`LaneSplit`]), if any.
    ///
    /// [`LaneSplit`]: crate::execution::LaneSplit
    pub main: Option<C>,

    /// The parts for sub-regions, at most one per sub-region.
    pub sub_regions: Vec<SubRegionPart<C>>,
}

impl<C> RegionParts<C> {
    /// Returns parts with everything for the component's own lanes, and nothing for any sub-region.
    pub fn main(message: C) -> Self {
        Self {
            main: Some(message),
            sub_regions: Vec::new(),
        }
    }
}

/// A control message that a region with sub-regions splits into the part for its own lanes and the parts for its
/// sub-regions.
pub trait SubRegionSplit: Sized {
    /// Splits the message, given the keys of the sub-regions that run now, in order.
    ///
    /// A message that prepares a generation starts each sub-region it has a region for ([`SubRegionPart::Start`]); one
    /// that every region applies, such as an abort, copies itself to every running sub-region
    /// ([`SubRegionPart::Running`]); and one for the lanes alone, such as a message for one lane's kernels, returns
    /// itself as the main part ([`RegionParts::main`]). A message whose parts are all dropped, or that has none, is left
    /// unhandled, as by a region that stopped.
    fn split_sub_regions(self, running: &[SubRegionKey]) -> RegionParts<Self>;
}

/// Builds the driver of a sub-region from its spec and the part of the control message that starts it.
pub(super) type SubRegionFactory<D> =
    Arc<dyn Fn(&SubRegionSpec, &<D as RegionDriver>::Control) -> Result<D, GenericError> + Send + Sync>;

/// How a region host runs sub-regions: how it builds their drivers, splits its control messages, and budgets their
/// calls.
pub(super) struct SubRegionConfig<D>
where
    D: RegionDriver,
{
    pub(super) factory: SubRegionFactory<D>,
    pub(super) split: fn(D::Control, &[SubRegionKey]) -> RegionParts<D::Control>,
    pub(super) turn_budget: Budget,
}

impl<D> Clone for SubRegionConfig<D>
where
    D: RegionDriver,
{
    fn clone(&self) -> Self {
        Self {
            factory: Arc::clone(&self.factory),
            split: self.split,
            turn_budget: self.turn_budget,
        }
    }
}

const RUNNING: u8 = 0;
const SHUTTING_DOWN: u8 = 1;
const FAILED: u8 = 2;

/// Where a component with sub-regions is in its life, which every task of the component shares: it tells a sub-region
/// whose input ends whether it retires, the component shuts down, or the component failed.
#[derive(Clone, Debug, Default)]
pub(super) struct HostPhase(Arc<AtomicU8>);

impl HostPhase {
    /// Records that the component's input ended: one of its lanes saw its own input end.
    pub(super) fn shut_down(&self) {
        let _ = self
            .0
            .compare_exchange(RUNNING, SHUTTING_DOWN, Ordering::SeqCst, Ordering::SeqCst);
    }

    /// Records that the component failed.
    pub(super) fn fail(&self) {
        self.0.store(FAILED, Ordering::SeqCst);
    }

    /// Returns the policy a sub-region whose input ended finishes with, or `None` if the component failed, so that the
    /// sub-region stops without finishing.
    ///
    /// Every lane records the end of the component's input before it finishes, and drops its senders only after, so a
    /// sub-region whose input ends because the lanes finished reads it.
    pub(super) fn finish_policy(&self) -> Option<DrainPolicy> {
        match self.0.load(Ordering::SeqCst) {
            RUNNING => Some(DrainPolicy::Retire),
            SHUTTING_DOWN => Some(DrainPolicy::Shutdown),
            _ => None,
        }
    }
}

/// The inputs of a component's running sub-regions, by key, which every task of the component connects to.
///
/// It holds no sender itself, so it keeps no sub-region running.
#[derive(Default)]
pub(super) struct SubRegionRegistry {
    entries: Mutex<BTreeMap<SubRegionKey, RegistryEntry>>,
}

struct RegistryEntry {
    instance: u64,
    name: Arc<str>,
    input: mpsc::WeakSender<EventsBuffer>,
}

impl SubRegionRegistry {
    fn insert(&self, key: SubRegionKey, instance: u64, name: Arc<str>, input: mpsc::WeakSender<EventsBuffer>) {
        self.lock().insert(key, RegistryEntry { instance, name, input });
    }

    /// Removes the entry of `key` if it is still that of the sub-region `instance`, which stopped.
    fn remove(&self, key: SubRegionKey, instance: u64) {
        let mut entries = self.lock();
        if entries.get(&key).is_some_and(|entry| entry.instance == instance) {
            entries.remove(&key);
        }
    }

    /// Connects to the input of the sub-region `key`, if one runs.
    fn connect(&self, key: SubRegionKey) -> Option<InternalExit> {
        let entries = self.lock();
        let entry = entries.get(&key)?;
        Some(InternalExit {
            key,
            name: Arc::clone(&entry.name),
            input: entry.input.upgrade()?,
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<SubRegionKey, RegistryEntry>> {
        // The map stays consistent whatever panicked while it was locked: every change is one insertion or removal.
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A connection to the input of one sub-region.
#[derive(Clone)]
pub(super) struct InternalExit {
    pub(super) key: SubRegionKey,
    pub(super) name: Arc<str>,
    pub(super) input: mpsc::Sender<EventsBuffer>,
}

/// The connections of one task of a component to the inputs of the sub-regions its driver routes to.
pub(super) struct InternalExits {
    registry: Option<Arc<SubRegionRegistry>>,
    held: Vec<InternalExit>,
    unsettled: bool,
}

impl InternalExits {
    /// Returns the connections of a task in a component without sub-regions.
    pub(super) fn none() -> Self {
        Self {
            registry: None,
            held: Vec::new(),
            unsettled: false,
        }
    }

    /// Returns the connections of a task in a component whose sub-regions `registry` holds, connected to none yet.
    pub(super) fn new(registry: Arc<SubRegionRegistry>) -> Self {
        Self {
            registry: Some(registry),
            ..Self::none()
        }
    }

    /// Returns `true` if the component has sub-regions.
    pub(super) fn has_sub_regions(&self) -> bool {
        self.registry.is_some()
    }

    /// Connects to every sub-region of `routes` not connected yet.
    ///
    /// # Errors
    ///
    /// If a sub-region of `routes` does not run, or the component has no sub-regions, an error is returned: the driver
    /// routes to a sub-region that no control message started.
    pub(super) fn connect(&mut self, routes: &[SubRegionKey]) -> Result<(), GenericError> {
        if routes.is_empty() && self.held.is_empty() {
            return Ok(());
        }
        // Whatever is no longer routed is dropped once everything written to it was sent.
        self.unsettled = true;
        for key in routes {
            if self.held.iter().any(|held| held.key == *key) {
                continue;
            }
            let Some(registry) = &self.registry else {
                return Err(generic_error!(
                    "The region's driver routes to sub-region {}, but the region's host has no sub-regions. Build \
                     the host with `RegionHostBuilder::with_sub_regions`.",
                    key
                ));
            };
            let exit = registry.connect(*key).ok_or_else(|| {
                generic_error!(
                    "The region's driver routes to sub-region {}, which does not run: no control message started it, \
                     or it retired. A driver lists a sub-region only once a control message that starts it, or one \
                     sent while it runs, gave it a route there.",
                    key
                )
            })?;
            self.held.push(exit);
        }
        Ok(())
    }

    /// Notes that the driver's routes may have shrunk, as after it released generations.
    pub(super) fn unsettle(&mut self) {
        self.unsettled |= !self.held.is_empty();
    }

    /// Drops the connection to every sub-region `routes` no longer has, if the routes may have shrunk since the last
    /// call. The caller calls this only once it sent everything its driver wrote.
    pub(super) fn settle(&mut self, routes: &[SubRegionKey]) {
        if std::mem::take(&mut self.unsettled) {
            self.held.retain(|held| routes.contains(&held.key));
        }
    }

    /// Returns the connection to the sub-region `key`, if there is one.
    pub(super) fn get(&self, key: SubRegionKey) -> Option<&InternalExit> {
        self.held.iter().find(|held| held.key == key)
    }
}

/// The coordinator's link to one sub-region task.
pub(super) struct SubRegionLink<C> {
    key: SubRegionKey,
    instance: u64,
    name: Arc<str>,
    link: LaneLink<C>,
    input: mpsc::WeakSender<EventsBuffer>,
}

impl<C> SubRegionLink<C> {
    /// Returns the sub-region's name.
    pub(super) fn name(&self) -> &str {
        &self.name
    }

    /// Returns `true` if the sub-region reported that it finished.
    pub(super) fn has_finished(&self) -> bool {
        self.link.has_finished()
    }

    /// Sends the sub-region a control message, and returns its acknowledgment.
    pub(super) fn send(&self, message: C) -> ControlAck {
        self.link.send(message)
    }
}

/// The probers of every sub-region task of a component that has not stopped, shared with whoever forwards the
/// component's liveness probes.
#[derive(Clone, Default)]
pub(super) struct SubRegionProbers(Arc<Mutex<Vec<(u64, LaneProber)>>>);

impl SubRegionProbers {
    /// Returns the probers of the sub-regions as of now.
    pub(super) fn snapshot(&self) -> Vec<LaneProber> {
        self.lock().iter().map(|(_, prober)| prober.clone()).collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<(u64, LaneProber)>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A control message routed to a component's sub-regions, with the parts not sent yet.
pub(super) struct Routed<C> {
    /// The part for the component's own lanes.
    pub(super) main: Option<C>,

    /// The part for each sub-region it addresses that runs.
    pub(super) parts: Vec<(Arc<SubRegionLink<C>>, C)>,

    /// A sender to the input of every sub-region that runs, which keeps it running until the message is acknowledged.
    pub(super) held: Vec<mpsc::Sender<EventsBuffer>>,
}

/// What the sub-regions of a running component share from it.
struct Attachment {
    dispatcher: Arc<EventsDispatcher>,
    group: SubsystemIdentifier,
    region: String,
}

/// The mailbox through which the one lane of a region without lanes hands itself its part of a message.
type Loopback<C> = (ControlSender<C>, ControlReceiver<C>);

/// How one sub-region task ended, as the task reports it.
type SubRegionExit = (u64, Result<Result<(), GenericError>, oneshot::error::RecvError>);

/// The sub-regions of one region component, as its coordinating task keeps them: the task of a region with lanes, or
/// the one lane of a region without.
pub(super) struct SubRegions<D>
where
    D: RegionDriver,
{
    config: SubRegionConfig<D>,
    exit_names: Arc<[String]>,
    work_per_turn: NonZeroUsize,
    component: ComponentContext,
    registry: Arc<SubRegionRegistry>,
    phase: HostPhase,
    attached: Option<Attachment>,

    /// The running sub-regions, by key: those that anything still holds a sender to, as of the last message routed.
    live: BTreeMap<SubRegionKey, Arc<SubRegionLink<D::Control>>>,

    /// Every sub-region task that has not stopped, running or retiring, by instance.
    tasks: BTreeMap<u64, Arc<SubRegionLink<D::Control>>>,
    probers: SubRegionProbers,
    exits: FuturesUnordered<BoxFuture<'static, SubRegionExit>>,
    next_instance: u64,

    /// The acknowledgments of messages whose parts went to sub-regions, for the one lane of a region without lanes,
    /// which hands its own part to itself through `loopback`.
    acknowledgments: FuturesUnordered<BoxFuture<'static, ()>>,
    loopback: Option<Loopback<D::Control>>,
}

impl<D> SubRegions<D>
where
    D: RegionDriver,
{
    /// Creates the sub-regions of a component, none running yet.
    pub(super) fn new(
        config: SubRegionConfig<D>, exit_names: Arc<[String]>, work_per_turn: NonZeroUsize,
        component: ComponentContext, registry: Arc<SubRegionRegistry>, phase: HostPhase,
    ) -> Self {
        Self {
            config,
            exit_names,
            work_per_turn,
            component,
            registry,
            phase,
            attached: None,
            live: BTreeMap::new(),
            tasks: BTreeMap::new(),
            probers: SubRegionProbers::default(),
            exits: FuturesUnordered::new(),
            next_instance: 0,
            acknowledgments: FuturesUnordered::new(),
            loopback: None,
        }
    }

    /// Gives the sub-regions what they share from the running component: its dispatcher, its resource group, below
    /// which each sub-region's own is, and its name.
    pub(super) fn attach(&mut self, dispatcher: Arc<EventsDispatcher>, group: SubsystemIdentifier, region: String) {
        self.attached = Some(Attachment {
            dispatcher,
            group,
            region,
        });
    }

    /// Returns the probers of the sub-region tasks, which follow the tasks as they start and stop.
    pub(super) fn probers(&self) -> SubRegionProbers {
        self.probers.clone()
    }

    /// Returns `true` while any sub-region task has not stopped.
    pub(super) fn has_tasks(&self) -> bool {
        !self.tasks.is_empty()
    }

    /// Waits until a sub-region task stops, or forever if none runs.
    ///
    /// # Errors
    ///
    /// If the sub-region failed, or its task ended without finishing, the error that fails the component is returned.
    pub(super) async fn next_exit(&mut self) -> Result<(), GenericError> {
        match self.exits.next().await {
            Some((instance, outcome)) => self.exited(instance, outcome),
            None => std::future::pending().await,
        }
    }

    /// Waits until a sub-region task stops, or the acknowledgment of a message that the one lane of a region without
    /// lanes routed completes; forever if neither can happen.
    ///
    /// # Errors
    ///
    /// As [`next_exit`][Self::next_exit].
    pub(super) async fn next_event(&mut self) -> Result<(), GenericError> {
        select! {
            Some((instance, outcome)) = self.exits.next(), if !self.exits.is_empty() => self.exited(instance, outcome),
            Some(()) = self.acknowledgments.next(), if !self.acknowledgments.is_empty() => Ok(()),
            else => std::future::pending().await,
        }
    }

    fn exited(
        &mut self, instance: u64, outcome: Result<Result<(), GenericError>, oneshot::error::RecvError>,
    ) -> Result<(), GenericError> {
        let Some(link) = self.tasks.remove(&instance) else {
            return Ok(());
        };
        self.probers.lock().retain(|(other, _)| *other != instance);
        self.registry.remove(link.key, instance);
        if self.live.get(&link.key).is_some_and(|live| live.instance == instance) {
            self.live.remove(&link.key);
        }
        let region = self.attached.as_ref().map_or("", |attached| attached.region.as_str());
        match outcome {
            Ok(Ok(())) => {
                debug!(sub_region = %link.name, key = %link.key, "Region sub-region stopped.");
                Ok(())
            }
            Ok(Err(e)) => Err(e.context(format!("Sub-region '{}' of region '{}' failed", link.name, region))),
            Err(_) => Err(generic_error!(
                "Sub-region '{}' of region '{}' stopped without finishing: its task panicked outside any supervised \
                 call, or was aborted. See its logs for the reason.",
                link.name,
                region
            )),
        }
    }

    /// Splits a control message, starts every sub-region it starts that does not run, and returns its parts, none sent
    /// yet.
    ///
    /// # Errors
    ///
    /// If the driver of a sub-region the message starts cannot be built, the error that rejects the message is
    /// returned, and nothing was started.
    pub(super) fn route(&mut self, message: D::Control) -> Result<Routed<D::Control>, GenericError> {
        // A sub-region runs while anything holds a sender to its input. Hold one for each running sub-region while the
        // message is routed and acknowledged, so that none retires meanwhile, and take one that nothing holds for
        // retiring: it takes no more messages, and a message that addresses its key starts a new sub-region.
        let mut held = Vec::with_capacity(self.live.len());
        let mut running = Vec::with_capacity(self.live.len());
        self.live.retain(|key, link| match link.input.upgrade() {
            Some(input) => {
                held.push(input);
                running.push(*key);
                true
            }
            None => {
                debug!(sub_region = %link.name, key = %key, "Region sub-region is retiring.");
                false
            }
        });
        let RegionParts { main, sub_regions } = (self.config.split)(message, &running);

        // Build every driver first, so that one that cannot be built rejects the message with nothing started.
        let mut starting: Vec<(&SubRegionSpec, D)> = Vec::new();
        for part in &sub_regions {
            if let SubRegionPart::Start(spec, message) = part {
                let known =
                    self.live.contains_key(&spec.key) || starting.iter().any(|(started, _)| started.key == spec.key);
                if !known {
                    starting.push((spec, self.build(spec, message)?));
                }
            }
        }
        let starting = starting
            .into_iter()
            .map(|(spec, driver)| (spec.clone(), driver))
            .collect::<Vec<_>>();
        for (spec, driver) in starting {
            held.push(self.spawn(spec, driver)?);
        }

        let mut parts = Vec::with_capacity(sub_regions.len());
        for part in sub_regions {
            let (key, message) = match part {
                SubRegionPart::Start(spec, message) => (spec.key, message),
                SubRegionPart::Running(key, message) => (key, message),
            };
            match self.live.get(&key) {
                Some(link) => parts.push((Arc::clone(link), message)),
                None => debug!(key = %key, "Dropped the part of a control message for a sub-region that does not run."),
            }
        }
        Ok(Routed { main, parts, held })
    }

    /// Routes a control message for the one lane of a region without lanes: sends each sub-region its part, and
    /// returns the lane's own part with the reply the lane answers it through, if it has one and `main_runs`.
    ///
    /// A message with parts for sub-regions is acknowledged once every part was, through
    /// [`next_event`][Self::next_event]; one without is the lane's alone, which answers it through its own reply.
    pub(super) fn route_one_lane(
        &mut self, message: D::Control, reply: ControlReply, main_runs: bool,
    ) -> Option<(D::Control, ControlReply)> {
        let Routed { main, parts, held } = match self.route(message) {
            Ok(routed) => routed,
            Err(e) => {
                warn!(error = %e, "Region could not start a sub-region.");
                reply.reject(e);
                return None;
            }
        };
        // A lane that finished has nothing left to apply its part to.
        let main = main.filter(|_| main_runs);
        if parts.is_empty() {
            drop(held);
            // A message for no region that runs is left unhandled, as by a region that stopped.
            return main.map(|main| (main, reply));
        }

        let mut acks = parts
            .into_iter()
            .map(|(link, part)| {
                let ack = link.send(part);
                (PartTarget::SubRegion(link), ack)
            })
            .collect::<Vec<_>>();
        let request = main.and_then(|main| {
            let (sender, receiver) = self.loopback.get_or_insert_with(|| {
                let (sender, mailbox) = control_mailbox();
                (sender, mailbox.into_receiver_for("region"))
            });
            acks.push((PartTarget::Main, sender.send(main)));
            receiver.recv().now_or_never().flatten()
        });
        self.acknowledgments.push(join_parts(acks, reply, held).boxed());
        request
    }

    /// Builds the driver of a sub-region from its spec and the part of the message that starts it.
    fn build(&self, spec: &SubRegionSpec, first: &D::Control) -> Result<D, GenericError> {
        let region = self.attached.as_ref().map_or("", |attached| attached.region.as_str());
        if internal_exit(self.exit_names.len(), spec.key).is_none() {
            return Err(generic_error!(
                "Sub-region '{}' has key {}, but region '{}' has {} exits, so its internal exit would be past the last \
                 exit an identifier names. Allocate sub-region keys below {}.",
                spec.name,
                spec.key,
                region,
                self.exit_names.len(),
                usize::from(u16::MAX) + 1 - self.exit_names.len()
            ));
        }
        match catch_unwind(AssertUnwindSafe(|| (self.config.factory)(spec, first))) {
            Ok(Ok(driver)) => Ok(driver),
            Ok(Err(e)) => Err(e.context(format!(
                "Failed to build the driver of sub-region '{}' of region '{}'",
                spec.name, region
            ))),
            Err(payload) => Err(generic_error!(
                "Building the driver of sub-region '{}' of region '{}' panicked: {}",
                spec.name,
                region,
                panic_message(payload.as_ref())
            )),
        }
    }

    /// Starts a sub-region task running `driver`, and returns a sender to its input.
    fn spawn(&mut self, spec: SubRegionSpec, driver: D) -> Result<mpsc::Sender<EventsBuffer>, GenericError> {
        let attached = self.attached.as_ref().ok_or_else(|| {
            generic_error!("A region started a sub-region before it ran; this is a bug in the region host.")
        })?;
        let instance = self.next_instance;
        self.next_instance += 1;
        let name = Arc::<str>::from(spec.name.as_str());

        let (input, input_rx) = mpsc::channel(spec.input_buffers.get());
        let (link, end) = lane_link(super::LaneContext::ONE);
        let lane = Lane::sub_region(
            driver,
            Arc::clone(&self.exit_names),
            self.work_per_turn,
            self.config.turn_budget,
            HostTelemetry::new(&self.component),
            InternalExits::new(Arc::clone(&self.registry)),
            self.phase.clone(),
        );
        let (outcome, exited) = oneshot::channel();
        let dispatcher = Arc::clone(&attached.dispatcher);
        let task = async move {
            let _ = outcome.send(lane.run_as_lane(end, LaneInput::Internal(input_rx), &dispatcher).await);
        };
        runtime::worker(SUB_REGION_TASK_NAME, task)
            .with_resource_group(attached.group.clone().child(&spec.name).to_string())
            .spawn_child_or_detached();
        self.exits.push(async move { (instance, exited.await) }.boxed());

        let weak = input.downgrade();
        self.registry
            .insert(spec.key, instance, Arc::clone(&name), weak.clone());
        self.probers.lock().push((instance, link.prober()));
        let link = Arc::new(SubRegionLink {
            key: spec.key,
            instance,
            name,
            link,
            input: weak,
        });
        self.live.insert(spec.key, Arc::clone(&link));
        self.tasks.insert(instance, link);
        debug!(sub_region = %spec.name, key = %spec.key, "Region started a sub-region.");
        Ok(input)
    }
}

impl<D> Drop for SubRegions<D>
where
    D: RegionDriver,
{
    fn drop(&mut self) {
        // The coordinator stops once every sub-region stopped, or because the component failed: a sub-region that is
        // still running then stops without finishing, whether it first sees its control queue close or its input end.
        self.phase.fail();
    }
}
