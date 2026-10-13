//! Steps S8 to S11 of the check: resources declared live, the policies that route to them, and the recipients they
//! add, drain, and retire (see the [module documentation](super)).

use std::time::{Duration, Instant};

use saluki_error::{generic_error, ErrorContext as _, GenericError};
use serde_json::{json, Value};

use super::{
    changed, default_routing,
    edits::{self, TEAM_D_FAMILY, TEAM_D_ROUTE},
    settled, Compiled, Installed, Outputs, Routing, Runner, Status, StepReport, ACK_TIMEOUT, POLL, ROUTING_TIMEOUT,
};
use crate::{
    clock::unix_millis,
    resources::TEAM_D_OUTPUT,
    stub::protocol::{dropped_outputs, POLICY_PRODUCT, RESOURCES_PRODUCT},
};

/// How long a retiring recipient may take to drain and report its retirement: the drain deadline (30 s), with room
/// for the resource transaction that releases it and for ADP to report it.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(75);

/// How many requests a paused intake answers before the drain step revokes, so that its recipient holds work.
const UNAVAILABLE_BEFORE_REVOKE: u64 = 3;

/// The intake that stands in for team D's declared endpoint.
const TEAM_D_INTAKE: &str = "team-d";

/// What identifies team D's declared recipient: its identity names its credential, `team_d`.
const TEAM_D_RECIPIENT: &str = "/declared/team_d/";

/// The forwarder telemetry that counts the transactions a recipient's close dropped.
const ITEMS_DROPPED: &str = "component_items_dropped_total";

/// The delivery host of team D's recipient.
const DELIVERY_HOST: &str = "dd_out";

/// Returns the families each intake receives once team D's route is in effect: the default routing, and the platform
/// family at team D as well.
pub(super) fn routed_to_team_d() -> Routing {
    changed(default_routing(), TEAM_D_INTAKE, &[TEAM_D_FAMILY], false)
}

/// Returns the plan node whose policy loss counts the emits to team D that a revocation turned into drops: the inline
/// `emit` of the route's `true` branch, which keeps its node ID as a drop.
fn dropped_emit_node() -> String {
    format!("{}_true", TEAM_D_ROUTE)
}

/// Returns `true` if a policy's acknowledgment is `active` and, as `degraded` says, degraded by team D's output or
/// clear of it.
///
/// Degraded, the acknowledgment says that the emits to the output are dropped: a degraded artifact is acknowledged
/// `active` with the reason, which ADP's top-level `degraded` does not stand in for (it reports native configuration
/// that cannot be re-resolved). Clear, neither the acknowledgment nor ADP's status names the output.
fn policy_degraded_by_team_d(acknowledgment: &Value, status: &Status, degraded: bool) -> bool {
    if acknowledgment["state"] != "active" {
        return false;
    }
    let error = acknowledgment["error"].as_str();
    if degraded {
        error.is_some_and(|error| dropped_outputs(error).contains(&TEAM_D_OUTPUT))
    } else {
        let names = |text: Option<&str>| text.is_some_and(|text| text.contains(TEAM_D_OUTPUT));
        !names(error) && !names(status.degraded())
    }
}

/// Returns team D's recipient in a sample of the demo's state, if the Recipients panel's record shows one in state
/// `wanted` since `since_ms` or later: the latest incarnation, if there are several.
fn team_d_recipient<'a>(state: &'a Value, wanted: &str, since_ms: u64) -> Option<&'a Value> {
    state["recipients"]["recipients"]
        .as_array()?
        .iter()
        .filter(|recipient| {
            recipient["host"] == DELIVERY_HOST
                && recipient["id"].as_str().is_some_and(|id| id.contains(TEAM_D_RECIPIENT))
                && recipient["state"] == wanted
                && recipient["state_since_ms"].as_u64().is_some_and(|at| at >= since_ms)
        })
        .max_by_key(|recipient| recipient["incarnation"].as_u64())
}

/// Returns the total of a native component counter of one component in a sample of the demo's state, summed over its
/// series, if the scraper has any.
pub(super) fn component_total(state: &Value, name: &str, component_id: &str) -> Option<f64> {
    let series = state["telemetry"]["components"]
        .as_array()?
        .iter()
        .filter(|entry| entry["name"] == name && entry["labels"]["component_id"] == component_id)
        .filter_map(|entry| entry["value"].as_f64())
        .collect::<Vec<_>>();
    (!series.is_empty()).then(|| series.iter().sum())
}

/// Returns the policy loss counted at the plan nodes whose IDs end with `node`, in a sample of the demo's state.
fn policy_loss_at(state: &Value, node: &str) -> f64 {
    state["telemetry"]["plan"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|entry| {
            entry["name"] == "plan_loss_total"
                && entry["labels"]["class"] == "policy"
                && entry["labels"]["component_id"]
                    .as_str()
                    .is_some_and(|id| id.ends_with(node))
        })
        .filter_map(|entry| entry["value"].as_f64())
        .sum()
}

/// Describes a drain report in a few words.
fn drain_summary(report: &Value) -> String {
    format!(
        "{} delivered, {} persisted, {} dropped, {}",
        report["delivered"],
        report["persisted"],
        report["dropped"],
        if report["deadline_reached"] == true {
            "at its deadline"
        } else {
            "with its queue empty"
        }
    )
}

impl Runner {
    /// Runs S8 to S11, from the default program with no resources declared (after S6).
    pub(super) async fn resources_steps(
        &mut self, source: &str, outputs: &Outputs, boot: &Status,
    ) -> Result<(), GenericError> {
        // From here on, compiles carry the Resources tab's draft, as the UI sends it.
        self.resources_draft = Some(self.template.clone());
        let route = edits::route_to_team_d(source, outputs, TEAM_D_OUTPUT)?;

        let mut step = StepReport::new("S8a", "publish the team D resources");
        let result = self.publish_resources_step(&mut step).await;
        self.finish(step, result)?;
        self.step(
            "S8b",
            "route team:platform to team D's declared output",
            route.clone(),
            Compiled::Clean { unchanged: false },
            Installed::Active {
                new_generation: true,
                routing: Some(routed_to_team_d()),
            },
        )
        .await?;
        let policy = self.last_revision("S8b")?;

        let mut step = StepReport::new("S9a", "revoke the resources: policy active and degraded");
        let result = self.revoke_resources_step(&mut step, policy).await;
        self.finish(step, result)?;
        let mut step = StepReport::new("S9b", "publish the resources again: the emit returns");
        let result = self.republish_resources_step(&mut step, policy).await;
        self.finish(step, result)?;

        let mut step = StepReport::new("S10a", "revoke the policy and the resources");
        let result = self.revoke_both_step(&mut step, boot).await;
        self.finish(step, result)?;
        self.step(
            "S10b",
            "publish the route before the resources: pending",
            route,
            Compiled::Pending { output: TEAM_D_OUTPUT },
            Installed::Pending { output: TEAM_D_OUTPUT },
        )
        .await?;
        let policy = self.last_revision("S10b")?;
        // The default program keeps running while the policy waits.
        self.expect_routing("S10b", &default_routing(), Instant::now()).await?;
        let mut step = StepReport::new("S10c", "publish the resources: the waiting policy installs");
        let result = self.install_waiting_policy_step(&mut step, policy).await;
        self.finish(step, result)?;

        let mut step = StepReport::new("S11", "pause team D, revoke: the drain reaches its deadline");
        let result = self.drain_to_deadline_step(&mut step).await;
        // Whatever happened, team D's intake answers again.
        let resumed = self.pause_team_d(false).await;
        self.finish(step, result.and(resumed))
    }

    /// Records a step's report, whatever its outcome, and names the step in its error.
    pub(super) fn finish(&mut self, step: StepReport, result: Result<(), GenericError>) -> Result<(), GenericError> {
        let (id, title) = (step.id, step.title);
        self.report.steps.push(step);
        result.with_error_context(|| format!("{} ({}) failed", id, title))
    }

    /// Returns the revision the last step published, which `id` names.
    fn last_revision(&self, id: &str) -> Result<u64, GenericError> {
        self.report
            .steps
            .last()
            .filter(|step| step.id == id)
            .and_then(|step| step.revision)
            .ok_or_else(|| generic_error!("{} published no revision.", id))
    }

    /// Checks the team D template through the demo (`POST /api/resources/check`), which must find it valid, and
    /// publishes it (`POST /api/resources/publish`). Returns the revision and when it was published.
    async fn publish_resources(&mut self) -> Result<(u64, Instant), GenericError> {
        let (status, check) = self
            .client
            .post_json("/api/resources/check", &json!({ "document": self.template }))
            .await?;
        if !status.is_success() || check["ok"] != true {
            return Err(generic_error!("The resources template cannot be published: {}", check));
        }
        if check["valid"] != true {
            return Err(generic_error!(
                "The demo's check of the resources template, as ADP validates it, refuses it: {}",
                check["validation_error"]
            ));
        }
        let declared = check["outputs"]
            .as_array()
            .is_some_and(|outputs| outputs.iter().any(|output| output["id"] == TEAM_D_OUTPUT));
        if !declared {
            return Err(generic_error!(
                "The resources template does not declare `{}`: {}",
                TEAM_D_OUTPUT,
                check["outputs"]
            ));
        }
        let published = Instant::now();
        let (status, answer) = self
            .client
            .post_json(
                "/api/resources/publish",
                &json!({ "artifact_hash": check["artifact_hash"] }),
            )
            .await?;
        if !status.is_success() {
            return Err(generic_error!("Publishing the resources was refused: {}", answer));
        }
        self.last_change_s = unix_millis() / 1000;
        let revision = answer["revision"]
            .as_u64()
            .ok_or_else(|| generic_error!("Publishing the resources answered without a revision: {}", answer))?;
        Ok((revision, published))
    }

    /// Revokes the resources (`POST /api/resources/revoke`), which must have been served. Returns when, as an instant
    /// and in milliseconds since the Unix epoch.
    async fn revoke_resources(&mut self) -> Result<(Instant, u64), GenericError> {
        let revoked = (Instant::now(), unix_millis());
        let (status, answer) = self.client.post_json("/api/resources/revoke", &json!({})).await?;
        if !status.is_success() || answer["revoked"] != true {
            return Err(generic_error!("Revoking the resources was refused: {}", answer));
        }
        self.last_change_s = unix_millis() / 1000;
        Ok(revoked)
    }

    /// Waits for ADP to acknowledge resources revision `revision` as `active`, with its active generation lowered with
    /// it, and returns ADP's status.
    async fn wait_resources_active(&mut self, revision: u64, since: Instant) -> Result<Status, GenericError> {
        let (acknowledgment, _) = self.wait_ack(RESOURCES_PRODUCT, revision, since, settled).await?;
        if acknowledgment["state"] != "active" {
            return Err(generic_error!(
                "ADP acknowledged resources revision {} as {}: {}",
                revision,
                acknowledgment["state"],
                acknowledgment["error"]
            ));
        }
        // The acknowledgment and the generation lowered with the revision show together, or nearly so.
        let deadline = Instant::now() + ACK_TIMEOUT;
        loop {
            let status = self.status().await?;
            if status.resources_revision == Some(revision) {
                return Ok(status);
            }
            if Instant::now() >= deadline {
                return Err(generic_error!(
                    "ADP acknowledged resources revision {} as active, but its status reports resources revision {:?} \
                     active.",
                    revision,
                    status.resources_revision
                ));
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// Waits for ADP's export, as the demo holds it, to declare team D's output, feeding team D's intake, or, with
    /// `declared` false, to no longer declare it: the editor composes against that export.
    async fn wait_declared(&mut self, declared: bool) -> Result<(), GenericError> {
        let what = if declared {
            "ADP's export to declare team D's output"
        } else {
            "ADP's export to no longer declare team D's output"
        };
        self.wait_for(what, ACK_TIMEOUT, |state| {
            let outputs = &state["legend"]["outputs"];
            let listed = outputs.as_array().into_iter().flatten().any(|output| {
                output["id"] == TEAM_D_OUTPUT && output["declared"] == true && output["intake"] == TEAM_D_INTAKE
            });
            if listed == declared && state["control"]["default_program_state"] == "available" {
                Ok(())
            } else {
                Err(format!(
                    "its outputs are {}; the default program is {}",
                    outputs, state["control"]["default_program_state"]
                ))
            }
        })
        .await
    }

    /// Waits for ADP to report a policy revision's acknowledgment `active` and, as `degraded` says, degraded by team
    /// D's output or clear of it (see [`policy_degraded_by_team_d`]), and for the status to satisfy `also`. Returns
    /// the status.
    async fn wait_policy(
        &mut self, policy: u64, degraded: bool, since: Instant, what: &str, also: impl Fn(&Status) -> bool,
    ) -> Result<Status, GenericError> {
        let deadline = since + ACK_TIMEOUT;
        loop {
            let status = self.status().await?;
            let held = status
                .acknowledgment(POLICY_PRODUCT, policy)
                .is_some_and(|acknowledgment| policy_degraded_by_team_d(acknowledgment, &status, degraded));
            if held && also(&status) {
                return Ok(status);
            }
            if Instant::now() >= deadline {
                return Err(generic_error!(
                    "{} s after {}, ADP does not report policy revision {} active{} as expected (its top-level \
                     degraded reason: {:?}); its status: {}",
                    ACK_TIMEOUT.as_secs(),
                    what,
                    policy,
                    if degraded {
                        format!(
                            " and degraded, its acknowledgment saying that the emits to `{}` are dropped,",
                            TEAM_D_OUTPUT
                        )
                    } else {
                        format!(" and not degraded, nothing naming `{}`,", TEAM_D_OUTPUT)
                    },
                    status.degraded(),
                    status.document
                ));
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// S8a: publishes the team D resources and checks that ADP declares them, with the endpoints unchanged, since
    /// nothing emits to the output yet.
    pub(super) async fn publish_resources_step(&mut self, step: &mut StepReport) -> Result<(), GenericError> {
        let (revision, published) = self.publish_resources().await?;
        step.revision = Some(revision);
        let after = self.wait_resources_active(revision, published).await?;
        step.ack_ms = Some(published.elapsed().as_millis() as u64);
        step.generation = after.generation;
        self.wait_declared(true).await?;
        let visible = self.expect_routing(step.id, &default_routing(), published).await?;
        step.visible_ms = Some(visible.as_millis() as u64);
        step.outcome = format!(
            "resources active in generation {}; ADP declares `{}`",
            after.generation.unwrap_or_default(),
            TEAM_D_OUTPUT
        );
        step.diff = "-".to_string();
        Ok(())
    }

    /// S9a: revokes the resources while the routing policy (revision `policy`) runs: the policy stays `active`,
    /// degraded, its emit to team D dropped and counted, the primary endpoint unchanged, and team D's recipient drains
    /// and retires.
    async fn revoke_resources_step(&mut self, step: &mut StepReport, policy: u64) -> Result<(), GenericError> {
        let before = self.status().await?;
        let node = dropped_emit_node();
        let loss_before = policy_loss_at(&self.state().await?, &node);
        let (revoked, revoked_ms) = self.revoke_resources().await?;
        let after = self
            .wait_policy(policy, true, revoked, "the revoke", |status| {
                status.resources_revision.is_none() && status.generation > before.generation
            })
            .await?;
        step.revision = Some(policy);
        step.ack_ms = Some(revoked.elapsed().as_millis() as u64);
        step.generation = after.generation;
        // Team D stops receiving; every other endpoint receives what it did.
        let visible = self.expect_routing(step.id, &default_routing(), revoked).await?;
        step.visible_ms = Some(visible.as_millis() as u64);
        self.wait_for(
            "the dropped emit's policy loss to be counted",
            ROUTING_TIMEOUT,
            |state| {
                let loss = policy_loss_at(state, &node);
                if loss > loss_before {
                    Ok(())
                } else {
                    Err(format!(
                        "plan_loss_total{{class=\"policy\"}} at `{}` is {} (was {})",
                        node, loss, loss_before
                    ))
                }
            },
        )
        .await?;
        let retired = self.wait_team_d_retired(revoked_ms).await?;
        step.outcome = format!(
            "policy rev {} active and degraded in generation {}; team D's recipient retired ({})",
            policy,
            after.generation.unwrap_or_default(),
            drain_summary(&retired["last_drain"])
        );
        step.diff = "-".to_string();
        Ok(())
    }

    /// S9b: publishes the resources again: the policy in effect (revision `policy`) composes with its emit again,
    /// without a new policy revision, and team D receives the platform family again.
    async fn republish_resources_step(&mut self, step: &mut StepReport, policy: u64) -> Result<(), GenericError> {
        let (revision, published) = self.publish_resources().await?;
        step.revision = Some(revision);
        self.wait_resources_active(revision, published).await?;
        let after = self
            .wait_policy(policy, false, published, "the resources returned", |status| {
                status.artifact_revision == Some(policy)
            })
            .await?;
        step.ack_ms = Some(published.elapsed().as_millis() as u64);
        step.generation = after.generation;
        let visible = self.expect_routing(step.id, &routed_to_team_d(), published).await?;
        step.visible_ms = Some(visible.as_millis() as u64);
        step.outcome = format!(
            "policy rev {} active again in generation {}, its emit restored",
            policy,
            after.generation.unwrap_or_default()
        );
        step.diff = "-".to_string();
        Ok(())
    }

    /// S10a: revokes the policy and the resources, and checks that the default program of boot runs again.
    async fn revoke_both_step(&mut self, step: &mut StepReport, boot: &Status) -> Result<(), GenericError> {
        let (revoked, _) = self.revoke_resources().await?;
        let (status, answer) = self.client.post_json("/api/revoke", &json!({})).await?;
        if !status.is_success() || answer["revoked"] != true {
            return Err(generic_error!("Revoking the policy was refused: {}", answer));
        }
        self.last_change_s = unix_millis() / 1000;
        let deadline = revoked + ACK_TIMEOUT;
        let after = loop {
            let status = self.status().await?;
            let bare = status.artifact_revision.is_none() && status.resources_revision.is_none();
            if bare && status.plan_hash == boot.plan_hash {
                break status;
            }
            if Instant::now() >= deadline {
                return Err(generic_error!(
                    "{} s after both revokes, ADP runs artifact {:?} with resources {:?} (plan {:?}), not the default \
                     program of boot (plan {:?}).",
                    ACK_TIMEOUT.as_secs(),
                    status.artifact_revision,
                    status.resources_revision,
                    status.plan_hash,
                    boot.plan_hash
                ));
            }
            tokio::time::sleep(POLL).await;
        };
        step.ack_ms = Some(revoked.elapsed().as_millis() as u64);
        step.generation = after.generation;
        let visible = self.expect_routing(step.id, &default_routing(), revoked).await?;
        step.visible_ms = Some(visible.as_millis() as u64);
        // The next step compiles against the export ADP lowers without the resources.
        self.wait_declared(false).await?;
        step.outcome = format!("default program in generation {}", after.generation.unwrap_or_default());
        step.diff = "-".to_string();
        Ok(())
    }

    /// S10c: publishes the resources a policy (revision `policy`) waits for, which installs it by itself.
    async fn install_waiting_policy_step(&mut self, step: &mut StepReport, policy: u64) -> Result<(), GenericError> {
        let before = self.status().await?;
        let (revision, published) = self.publish_resources().await?;
        step.revision = Some(revision);
        self.wait_resources_active(revision, published).await?;
        let (acknowledgment, after) = self.wait_ack(POLICY_PRODUCT, policy, published, settled).await?;
        if acknowledgment["state"] != "active" || after.artifact_revision != Some(policy) {
            return Err(generic_error!(
                "Once the resources arrived, ADP acknowledged policy revision {} as {} ({}), with artifact {:?} \
                 active.",
                policy,
                acknowledgment["state"],
                acknowledgment["error"],
                after.artifact_revision
            ));
        }
        if after.generation <= before.generation {
            return Err(generic_error!(
                "ADP installed policy revision {} without a new generation (still {:?}).",
                policy,
                after.generation
            ));
        }
        step.ack_ms = Some(published.elapsed().as_millis() as u64);
        step.generation = after.generation;
        let visible = self.expect_routing(step.id, &routed_to_team_d(), published).await?;
        step.visible_ms = Some(visible.as_millis() as u64);
        step.outcome = format!(
            "policy rev {} active in generation {}",
            policy,
            after.generation.unwrap_or_default()
        );
        step.diff = "-".to_string();
        Ok(())
    }

    /// S11: pauses team D's intake, then revokes the resources: team D's recipient drains until its deadline and drops
    /// what it holds, as its drain report and the forwarder's telemetry both count. The caller resumes the intake.
    async fn drain_to_deadline_step(&mut self, step: &mut StepReport) -> Result<(), GenericError> {
        self.pause_team_d(true).await?;
        self.wait_for(
            "team D's paused intake to answer ADP's retries",
            ROUTING_TIMEOUT,
            |state| {
                let intake = state["intakes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|intake| intake["name"] == TEAM_D_INTAKE);
                match intake.and_then(|intake| intake["unavailable"].as_u64()) {
                    Some(answered) if answered >= UNAVAILABLE_BEFORE_REVOKE => Ok(()),
                    answered => Err(format!("it answered {:?} requests with 503", answered)),
                }
            },
        )
        .await?;
        // A counter that never counted may not be exposed yet: it counts from zero.
        let dropped_before = self
            .wait_for("ADP's telemetry", ROUTING_TIMEOUT, |state| {
                match state["telemetry"]["state"].as_str() {
                    Some("ok") => Ok(component_total(state, ITEMS_DROPPED, DELIVERY_HOST).unwrap_or_default()),
                    other => Err(format!("the scraper's state is {:?}", other)),
                }
            })
            .await?;

        let (revoked, revoked_ms) = self.revoke_resources().await?;
        let retired = self.wait_team_d_retired(revoked_ms).await?;
        step.ack_ms = Some(revoked.elapsed().as_millis() as u64);
        let report = retired["last_drain"].clone();
        if report["deadline_reached"] != true {
            return Err(generic_error!(
                "Team D's recipient retired with its queue empty while its intake was paused: {}",
                report
            ));
        }
        let dropped = report["dropped"].as_u64().unwrap_or_default();
        if dropped == 0 {
            return Err(generic_error!(
                "Team D's recipient reached its deadline but dropped nothing: {}",
                report
            ));
        }
        let counted = self
            .wait_for(
                "the forwarder's telemetry to count the drops",
                ROUTING_TIMEOUT,
                |state| {
                    let counted =
                        component_total(state, ITEMS_DROPPED, DELIVERY_HOST).unwrap_or_default() - dropped_before;
                    if counted == dropped as f64 {
                        Ok(counted)
                    } else {
                        Err(format!(
                            "{}{{component_id=\"{}\"}} rose by {} since the revoke, and the drain report says {} \
                             dropped",
                            ITEMS_DROPPED, DELIVERY_HOST, counted, dropped
                        ))
                    }
                },
            )
            .await?;
        step.visible_ms = Some(revoked.elapsed().as_millis() as u64);
        step.outcome = format!(
            "team D's recipient retired ({}); {} counted {} drops",
            drain_summary(&report),
            DELIVERY_HOST,
            counted
        );
        step.diff = "-".to_string();
        Ok(())
    }

    /// Pauses or resumes team D's intake.
    async fn pause_team_d(&mut self, paused: bool) -> Result<(), GenericError> {
        let path = format!("/api/intakes/{}/pause", TEAM_D_INTAKE);
        let (status, answer) = self.client.post_json(&path, &json!({ "paused": paused })).await?;
        if status.is_success() {
            Ok(())
        } else {
            Err(generic_error!("Pausing team D's intake was refused: {}", answer))
        }
    }

    /// Waits for the Recipients panel's record to show team D's recipient retired since `since_ms`, with a drain
    /// report, and returns it.
    async fn wait_team_d_retired(&mut self, since_ms: u64) -> Result<Value, GenericError> {
        self.wait_for("team D's recipient to drain and retire", DRAIN_TIMEOUT, |state| {
            if state["recipients"]["reported"] != true {
                return Err("ADP's status lists no recipients (`recipients`)".to_string());
            }
            match team_d_recipient(state, "retired", since_ms) {
                Some(retired) if !retired["last_drain"].is_null() => Ok(retired.clone()),
                Some(retired) => Err(format!("it retired without a drain report: {}", retired)),
                None => Err(format!("the recipients are {}", state["recipients"]["recipients"])),
            }
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn team_d_routing_adds_the_platform_family_to_team_d_only() {
        let routed = routed_to_team_d();
        assert_eq!(
            routed["team-d"].iter().collect::<Vec<_>>(),
            ["demo.platform.queue_depth"]
        );
        let mut without = routed.clone();
        without.insert("team-d".to_string(), Default::default());
        assert_eq!(without, default_routing());
        assert_eq!(dropped_emit_node(), "team_d_route_true");
    }

    /// A sample of the demo's state as the server writes it, with what these steps read: the Recipients panel's
    /// record and the scraper's series.
    fn state() -> Value {
        json!({
            "recipients": {
                "reported": true,
                "recipients": [
                    {"host": "dd_out", "id": "dd_out/primary/0", "incarnation": 1, "state": "active",
                     "state_since_ms": 1_000},
                    {"host": "dd_out", "id": "dd_out/declared/team_d/http://127.0.0.1:27105", "incarnation": 1,
                     "state": "retired", "state_since_ms": 5_000,
                     "last_drain": {"delivered": 3, "persisted": 0, "dropped": 0, "deadline_reached": false}},
                    {"host": "dd_out", "id": "dd_out/declared/team_d/http://127.0.0.1:27105", "incarnation": 2,
                     "state": "retired", "state_since_ms": 9_000,
                     "last_drain": {"delivered": 0, "persisted": 0, "dropped": 4, "deadline_reached": true}}
                ]
            },
            "telemetry": {
                "plan": [
                    {"name": "plan_loss_total", "value": 12.0,
                     "labels": {"class": "policy", "component_id": "policy/team_d_route_true"}},
                    {"name": "plan_loss_total", "value": 5.0,
                     "labels": {"class": "capacity", "component_id": "policy/team_d_route_true"}},
                    {"name": "plan_loss_total", "value": 80.0,
                     "labels": {"class": "policy", "component_id": "policy/team_d_route_false"}}
                ],
                "components": [
                    {"name": "component_items_dropped_total", "value": 4.0,
                     "labels": {"component_id": "dd_out", "domain": "a"}},
                    {"name": "component_items_dropped_total", "value": 2.0,
                     "labels": {"component_id": "dd_out", "domain": "b"}},
                    {"name": "component_items_dropped_total", "value": 9.0, "labels": {"component_id": "mrf_dd_out"}}
                ]
            }
        })
    }

    #[test]
    fn the_steps_read_team_ds_latest_retirement_and_the_counters_they_compare() {
        let state = state();
        let latest = team_d_recipient(&state, "retired", 0).unwrap();
        assert_eq!(latest["incarnation"], 2);
        assert_eq!(
            drain_summary(&latest["last_drain"]),
            "0 delivered, 0 persisted, 4 dropped, at its deadline"
        );
        // Retired before the revoke: not the retirement a step waits for.
        assert!(team_d_recipient(&state, "retired", 9_001).is_none());
        assert!(team_d_recipient(&state, "active", 0).is_none());

        assert_eq!(component_total(&state, ITEMS_DROPPED, DELIVERY_HOST), Some(6.0));
        assert_eq!(component_total(&state, ITEMS_DROPPED, "cluster_agent_out"), None);
        assert_eq!(policy_loss_at(&state, &dropped_emit_node()), 12.0);
        assert_eq!(policy_loss_at(&state, "nothing_here"), 0.0);
    }

    #[test]
    fn the_drain_steps_read_team_ds_retirement_from_a_status_that_lists_only_retired_recipients() {
        use crate::{recipients::RecipientLog, stub::protocol::AdpStatus};

        // A recipient entry as ADP's privileged status route writes it (stage 7 part P7.5): retired incarnations only,
        // the latest 16, each with its drain report.
        let retired = |incarnation: u64, delivered: u64, dropped: u64, retired_ms: u64| {
            json!({
                "host": "dd_out",
                "id": "dd_out/declared/team_d/http://127.0.0.1:27105",
                "slot": 3,
                "incarnation": incarnation,
                "state": "retired",
                "provenance": {"declared": {"revision": 2, "hash": format!("sha256:{}", "ab".repeat(32))}},
                "last_drain": {"delivered": delivered, "persisted": 0, "dropped": dropped,
                               "deadline_reached": dropped > 0, "retired_ms": retired_ms},
            })
        };
        let status = |recipients: Vec<Value>| {
            let (status, notes) = AdpStatus::read(&json!({
                "active_generation": 9, "revision_high_water": 7, "config_states": [], "recipients": recipients,
            }));
            assert!(notes.is_empty(), "{:?}", notes);
            status
        };
        let mut log = RecipientLog::default();
        let state = |log: &RecipientLog| json!({ "recipients": log.view() });

        // Before the revoke, ADP lists none; S9a revokes at 10 000, and the retirement shows after it.
        log.observe(status(Vec::new()).recipients.as_deref(), 9_000);
        assert!(team_d_recipient(&state(&log), "retired", 10_000).is_none());
        log.observe(status(vec![retired(2, 5, 0, 10_400)]).recipients.as_deref(), 10_500);
        let s9a = team_d_recipient(&state(&log), "retired", 10_000).cloned().unwrap();
        assert_eq!(s9a["incarnation"], 2);
        assert_eq!(
            drain_summary(&s9a["last_drain"]),
            "5 delivered, 0 persisted, 0 dropped, with its queue empty"
        );

        // S11 revokes at 60 000: the earlier retirement is still listed, and the step reads the new incarnation's.
        log.observe(status(vec![retired(2, 5, 0, 10_400)]).recipients.as_deref(), 59_000);
        assert!(team_d_recipient(&state(&log), "retired", 60_000).is_none());
        let listed = vec![retired(2, 5, 0, 10_400), retired(4, 0, 7, 90_100)];
        log.observe(status(listed.clone()).recipients.as_deref(), 90_250);
        log.observe(status(listed).recipients.as_deref(), 91_250);
        let s11 = team_d_recipient(&state(&log), "retired", 60_000).cloned().unwrap();
        assert_eq!(s11["incarnation"], 4);
        assert_eq!(s11["state_since_ms"], 90_250);
        assert_eq!(
            drain_summary(&s11["last_drain"]),
            "0 delivered, 0 persisted, 7 dropped, at its deadline"
        );
    }

    #[test]
    fn a_policy_is_degraded_by_team_d_only_when_its_acknowledgment_says_the_emits_to_it_are_dropped() {
        let status = |degraded: Value| Status::read(json!({ "degraded": degraded }));
        let quiet_status = status(Value::Null);
        // What S9a waits for (degraded) and what S9b waits for (clear).
        let s9a = |acknowledgment: &Value, status: &Status| policy_degraded_by_team_d(acknowledgment, status, true);
        let s9b = |acknowledgment: &Value, status: &Status| policy_degraded_by_team_d(acknowledgment, status, false);

        let acknowledgment = json!({
            "state": "active",
            "error": "output `resources_team_d` is no longer declared; its emits are dropped until it returns"
        });
        assert!(s9a(&acknowledgment, &quiet_status));
        assert!(!s9b(&acknowledgment, &quiet_status));

        // ADP's top-level reason alone does not say that the acknowledgment carries the reason: neither degraded as
        // S9a expects, nor clear as S9b expects.
        let quiet = json!({ "state": "active", "error": null });
        let named = status(json!("policy: output `resources_team_d` is no longer declared"));
        assert!(!s9a(&quiet, &named));
        assert!(!s9b(&quiet, &named));
        assert!(s9b(&quiet, &quiet_status));

        // A note that drops nothing, a reason about another output, and an artifact not active are not degraded by it.
        let rebase = json!({
            "state": "active",
            "error": "the base records `dd_metrics_encode` with version 1 fingerprints, which still match; rebase to \
                      fingerprint version 2"
        });
        assert!(!s9a(&rebase, &quiet_status));
        assert!(s9b(&rebase, &quiet_status));
        let other = json!({
            "state": "active", "error": "output `metrics_routing_encode_0` now delivers elsewhere; its emits are \
                                          dropped; rebase"
        });
        assert!(!s9a(&other, &quiet_status));
        assert!(s9b(&other, &quiet_status));
        let pending = json!({ "state": "pending", "error": "waiting for output `resources_team_d`" });
        assert!(!s9a(&pending, &quiet_status));
        assert!(!s9b(&json!({ "state": "pending", "error": null }), &quiet_status));
    }
}
