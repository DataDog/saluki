//! Context comparison shared by the `eventually_` and `finally_` checks.
//!
//! Each check fetches the two lanes from the intake and builds a [`Difference`], the symmetric
//! difference `D` of their context sets. `eventually_` fails an overdue member. `finally_` fails any
//! residual after a drain.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};

/// A metric context: name, tagset, and flushed type. The whole triple is the identity; `tagset` is a
/// set, so tag order never decides equality.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct Context {
    name: String,
    tagset: BTreeSet<String>,
    kind: String,
}

/// A context and the time it first arrived on a lane, the flat wire shape the intake serves.
#[derive(Clone, Debug, Deserialize)]
struct Captured {
    #[serde(flatten)]
    context: Context,
    first_seen: i64,
}

/// A lane's cumulative context set with each context's first-seen time. The README's `C_ADP`/`C_DA`.
type Cumulative = BTreeMap<Context, i64>;

/// The lane to read from the intake control API.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Lane {
    Agent,
    Adp,
}

impl Lane {
    fn as_str(self) -> &'static str {
        match self {
            Lane::Agent => "agent",
            Lane::Adp => "adp",
        }
    }
}

/// One lane's contexts and the intake's current time. `now` ages `first_seen` against the same clock
/// that stamped it.
#[derive(Clone, Debug, Deserialize)]
pub struct LaneView {
    now: i64,
    contexts: Vec<Captured>,
}

impl LaneView {
    /// Fetches a lane's contexts and the intake's current time from the control API.
    ///
    /// # Errors
    ///
    /// Errors when the request fails, the intake returns an error status, or the body does not parse.
    pub fn fetch(client: &Client, intake_addr: &str, lane: Lane) -> anyhow::Result<Self> {
        let url = format!("http://{intake_addr}/antithesis/metrics/{}", lane.as_str());
        client
            .get(&url)
            .send()
            .map_err(|e| anyhow::anyhow!("GET {url}: {e}"))?
            .error_for_status()
            .map_err(|e| anyhow::anyhow!("GET {url} returned an error status: {e}"))?
            .json()
            .map_err(|e| anyhow::anyhow!("parse JSON response from {url}: {e}"))
    }

    fn cumulative(&self) -> Cumulative {
        self.contexts
            .iter()
            .map(|c| (c.context.clone(), c.first_seen))
            .collect()
    }
}

/// A diverging context's lane and first-seen time, before it is aged against `now`.
#[derive(Clone, Copy, Debug)]
struct Member {
    lane: Lane,
    first_seen: i64,
}

/// A diverging context tagged with the lane that carries it and how long it has sat in `D`.
///
/// `agent` means ADP dropped or mangled a context the Datadog Agent emitted; `adp` means ADP emitted
/// one the Agent did not. The Agent is normative, so either direction is an ADP defect.
#[derive(Clone, Debug, Serialize)]
pub struct Diverging {
    pub lane: Lane,
    #[serde(flatten)]
    pub context: Context,
    pub age_secs: i64,
}

/// The symmetric difference `D` of `C_ADP` and `C_DA`: contexts on one lane but not both, aged
/// against the intake's clock.
pub struct Difference {
    members: BTreeMap<Context, Member>,
    now: i64,
}

impl Difference {
    /// Computes `D` from the two lanes' cumulative sets.
    #[must_use]
    pub fn between(agent: &LaneView, adp: &LaneView) -> Self {
        let c_da = agent.cumulative();
        let c_adp = adp.cumulative();
        let mut members = BTreeMap::new();
        for (context, &first_seen) in &c_da {
            if !c_adp.contains_key(context) {
                members.insert(
                    context.clone(),
                    Member {
                        lane: Lane::Agent,
                        first_seen,
                    },
                );
            }
        }
        for (context, &first_seen) in &c_adp {
            if !c_da.contains_key(context) {
                members.insert(
                    context.clone(),
                    Member {
                        lane: Lane::Adp,
                        first_seen,
                    },
                );
            }
        }
        Self {
            members,
            now: agent.now.max(adp.now),
        }
    }

    /// The number of diverging contexts.
    #[must_use]
    pub fn len(&self) -> usize {
        self.members.len()
    }

    /// Whether the lanes agree.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    /// How many members are `delayed`: sat in `D` longer than `budget` by the intake's clock.
    #[must_use]
    pub fn delayed(&self, budget: Duration) -> usize {
        let budget = budget.as_secs() as i64;
        self.members
            .values()
            .filter(|m| self.now - m.first_seen > budget)
            .count()
    }

    /// The diverging contexts, each tagged with its lane and its age in `D` by the intake's clock.
    #[must_use]
    pub fn diverging(&self) -> Vec<Diverging> {
        self.members
            .iter()
            .map(|(context, member)| Diverging {
                lane: member.lane,
                context: context.clone(),
                age_secs: self.now - member.first_seen,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn captured(name: &str, kind: &str, tags: &[&str], first_seen: i64) -> Captured {
        Captured {
            context: Context {
                name: name.to_string(),
                tagset: tags.iter().map(|t| (*t).to_string()).collect(),
                kind: kind.to_string(),
            },
            first_seen,
        }
    }

    fn lane(now: i64, contexts: Vec<Captured>) -> LaneView {
        LaneView { now, contexts }
    }

    #[test]
    fn flushed_type_is_part_of_identity() {
        // Same name and tags but different flushed type are distinct contexts.
        let agent = lane(10, vec![captured("adp.requests", "count", &["host:h"], 10)]);
        let adp = lane(10, vec![captured("adp.requests", "gauge", &["host:h"], 10)]);

        assert_eq!(Difference::between(&agent, &adp).len(), 2);
    }

    #[test]
    fn tag_order_does_not_decide_identity() {
        // The same tag set in any order is one context, so the lanes agree.
        let agent = lane(10, vec![captured("adp.requests", "count", &["b:2", "a:1"], 10)]);
        let adp = lane(11, vec![captured("adp.requests", "count", &["a:1", "b:2"], 11)]);

        assert!(Difference::between(&agent, &adp).is_empty());
    }

    #[test]
    fn diverging_tags_each_member_with_its_lane() {
        // A context only the Agent emitted is agent-side; one only ADP emitted is adp-side. Age is the
        // intake clock minus first_seen.
        let agent = lane(10, vec![captured("agent.only", "count", &["host:h"], 4)]);
        let adp = lane(10, vec![captured("adp.only", "gauge", &["host:h"], 7)]);

        let mut members = Difference::between(&agent, &adp).diverging();
        members.sort_by(|a, b| a.context.name.cmp(&b.context.name));

        assert!(matches!(members[0].lane, Lane::Adp));
        assert_eq!(members[0].context.name, "adp.only");
        assert_eq!(members[0].age_secs, 3);
        assert!(matches!(members[1].lane, Lane::Agent));
        assert_eq!(members[1].context.name, "agent.only");
        assert_eq!(members[1].age_secs, 6);
    }

    #[test]
    fn delayed_counts_members_past_the_budget() {
        // A member is delayed once the intake clock has moved more than the budget past first_seen.
        let present = vec![captured("adp.requests", "count", &["host:h"], 100)];
        let fresh = Difference::between(&lane(140, present.clone()), &lane(140, vec![]));
        let stale = Difference::between(&lane(200, present), &lane(200, vec![]));

        assert_eq!(fresh.delayed(Duration::from_secs(60)), 0);
        assert_eq!(stale.delayed(Duration::from_secs(60)), 1);
    }

    #[test]
    fn deserializes_the_flat_wire_shape() -> Result<(), serde_json::Error> {
        let view: LaneView = serde_json::from_value(json!({
            "now": 2000,
            "contexts": [{ "name": "adp.requests", "tagset": ["host:h"], "kind": "count", "first_seen": 1000 }],
        }))?;

        assert_eq!(view.now, 2000);
        assert_eq!(view.contexts.len(), 1);
        Ok(())
    }
}
