//! Steps S12 to S14 of the check, in a second launch of ADP whose metrics path runs on lanes: the check's
//! `--decoders`, `--state-lanes`, and `--post-lanes` (2, 2, and 3 by default), with 10-second aggregation windows.
//! Lane counts are fixed when ADP boots, so these steps cannot run in the first launch.
//!
//! - **S12a**: the frame runs on the lanes asked for: the explain document's layout has those counts, the producer
//!   lanes' exit into aggregation routes by key (each state lane owning a contiguous range of the 256 key groups),
//!   their passthrough exit by affinity, and aggregation's output spreads over the post-aggregation lanes; and every
//!   state and post-aggregation lane receives events (`plan_lane_events_total`).
//! - **S12b**: a counter whose state lane is empty at a tick (fixture X1 of the stages 7 and 9 plan, end to end). The
//!   traffic is cut to the canary alone, of one shard: one context in one state lane, until another state lane has
//!   held nothing for two aggregation windows; then probe counters, one new context each, are sent one at a time until
//!   one lands in the empty lane (its `plan_lane_events_total` grows by exactly one). Natively, a counter first seen
//!   after an idle period gains zeros back to the previous flush of its one aggregation core, which holds the canary
//!   and flushed a second ago; with lanes, an empty lane's own last flush is windows old, so each probe must still
//!   arrive as one point of 1 at its bucket and no zero before it, whichever lane holds it (the lanes share the
//!   lineage's flush log). Every probe arriving exactly once is the conservation check of the probes over both lanes.
//! - **S13a**: a width change, 10 s to 30 s, in the default program, the program the first launch edits
//!   too: the artifact edits `window` of DogStatsD aggregation and of the timestamp split ahead of it, and installs at
//!   a cut, at a multiple of 30 s (`plan_cuts_total{outcome="completed"}`). Every state
//!   lane crosses on its own (`plan_lane_crossings_total{kind="cut"}`, with more than one state lane), and every count
//!   is once in its bucket: the canaries of the last 10-second bucket before the cut and of the first 30-second bucket
//!   after it balance, each bucket whole at its start.
//! - **S13b**: revoking the artifact brings 10-second buckets back at the next cut, with the same checks. A revoke
//!   undoes everything the artifact changed, routing included, so the routing steps come after it.
//! - **S14a**: the default program with its aggregation renamed `agg`. A node's ID is its name, so `agg` is a new
//!   lineage: the generation installs at a cut (`plan_cuts_total{outcome="completed"}`), where `dsd_agg`'s lineage
//!   ends and `agg`'s starts, and the canary check balances across it, as across any cut. The editor warns that the
//!   host's telemetry contract reads the aggregator's series from a node named `dsd_agg`, which the program lacks.
//! - **S14b**: the renamed aggregation with `demo.debug.*` dropped ahead of the policy: a new generation, which keeps
//!   `agg`'s state: no cut is armed, completed, or given up (`plan_cuts_total`). Since V7a, `agg` runs in a sub-region
//!   of the slot its input comes from, on one lane, so the state lanes of the slot `dsd_agg` count no events.
//!
//! The second launch's counters expire one second after their last update (`dogstatsd_expiry_seconds: 1`), rather
//! than never, as the first launch's do (`0`): an aggregation keeps a counter without expiry forever, so a state lane
//! that ever held one would never be empty, and S12b could not empty one. Throughout, the canary check runs as in the
//! first launch, on ranges aligned to every width in use (the least common multiple of 10 and 30 s once S13a
//! published). The traffic's canary is sixteen contexts ([`CANARY_SHARDS`](crate::traffic::CANARY_SHARDS)), which the
//! keyed exit spreads over the state lanes, so that the canary check, the whole run's and S13's buckets, covers every
//! state lane's crossings, not one lane's (all sixteen land in one of two lanes once in 32,768 launches).

use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use saluki_error::{generic_error, ErrorContext as _, GenericError};
use serde_json::{json, Value};
use tokio::net::UdpSocket;

use super::{changed, default_routing, edits, Compiled, Installed, Runner, StepReport, Target, BASELINE_TIMEOUT, POLL};
use crate::{
    canary::{lcm, LAG_SECS},
    clock::unix_millis,
    settings::{DemoPorts, LaneFlags, RunSettings},
    traffic::TrafficMix,
};

/// The slot of the producer lanes.
const PRODUCER_SLOT: &str = "dsd_pre_aggregation";

/// The slot of the state lanes: DogStatsD aggregation.
const STATE_SLOT: &str = "dsd_agg";

/// The slot of the post-aggregation lanes.
const POST_SLOT: &str = "metrics_enrich";

/// The width S13 changes aggregation to, in seconds.
const NEW_WIDTH_S: u64 = 30;

/// The most producer lanes ADP runs: one per decoder up to this many.
const PRODUCER_LANES_MAX: u16 = 16;

/// The counter S12b's probes count, one context per probe (`probe:<n>`).
const PROBE_METRIC: &str = "demo.lanes.probe";

/// How many probes S12b sends at most before it gives up finding the empty lane: each lands in it with a chance of one
/// in the number of state lanes, so twelve all miss one of two lanes once in 4,096 runs.
const MAX_PROBES: usize = 12;

/// How long S12b waits to see which lane a probe reached: the lanes publish once a second, and the demo scrapes once
/// a second.
const PROBE_SEEN: Duration = Duration::from_secs(5);

/// The outcomes `plan_cuts_total` counts: no cut may change any of them while the aggregation is renamed.
pub(super) const CUT_OUTCOMES: [&str; 3] = ["completed", "cancelled", "not_armed"];

/// Returns the values of a `plan_lane_*` series by lane, for the lanes of `component`, from a sample of the demo's
/// state; with `kind`, of the series with that `kind` label only.
fn lane_values(state: &Value, name: &str, component: &str, kind: Option<&str>) -> BTreeMap<u16, f64> {
    state["telemetry"]["plan"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|entry| entry["name"] == name && entry["labels"]["component_id"] == component)
        .filter(|entry| kind.is_none_or(|kind| entry["labels"]["kind"] == kind))
        .filter_map(|entry| {
            let lane = entry["labels"]["lane"].as_str()?.parse::<u16>().ok()?;
            Some((lane, entry["value"].as_f64()?))
        })
        .fold(BTreeMap::new(), |mut lanes, (lane, value)| {
            *lanes.entry(lane).or_insert(0.0) += value;
            lanes
        })
}

/// Returns the value of a plan-level series with one label, if the scraper has it.
fn plan_value(state: &Value, name: &str, label: (&str, &str)) -> Option<f64> {
    state["telemetry"]["plan"]
        .as_array()?
        .iter()
        .find(|entry| entry["name"] == name && entry["labels"][label.0] == label.1)
        .and_then(|entry| entry["value"].as_f64())
}

/// Returns the cut installations ADP counted as ending with `outcome`.
pub(super) fn cuts(state: &Value, outcome: &str) -> f64 {
    plan_value(state, "plan_cuts_total", ("outcome", outcome)).unwrap_or_default()
}

/// Returns how many state lanes DogStatsD aggregation runs on, from the Lanes panel's view in a sample of the demo's
/// state: one if the view does not say.
pub(super) fn state_lanes(state: &Value) -> u64 {
    lane_slot(state, STATE_SLOT)
        .and_then(|slot| slot["lanes"].as_u64())
        .unwrap_or(1)
}

/// Returns the slot named `component` of the Lanes panel's view in a sample of the demo's state.
fn lane_slot<'a>(state: &'a Value, component: &str) -> Option<&'a Value> {
    state["lanes"]["slots"]
        .as_array()?
        .iter()
        .find(|slot| slot["component"] == component)
}

/// Returns the exit of the slot `from` that leads to `to`, among those that reach lanes, from the Lanes panel's view.
fn lane_exit<'a>(state: &'a Value, from: &str, to: &str) -> Option<&'a Value> {
    lane_slot(state, from)?["exits"]
        .as_array()?
        .iter()
        .find(|exit| exit["to"] == to)
}

/// Returns the lane counts the launcher asked for: the producer lanes from the decoders, up to sixteen. A count the
/// launcher left to ADP, or asked ADP to derive (`0`, its automatic count), is `None`.
fn asked_lanes(lanes: &LaneFlags) -> [(&'static str, Option<u16>); 3] {
    let given = |count: Option<u16>| count.filter(|count| *count > 0);
    [
        (
            "dogstatsd_lanes",
            given(lanes.decoders).map(|decoders| decoders.min(PRODUCER_LANES_MAX)),
        ),
        ("state_lanes", given(lanes.state)),
        ("post_lanes", given(lanes.post)),
    ]
}

/// Checks a keyed exit's key groups: every lane of `lanes` owns one contiguous range, in lane order, and the ranges
/// cover the 256 groups once. Returns them, in a few words.
fn check_key_groups(exit: &Value, lanes: u16) -> Result<String, String> {
    let groups = exit["key_groups"].as_array().cloned().unwrap_or_default();
    let mut next = 0;
    let mut described = Vec::new();
    for (lane, range) in groups.iter().enumerate() {
        let (first, last) = (range["first"].as_u64(), range["last"].as_u64());
        match (range["lane"].as_u64(), first, last) {
            (Some(at), Some(first), Some(last)) if at == lane as u64 && first == next && last >= first => {
                described.push(format!("lane {} {}–{}", lane, first, last));
                next = last + 1;
            }
            _ => {
                return Err(format!(
                    "the key groups are not contiguous ranges in lane order: {}",
                    exit
                ))
            }
        }
    }
    if groups.len() != usize::from(lanes) || next != 256 {
        return Err(format!(
            "the key groups do not give each of the {} state lanes a share of the 256 groups: {}",
            lanes, exit
        ));
    }
    Ok(described.join(", "))
}

/// One probe of S12b.
#[derive(Debug)]
struct Probe {
    /// The probe's tag, which makes it a context of its own.
    tag: String,

    /// When it was sent, in milliseconds since the Unix epoch.
    sent_ms: u64,

    /// Whether it reached the empty lane.
    in_empty_lane: bool,
}

/// Returns the points the intake's recent series hold of the context tagged `tag` of `metric`, as `(timestamp,
/// value)`, in the order they arrived.
fn points_of(recent: &Value, metric: &str, tag: &str) -> Vec<(u64, f64)> {
    let series = recent["series"].as_array().map(Vec::as_slice).unwrap_or_default();
    series
        .iter()
        // Recent series are listed most recent first.
        .rev()
        .filter(|series| series["name"] == metric)
        .filter(|series| {
            series["tags"]
                .as_array()
                .is_some_and(|tags| tags.iter().any(|candidate| candidate == tag))
        })
        .flat_map(|series| series["values"].as_array().cloned().unwrap_or_default())
        .filter_map(|point| Some((point["timestamp"].as_u64()?, point["value"].as_f64()?)))
        .collect()
}

/// Checks a probe's points: one point of 1, at the start of the bucket of `width_s` seconds it was sent in (or the
/// next, when `edge` allows it, for a probe sent too near a bucket's end to tell), and nothing else, no zero
/// included.
fn check_probe(points: &[(u64, f64)], sent_ms: u64, width_s: u64, edge: bool) -> Result<u64, String> {
    let bucket = (sent_ms / 1000 / width_s) * width_s;
    match points {
        [(timestamp, value)] if *value == 1.0 && (*timestamp == bucket || (edge && *timestamp == bucket + width_s)) => {
            Ok(*timestamp)
        }
        [] => Err("it never arrived".to_string()),
        points => Err(format!(
            "it arrived as {:?} (timestamp, count), where one point of 1 at {} was expected",
            points, bucket
        )),
    }
}

impl Runner {
    /// Runs S12 to S14 against a demo whose ADP runs on the lanes `settings` asks for.
    pub(super) async fn lane_scenarios(&mut self, settings: &RunSettings) -> Result<(), GenericError> {
        let (source, _) = self.baseline().await?;

        let mut step = StepReport::new("S12a", "the frame runs on the lanes asked for");
        let result = self.lanes_step(&mut step, &settings.lanes).await;
        self.finish(step, result)?;
        let state_lanes = state_lanes(&self.state().await?);

        let mut step = StepReport::new("S12b", "a counter whose state lane is empty at a tick");
        let result = if state_lanes < 2 {
            step.outcome = "not run: aggregation runs on one state lane, and this step needs two or more".to_string();
            Ok(())
        } else {
            self.empty_lane_step(&mut step, settings).await
        };
        self.finish(step, result)?;

        self.width_change_step(&source, state_lanes).await?;
        let mut step = StepReport::new("S13b", "revoke the width change: 10-second buckets again at a cut");
        let result = self.revoke_width_step(&mut step, state_lanes).await;
        self.finish(step, result)?;

        self.rename_steps(&source).await?;
        self.whole_run_canary().await
    }

    /// S14a and S14b: the aggregation renamed is a new lineage, which starts at a cut; renamed, with a routing change,
    /// it keeps its state: no cut.
    async fn rename_steps(&mut self, source: &str) -> Result<(), GenericError> {
        let renamed = edits::rename_aggregation(source)?;
        let completed = cuts(&self.state().await?, "completed");
        let published = Instant::now();
        let target = Target {
            ack_timeout: Self::cut_timeout(self.window_s, self.window_s),
        };
        let title = "rename the aggregation agg: a new lineage, at a cut";
        self.step_in(
            target,
            "S14a",
            title,
            renamed.clone(),
            Compiled::Warned {
                fragment: "telemetry_contract",
            },
            Installed::Active {
                new_generation: true,
                routing: Some(default_routing()),
            },
        )
        .await?;
        let seen_s = self
            .wait_cut(completed, published, target.ack_timeout)
            .await
            .with_error_context(|| format!("S14a ({}) failed", title))?;
        if let Some(step) = self.report.steps.last_mut() {
            step.outcome = format!("{}; cut seen at {}", step.outcome, seen_s);
        }

        let cuts_of = |state: &Value| CUT_OUTCOMES.map(|outcome| cuts(state, outcome));
        let state = self.state().await?;
        let before = cuts_of(&state);
        let events_before = lane_values(&state, "plan_lane_events_total", STATE_SLOT, None);
        let no_debug = ["primary", "team-b", "team-c"]
            .into_iter()
            .fold(default_routing(), |routing, intake| {
                changed(routing, intake, &["demo.debug.*"], true)
            });
        self.step(
            "S14b",
            "renamed, with demo.debug.* dropped: a new generation, no cut",
            edits::drop_debug(&renamed)?,
            Compiled::Warned {
                fragment: "telemetry_contract",
            },
            Installed::Active {
                new_generation: true,
                routing: Some(no_debug),
            },
        )
        .await?;
        let state = self.state().await?;
        let after = cuts_of(&state);
        if after != before {
            return Err(generic_error!(
                "S14b: a routing change of the renamed aggregation's program changed plan_cuts_total (completed, \
                 cancelled, not_armed) from {:?} to {:?}: its state was cut.",
                before,
                after
            ));
        }
        // Since V7a, the renamed aggregation runs in a sub-region of the slot its input comes from, on one lane, not
        // on the state lanes the boot layout gave `dsd_agg`, which then receive nothing.
        let events = lane_values(&state, "plan_lane_events_total", STATE_SLOT, None);
        if events != events_before {
            return Err(generic_error!(
                "S14b: the state lanes of `{}` counted events since the routing change, though the renamed \
                 aggregation runs in a sub-region: {:?}, before {:?}.",
                STATE_SLOT,
                events,
                events_before
            ));
        }
        if let Some(step) = self.report.steps.last_mut() {
            step.outcome = format!(
                "{}; plan_cuts_total unchanged {:?}; the state lanes of `{}` idle, `agg` in a sub-region",
                step.outcome, after, STATE_SLOT
            );
        }
        Ok(())
    }

    /// Sets the traffic generator's mix.
    async fn set_mix(&mut self, mix: TrafficMix) -> Result<(), GenericError> {
        let (status, answer) = self.client.post_json("/api/traffic", &json!({ "mix": mix })).await?;
        if status.is_success() && answer["mix"] == json!(mix) {
            Ok(())
        } else {
            Err(generic_error!("Setting the traffic's mix was refused: {}", answer))
        }
    }

    /// S12a: waits for the Lanes panel's layout, checks it against the lanes asked for, and waits until every state
    /// and post-aggregation lane has received events.
    async fn lanes_step(&mut self, step: &mut StepReport, asked: &LaneFlags) -> Result<(), GenericError> {
        let started = Instant::now();
        let state = self
            .wait_for("the lanes ADP's explain document lays out", BASELINE_TIMEOUT, |state| {
                let lanes = &state["lanes"];
                if lanes["layout_known"] == true && !lanes["policy"].is_null() {
                    Ok(state.clone())
                } else {
                    Err(format!(
                        "the Lanes panel has no layout ({}; error: {})",
                        if lanes["layout_known"] == true {
                            "the frame is not laid out on lanes"
                        } else {
                            "ADP's explain document was not read"
                        },
                        lanes["layout_error"]
                    ))
                }
            })
            .await?;
        let policy = &state["lanes"]["policy"];
        for (member, count) in asked_lanes(asked) {
            if let Some(count) = count {
                if policy[member].as_u64() != Some(u64::from(count)) {
                    return Err(generic_error!(
                        "ADP laid its metrics path out on {} {}, not the {} asked for: {}",
                        policy[member],
                        member,
                        count,
                        policy
                    ));
                }
            }
        }
        if policy["pipelining"] != false {
            return Err(generic_error!(
                "R1's cost model still pipelines the slots on lanes: {}",
                policy
            ));
        }
        let count = |member: &str| policy[member].as_u64().unwrap_or(1) as u16;
        let (producer_lanes, state_lanes, post_lanes) =
            (count("dogstatsd_lanes"), count("state_lanes"), count("post_lanes"));

        // How each exit reaches the lanes of the slot it leads to; an exit into a slot on one lane reaches no lanes.
        let routing = |from: &str, to: &str| lane_exit(&state, from, to).map(|exit| exit["routing"].clone());
        let expected = |lanes: u16, routing: &str| (lanes > 1).then(|| json!(routing));
        let checks = [
            (PRODUCER_SLOT, STATE_SLOT, expected(state_lanes, "keyed")),
            (PRODUCER_SLOT, POST_SLOT, expected(post_lanes, "affinity")),
            (STATE_SLOT, POST_SLOT, expected(post_lanes, "spread")),
        ];
        for (from, to, wanted) in checks {
            if routing(from, to) != wanted {
                return Err(generic_error!(
                    "The exit of `{}` into `{}` reaches its lanes as {:?}, not {:?}.",
                    from,
                    to,
                    routing(from, to),
                    wanted
                ));
            }
        }
        let routed = match lane_exit(&state, PRODUCER_SLOT, STATE_SLOT) {
            Some(exit) => format!(
                "keyed by groups {}",
                check_key_groups(exit, state_lanes).map_err(|error| generic_error!("{}", error))?
            ),
            None => "not keyed".to_string(),
        };

        // Every state and post-aggregation lane receives events: the keyed exit's routing, and the spread of each
        // state lane's flushes over the post-aggregation lanes, which takes a few ticks to reach every lane.
        let timeout = BASELINE_TIMEOUT.max(Duration::from_secs(9 * self.window_s));
        let events = self
            .wait_for(
                "every state and post-aggregation lane to receive events",
                timeout,
                |state| {
                    let events = |component: &str| lane_values(state, "plan_lane_events_total", component, None);
                    let (producers, states, posts) = (events(PRODUCER_SLOT), events(STATE_SLOT), events(POST_SLOT));
                    let busy =
                        |lanes: &BTreeMap<u16, f64>, count: u16| (0..count).all(|lane| lanes.get(&lane) > Some(&0.0));
                    if busy(&states, state_lanes)
                        && busy(&posts, post_lanes)
                        && producers.values().any(|events| *events > 0.0)
                    {
                        Ok((producers, states, posts))
                    } else {
                        Err(format!(
                            "events by lane: producer {:?}, state {:?}, post-aggregation {:?}",
                            producers, states, posts
                        ))
                    }
                },
            )
            .await?;
        let listed = |lanes: &BTreeMap<u16, f64>| {
            lanes
                .values()
                .map(|events| format!("{}", events))
                .collect::<Vec<_>>()
                .join("/")
        };
        step.visible_ms = Some(started.elapsed().as_millis() as u64);
        step.outcome = format!(
            "producer lanes {} (events {}), state lanes {} {} (events {}), post-aggregation lanes {} (events {})",
            producer_lanes,
            listed(&events.0),
            state_lanes,
            routed,
            listed(&events.1),
            post_lanes,
            listed(&events.2)
        );
        step.diff = "-".to_string();
        Ok(())
    }

    /// S12b: empties a state lane, then sends probes until one lands in it, and checks that every probe arrives once,
    /// as one point at its bucket and no zero before it. The traffic's mix is restored whatever happens.
    async fn empty_lane_step(&mut self, step: &mut StepReport, settings: &RunSettings) -> Result<(), GenericError> {
        self.set_mix(TrafficMix::CanaryOnly).await?;
        self.last_change_s = unix_millis() / 1000;
        let result = self.probe_empty_lane(step, settings).await;
        let restored = self.set_mix(TrafficMix::Full).await;
        result.and(restored)?;
        // Every family comes back, and the probes leave.
        let visible = self.expect_routing(step.id, &default_routing(), Instant::now()).await?;
        step.visible_ms = Some(step.visible_ms.unwrap_or_default() + visible.as_millis() as u64);
        step.diff = "-".to_string();
        Ok(())
    }

    async fn probe_empty_lane(&mut self, step: &mut StepReport, settings: &RunSettings) -> Result<(), GenericError> {
        let width_s = self.window_s;
        let started = Instant::now();
        // A lane whose own last flush is two windows old: emptied, and quiet for two windows since.
        let quiet_for = Duration::from_secs(2 * width_s + 4);

        // The ticks the lanes counted as divergent so far: a lane that cannot start its zeros where the lanes left
        // them falls back to its own last flush, and counts it.
        let divergent = |state: &Value| {
            lane_values(state, "plan_lane_tick_divergence_total", STATE_SLOT, None)
                .values()
                .sum::<f64>()
        };
        let divergent_before = divergent(&self.state().await?);

        // Which lane empties: the canary's context stays in one lane, every other lane stops receiving events.
        let mut changed: BTreeMap<u16, (f64, Instant)> = BTreeMap::new();
        let deadline = Instant::now() + quiet_for + Duration::from_secs(2 * width_s + 30);
        let empty = loop {
            let state = self.state().await?;
            let now = Instant::now();
            for (lane, events) in lane_values(&state, "plan_lane_events_total", STATE_SLOT, None) {
                match changed.get(&lane) {
                    Some((seen, _)) if *seen == events => {}
                    _ => {
                        changed.insert(lane, (events, now));
                    }
                }
            }
            let contexts = lane_values(&state, "plan_lane_contexts", STATE_SLOT, None);
            let active = changed.values().any(|(_, at)| now - *at < Duration::from_secs(3));
            let quiet = changed
                .iter()
                .find(|(lane, (_, at))| now - *at >= quiet_for && contexts.get(lane).is_none_or(|held| *held == 0.0))
                .map(|(lane, _)| *lane);
            if let (true, Some(lane)) = (active, quiet) {
                break lane;
            }
            if now >= deadline {
                return Err(generic_error!(
                    "With the canary alone, no state lane went without events for {} s while another received the \
                     canary: events by lane {:?}, contexts {:?}.",
                    quiet_for.as_secs(),
                    changed
                        .iter()
                        .map(|(lane, (events, _))| (*lane, *events))
                        .collect::<BTreeMap<_, _>>(),
                    contexts
                ));
            }
            tokio::time::sleep(POLL).await;
        };
        let quiet_since = changed[&empty].1;

        // Probes, one context each, until one lands in the empty lane.
        let socket = UdpSocket::bind(DemoPorts::loopback(0))
            .await
            .error_context("Failed to bind a UDP socket for the probes.")?;
        let target = DemoPorts::loopback(settings.ports.dogstatsd);
        // Away from a bucket's edges, so that the bucket a probe is stamped in is the one it was sent in.
        let guard = width_s >= 5;
        let mut probes = Vec::new();
        while probes.len() < MAX_PROBES {
            if guard {
                loop {
                    let into_bucket = (unix_millis() / 1000) % width_s;
                    if (1..width_s - 3).contains(&into_bucket) {
                        break;
                    }
                    tokio::time::sleep(POLL).await;
                }
            }
            let before = lane_values(&self.state().await?, "plan_lane_events_total", STATE_SLOT, None);
            let tag = format!("probe:{}", probes.len());
            let sent_ms = unix_millis();
            socket
                .send_to(format!("{}:1|c|#{}", PROBE_METRIC, tag).as_bytes(), target)
                .await
                .error_context("Failed to send a probe.")?;
            let seen = Instant::now() + PROBE_SEEN;
            let mut grew = 0.0;
            while Instant::now() < seen {
                tokio::time::sleep(POLL).await;
                let events = lane_values(&self.state().await?, "plan_lane_events_total", STATE_SLOT, None);
                grew =
                    events.get(&empty).copied().unwrap_or_default() - before.get(&empty).copied().unwrap_or_default();
                if grew != 0.0 {
                    break;
                }
            }
            if grew > 1.0 {
                return Err(generic_error!(
                    "The empty state lane {} received {} events for one probe: something besides the canary and the \
                     probes reached it.",
                    empty,
                    grew
                ));
            }
            probes.push(Probe {
                tag,
                sent_ms,
                in_empty_lane: grew == 1.0,
            });
            if grew == 1.0 {
                break;
            }
        }
        let Some(landed) = probes.iter().position(|probe| probe.in_empty_lane) else {
            return Err(generic_error!(
                "None of {} probes reached the empty state lane {}: each lands in it with a chance of one in the \
                 number of state lanes.",
                probes.len(),
                empty
            ));
        };
        let quiet_s = (Instant::now() - quiet_since).as_secs();

        // Every probe arrives, as one point. A zero before it would arrive with it or before it, and a later one within
        // a window: once every probe arrived, the intake's record is read again a window later. One that never arrives
        // is reported below.
        let deadline = Instant::now() + Duration::from_secs(2 * width_s + LAG_SECS + 10);
        loop {
            let recent = self.client.get_json("/api/intakes/primary/recent").await?;
            let arrived = probes
                .iter()
                .all(|probe| !points_of(&recent, PROBE_METRIC, &probe.tag).is_empty());
            if arrived || Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(POLL).await;
        }
        tokio::time::sleep(self.stable()).await;
        let recent = self.client.get_json("/api/intakes/primary/recent").await?;
        let mut described = Vec::new();
        for (index, probe) in probes.iter().enumerate() {
            let points = points_of(&recent, PROBE_METRIC, &probe.tag);
            let timestamp = check_probe(&points, probe.sent_ms, width_s, !guard).map_err(|problem| {
                generic_error!(
                    "Probe {} ({}, sent at {} ms, {}): {}",
                    index,
                    probe.tag,
                    probe.sent_ms,
                    if probe.in_empty_lane {
                        format!("in state lane {}, empty for {} s", empty, quiet_s)
                    } else {
                        "in another state lane".to_string()
                    },
                    problem
                )
            })?;
            if index == landed {
                described.push(format!(
                    "probe {} on empty lane {}: one point of 1 at {}",
                    index, empty, timestamp
                ));
            }
        }
        let divergent_since = divergent(&self.state().await?) - divergent_before;
        step.visible_ms = Some(started.elapsed().as_millis() as u64);
        step.outcome = format!(
            "lane {} empty for {} s; {}, no zero before it; {} probes on other lanes, one point each; {} divergent \
             ticks meanwhile",
            empty,
            quiet_s,
            described.join(""),
            probes.len() - 1,
            divergent_since
        );
        Ok(())
    }

    /// Returns what the canary check says of a range of seconds.
    async fn canary_range(&mut self, from_s: u64, to_s: u64) -> Result<Value, GenericError> {
        self.client
            .get_json(&format!("/api/canary?from_s={}&to_s={}", from_s, to_s))
            .await
    }

    /// Checks the canary's buckets around a cut at `boundary`: the last bucket before it, `before_s` wide, and the
    /// first after it, `after_s` wide, each balance and hold all of their canaries at their start. Waits until the
    /// bucket after the cut was delivered. Returns what it found, in a few words.
    async fn check_buckets(&mut self, boundary: u64, before_s: u64, after_s: u64) -> Result<String, GenericError> {
        let delivered_ms = (boundary + after_s + 2 + LAG_SECS) * 1000;
        let now_ms = unix_millis();
        if now_ms < delivered_ms {
            tokio::time::sleep(Duration::from_millis(delivered_ms - now_ms)).await;
        }
        // What the primary endpoint received around the cut, by bucket start, for the error if a check fails.
        let around = self.canary_range(boundary - 2 * before_s, boundary + after_s).await?;
        let around = &around["received_by_timestamp"];
        let mut found = Vec::new();
        for (from_s, width_s, which) in [(boundary - before_s, before_s, "before"), (boundary, after_s, "after")] {
            let whole = self.canary_range(from_s, from_s + width_s).await?;
            if whole["state"] != "balanced" {
                // The plan's own counts of what it lost, which name the node and the class of any loss.
                let state = self.state().await?;
                let losses = state["telemetry"]["plan"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|entry| entry["name"] == "plan_loss_total" && entry["value"].as_f64() > Some(0.0))
                    .map(|entry| {
                        format!(
                            "{} {} {}",
                            entry["labels"]["component_id"], entry["labels"]["class"], entry["value"]
                        )
                    })
                    .collect::<Vec<_>>();
                return Err(generic_error!(
                    "The canaries of the {}-second bucket {} the cut at {} do not balance: {}; received around the \
                     cut by bucket start: {}; plan_loss_total: {:?}",
                    width_s,
                    which,
                    boundary,
                    whole,
                    around,
                    losses
                ));
            }
            // Every canary of the bucket is at its start: no bucket of another width starts inside it.
            if width_s > 1 {
                let inside = self.canary_range(from_s + 1, from_s + width_s).await?;
                if inside["received"].as_f64() != Some(0.0) {
                    return Err(generic_error!(
                        "The {}-second bucket {} the cut at {} is not one bucket: canaries arrived for seconds {} to \
                         {}: {}",
                        width_s,
                        which,
                        boundary,
                        from_s + 1,
                        from_s + width_s,
                        inside
                    ));
                }
            }
            found.push(format!(
                "{} canaries in the {}-second bucket at {}",
                whole["received"], width_s, from_s
            ));
        }
        Ok(found.join(", "))
    }

    /// Waits for ADP to count one more completed cut than `completed`, and returns when it saw it, in Unix seconds.
    pub(super) async fn wait_cut(
        &mut self, completed: f64, since: Instant, timeout: Duration,
    ) -> Result<u64, GenericError> {
        let (cancelled, not_armed) = {
            let state = self.state().await?;
            (cuts(&state, "cancelled"), cuts(&state, "not_armed"))
        };
        let deadline = since + timeout;
        loop {
            let state = self.state().await?;
            if cuts(&state, "completed") > completed {
                return Ok(unix_millis() / 1000);
            }
            if cuts(&state, "cancelled") > cancelled || cuts(&state, "not_armed") > not_armed {
                return Err(generic_error!(
                    "ADP gave the cut up: plan_cuts_total is cancelled {} and not_armed {}.",
                    cuts(&state, "cancelled"),
                    cuts(&state, "not_armed")
                ));
            }
            if Instant::now() >= deadline {
                return Err(generic_error!(
                    "ADP counted no completed cut within {} s (plan_cuts_total{{outcome=\"completed\"}} is {}).",
                    timeout.as_secs(),
                    cuts(&state, "completed")
                ));
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// Waits until every one of `lanes` state lanes counted a cut crossing more than `before`, with more than one
    /// state lane (one lane has no crossings of its own), and returns the lanes' counts.
    async fn wait_crossings(
        &mut self, before: &BTreeMap<u16, f64>, lanes: u64,
    ) -> Result<BTreeMap<u16, f64>, GenericError> {
        if lanes < 2 {
            return Ok(BTreeMap::new());
        }
        self.wait_for(
            "every state lane to count its own cut crossing",
            Duration::from_secs(20),
            |state| {
                let crossings = lane_values(state, "plan_lane_crossings_total", STATE_SLOT, Some("cut"));
                let crossed =
                    (0..lanes as u16).all(|lane| crossings.get(&lane) > Some(before.get(&lane).unwrap_or(&0.0)));
                if crossed {
                    Ok(crossings)
                } else {
                    Err(format!(
                        "plan_lane_crossings_total{{component_id=\"{}\",kind=\"cut\"}} by lane is {:?} (was {:?})",
                        STATE_SLOT, crossings, before
                    ))
                }
            },
        )
        .await
    }

    /// Returns how long a cut between widths `from_s` and `to_s` may take to install and be counted: the least common
    /// multiple of the widths, which the cut waits for at most, one window more, and room for ADP's polls.
    fn cut_timeout(from_s: u64, to_s: u64) -> Duration {
        Duration::from_secs(lcm(from_s, to_s) + from_s.max(to_s) + 20)
    }

    /// S13a: changes aggregation's width from the configured window to [`NEW_WIDTH_S`] in the default program, and
    /// checks the cut it installs at.
    async fn width_change_step(&mut self, source: &str, state_lanes: u64) -> Result<(), GenericError> {
        let (from_s, to_s) = (self.window_s, NEW_WIDTH_S);
        let width = lcm(from_s, to_s);
        let edited = edits::width_change(source, &format!("{}s", to_s))?;
        let state = self.state().await?;
        let completed = cuts(&state, "completed");
        let crossings = lane_values(&state, "plan_lane_crossings_total", STATE_SLOT, Some("cut"));
        let published_s = unix_millis() / 1000;
        let published = Instant::now();
        let target = Target {
            ack_timeout: Self::cut_timeout(from_s, to_s),
        };
        let title = "width change, 10 s to 30 s, in the default program";
        self.step_in(
            target,
            "S13a",
            title,
            edited,
            Compiled::Clean { unchanged: false },
            Installed::Active {
                new_generation: true,
                routing: None,
            },
        )
        .await?;
        let result = async {
            let seen_s = self.wait_cut(completed, published, target.ack_timeout).await?;
            let boundary = (seen_s / width) * width;
            if boundary < published_s {
                return Err(generic_error!(
                    "The cut completed at {} s, which puts it at {}, before the artifact was published at {}.",
                    seen_s,
                    boundary,
                    published_s
                ));
            }
            let crossed = self.wait_crossings(&crossings, state_lanes).await?;
            let buckets = self.check_buckets(boundary, from_s, to_s).await?;
            Ok((boundary, crossed, buckets))
        }
        .await;
        let report = self.report.steps.last_mut().expect("the step was recorded");
        match result {
            Ok((boundary, crossed, buckets)) => {
                report.visible_ms = Some(published.elapsed().as_millis() as u64);
                report.outcome = format!(
                    "{}; cut at {}{}; {}",
                    report.outcome,
                    boundary,
                    crossings_text(&crossed),
                    buckets
                );
                Ok(())
            }
            Err(error) => Err(error).with_error_context(|| format!("S13a ({}) failed", title)),
        }
    }

    /// S13b: revokes the width change, and checks the cut back to the configured window.
    async fn revoke_width_step(&mut self, step: &mut StepReport, state_lanes: u64) -> Result<(), GenericError> {
        let (from_s, to_s) = (NEW_WIDTH_S, self.window_s);
        let width = lcm(from_s, to_s);
        let before = self.status().await?;
        let state = self.state().await?;
        let completed = cuts(&state, "completed");
        let crossings = lane_values(&state, "plan_lane_crossings_total", STATE_SLOT, Some("cut"));
        let revoked = Instant::now();
        let revoked_s = unix_millis() / 1000;
        let (status, answer) = self.client.post_json("/api/revoke", &json!({})).await?;
        if !status.is_success() || answer["revoked"] != true {
            return Err(generic_error!("Revoking the width change was refused: {}", answer));
        }
        self.last_change_s = revoked_s;
        let timeout = Self::cut_timeout(from_s, to_s);
        let deadline = revoked + timeout;
        let after = loop {
            let status = self.status().await?;
            if status.artifact_revision.is_none() && status.generation > before.generation {
                break status;
            }
            if Instant::now() >= deadline {
                return Err(generic_error!(
                    "{} s after the revoke, ADP runs generation {:?} with artifact {:?}, not a new generation without \
                     it.",
                    timeout.as_secs(),
                    status.generation,
                    status.artifact_revision
                ));
            }
            tokio::time::sleep(POLL).await;
        };
        step.ack_ms = Some(revoked.elapsed().as_millis() as u64);
        step.generation = after.generation;
        let seen_s = self.wait_cut(completed, revoked, timeout).await?;
        let boundary = (seen_s / width) * width;
        if boundary < revoked_s {
            return Err(generic_error!(
                "The cut completed at {} s, which puts it at {}, before the revoke at {}.",
                seen_s,
                boundary,
                revoked_s
            ));
        }
        let crossed = self.wait_crossings(&crossings, state_lanes).await?;
        let buckets = self.check_buckets(boundary, from_s, to_s).await?;
        let visible = self.expect_routing(step.id, &default_routing(), revoked).await?;
        step.visible_ms = Some(visible.as_millis() as u64);
        step.outcome = format!(
            "default program in generation {}; cut at {}{}; {}",
            after.generation.unwrap_or_default(),
            boundary,
            crossings_text(&crossed),
            buckets
        );
        step.diff = "-".to_string();
        Ok(())
    }
}

/// Describes the state lanes' cut crossings for an outcome, if there are lanes of their own.
fn crossings_text(crossed: &BTreeMap<u16, f64>) -> String {
    if crossed.is_empty() {
        return String::new();
    }
    format!(
        ", crossed by lanes {}",
        crossed
            .keys()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" and ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sample of the demo's state with lane series and the Lanes panel's view, as the server writes them.
    fn state() -> Value {
        let lane = |name: &str, component: &str, lane: u16, value: f64| {
            let labels = json!({"component_id": component, "lane": lane.to_string()});
            json!({"name": name, "labels": labels, "value": value})
        };
        json!({
            "telemetry": {"plan": [
                lane("plan_lane_events_total", "dsd_agg", 0, 120.0),
                lane("plan_lane_events_total", "dsd_agg", 1, 80.0),
                lane("plan_lane_events_total", "metrics_enrich", 2, 5.0),
                {"name": "plan_lane_crossings_total", "value": 1.0,
                 "labels": {"component_id": "dsd_agg", "lane": "0", "kind": "cut"}},
                {"name": "plan_lane_crossings_total", "value": 2.0,
                 "labels": {"component_id": "dsd_agg", "lane": "0", "kind": "reset"}},
                {"name": "plan_cuts_total", "value": 3.0, "labels": {"outcome": "completed"}},
                {"name": "plan_cuts_total", "value": 0.0, "labels": {"outcome": "cancelled"}}
            ]},
            "lanes": {"slots": [
                {"component": "dsd_pre_aggregation", "lanes": 2, "exits": [
                    {"name": "dsd_agg", "to": "dsd_agg", "routing": "keyed",
                     "key_groups": [{"lane": 0, "first": 0, "last": 127}, {"lane": 1, "first": 128, "last": 255}]}
                ]}
            ]}
        })
    }

    #[test]
    fn lane_series_are_read_by_lane_and_kind_and_cuts_by_outcome() {
        let state = state();
        assert_eq!(
            lane_values(&state, "plan_lane_events_total", "dsd_agg", None),
            BTreeMap::from([(0, 120.0), (1, 80.0)])
        );
        assert_eq!(
            lane_values(&state, "plan_lane_crossings_total", "dsd_agg", Some("cut")),
            BTreeMap::from([(0, 1.0)])
        );
        // Without a kind, every kind's series of a lane sum.
        assert_eq!(
            lane_values(&state, "plan_lane_crossings_total", "dsd_agg", None),
            BTreeMap::from([(0, 3.0)])
        );
        assert!(lane_values(&state, "plan_lane_events_total", "nothing", None).is_empty());
        assert_eq!((cuts(&state, "completed"), cuts(&state, "not_armed")), (3.0, 0.0));
        let exit = lane_exit(&state, "dsd_pre_aggregation", "dsd_agg").unwrap();
        assert_eq!(exit["routing"], "keyed");
        assert!(lane_exit(&state, "dsd_pre_aggregation", "metrics_enrich").is_none());
    }

    #[test]
    fn key_groups_must_give_each_state_lane_one_contiguous_share_of_the_256_groups() {
        let exit = |groups: Value| json!({ "key_groups": groups });
        let groups = |ranges: &[(u64, u64, u64)]| {
            Value::Array(
                ranges
                    .iter()
                    .map(|(lane, first, last)| json!({"lane": lane, "first": first, "last": last}))
                    .collect(),
            )
        };
        assert_eq!(
            check_key_groups(&exit(groups(&[(0, 0, 127), (1, 128, 255)])), 2).unwrap(),
            "lane 0 0–127, lane 1 128–255"
        );
        assert_eq!(
            check_key_groups(&exit(groups(&[(0, 0, 84), (1, 85, 169), (2, 170, 255)])), 3).unwrap(),
            "lane 0 0–84, lane 1 85–169, lane 2 170–255"
        );
        // A gap, an overlap, a lane out of order, groups left over, and a lane without a share are all refused.
        for (ranges, lanes) in [
            (vec![(0, 0, 126), (1, 128, 255)], 2),
            (vec![(0, 0, 128), (1, 128, 255)], 2),
            (vec![(1, 0, 127), (0, 128, 255)], 2),
            (vec![(0, 0, 127), (1, 128, 254)], 2),
            (vec![(0, 0, 127), (1, 128, 255)], 3),
        ] {
            assert!(check_key_groups(&exit(groups(&ranges)), lanes).is_err(), "{:?}", ranges);
        }
    }

    #[test]
    fn a_probe_arrives_as_one_point_of_one_at_its_bucket_and_nothing_else() {
        // Sent at 1,000,047.3 s with 10-second buckets: its bucket starts at 1,000,040.
        let sent_ms = 1_000_047_300;
        assert_eq!(check_probe(&[(1_000_040, 1.0)], sent_ms, 10, false), Ok(1_000_040));
        // The zeros a lane whose own last flush is windows old would add before it, as X1 says it must not.
        let with_zeros = [(1_000_020, 0.0), (1_000_030, 0.0), (1_000_040, 1.0)];
        assert!(check_probe(&with_zeros, sent_ms, 10, false)
            .unwrap_err()
            .contains("arrived as"));
        // Lost, doubled, in the wrong bucket.
        assert_eq!(
            check_probe(&[], sent_ms, 10, false),
            Err("it never arrived".to_string())
        );
        assert!(check_probe(&[(1_000_040, 2.0)], sent_ms, 10, false).is_err());
        assert!(check_probe(&[(1_000_040, 1.0), (1_000_040, 1.0)], sent_ms, 10, false).is_err());
        assert!(check_probe(&[(1_000_050, 1.0)], sent_ms, 10, false).is_err());
        // Without the guard against a bucket's edge, the next bucket is allowed too.
        assert_eq!(check_probe(&[(1_000_050, 1.0)], sent_ms, 10, true), Ok(1_000_050));
    }

    #[test]
    fn a_probes_points_are_read_from_the_intakes_recent_series_by_its_tag() {
        let recent = json!({"series": [
            {"name": "demo.lanes.probe", "tags": ["probe:1", "host:adp-demo"], "values": [
                {"timestamp": 50, "value": 1.0}
            ]},
            {"name": "demo.lanes.probe", "tags": ["probe:0", "host:adp-demo"], "values": [
                {"timestamp": 40, "value": 1.0}
            ]},
            {"name": "demo.lanes.probe", "tags": ["probe:0", "host:adp-demo"], "values": [
                {"timestamp": 20, "value": 0.0}, {"timestamp": 30, "value": 0.0}
            ]},
            {"name": "demo.canary", "tags": ["probe:0"], "values": [{"timestamp": 40, "value": 9.0}]}
        ]});
        // Most recent first in the intake's list; in arrival order here.
        assert_eq!(
            points_of(&recent, PROBE_METRIC, "probe:0"),
            [(20, 0.0), (30, 0.0), (40, 1.0)]
        );
        assert_eq!(points_of(&recent, PROBE_METRIC, "probe:1"), [(50, 1.0)]);
        assert!(points_of(&recent, PROBE_METRIC, "probe:2").is_empty());
    }

    #[test]
    fn the_lanes_asked_for_count_producer_lanes_up_to_sixteen_and_leave_automatic_counts_unchecked() {
        let asked = asked_lanes(&LaneFlags {
            decoders: Some(20),
            state: Some(2),
            post: Some(0),
        });
        assert_eq!(
            asked,
            [
                ("dogstatsd_lanes", Some(16)),
                ("state_lanes", Some(2)),
                ("post_lanes", None)
            ]
        );
        assert_eq!(
            asked_lanes(&LaneFlags::default()).map(|(_, count)| count),
            [None, None, None]
        );
        // Zero decoders is ADP's automatic count, derived from the host, as zero state or post-aggregation lanes are.
        let automatic = LaneFlags {
            decoders: Some(0),
            state: Some(0),
            post: Some(3),
        };
        assert_eq!(asked_lanes(&automatic).map(|(_, count)| count), [None, None, Some(3)]);
        // A cut from 10 to 30 seconds waits for at most 30 s, one 30-second window, and 20 s of polls.
        assert_eq!(Runner::cut_timeout(10, 30), Duration::from_secs(80));
        assert_eq!(crossings_text(&BTreeMap::new()), "");
        assert_eq!(
            crossings_text(&BTreeMap::from([(0, 1.0), (1, 1.0)])),
            ", crossed by lanes 0 and 1"
        );
    }
}
