//! The scripted check of the demo (`programmable-metrics-demo check`, `make demo-programmable-metrics-check`): the
//! scenarios S0 to S6 of the implementation design's Demo section, driven through the demo server's API against the
//! ADP the demo launched, with what each endpoint receives checked after every step.
//!
//! Every scenario edits the default program as ADP exports it (see [`edits`]), compiles it (`POST /api/compile`, which
//! composes it into ADP's default program, validates it, and diffs it against the active plan), publishes it (`POST
//! /api/publish`), waits for ADP's acknowledgment (`GET /api/adp/status`, and the stub's record of ADP's polls), and
//! then waits for the families each intake receives to be what the scenario says, and to stay so for [`STABLE`]:
//!
//! - **S0**: the unmodified default program is acknowledged `active` without a new generation, its plan diff is empty,
//!   and every endpoint receives what it does natively: the primary endpoint everything, team B `demo.checkout.*`,
//!   team C `demo.catalog.*`, the failover endpoint nothing.
//! - **S1**: `demo.debug.*` is dropped: it leaves the primary endpoint, and the other families continue.
//! - **S2**: team B's route widened with `or tag("team") == "platform"`: team B also receives the platform family.
//! - **S3**: exclusive tag routing: the metrics tagged `team:checkout` reach team B only, not the primary endpoint.
//! - **S4**: mirroring with `true` and a literal allowlist: the failover endpoint receives the allowlisted family only.
//! - **S5**: guardrails, each published anyway: a duplicate `tee` branch to the primary output, an emit to an output
//!   ADP does not declare, and an aggregation without its parameters, each flagged by the editor, and a fully
//!   parameterized aggregation in the policy scope, which the frame cannot run there (the editor does not place plans,
//!   so it may not flag it), are each rejected by ADP with an error that says so, and S4's generation keeps running,
//!   endpoints unchanged.
//! - **S6**: revoking (an empty snapshot) brings the default program back: the active plan is the boot plan again, and
//!   the endpoints receive what they did in S0.
//!
//! Each step after S0 also checks that the active plan hash ADP reports is the hash of the plan the editor composed, so
//! the editor's preview is what ADP installs. Throughout, every sample of the demo's state checks the canary
//! conservation of the moment (the canaries the primary endpoint received for a recent range of seconds, against those
//! sent), that no other endpoint receives canaries, and that no intake refused a request or saw an unexpected API key;
//! at the end, the canaries of the whole run, from the first balanced range to the last, are checked at once. Every
//! install is in the ranges checked, but a check balances within an allowance for canaries counted in the second next
//! to its range's edges (6 at the default 20 datagrams per second): an aggregated canary point lost or delivered twice
//! (20 canaries at that rate, with the demo's 1-second windows) fails it, while a few datagrams lost or doubled ahead
//! of aggregation do not. ADP's continuous-input tests (`installer::lifecycle_tests`) check that no point is lost or
//! duplicated across installs, exactly.
//!
//! The steps are timed: compile, publish to acknowledgment, and publish to the endpoints showing the change. The
//! report ([`Report`]) is written to `check-report.json` in the run directory.

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
    edits::{Outputs, MIRRORED_METRIC, UNDECLARED_OUTPUT},
};
use crate::{clock::unix_millis, settings::RunSettings};

pub mod client;
mod edits;

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

/// The metric families the traffic generator sends.
const FAMILIES: [&str; 6] = [
    "demo.canary",
    "demo.catalog.views",
    "demo.checkout.latency",
    "demo.checkout.requests",
    "demo.debug.*",
    "demo.platform.queue_depth",
];

/// The checkout families, which team B's route matches by prefix.
const CHECKOUT: [&str; 2] = ["demo.checkout.latency", "demo.checkout.requests"];

/// The intakes, as the demo names them.
const INTAKES: [&str; 4] = ["primary", "team-b", "team-c", "mrf"];

/// The metric families each intake receives, by intake name.
type Routing = BTreeMap<String, BTreeSet<String>>;

/// Returns the routing of the default program: everything to the primary endpoint, the checkout families to team B,
/// the catalog family to team C, nothing to the failover endpoint.
fn default_routing() -> Routing {
    routing(&[
        ("primary", &FAMILIES),
        ("team-b", &CHECKOUT),
        ("team-c", &["demo.catalog.views"]),
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

/// Returns the families each intake receives now, according to the demo's state: the `demo.` families seen within
/// [`PRESENT_MS`].
fn present(state: &Value) -> Routing {
    let mut found = Routing::new();
    for intake in state["intakes"].as_array().into_iter().flatten() {
        let name = intake["name"].as_str().unwrap_or_default().to_string();
        let families = intake["families"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|family| family["age_ms"].as_u64().is_some_and(|age| age <= PRESENT_MS))
            .filter_map(|family| family["family"].as_str())
            .filter(|family| family.starts_with("demo."))
            .map(ToString::to_string)
            .collect();
        found.insert(name, families);
    }
    found
}

/// Describes how `found` differs from `expected`, intake by intake.
fn routing_difference(expected: &Routing, found: &Routing) -> String {
    let empty = BTreeSet::new();
    INTAKES
        .iter()
        .filter_map(|intake| {
            let want = expected.get(*intake).unwrap_or(&empty);
            let have = found.get(*intake).unwrap_or(&empty);
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

    /// The check's state: `balanced`, `missing`, or `excess`.
    pub state: String,
}

/// The report of a run.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Report {
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

    /// Problems seen while sampling: canaries missing, doubled, or at another endpoint; refused requests; unexpected
    /// API keys.
    pub violations: Vec<String>,

    /// Why the run stopped, if a step failed.
    pub error: Option<String>,
}

impl Report {
    /// Returns why the check failed, if it did.
    pub fn failure(&self) -> Option<String> {
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
            Some(canary) if canary.state != "balanced" => Some(format!(
                "The whole-run canary check is {}: {} sent, {} received at the primary endpoint.",
                canary.state, canary.sent, canary.received
            )),
            Some(_) => None,
        }
    }

    /// Returns the report as lines for the console: one per step, then the canary check.
    pub fn summary_lines(&self) -> Vec<String> {
        let seconds = |ms: Option<u64>| ms.map_or_else(|| "-".to_string(), |ms| format!("{:.1} s", ms as f64 / 1000.0));
        let mut lines = vec![format!(
            "{:<4} {:<52} {:>4} {:>8} {:>8} {:>8}  outcome; plan diff",
            "step", "", "rev", "compile", "ack", "visible"
        )];
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
            lines.push(format!(
                "canary: {} sent and {} received at the primary endpoint over {} s ({}); {} rolling checks balanced",
                canary.sent,
                canary.received,
                canary.to_s - canary.from_s,
                canary.state,
                self.canary_samples
            ));
        }
        lines.push(format!(
            "run: {:.1} s, baseline {}",
            self.duration_ms as f64 / 1000.0,
            seconds(self.baseline_ms)
        ));
        lines
    }
}

/// Runs the scenarios against the demo server of `settings`, and returns the report. A failed step stops the run, and
/// the report says why.
pub async fn run(settings: &RunSettings) -> Report {
    let started = Instant::now();
    let mut runner = match DemoClient::new(settings.ports.ui) {
        Ok(client) => Runner {
            client,
            report: Report {
                run_dir: settings.run_dir.display().to_string(),
                ..Report::default()
            },
            first_range: None,
            last_change_s: 0,
            alerted: BTreeSet::new(),
        },
        Err(error) => {
            return Report {
                error: Some(format!("{:#}", error)),
                ..Report::default()
            }
        }
    };
    if let Err(error) = runner.scenarios().await {
        runner.report.error = Some(format!("{:#}", error));
    }
    runner.report.duration_ms = started.elapsed().as_millis() as u64;
    runner.report
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

    /// Returns ADP's acknowledgment of `revision`, if it reports one.
    fn acknowledgment(&self, revision: u64) -> Option<&Value> {
        self.document["config_states"]
            .as_array()?
            .iter()
            .find(|state| state["revision"].as_u64() == Some(revision))
    }
}

/// What a scenario expects of a compile.
enum Compiled {
    /// No findings, and a plan diff: empty for an unmodified export, not empty otherwise.
    Clean { unchanged: bool },

    /// An error whose message (or validation rule) has this fragment.
    Flagged { fragment: &'static str },

    /// An artifact, with findings or without: what refuses it is ADP's placement, which the editor does not run.
    Unplaced,
}

/// The fragment of ADP's refusal of an aggregation in the policy scope: placement refuses it with the capability it
/// lacks, since a state owner runs only in an isolation slot, and none is behind the policy scope.
const AGGREGATION_OUTSIDE_ITS_SLOT: &[&str] = &["runs only in an isolation slot"];

/// What a scenario expects ADP to do with a publish.
enum Installed {
    /// Activate the artifact; with `new_generation` false, in the generation already active.
    Active { new_generation: bool, routing: Routing },

    /// Reject it with an error that has one of these fragments, and keep the active generation running.
    Rejected { fragments: &'static [&'static str] },
}

struct Runner {
    client: DemoClient,
    report: Report,
    /// The first canary range found balanced: where the whole-run check starts.
    first_range: Option<(u64, u64)>,
    /// When the last publish or revoke was made, in Unix seconds.
    last_change_s: u64,
    /// The intake problems already reported, so that each is reported once.
    alerted: BTreeSet<String>,
}

impl Runner {
    async fn scenarios(&mut self) -> Result<(), GenericError> {
        let baseline = Instant::now();
        // ADP serves its export and default program, and the canary check has a whole range of seconds behind it.
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
        let outputs = outputs(&program["legend"])?;
        let boot = self.status().await?;
        self.expect_routing("baseline", &default_routing(), Instant::now())
            .await?;

        self.step(
            "S0",
            "publish the unmodified default program",
            source.clone(),
            Compiled::Clean { unchanged: true },
            Installed::Active {
                new_generation: false,
                routing: default_routing(),
            },
        )
        .await?;
        self.check_routes(&boot, None).await?;

        let s1 = edits::drop_debug(&source)?;
        self.step(
            "S1",
            "drop demo.debug.*",
            s1,
            Compiled::Clean { unchanged: false },
            Installed::Active {
                new_generation: true,
                routing: changed(default_routing(), "primary", &["demo.debug.*"], true),
            },
        )
        .await?;
        let s1_status = self.status().await?;
        self.check_routes(&boot, Some(&s1_status)).await?;

        self.step(
            "S2",
            "widen team B's route with team:platform",
            edits::widen_team_b(&source, &outputs)?,
            Compiled::Clean { unchanged: false },
            Installed::Active {
                new_generation: true,
                routing: changed(default_routing(), "team-b", &["demo.platform.queue_depth"], false),
            },
        )
        .await?;

        self.step(
            "S3",
            "route team:checkout to team B only",
            edits::exclusive_team_b(&source, &outputs)?,
            Compiled::Clean { unchanged: false },
            Installed::Active {
                new_generation: true,
                routing: changed(default_routing(), "primary", &CHECKOUT, true),
            },
        )
        .await?;

        let mirrored = changed(default_routing(), "mrf", &[MIRRORED_METRIC], false);
        self.step(
            "S4",
            "mirror one family to failover with true and a literal list",
            edits::mirror_to_failover(&source, &outputs)?,
            Compiled::Clean { unchanged: false },
            Installed::Active {
                new_generation: true,
                routing: mirrored.clone(),
            },
        )
        .await?;

        let guardrails: [(_, _, _, &[&str]); 3] = [
            (
                "S5a",
                "guardrail: a duplicate branch to the primary output",
                edits::duplicate_branch(&source, &outputs)?,
                &["duplicate_contribution"],
            ),
            (
                "S5b",
                "guardrail: an emit to an undeclared output",
                edits::undeclared_output(&source, &outputs)?,
                &[UNDECLARED_OUTPUT],
            ),
            (
                "S5c",
                "guardrail: an aggregation without its parameters",
                edits::aggregation_without_parameters(&source)?,
                &["valid_parameters"],
            ),
        ];
        for (id, title, source, fragments) in guardrails {
            let compiled = Compiled::Flagged { fragment: fragments[0] };
            self.step(id, title, source, compiled, Installed::Rejected { fragments })
                .await?;
        }
        self.step(
            "S5d",
            "guardrail: an aggregation in the policy scope",
            edits::aggregation_in_the_policy_scope(&source)?,
            Compiled::Unplaced,
            Installed::Rejected {
                fragments: AGGREGATION_OUTSIDE_ITS_SLOT,
            },
        )
        .await?;
        // The previous generation keeps running: the endpoints still receive what S4 routes.
        self.expect_routing("S5", &mirrored, Instant::now()).await?;

        self.revoke(&boot).await?;
        self.whole_run_canary().await
    }

    /// Runs one scenario step: compile, publish, acknowledgment, and what the endpoints receive.
    async fn step(
        &mut self, id: &'static str, title: &'static str, source: String, compiled: Compiled, installed: Installed,
    ) -> Result<(), GenericError> {
        let mut step = StepReport {
            id,
            title,
            ..StepReport::default()
        };
        let result = self.run_step(&mut step, source, compiled, installed).await;
        self.report.steps.push(step);
        result.with_error_context(|| format!("{} ({}) failed", id, title))
    }

    async fn run_step(
        &mut self, step: &mut StepReport, source: String, compiled: Compiled, installed: Installed,
    ) -> Result<(), GenericError> {
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
        let diagnostics = outcome["diagnostics"].as_array().cloned().unwrap_or_default();
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
        match compiled {
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

        let (acknowledgment, after) = self.wait_ack(revision, published).await?;
        step.ack_ms = Some(published.elapsed().as_millis() as u64);
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
                if after.artifact_revision != Some(revision) {
                    return Err(generic_error!(
                        "ADP reports artifact revision {:?} active, not {}.",
                        after.artifact_revision,
                        revision
                    ));
                }
                let composed = outcome["preview"]["plan_hash"].as_str();
                if after.plan_hash.as_deref() != composed {
                    return Err(generic_error!(
                        "ADP's active plan is {:?}, but the editor composed {:?}: the preview is not what ADP \
                         installed.",
                        after.plan_hash,
                        composed
                    ));
                }
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
                let visible = self.expect_routing(step.id, &routing, published).await?;
                step.visible_ms = Some(visible.as_millis() as u64);
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
                if (after.generation, after.artifact_revision) != (before.generation, before.artifact_revision) {
                    return Err(generic_error!(
                        "After rejecting revision {}, ADP runs generation {:?} (artifact {:?}), not generation {:?} \
                         (artifact {:?}) as before.",
                        revision,
                        after.generation,
                        after.artifact_revision,
                        before.generation,
                        before.artifact_revision
                    ));
                }
                step.outcome = format!(
                    "rejected, generation {} kept: {}",
                    after.generation.unwrap_or_default(),
                    shorten(&error, 100)
                );
            }
        }
        Ok(())
    }

    /// S6: revokes the artifact, and checks that the default program comes back, with what the endpoints receive.
    async fn revoke(&mut self, boot: &Status) -> Result<(), GenericError> {
        let mut step = StepReport {
            id: "S6",
            title: "revoke: an empty snapshot",
            ..StepReport::default()
        };
        let result = async {
            let before = self.status().await?;
            let revoked = Instant::now();
            let (status, answer) = self.client.post_json("/api/revoke", &json!({})).await?;
            if !status.is_success() || answer["revoked"] != true {
                return Err(generic_error!("Revoking was refused: {}", answer));
            }
            self.last_change_s = unix_millis() / 1000;
            let deadline = revoked + ACK_TIMEOUT;
            let after = loop {
                let status = self.status().await?;
                if status.artifact_revision.is_none() && status.plan_hash == boot.plan_hash {
                    break status;
                }
                if Instant::now() >= deadline {
                    return Err(generic_error!(
                        "ADP still runs artifact revision {:?} (plan {:?}) {} s after the revoke, not the default \
                         program (plan {:?}).",
                        status.artifact_revision,
                        status.plan_hash,
                        ACK_TIMEOUT.as_secs(),
                        boot.plan_hash
                    ));
                }
                tokio::time::sleep(POLL).await;
            };
            step.ack_ms = Some(revoked.elapsed().as_millis() as u64);
            step.generation = after.generation;
            if after.generation <= before.generation {
                return Err(generic_error!(
                    "ADP returned to the default program without a new generation (still {:?}).",
                    after.generation
                ));
            }
            // The stub's record of ADP's polls says so too.
            self.wait_for("ADP's polls to report the default program", ACK_TIMEOUT, |state| {
                let status = &state["channel"]["last_poll"]["status"];
                let default_program = status["active_plan_hash"].as_str() == boot.plan_hash.as_deref();
                if status["active_artifact"].is_null() && default_program {
                    Ok(())
                } else {
                    Err(format!("the last poll reports {}", status))
                }
            })
            .await?;
            step.outcome = format!("default program in generation {}", after.generation.unwrap_or_default());
            step.diff = "-".to_string();
            let visible = self.expect_routing("S6", &default_routing(), revoked).await?;
            step.visible_ms = Some(visible.as_millis() as u64);
            Ok(())
        }
        .await;
        self.report.steps.push(step);
        result.error_context("S6 (revoke) failed")
    }

    /// Reads the control surface routes through the demo and checks them against ADP's status: the export and the
    /// default program after boot (`active` is `None`), and the active plan and its explain document while an artifact
    /// is active.
    async fn check_routes(&mut self, boot: &Status, active: Option<&Status>) -> Result<(), GenericError> {
        let Some(active) = active else {
            let export = self.client.get_json("/api/adp/export").await?;
            let program_hash = export["artifact"]["base"]["program_hash"].as_str();
            if program_hash != boot.default_program_hash.as_deref() {
                return Err(generic_error!(
                    "The export's base is {:?}, but ADP's status reports the default program {:?}.",
                    program_hash,
                    boot.default_program_hash
                ));
            }
            self.report.routes.push(RouteReport {
                route: "/api/adp/export",
                checked: "its base program hash is the status's default program hash".to_string(),
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
        let has_node = plan["nodes"]
            .as_array()
            .is_some_and(|nodes| nodes.iter().any(|node| node["id"] == "policy/drop_debug"));
        if !has_node {
            return Err(generic_error!("The active plan has no node `policy/drop_debug`."));
        }
        self.report.routes.push(RouteReport {
            route: "/api/adp/plan",
            checked: "it hashes to the status's active plan hash and has S1's node `policy/drop_debug`".to_string(),
        });
        let explain = self.client.get_json("/api/adp/explain").await?;
        if explain["plan"]["hash"].as_str() != active.plan_hash.as_deref() {
            return Err(generic_error!(
                "The explain document describes plan {}, not the active plan {:?}.",
                explain["plan"]["hash"],
                active.plan_hash
            ));
        }
        let (status, text) = self.client.get("/api/adp/explain-text").await?;
        let text = String::from_utf8_lossy(&text);
        if !status.is_success() || !text.contains("policy/drop_debug") {
            return Err(generic_error!(
                "The explain text ({}) does not describe `policy/drop_debug`.",
                status
            ));
        }
        self.report.routes.push(RouteReport {
            route: "/api/adp/explain, /api/adp/explain-text",
            checked: "both describe the active plan, with `policy/drop_debug`".to_string(),
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
        let (from_s, _) = self
            .first_range
            .ok_or_else(|| generic_error!("No canary range was balanced."))?;
        let check = self
            .client
            .get_json(&format!("/api/canary?from_s={}&to_s={}", from_s, last_range))
            .await?;
        self.report.canary = Some(CanaryReport {
            from_s,
            to_s: last_range,
            sent: check["sent"].as_u64().unwrap_or_default(),
            received: check["received"].as_f64().unwrap_or_default(),
            state: check["state"].as_str().unwrap_or_default().to_string(),
        });
        Ok(())
    }

    /// Returns ADP's status, read through the demo's pass-through of the privileged status route.
    async fn status(&mut self) -> Result<Status, GenericError> {
        Ok(Status::read(self.client.get_json("/api/adp/status").await?))
    }

    /// Waits for ADP to acknowledge `revision` (`active`, `rejected`, or `failed`) on the privileged status route and
    /// in its polls of the stub, and returns the acknowledgment with ADP's status.
    async fn wait_ack(&mut self, revision: u64, since: Instant) -> Result<(Value, Status), GenericError> {
        let deadline = since + ACK_TIMEOUT;
        let (acknowledgment, status) = loop {
            let status = self.status().await?;
            if let Some(acknowledgment) = status.acknowledgment(revision) {
                if matches!(acknowledgment["state"].as_str(), Some("active" | "rejected" | "failed")) {
                    break (acknowledgment.clone(), status);
                }
            }
            if Instant::now() >= deadline {
                return Err(generic_error!(
                    "ADP did not acknowledge revision {} within {} s; its status: {}",
                    revision,
                    ACK_TIMEOUT.as_secs(),
                    status.document
                ));
            }
            tokio::time::sleep(POLL).await;
        };
        let expected = acknowledgment["state"].clone();
        self.wait_for(
            "ADP's polls of the stub to report the acknowledgment",
            ACK_TIMEOUT,
            |state| {
                let states = &state["channel"]["last_poll"]["config_states"];
                let polled = states
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|state| state["revision"].as_u64() == Some(revision));
                match polled {
                    Some(polled) if polled["state"] == expected => Ok(()),
                    other => Err(format!("the last poll reports {:?}", other)),
                }
            },
        )
        .await?;
        Ok((acknowledgment, status))
    }

    /// Waits until the endpoints receive what `expected` says, and keep doing so for [`STABLE`]. Returns the time from
    /// `since` until they first did.
    async fn expect_routing(
        &mut self, what: &str, expected: &Routing, since: Instant,
    ) -> Result<Duration, GenericError> {
        let deadline = Instant::now() + ROUTING_TIMEOUT;
        let mut reached: Option<Duration> = None;
        let mut held_since: Option<Instant> = None;
        let mut flapped = 0;
        loop {
            let state = self.state().await?;
            let found = present(&state);
            let holds = INTAKES.iter().all(|intake| found.get(*intake) == expected.get(*intake));
            match (holds, held_since) {
                (true, None) => {
                    reached.get_or_insert_with(|| since.elapsed());
                    held_since = Some(Instant::now());
                }
                (true, Some(start)) if start.elapsed() >= STABLE => {
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
                    ROUTING_TIMEOUT.as_secs(),
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

    /// Returns the demo's state, after checking the canary conservation and the intakes in it.
    async fn state(&mut self) -> Result<Value, GenericError> {
        let state = self.client.get_json("/api/state").await?;
        self.observe(&state);
        Ok(state)
    }

    /// Records the canary check and the intakes' problems in a sample of the demo's state.
    fn observe(&mut self, state: &Value) {
        let canary = &state["canary"];
        let range = (
            canary["from_s"].as_u64().unwrap_or_default(),
            canary["to_s"].as_u64().unwrap_or_default(),
        );
        match canary["state"].as_str() {
            Some("balanced") => {
                self.first_range.get_or_insert(range);
                self.report.canary_samples += 1;
            }
            Some(problem @ ("missing" | "excess")) => {
                self.alert(
                    format!("canary {} {}-{}", problem, range.0, range.1),
                    format!(
                        "canaries {} for seconds {} to {}: {} sent, {} received at the primary endpoint",
                        problem, range.0, range.1, canary["sent"], canary["received"]
                    ),
                );
            }
            _ => {}
        }
        for elsewhere in canary["elsewhere"].as_array().into_iter().flatten() {
            self.alert(
                format!("canary at {}", elsewhere["intake"]),
                format!("canaries reached {}: {}", elsewhere["intake"], elsewhere["received"]),
            );
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
    }

    fn alert(&mut self, key: String, message: String) {
        if self.alerted.insert(key) {
            tracing::warn!("{}", message);
            self.report.violations.push(message);
        }
    }
}

/// Returns the outputs that feed each intake, from the export's legend.
fn outputs(legend: &Value) -> Result<Outputs, GenericError> {
    let feeding = |intake: &str| {
        legend["outputs"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|output| output["intake"] == intake)
            .and_then(|output| output["id"].as_str())
            .map(ToString::to_string)
            .ok_or_else(|| generic_error!("No output of the export feeds the `{}` intake: {}", intake, legend))
    };
    Ok(Outputs {
        primary: feeding("primary")?,
        team_b: feeding("team-b")?,
        team_c: feeding("team-c")?,
        mrf: feeding("mrf")?,
    })
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
    let provenance_only = nodes
        .iter()
        .filter(|node| node["change"] == "changed" && node["aspects"] == json!(["provenance"]))
        .count();
    format!(
        "{} added, {} changed ({} in provenance only), {} removed, {} edges added, {} removed",
        count("added"),
        count("changed"),
        provenance_only,
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
mod tests;
