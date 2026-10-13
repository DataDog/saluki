//! The Lanes panel: the lanes each slot of ADP's metrics path runs on, how its exits reach the lanes of the slots they
//! lead to, and what each lane measures of its own work.
//!
//! The layout comes from the physical sections of ADP's explain document ([`LaneLayout::of_explain`]): the lane counts
//! the frame was laid out with, each slot's lanes, and each exit's routing, with the key groups each state lane owns
//! behind a keyed exit. Lanes are fixed when ADP boots, so the layout changes only with a restart. What each lane does
//! comes from the `plan_lane_*` series ADP publishes per `component_id` and `lane` once a second ([`view`]):
//!
//! - `plan_lane_events_total` (events per second, and their recent history), `plan_lane_busy_nanos_total` (the share
//!   of one thread the lane spent in its driver), and `plan_lane_input_depth`, for every lane;
//! - `plan_lane_routed_events_total` and `plan_lane_progress_total`, for the lanes whose exits route by key;
//! - `plan_lane_contexts` and `plan_lane_watermark_lag_seconds`, for state lanes, with the skew of the slot (the most
//!   contexts a lane holds over the mean of its lanes);
//! - the ticks each state lane flushed (`plan_lane_flush_seconds`, a histogram whose count grows once per flush) and
//!   the crossings it made (`plan_lane_crossings_total{kind}`, `cut` or `reset`), as marks per scrape, so that lanes
//!   are seen flushing together without a barrier, and crossing on their own at a cut;
//! - `plan_lane_misrouted_total`, which must stay zero.
//!
//! A series ADP does not publish is left out (`None`), so the panel shows what this ADP reports and no more.

use std::collections::BTreeMap;

use saluki_metric_plan::{
    explain::{LayoutEntry, PhysicalPlan},
    limits::values::MAX_LANES,
};
use serde::Serialize;
use serde_json::Value;

use super::{SeriesEntry, TelemetrySnapshot, HISTORY_LEN};

/// The component ID of the DogStatsD source, whose metrics output the producer lanes take.
const DOGSTATSD_SOURCE: &str = "dsd_in";

/// The kind of work a slot's lanes do.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneRole {
    /// Producer lanes: the slot the DogStatsD source's metrics enter, one lane per decoder up to sixteen.
    Producer,

    /// State lanes: an isolation slot, each lane owning a share of every aggregation lineage's contexts.
    State,

    /// Post-aggregation lanes: every other slot.
    Post,
}

/// The key groups one lane of a state slot owns, behind a keyed exit.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct KeyGroupRange {
    /// The lane.
    pub lane: u16,

    /// The first group the lane owns.
    pub first: u16,

    /// The last group the lane owns.
    pub last: u16,
}

/// One exit of a slot, and how it reaches the lanes of what it leads to.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ExitLayout {
    /// The exit's name.
    pub name: String,

    /// Where it leads.
    pub to: String,

    /// How it reaches the lanes of the slot it leads to: `keyed`, `affinity`, `spread`, or `one` when that slot runs
    /// one lane or it leads to no slot.
    pub routing: String,

    /// For a keyed exit, the key groups each lane of the slot it leads to owns.
    pub key_groups: Vec<KeyGroupRange>,

    /// Whether the active plan leaves it idle.
    pub idle: bool,
}

/// One slot of the frame, and its lanes.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SlotLayout {
    /// The slot's component ID.
    pub name: String,

    /// The kind of work its lanes do.
    pub role: LaneRole,

    /// How many lanes it runs on.
    pub lanes: u16,

    /// Its exits, in order.
    pub exits: Vec<ExitLayout>,
}

/// The lanes ADP's metrics path was laid out on at boot, from its explain document.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LaneLayout {
    /// The lane counts and whether R1's cost model pipelines the slots: `None` for a frame on one lane per slot laid
    /// out with pipelining, the layout before lanes.
    pub policy: Option<LayoutEntry>,

    /// The slots, in frame order.
    pub slots: Vec<SlotLayout>,
}

impl LaneLayout {
    /// Reads the layout from an explain document's physical sections.
    ///
    /// Returns `Ok(None)` for a document without them (a plan not placed into a frame).
    ///
    /// # Errors
    ///
    /// Returns an error if the physical sections cannot be read.
    pub fn of_explain(explain: &Value) -> Result<Option<Self>, String> {
        let Some(physical) = explain.get("physical").filter(|physical| !physical.is_null()) else {
            return Ok(None);
        };
        let physical = serde_json::from_value::<PhysicalPlan>(physical.clone())
            .map_err(|error| format!("The explain document's physical sections cannot be read: {}", error))?;
        let producer_slots = physical
            .fixed
            .iter()
            .filter(|fixed| fixed.node.to_string() == DOGSTATSD_SOURCE)
            .flat_map(|fixed| &fixed.admission_points)
            .filter_map(|point| point.to.as_deref())
            .collect::<Vec<_>>();
        let slots = physical
            .slots
            .iter()
            .map(|slot| SlotLayout {
                name: slot.name.clone(),
                role: if slot.isolation.is_some() {
                    LaneRole::State
                } else if producer_slots.contains(&slot.name.as_str()) {
                    LaneRole::Producer
                } else {
                    LaneRole::Post
                },
                lanes: slot.lanes,
                exits: slot
                    .exits
                    .iter()
                    .map(|exit| ExitLayout {
                        name: exit.name.clone(),
                        to: exit.to.clone(),
                        routing: exit.routing.clone(),
                        key_groups: exit
                            .key_groups
                            .iter()
                            .map(|groups| KeyGroupRange {
                                lane: groups.lane,
                                first: groups.first,
                                last: groups.last,
                            })
                            .collect(),
                        idle: exit.idle,
                    })
                    .collect(),
            })
            .collect();
        Ok(Some(Self {
            policy: physical.layout,
            slots,
        }))
    }

    /// Returns the slot named `name`, if the frame has it.
    pub fn slot(&self, name: &str) -> Option<&SlotLayout> {
        self.slots.iter().find(|slot| slot.name == name)
    }
}

/// What one lane measures of its own work, as the panel shows it.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct LaneRow {
    /// The lane.
    pub lane: u16,

    /// The events the lane received per second (`plan_lane_events_total`).
    pub events_per_s: Option<f64>,

    /// The events the lane received in all.
    pub events_total: Option<f64>,

    /// The events it received per second at each recent scrape, oldest first.
    pub events_history: Vec<f64>,

    /// The share of one thread the lane spent in its driver (`plan_lane_busy_nanos_total`).
    pub busy: Option<f64>,

    /// The batches waiting on its input when it last ended a turn (`plan_lane_input_depth`).
    pub input_depth: Option<f64>,

    /// The events its keyed exits routed per second (`plan_lane_routed_events_total`).
    pub routed_events_per_s: Option<f64>,

    /// The header-only buffers its keyed exits sent per second (`plan_lane_progress_total`).
    pub progress_per_s: Option<f64>,

    /// The contexts its state owners hold (`plan_lane_contexts`).
    pub contexts: Option<f64>,

    /// How far its watermark trails its points' latest admission time, in seconds
    /// (`plan_lane_watermark_lag_seconds`).
    pub watermark_lag_s: Option<f64>,

    /// The flushes it made at each recent scrape, oldest first, [`HISTORY_LEN`] long, if ADP reports them
    /// (`plan_lane_flush_seconds`).
    pub ticks: Option<Vec<f64>>,

    /// The crossings it made at each recent scrape, cuts and resets, oldest first, [`HISTORY_LEN`] long, if ADP
    /// reports them (`plan_lane_crossings_total`).
    pub crossings: Option<Vec<f64>>,

    /// The cut crossings it made in all.
    pub cut_crossings: Option<f64>,

    /// The reset crossings it made in all.
    pub reset_crossings: Option<f64>,

    /// The events it admitted whose key belongs to another lane, which must stay zero (`plan_lane_misrouted_total`).
    pub misrouted: Option<f64>,
}

/// One slot's lanes, as the panel shows them.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SlotLanesView {
    /// The slot's component ID.
    pub component: String,

    /// The kind of work its lanes do, if the layout says.
    pub role: Option<LaneRole>,

    /// How many lanes it runs on: the layout's count, or the lanes its series name without one.
    pub lanes: u16,

    /// Its exits that reach the lanes of a slot with lanes, from the layout.
    pub exits: Vec<ExitLayout>,

    /// Its lanes, in order.
    pub rows: Vec<LaneRow>,

    /// The most contexts one of its lanes holds over the mean of its lanes, for a slot whose lanes report contexts,
    /// while they hold any.
    pub skew: Option<f64>,
}

/// What the Lanes panel shows.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LanesView {
    /// Whether ADP's metrics path runs on lanes: its frame was laid out on them, or it publishes lane series.
    pub on: bool,

    /// The lane counts and whether R1 pipelines the slots, as the explain document says.
    pub policy: Option<LayoutEntry>,

    /// Whether the layout is known: ADP served an explain document with physical sections.
    pub layout_known: bool,

    /// Why the layout is not known, if reading it failed.
    pub layout_error: Option<String>,

    /// The slots, in frame order, then any whose series the layout does not name.
    pub slots: Vec<SlotLanesView>,
}

/// The series of one lane, by name without its `plan_lane_` prefix.
#[derive(Default)]
struct LaneSeries<'a> {
    series: BTreeMap<&'a str, Vec<&'a SeriesEntry>>,
}

impl<'a> LaneSeries<'a> {
    fn first(&self, name: &str) -> Option<&'a SeriesEntry> {
        self.series.get(name).and_then(|entries| entries.first().copied())
    }

    fn value(&self, name: &str) -> Option<f64> {
        self.first(name).map(|entry| entry.value)
    }

    fn rate(&self, name: &str) -> Option<f64> {
        self.first(name).map(|entry| entry.rate)
    }

    /// Returns the per-scrape histories of every series named `name` (one per label set), summed, right-aligned on
    /// [`HISTORY_LEN`] scrapes.
    fn summed_history(&self, name: &str) -> Option<Vec<f64>> {
        let entries = self.series.get(name)?;
        let mut summed = vec![0.0; HISTORY_LEN];
        for entry in entries {
            let offset = HISTORY_LEN.saturating_sub(entry.history.len());
            for (at, value) in entry.history.iter().rev().take(HISTORY_LEN).rev().enumerate() {
                summed[offset + at] += value;
            }
        }
        Some(summed)
    }

    fn crossings_of(&self, kind: &str) -> Option<f64> {
        let entries = self.series.get("crossings_total")?;
        Some(
            entries
                .iter()
                .filter(|entry| entry.labels.get("kind").map(String::as_str) == Some(kind))
                .map(|entry| entry.value)
                .sum(),
        )
    }

    fn row(&self, lane: u16) -> LaneRow {
        LaneRow {
            lane,
            events_per_s: self.rate("events_total"),
            events_total: self.value("events_total"),
            events_history: self
                .first("events_total")
                .map(|entry| entry.history.clone())
                .unwrap_or_default(),
            busy: self.rate("busy_nanos_total").map(|nanos| nanos / 1e9),
            input_depth: self.value("input_depth"),
            routed_events_per_s: self.rate("routed_events_total"),
            progress_per_s: self.rate("progress_total"),
            contexts: self.value("contexts"),
            watermark_lag_s: self.value("watermark_lag_seconds"),
            ticks: self.summed_history("flush_seconds_count"),
            crossings: self.summed_history("crossings_total"),
            cut_crossings: self.crossings_of("cut"),
            reset_crossings: self.crossings_of("reset"),
            misrouted: self.value("misrouted_total"),
        }
    }
}

/// Returns what the Lanes panel shows: the layout, if known (with why it is not, if reading it failed), and the
/// `plan_lane_*` series of the telemetry snapshot.
pub fn view(layout: Option<&LaneLayout>, layout_error: Option<String>, telemetry: &TelemetrySnapshot) -> LanesView {
    // The lane series, by component and lane.
    let mut by_lane: BTreeMap<(&str, u16), LaneSeries<'_>> = BTreeMap::new();
    for entry in &telemetry.plan {
        let Some(name) = entry.name.strip_prefix("plan_lane_") else {
            continue;
        };
        let (Some(component), Some(lane)) = (
            entry.labels.get("component_id"),
            entry.labels.get("lane").and_then(|lane| lane.parse::<u16>().ok()),
        ) else {
            continue;
        };
        by_lane
            .entry((component.as_str(), lane))
            .or_default()
            .series
            .entry(name)
            .or_default()
            .push(entry);
    }

    let slot_view = |component: &str, role: Option<LaneRole>, layout_lanes: Option<u16>, exits: Vec<ExitLayout>| {
        let reported = by_lane
            .range((component, 0)..=(component, u16::MAX))
            .map(|((_, lane), _)| lane.saturating_add(1))
            .max()
            .unwrap_or(0);
        // A slot runs on at most `MAX_LANES` lanes, whatever a series' label says.
        let lanes = layout_lanes.unwrap_or(reported).clamp(1, MAX_LANES as u16);
        let empty = LaneSeries::default();
        let rows = (0..lanes)
            .map(|lane| by_lane.get(&(component, lane)).unwrap_or(&empty).row(lane))
            .collect::<Vec<_>>();
        let contexts = rows.iter().filter_map(|row| row.contexts).collect::<Vec<_>>();
        let mean = contexts.iter().sum::<f64>() / contexts.len().max(1) as f64;
        let skew =
            (contexts.len() == rows.len() && mean > 0.0).then(|| contexts.iter().copied().fold(0.0, f64::max) / mean);
        SlotLanesView {
            component: component.to_string(),
            role,
            lanes,
            exits,
            rows,
            skew,
        }
    };

    let mut slots = Vec::new();
    if let Some(layout) = layout {
        for slot in &layout.slots {
            // An exit reaches lanes when the slot it leads to runs several, which its routing then says.
            let exits = slot
                .exits
                .iter()
                .filter(|exit| exit.routing != "one")
                .cloned()
                .collect();
            slots.push(slot_view(&slot.name, Some(slot.role), Some(slot.lanes), exits));
        }
    }
    let mut unlisted = by_lane
        .keys()
        .map(|(component, _)| *component)
        .filter(|component| layout.is_none_or(|layout| layout.slot(component).is_none()))
        .collect::<Vec<_>>();
    unlisted.dedup();
    for component in unlisted {
        slots.push(slot_view(component, None, None, Vec::new()));
    }

    let policy = layout.and_then(|layout| layout.policy.clone());
    LanesView {
        on: policy.is_some() || !by_lane.is_empty(),
        policy,
        layout_known: layout.is_some(),
        layout_error,
        slots,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::telemetry::{prometheus, TelemetryScraper};

    /// The physical sections of an explain document of the default program laid out on 2 producer lanes, 2 state
    /// lanes, and 3 post-aggregation lanes, cut down to what the layout reads (the plan crate's golden file at
    /// `{2, 2, 2}`, `default-program-stage9-physical.json`, has the same shape).
    fn explain() -> Value {
        json!({
            "explain_version": 1,
            "physical": {
                "layout": {"dogstatsd_lanes": 2, "state_lanes": 2, "post_lanes": 3, "pipelining": false},
                "fixed": [
                    {"node": "dsd_in", "op": "source", "boundary": "io_host", "input": null, "idle": false,
                     "admission_points": [{"id": 0, "lanes": 2, "port": "metrics", "mode": "admit",
                                           "to": "dsd_pre_aggregation", "routing": "fixed", "feeds_state": true}]},
                    {"node": "otlp_in", "op": "source", "boundary": "io_host", "input": null, "idle": false,
                     "admission_points": [{"id": 2, "port": "metrics", "mode": "admit", "to": "metrics_enrich",
                                           "routing": "spread"}]}
                ],
                "slots": [
                    {"name": "dsd_pre_aggregation", "cost": 200, "lanes": 2, "nodes": [], "entries": [],
                     "exits": [
                         {"index": 0, "name": "dsd_agg", "to": "dsd_agg", "idle": false, "routing": "keyed",
                          "key_groups": [{"lane": 0, "first": 0, "last": 127}, {"lane": 1, "first": 128, "last": 255}]},
                         {"index": 1, "name": "metrics_enrich", "to": "metrics_enrich", "idle": false,
                          "routing": "affinity"},
                         {"index": 2, "name": "dd_metrics_encode", "to": "encode host dd_metrics_encode of dd_out",
                          "idle": true}
                     ]},
                    {"name": "dsd_agg", "lanes": 2, "nodes": [], "entries": [],
                     "exits": [{"index": 0, "name": "metrics_enrich", "to": "metrics_enrich", "idle": false,
                                "routing": "spread"}],
                     "isolation": {"output_point": 6, "output_points": [6, 7], "upstream": [0, 1]}},
                    {"name": "metrics_enrich", "lanes": 3, "nodes": [], "entries": [],
                     "exits": [{"index": 0, "name": "dd_metrics_encode",
                                "to": "encode host dd_metrics_encode of dd_out", "idle": false}]}
                ],
                "edges": [], "fusion": [], "segments": [], "output_bindings": []
            }
        })
    }

    /// Two scrapes a second apart of the lane series a frame at `{2, 2, 3}` publishes, with the state lanes' series of
    /// the parts that build them (contexts, crossings, flush counts, events routed to the wrong lane).
    fn snapshot() -> TelemetrySnapshot {
        let scrape = |events: [u64; 7], busy: u64, flushes: [u64; 2], cuts: [u64; 2]| {
            let mut text = String::from(
                "# TYPE adp__plan_lane_events_total counter\n# TYPE adp__plan_lane_busy_nanos_total counter\n\
                 # TYPE adp__plan_lane_input_depth gauge\n# TYPE adp__plan_lane_contexts gauge\n\
                 # TYPE adp__plan_lane_routed_events_total counter\n# TYPE adp__plan_lane_crossings_total counter\n\
                 # TYPE adp__plan_lane_flush_seconds histogram\n# TYPE adp__plan_lane_misrouted_total counter\n",
            );
            let lanes = [
                ("dsd_pre_aggregation", 0),
                ("dsd_pre_aggregation", 1),
                ("dsd_agg", 0),
                ("dsd_agg", 1),
                ("metrics_enrich", 0),
                ("metrics_enrich", 1),
                ("metrics_enrich", 2),
            ];
            for ((component, lane), events) in lanes.iter().zip(events) {
                let labels = format!("component_id=\"{}\",lane=\"{}\"", component, lane);
                text.push_str(&format!("adp__plan_lane_events_total{{{}}} {}\n", labels, events));
                text.push_str(&format!("adp__plan_lane_busy_nanos_total{{{}}} {}\n", labels, busy));
                text.push_str(&format!("adp__plan_lane_input_depth{{{}}} 1\n", labels));
            }
            for (lane, routed) in events.iter().take(2).enumerate() {
                let labels = format!("component_id=\"dsd_pre_aggregation\",lane=\"{}\"", lane);
                text.push_str(&format!(
                    "adp__plan_lane_routed_events_total{{{}}} {}\n",
                    labels, routed
                ));
            }
            for (lane, contexts) in [(0, 30), (1, 10)] {
                let labels = format!("component_id=\"dsd_agg\",lane=\"{}\"", lane);
                text.push_str(&format!("adp__plan_lane_contexts{{{}}} {}\n", labels, contexts));
                text.push_str(&format!(
                    "adp__plan_lane_crossings_total{{{},kind=\"cut\"}} {}\n",
                    labels, cuts[lane]
                ));
                text.push_str(&format!(
                    "adp__plan_lane_crossings_total{{{},kind=\"reset\"}} 0\n",
                    labels
                ));
                text.push_str(&format!(
                    "adp__plan_lane_flush_seconds_bucket{{{},le=\"0.1\"}} {}\n",
                    labels, flushes[lane]
                ));
                text.push_str(&format!("adp__plan_lane_flush_seconds_sum{{{}}} 0.5\n", labels));
                text.push_str(&format!(
                    "adp__plan_lane_flush_seconds_count{{{}}} {}\n",
                    labels, flushes[lane]
                ));
                text.push_str(&format!("adp__plan_lane_misrouted_total{{{}}} 0\n", labels));
            }
            prometheus::parse(&text)
        };
        let scraper = TelemetryScraper::new();
        scraper.observe(10_000, &scrape([100, 100, 150, 50, 0, 0, 0], 0, [4, 4], [0, 0]));
        // One second later: each producer lane received 200 events, the state lanes 300 and 100, post-aggregation lane
        // 1 nothing; every lane was busy 250 ms; both state lanes flushed once, and lane 0 crossed a cut.
        scraper.observe(
            11_000,
            &scrape([300, 300, 450, 150, 40, 0, 40], 250_000_000, [5, 5], [1, 0]),
        );
        scraper.snapshot(11_000)
    }

    #[test]
    fn the_layout_names_each_slots_lanes_role_and_exit_routing() {
        let layout = LaneLayout::of_explain(&explain()).unwrap().unwrap();
        let policy = layout.policy.clone().unwrap();
        assert_eq!(
            (
                policy.dogstatsd_lanes,
                policy.state_lanes,
                policy.post_lanes,
                policy.pipelining
            ),
            (2, 2, 3, false)
        );
        let roles = layout
            .slots
            .iter()
            .map(|slot| (slot.name.as_str(), slot.role, slot.lanes))
            .collect::<Vec<_>>();
        assert_eq!(
            roles,
            [
                ("dsd_pre_aggregation", LaneRole::Producer, 2),
                ("dsd_agg", LaneRole::State, 2),
                ("metrics_enrich", LaneRole::Post, 3),
            ]
        );
        let keyed = &layout.slot("dsd_pre_aggregation").unwrap().exits[0];
        assert_eq!((keyed.name.as_str(), keyed.routing.as_str()), ("dsd_agg", "keyed"));
        assert_eq!(
            keyed.key_groups,
            [
                KeyGroupRange {
                    lane: 0,
                    first: 0,
                    last: 127
                },
                KeyGroupRange {
                    lane: 1,
                    first: 128,
                    last: 255
                },
            ]
        );
        // An exit to no slot reaches no lanes: `one`, which the document leaves out.
        assert_eq!(layout.slot("metrics_enrich").unwrap().exits[0].routing, "one");

        // A plan not placed into a frame has no layout, and sections that cannot be read say so.
        assert_eq!(LaneLayout::of_explain(&json!({"explain_version": 1})).unwrap(), None);
        let unreadable = LaneLayout::of_explain(&json!({"physical": {"slots": 3}})).unwrap_err();
        assert!(unreadable.contains("cannot be read"), "{}", unreadable);
    }

    #[test]
    fn each_lane_shows_its_rates_busy_share_contexts_and_marks_and_a_state_slot_its_skew() {
        let layout = LaneLayout::of_explain(&explain()).unwrap().unwrap();
        let view = view(Some(&layout), None, &snapshot());
        assert!(view.on && view.layout_known);
        let slots = view
            .slots
            .iter()
            .map(|slot| (slot.component.as_str(), slot.lanes, slot.rows.len()))
            .collect::<Vec<_>>();
        assert_eq!(
            slots,
            [
                ("dsd_pre_aggregation", 2, 2),
                ("dsd_agg", 2, 2),
                ("metrics_enrich", 3, 3)
            ]
        );

        let producer = &view.slots[0];
        // The keyed exit and the affinity exit reach lanes; the idle exit to the encode host does not.
        assert_eq!(
            producer
                .exits
                .iter()
                .map(|exit| exit.routing.as_str())
                .collect::<Vec<_>>(),
            ["keyed", "affinity"]
        );
        let lane = &producer.rows[1];
        assert_eq!(lane.events_per_s, Some(200.0));
        assert_eq!(lane.events_total, Some(300.0));
        assert_eq!(lane.events_history, [200.0]);
        assert_eq!(lane.busy, Some(0.25));
        assert_eq!(lane.input_depth, Some(1.0));
        assert_eq!(lane.routed_events_per_s, Some(200.0));
        assert_eq!((lane.contexts, lane.ticks.as_ref(), lane.misrouted), (None, None, None));
        assert_eq!(producer.skew, None);

        let state = &view.slots[1];
        assert_eq!(state.role, Some(LaneRole::State));
        assert_eq!(state.rows[0].events_per_s, Some(300.0));
        assert_eq!(state.rows[1].contexts, Some(10.0));
        // 30 and 10 contexts: the busiest lane holds 1.5 times the mean.
        assert_eq!(state.skew, Some(1.5));
        let mut marked = vec![0.0; HISTORY_LEN];
        marked[HISTORY_LEN - 1] = 1.0;
        assert_eq!(state.rows[0].ticks.as_ref(), Some(&marked));
        assert_eq!(state.rows[1].ticks.as_ref(), Some(&marked));
        assert_eq!(state.rows[0].crossings.as_ref(), Some(&marked));
        assert_eq!(state.rows[1].crossings.as_ref(), Some(&vec![0.0; HISTORY_LEN]));
        assert_eq!(
            (state.rows[0].cut_crossings, state.rows[0].reset_crossings),
            (Some(1.0), Some(0.0))
        );
        assert_eq!(state.rows[1].misrouted, Some(0.0));

        let post = &view.slots[2];
        assert_eq!(post.rows[1].events_per_s, Some(0.0));
        assert_eq!(post.rows[2].events_per_s, Some(40.0));
    }

    #[test]
    fn without_a_layout_the_series_alone_list_the_lanes_and_without_either_lanes_are_off() {
        let view = view(None, Some("ADP answered 503".to_string()), &snapshot());
        assert!(view.on && !view.layout_known);
        assert_eq!(view.layout_error.as_deref(), Some("ADP answered 503"));
        let slots = view
            .slots
            .iter()
            .map(|slot| (slot.component.as_str(), slot.role, slot.lanes, slot.exits.len()))
            .collect::<Vec<_>>();
        assert_eq!(
            slots,
            [
                ("dsd_agg", None, 2, 0),
                ("dsd_pre_aggregation", None, 2, 0),
                ("metrics_enrich", None, 3, 0)
            ]
        );

        let off = super::view(None, None, &TelemetryScraper::new().snapshot(0));
        assert!(!off.on && off.slots.is_empty() && off.policy.is_none());

        // A lane label beyond what a slot can run on lists no more lanes than a slot has at most.
        let scraper = TelemetryScraper::new();
        scraper.observe(
            1_000,
            &prometheus::parse(
                "# TYPE adp__plan_lane_events_total counter\n\
                 adp__plan_lane_events_total{component_id=\"dsd_agg\",lane=\"65535\"} 1\n",
            ),
        );
        let wide = super::view(None, None, &scraper.snapshot(1_000));
        assert_eq!(wide.slots[0].rows.len(), MAX_LANES as usize);

        // A frame on one lane per slot, laid out as before lanes: no policy, every slot on one lane.
        let mut one_lane = explain();
        let physical = &mut one_lane["physical"];
        physical.as_object_mut().unwrap().remove("layout");
        for slot in physical["slots"].as_array_mut().unwrap() {
            slot.as_object_mut().unwrap().remove("lanes");
        }
        let layout = LaneLayout::of_explain(&one_lane).unwrap().unwrap();
        let view = super::view(Some(&layout), None, &TelemetryScraper::new().snapshot(0));
        assert!(!view.on && view.layout_known);
        assert!(view.slots.iter().all(|slot| slot.lanes == 1 && slot.rows.len() == 1));
    }
}
