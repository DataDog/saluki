//! The installer core's resource paths, driven against the fake encode and delivery hosts as the ADP installer worker
//! drives the real ones, with the outcomes expected after each step written by hand, and a property test over random
//! event sequences.
//!
//! The world boots the core with the default program, whose outputs it binds to two delivery hosts, each with its
//! encode host in the frame: the main output, `dd_metrics_encode`, to `dd_out`'s primary recipient, each routing group
//! to its endpoints of `dd_out` while they are configured, and the failover output to `mrf_dd_out`'s recipient while
//! failover is on. What a configuration changes is which recipients the lowering declares, and so which of them each
//! output's lease resolves to: a routing group's endpoints, the failover recipient, each endpoint's protocol,
//! additional endpoints that only the native set names (as a logs dual-ship endpoint), and team D's recipient and
//! output, which a resources artifact declares and lowering declares from `accepted_resources` alone.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{Arc, Mutex},
};

use proptest::prelude::*;
use saluki_core::execution::{AdmissionPointId, AdmissionTime, GenerationId, GenerationRef, RetireNotifier};
use serde_json::{json, Value};

use super::{
    testing::{Creator, FakeDeliveryHost, FakeEncodeHost, FakeExit, FakeRecipientState},
    tests::{describe, node_mut},
    *,
};
use crate::{
    canonical::{PlanHash, ResolvedValues},
    channel::{ApplyState, ArtifactConfigId, ArtifactProduct},
    compile::{Frame, FrameHosts, LeaseTable, SlotId},
    compose::{Approval, ComposeError, Composed, Composer},
    ir::{
        HostBinding, HostId, NativeComponentId, OutputDecl, Plan, ReceivedArtifact, RecipientDecl, RecipientId,
        ResourceProvenance,
    },
    operators::OperatorRegistry,
    resources::{fingerprint, DeclaredAccepts, DeclaredOutput, DeclaredProtocol, DeclaredRecipient, ResourcesArtifact},
    testing::{default_program_catalog, default_program_slot_name, DEFAULT_PROGRAM_JSON},
    validate::{CatalogOutput, Validator},
};

/// The listener filterlist of the boot default program.
const BOOT_LISTENER: &str = "tenant.raw.blocked";

/// The additional endpoints a configuration can list for `dd_out`, which only its native set names.
const ENDPOINTS: [&str; 3] = ["a", "b", "c"];

fn dd_out() -> HostId {
    HostId::new("dd_out").unwrap()
}

fn mrf_dd_out() -> HostId {
    HostId::new("mrf_dd_out").unwrap()
}

fn recipient_id(id: &str) -> RecipientId {
    RecipientId::new(id).unwrap()
}

fn primary() -> RecipientId {
    recipient_id("dd_out/primary/0")
}

fn group_0() -> RecipientId {
    recipient_id("dd_out/additional_0/0")
}

fn group_1() -> [RecipientId; 2] {
    [
        recipient_id("dd_out/additional_1/0"),
        recipient_id("dd_out/additional_1/1"),
    ]
}

fn mrf_primary() -> RecipientId {
    recipient_id("mrf_dd_out/primary/0")
}

fn additional(name: &str) -> RecipientId {
    RecipientId::new(format!("dd_out/additional/0/https://{}.example.com", name)).unwrap()
}

fn team_d() -> RecipientId {
    recipient_id("dd_out/declared/team_d/https://team-d.example.com")
}

/// What native configuration declares, and whether the operator publishes team D's resources artifact.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Config {
    /// The additional endpoints of `dd_out` that only its native set names.
    endpoints: BTreeSet<&'static str>,

    /// The routing groups whose endpoints are configured.
    groups: BTreeSet<u8>,

    /// Whether the first routing group's endpoint accepts V3 series.
    v3: bool,

    /// Whether Multi-Region Failover is on.
    failover: bool,

    /// Whether the resources product delivers team D's artifact.
    declared: bool,

    /// The listener filterlist, which makes plans differ without changing their resources.
    listener: &'static str,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            endpoints: BTreeSet::new(),
            groups: BTreeSet::new(),
            v3: false,
            failover: false,
            declared: false,
            listener: BOOT_LISTENER,
        }
    }
}

fn config(endpoints: &[&'static str], groups: &[u8], failover: bool, declared: bool) -> Config {
    Config {
        endpoints: endpoints.iter().copied().collect(),
        groups: groups.iter().copied().collect(),
        failover,
        declared,
        ..Config::default()
    }
}

fn recipient_decl(id: &RecipientId, host: &str, v3: bool, provenance: Value) -> Value {
    let declared = provenance != json!("native");
    json!({
        "id": id, "host": host, "endpoint": format!("https://{}.example.com", id.as_str().replace('/', "-")),
        "credential": if declared { "credentials/team_d" } else { "native:api_key" },
        "classes": if declared { "metrics_only" } else { "all" },
        "protocol": {"v3_series": v3},
        "provenance": provenance,
    })
}

/// Lowers a configuration into the default program: its outputs bound as at boot, and its resources declared, team
/// D's recipient only while the resources artifact `declared` (revision and hash) is accepted.
fn lowered(config: &Config, declared: Option<(u64, PlanHash)>) -> Plan {
    let mut value = serde_json::from_str::<Value>(DEFAULT_PROGRAM_JSON).unwrap();
    node_mut(&mut value, "dsd_prefix_filter_blocklist")["params"]["predicate"]["intake_name"]["matches"]["values"]
        ["resolved"] = json!([config.listener]);
    let native = json!("native");
    let mut recipients = vec![recipient_decl(&primary(), "dd_out", false, native.clone())];
    if config.groups.contains(&0) {
        recipients.push(recipient_decl(&group_0(), "dd_out", config.v3, native.clone()));
    }
    if config.groups.contains(&1) {
        for id in group_1() {
            recipients.push(recipient_decl(&id, "dd_out", false, native.clone()));
        }
    }
    for endpoint in &config.endpoints {
        recipients.push(recipient_decl(&additional(endpoint), "dd_out", false, native.clone()));
    }
    if let Some((revision, hash)) = declared {
        let provenance = json!({"declared": {"revision": revision, "hash": hash}});
        recipients.push(recipient_decl(&team_d(), "dd_out", false, provenance.clone()));
        // The declared output delivers to team D's recipient: placement binds it to `dd_out`'s encode host, and no
        // default-program node emits to it.
        value["outputs"].as_array_mut().unwrap().push(json!({
            "id": "resources_team_d",
            "accepts": {"kinds": "all", "timestamps": "mixed"},
            "fingerprint": PlanHash::of_bytes(b"computed below"),
            "recipients": [team_d()],
            "binding": {
                "host": "dd_out", "selector": {"declared": [team_d()]}, "telemetry": "resources_team_d",
                "provenance": provenance,
            },
        }));
    }
    if config.failover {
        recipients.push(recipient_decl(&mrf_primary(), "mrf_dd_out", false, native));
    }
    let bindings = [
        ("dd_metrics_encode", "dd_out", json!({"role": "main"})),
        (
            "metrics_routing_encode_0",
            "dd_out",
            json!({"group": format!("sha256:{}", "3".repeat(64))}),
        ),
        (
            "metrics_routing_encode_1",
            "dd_out",
            json!({"group": format!("sha256:{}", "4".repeat(64))}),
        ),
        (
            "mrf_metrics_encode",
            "mrf_dd_out",
            json!({"role": "multi_region_failover"}),
        ),
    ];
    // Each bound output delivers to the recipients the lowering declares for it, as native lowering resolves them.
    let mut delivers_to = BTreeMap::from([("dd_metrics_encode", vec![primary()])]);
    delivers_to.insert(
        "metrics_routing_encode_0",
        config.groups.contains(&0).then(group_0).into_iter().collect(),
    );
    delivers_to.insert(
        "metrics_routing_encode_1",
        if config.groups.contains(&1) {
            group_1().to_vec()
        } else {
            Vec::new()
        },
    );
    delivers_to.insert(
        "mrf_metrics_encode",
        config.failover.then(mrf_primary).into_iter().collect(),
    );
    for output in value["outputs"].as_array_mut().unwrap() {
        if let Some((id, host, selector)) = bindings.iter().find(|(id, ..)| output["id"] == *id) {
            output["binding"] = json!({"host": host, "selector": selector, "telemetry": id, "provenance": "native"});
            output["recipients"] = json!(delivers_to[id]);
        }
    }
    value["resources"] = json!({
        "hosts": [{"host": "dd_out", "settings": {}}, {"host": "mrf_dd_out", "settings": {}}],
        "recipients": recipients,
    });
    let mut plan = Plan::from_value(value).unwrap();
    // A bound output's fingerprint is the version 2 digest of its binding, as validation requires.
    for output in &mut plan.outputs {
        if let Some(binding) = &output.binding {
            output.fingerprint = fingerprint(&binding.host, output.accepts, &binding.selector);
        }
    }
    plan
}

/// The frame's delivery hosts, each with its encode host.
fn frame_hosts() -> FrameHosts {
    FrameHosts::new()
        .with_host(dd_out(), NativeComponentId::new("dd_metrics_encode").unwrap())
        .with_host(mrf_dd_out(), NativeComponentId::new("mrf_metrics_encode").unwrap())
}

/// Validates a plan against the stage 3 catalog.
fn candidate(plan: Plan, origin: Origin) -> Candidate {
    let mut catalog = default_program_catalog();
    for output in &plan.outputs {
        catalog = catalog.with_output(
            output.id.clone(),
            CatalogOutput {
                accepts: output.accepts,
                fingerprint: output.fingerprint,
                recipients: output.recipients.clone(),
                legacy_fingerprint: None,
            },
        );
    }
    let report = Validator::new(&OperatorRegistry::native(), &catalog).validate(&plan);
    Candidate {
        plan: Arc::new(plan),
        report,
        origin,
    }
}

/// A composer for a world without policy artifacts from the channel: the core never calls it.
struct NoComposer;

impl Composer for NoComposer {
    fn compose_all(&self, _: &[(&ReceivedArtifact, &Approval)]) -> Result<Composed, ComposeError> {
        Err(ComposeError::Malformed {
            message: "the world composes no artifact".to_string(),
        })
    }
}

/// Returns the listener filterlist of a plan.
fn listener(plan: &Plan) -> String {
    plan.node(&"dsd_prefix_filter_blocklist".parse().unwrap())
        .and_then(|node| node.params.get("predicate"))
        .and_then(|predicate| predicate.pointer("/intake_name/matches/values/resolved/0"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Makes a candidate whose listener filterlist starts with `cut`, and differs from the active plan's, a cut.
fn classify(active: &Plan, candidate: &Plan) -> Option<(CutPlan, Vec<AdmissionPointId>)> {
    let (old, new) = (listener(active), listener(candidate));
    if old == new || !new.starts_with("cut") {
        return None;
    }
    let plan = CutPlan {
        owners: vec!["dsd_agg".parse().unwrap()],
        widths: vec![std::time::Duration::from_secs(10)],
        unknown_widths: Vec::new(),
    };
    Some((plan, vec![AdmissionPointId::new(0)]))
}

/// Bytes for every resource, so that every creation holds a credit.
#[derive(Debug)]
struct Bounds;

impl ResourceBounds for Bounds {
    fn lease(&self, _: &HostId, _: &OutputDecl) -> u64 {
        1_000
    }

    fn recipient(&self, _: &RecipientDecl, _: Option<&HostBinding>) -> u64 {
        10
    }

    fn host_queue(&self, _: &HostId, _: Option<&HostBinding>) -> u64 {
        100_000
    }
}

/// What a generation holds, as its preparation said: its plan and its lease table.
#[derive(Clone, Debug)]
struct Known {
    plan: Arc<Plan>,
    leases: Arc<LeaseTable>,
}

impl Known {
    /// Returns every lease the generation runs in a host, as the host addresses it.
    fn leases(&self) -> Vec<(HostId, LeaseRef)> {
        self.plan
            .outputs
            .iter()
            .filter_map(|output| {
                let binding = output.binding.as_ref()?;
                let key = LeaseKey::of(output, &self.plan.resources)?;
                let lease = self.leases.lease(&output.id)?;
                Some((binding.host.clone(), LeaseRef { lease, key }))
            })
            .collect()
    }

    /// Returns every recipient the generation uses, by host: those its leases name, and the declared ones.
    fn recipients(&self) -> BTreeSet<(HostId, RecipientId)> {
        let mut used = BTreeSet::new();
        for output in &self.plan.outputs {
            let Some(binding) = &output.binding else { continue };
            for id in &output.recipients {
                if self.plan.resources.recipient(id).is_some() {
                    used.insert((binding.host.clone(), id.clone()));
                }
            }
        }
        for recipient in &self.plan.resources.recipients {
            if matches!(recipient.provenance, ResourceProvenance::Declared(_)) {
                used.insert((recipient.host.clone(), recipient.id.clone()));
            }
        }
        used
    }
}

fn document() -> ResourcesArtifact {
    ResourcesArtifact {
        resources_version: 1,
        recipients: vec![DeclaredRecipient {
            name: "team_d".to_string(),
            host: dd_out(),
            endpoint: "https://team-d.example.com".to_string(),
            credential: "credentials/team_d".parse().unwrap(),
            protocol: DeclaredProtocol::default(),
            delivery: None,
        }],
        outputs: vec![DeclaredOutput {
            name: "team_d".to_string(),
            recipients: vec!["team_d".to_string()],
            accepts: DeclaredAccepts::Any,
            label: None,
        }],
    }
}

fn resources_hash(revision: u64) -> PlanHash {
    PlanHash::of_bytes(format!("resources revision {}", revision).as_bytes())
}

fn delivered(revision: u64, document: Result<ResourcesArtifact, String>) -> ResourcesEvent {
    ResourcesEvent::Delivered {
        config: ArtifactConfigId::new("resources"),
        version: revision,
        revision,
        artifact_hash: resources_hash(revision),
        document,
    }
}

/// The topology around the core: it performs the core's actions as the ADP installer worker does, against fake hosts
/// that answer later, when a step says so, and checks every rule the core must keep as it goes.
struct World {
    core: InstallerCore,
    retired: Arc<Mutex<Vec<GenerationId>>>,
    encode: BTreeMap<HostId, FakeEncodeHost>,
    delivery: BTreeMap<HostId, FakeDeliveryHost>,
    /// What the schedule holds: the generation committed last, and the cut's new generation while one is armed.
    schedule: Option<GenerationRef>,
    cut: Option<GenerationRef>,
    /// The generation whose regions have not acknowledged it yet.
    regions: Option<GenerationId>,
    host_acks: Vec<(GenerationId, HostId, PrepareOutcome)>,
    resource_acks: Vec<(ResourcesRevision, HostId, PrepareOutcome)>,
    /// The parts a delivery host holds unanswered while they wait for a dropped loop's exit, with what the part's
    /// encode host answered.
    waiting_parts: Vec<(Creator, HostId, Result<(), String>)>,
    lease_releases: Vec<(HostId, LeaseRef)>,
    exits: Vec<FakeExit>,
    retry_due: bool,
    timeout_due: Option<ResourcesRevision>,
    arming: Option<GenerationId>,
    cancelling: Option<GenerationId>,
    config: Config,
    lowered: Arc<Plan>,
    /// The revision the resources product delivered last, if it delivers one.
    resources_revision: u64,
    known: BTreeMap<GenerationId, Known>,
    /// The credits the caller holds, and every credit ever listed and returned.
    ledger: BTreeMap<CreditKey, u64>,
    listed: BTreeSet<CreditKey>,
    returned: BTreeSet<CreditKey>,
    /// Whether the budget refuses the next credit a preparation lists.
    refusing: bool,
    next_revision: u64,
    next_change: u64,
    /// The actions the latest step performed, which a worker restart performs again.
    last: Vec<Action>,
    replaying: bool,
    /// Every rule a performed action broke.
    errors: Vec<String>,
    /// Every action performed, described.
    log: Vec<String>,
}

impl World {
    fn new() -> Self {
        let retired = Arc::new(Mutex::new(Vec::new()));
        let notified = Arc::clone(&retired);
        let notifier = move |_: GenerationId| {
            let notified = Arc::clone(&notified);
            RetireNotifier::new(move |id| notified.lock().unwrap().push(id))
        };
        let config = Config::default();
        let plan = lowered(&config, None);
        let frame = Arc::new(
            Frame::compute_with_hosts(
                &plan,
                &OperatorRegistry::native(),
                &default_program_slot_name,
                &frame_hosts(),
            )
            .unwrap(),
        );
        let (mut core, boot) = InstallerCore::with_resource_bounds(
            frame,
            OperatorRegistry::native(),
            InstallerConfig::default(),
            Arc::new(Bounds),
            notifier,
            candidate(plan.clone(), Origin::DefaultProgram { config_change: 0 }),
        )
        .unwrap();
        core.classify_cuts_with(classify);
        let mut world = Self {
            core,
            retired,
            encode: [dd_out(), mrf_dd_out()]
                .into_iter()
                .map(|host| (host.clone(), FakeEncodeHost::new(host)))
                .collect(),
            delivery: [dd_out(), mrf_dd_out()]
                .into_iter()
                .map(|host| (host.clone(), FakeDeliveryHost::new(host)))
                .collect(),
            schedule: Some(boot.generation.clone()),
            cut: None,
            regions: None,
            host_acks: Vec::new(),
            resource_acks: Vec::new(),
            waiting_parts: Vec::new(),
            lease_releases: Vec::new(),
            exits: Vec::new(),
            retry_due: false,
            timeout_due: None,
            arming: None,
            cancelling: None,
            config,
            lowered: Arc::new(plan.clone()),
            resources_revision: 0,
            known: BTreeMap::new(),
            ledger: BTreeMap::new(),
            listed: BTreeSet::new(),
            returned: BTreeSet::new(),
            refusing: false,
            next_revision: 1,
            next_change: 1,
            last: Vec::new(),
            replaying: false,
            errors: Vec::new(),
            log: Vec::new(),
        };
        // The topology builds every host with what the boot generation needs, and each publishes the boot revision.
        let creator = Creator::Generation(boot.generation.id());
        for part in &boot.hosts {
            assert_eq!(world.encode_mut(&part.host).prepare(creator, part), Ok(()));
            assert_eq!(
                world.delivery_mut(&part.host).prepare(creator, part),
                Some(PrepareOutcome::Prepared)
            );
        }
        for publish in &boot.publish {
            assert_eq!(
                world.delivery_mut(&publish.host).publish(boot.revision, publish),
                Ok(true)
            );
        }
        for (credit, bytes) in &boot.admitted {
            world.ledger.insert(credit.clone(), *bytes);
            world.listed.insert(credit.clone());
        }
        world.known.insert(
            boot.generation.id(),
            Known {
                plan: Arc::new(plan),
                leases: boot.leases,
            },
        );
        world
    }

    fn encode_mut(&mut self, host: &HostId) -> &mut FakeEncodeHost {
        self.encode
            .entry(host.clone())
            .or_insert_with(|| FakeEncodeHost::new(host.clone()))
    }

    fn delivery_mut(&mut self, host: &HostId) -> &mut FakeDeliveryHost {
        self.delivery
            .entry(host.clone())
            .or_insert_with(|| FakeDeliveryHost::new(host.clone()))
    }

    /// Returns the recipients a fake delivery host holds of an identity.
    fn held(&self, host: &HostId, id: &RecipientId) -> Option<(RecipientRef, FakeRecipientState)> {
        self.delivery[host]
            .recipients()
            .into_iter()
            .find(|(recipient, _)| recipient.id == *id)
    }

    fn native_ids(&self, host: &HostId) -> BTreeSet<RecipientId> {
        self.delivery[host]
            .native_set()
            .iter()
            .map(|recipient| recipient.id.clone())
            .collect()
    }

    /// Returns the generations past their barrier whose tokens are alive: active, kept, a cut's, or draining.
    fn established(&self) -> Vec<GenerationId> {
        let status = self.core.status();
        status
            .generations
            .iter()
            .filter(|record| {
                status.live.contains(&record.id)
                    && matches!(
                        record.state,
                        GenerationState::Active | GenerationState::Prepared | GenerationState::Draining
                    )
            })
            .map(|record| record.id)
            .collect()
    }

    /// Lowers the configuration as ADP does: with team D's recipient only while the core accepts its artifact.
    fn lower(&mut self) -> Candidate {
        let declared = self
            .core
            .accepted_resources()
            .map(|_| (self.resources_revision, resources_hash(self.resources_revision)));
        let plan = lowered(&self.config, declared);
        self.lowered = Arc::new(plan.clone());
        let change = self.next_change;
        self.next_change += 1;
        candidate(plan, Origin::DefaultProgram { config_change: change })
    }

    /// Performs the core's actions in order, as the installer worker does, with every action they lead to.
    fn perform(&mut self, actions: Vec<Action>) {
        let mut queue = actions.into_iter().collect::<VecDeque<_>>();
        while let Some(action) = queue.pop_front() {
            self.log.push(describe(&action));
            if !self.replaying {
                self.last.push(action.clone());
            }
            let more = self.perform_one(action);
            queue.extend(more);
        }
    }

    fn perform_one(&mut self, action: Action) -> Vec<Action> {
        match action {
            Action::Prepare(prepare) => {
                let generation = prepare.generation.id();
                if let Some(refused) = self.acquire(&prepare.admitted) {
                    return self.core.prepare_acknowledged(
                        generation,
                        PrepareSite::Budget { resource: refused },
                        Err("the grant has no room".to_string()),
                    );
                }
                let preparing = self.core.status().preparing.map(|record| record.id);
                if let Some(plan) = self.core.preparing_plan().filter(|_| preparing == Some(generation)) {
                    let leases = prepare
                        .slots
                        .first()
                        .map(|slot| Arc::clone(&slot.leases))
                        .unwrap_or_default();
                    let plan = Arc::clone(plan);
                    self.known.insert(generation, Known { plan, leases });
                }
                let creator = Creator::Generation(generation);
                for part in &prepare.hosts {
                    let encoded = self.encode_mut(&part.host).prepare(creator, part);
                    match self.delivery_mut(&part.host).prepare(creator, part) {
                        Some(delivered) => self.answer(creator, part.host.clone(), encoded, delivered),
                        None => self.waiting_parts.push((creator, part.host.clone(), encoded)),
                    }
                }
                self.regions = Some(generation);
                Vec::new()
            }
            Action::Abort { generation, hosts, .. } => {
                for host in hosts {
                    self.delivery_mut(&host).abort(Creator::Generation(generation));
                    self.collect_answers(&host);
                }
                if self.regions == Some(generation) {
                    self.regions = None;
                }
                Vec::new()
            }
            Action::Commit { generation } => {
                self.check_not_withdrawn(generation.id());
                self.schedule = Some(generation);
                self.cut = None;
                Vec::new()
            }
            // A cut's generation started sending when its cut was armed, which was checked then.
            Action::FinalizeCut { generation } => {
                self.schedule = Some(generation);
                self.cut = None;
                Vec::new()
            }
            Action::RetryAfter(_) => {
                self.retry_due = true;
                Vec::new()
            }
            Action::InstallDefaultProgram => {
                let candidate = self.lower();
                self.core.on_default_program(candidate, &NoComposer).1
            }
            Action::Report(_) | Action::WatchCut { .. } => Vec::new(),
            Action::ArmCut(arm) => {
                self.check_not_withdrawn(arm.generation.id());
                self.arming = Some(arm.generation.id());
                Vec::new()
            }
            Action::CancelCut { generation, .. } => {
                self.cancelling = Some(generation);
                Vec::new()
            }
            Action::PrepareResources {
                revision,
                hosts,
                admitted,
                ..
            } => {
                let mut actions = Vec::new();
                let host_of = |credit: &CreditKey| match credit {
                    CreditKey::Recipient(host, _) | CreditKey::HostQueue(host) | CreditKey::Lease(host, ..) => {
                        Some(host.clone())
                    }
                    CreditKey::Instances(..) => None,
                };
                // A refusal fails only the part of the host whose credit it refused: every other part is sent, with its
                // credits acquired.
                let refused = self.acquire(&admitted).and_then(|credit| host_of(&credit));
                if let Some(refused) = &refused {
                    let others = admitted
                        .iter()
                        .filter(|(credit, _)| host_of(credit).as_ref() != Some(refused))
                        .cloned()
                        .collect::<Vec<_>>();
                    self.acquire(&others);
                }
                for part in hosts {
                    let refused_here = refused.as_ref() == Some(&part.host);
                    if refused_here {
                        actions.extend(self.core.resources_prepared(
                            revision,
                            part.host.clone(),
                            Err("the grant has no room".to_string()),
                        ));
                        continue;
                    }
                    let creator = Creator::Transaction(revision);
                    match self.delivery_mut(&part.host).prepare(creator, &part) {
                        Some(outcome) => self.answer(creator, part.host.clone(), Ok(()), outcome),
                        None => self.waiting_parts.push((creator, part.host.clone(), Ok(()))),
                    }
                }
                self.timeout_due = Some(revision);
                actions
            }
            Action::PublishResources { revision, hosts } => {
                for publish in hosts {
                    if let Err(error) = self.delivery_mut(&publish.host).publish(revision, &publish) {
                        self.errors.push(format!("publication {} refused: {}", revision, error));
                    }
                }
                Vec::new()
            }
            Action::AbortResources { revision, hosts } => {
                for host in hosts {
                    self.delivery_mut(&host).abort(Creator::Transaction(revision));
                    self.collect_answers(&host);
                }
                Vec::new()
            }
            Action::ReleaseLeases { host, leases } => {
                let established = self.established();
                for lease in leases {
                    for generation in &established {
                        let used = self
                            .known
                            .get(generation)
                            .is_some_and(|known| known.leases().contains(&(host.clone(), lease.clone())));
                        if used {
                            self.errors.push(format!(
                                "lease {} of `{}` released while generation {} uses it",
                                lease.lease.get(),
                                host,
                                generation
                            ));
                        }
                    }
                    self.encode_mut(&host).drain(&lease);
                    self.lease_releases.push((host.clone(), lease));
                }
                Vec::new()
            }
            Action::ReleaseRecipients { host, recipients } => {
                if !self.replaying {
                    let established = self.established();
                    for recipient in &recipients {
                        for generation in &established {
                            let used = self.known.get(generation).is_some_and(|known| {
                                known.recipients().contains(&(host.clone(), recipient.id.clone()))
                            });
                            if used {
                                self.errors.push(format!(
                                    "{} released while generation {} uses it",
                                    recipient, generation
                                ));
                            }
                        }
                        if self.delivery_mut(&host).native_set().contains(recipient) {
                            self.errors.push(format!(
                                "{} released while the published native set names it",
                                recipient
                            ));
                        }
                    }
                }
                self.delivery_mut(&host).release(&recipients);
                Vec::new()
            }
            Action::ReleaseCredits { credits } => {
                for credit in credits {
                    if !self.replaying {
                        if !self.listed.remove(&credit) {
                            self.errors
                                .push(format!("{} released, but not listed since its last release", credit));
                        }
                        self.returned.insert(credit.clone());
                    }
                    self.ledger.remove(&credit);
                }
                Vec::new()
            }
        }
    }

    /// Checks that a generation whose payloads its leases start sending uses no recipient its host withdraws while the
    /// latest lowering declares it: a publication restoring it comes first.
    fn check_not_withdrawn(&mut self, generation: GenerationId) {
        if self.replaying {
            return;
        }
        let Some(known) = self.known.get(&generation) else {
            return;
        };
        for (host, id) in known.recipients() {
            let withdrawn = self
                .delivery
                .get(&host)
                .is_some_and(|delivery| delivery.withdrawn().contains(&id));
            if withdrawn && self.lowered.resources.recipient(&id).is_some() {
                self.errors.push(format!(
                    "generation {} sends to `{}` of `{}`, which its host withdraws while the latest lowering declares \
                     it",
                    generation, id, host
                ));
            }
        }
    }

    /// Queues a host's answer to its part of a preparation, which the core is handed at the next acknowledgment step:
    /// the encode host's failure, or the delivery host's answer.
    fn answer(&mut self, creator: Creator, host: HostId, encoded: Result<(), String>, delivered: PrepareOutcome) {
        let outcome = match encoded {
            Err(reason) => PrepareOutcome::Failed(reason),
            Ok(()) => delivered,
        };
        match creator {
            Creator::Generation(generation) => self.host_acks.push((generation, host, outcome)),
            Creator::Transaction(revision) => self.resource_acks.push((revision, host, outcome)),
        }
    }

    /// Queues the answers a delivery host gave to parts that waited for a dropped loop's exit.
    fn collect_answers(&mut self, host: &HostId) {
        for (creator, delivered) in self.delivery_mut(host).take_answers() {
            let Some(position) = self
                .waiting_parts
                .iter()
                .position(|(by, waited_in, _)| *by == creator && waited_in == host)
            else {
                self.errors
                    .push(format!("`{}` answered a part of {} it was not holding", host, creator));
                continue;
            };
            let (_, _, encoded) = self.waiting_parts.remove(position);
            self.answer(creator, host.clone(), encoded, delivered);
        }
    }

    /// Acquires the credits a preparation lists that the caller does not hold yet, or returns the first one the budget
    /// refuses, acquiring nothing.
    fn acquire(&mut self, admitted: &[(CreditKey, u64)]) -> Option<CreditKey> {
        // The core lists every credit, whether or not the budget grants it, and releases it once per listing, which a
        // caller that does not hold it ignores. Only an idle host's queue is listed again after its release, when the
        // host has a recipient again.
        for (credit, _) in admitted {
            if !self.replaying && !self.listed.contains(credit) && self.returned.contains(credit) {
                if !matches!(credit, CreditKey::HostQueue(_)) {
                    self.errors
                        .push(format!("{} listed again after it was released", credit));
                }
                self.returned.remove(credit);
            }
            self.listed.insert(credit.clone());
        }
        let new = admitted
            .iter()
            .filter(|(credit, _)| !self.ledger.contains_key(credit))
            .cloned()
            .collect::<Vec<_>>();
        if self.refusing {
            if let Some((credit, _)) = new.first() {
                self.refusing = false;
                return Some(credit.clone());
            }
        }
        for (credit, bytes) in new {
            self.ledger.insert(credit, bytes);
        }
        None
    }

    /// Changes the configuration: the resources product delivers or revokes team D's artifact if that changed, and the
    /// configuration is lowered.
    fn configure(&mut self, config: Config) -> Vec<Action> {
        let declare = config.declared && !self.config.declared;
        let revoke = !config.declared && self.config.declared;
        self.config = config;
        let mut actions = Vec::new();
        if declare {
            self.resources_revision += 1;
            actions.extend(
                self.core
                    .on_resources(delivered(self.resources_revision, Ok(document()))),
            );
        } else if revoke {
            actions.extend(self.core.on_resources(ResourcesEvent::Revoked));
        }
        let candidate = self.lower();
        actions.extend(self.core.on_default_program(candidate, &NoComposer).1);
        actions
    }

    /// Proposes an artifact composed from the latest lowering, with `listener` as its listener filterlist.
    fn propose_artifact(&mut self, listener: &'static str) -> Vec<Action> {
        let revision = self.next_revision;
        self.next_revision += 1;
        let config = Config {
            listener,
            ..self.config.clone()
        };
        let declared = self
            .lowered
            .resources
            .recipient(&team_d())
            .map(|_| (self.resources_revision, resources_hash(self.resources_revision)));
        let plan = lowered(&config, declared);
        let hash = plan.hash(ResolvedValues::Include).unwrap();
        self.core
            .propose(candidate(plan, Origin::Artifact { revision, hash }))
            .1
    }

    fn acknowledge_regions(&mut self) -> Vec<Action> {
        let Some(generation) = self.regions.take() else {
            return Vec::new();
        };
        let mut actions = Vec::new();
        for slot in 0..self.core.frame().slots().len() as u16 {
            actions.extend(self.core.prepare_acknowledged(generation, SlotId::new(slot), Ok(())));
        }
        actions
    }

    fn acknowledge_hosts(&mut self) -> Vec<Action> {
        let mut actions = Vec::new();
        for (generation, host, outcome) in std::mem::take(&mut self.host_acks) {
            actions.extend(
                self.core
                    .prepare_acknowledged(generation, PrepareSite::Host(host), outcome),
            );
        }
        for (revision, host, outcome) in std::mem::take(&mut self.resource_acks) {
            actions.extend(self.core.resources_prepared(revision, host, outcome));
        }
        actions
    }

    fn deliver_lease_releases(&mut self) -> Vec<Action> {
        std::mem::take(&mut self.lease_releases)
            .into_iter()
            .flat_map(|(host, lease)| self.core.lease_released(host, lease))
            .collect()
    }

    /// Ends the drain of every draining recipient.
    fn end_drains(&mut self) {
        for delivery in self.delivery.values_mut() {
            let draining = delivery
                .recipients()
                .into_iter()
                .filter(|(_, state)| *state == FakeRecipientState::Draining)
                .map(|(recipient, _)| recipient)
                .collect::<Vec<_>>();
            for recipient in draining {
                delivery.retire(&recipient, RetirementReport::default());
            }
        }
    }

    fn deliver_exits(&mut self) -> Vec<Action> {
        for delivery in self.delivery.values_mut() {
            self.exits.extend(delivery.take_exits());
        }
        for host in self.delivery.keys().cloned().collect::<Vec<_>>() {
            self.collect_answers(&host);
        }
        let mut actions = Vec::new();
        for exit in std::mem::take(&mut self.exits) {
            actions.extend(match exit {
                FakeExit::Retired(recipient, report) => self.core.recipient_retired(recipient, report),
                FakeExit::Failed(recipient, reason) => self.core.recipient_failed(recipient, &reason),
            });
        }
        actions
    }

    fn deliver_retirements(&mut self) -> Vec<Action> {
        let retired = std::mem::take(&mut *self.retired.lock().unwrap());
        retired.into_iter().flat_map(|id| self.core.retired(id)).collect()
    }

    /// Runs one step, then checks every rule.
    fn step(&mut self, step: &Step) {
        if !matches!(step, Step::Restart) {
            self.last.clear();
        }
        let actions = match step {
            Step::Configure(config) => self.configure(config.clone()),
            Step::Artifact(listener) => self.propose_artifact(listener),
            Step::AcknowledgeRegions => self.acknowledge_regions(),
            Step::AcknowledgeHosts => self.acknowledge_hosts(),
            Step::FailNextPrepare(failover) => {
                if *failover {
                    self.delivery_mut(&mrf_dd_out())
                        .fail_next_prepare("the directory is unreadable");
                } else {
                    self.encode_mut(&dd_out())
                        .fail_next_prepare("the builders could not start");
                }
                Vec::new()
            }
            Step::TimeOut => match self.core.status().preparing {
                Some(record) => self.core.prepare_timed_out(record.id),
                None => Vec::new(),
            },
            Step::TimeOutResources => match self.timeout_due.take() {
                Some(revision) => self.core.resources_timed_out(revision),
                None => Vec::new(),
            },
            Step::Retry => {
                if std::mem::take(&mut self.retry_due) {
                    self.core.retry()
                } else {
                    Vec::new()
                }
            }
            Step::Retirements => self.deliver_retirements(),
            Step::LeaseReleases => self.deliver_lease_releases(),
            Step::EndDrains => {
                self.end_drains();
                Vec::new()
            }
            Step::FailRecipient(index) => {
                let live = self
                    .delivery
                    .values()
                    .flat_map(|delivery| {
                        delivery
                            .recipients()
                            .into_iter()
                            .filter(|(_, state)| *state == FakeRecipientState::Live)
                            .map(move |(recipient, _)| (delivery.host().clone(), recipient))
                    })
                    .collect::<Vec<_>>();
                if let Some((host, recipient)) = live.get(index % live.len().max(1)).cloned() {
                    self.delivery_mut(&host).fail(&recipient, "connection reset");
                }
                Vec::new()
            }
            Step::Exits => self.deliver_exits(),
            Step::Panic => match self.core.status().active {
                Some(record) => self.core.generation_failed(record.id, "3 panics in 60 seconds"),
                None => Vec::new(),
            },
            Step::Arm => match self.arming.take() {
                Some(generation) => {
                    self.cut = self
                        .core
                        .cut_in_flight()
                        .filter(|cut| cut.generation().id() == generation)
                        .map(|cut| cut.generation().clone());
                    self.core
                        .cut_armed(generation, AdmissionTime::from_unix_secs(1_700_000_100))
                }
                None => Vec::new(),
            },
            Step::Stamp { all } => match self.core.cut_in_flight().map(|cut| cut.generation().id()) {
                Some(generation) => {
                    let progress = CutProgress {
                        stamped_new: true,
                        passed: usize::from(*all),
                        affected: 1,
                    };
                    self.core.cut_progress(generation, progress)
                }
                None => Vec::new(),
            },
            Step::Cancel { refused } => match self.cancelling.take() {
                Some(generation) if *refused => self.core.cut_cancel_refused(generation),
                Some(generation) => {
                    self.cut = None;
                    self.core.cut_cancelled(generation)
                }
                None => Vec::new(),
            },
            Step::RefuseCredits => {
                self.refusing = true;
                Vec::new()
            }
            Step::Restart => {
                // A restarted worker performs the actions in progress again; hosts and the budget are idempotent.
                let last = self.last.clone();
                self.replaying = true;
                self.perform(last);
                self.replaying = false;
                Vec::new()
            }
        };
        self.perform(actions);
        self.check();
    }

    /// Checks the rules that hold after every step.
    fn check(&mut self) {
        // Every lease and recipient of a generation past its barrier stays with its host.
        for generation in self.established() {
            let Some(known) = self.known.get(&generation) else {
                continue;
            };
            for (host, lease) in known.leases() {
                if !self.encode[&host].leases().contains(&lease) {
                    self.errors.push(format!(
                        "generation {} uses lease {} of `{}`, which its host does not hold",
                        generation,
                        lease.lease.get(),
                        host
                    ));
                }
            }
            for (host, id) in known.recipients() {
                if self
                    .held(&host, &id)
                    .is_none_or(|(_, state)| state == FakeRecipientState::Draining)
                {
                    self.errors.push(format!(
                        "generation {} uses recipient `{}` of `{}`, which its host does not hold live",
                        generation, id, host
                    ));
                }
            }
        }

        // A resources revision is never rejected for a queue directory a loop still owns: the core prepares an
        // identity's next incarnation only once its previous one exited, and a host holds a part on a dropped loop's
        // directory until that loop exits.
        let resources = self
            .core
            .config_states()
            .into_iter()
            .find(|ack| ack.product == ArtifactProduct::Resources);
        if let Some(error) = resources
            .filter(|ack| ack.state == ApplyState::Rejected)
            .and_then(|ack| ack.error)
            .filter(|error| error.contains("owned by"))
        {
            self.errors.push(format!(
                "a resources revision was rejected for a directory race: {}",
                error
            ));
        }

        // Nothing kept to roll back to declares an output or recipient the latest lowering does not.
        let status = self.core.status();
        if let Some(kept) = status.last_known_good.as_ref() {
            if let Some(known) = self.known.get(&kept.id) {
                if !core::keeps(&self.lowered, &known.plan) {
                    self.errors.push(format!(
                        "generation {} is kept although the latest lowering does not declare all it uses",
                        kept.id
                    ));
                }
            }
        }
        if let Some(rollback) = self.core.rollback_candidate() {
            if !core::keeps(&self.lowered, &rollback.plan) {
                self.errors
                    .push("a cut's rollback plan uses what the latest lowering does not declare".to_string());
            }
        }

        // Once no transaction is in flight, every host withdraws exactly the recipients of its table the latest
        // lowering does not declare, and runs the withdrawn set the core published.
        let table = self.core.resource_table();
        if table.transaction_in_flight().is_none() {
            for host in self.delivery.keys() {
                let Some((_, publish)) = table.published(host) else {
                    continue;
                };
                let held = table
                    .recipients()
                    .into_iter()
                    .filter(|status| status.host == *host)
                    .map(|status| status.recipient.id)
                    .collect::<BTreeSet<_>>();
                let stale = publish
                    .withdrawn
                    .iter()
                    .filter(|id| !held.contains(*id))
                    .collect::<Vec<_>>();
                if !stale.is_empty() {
                    self.errors.push(format!(
                        "`{}` withdraws {:?}, which its table no longer holds",
                        host, stale
                    ));
                }
            }
            for status in table.recipients() {
                if self.lowered.resources.recipient(&status.recipient.id).is_none() {
                    let withdrawn = table
                        .published(&status.host)
                        .is_some_and(|(_, publish)| publish.withdrawn.contains(&status.recipient.id));
                    if !withdrawn {
                        self.errors.push(format!(
                            "{} is not withdrawn although the latest lowering does not declare it",
                            status.recipient
                        ));
                    }
                }
            }
            for (host, delivery) in &self.delivery {
                if let Some((revision, publish)) = table.published(host) {
                    if delivery.published() == Some(revision) && *delivery.withdrawn() != publish.withdrawn {
                        self.errors
                            .push(format!("`{}` runs another withdrawn set than the core published", host));
                    }
                }
            }
        }
    }

    /// Answers everything, ends every drain, and delivers every report, until nothing changes: what the world looks
    /// like once every host and every batch caught up.
    fn quiesce(&mut self) {
        self.refusing = false;
        for _ in 0..64 {
            let before = self.log.len();
            for step in [
                Step::Arm,
                Step::Stamp { all: true },
                Step::Cancel { refused: false },
                Step::AcknowledgeRegions,
                Step::AcknowledgeHosts,
                Step::LeaseReleases,
                Step::EndDrains,
                Step::Exits,
                Step::Retirements,
                Step::Retry,
            ] {
                self.step(&step);
            }
            // An answer a delivery host gave once a dropped loop's exit was taken waits for the next acknowledgment.
            let quiet = self.log.len() == before
                && self.host_acks.is_empty()
                && self.resource_acks.is_empty()
                && self.retired.lock().unwrap().is_empty()
                && self.delivery.values().all(|delivery| {
                    delivery
                        .recipients()
                        .iter()
                        .all(|(_, state)| *state != FakeRecipientState::Draining)
                });
            if quiet {
                break;
            }
        }
    }

    /// Checks that what is left once the world caught up is what something still uses: every lease is a live
    /// generation's, every recipient a live generation's or the native set's, and the caller holds exactly the credits
    /// the core has not released.
    fn check_reclaimed(&mut self) {
        let status = self.core.status();
        let live = status
            .live
            .iter()
            .filter_map(|generation| self.known.get(generation))
            .collect::<Vec<_>>();
        for (host, encode) in &self.encode {
            for lease in encode.leases() {
                if !live
                    .iter()
                    .any(|known| known.leases().contains(&(host.clone(), lease.clone())))
                {
                    self.errors.push(format!(
                        "lease {} of `{}` is held, and no live generation uses it",
                        lease.lease.get(),
                        host
                    ));
                }
            }
        }
        for (host, delivery) in &self.delivery {
            for (recipient, state) in delivery.recipients() {
                let used = live
                    .iter()
                    .any(|known| known.recipients().contains(&(host.clone(), recipient.id.clone())));
                let native = delivery.native_set().contains(&recipient);
                if !used && !native {
                    self.errors.push(format!(
                        "{} of `{}` is held ({:?}), and nothing uses it",
                        recipient, host, state
                    ));
                }
            }
        }
        let outstanding = self
            .core
            .resource_table()
            .credits()
            .map(|(credit, _)| credit.clone())
            .collect::<BTreeSet<_>>();
        let held = self.ledger.keys().cloned().collect::<BTreeSet<_>>();
        if outstanding != held {
            self.errors.push(format!(
                "the caller holds the credits {:?}, and the core has not released {:?}",
                held, outstanding
            ));
        }
    }
}

/// One thing that happens around the core.
#[derive(Clone, Debug)]
enum Step {
    /// The configuration changes, and is lowered.
    Configure(Config),
    /// An artifact with this listener filterlist is proposed; one whose filterlist starts with `cut` is a cut.
    Artifact(&'static str),
    AcknowledgeRegions,
    AcknowledgeHosts,
    /// The next preparation of `dd_out`'s encode host, or of `mrf_dd_out`'s delivery host, fails.
    FailNextPrepare(bool),
    TimeOut,
    TimeOutResources,
    Retry,
    Retirements,
    LeaseReleases,
    EndDrains,
    FailRecipient(usize),
    Exits,
    Panic,
    Arm,
    Stamp {
        all: bool,
    },
    Cancel {
        refused: bool,
    },
    RefuseCredits,
    Restart,
}

fn resources_ack(core: &InstallerCore) -> (ApplyState, Option<u64>, Option<String>) {
    let ack = core
        .config_states()
        .into_iter()
        .find(|ack| ack.product == ArtifactProduct::Resources)
        .unwrap();
    (ack.state, ack.revision, ack.error)
}

fn has_install(actions: &[Action]) -> bool {
    actions
        .iter()
        .any(|action| matches!(action, Action::InstallDefaultProgram))
}

fn assert_clean(world: &mut World) {
    world.quiesce();
    world.check_reclaimed();
    assert!(world.errors.is_empty(), "{:?}\n{:#?}", world.errors, world.log);
}

// Hand-written traces.

#[test]
fn the_boot_generation_builds_its_hosts_with_its_resources_and_publishes_the_first_revision() {
    let mut world = World::new();
    let table = world.core.resource_table();
    let recipients = table
        .recipients()
        .iter()
        .map(|status| (status.host.to_string(), status.recipient.to_string(), status.state))
        .collect::<Vec<_>>();
    assert_eq!(
        recipients,
        vec![(
            "dd_out".to_string(),
            "recipient `dd_out/primary/0` (slot 0, incarnation 1)".to_string(),
            RecipientState::Live
        )]
    );
    // `dd_out` runs the main output's and both routing groups' leases; the failover output's lease runs in its host's
    // encode host although it delivers to no recipient yet.
    assert_eq!(table.leases(&dd_out()).len(), 3);
    assert_eq!(table.leases(&mrf_dd_out()).len(), 1);
    let (revision, publish) = table.published(&dd_out()).unwrap();
    assert_eq!(revision, ResourcesRevision::new(1));
    assert_eq!(publish.native_set.len(), 1);
    assert_eq!(world.delivery[&dd_out()].published(), Some(revision));
    // The idle failover host published its settings, with an empty native set.
    assert_eq!(table.published(&mrf_dd_out()).unwrap().1.native_set, vec![]);
    // The boot generation's credits: its four leases, its recipient, and its kernel instances; no idle host's queue.
    let credits = table.credits().map(|(credit, _)| credit.clone()).collect::<Vec<_>>();
    assert!(credits.iter().any(|credit| matches!(credit, CreditKey::Instances(..))));
    let count = |kind: fn(&CreditKey) -> bool| credits.iter().filter(|credit| kind(credit)).count();
    assert_eq!(count(|credit| matches!(credit, CreditKey::Lease(..))), 4);
    assert_eq!(count(|credit| matches!(credit, CreditKey::Recipient(..))), 1);
    assert_eq!(count(|credit| matches!(credit, CreditKey::HostQueue(_))), 0);
    assert_eq!(world.ledger.len(), credits.len());
    assert_clean(&mut world);
}

#[test]
fn an_added_endpoint_is_published_by_a_transaction_and_the_new_lease_naming_it_creates_it_again() {
    let mut world = World::new();
    world.step(&Step::Configure(config(&[], &[0], false, false)));
    // First the transaction prepares the routing group's endpoint, then the generation prepares the group's new lease,
    // which creates the endpoint again under the transaction's reference, so that either one's abort keeps it.
    assert_eq!(world.log[0], "prepare resources revision 2 in 1 hosts within 10s");
    assert!(world.log[1].starts_with("prepare 2 ["), "{:?}", world.log);
    assert_eq!(world.host_acks.len(), 1);
    world.step(&Step::AcknowledgeHosts);
    assert_eq!(world.log.last().unwrap(), "publish resources revision 2 in 1 hosts");
    assert_eq!(
        world.native_ids(&dd_out()),
        BTreeSet::from([primary(), group_0()]),
        "payloads that name no recipients dual-ship at once"
    );
    world.step(&Step::AcknowledgeRegions);
    assert_eq!(world.core.status().active.unwrap().id, GenerationId::new(2));
    // The boot generation retires: the group's old lease drains; the others are reused.
    world.log.clear();
    world.step(&Step::Retirements);
    assert_eq!(
        world.log[..2],
        [
            "release 1 leases of dd_out".to_string(),
            "candidate 1 generation 1: retired".to_string()
        ]
    );
    assert_clean(&mut world);
}

#[test]
fn a_removed_endpoint_is_withdrawn_at_the_transaction_and_released_after_the_last_lease_naming_it_drains() {
    let mut world = World::new();
    world.step(&Step::Configure(config(&[], &[0], false, false)));
    world.quiesce();
    let (endpoint, _) = world.held(&dd_out(), &group_0()).unwrap();

    world.log.clear();
    world.step(&Step::Configure(config(&[], &[], false, false)));
    // The removal publishes at once, withdrawing the endpoint, while the generation without it prepares.
    assert_eq!(world.log[0], "publish resources revision 3 in 1 hosts");
    assert!(world.delivery[&dd_out()].withdrawn().contains(&group_0()));
    assert!(!world.delivery[&dd_out()].native_set().contains(&endpoint));
    assert_eq!(
        world.delivery[&dd_out()].state(&endpoint),
        Some(FakeRecipientState::Live)
    );

    // Two-phase: the generation whose lease names the endpoint retires, the lease drains, and only then is the
    // endpoint released.
    world.step(&Step::AcknowledgeHosts);
    world.step(&Step::AcknowledgeRegions);
    world.step(&Step::Retirements);
    assert_eq!(
        world.delivery[&dd_out()].state(&endpoint),
        Some(FakeRecipientState::Live)
    );
    world.step(&Step::LeaseReleases);
    assert_eq!(
        world.delivery[&dd_out()].state(&endpoint),
        Some(FakeRecipientState::Draining)
    );
    world.step(&Step::EndDrains);
    world.step(&Step::Exits);
    assert_eq!(world.delivery[&dd_out()].state(&endpoint), None);
    assert_clean(&mut world);
}

#[test]
fn a_recipient_its_transaction_failed_to_prepare_is_created_by_the_generation_that_needs_it_and_published_later() {
    let mut world = World::new();
    // The failover host fails its next preparation: the transaction's addition of the failover recipient.
    world.step(&Step::FailNextPrepare(true));
    world.step(&Step::Configure(config(&[], &[], true, false)));
    world.step(&Step::AcknowledgeHosts);
    let degraded = world.core.status_summary(None, None).degraded.unwrap();
    assert!(
        degraded.contains("recipient `mrf_dd_out/primary/0` could not be prepared, and is tried again"),
        "{}",
        degraded
    );
    assert!(world.native_ids(&mrf_dd_out()).is_empty());
    // The generation, whose failover lease names the recipient, created it again under the same reference, and its
    // host prepared it for the generation.
    assert_eq!(
        world.held(&mrf_dd_out(), &mrf_primary()).map(|(_, state)| state),
        Some(FakeRecipientState::Live)
    );
    world.step(&Step::AcknowledgeRegions);
    assert!(world.retry_due);
    world.step(&Step::Retry);
    world.step(&Step::AcknowledgeHosts);
    assert_eq!(world.native_ids(&mrf_dd_out()), BTreeSet::from([mrf_primary()]));
    assert!(world.core.status_summary(None, None).degraded.is_none());
    assert_clean(&mut world);
}

#[test]
fn a_generation_that_requires_a_recipient_its_host_no_longer_holds_live_fails_and_is_prepared_again() {
    let mut world = World::new();
    let (main, _) = world.held(&dd_out(), &primary()).unwrap();
    // The primary's loop fails; its exit is still in the host's event channel when a configuration change lowers.
    assert!(world.delivery_mut(&dd_out()).fail(&main, "connection reset"));
    world.step(&Step::Configure(Config {
        listener: "x",
        ..Config::default()
    }));
    world.step(&Step::AcknowledgeHosts);
    let reason = world
        .core
        .status()
        .generations
        .iter()
        .find(|record| record.id == GenerationId::new(2))
        .and_then(|record| record.reason.clone())
        .unwrap();
    assert!(reason.contains("is not live"), "{}", reason);
    assert!(world.retry_due);
    // The exit arrives: the primary is failed, and is no longer required; its next incarnation takes its slot.
    world.step(&Step::Exits);
    world.step(&Step::Retry);
    assert_clean(&mut world);
    let (next, state) = world.held(&dd_out(), &primary()).unwrap();
    assert_eq!((next.slot, state), (main.slot, FakeRecipientState::Live));
    assert!(next.incarnation > main.incarnation);
    assert_eq!(listener(world.core.active_plan().unwrap()), "x");
}

#[test]
fn a_newer_lowering_supersedes_a_transaction_that_has_not_published() {
    let mut world = World::new();
    world.step(&Step::Configure(config(&["a", "b"], &[], false, false)));
    world.log.clear();
    world.step(&Step::Configure(config(&["b"], &[], false, false)));
    // The newer transaction prepares `b` again under the older one's reference, then the older one is aborted.
    assert_eq!(world.log[0], "prepare resources revision 3 in 1 hosts within 10s");
    assert_eq!(world.log[1], "abort resources revision 2 in 1 hosts");
    world.step(&Step::AcknowledgeHosts);
    assert_eq!(
        world.native_ids(&dd_out()),
        BTreeSet::from([primary(), additional("b")])
    );
    assert_eq!(world.held(&dd_out(), &additional("a")), None);
    assert_clean(&mut world);
}

#[test]
fn an_unanswered_host_is_aborted_at_the_timeout_and_the_transaction_is_tried_again() {
    let mut world = World::new();
    world.step(&Step::Configure(config(&["a"], &[], false, false)));
    world.resource_acks.clear();
    world.log.clear();
    world.step(&Step::TimeOutResources);
    assert_eq!(
        world.log[..2],
        [
            "abort resources revision 2 in 1 hosts".to_string(),
            "publish resources revision 2 in 1 hosts".to_string()
        ]
    );
    let degraded = world.core.status_summary(None, None).degraded.unwrap();
    assert!(degraded.contains("did not acknowledge"), "{}", degraded);
    assert_clean(&mut world);
    assert!(world.core.status_summary(None, None).degraded.is_none());
    assert_eq!(
        world.native_ids(&dd_out()),
        BTreeSet::from([primary(), additional("a")])
    );
}

#[test]
fn a_reopen_that_finds_the_loop_exited_prepares_the_generation_again_once_the_exit_is_reported() {
    let mut world = World::new();
    world.step(&Step::Configure(config(&[], &[], false, true)));
    world.quiesce();
    // Team D's recipient is released once a configuration without it commits, and the lease of its output, which
    // names it, drained.
    world.step(&Step::Configure(config(&[], &[], false, false)));
    world.step(&Step::AcknowledgeHosts);
    world.step(&Step::AcknowledgeRegions);
    world.step(&Step::Retirements);
    world.step(&Step::LeaseReleases);
    let (d, state) = world.held(&dd_out(), &team_d()).unwrap();
    assert_eq!(state, FakeRecipientState::Draining);
    // Its drain ends as it is declared again: the reopen finds its loop exited, and the generation waits for the exit.
    world.step(&Step::EndDrains);
    world.step(&Step::Configure(config(&[], &[], false, true)));
    world.step(&Step::AcknowledgeHosts);
    assert_eq!(world.core.status().preparing, None);
    world.step(&Step::Exits);
    let preparing = world.core.status().preparing.expect("prepared again after the exit");
    assert_clean(&mut world);
    let (next, state) = world.held(&dd_out(), &team_d()).unwrap();
    assert_eq!(state, FakeRecipientState::Live);
    assert!(next.incarnation > d.incarnation, "its next incarnation");
    assert_eq!(world.core.status().active.unwrap().id, preparing.id);
}

#[test]
fn a_failed_recipient_is_replaced_in_its_slot_after_its_backoff_without_a_plan_change() {
    let mut world = World::new();
    world.step(&Step::Configure(config(&["a"], &[], false, false)));
    world.quiesce();
    let (a, _) = world.held(&dd_out(), &additional("a")).unwrap();
    assert!(world.delivery_mut(&dd_out()).fail(&a, "connection reset"));
    world.step(&Step::Exits);
    assert!(world.retry_due, "the next incarnation waits for its backoff");
    world.log.clear();
    world.step(&Step::Retry);
    assert!(
        world.log[0].starts_with("prepare resources revision"),
        "{:?}",
        world.log
    );
    assert_clean(&mut world);
    let (next, state) = world.held(&dd_out(), &additional("a")).unwrap();
    assert_eq!((next.slot, state), (a.slot, FakeRecipientState::Live));
    assert!(next.incarnation > a.incarnation);
    assert!(world.delivery[&dd_out()].native_set().contains(&next));
}

#[test]
fn a_lowering_that_removes_what_the_kept_generation_uses_releases_it() {
    let mut world = World::new();
    world.step(&Step::Configure(config(&[], &[0], false, false)));
    world.quiesce();
    world.step(&Step::Artifact("policy-a"));
    world.quiesce();
    let kept = world
        .core
        .status()
        .last_known_good
        .expect("the default program is kept")
        .id;
    // Adding a routing group's endpoints keeps it: everything it uses is still declared.
    world.step(&Step::Configure(config(&[], &[0, 1], false, false)));
    assert_eq!(world.core.status().last_known_good.map(|record| record.id), Some(kept));
    // Removing the endpoint its group's lease names releases it at once, before anything is prepared.
    world.step(&Step::Configure(config(&[], &[1], false, false)));
    assert_eq!(world.core.status().last_known_good, None);
    let state = world
        .core
        .status()
        .generations
        .iter()
        .find(|record| record.id == kept)
        .unwrap()
        .state;
    assert_eq!(state, GenerationState::Draining);
    assert_clean(&mut world);
}

#[test]
fn a_lowering_that_flips_a_recipients_protocol_releases_the_kept_generation_and_rebinds_the_lease() {
    let mut world = World::new();
    world.step(&Step::Configure(config(&[], &[0], false, false)));
    world.quiesce();
    world.step(&Step::Artifact("policy-a"));
    world.quiesce();
    assert!(world.core.status().last_known_good.is_some());
    let before = world.core.resource_table().leases(&dd_out());
    world.step(&Step::Configure(Config {
        v3: true,
        ..config(&[], &[0], false, false)
    }));
    assert_eq!(world.core.status().last_known_good, None);
    assert_clean(&mut world);
    let after = world.core.resource_table().leases(&dd_out());
    assert_eq!(after.len(), 3);
    assert_ne!(before, after, "the group's lease is replaced");
}

#[test]
fn a_budget_refusal_of_a_declared_recipient_rejects_its_resources_revision_and_lowers_without_it() {
    let mut world = World::new();
    world.step(&Step::RefuseCredits);
    world.step(&Step::Configure(config(&[], &[], false, true)));
    // The refused credit is team D's recipient's: its revision is rejected, the host lowers without it, and the
    // candidate that needed it is not rejected but tried again, as a native resource's would be.
    let (state, revision, error) = resources_ack(&world.core);
    assert_eq!((state, revision), (ApplyState::Rejected, Some(1)));
    assert!(error.unwrap().contains("the resource budget refused recipient"));
    assert_eq!(world.core.accepted_resources(), None);
    assert!(world.lowered.resources.recipient(&team_d()).is_none());
    assert_clean(&mut world);
    assert!(world.held(&dd_out(), &team_d()).is_none());
}

#[test]
fn a_declared_recipient_that_fails_to_prepare_rejects_its_revision_and_native_changes_keep_applying() {
    let mut world = World::new();
    world
        .delivery_mut(&dd_out())
        .fail_next_prepare(format!("{} duplicates recipient `dd_out/primary/0`", team_d()));
    world.step(&Step::Configure(config(&[], &[], false, true)));
    world.step(&Step::AcknowledgeHosts);
    let (state, _, error) = resources_ack(&world.core);
    assert_eq!(state, ApplyState::Rejected);
    assert!(error.unwrap().contains("duplicates recipient"));
    assert_eq!(world.core.accepted_resources(), None);
    assert!(world.log.iter().any(|action| action == "install the default program"));
    // Native configuration changes keep applying.
    world.step(&Step::Configure(config(&["a"], &[], false, true)));
    assert_clean(&mut world);
    assert_eq!(
        world.native_ids(&dd_out()),
        BTreeSet::from([primary(), additional("a")])
    );
}

#[test]
fn a_resources_revision_is_active_once_a_generation_lowered_with_it_commits_and_retires_after_a_revocation() {
    let mut world = World::new();
    world.step(&Step::Configure(config(&[], &[], false, true)));
    assert_eq!(resources_ack(&world.core).0, ApplyState::Pending);
    world.quiesce();
    assert_eq!(resources_ack(&world.core), (ApplyState::Active, Some(1), None));
    let active = world.core.status_summary(None, None).resources.unwrap().active_artifact;
    assert_eq!(active.map(|artifact| artifact.revision), Some(1));
    // Revoked: lowered without it, and retired once the generation lowered with it retired.
    world.step(&Step::Configure(config(&[], &[], false, false)));
    assert_eq!(resources_ack(&world.core).0, ApplyState::Active);
    assert_clean(&mut world);
    assert_eq!(resources_ack(&world.core).0, ApplyState::Retired);
    assert_eq!(
        world.core.status_summary(None, None).resources.unwrap().active_artifact,
        None
    );
}

#[test]
fn resources_revisions_follow_the_policy_products_rules_on_their_own_high_water_mark() {
    let mut world = World::new();
    let core = &mut world.core;
    assert_eq!(core.status_summary(None, None).resources, None);
    // Accepted: lowering declares from it.
    assert!(has_install(&core.on_resources(delivered(5, Ok(document())))));
    assert_eq!(core.accepted_resources(), Some(&document()));
    assert_eq!(resources_ack(core), (ApplyState::Pending, Some(5), None));
    // A redelivery changes nothing; a stale revision is rejected and changes nothing.
    assert!(core.on_resources(delivered(5, Ok(document()))).is_empty());
    assert!(!has_install(&core.on_resources(delivered(4, Ok(document())))));
    let (state, revision, error) = resources_ack(core);
    assert_eq!((state, revision), (ApplyState::Rejected, Some(4)));
    assert!(error.unwrap().starts_with("revision 4 is stale"));
    assert_eq!(core.accepted_resources(), Some(&document()));
    // A newer invalid document is rejected: its revision is seen, and the accepted one stays in place.
    core.on_resources(delivered(6, Err("endpoint `ftp://x` is not http".to_string())));
    let expected = (
        ApplyState::Rejected,
        Some(6),
        Some("endpoint `ftp://x` is not http".to_string()),
    );
    assert_eq!(resources_ack(core), expected);
    assert_eq!(core.accepted_resources(), Some(&document()));
    let mark = |core: &InstallerCore| core.status_summary(None, None).resources.unwrap().revision_high_water;
    assert_eq!(mark(core), Some(6));
    // A refused config's revision is not seen.
    core.on_resources(ResourcesEvent::Refused {
        config: ArtifactConfigId::new("resources"),
        version: 9,
        reason: "two configs in one snapshot".to_string(),
    });
    assert_eq!(mark(core), Some(6));
    // The same document under a newer revision is approved again: what lowering sees is another revision.
    assert!(has_install(&core.on_resources(delivered(7, Ok(document())))));
    // Revocation removes it.
    assert!(has_install(&core.on_resources(ResourcesEvent::Revoked)));
    assert_eq!(core.accepted_resources(), None);
}

#[test]
fn a_pending_resources_revision_leaves_the_one_in_use_in_place_until_its_credential_is_configured() {
    let mut world = World::new();
    let core = &mut world.core;
    let other = |credential: &str| {
        let mut document = document();
        document.recipients[0].credential = credential.parse().unwrap();
        document
    };
    let revision = |revision: u64| crate::ir::ArtifactRevision {
        revision,
        hash: resources_hash(revision),
    };
    core.on_resources(delivered(1, Ok(document())));
    assert_eq!(core.accepted_revision(), Some(revision(1)));
    assert_eq!(core.held_resources(), vec![(&document(), revision(1))]);
    // Revision 2 references a credential the configuration does not provide: revision 1 stays in use, and both are
    // held, the newest first, so that the host can tell which credentials either still needs.
    core.credentials_changed(BTreeSet::from(["credentials/team_e".parse().unwrap()]));
    let actions = core.on_resources(delivered(2, Ok(other("credentials/team_e"))));
    assert!(!has_install(&actions), "what lowering sees did not change");
    assert_eq!(core.accepted_resources(), Some(&document()));
    assert_eq!(core.accepted_revision(), Some(revision(1)));
    assert_eq!(
        core.held_resources(),
        vec![(&other("credentials/team_e"), revision(2)), (&document(), revision(1))]
    );
    let (state, _, error) = resources_ack(core);
    assert_eq!(state, ApplyState::Pending);
    assert!(error
        .unwrap()
        .starts_with("waiting for credential `credentials/team_e`"));
    // Once the credential is configured, revision 2 is in use, and the only one held.
    core.credentials_changed(BTreeSet::new());
    assert_eq!(core.accepted_resources(), Some(&other("credentials/team_e")));
    assert_eq!(core.accepted_revision(), Some(revision(2)));
    assert_eq!(core.held_resources(), vec![(&other("credentials/team_e"), revision(2))]);
    assert_eq!(resources_ack(core), (ApplyState::Pending, Some(2), None));
    // The revision in use becoming pending itself removes the declared resources, though it is still held.
    core.credentials_changed(BTreeSet::from(["credentials/team_e".parse().unwrap()]));
    assert_eq!(core.accepted_resources(), None);
    assert_eq!(core.accepted_revision(), None);
    assert_eq!(core.held_resources().len(), 1);
}

// Races the property test below found, each pinned by hand.

#[test]
fn a_recipient_whose_transaction_the_budget_refused_holds_its_credit_through_the_generation_that_keeps_it() {
    let mut world = World::new();
    world.step(&Step::RefuseCredits);
    world.step(&Step::Configure(config(&[], &[], true, false)));
    // The transaction's part for the failover host was refused and never sent; the generation that creates the
    // failover recipient again under the same reference lists the recipient's credits, and the idle host's queue, so
    // the caller holds them while the generation keeps the recipient.
    let held = world.ledger.keys().cloned().collect::<BTreeSet<_>>();
    assert!(held.contains(&CreditKey::HostQueue(mrf_dd_out())), "{:?}", held);
    let recipient_credit = |credit: &CreditKey| match credit {
        CreditKey::Recipient(host, recipient) => *host == mrf_dd_out() && recipient.id == mrf_primary(),
        _ => false,
    };
    assert!(held.iter().any(recipient_credit), "{:?}", held);
    assert_clean(&mut world);
    assert_eq!(world.native_ids(&mrf_dd_out()), BTreeSet::from([mrf_primary()]));
}

#[test]
fn a_part_a_worker_restart_sends_again_after_the_core_counted_it_failed_is_aborted_again() {
    let mut world = World::new();
    world.step(&Step::Configure(config(&["c"], &[], false, false)));
    world.step(&Step::TimeOutResources);
    // The dropped loop exits, freeing its directory. The retried transaction's part is refused by the budget, then
    // sent anyway by a worker restart that repeats the action.
    world.step(&Step::Exits);
    world.step(&Step::RefuseCredits);
    world.step(&Step::Retry);
    let revision = world.core.resource_table().revision();
    world.step(&Step::Restart);
    world.log.clear();
    world.step(&Step::AcknowledgeHosts);
    let abort = format!("abort resources revision {} in 1 hosts", revision.get());
    assert!(world.log.contains(&abort), "{:?}", world.log);
    assert_clean(&mut world);
}

#[test]
fn a_released_recipient_that_fails_while_reopens_are_in_flight_is_gone_once_they_answer_exited() {
    let mut world = World::new();
    world.step(&Step::Configure(config(&[], &[], true, false)));
    world.quiesce();
    let (failed, _) = world.held(&mrf_dd_out(), &mrf_primary()).unwrap();
    // The failover recipient's loop fails; its exit is still on the way when failover is turned off, and the core
    // releases it once its lease drained.
    assert!(world.delivery_mut(&mrf_dd_out()).fail(&failed, "connection reset"));
    world.step(&Step::Configure(config(&[], &[], false, false)));
    world.step(&Step::AcknowledgeHosts);
    world.step(&Step::AcknowledgeRegions);
    world.step(&Step::Retirements);
    world.step(&Step::LeaseReleases);
    assert_eq!(
        world.held(&mrf_dd_out(), &mrf_primary()),
        None,
        "a failed loop's release frees its slot"
    );
    // Failover is turned on again: the transaction and the generation reopen the recipient, and its host answers both
    // that its loop exited. The failure's exit arrives between the reopens and their answers.
    world.step(&Step::Configure(config(&[], &[], true, false)));
    world.step(&Step::Exits);
    world.step(&Step::AcknowledgeHosts);
    assert_clean(&mut world);
    let (next, state) = world.held(&mrf_dd_out(), &mrf_primary()).unwrap();
    assert_eq!(state, FakeRecipientState::Live);
    assert!(next.incarnation > failed.incarnation);
    assert!(world.delivery[&mrf_dd_out()].native_set().contains(&next));
}

// Races a review found, each pinned by hand.

#[test]
fn a_failed_recipient_whose_creator_commits_while_its_next_incarnation_is_prepared_is_released_once_unused() {
    let mut world = World::new();
    world.step(&Step::Configure(config(&[], &[], false, true)));
    // The generation that creates team D's recipient has its host part acknowledged; its regions have not answered.
    world.step(&Step::AcknowledgeHosts);
    let (d, _) = world.held(&dd_out(), &team_d()).unwrap();
    assert!(world.delivery_mut(&dd_out()).fail(&d, "connection reset"));
    world.step(&Step::Exits);
    // The backoff passes: a transaction prepares the next incarnation into the slot, and the generation commits while
    // that transaction is in flight, which commits the failed incarnation it created.
    world.step(&Step::Retry);
    assert!(world.core.resource_table().transaction_in_flight().is_some());
    world.step(&Step::AcknowledgeRegions);
    // The resources product is revoked: the transaction is superseded, and the failed incarnation, which nothing uses
    // once the generation retires, is released.
    world.step(&Step::Configure(config(&[], &[], false, false)));
    assert_clean(&mut world);
    assert_eq!(world.held(&dd_out(), &team_d()), None);
    // Published again, team D's recipient is created anew, and the generation that declares it installs.
    world.step(&Step::Configure(config(&[], &[], false, true)));
    assert_clean(&mut world);
    let active = world.core.active_plan().unwrap();
    assert!(active.resources.recipient(&team_d()).is_some());
    assert_eq!(
        world.held(&dd_out(), &team_d()).map(|(_, state)| state),
        Some(FakeRecipientState::Live)
    );
}

#[test]
fn a_host_failure_naming_a_declared_recipient_the_generation_does_not_create_is_retried_and_rejects_nothing() {
    let mut world = World::new();
    world.step(&Step::Configure(config(&[], &[], false, true)));
    world.quiesce();
    assert_eq!(resources_ack(&world.core).0, ApplyState::Active);
    let (d, _) = world.held(&dd_out(), &team_d()).unwrap();
    // A configuration change: the next generation uses team D's recipient, which it neither creates nor reopens. Its
    // host answers that the recipient's loop failed before the exit reached the core, naming it, as a host does for a
    // recipient a part requires.
    world.step(&Step::Configure(Config {
        listener: "x",
        ..config(&[], &[], false, true)
    }));
    world.host_acks.clear();
    let generation = world.core.status().preparing.unwrap().id;
    let message = format!(
        "Recipient {} is required by generation {}, but its loop failed. Prepare it again first.",
        d,
        generation.get()
    );
    let actions = world
        .core
        .prepare_acknowledged(generation, PrepareSite::Host(dd_out()), Err(message));
    world.perform(actions);
    // The revision stays accepted and active, and the candidate is tried again, as for a native recipient.
    assert_eq!(resources_ack(&world.core), (ApplyState::Active, Some(1), None));
    assert!(world.core.accepted_resources().is_some());
    assert!(world.retry_due);
    assert_clean(&mut world);
    assert_eq!(listener(world.core.active_plan().unwrap()), "x");
    assert!(world
        .core
        .active_plan()
        .unwrap()
        .resources
        .recipient(&team_d())
        .is_some());
}

#[test]
fn a_host_failure_of_a_declared_recipients_next_incarnation_that_a_generation_creates_again_rejects_nothing() {
    let mut world = World::new();
    world.step(&Step::Configure(config(&[], &[], false, true)));
    world.quiesce();
    let (d, _) = world.held(&dd_out(), &team_d()).unwrap();
    assert!(world.delivery_mut(&dd_out()).fail(&d, "connection reset"));
    world.step(&Step::Exits);
    // After its backoff, a transaction prepares the next incarnation; a configuration change's generation creates it
    // again under the transaction's reference, and its host fails that part, naming the recipient.
    world.step(&Step::Retry);
    let (next, _) = world.held(&dd_out(), &team_d()).unwrap();
    assert!(next.incarnation > d.incarnation);
    world
        .delivery_mut(&dd_out())
        .fail_next_prepare(format!("{} was dropped: prepare its next incarnation", next));
    world.step(&Step::Configure(Config {
        listener: "x",
        ..config(&[], &[], false, true)
    }));
    world.step(&Step::AcknowledgeHosts);
    assert_eq!(resources_ack(&world.core).0, ApplyState::Active);
    assert!(world.core.accepted_resources().is_some());
    assert_clean(&mut world);
    assert_eq!(listener(world.core.active_plan().unwrap()), "x");
    assert!(world
        .core
        .active_plan()
        .unwrap()
        .resources
        .recipient(&team_d())
        .is_some());
}

#[test]
fn a_resources_revision_whose_lowering_changes_no_plan_is_active_at_once_and_retires_when_revoked() {
    let mut world = World::new();
    let empty = ResourcesArtifact {
        resources_version: 1,
        recipients: vec![],
        outputs: vec![],
    };
    assert!(has_install(&world.core.on_resources(delivered(1, Ok(empty)))));
    // The document declares nothing, so the lowering equals what runs: the active generation installs the revision.
    let plan = lowered(&Config::default(), None);
    let (_, actions) = world.core.on_default_program(
        candidate(plan, Origin::DefaultProgram { config_change: 1 }),
        &NoComposer,
    );
    world.perform(actions);
    assert_eq!(resources_ack(&world.core), (ApplyState::Active, Some(1), None));
    let active = world.core.status_summary(None, None).resources.unwrap().active_artifact;
    assert_eq!(active.map(|artifact| artifact.revision), Some(1));
    // Revoked, the lowering is the same again, and the revision is retired.
    assert!(has_install(&world.core.on_resources(ResourcesEvent::Revoked)));
    let plan = lowered(&Config::default(), None);
    let (_, actions) = world.core.on_default_program(
        candidate(plan, Origin::DefaultProgram { config_change: 2 }),
        &NoComposer,
    );
    world.perform(actions);
    assert_eq!(resources_ack(&world.core).0, ApplyState::Retired);
    assert_eq!(
        world.core.status_summary(None, None).resources.unwrap().active_artifact,
        None
    );
    assert_clean(&mut world);
}

#[test]
fn a_failed_recipient_waits_its_own_backoff_and_not_the_longer_one_of_a_candidate_that_keeps_failing() {
    let mut world = World::new();
    world.step(&Step::Configure(config(&["a"], &[], false, false)));
    world.quiesce();
    // A configuration change whose preparation keeps failing: its candidate waits 1 s, then 2 s, then 4 s.
    world.step(&Step::FailNextPrepare(false));
    world.step(&Step::Configure(Config {
        listener: "x",
        ..config(&["a"], &[], false, false)
    }));
    for delay in ["1s", "2s", "4s"] {
        world.log.clear();
        world.step(&Step::AcknowledgeHosts);
        assert!(world.log.contains(&format!("retry after {}", delay)), "{:?}", world.log);
        if delay != "4s" {
            world.step(&Step::FailNextPrepare(false));
            world.step(&Step::Retry);
        }
    }
    // Endpoint `a`'s loop fails: its next incarnation waits its own 1 s.
    let (a, _) = world.held(&dd_out(), &additional("a")).unwrap();
    assert!(world.delivery_mut(&dd_out()).fail(&a, "connection reset"));
    world.log.clear();
    world.step(&Step::Exits);
    assert_eq!(world.log[0], "retry after 1s", "{:?}", world.log);
    assert!(!world.log.iter().any(|action| action.starts_with("prepare resources")));
    world.log.clear();
    world.step(&Step::Retry);
    // The transaction prepares the next incarnation, while the candidate waits out the rest of its delay.
    assert!(
        world
            .log
            .iter()
            .any(|action| action.starts_with("prepare resources revision")),
        "{:?}",
        world.log
    );
    assert!(world.log.contains(&"retry after 3s".to_string()), "{:?}", world.log);
    assert_eq!(world.core.status().preparing, None);
    assert!(world.core.status().pending.is_some());
    assert_clean(&mut world);
    assert_eq!(listener(world.core.active_plan().unwrap()), "x");
    let (next, state) = world.held(&dd_out(), &additional("a")).unwrap();
    assert_eq!((next.slot, state), (a.slot, FakeRecipientState::Live));
}

#[test]
fn each_failed_recipient_waits_its_own_backoff_before_its_next_incarnation() {
    let mut world = World::new();
    world.step(&Step::Configure(config(&["a", "b"], &[], false, false)));
    world.quiesce();
    // Endpoint `b` fails twice, and is replaced each time after its backoff.
    for _ in 0..2 {
        let (b, _) = world.held(&dd_out(), &additional("b")).unwrap();
        assert!(world.delivery_mut(&dd_out()).fail(&b, "connection reset"));
        world.quiesce();
    }
    // Its third failure waits 4 s; then `a`'s first waits 1 s.
    let (b, _) = world.held(&dd_out(), &additional("b")).unwrap();
    assert!(world.delivery_mut(&dd_out()).fail(&b, "connection reset"));
    world.log.clear();
    world.step(&Step::Exits);
    assert_eq!(world.log[0], "retry after 4s", "{:?}", world.log);
    let (a, _) = world.held(&dd_out(), &additional("a")).unwrap();
    assert!(world.delivery_mut(&dd_out()).fail(&a, "connection reset"));
    world.log.clear();
    world.step(&Step::Exits);
    assert_eq!(world.log[0], "retry after 1s", "{:?}", world.log);
    // After 1 s, only `a`'s next incarnation is prepared; `b` waits the rest of its own backoff.
    world.log.clear();
    world.step(&Step::Retry);
    assert!(world.log.contains(&"retry after 3s".to_string()), "{:?}", world.log);
    world.step(&Step::AcknowledgeHosts);
    let state = |world: &World, name| world.held(&dd_out(), &additional(name)).unwrap();
    assert!(state(&world, "a").0.incarnation > a.incarnation);
    assert_eq!(state(&world, "b"), (b.clone(), FakeRecipientState::Failed));
    world.step(&Step::Retry);
    world.step(&Step::AcknowledgeHosts);
    assert!(state(&world, "b").0.incarnation > b.incarnation);
    assert_clean(&mut world);
}

// The host contracts the core counts on, each shown by the shortest sequence that breaks without it (the property
// test found both against a fake without the rule).

#[test]
fn an_aborted_next_incarnation_of_a_failed_recipient_gives_the_slot_back_to_the_failed_one() {
    let mut world = World::new();
    let (main, _) = world.held(&dd_out(), &primary()).unwrap();
    assert!(world.delivery_mut(&dd_out()).fail(&main, "connection reset"));
    world.step(&Step::Exits);
    world.step(&Step::Retry);
    // The transaction that prepares the primary's next incarnation times out: its host drops the next incarnation
    // and puts the failed one back in the slot the boot generation's leases name, so that no payload names an empty
    // slot.
    world.step(&Step::TimeOutResources);
    assert_eq!(
        world.held(&dd_out(), &primary()),
        Some((main.clone(), FakeRecipientState::Failed))
    );
    assert!(world.errors.is_empty(), "{:?}", world.errors);
    assert_clean(&mut world);
    let (next, state) = world.held(&dd_out(), &primary()).unwrap();
    assert_eq!((next.slot, state), (main.slot, FakeRecipientState::Live));
}

#[test]
fn a_recipient_its_host_dropped_while_another_part_naming_it_failed_there_is_never_created_twice() {
    let mut world = World::new();
    // The primary's loop fails, unreported: a generation that requires it fails at its host, which then holds none
    // of the routing group's endpoints for that generation, while the core counts the generation as creating them
    // again under the transaction's references.
    let (main, _) = world.held(&dd_out(), &primary()).unwrap();
    assert!(world.delivery_mut(&dd_out()).fail(&main, "connection reset"));
    world.step(&Step::Configure(config(&["b", "c"], &[1], false, false)));
    let (endpoint, _) = world.held(&dd_out(), &group_1()[0]).unwrap();
    assert!(world.delivery_mut(&dd_out()).fail(&endpoint, "connection reset"));
    // The transaction times out, and its host drops the endpoints; a newer lowering's transaction lists them again
    // under the same references, which the host created once already, so it fails that part rather than start
    // second loops under them.
    world.step(&Step::TimeOutResources);
    world.step(&Step::Configure(config(&["a", "b"], &[1], false, false)));
    assert_eq!(
        world.held(&dd_out(), &additional("a")),
        None,
        "the part failed as a whole"
    );
    assert_clean(&mut world);
    assert_eq!(
        world.held(&dd_out(), &additional("a")).map(|(_, state)| state),
        Some(FakeRecipientState::Live)
    );
    for id in group_1() {
        let (recipient, state) = world.held(&dd_out(), &id).unwrap();
        assert_eq!(state, FakeRecipientState::Live);
        assert!(world.delivery[&dd_out()].native_set().contains(&recipient));
    }
}

#[test]
fn a_declared_recipients_next_incarnation_waits_for_the_loop_a_cancelled_preparation_dropped_and_rejects_nothing() {
    // A policy artifact arrives while the generation that first creates team D's recipient prepares: the core cancels
    // that generation, whose abort drops the recipient, and the artifact's generation creates its next incarnation at
    // once. The dropped loop still owns the queue directory, so the host holds that part until the loop's exit; the
    // resources revision is not rejected, and the artifact's generation commits with the next incarnation.
    let mut world = World::new();
    world.step(&Step::Configure(config(&[], &[], false, true)));
    let (dropped, _) = world
        .held(&dd_out(), &team_d())
        .expect("the generation created team D's recipient");
    world.step(&Step::Artifact("policy-a"));
    world.step(&Step::AcknowledgeHosts);
    world.step(&Step::AcknowledgeRegions);
    assert!(
        world.held(&dd_out(), &team_d()).is_none(),
        "the next incarnation waits for the dropped loop"
    );
    let preparing = world
        .core
        .status()
        .preparing
        .expect("the artifact's generation prepares");
    world.step(&Step::Exits);
    world.step(&Step::AcknowledgeHosts);
    let (next, state) = world
        .held(&dd_out(), &team_d())
        .expect("the next incarnation is prepared");
    assert!(next.incarnation > dropped.incarnation);
    assert_eq!(state, FakeRecipientState::Live);
    assert_eq!(world.core.status().active.map(|record| record.id), Some(preparing.id));
    let (state, _, error) = resources_ack(&world.core);
    assert_ne!(state, ApplyState::Rejected, "{:?}", error);
    assert_clean(&mut world);
    assert_eq!(resources_ack(&world.core).0, ApplyState::Active);
}

#[test]
fn a_retired_endpoints_withdrawal_is_dropped_by_a_publication() {
    // Withdrawal is by identity: a withdrawn set still naming a retired endpoint would withdraw the next incarnation a
    // generation prepares for it, before any transaction restored it.
    let mut world = World::new();
    world.step(&Step::Configure(config(&["a"], &[], false, false)));
    world.quiesce();
    world.step(&Step::Configure(config(&[], &[], false, false)));
    world.quiesce();
    assert!(
        world.held(&dd_out(), &additional("a")).is_none(),
        "the endpoint retired"
    );
    assert!(
        world.delivery[&dd_out()].withdrawn().is_empty(),
        "the host withdraws an identity it no longer holds: {:?}",
        world.delivery[&dd_out()].withdrawn()
    );
    assert_clean(&mut world);
}

#[test]
fn a_generation_using_an_endpoint_added_back_while_it_drains_commits_once_the_transaction_restoring_it_published() {
    // A routing group's endpoint is removed, withdrawn, and drains once the lease naming it drained; the group is
    // added back, and a second change supersedes the transaction that restores the endpoint before that one is
    // acknowledged. The generation whose lease names the endpoint again is acknowledged first, and waits for the newer
    // transaction's publication: committing before it would send its lease's payloads to an endpoint its host still
    // withdraws.
    let mut world = World::new();
    world.step(&Step::Configure(config(&[], &[0], false, false)));
    world.quiesce();
    world.step(&Step::Configure(config(&[], &[], false, false)));
    world.step(&Step::AcknowledgeRegions);
    world.step(&Step::AcknowledgeHosts);
    world.step(&Step::Retirements);
    world.step(&Step::LeaseReleases);
    let (draining, state) = world.held(&dd_out(), &group_0()).expect("the endpoint drains");
    assert_eq!(state, FakeRecipientState::Draining);
    assert!(world.delivery[&dd_out()].withdrawn().contains(&group_0()));

    world.step(&Step::Configure(config(&[], &[0], false, false)));
    world.step(&Step::Configure(config(&["b"], &[0], false, false)));
    world.step(&Step::AcknowledgeRegions);
    world.step(&Step::AcknowledgeHosts);
    world.step(&Step::AcknowledgeRegions);
    world.step(&Step::AcknowledgeHosts);
    assert!(world.errors.is_empty(), "{:?}\n{:#?}", world.errors, world.log);
    let (reopened, state) = world.held(&dd_out(), &group_0()).expect("the endpoint is back");
    assert_eq!((reopened, state), (draining, FakeRecipientState::Live));
    assert!(
        world.core.status().preparing.is_none(),
        "the generation committed once the publication was sent"
    );
    assert!(
        world
            .core
            .active_plan()
            .is_some_and(|active| active.resources.recipient(&group_0()).is_some()),
        "the generation that uses the endpoint again is active"
    );
    assert_clean(&mut world);
}

#[test]
fn a_failed_recipient_keeps_its_slot_when_its_next_incarnation_fails_too_and_every_later_preparation_aborts() {
    // The primary endpoint fails; the transaction preparing its next incarnation, which a generation creates again,
    // is not acknowledged before that one fails too; a newer transaction replaces the second with a third; then every
    // preparation of the second and third aborts. The first, committed, failed incarnation takes the slot back, in its
    // host as in the table, so that the leases of the active generation keep naming it.
    let mut world = World::new();
    let blocked = |declared| Config {
        listener: "tenant.raw.blocked",
        ..config(&[], &[], false, declared)
    };
    for step in [
        Step::FailRecipient(0),
        Step::Exits,
        Step::Retry,
        Step::Artifact("policy-a"),
        Step::FailRecipient(0),
        Step::Exits,
        Step::Retry,
        Step::Configure(blocked(false)),
        Step::Configure(blocked(true)),
        Step::RefuseCredits,
        Step::TimeOut,
        Step::TimeOutResources,
    ] {
        world.step(&step);
        assert!(
            world.errors.is_empty(),
            "after {:?}: {:?}\n{:#?}",
            step,
            world.errors,
            world.log
        );
    }
    let (held, _) = world
        .held(&dd_out(), &primary())
        .expect("the host holds the primary endpoint");
    assert_eq!(held.slot, RecipientSlot::new(0));
    assert!(
        world
            .core
            .resource_table()
            .recipients()
            .iter()
            .any(|status| status.recipient == held && status.state == RecipientState::Failed),
        "the table holds the failed marker its host holds: {:?}",
        world.core.resource_table().recipients()
    );
    assert_clean(&mut world);
}

#[test]
fn a_recipient_a_generation_of_an_older_lowering_creates_after_a_newer_publication_is_withdrawn_at_once() {
    // A generation waits for the transaction that restores the routing groups' endpoints. A newer lowering, which
    // removes them and failover again, publishes at once, and the waiting generation commits, which starts the
    // candidate of the lowering before it: that one creates the failover endpoint the latest lowering no longer
    // declares, and a publication withdraws it before anything else happens.
    let mut world = World::new();
    let blocked = |groups: &[u8], failover| Config {
        listener: "tenant.raw.blocked",
        ..config(&[], groups, failover, false)
    };
    for step in [
        Step::Configure(blocked(&[0, 1], false)),
        Step::AcknowledgeHosts,
        Step::Configure(blocked(&[], false)),
        Step::Configure(blocked(&[1], true)),
        Step::AcknowledgeRegions,
        Step::Configure(blocked(&[], false)),
    ] {
        world.step(&step);
        assert!(
            world.errors.is_empty(),
            "after {:?}: {:?}\n{:#?}",
            step,
            world.errors,
            world.log
        );
    }
    if let Some((_, state)) = world.held(&mrf_dd_out(), &mrf_primary()) {
        assert_ne!(state, FakeRecipientState::Draining);
        assert!(world.delivery[&mrf_dd_out()].withdrawn().contains(&mrf_primary()));
    }
    assert_clean(&mut world);
}

// Random sequences.

fn arb_config() -> impl Strategy<Value = Config> {
    (
        prop::sample::subsequence(ENDPOINTS.to_vec(), 0..=3),
        prop::sample::subsequence(vec![0u8, 1], 0..=2),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        prop::sample::select(vec![BOOT_LISTENER, "x", "y"]),
    )
        .prop_map(|(endpoints, groups, v3, failover, declared, listener)| Config {
            endpoints: endpoints.into_iter().collect(),
            groups: groups.into_iter().collect(),
            v3,
            failover,
            declared,
            listener,
        })
}

fn arb_step() -> impl Strategy<Value = Step> {
    prop_oneof![
        4 => arb_config().prop_map(Step::Configure),
        2 => prop::sample::select(vec!["policy-a", "policy-b", "cut-a", "cut-b"]).prop_map(Step::Artifact),
        5 => Just(Step::AcknowledgeRegions),
        5 => Just(Step::AcknowledgeHosts),
        1 => any::<bool>().prop_map(Step::FailNextPrepare),
        1 => Just(Step::TimeOut),
        1 => Just(Step::TimeOutResources),
        2 => Just(Step::Retry),
        3 => Just(Step::Retirements),
        3 => Just(Step::LeaseReleases),
        2 => Just(Step::EndDrains),
        1 => (0..8usize).prop_map(Step::FailRecipient),
        3 => Just(Step::Exits),
        1 => Just(Step::Panic),
        2 => Just(Step::Arm),
        2 => any::<bool>().prop_map(|all| Step::Stamp { all }),
        1 => any::<bool>().prop_map(|refused| Step::Cancel { refused }),
        1 => Just(Step::RefuseCredits),
        2 => Just(Step::Restart),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// Random lowerings and artifacts, with resource transactions and their supersession, commits, aborts, cuts and
    /// their cancellations, preparation and recipient failures, budget refusals, worker restarts that repeat the
    /// actions just performed, and retirements, never release a lease or recipient a live generation past its barrier
    /// or the published native set uses, never keep a generation or rollback plan that uses a recipient or output the
    /// latest lowering does not declare, withdraw every recipient the latest lowering does not declare once no
    /// transaction is in flight, release every credit exactly once, and, once every host and batch caught up, release
    /// everything nothing uses any more.
    #[test]
    fn property_test_resource_table_never_releases_what_a_generation_uses(
        steps in prop::collection::vec(arb_step(), 1..40),
    ) {
        let mut world = World::new();
        for step in &steps {
            world.step(step);
            prop_assert!(world.errors.is_empty(), "after {:?}: {:?}\n{:#?}", step, world.errors, world.log);
        }
        world.quiesce();
        world.check_reclaimed();
        prop_assert!(world.errors.is_empty(), "once caught up: {:?}\n{:#?}", world.errors, world.log);
    }
}
