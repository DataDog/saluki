//! The scripted check of the demo (`programmable-metrics-demo check`, `make demo-programmable-metrics-check`): the
//! scenarios of the detached programs design's demo section, driven through the demo server's API against the ADP the
//! demo launched, with what each endpoint receives checked after every step.
//!
//! ADP boots with the primary endpoint, teams B and C as additional endpoints, and the failover endpoint. Natively an
//! additional endpoint receives everything (dual shipping), so the default program, which ADP runs until a program is
//! published, sends the primary endpoint and teams B and C every family, and the failover endpoint nothing, since
//! failover mirroring is off. Every policy step edits the default program as ADP exports it, detached: its policy
//! settings written as literals, its outputs as the recipients they deliver to (see [`edits`]). It compiles the program
//! (`POST /api/compile`, which composes it into ADP's default program, validates it, and diffs it against the active
//! plan), publishes it (`POST /api/publish`), waits for ADP's acknowledgment (`GET /api/adp/status`, and the stub's
//! record of ADP's polls), and then waits for the families each intake receives to be what the scenario says, and to
//! stay so for [`STABLE`]:
//!
//! - **S0**: the unmodified default program adopts the running generation (no new generation), its plan diff is empty,
//!   ADP's status says a published program runs and native policy settings are not applied, and the endpoints receive
//!   what they did.
//! - **S1**: `demo.debug.*` is dropped ahead of the policy: it leaves every endpoint.
//! - **S2**, the recipe: team B is narrowed to the checkout families: its URL leaves the main `emit`, and a branch
//!   emits `demo.checkout.*` to it. Both `emit`s address recipient sets no output delivers to, so the editor says ADP
//!   adds implied outputs, ADP adds them while the program waits, and the program then installs; the editor, compiling
//!   the program again, composes the plan ADP runs.
//! - **S3**: S2's route widened with `or tag("team") == "platform"` (S3a); then exclusive routing, `team:checkout` to
//!   team B only, ahead of the policy (S3b).
//! - **S4**: the failover gateway mirrors only `demo.checkout.requests` (S4a), which shows once failover turns on
//!   (S4b, `multi_region_failover.failover_metrics` in the configuration file, a signal the program reads live) and
//!   stops when it turns off (S4c).
//! - **S5**: guardrails, each published anyway and refused while S4's generation keeps running: a duplicate branch to
//!   the main `emit`'s recipients, an `emit` to recipients of two delivery hosts, a second aggregation, the aggregation
//!   moved after enrichment (refused by placement, which the editor does not run), a binding of a policy setting, which
//!   ADP does not declare (policy settings are literals, never bindings), each rejected; and an `emit` to a recipient
//!   ADP does not know, held `pending`.
//! - **S6**: revoking (an empty snapshot) brings the default program back, the boot plan again.
//!
//! Then destinations come and go while ADP runs ([`destinations`]): ADP re-reads its configuration file, which the demo
//! rewrites, so an endpoint added to or removed from `additional_endpoints` becomes, or stops being, a recipient live.
//!
//! - **S8**: under the default program, an added destination D receives everything (S8a); removed, its recipient
//!   drains and retires and its intake stops, the others unchanged (S8b).
//! - **S9**: under a published program (the recipe again, S9a), an added destination E is declared but not addressed,
//!   and receives nothing (S9b), until the program routes `team:platform` to it (S9c).
//! - **S10**: E removed while the program routes to it: the program stays `active`, degraded, its `emit` to E a
//!   counted drop of policy loss at its node, E's recipient retires, and the other endpoints are unchanged. Then team
//!   C removed (S10b), which the program's main `emit` names beside the primary endpoint: the `emit` keeps delivering
//!   to the primary endpoint, whose canaries stay exact, what team C would have received is counted as policy loss at
//!   the drop beside it (`metrics_fanout_0_missing`), team B is unchanged, and team C's recipient retires. Team C stays
//!   out of the configuration for the rest of the launch.
//! - **S11**: a native policy change under the published program (`metric_filterlist` in the file) makes no new
//!   generation and changes nothing it sends, and ADP's status lists the setting as differing (S11a); revoking then
//!   applies it, and the filterlisted family leaves every endpoint (S11b); clearing the filterlist brings it back
//!   (S11c).
//!
//! Each policy step after S0 also checks that the active plan hash ADP reports is the hash of the plan the editor
//! composed, so the editor's preview is what ADP installs. Throughout, every sample of the demo's state checks the
//! canary conservation of the moment (the canaries the primary endpoint received for a recent range of seconds, against
//! an unexpected API key, and that no lane admitted an event another lane owns (`plan_lane_misrouted_total` stays 0);
//! at the end, the canaries of the whole run, from the first balanced range to the last, are checked at once, across
//! every install, every destination added and removed, and every configuration change. The demo keeps five minutes of
//! canary counts, so the runner reads the whole run's counts a minute at a time as it goes ([`CanaryTotals`]) and
//! checks their sum. Every install is in the ranges checked, but a check balances within an allowance for canaries
//! counted in the second next to its range's edges (6 at the default 20 datagrams per second): an aggregated canary
//! point lost or delivered twice (20 canaries at that rate, with the demo's 1-second windows) fails it, while a few
//! datagrams lost or doubled ahead of aggregation do not. ADP's continuous-input tests (`installer::lifecycle_tests`)
//! check that no point is lost or duplicated across installs, exactly.
//!
//! Canaries the host dropped before ADP read them, as it does while another process on the host floods UDP, are no
//! loss of ADP's: a range in which the demo's control datagrams were dropped too, and ADP's DogStatsD source received
//! fewer datagrams than were sent by as many as the canaries missing, is `host_dropped`
//! ([`CanaryState::HostDropped`]), which the runner logs once per stretch of such ranges and counts
//! ([`Report::host_dropped_samples`]), and the whole run's check subtracts what the host dropped
//! ([`canary::classify_run`]). A range of the whole run that the check cannot say yet, because no scrape of ADP's
//! telemetry has covered its end while the host was dropping datagrams, is read again later, to a later end.
//!
//! Lane counts are fixed when ADP boots, so S12 to S14 run in a second launch of ADP, its metrics path on lanes, with
//! 10-second windows ([`run_lanes`]; see [`lanes`]): the frame on the lanes asked for, a counter whose state lane is
//! empty at a tick, a width change of aggregation with per-lane crossings, and the aggregation renamed, which keeps its
//! state. With longer windows, a family counts as arriving for a window longer ([`present_ms`]), and the endpoints must
//! hold what a step says for one window ([`Runner::stable`]).
//!
//! The steps are timed: compile, publish to acknowledgment, and publish to the endpoints showing the change. The
//! report ([`Report`]) is written to `check-report.json` in the run directory, the second launch's under
//! `second_launch`.

use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};

use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_metric_plan::{canonical::ResolvedValues, ir::Plan};
use serde::Serialize;
use serde_json::{json, Value};

use self::{
    client::DemoClient,
    edits::{MIRRORED_METRIC, UNKNOWN_RECIPIENT},
};
use crate::{
    canary::{self, CanaryState},
    clock::unix_millis,
    settings::{DemoPorts, RunSettings},
    stub::protocol::POLICY_PRODUCT,
};

pub mod client;
mod destinations;
pub(crate) mod edits;
mod lanes;

/// How often the runner samples the demo's state while it waits.
const POLL: Duration = Duration::from_millis(250);

/// How recently a family must have reached an intake to count as arriving there, in milliseconds. Intakes receive a
/// payload every second or two; a family that stopped arriving leaves after this.
const PRESENT_MS: u64 = 3_500;

/// How long the endpoints must keep receiving what a scenario says once they do.
const STABLE: Duration = Duration::from_secs(3);

/// How long ADP may take to serve its export and default program, and the canary check to have a whole range.
const BASELINE_TIMEOUT: Duration = Duration::from_secs(90);

/// How long ADP may take to acknowledge a publish or a revoke.
const ACK_TIMEOUT: Duration = Duration::from_secs(20);

/// How long the endpoints may take to show a change.
const ROUTING_TIMEOUT: Duration = Duration::from_secs(30);

/// How many seconds of canaries the runner adds to the whole run's totals at once, as soon as the rolling check's
/// range has moved that far past them: well within the five minutes of counts the demo keeps, so that the range read
/// is still held whole however long the runner then goes without sampling the demo's state.
const CANARY_SEGMENT_SECS: u64 = 60;

/// How long the runner goes at most without sampling the demo's state, which checks the canaries, the intakes, and
/// the lanes, and adds to the whole run's canary totals: steps that wait on ADP's status alone sample it this often.
const SAMPLE_INTERVAL: Duration = Duration::from_secs(5);

/// The metric families the traffic generator sends.
const FAMILIES: [&str; 6] = [
    "demo.canary",
    "demo.catalog.views",
    "demo.checkout.latency",
    "demo.checkout.requests",
    "demo.debug.*",
    "demo.platform.queue_depth",
];

/// The checkout families, which the recipe's route to team B matches by prefix.
const CHECKOUT: [&str; 2] = ["demo.checkout.latency", "demo.checkout.requests"];

/// The family the filterlist scenario lists.
const FILTERLISTED: &str = "demo.catalog.views";

/// The metric families each intake receives, by intake name. An intake the routing does not name receives nothing.
type Routing = BTreeMap<String, BTreeSet<String>>;

/// Returns the routing of the default program: everything to the primary endpoint and to teams B and C, which are
/// additional endpoints and natively receive everything, and nothing to the failover endpoint while failover is off.
fn default_routing() -> Routing {
    routing(&[
        ("primary", &FAMILIES),
        ("team-b", &FAMILIES),
        ("team-c", &FAMILIES),
        ("mrf", &[]),
    ])
}

/// Returns the routing of the recipe (S2): team B receives the checkout families only.
fn narrowed_routing() -> Routing {
    routing(&[
        ("primary", &FAMILIES),
        ("team-b", &CHECKOUT),
        ("team-c", &FAMILIES),
        ("mrf", &[]),
    ])
}

fn routing(entries: &[(&str, &[&str])]) -> Routing {
    entries
        .iter()
        .map(|(intake, families)| (intake.to_string(), families.iter().map(ToString::to_string).collect()))
        .collect()
}

/// Returns `routing` with `families` added to (or, with `remove`, removed from) what `intake` receives.
fn changed(mut routing: Routing, intake: &str, families: &[&str], remove: bool) -> Routing {
    let set = routing.entry(intake.to_string()).or_default();
    for family in families {
        if remove {
            set.remove(*family);
        } else {
            set.insert(family.to_string());
        }
    }
    routing
}

/// Returns how recently a family must have reached an intake to count as arriving there, in milliseconds, with
/// aggregation windows of `window_s` seconds: [`PRESENT_MS`] with one-second windows, and a window longer for each
/// second more, since a family arrives once per window.
fn present_ms(window_s: u64) -> u64 {
    PRESENT_MS + window_s.saturating_sub(1) * 1000
}

/// Returns the families each intake receives now, according to the demo's state: the `demo.` families seen within
/// `present_ms` milliseconds.
fn present(state: &Value, present_ms: u64) -> Routing {
    let mut found = Routing::new();
    for intake in state["intakes"].as_array().into_iter().flatten() {
        let name = intake["name"].as_str().unwrap_or_default().to_string();
        let families = intake["families"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|family| family["age_ms"].as_u64().is_some_and(|age| age <= present_ms))
            .filter_map(|family| family["family"].as_str())
            .filter(|family| family.starts_with("demo."))
            .map(ToString::to_string)
            .collect();
        found.insert(name, families);
    }
    found
}

/// Returns `true` if the intakes receive what `expected` says: those it names, its families, and every other intake,
/// nothing.
fn routing_holds(expected: &Routing, found: &Routing) -> bool {
    let empty = BTreeSet::new();
    intakes_of(expected, found)
        .all(|intake| expected.get(intake).unwrap_or(&empty) == found.get(intake).unwrap_or(&empty))
}

/// Returns the intakes `expected` or `found` names, each once.
fn intakes_of<'a>(expected: &'a Routing, found: &'a Routing) -> impl Iterator<Item = &'a str> {
    expected
        .keys()
        .chain(found.keys())
        .map(String::as_str)
        .collect::<BTreeSet<_>>()
        .into_iter()
}

/// Describes how `found` differs from `expected`, intake by intake.
fn routing_difference(expected: &Routing, found: &Routing) -> String {
    let empty = BTreeSet::new();
    intakes_of(expected, found)
        .filter_map(|intake| {
            let want = expected.get(intake).unwrap_or(&empty);
            let have = found.get(intake).unwrap_or(&empty);
            (want != have).then(|| {
                let missing = want.difference(have).cloned().collect::<Vec<_>>();
                let extra = have.difference(want).cloned().collect::<Vec<_>>();
                let mut parts = Vec::new();
                if !missing.is_empty() {
                    parts.push(format!("not receiving {}", missing.join(", ")));
                }
                if !extra.is_empty() {
                    parts.push(format!("also receiving {}", extra.join(", ")));
                }
                format!("{} is {}", intake, parts.join(" and "))
            })
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// One step of the run, as the report records it.
#[derive(Clone, Debug, Default, Serialize)]
pub struct StepReport {
    /// The scenario, such as `S1`.
    pub id: &'static str,

    /// What the step does.
    pub title: &'static str,

    /// The revision published, if the step published one.
    pub revision: Option<u64>,

    /// What ADP did with it.
    pub outcome: String,

    /// The plan diff the editor showed, in a few words.
    pub diff: String,

    /// How long compiling took, in milliseconds.
    pub compile_ms: Option<u64>,

    /// From publishing (or revoking) to ADP's acknowledgment, in milliseconds.
    pub ack_ms: Option<u64>,

    /// From publishing (or revoking) to the endpoints receiving what the scenario says, in milliseconds.
    pub visible_ms: Option<u64>,

    /// The generation active after the step.
    pub generation: Option<u64>,
}

impl StepReport {
    /// Returns the report of a step that has not run yet.
    fn new(id: &'static str, title: &'static str) -> Self {
        Self {
            id,
            title,
            ..Self::default()
        }
    }
}

/// One control surface route the run read through the demo, and what it checked.
#[derive(Clone, Debug, Serialize)]
pub struct RouteReport {
    /// The demo's pass-through route.
    pub route: &'static str,

    /// What was checked.
    pub checked: String,
}

/// The whole-run canary check.
#[derive(Clone, Debug, Serialize)]
pub struct CanaryReport {
    /// The first second of the range (Unix seconds).
    pub from_s: u64,

    /// The second just past the range.
    pub to_s: u64,

    /// Canaries sent in the range.
    pub sent: u64,

    /// Canaries the primary endpoint received for the range.
    pub received: f64,

    /// Canaries of the range that the host dropped before ADP read them: the shortfall of its ranges whose check found
    /// the host dropped them.
    pub host_dropped: f64,

    /// The check's state: `balanced`, `host_dropped` (balanced once the host's drops are counted), `missing`, or
    /// `excess`.
    pub state: String,
}

/// The canaries of the whole run so far, from the first balanced range of the rolling check: what the demo's canary
/// check says of contiguous ranges of seconds, each read while it is within the history the demo keeps, summed.
///
/// The sums over contiguous ranges are the sums over the whole, whatever the bucket widths: each range ends where the
/// rolling check's range ended when it was read, at a boundary of every bucket width in use, with every bucket before
/// it delivered, so that each canary is counted in exactly one range, as sent and as received.
#[derive(Clone, Debug, PartialEq)]
struct CanaryTotals {
    /// The first second of the whole run's range.
    from_s: u64,

    /// The second just past the ranges added so far.
    to_s: u64,

    /// Canaries sent in the ranges added.
    sent: u64,

    /// Canaries the primary endpoint received for the ranges added.
    received: f64,

    /// Canaries the host dropped before ADP read them, in the ranges added: the shortfall of those whose check found
    /// that the host dropped them.
    host_dropped: f64,

    /// The allowance of the last range added: that of the traffic's rate, whatever the range.
    allowance: f64,

    /// Whether the check could not say what a range added holds, since ADP was not up for all of it.
    waiting: bool,
}

impl CanaryTotals {
    /// Returns the totals of a run whose first balanced range starts at `from_s`: nothing added yet.
    fn starting_at(from_s: u64) -> Self {
        Self {
            from_s,
            to_s: from_s,
            sent: 0,
            received: 0.0,
            host_dropped: 0.0,
            allowance: 0.0,
            waiting: false,
        }
    }

    /// Returns the range to add next, once the rolling check's range ends at `rolling_to_s`: none until that is
    /// [`CANARY_SEGMENT_SECS`] past what was added.
    fn due(&self, rolling_to_s: u64) -> Option<(u64, u64)> {
        (rolling_to_s >= self.to_s + CANARY_SEGMENT_SECS).then_some((self.to_s, rolling_to_s))
    }

    /// Adds what the demo's canary check says of `range` (`GET /api/canary`), which must start where the ranges added
    /// so far end.
    fn add(&mut self, range: (u64, u64), check: &Value) -> Result<(), GenericError> {
        if range.0 != self.to_s || range.1 <= range.0 {
            return Err(generic_error!(
                "The canaries of seconds {} to {} do not continue the whole run's, which end at {}.",
                range.0,
                range.1,
                self.to_s
            ));
        }
        let (Some(sent), Some(received), Some(allowance)) = (
            check["sent"].as_u64(),
            check["received"].as_f64(),
            check["allowance"].as_f64(),
        ) else {
            return Err(generic_error!(
                "The canary check of seconds {} to {} answered without its counts: {}",
                range.0,
                range.1,
                check
            ));
        };
        self.to_s = range.1;
        self.sent += sent;
        self.received += received;
        if check["state"] == CanaryState::HostDropped.name() {
            self.host_dropped += sent as f64 - received;
        }
        self.allowance = allowance;
        self.waiting |= check["state"] == CanaryState::Waiting.name();
        Ok(())
    }

    /// Returns the whole-run check over the ranges added.
    fn report(&self) -> CanaryReport {
        let state = if self.waiting {
            CanaryState::Waiting
        } else {
            canary::classify_run(self.sent, self.received, self.host_dropped, self.allowance)
        };
        CanaryReport {
            from_s: self.from_s,
            to_s: self.to_s,
            sent: self.sent,
            received: self.received,
            host_dropped: self.host_dropped,
            state: state.name().to_string(),
        }
    }
}

/// The report of a run: of one launch of ADP, and of the second launch, on lanes, when the check ran one.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Report {
    /// What ADP ran on in this launch: its aggregation window and its lanes.
    pub launch: String,

    /// The run directory.
    pub run_dir: String,

    /// How long the run took, in milliseconds.
    pub duration_ms: u64,

    /// How long ADP took to serve its export and the canary check to have a whole range, in milliseconds.
    pub baseline_ms: Option<u64>,

    /// The steps, in order.
    pub steps: Vec<StepReport>,

    /// The control surface routes read.
    pub routes: Vec<RouteReport>,

    /// The whole-run canary check.
    pub canary: Option<CanaryReport>,

    /// How many samples of the demo's state checked canary conservation, balanced.
    pub canary_samples: u64,

    /// How many samples of the demo's state found canaries that the host dropped before ADP read them, and none lost
    /// in ADP.
    pub host_dropped_samples: u64,

    /// Problems seen while sampling: canaries missing, doubled, or at another endpoint; refused requests; unexpected
    /// API keys.
    pub violations: Vec<String>,

    /// Why the run stopped, if a step failed.
    pub error: Option<String>,

    /// The report of the second launch, on lanes (S12 to S14), if the check ran it after this one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub second_launch: Option<Box<Report>>,
}

impl Report {
    /// Returns why the check failed, if it did: in this launch, or else in the second.
    pub fn failure(&self) -> Option<String> {
        self.own_failure().or_else(|| {
            let second = self.second_launch.as_ref()?;
            second
                .own_failure()
                .map(|failure| format!("in the second launch ({}): {}", second.launch, failure))
        })
    }

    /// Returns why this launch failed, if it did.
    fn own_failure(&self) -> Option<String> {
        if let Some(error) = &self.error {
            return Some(error.clone());
        }
        if !self.violations.is_empty() {
            return Some(format!(
                "{} problems were seen while the scenarios ran: {}",
                self.violations.len(),
                self.violations.join("; ")
            ));
        }
        match &self.canary {
            None => Some("The whole-run canary check did not run.".to_string()),
            Some(canary)
                if canary.state != CanaryState::Balanced.name() && canary.state != CanaryState::HostDropped.name() =>
            {
                Some(format!(
                    "The whole-run canary check is {}: {} sent, {} received at the primary endpoint.",
                    canary.state, canary.sent, canary.received
                ))
            }
            Some(_) => None,
        }
    }

    /// Returns the report as lines for the console: the launch, one line per step, then the canary check; then the
    /// second launch's, if any.
    pub fn summary_lines(&self) -> Vec<String> {
        let seconds = |ms: Option<u64>| ms.map_or_else(|| "-".to_string(), |ms| format!("{:.1} s", ms as f64 / 1000.0));
        let mut lines = vec![
            format!("launch: {}", self.launch),
            format!(
                "{:<4} {:<52} {:>4} {:>8} {:>8} {:>8}  outcome; plan diff",
                "step", "", "rev", "compile", "ack", "visible"
            ),
        ];
        for step in &self.steps {
            lines.push(format!(
                "{:<4} {:<52} {:>4} {:>8} {:>8} {:>8}  {}; {}",
                step.id,
                step.title,
                step.revision
                    .map_or_else(|| "-".to_string(), |revision| revision.to_string()),
                seconds(step.compile_ms),
                seconds(step.ack_ms),
                seconds(step.visible_ms),
                step.outcome,
                step.diff
            ));
        }
        if let Some(canary) = &self.canary {
            let host_dropped = if self.host_dropped_samples > 0 || canary.host_dropped > 0.0 {
                format!(
                    "; the host dropped {} before ADP read them ({} rolling checks)",
                    canary.host_dropped, self.host_dropped_samples
                )
            } else {
                String::new()
            };
            lines.push(format!(
                "canary: {} sent and {} received at the primary endpoint over {} s ({}); {} rolling checks balanced{}",
                canary.sent,
                canary.received,
                canary.to_s - canary.from_s,
                canary.state,
                self.canary_samples,
                host_dropped
            ));
        }
        lines.push(format!(
            "run: {:.1} s, baseline {}",
            self.duration_ms as f64 / 1000.0,
            seconds(self.baseline_ms)
        ));
        if let Some(second) = &self.second_launch {
            lines.extend(second.summary_lines());
        }
        lines
    }
}

/// Runs the scenarios S0 to S6 and S8 to S11 against the demo server of `settings`, and returns the report. A failed
/// step stops the run, and the report says why.
pub async fn run(settings: &RunSettings) -> Report {
    let started = Instant::now();
    let mut runner = match Runner::new(settings) {
        Ok(runner) => runner,
        Err(error) => return not_started(settings, error),
    };
    if let Err(error) = runner.scenarios().await {
        runner.report.error = Some(format!("{:#}", error));
    }
    runner.report.duration_ms = started.elapsed().as_millis() as u64;
    runner.report
}

/// Runs the scenarios S12 to S14 against the demo server of `settings`, whose ADP runs its metrics path on the lanes
/// `settings` asks for (see [`lanes`]), and returns the report. A failed step stops the run, and the report says why.
pub async fn run_lanes(settings: &RunSettings) -> Report {
    let started = Instant::now();
    let mut runner = match Runner::new(settings) {
        Ok(runner) => runner,
        Err(error) => return not_started(settings, error),
    };
    if let Err(error) = runner.lane_scenarios(settings).await {
        runner.report.error = Some(format!("{:#}", error));
    }
    runner.report.duration_ms = started.elapsed().as_millis() as u64;
    runner.report
}

/// Returns the report of a launch of `settings` that did not run its scenarios to the end: its runner could not be
/// created, or the launch itself failed (the demo or ADP did not start, ADP exited, or a stop signal arrived), as
/// `error` says.
pub fn not_started(settings: &RunSettings, error: GenericError) -> Report {
    Report {
        launch: describe_launch(settings),
        error: Some(format!("{:#}", error)),
        ..Report::default()
    }
}

/// Describes what ADP runs on in a launch, for the report: its aggregation window, and the lanes the launcher asked
/// for.
fn describe_launch(settings: &RunSettings) -> String {
    let lanes = settings.lanes;
    let count = |count: Option<u16>| count.map_or_else(|| "automatic".to_string(), |count| count.to_string());
    let lanes = if lanes.any() {
        format!(
            "on lanes: decoders {}, state lanes {}, post-aggregation lanes {}",
            count(lanes.decoders),
            count(lanes.state),
            count(lanes.post)
        )
    } else {
        "with ADP's default lanes".to_string()
    };
    format!(
        "{}-second aggregation windows, counter expiry {} s, {}",
        settings.aggregate_window_secs, settings.counter_expiry_secs, lanes
    )
}

/// What the runner knows about ADP's status at one moment.
#[derive(Clone, Debug)]
struct Status {
    generation: Option<u64>,
    plan_hash: Option<String>,
    artifact_revision: Option<u64>,
    default_program_hash: Option<String>,
    document: Value,
}

impl Status {
    fn read(document: Value) -> Self {
        Self {
            generation: document["active_generation"].as_u64(),
            plan_hash: document["active_plan_hash"].as_str().map(ToString::to_string),
            artifact_revision: document["active_artifact"]["revision"].as_u64(),
            default_program_hash: document["default_program_hash"].as_str().map(ToString::to_string),
            document,
        }
    }

    /// Returns which program runs, as the status's `program` member says: `default` or `published`.
    fn runs(&self) -> Option<&str> {
        self.document["program"]["runs"].as_str()
    }

    /// Returns whether the status's `program` member says the native policy settings apply: `applied` or
    /// `not_applied`.
    fn native_policy(&self) -> Option<&str> {
        self.document["program"]["native_policy"].as_str()
    }

    /// Returns the nodes of the default program the status's `program` member lists as differing from what runs.
    fn differs(&self) -> Vec<&str> {
        self.document["program"]["differs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect()
    }

    /// Returns ADP's acknowledgment of `revision` of `product`, if it reports one. A config state that names no
    /// product is the policy product's, as the first version of the protocol wrote it.
    fn acknowledgment(&self, product: &str, revision: u64) -> Option<&Value> {
        self.document["config_states"]
            .as_array()?
            .iter()
            .find(|state| state["revision"].as_u64() == Some(revision) && product_of(state) == product)
    }
}

/// Returns `true` if an acknowledgment is final for the step that waits for it: `active`, `rejected`, or `failed`.
fn settled(acknowledgment: &Value) -> bool {
    matches!(acknowledgment["state"].as_str(), Some("active" | "rejected" | "failed"))
}

/// Returns `true` if an acknowledgment is `pending` with an error that names `what`: an artifact that waits for it,
/// such as a recipient, rather than one being installed, which is `pending` too.
fn waiting_for(acknowledgment: &Value, what: &str) -> bool {
    acknowledgment["state"] == "pending"
        && acknowledgment["error"]
            .as_str()
            .is_some_and(|error| error.contains(what))
}

/// Returns the product of a config state: the one it names, or the policy product when it names none.
fn product_of(state: &Value) -> &str {
    state["product"]
        .as_str()
        .filter(|product| !product.is_empty())
        .unwrap_or(POLICY_PRODUCT)
}

/// What a scenario expects of a compile.
enum Compiled {
    /// No findings, and a plan diff: empty for an unmodified export, not empty otherwise. Or, for a program that emits
    /// to recipients no output delivers to exactly, only the note that ADP adds an output for them (an implied output)
    /// and no plan diff: the step then compiles the program again once ADP runs it, and checks that the editor composes
    /// the plan ADP installed.
    Clean { unchanged: bool },

    /// An error whose message (or validation rule) has this fragment.
    Flagged { fragment: &'static str },

    /// An artifact, with findings or without: what refuses it is ADP's placement, which the editor does not run.
    Unplaced,

    /// A warning whose message has this fragment, and no plan diff: ADP holds the program pending, as for a recipient
    /// its directory does not name.
    Waiting { fragment: &'static str },
}

/// How the editor starts what it says of a program that emits to recipients no output delivers to exactly yet: ADP
/// adds an output for them.
const IMPLIED_OUTPUT: &str = crate::preview::IMPLIED_OUTPUT_NOTE;

/// The fragment of ADP's refusal of an `emit` to recipients of two delivery hosts.
const TWO_HOSTS: &[&str] = &["more than one delivery host"];

/// The fragment of ADP's refusal of a binding of a policy setting: policy settings are literals, never bindings, so the
/// binding is one the host does not declare.
const POLICY_BINDING: &[&str] = &["which is not a configuration binding the host declares"];

/// The fragment of ADP's refusal of a second aggregation: the validation rule that a program has at most one.
const SECOND_AGGREGATION: &[&str] = &["single_aggregation"];

/// The fragment of ADP's refusal of the aggregation moved after enrichment: placement refuses it with the capability it
/// lacks, since a state owner runs only in an isolation slot, and none is behind enrichment.
const AGGREGATION_OUTSIDE_ITS_SLOT: &[&str] = &["runs only in an isolation slot"];

/// How long ADP may take to acknowledge a step's artifact.
#[derive(Clone, Copy, Debug)]
struct Target {
    /// How long ADP may take to acknowledge the artifact.
    ack_timeout: Duration,
}

/// An artifact that installs without a cut.
const POLICY_TARGET: Target = Target {
    ack_timeout: ACK_TIMEOUT,
};

/// What a scenario expects ADP to do with a publish.
enum Installed {
    /// Activate the artifact; with `new_generation` false, in the generation already active. With `routing`, the
    /// endpoints then receive what it says; without, the step checks what they receive itself.
    Active {
        new_generation: bool,
        routing: Option<Routing>,
    },

    /// Reject it with an error that has one of these fragments, and keep the active generation running.
    Rejected { fragments: &'static [&'static str] },

    /// Hold it `pending`, with an error that names what it waits for, and keep the active generation running.
    Pending { waits_for: &'static str },
}

struct Runner {
    client: DemoClient,
    report: Report,
    /// The whole run's canaries so far, from the first canary range found balanced.
    whole_run: Option<CanaryTotals>,
    /// When the runner last sampled the demo's state.
    sampled: Option<Instant>,
    /// When the last publish or revoke was made, in Unix seconds.
    last_change_s: u64,
    /// The intake problems already reported, so that each is reported once.
    alerted: BTreeSet<String>,
    /// Whether the last sample's canary check found that the host dropped canaries, so that each stretch of such
    /// samples is logged once.
    host_dropping: bool,
    /// ADP's aggregation window, in seconds, which sets how often each family reaches the intakes.
    window_s: u64,
    /// The demo's ports, which give the URLs of teams B and C.
    ports: DemoPorts,
}

impl Runner {
    /// Creates the runner of the demo server of `settings`.
    ///
    /// # Errors
    ///
    /// Returns an error if the demo server's client cannot be built.
    fn new(settings: &RunSettings) -> Result<Self, GenericError> {
        let client = DemoClient::new(settings.ports.ui)?;
        Ok(Self {
            client,
            report: Report {
                launch: describe_launch(settings),
                run_dir: settings.run_dir.display().to_string(),
                ..Report::default()
            },
            whole_run: None,
            sampled: None,
            last_change_s: 0,
            alerted: BTreeSet::new(),
            host_dropping: false,
            window_s: settings.aggregate_window_secs.max(1),
            ports: settings.ports.clone(),
        })
    }

    /// Returns how recently a family must have reached an intake to count as arriving there (see [`present_ms`]).
    fn present_ms(&self) -> u64 {
        present_ms(self.window_s)
    }

    /// Returns how long the endpoints must keep receiving what a scenario says once they do: [`STABLE`], or one
    /// aggregation window if longer, so that at least one more flush shows the same.
    fn stable(&self) -> Duration {
        STABLE.max(Duration::from_secs(self.window_s))
    }

    /// Returns how long the endpoints may take to show a change: [`ROUTING_TIMEOUT`], or, with long windows, long
    /// enough for a family to stop counting as present and for one to arrive, twice.
    fn routing_timeout(&self) -> Duration {
        ROUTING_TIMEOUT.max(Duration::from_millis(2 * self.present_ms()) + Duration::from_secs(self.window_s))
    }

    /// Returns team B's URL, which names its recipient.
    fn team_b(&self) -> String {
        DemoPorts::http_url(self.ports.team_b)
    }

    /// Waits for ADP to serve its export and default program, and for the canary check to have a whole range of
    /// seconds behind it, and checks that the endpoints receive what the default program routes. Returns the
    /// default program as the editor shows it, and ADP's status.
    async fn baseline(&mut self) -> Result<(String, Status), GenericError> {
        let baseline = Instant::now();
        self.wait_for(
            "ADP's export and default program, and a balanced canary check",
            BASELINE_TIMEOUT,
            |state| {
                let control = &state["control"];
                if control["export_state"] != "available" {
                    return Err(format!("the export is {}", control["export_state"]));
                }
                if control["default_program_state"] != "available" {
                    return Err(format!(
                        "the default program is {} ({})",
                        control["default_program_state"], control["default_program_error"]
                    ));
                }
                match state["canary"]["state"].as_str() {
                    Some("balanced") => Ok(()),
                    other => Err(format!(
                        "the canary check is {:?}: {}",
                        other, state["canary"]["reason"]
                    )),
                }
            },
        )
        .await?;
        self.report.baseline_ms = Some(baseline.elapsed().as_millis() as u64);
        let program = self.client.get_json("/api/default-program").await?;
        let source = program["source"]
            .as_str()
            .ok_or_else(|| generic_error!("The default program is not available: {}", program))?
            .to_string();
        let boot = self.status().await?;
        self.expect_routing("baseline", &default_routing(), Instant::now())
            .await?;
        Ok((source, boot))
    }

    async fn scenarios(&mut self) -> Result<(), GenericError> {
        let (source, boot) = self.baseline().await?;

        // The default program as the editor loads it leaves out what does nothing as configured: it does what the
        // default program does, so the editor shows no change in what it does, and ADP installs it as a generation
        // that builds no kernel and makes no cut, after which the plan running is the program as published.
        let cuts_of = |state: &Value| lanes::CUT_OUTCOMES.map(|outcome| lanes::cuts(state, outcome));
        let cuts_before = cuts_of(&self.state().await?);
        self.step(
            "S0",
            "publish the unmodified default program",
            source.clone(),
            Compiled::Clean { unchanged: true },
            Installed::Active {
                new_generation: true,
                routing: Some(default_routing()),
            },
        )
        .await?;
        let cuts_after = cuts_of(&self.state().await?);
        if cuts_after != cuts_before {
            return Err(generic_error!(
                "S0: publishing the unmodified default program changed plan_cuts_total (completed, cancelled, \
                 not_armed) from {:?} to {:?}: it should be a commit.",
                cuts_before,
                cuts_after
            ));
        }
        self.check_routes(&boot, None).await?;
        self.check_program("S0", "published", "not_applied").await?;

        let s1 = edits::drop_debug(&source)?;
        let no_debug = ["primary", "team-b", "team-c"]
            .into_iter()
            .fold(default_routing(), |routing, intake| {
                changed(routing, intake, &["demo.debug.*"], true)
            });
        self.step(
            "S1",
            "drop demo.debug.*",
            s1,
            Compiled::Clean { unchanged: false },
            Installed::Active {
                new_generation: true,
                routing: Some(no_debug),
            },
        )
        .await?;
        let s1_status = self.status().await?;
        self.check_routes(&boot, Some(&s1_status)).await?;

        let recipe = edits::narrow_team_b(&source, &self.team_b())?;
        self.step(
            "S2",
            "the recipe: narrow team B to checkout",
            recipe.clone(),
            Compiled::Clean { unchanged: false },
            Installed::Active {
                new_generation: true,
                routing: Some(narrowed_routing()),
            },
        )
        .await?;

        self.step(
            "S3a",
            "widen team B's route with team:platform",
            edits::widen_team_b(&recipe)?,
            Compiled::Clean { unchanged: false },
            Installed::Active {
                new_generation: true,
                routing: Some(changed(
                    narrowed_routing(),
                    "team-b",
                    &["demo.platform.queue_depth"],
                    false,
                )),
            },
        )
        .await?;
        let exclusive = ["primary", "team-c"]
            .into_iter()
            .fold(default_routing(), |routing, intake| {
                changed(routing, intake, &CHECKOUT, true)
            });
        self.step(
            "S3b",
            "route team:checkout to team B only",
            edits::exclusive_team_b(&source, &self.team_b())?,
            Compiled::Clean { unchanged: false },
            Installed::Active {
                new_generation: true,
                routing: Some(exclusive),
            },
        )
        .await?;

        self.step(
            "S4a",
            "mirror only checkout requests on failover",
            edits::mirror_checkout_on_failover(&source)?,
            Compiled::Clean { unchanged: false },
            Installed::Active {
                new_generation: true,
                routing: Some(default_routing()),
            },
        )
        .await?;
        let mirrored = changed(default_routing(), "mrf", &[MIRRORED_METRIC], false);
        self.native_step(
            "S4b",
            "failover on (a signal): one family mirrored",
            json!({ "failover_metrics": true }),
            &mirrored,
        )
        .await?;
        self.native_step(
            "S4c",
            "failover off: mirroring stops",
            json!({ "failover_metrics": false }),
            &default_routing(),
        )
        .await?;

        let guardrails: [(_, _, _, &[&str]); 5] = [
            (
                "S5a",
                "guardrail: a duplicate branch to the main emit's recipients",
                edits::duplicate_branch(&source)?,
                &["duplicate_contribution"],
            ),
            (
                "S5b",
                "guardrail: an emit to recipients of two hosts",
                edits::two_hosts(&source)?,
                TWO_HOSTS,
            ),
            (
                "S5c",
                "guardrail: a second aggregation",
                edits::second_aggregation(&source)?,
                SECOND_AGGREGATION,
            ),
            (
                "S5d",
                "guardrail: the aggregation moved after enrichment",
                edits::aggregation_after_enrichment(&source)?,
                AGGREGATION_OUTSIDE_ITS_SLOT,
            ),
            (
                "S5e",
                "guardrail: a binding of a policy setting, which ADP does not declare",
                edits::policy_binding(&source)?,
                POLICY_BINDING,
            ),
        ];
        for (id, title, source, fragments) in guardrails {
            // The editor does not place plans, so it may not flag the aggregation moved after enrichment.
            let compiled = if fragments == AGGREGATION_OUTSIDE_ITS_SLOT {
                Compiled::Unplaced
            } else {
                Compiled::Flagged { fragment: fragments[0] }
            };
            self.step(id, title, source, compiled, Installed::Rejected { fragments })
                .await?;
        }
        self.step(
            "S5f",
            "guardrail: an emit to a recipient ADP does not know",
            edits::unknown_recipient(&source)?,
            Compiled::Waiting {
                fragment: UNKNOWN_RECIPIENT,
            },
            Installed::Pending {
                waits_for: UNKNOWN_RECIPIENT,
            },
        )
        .await?;
        // The previous generation keeps running: the endpoints still receive what S4 routes.
        self.expect_routing("S5", &default_routing(), Instant::now()).await?;

        self.revoke_step("S6", "revoke: an empty snapshot", Some(&boot), &default_routing())
            .await?;
        self.destination_steps(&recipe).await?;
        self.whole_run_canary().await
    }

    /// Checks that ADP's status says which program runs and whether native policy settings apply, as `runs` and
    /// `native_policy` say, and records it on the step `id`.
    async fn check_program(&mut self, id: &str, runs: &str, native_policy: &str) -> Result<(), GenericError> {
        let status = self.status().await?;
        if status.runs() != Some(runs) || status.native_policy() != Some(native_policy) {
            return Err(generic_error!(
                "{}: ADP's status says the program that runs is {:?}, with native policy {:?}, not {} and {}: {}",
                id,
                status.runs(),
                status.native_policy(),
                runs,
                native_policy,
                status.document["program"]
            ));
        }
        if let Some(step) = self.report.steps.iter_mut().rev().find(|step| step.id == id) {
            step.outcome = format!("{}; program {}, native policy {}", step.outcome, runs, native_policy);
        }
        Ok(())
    }

    /// Changes the native settings the demo writes in ADP's configuration file (`POST /api/native`), and checks that
    /// the endpoints then receive what `expected` says, with the artifact in effect unchanged.
    async fn native_step(
        &mut self, id: &'static str, title: &'static str, change: Value, expected: &Routing,
    ) -> Result<(), GenericError> {
        let mut step = StepReport::new(id, title);
        let result = async {
            let before = self.status().await?;
            let changed = Instant::now();
            let (status, answer) = self.client.post_json("/api/native", &change).await?;
            if !status.is_success() {
                return Err(generic_error!("Changing the native settings was refused: {}", answer));
            }
            self.last_change_s = unix_millis() / 1000;
            let visible = self.expect_routing(id, expected, changed).await?;
            let after = self.status().await?;
            if after.artifact_revision != before.artifact_revision {
                return Err(generic_error!(
                    "ADP runs artifact revision {:?} after the configuration change, not {:?}.",
                    after.artifact_revision,
                    before.artifact_revision
                ));
            }
            step.visible_ms = Some(visible.as_millis() as u64);
            step.generation = after.generation;
            step.outcome = format!(
                "configuration file: {}; {} still runs, generation {}",
                change,
                after.artifact_revision.map_or_else(
                    || "the default program".to_string(),
                    |revision| format!("revision {}", revision)
                ),
                after.generation.unwrap_or_default()
            );
            step.diff = "-".to_string();
            Ok(())
        }
        .await;
        self.finish(step, result)
    }

    /// Runs one scenario step that installs without a cut: compile, publish, acknowledgment, and what the endpoints
    /// receive.
    async fn step(
        &mut self, id: &'static str, title: &'static str, source: String, compiled: Compiled, installed: Installed,
    ) -> Result<(), GenericError> {
        self.step_in(POLICY_TARGET, id, title, source, compiled, installed)
            .await
            .map(|_| ())
    }

    /// Runs one scenario step, waiting as long as `target` says for its acknowledgment, and returns ADP's status once it
    /// settled.
    async fn step_in(
        &mut self, target: Target, id: &'static str, title: &'static str, source: String, compiled: Compiled,
        installed: Installed,
    ) -> Result<Status, GenericError> {
        let mut step = StepReport {
            id,
            title,
            ..StepReport::default()
        };
        let result = self.run_step(target, &mut step, source, compiled, installed).await;
        self.report.steps.push(step);
        result.with_error_context(|| format!("{} ({}) failed", id, title))
    }

    async fn run_step(
        &mut self, target: Target, step: &mut StepReport, source: String, compiled: Compiled, installed: Installed,
    ) -> Result<Status, GenericError> {
        let before = self.status().await?;
        let compile_started = Instant::now();
        let (status, outcome) = self
            .client
            .post_json("/api/compile", &json!({ "source": source }))
            .await?;
        step.compile_ms = Some(compile_started.elapsed().as_millis() as u64);
        if !status.is_success() || outcome["ok"] != true {
            return Err(generic_error!("The program did not compile: {}", outcome));
        }
        step.diff = diff_summary(&outcome["preview"]);
        // A program that no longer emits to a native output, such as the main output once a recipient leaves its
        // `emit`, leaves that output unused: a warning that says so, not a finding against the program.
        let diagnostics = outcome["diagnostics"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|diagnostic| !unused_output(diagnostic))
            .cloned()
            .collect::<Vec<_>>();
        let messages = || {
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic["message"].as_str().unwrap_or_default().to_string())
                .collect::<Vec<_>>()
                .join(" | ")
        };
        if outcome["checked_against"]["composed"] != true {
            return Err(generic_error!(
                "The editor did not compose the artifact into ADP's default program: {}",
                outcome["checked_against"]
            ));
        }
        // Whether the program emits to recipients no output delivers to yet, for which ADP adds an implied output: the
        // editor then has no plan diff to show until ADP runs the program.
        let implied = !diagnostics.is_empty()
            && diagnostics.iter().all(|diagnostic| {
                diagnostic["severity"] == "warning"
                    && diagnostic["message"]
                        .as_str()
                        .is_some_and(|message| message.starts_with(IMPLIED_OUTPUT))
            });
        match compiled {
            Compiled::Clean { .. } if implied => {}
            Compiled::Clean { unchanged } => {
                if !diagnostics.is_empty() {
                    return Err(generic_error!("The editor reported findings: {}", messages()));
                }
                let preview = &outcome["preview"];
                if preview["available"] != true || preview["valid"] != true {
                    return Err(generic_error!("The editor has no valid plan diff: {}", preview));
                }
                if preview["unchanged"] != unchanged {
                    return Err(generic_error!(
                        "The editor's plan diff is {}, but the scenario {}: {}",
                        if unchanged { "not empty" } else { "empty" },
                        if unchanged {
                            "publishes the unmodified default program"
                        } else {
                            "changes the plan"
                        },
                        preview
                    ));
                }
            }
            // The editor validates without placing the plan: what it finds does not decide the step.
            Compiled::Unplaced => {}
            Compiled::Waiting { fragment } => {
                let waits = diagnostics.iter().any(|diagnostic| {
                    diagnostic["severity"] == "warning"
                        && diagnostic["message"]
                            .as_str()
                            .is_some_and(|message| message.contains(fragment))
                });
                if !waits || outcome["preview"]["available"] == true {
                    return Err(generic_error!(
                        "The editor did not say that ADP holds the program pending for `{}`: {}",
                        fragment,
                        messages()
                    ));
                }
            }
            Compiled::Flagged { fragment } => {
                let flagged = diagnostics.iter().any(|diagnostic| {
                    diagnostic["severity"] == "error"
                        && (diagnostic["message"]
                            .as_str()
                            .is_some_and(|message| message.contains(fragment))
                            || diagnostic["rule"] == fragment)
                });
                if !flagged {
                    return Err(generic_error!(
                        "The editor did not flag `{}` as an error: {}",
                        fragment,
                        messages()
                    ));
                }
            }
        }

        let hash = outcome["artifact_hash"].as_str().unwrap_or_default().to_string();
        let published = Instant::now();
        let (status, answer) = self
            .client
            .post_json("/api/publish", &json!({ "artifact_hash": hash }))
            .await?;
        if !status.is_success() {
            return Err(generic_error!("Publishing was refused: {}", answer));
        }
        let revision = answer["revision"]
            .as_u64()
            .ok_or_else(|| generic_error!("Publishing answered without a revision: {}", answer))?;
        self.last_change_s = unix_millis() / 1000;
        step.revision = Some(revision);

        let waits_for = match &installed {
            Installed::Pending { waits_for } => Some(*waits_for),
            Installed::Active { .. } | Installed::Rejected { .. } => None,
        };
        let expected = |acknowledgment: &Value| match waits_for {
            Some(what) => waiting_for(acknowledgment, what),
            None => settled(acknowledgment),
        };
        let (acknowledgment, after) = self
            .wait_ack_within(POLICY_PRODUCT, revision, published, target.ack_timeout, expected)
            .await?;
        step.ack_ms = Some(published.elapsed().as_millis() as u64);
        let artifact_revision = |status: &Status| status.artifact_revision;
        let state = acknowledgment["state"].as_str().unwrap_or_default().to_string();
        let error = acknowledgment["error"].as_str().unwrap_or_default().to_string();
        step.generation = after.generation;
        match installed {
            Installed::Active {
                new_generation,
                routing,
            } => {
                if state != "active" {
                    return Err(generic_error!(
                        "ADP acknowledged revision {} as `{}`: {}",
                        revision,
                        state,
                        error
                    ));
                }
                if artifact_revision(&after) != Some(revision) {
                    return Err(generic_error!(
                        "ADP reports artifact revision {:?} active, not {}.",
                        artifact_revision(&after),
                        revision
                    ));
                }
                // Without a diff (implied outputs ADP adds), or when ADP lowered again once the program was in effect
                // (retiring the implied outputs only the program it replaced asked for), the editor composes the
                // program again against what ADP serves now, which must be the plan ADP runs.
                let composed = outcome["preview"]["plan_hash"].as_str().filter(|_| !implied);
                let implied_note = if implied || after.plan_hash.as_deref() != composed {
                    Some(
                        self.compose_again(&source, &after, implied)
                            .await
                            .with_error_context(|| {
                                format!(
                                    "ADP's active plan is {:?}, and the editor composed {:?}",
                                    after.plan_hash, composed
                                )
                            })?,
                    )
                } else {
                    None
                };
                match (new_generation, after.generation > before.generation) {
                    (false, true) => {
                        return Err(generic_error!(
                            "ADP installed revision {} as a new generation {:?}, but it is the plan already running.",
                            revision,
                            after.generation
                        ))
                    }
                    (true, false) => {
                        return Err(generic_error!(
                            "ADP acknowledged revision {} without a new generation (still {:?}).",
                            revision,
                            after.generation
                        ))
                    }
                    _ => {}
                }
                step.outcome = format!(
                    "active in generation {}{}",
                    after.generation.unwrap_or_default(),
                    if new_generation { "" } else { " (no new generation)" }
                );
                if let Some(note) = implied_note {
                    step.outcome = format!("{}; {}", step.outcome, note);
                    if implied {
                        step.diff = "none before ADP adds the implied outputs".to_string();
                    }
                }
                if let Some(routing) = routing {
                    let visible = self.expect_routing(step.id, &routing, published).await?;
                    step.visible_ms = Some(visible.as_millis() as u64);
                }
            }
            Installed::Rejected { fragments } => {
                if state != "rejected" {
                    return Err(generic_error!(
                        "ADP acknowledged revision {} as `{}`, not `rejected`.",
                        revision,
                        state
                    ));
                }
                if !fragments.iter().any(|fragment| error.contains(fragment)) {
                    return Err(generic_error!(
                        "ADP rejected revision {}, but its error does not say `{}`: {}",
                        revision,
                        fragments.join("` or `"),
                        error
                    ));
                }
                if (after.generation, artifact_revision(&after)) != (before.generation, artifact_revision(&before)) {
                    return Err(generic_error!(
                        "After rejecting revision {}, ADP runs generation {:?} (artifact {:?}), not generation {:?} \
                         (artifact {:?}) as before.",
                        revision,
                        after.generation,
                        artifact_revision(&after),
                        before.generation,
                        artifact_revision(&before)
                    ));
                }
                step.outcome = format!(
                    "rejected, generation {} kept: {}",
                    after.generation.unwrap_or_default(),
                    shorten(&error, 100)
                );
            }
            Installed::Pending { .. } => {
                if (after.generation, artifact_revision(&after)) != (before.generation, artifact_revision(&before)) {
                    return Err(generic_error!(
                        "While revision {} waits, ADP runs generation {:?} (artifact {:?}), not generation {:?} \
                         (artifact {:?}) as before.",
                        revision,
                        after.generation,
                        artifact_revision(&after),
                        before.generation,
                        artifact_revision(&before)
                    ));
                }
                step.outcome = format!(
                    "pending, generation {} kept: {}",
                    after.generation.unwrap_or_default(),
                    shorten(&error, 100)
                );
            }
        }
        Ok(after)
    }

    /// Compiles `source` again once ADP runs it (as the artifact revision `after` names), and checks that the editor
    /// now composes it without findings to the plan ADP runs: with the implied outputs ADP added for it, if `implied`,
    /// or without those that only the program it replaced asked for, which ADP retired, lowering again (a generation
    /// after `after`'s). Returns what it found, in a few words.
    async fn compose_again(&mut self, source: &str, after: &Status, implied: bool) -> Result<String, GenericError> {
        let deadline = Instant::now() + ACK_TIMEOUT;
        loop {
            let running = self.status().await?;
            if running.artifact_revision != after.artifact_revision {
                return Err(generic_error!(
                    "ADP runs artifact revision {:?}, not {:?}, while the program is compiled again.",
                    running.artifact_revision,
                    after.artifact_revision
                ));
            }
            let (status, outcome) = self
                .client
                .post_json("/api/compile", &json!({ "source": source }))
                .await?;
            let preview = &outcome["preview"];
            let composed = preview["plan_hash"].as_str();
            let clean = status.is_success()
                && outcome["diagnostics"]
                    .as_array()
                    .is_some_and(|diagnostics| diagnostics.iter().all(unused_output))
                && preview["valid"] == true
                && preview["unchanged"] == true;
            if clean && composed.is_some() && composed == running.plan_hash.as_deref() {
                break;
            }
            if Instant::now() >= deadline {
                return Err(generic_error!(
                    "Compiled again once ADP runs it, the program does not compose to ADP's active plan {:?} without \
                     findings: {:?}, diagnostics {}, preview {}",
                    running.plan_hash,
                    composed,
                    outcome["diagnostics"],
                    preview
                ));
            }
            tokio::time::sleep(POLL).await;
        }
        if !implied {
            return Ok(
                "ADP lowered again, retiring implied outputs; compiled again, it composes to the plan ADP runs"
                    .to_string(),
            );
        }
        let export = self.client.get_json("/api/adp/export").await?;
        let implied = export["catalog"]["outputs"]
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(id, _)| id.starts_with("implied_"))
            .map(|(id, output)| {
                format!(
                    "{} ({} recipients)",
                    id,
                    output["recipients"].as_array().map_or(0, Vec::len)
                )
            })
            .collect::<Vec<_>>();
        if implied.is_empty() {
            return Err(generic_error!(
                "ADP's export declares no implied output, though the program emits to recipients no other output \
                 delivers to exactly."
            ));
        }
        Ok(format!(
            "implied outputs {}; compiled again, it composes to the plan ADP runs",
            implied.join(", ")
        ))
    }

    /// Revokes the artifact (an empty snapshot), and checks that the default program comes back, the boot plan again if
    /// `boot` is given, with what the endpoints then receive, `expected`. The default program runs in a new generation,
    /// or adopts the one running when that one does what the default program does: plans are compared with their
    /// bindings written as values, so a published program whose only change is a split that the signals' values decide
    /// as the default program's does, such as a failover gateway while failover is off, is the same operation.
    async fn revoke_step(
        &mut self, id: &'static str, title: &'static str, boot: Option<&Status>, expected: &Routing,
    ) -> Result<(), GenericError> {
        let mut step = StepReport::new(id, title);
        let result = async {
            let before = self.status().await?;
            let revoked = Instant::now();
            let (status, answer) = self.client.post_json("/api/revoke", &json!({})).await?;
            if !status.is_success() || answer["revoked"] != true {
                return Err(generic_error!("Revoking was refused: {}", answer));
            }
            self.last_change_s = unix_millis() / 1000;
            let boot_plan = boot.and_then(|boot| boot.plan_hash.clone());
            let deadline = revoked + ACK_TIMEOUT;
            loop {
                let status = self.status().await?;
                let default_program = status.artifact_revision.is_none() && status.runs() == Some("default");
                if default_program && (boot_plan.is_none() || status.plan_hash == boot_plan) {
                    break;
                }
                if Instant::now() >= deadline {
                    return Err(generic_error!(
                        "ADP still runs artifact revision {:?} (plan {:?}, generation {:?}, program {}) {} s after the \
                         revoke, not the default program{}.",
                        status.artifact_revision,
                        status.plan_hash,
                        status.generation,
                        status.document["program"],
                        ACK_TIMEOUT.as_secs(),
                        boot_plan
                            .as_ref()
                            .map(|plan| format!(" (plan {})", plan))
                            .unwrap_or_default()
                    ));
                }
                tokio::time::sleep(POLL).await;
            }
            step.ack_ms = Some(revoked.elapsed().as_millis() as u64);
            // The stub's record of ADP's polls says so too, with the plan the status route reports: ADP may lower once
            // more, after the revoked program's implied outputs retire.
            self.wait_for("ADP's polls to report the default program", ACK_TIMEOUT, |state| {
                let polled = &state["channel"]["last_poll"]["status"];
                let reported = &state["control"]["status"];
                if polled["active_artifact"].is_null()
                    && polled["active_plan_hash"].is_string()
                    && polled["active_plan_hash"] == reported["active_plan_hash"]
                {
                    Ok(())
                } else {
                    Err(format!("the last poll reports {}", polled))
                }
            })
            .await?;
            let after = self.status().await?;
            step.generation = after.generation;
            step.outcome = format!(
                "default program {} generation {}{}; native policy {}",
                if after.generation > before.generation {
                    "in"
                } else {
                    "adopted the running"
                },
                after.generation.unwrap_or_default(),
                if boot.is_some() { ", the boot plan" } else { "" },
                after.native_policy().unwrap_or("not reported")
            );
            step.diff = "-".to_string();
            let visible = self.expect_routing(id, expected, revoked).await?;
            step.visible_ms = Some(visible.as_millis() as u64);
            Ok(())
        }
        .await;
        self.finish(step, result)
    }

    /// Reads the control surface routes through the demo and checks them against ADP's status: the export and the
    /// default program after boot (`active` is `None`), and the active plan and its explain document while an artifact
    /// is active.
    async fn check_routes(&mut self, boot: &Status, active: Option<&Status>) -> Result<(), GenericError> {
        let Some(active) = active else {
            let export = self.client.get_json("/api/adp/export").await?;
            let program_hash = crate::export::ExportBundle::from_value(export)
                .map_err(|error| generic_error!("ADP's export cannot be read: {}", error))?
                .program_hash
                .to_string();
            if Some(program_hash.as_str()) != boot.default_program_hash.as_deref() {
                return Err(generic_error!(
                    "The export's program hash is {}, but ADP's status reports the default program {:?}.",
                    program_hash,
                    boot.default_program_hash
                ));
            }
            self.report.routes.push(RouteReport {
                route: "/api/adp/export",
                checked: "the program hash of its artifact is the status's default program hash".to_string(),
            });
            let default_program = plan_hash(self.client.get_json("/api/adp/default-program").await?)?;
            if Some(&default_program) != boot.plan_hash.as_ref() {
                return Err(generic_error!(
                    "The default program hashes to {}, but the boot generation runs {:?}.",
                    default_program,
                    boot.plan_hash
                ));
            }
            self.report.routes.push(RouteReport {
                route: "/api/adp/default-program",
                checked: "it hashes to the boot generation's plan".to_string(),
            });
            return Ok(());
        };

        let plan = self.client.get_json("/api/adp/plan").await?;
        let hash = plan_hash(plan.clone())?;
        if Some(&hash) != active.plan_hash.as_ref() {
            return Err(generic_error!(
                "The active plan hashes to {}, but ADP's status reports {:?}.",
                hash,
                active.plan_hash
            ));
        }
        // S1's `where` is not named, so its node is known by what it is: the `split` on `demo.debug.`.
        let drop_debug = plan["nodes"]
            .as_array()
            .and_then(|nodes| nodes.iter().find(|node| edits::is_drop_debug(node)))
            .and_then(|node| node["id"].as_str())
            .map(str::to_string)
            .ok_or_else(|| generic_error!("The active plan has no node for S1's `{}`.", edits::DROP_DEBUG))?;
        self.report.routes.push(RouteReport {
            route: "/api/adp/plan",
            checked: format!(
                "it hashes to the status's active plan hash and has S1's node `{}`",
                drop_debug
            ),
        });
        let explain = self.client.get_json("/api/adp/explain").await?;
        if explain["plan"]["hash"].as_str() != active.plan_hash.as_deref() {
            return Err(generic_error!(
                "The explain document describes plan {}, not the active plan {:?}.",
                explain["plan"]["hash"],
                active.plan_hash
            ));
        }
        // The node is named `metrics`, a word every plan's text has, so the document must show the `split` itself.
        if !explain["graph"]["nodes"].as_array().is_some_and(|nodes| {
            nodes
                .iter()
                .any(|node| node["id"] == drop_debug && edits::is_drop_debug(node))
        }) {
            return Err(generic_error!(
                "The explain document has no `split` node `{}` on `demo.debug.`.",
                drop_debug
            ));
        }
        let (status, text) = self.client.get("/api/adp/explain-text").await?;
        let text = String::from_utf8_lossy(&text);
        if !status.is_success() || !text.contains("demo.debug.") {
            return Err(generic_error!(
                "The explain text ({}) does not describe S1's `split` on `demo.debug.`.",
                status
            ));
        }
        self.report.routes.push(RouteReport {
            route: "/api/adp/explain, /api/adp/explain-text",
            checked: format!(
                "both describe the active plan, with S1's `split` node `{}` on `demo.debug.`",
                drop_debug
            ),
        });
        Ok(())
    }

    /// Waits until the rolling canary check covers the last change, then checks the canaries of the whole run.
    async fn whole_run_canary(&mut self) -> Result<(), GenericError> {
        let last_change_s = self.last_change_s;
        let last_range = self
            .wait_for(
                "the canary check to cover the last change",
                BASELINE_TIMEOUT,
                |state| match state["canary"]["to_s"].as_u64() {
                    Some(to_s) if to_s > last_change_s + 2 => Ok(to_s),
                    other => Err(format!("its range ends at {:?}", other)),
                },
            )
            .await?;
        let to_s = self
            .whole_run
            .as_ref()
            .ok_or_else(|| generic_error!("No canary range was balanced."))?
            .to_s;
        if last_range > to_s {
            // While the check waits for a scrape of ADP's telemetry, read the range again; past the deadline, add it as
            // it is, waiting, which the whole run's check reports.
            let deadline = Instant::now() + ROUTING_TIMEOUT;
            while !self.add_canary_range((to_s, last_range)).await? {
                if Instant::now() >= deadline {
                    tracing::warn!(
                        "The canary check of seconds {} to {} still waits for a scrape of ADP's telemetry after {} s; \
                         adding it as it is.",
                        to_s,
                        last_range,
                        ROUTING_TIMEOUT.as_secs()
                    );
                    self.add_canary_range_as_it_is((to_s, last_range)).await?;
                    break;
                }
                tokio::time::sleep(POLL).await;
            }
        }
        self.report.canary = self.whole_run.as_ref().map(CanaryTotals::report);
        Ok(())
    }

    /// Reads what the demo's canary check says of `range`, and adds it to the whole run's totals, unless the check
    /// waits for a scrape of ADP's telemetry, so that the range read later can say more. Returns `true` if the range
    /// was added.
    async fn add_canary_range(&mut self, range: (u64, u64)) -> Result<bool, GenericError> {
        let check = self.read_canary_range(range).await?;
        if check["pending_telemetry"] == true {
            return Ok(false);
        }
        if check["state"] == CanaryState::Waiting.name() {
            tracing::warn!(
                "The canary check of seconds {} to {}, added to the whole run's, waits: {}",
                range.0,
                range.1,
                check["reason"].as_str().unwrap_or_default()
            );
        }
        self.add_canary_check(range, &check)?;
        Ok(true)
    }

    /// Reads what the demo's canary check says of `range`, and adds it to the whole run's totals, whatever it says.
    async fn add_canary_range_as_it_is(&mut self, range: (u64, u64)) -> Result<(), GenericError> {
        let check = self.read_canary_range(range).await?;
        self.add_canary_check(range, &check)
    }

    /// Reads what the demo's canary check says of `range` (`GET /api/canary`).
    async fn read_canary_range(&mut self, range: (u64, u64)) -> Result<Value, GenericError> {
        self.client
            .get_json(&format!("/api/canary?from_s={}&to_s={}", range.0, range.1))
            .await
    }

    /// Adds what the demo's canary check said of `range` to the whole run's totals.
    fn add_canary_check(&mut self, range: (u64, u64), check: &Value) -> Result<(), GenericError> {
        match self.whole_run.as_mut() {
            Some(totals) => totals.add(range, check),
            None => Err(generic_error!("No canary range was balanced.")),
        }
    }

    /// Returns ADP's status, read through the demo's pass-through of the privileged status route, after sampling the
    /// demo's state if the runner has not for [`SAMPLE_INTERVAL`].
    async fn status(&mut self) -> Result<Status, GenericError> {
        if self.sampled.is_none_or(|sampled| sampled.elapsed() >= SAMPLE_INTERVAL) {
            self.state().await?;
        }
        Ok(Status::read(self.client.get_json("/api/adp/status").await?))
    }

    /// Waits for ADP to acknowledge `revision` of `product` as `expected` says (see [`settled`] and [`waiting_for`])
    /// on the privileged status route and in its polls of the stub, and returns the acknowledgment with ADP's status.
    async fn wait_ack(
        &mut self, product: &str, revision: u64, since: Instant, expected: impl Fn(&Value) -> bool,
    ) -> Result<(Value, Status), GenericError> {
        self.wait_ack_within(product, revision, since, ACK_TIMEOUT, expected)
            .await
    }

    /// Waits as [`Runner::wait_ack`] does, for up to `timeout` from `since` for the acknowledgment on the status route.
    async fn wait_ack_within(
        &mut self, product: &str, revision: u64, since: Instant, timeout: Duration, expected: impl Fn(&Value) -> bool,
    ) -> Result<(Value, Status), GenericError> {
        let deadline = since + timeout;
        let (acknowledgment, status) = loop {
            let status = self.status().await?;
            if let Some(acknowledgment) = status.acknowledgment(product, revision) {
                if expected(acknowledgment) {
                    break (acknowledgment.clone(), status);
                }
            }
            if Instant::now() >= deadline {
                return Err(generic_error!(
                    "ADP did not acknowledge {} revision {} as expected within {} s; its status: {}",
                    product,
                    revision,
                    timeout.as_secs(),
                    status.document
                ));
            }
            tokio::time::sleep(POLL).await;
        };
        let acknowledged = acknowledgment["state"].clone();
        self.wait_for(
            "ADP's polls of the stub to report the acknowledgment",
            ACK_TIMEOUT,
            |state| {
                let states = &state["channel"]["last_poll"]["config_states"];
                let polled = states
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|state| state["revision"].as_u64() == Some(revision) && product_of(state) == product);
                match polled {
                    Some(polled) if polled["state"] == acknowledged => Ok(()),
                    other => Err(format!("the last poll reports {:?}", other)),
                }
            },
        )
        .await?;
        Ok((acknowledgment, status))
    }

    /// Waits until the endpoints receive what `expected` says, and keep doing so for [`Runner::stable`]. Returns the
    /// time from `since` until they first did.
    async fn expect_routing(
        &mut self, what: &str, expected: &Routing, since: Instant,
    ) -> Result<Duration, GenericError> {
        let timeout = self.routing_timeout();
        let deadline = Instant::now() + timeout;
        let stable = self.stable();
        let mut reached: Option<Duration> = None;
        let mut held_since: Option<Instant> = None;
        let mut flapped = 0;
        loop {
            let state = self.state().await?;
            let found = present(&state, self.present_ms());
            let holds = routing_holds(expected, &found);
            match (holds, held_since) {
                (true, None) => {
                    reached.get_or_insert_with(|| since.elapsed());
                    held_since = Some(Instant::now());
                }
                (true, Some(start)) if start.elapsed() >= stable => {
                    return Ok(reached.unwrap_or_default());
                }
                (true, Some(_)) => {}
                (false, Some(_)) => {
                    flapped += 1;
                    held_since = None;
                }
                (false, None) => {}
            }
            if Instant::now() >= deadline {
                return Err(generic_error!(
                    "{}: the endpoints did not receive what the scenario says within {} s ({} times they did, then \
                     stopped): {}",
                    what,
                    timeout.as_secs(),
                    flapped,
                    routing_difference(expected, &found)
                ));
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// Samples the demo's state until `condition` holds, returning what it returns. `condition` says why it does not
    /// hold yet, for the error when it never does.
    async fn wait_for<T>(
        &mut self, what: &str, timeout: Duration, mut condition: impl FnMut(&Value) -> Result<T, String>,
    ) -> Result<T, GenericError> {
        let deadline = Instant::now() + timeout;
        loop {
            let state = self.state().await?;
            let why = match condition(&state) {
                Ok(value) => return Ok(value),
                Err(why) => why,
            };
            if Instant::now() >= deadline {
                return Err(generic_error!(
                    "Timed out after {} s waiting for {}: {}",
                    timeout.as_secs(),
                    what,
                    why
                ));
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// Returns the demo's state, after checking the canary conservation, the intakes, and the lanes in it, and adding
    /// to the whole run's canary totals once a range is due.
    async fn state(&mut self) -> Result<Value, GenericError> {
        let state = self.client.get_json("/api/state").await?;
        self.sampled = Some(Instant::now());
        self.observe(&state);
        // The rolling check's range ends at a boundary of every bucket width in use, unless no width is aligned to.
        let aligned_to_s = state["canary_width_secs"]
            .as_u64()
            .and(state["canary"]["to_s"].as_u64());
        let due = self
            .whole_run
            .as_ref()
            .zip(aligned_to_s)
            .and_then(|(totals, rolling_to_s)| totals.due(rolling_to_s));
        if let Some(range) = due {
            // A range the check cannot say yet waits for a later sample, which reads it to a later end.
            self.add_canary_range(range).await?;
        }
        Ok(state)
    }

    /// Records the canary check, the intakes' problems, and lane events admitted by a lane that does not own their key
    /// in a sample of the demo's state.
    fn observe(&mut self, state: &Value) {
        let canary = &state["canary"];
        let range = (
            canary["from_s"].as_u64().unwrap_or_default(),
            canary["to_s"].as_u64().unwrap_or_default(),
        );
        let host_dropping = canary["state"] == CanaryState::HostDropped.name();
        match canary["state"].as_str() {
            Some("balanced") => {
                self.whole_run.get_or_insert_with(|| CanaryTotals::starting_at(range.0));
                self.report.canary_samples += 1;
            }
            Some("host_dropped") => {
                self.report.host_dropped_samples += 1;
                if !self.host_dropping {
                    tracing::warn!(
                        "canaries short for seconds {} to {}, which the host dropped: {} sent, {} received at the \
                         primary endpoint. {}",
                        range.0,
                        range.1,
                        canary["sent"],
                        canary["received"],
                        canary["reason"].as_str().unwrap_or_default()
                    );
                }
            }
            Some(problem @ ("missing" | "excess")) => {
                let reason = canary["reason"]
                    .as_str()
                    .map(|reason| format!(" ({})", reason))
                    .unwrap_or_default();
                self.alert(
                    format!("canary {} {}-{}", problem, range.0, range.1),
                    format!(
                        "canaries {} for seconds {} to {}: {} sent, {} received at the primary endpoint{}",
                        problem, range.0, range.1, canary["sent"], canary["received"], reason
                    ),
                );
            }
            _ => {}
        }
        self.host_dropping = host_dropping;
        // The other endpoints a program sends everything to receive the canaries too, from where their routing starts
        // within the range: never more than were sent, which would be a canary delivered twice.
        let most = canary["sent"].as_f64().unwrap_or_default() + canary["allowance"].as_f64().unwrap_or_default();
        for elsewhere in canary["elsewhere"].as_array().into_iter().flatten() {
            let received = elsewhere["received"].as_f64().unwrap_or_default();
            if received > most {
                self.alert(
                    format!("canary excess at {} {}-{}", elsewhere["intake"], range.0, range.1),
                    format!(
                        "{} received {} canaries for seconds {} to {}, more than the {} sent",
                        elsewhere["intake"], received, range.0, range.1, canary["sent"]
                    ),
                );
            }
        }
        for intake in state["intakes"].as_array().into_iter().flatten() {
            let name = intake["name"].as_str().unwrap_or_default();
            if intake["refused"].as_u64().unwrap_or_default() > 0 {
                self.alert(
                    format!("{} refused", name),
                    format!("{} refused a request: {}", name, intake["last_refusal"]),
                );
            }
            let unexpected = intake["api_keys"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|key| key["expected"] != true)
                .map(|key| key["key"].to_string())
                .collect::<Vec<_>>();
            if !unexpected.is_empty() || intake["other_api_key_requests"].as_u64().unwrap_or_default() > 0 {
                self.alert(
                    format!("{} keys", name),
                    format!(
                        "{} received requests with unexpected API keys: {}",
                        name,
                        unexpected.join(", ")
                    ),
                );
            }
        }
        // An event a lane admitted whose key another lane owns: the partitioning is wrong, whatever else holds.
        for entry in state["telemetry"]["plan"].as_array().into_iter().flatten() {
            let Some(misrouted) = entry["value"].as_f64().filter(|value| *value > 0.0) else {
                continue;
            };
            if entry["name"] != "plan_lane_misrouted_total" {
                continue;
            }
            let component = entry["labels"]["component_id"].as_str().unwrap_or_default();
            let lane = entry["labels"]["lane"].as_str().unwrap_or_default();
            self.alert(
                format!("misrouted {} {}", component, lane),
                format!(
                    "lane {} of {} admitted {} events whose key another lane owns (plan_lane_misrouted_total)",
                    lane, component, misrouted
                ),
            );
        }
    }

    fn alert(&mut self, key: String, message: String) {
        if self.alerted.insert(key) {
            tracing::warn!("{}", message);
            self.report.violations.push(message);
        }
    }
}

/// Returns `true` if a diagnostic of the editor is the warning that a native output is left unused: no `emit` of the
/// composed plan delivers to it.
fn unused_output(diagnostic: &Value) -> bool {
    diagnostic["severity"] == "warning" && diagnostic["rule"] == "unused_output"
}

/// Returns the hash of a plan document, with resolved values.
fn plan_hash(document: Value) -> Result<String, GenericError> {
    let plan = Plan::from_value(document).error_context("The route did not serve a plan.")?;
    Ok(plan
        .hash(ResolvedValues::Include)
        .error_context("The plan cannot be hashed.")?
        .to_string())
}

/// Summarizes the plan diff preview of a compile in a few words.
fn diff_summary(preview: &Value) -> String {
    if preview["available"] != true {
        return "no plan diff".to_string();
    }
    if preview["unchanged"] == true {
        return "no change".to_string();
    }
    let nodes = preview["nodes"].as_array().cloned().unwrap_or_default();
    let count = |change: &str| nodes.iter().filter(|node| node["change"] == change).count();
    format!(
        "{} added, {} changed, {} removed, {} edges added, {} removed",
        count("added"),
        count("changed"),
        count("removed"),
        preview["edges_added"].as_array().map_or(0, Vec::len),
        preview["edges_removed"].as_array().map_or(0, Vec::len)
    )
}

/// Shortens a message to `limit` characters, for the summary.
fn shorten(message: &str, limit: usize) -> String {
    if message.chars().count() <= limit {
        return message.to_string();
    }
    let mut short = message.chars().take(limit).collect::<String>();
    short.push('…');
    short
}

#[cfg(test)]
pub(crate) mod tests;
