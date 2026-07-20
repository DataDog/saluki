//! The two lanes' aggregation curves, fetched from the intake and keyed by context identity.
//!
//! The curve oracle pairs the two lanes' [`LaneView::curves`] maps by context and hands each pair to
//! the decision layer.

use std::collections::{BTreeMap, BTreeSet};

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

/// A DDSketch summary plus its log-grid bins as `(key, count)`. The differential-crate mirror of the
/// intake's `capture::SketchValue`; both sides serde the same wire shape so no second decoder sits on
/// either lane.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SketchValue {
    pub count: i64,
    pub sum: f64,
    pub min: f64,
    pub max: f64,
    pub bins: Vec<(i32, u32)>,
}

/// One aggregated bucket value, kind-agnostic. The mirror of the intake's `capture::BucketValue`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub enum BucketValue {
    /// A count, rate, or gauge scalar.
    Scalar(f64),
    /// A DDSketch summary plus its log-grid bins.
    Sketch(SketchValue),
}

/// One bucket on a context's aggregation curve: the wire bucket-start, its value, and whether a
/// second differing value landed on the same bucket-start on the same lane. The mirror of the
/// intake's `capture::Bucket`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Bucket {
    pub bucket_start: u64,
    pub value: BucketValue,
    pub conflict: bool,
}

/// A context and its captured curve: the curve DTO the intake serves and this bin consumes. `kind`
/// rides in `context`.
#[derive(Clone, Debug, Deserialize)]
struct Captured {
    #[serde(flatten)]
    context: Context,
    /// The context's aggregation curve in bucket-start order.
    #[serde(default)]
    buckets: Vec<Bucket>,
}

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

/// One lane's captured contexts, fetched from the intake control API.
#[derive(Clone, Debug, Deserialize)]
pub struct LaneView {
    contexts: Vec<Captured>,
}

impl LaneView {
    /// Fetches a lane's contexts from the control API.
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

    /// Each context's captured aggregation curve keyed by context identity. The curve oracle pairs the
    /// two lanes' maps by context; a context on only one map is a presence concern the decision layer
    /// settles against the band.
    #[must_use]
    pub fn curves(&self) -> BTreeMap<Context, Vec<Bucket>> {
        self.contexts
            .iter()
            .map(|c| (c.context.clone(), c.buckets.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn flushed_type_is_part_of_identity() {
        // Same name and tags but different flushed type are distinct contexts, so the curve keys differ.
        let view: LaneView = serde_json::from_value(json!({
            "contexts": [
                { "name": "adp.requests", "tagset": ["host:h"], "kind": "count" },
                { "name": "adp.requests", "tagset": ["host:h"], "kind": "gauge" },
            ],
        }))
        .expect("deserialize LaneView");

        assert_eq!(view.curves().len(), 2);
    }

    #[test]
    fn tag_order_does_not_decide_identity() {
        // The same tag set in any order is one context, so both lanes key their curves the same.
        let agent: LaneView = serde_json::from_value(json!({
            "contexts": [{ "name": "adp.requests", "tagset": ["b:2", "a:1"], "kind": "count" }],
        }))
        .expect("deserialize agent LaneView");
        let adp: LaneView = serde_json::from_value(json!({
            "contexts": [{ "name": "adp.requests", "tagset": ["a:1", "b:2"], "kind": "count" }],
        }))
        .expect("deserialize adp LaneView");

        assert_eq!(
            agent.curves().keys().collect::<Vec<_>>(),
            adp.curves().keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn deserializes_the_flat_wire_shape() -> Result<(), serde_json::Error> {
        let view: LaneView = serde_json::from_value(json!({
            "contexts": [{ "name": "adp.requests", "tagset": ["host:h"], "kind": "count" }],
        }))?;

        assert_eq!(view.contexts.len(), 1);
        Ok(())
    }

    // The curve DTO the intake serves deserializes into the consumer mirror: a context carrying a mix
    // of scalar and sketch buckets, one flagged as a within-lane conflict.
    #[test]
    fn deserializes_the_curve_dto() -> Result<(), serde_json::Error> {
        let view: LaneView = serde_json::from_value(json!({
            "contexts": [{
                "name": "app.dist",
                "tagset": ["env:prod"],
                "kind": "sketch",
                "buckets": [
                    { "bucket_start": 100, "value": { "Scalar": 1.5 }, "conflict": false },
                    { "bucket_start": 110, "value": { "Sketch": {
                        "count": 5, "sum": 30.0, "min": 1.0, "max": 9.0, "bins": [[3, 2], [5, 3]]
                    } }, "conflict": true },
                ],
            }],
        }))?;

        let captured = &view.contexts[0];
        assert_eq!(captured.buckets.len(), 2);
        assert_eq!(
            captured.buckets[0],
            Bucket {
                bucket_start: 100,
                value: BucketValue::Scalar(1.5),
                conflict: false,
            }
        );
        assert!(captured.buckets[1].conflict);
        assert!(matches!(captured.buckets[1].value, BucketValue::Sketch(_)));
        Ok(())
    }
}
