//! Steps S8 to S11 of the check: destinations added and removed while ADP runs, the programs that route to them, and
//! a native policy change under a published program (see the [module documentation](super)).
//!
//! A destination is added with `POST /api/destinations`, which starts its intake on a free port and adds it to
//! `additional_endpoints` in ADP's configuration file, and removed with `DELETE /api/destinations/{name}`, which takes
//! it out; ADP re-reads the file. Its recipient in ADP's directory is named by its URL.

use std::time::{Duration, Instant};

use saluki_error::{generic_error, ErrorContext as _, GenericError};
use serde_json::{json, Value};

use super::{
    changed, default_routing,
    edits::{self, DESTINATION_ROUTE, PLATFORM_FAMILY},
    narrowed_routing, Compiled, Installed, Routing, Runner, StepReport, ACK_TIMEOUT, FAMILIES, FILTERLISTED, POLL,
};
use crate::{
    settings::DemoPorts,
    stub::protocol::{dropped_outputs, POLICY_PRODUCT},
};

/// How long a removed destination's recipient may take to drain and retire, and its intake to stop: the demo's own
/// wait for the retirement ([`REMOVAL_TIMEOUT`](crate::destinations::REMOVAL_TIMEOUT)), with room for its polls.
const RETIRE_TIMEOUT: Duration = Duration::from_secs(75);

/// The binding of the metric filterlist, which ADP's status lists as differing while a published program runs.
const FILTERLIST_BINDING: &str = "dogstatsd.metric_filter.values";

/// An added destination, as `POST /api/destinations` answers.
#[derive(Clone, Debug)]
struct Added {
    /// The intake's name, such as `dest-d`.
    name: String,

    /// Its label, such as `Destination D`.
    label: String,

    /// Its URL, which names its recipient.
    url: String,
}

/// Returns the intake named `name` in a sample of the demo's state.
fn intake<'a>(state: &'a Value, name: &str) -> Option<&'a Value> {
    state["intakes"]
        .as_array()?
        .iter()
        .find(|intake| intake["name"] == name)
}

/// Returns the latest incarnation of the recipient of the endpoint at `url` in a sample of the demo's state, as the
/// Recipients panel's record shows it.
fn recipient<'a>(state: &'a Value, url: &str) -> Option<&'a Value> {
    let suffix = format!("/{}", url);
    state["recipients"]["recipients"]
        .as_array()?
        .iter()
        .filter(|recipient| recipient["id"].as_str().is_some_and(|id| id.ends_with(&suffix)))
        .max_by_key(|recipient| recipient["incarnation"].as_u64())
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
    if report.is_null() {
        return "no drain report".to_string();
    }
    format!(
        "{} delivered, {} dropped{}",
        report["delivered"],
        report["dropped"],
        if report["deadline_reached"] == true {
            ", at its deadline"
        } else {
            ""
        }
    )
}

/// The plan node that counts what the recipe's main `emit` would have sent a recipient that is gone, once it delivers
/// to those that remain: the counted drop beside it.
const MAIN_EMIT_MISSING: &str = "metrics_fanout_0_missing";

/// Returns `routing` without the intake `name`, which then receives nothing.
fn without(mut routing: Routing, name: &str) -> Routing {
    routing.remove(name);
    routing
}

/// Returns `routing` with the intake `name` receiving `families`.
fn with_intake(mut routing: Routing, name: &str, families: &[&str]) -> Routing {
    routing.insert(name.to_string(), families.iter().map(ToString::to_string).collect());
    routing
}

impl Runner {
    /// Runs S8 to S11, from the default program (after S6). `recipe` is S2's edit of the default program as the editor
    /// showed it at the start.
    pub(super) async fn destination_steps(&mut self, recipe: &str) -> Result<(), GenericError> {
        // S8: under the default program, an added endpoint dual-ships, as natively.
        let mut step = StepReport::new(
            "S8a",
            "add destination D under the default program: it receives everything",
        );
        let result = self
            .add_step(
                &mut step,
                |added| with_intake(default_routing(), &added.name, &FAMILIES),
                true,
            )
            .await;
        let d = self.finish_with(step, result)?;
        let mut step = StepReport::new("S8b", "remove destination D: its recipient drains and retires");
        let result = self.remove_step(&mut step, &d, &default_routing(), None).await;
        self.finish(step, result)?;

        // S9: under a published program, an added endpoint receives only what the program routes to it.
        self.step(
            "S9a",
            "publish the recipe again: team B narrowed to checkout",
            recipe.to_string(),
            Compiled::Clean { unchanged: false },
            Installed::Active {
                new_generation: true,
                routing: Some(narrowed_routing()),
            },
        )
        .await?;
        let mut step = StepReport::new("S9b", "add destination E under it: declared, not addressed");
        let result = self
            .add_step(
                &mut step,
                |added| with_intake(narrowed_routing(), &added.name, &[]),
                false,
            )
            .await;
        let e = self.finish_with(step, result)?;
        let routed = with_intake(narrowed_routing(), &e.name, &[PLATFORM_FAMILY]);
        self.step(
            "S9c",
            "route team:platform to destination E",
            edits::route_platform_to(recipe, &e.url)?,
            Compiled::Clean { unchanged: false },
            Installed::Active {
                new_generation: true,
                routing: Some(routed),
            },
        )
        .await?;

        // S10: the destination a program routes to goes away while it runs.
        let mut step = StepReport::new("S10", "remove destination E while the program routes to it: degraded");
        let result = self
            .remove_step(
                &mut step,
                &e,
                &narrowed_routing(),
                Some(&format!("{}_true", DESTINATION_ROUTE)),
            )
            .await;
        self.finish(step, result)?;

        // S10b: a destination the program names beside others goes away: the others keep receiving, exactly.
        let team_c = Added {
            name: "team-c".to_string(),
            label: "Team C".to_string(),
            url: DemoPorts::http_url(self.ports.team_c),
        };
        let without_c = without(narrowed_routing(), "team-c");
        let mut step = StepReport::new(
            "S10b",
            "remove team C, which the main emit names beside the primary: the primary keeps receiving",
        );
        let result = self
            .remove_step(&mut step, &team_c, &without_c, Some(MAIN_EMIT_MISSING))
            .await;
        self.finish(step, result)?;

        // S11: a native policy change reaches the default program only.
        let mut step = StepReport::new(
            "S11a",
            "set metric_filterlist under the published program: no new generation",
        );
        let result = self.filterlist_under_program_step(&mut step).await;
        self.finish(step, result)?;
        // Team C is gone from here on (S10b).
        let filtered = ["primary", "team-b"]
            .into_iter()
            .fold(without(default_routing(), "team-c"), |routing, intake| {
                changed(routing, intake, &[FILTERLISTED], true)
            });
        self.revoke_step(
            "S11b",
            "revoke: the default program applies the filterlist",
            None,
            &filtered,
        )
        .await?;
        self.native_step(
            "S11c",
            "clear metric_filterlist: the family returns",
            json!({ "metric_filterlist": [] }),
            &without(default_routing(), "team-c"),
        )
        .await
    }

    /// Records a step that returns a value, as [`Runner::finish`] does.
    fn finish_with<T>(&mut self, step: StepReport, result: Result<T, GenericError>) -> Result<T, GenericError> {
        let id = step.id;
        let title = step.title;
        self.report.steps.push(step);
        result.with_error_context(|| format!("{} ({}) failed", id, title))
    }

    /// Records a step: the step is in the report whether it passed or failed.
    pub(super) fn finish(&mut self, step: StepReport, result: Result<(), GenericError>) -> Result<(), GenericError> {
        self.finish_with(step, result)
    }

    /// Adds a destination, waits until ADP's directory names it, with the active plan addressing it or not as
    /// `addressed` says, and checks that the endpoints then receive what `expected` says of the routing with it.
    async fn add_step(
        &mut self, step: &mut StepReport, expected: impl Fn(&Added) -> Routing, addressed: bool,
    ) -> Result<Added, GenericError> {
        let before = self.status().await?;
        let added_at = Instant::now();
        let (status, answer) = self.client.post_json("/api/destinations", &json!({})).await?;
        if !status.is_success() {
            return Err(generic_error!("Adding a destination was refused: {}", answer));
        }
        self.last_change_s = crate::clock::unix_millis() / 1000;
        let field = |name: &str| {
            answer[name]
                .as_str()
                .map(ToString::to_string)
                .ok_or_else(|| generic_error!("Adding a destination answered without its {}: {}", name, answer))
        };
        let added = Added {
            name: field("name")?,
            label: field("label")?,
            url: field("url")?,
        };
        let name = added.name.clone();
        self.wait_for(
            "ADP to declare the destination, as its directory and active plan say",
            ACK_TIMEOUT,
            |state| {
                let intake = intake(state, &name).ok_or_else(|| "the demo lists no such intake".to_string())?;
                if intake["in_directory"] == true && intake["addressed"] == addressed {
                    Ok(())
                } else {
                    Err(format!(
                        "its intake is in ADP's directory: {}, addressed by the active plan: {}",
                        intake["in_directory"], intake["addressed"]
                    ))
                }
            },
        )
        .await?;
        step.ack_ms = Some(added_at.elapsed().as_millis() as u64);
        let visible = self.expect_routing(step.id, &expected(&added), added_at).await?;
        step.visible_ms = Some(visible.as_millis() as u64);
        let after = self.status().await?;
        if after.artifact_revision != before.artifact_revision {
            return Err(generic_error!(
                "ADP runs artifact revision {:?} after the destination was added, not {:?}.",
                after.artifact_revision,
                before.artifact_revision
            ));
        }
        step.generation = after.generation;
        step.outcome = format!(
            "{} at {}, {}; program {}",
            added.label,
            added.url,
            if addressed {
                "receives everything"
            } else {
                "declared, not addressed: receives nothing"
            },
            after.runs().unwrap_or("not reported")
        );
        step.diff = "-".to_string();
        Ok(added)
    }

    /// Removes a destination, waits until ADP retires its recipient and the demo stops its intake, and checks that the
    /// endpoints then receive what `expected` says. With `degraded_at`, the plan node that counts what the program in
    /// effect would have sent the destination: the program stays `active` without a new revision, degraded, its
    /// acknowledgment saying that the emits to the destination are dropped, which the policy loss at that node counts.
    async fn remove_step(
        &mut self, step: &mut StepReport, added: &Added, expected: &Routing, degraded_at: Option<&str>,
    ) -> Result<(), GenericError> {
        let before = self.status().await?;
        let loss_node = degraded_at.map(ToString::to_string);
        let loss_before = match &loss_node {
            Some(node) => policy_loss_at(&self.state().await?, node),
            None => 0.0,
        };
        let removed_at = Instant::now();
        let (status, answer) = self
            .client
            .delete_json(&format!("/api/destinations/{}", added.name))
            .await?;
        if !status.is_success() || answer["removing"] != true {
            return Err(generic_error!("Removing {} was refused: {}", added.label, answer));
        }
        self.last_change_s = crate::clock::unix_millis() / 1000;

        // ADP retires the recipient once it drained, and the demo then stops the intake.
        let (url, name) = (added.url.clone(), added.name.clone());
        let drain = self
            .wait_for(
                "ADP to retire the removed destination's recipient, and the demo to stop its intake",
                RETIRE_TIMEOUT,
                |state| {
                    let retired = recipient(state, &url)
                        .filter(|recipient| recipient["listed"] == false || recipient["state"] == "retired");
                    match (retired, intake(state, &name)) {
                        (Some(recipient), None) => Ok(recipient["last_drain"].clone()),
                        (retired, intake) => Err(format!(
                            "its recipient is {}, and its intake is {}",
                            retired
                                .or_else(|| recipient(state, &url))
                                .map_or_else(|| "not listed".to_string(), |recipient| recipient["state"].to_string()),
                            if intake.is_some() { "still listed" } else { "stopped" }
                        )),
                    }
                },
            )
            .await?;
        step.ack_ms = Some(removed_at.elapsed().as_millis() as u64);

        let mut outcome = format!(
            "{}'s recipient retired: {}; its intake stopped",
            added.label,
            drain_summary(&drain)
        );
        if let (Some(node), Some(revision)) = (&loss_node, before.artifact_revision) {
            let target = format!("to [{}]", edits::quoted(&added.url));
            let (acknowledgment, after) = self
                .wait_ack(POLICY_PRODUCT, revision, removed_at, |acknowledgment| {
                    acknowledgment["state"] == "active"
                        && acknowledgment["error"]
                            .as_str()
                            .is_some_and(|error| dropped_outputs(error).contains(&target.as_str()))
                })
                .await?;
            if after.artifact_revision != Some(revision) {
                return Err(generic_error!(
                    "ADP runs artifact revision {:?}, not the degraded revision {}.",
                    after.artifact_revision,
                    revision
                ));
            }
            let node = node.clone();
            let loss = self
                .wait_for(
                    "the dropped emits to count as policy loss at their node",
                    ACK_TIMEOUT,
                    |state| {
                        let loss = policy_loss_at(state, &node);
                        if loss > loss_before {
                            Ok(loss - loss_before)
                        } else {
                            Err(format!("plan_loss_total{{class=\"policy\"}} at `{}` is {}", node, loss))
                        }
                    },
                )
                .await?;
            step.revision = Some(revision);
            step.generation = after.generation;
            outcome = format!(
                "{}; revision {} active, degraded ({}); {} policy loss at `{}` so far",
                outcome,
                revision,
                acknowledgment["error"].as_str().unwrap_or_default(),
                loss,
                node
            );
        }
        let visible = self.expect_routing(step.id, expected, removed_at).await?;
        step.visible_ms = Some(visible.as_millis() as u64);
        step.outcome = outcome;
        step.diff = "-".to_string();
        Ok(())
    }

    /// S11a: sets `metric_filterlist` in ADP's configuration file while a published program runs, and checks that ADP
    /// makes no new generation, keeps running the program as it was, and lists the filterlist as differing.
    async fn filterlist_under_program_step(&mut self, step: &mut StepReport) -> Result<(), GenericError> {
        let before = self.status().await?;
        let changed_at = Instant::now();
        let (status, answer) = self
            .client
            .post_json("/api/native", &json!({ "metric_filterlist": [FILTERLISTED] }))
            .await?;
        if !status.is_success() {
            return Err(generic_error!("Setting the filterlist was refused: {}", answer));
        }
        self.last_change_s = crate::clock::unix_millis() / 1000;
        let deadline = changed_at + ACK_TIMEOUT;
        let after = loop {
            let status = self.status().await?;
            if status.differs().contains(&FILTERLIST_BINDING) {
                break status;
            }
            if Instant::now() >= deadline {
                return Err(generic_error!(
                    "ADP's status does not list `{}` as differing from the published program {} s after the \
                     filterlist changed: {}",
                    FILTERLIST_BINDING,
                    ACK_TIMEOUT.as_secs(),
                    status.document["program"]
                ));
            }
            tokio::time::sleep(POLL).await;
        };
        step.ack_ms = Some(changed_at.elapsed().as_millis() as u64);
        // The program sends what it sent, the filterlisted family included, and stays in the generation it ran in.
        let visible = self
            .expect_routing(step.id, &without(narrowed_routing(), "team-c"), changed_at)
            .await?;
        step.visible_ms = Some(visible.as_millis() as u64);
        let held = self.status().await?;
        for status in [&after, &held] {
            if (status.generation, status.artifact_revision) != (before.generation, before.artifact_revision) {
                return Err(generic_error!(
                    "After the filterlist changed, ADP runs generation {:?} (artifact {:?}), not generation {:?} \
                     (artifact {:?}) as before: a native policy change made a generation under a published program.",
                    status.generation,
                    status.artifact_revision,
                    before.generation,
                    before.artifact_revision
                ));
            }
        }
        step.generation = held.generation;
        step.outcome = format!(
            "generation {} kept, artifact revision {} in effect; status lists {} as differing",
            held.generation.unwrap_or_default(),
            held.artifact_revision.unwrap_or_default(),
            held.differs().join(", ")
        );
        step.diff = "-".to_string();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_steps_read_a_destinations_intake_recipient_and_loss_from_the_demos_state() {
        let state = json!({
            "intakes": [{"name": "primary"}, {"name": "dest-e", "in_directory": true, "addressed": false}],
            "recipients": {"recipients": [
                {"id": "dd_out/additional/0/http://127.0.0.1:50001", "incarnation": 1, "state": "retired",
                 "listed": false, "last_drain": {"delivered": 4, "dropped": 0, "deadline_reached": false}},
                {"id": "dd_out/additional/0/http://127.0.0.1:50001", "incarnation": 2, "state": "active",
                 "listed": true},
                {"id": "dd_out/additional/0/http://127.0.0.1:500011", "incarnation": 9, "state": "active"}
            ]},
            "telemetry": {"plan": [
                {"name": "plan_loss_total", "labels": {"class": "policy", "component_id": "policy/to_destination_true"},
                 "value": 7.0},
                {"name": "plan_loss_total", "labels": {"class": "native", "component_id": "to_destination_true"},
                 "value": 100.0}
            ]}
        });
        assert_eq!(intake(&state, "dest-e").unwrap()["addressed"], false);
        assert!(intake(&state, "dest-d").is_none());
        // The latest incarnation of the endpoint's own recipient, not one whose URL only starts the same.
        assert_eq!(recipient(&state, "http://127.0.0.1:50001").unwrap()["incarnation"], 2);
        assert_eq!(policy_loss_at(&state, "to_destination_true"), 7.0);
        let retired = &state["recipients"]["recipients"][0]["last_drain"];
        assert_eq!(drain_summary(retired), "4 delivered, 0 dropped");
        assert_eq!(drain_summary(&Value::Null), "no drain report");
    }
}
