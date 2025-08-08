use std::collections::HashMap;
use std::time::Duration;

use saluki_context::tags::SharedTagSet;

/// The state we store for each unique time series.
#[derive(Clone, Debug)]
pub struct NumberCounter {
    pub value: f64,
    pub timestamp: u64,
    pub start_timestamp: u64,
}

/// A cache for storing previous data points to calculate deltas for cumulative metrics.
pub struct Cache {
    points: HashMap<String, NumberCounter>,
}

impl Cache {
    pub fn new() -> Self {
        Self { points: HashMap::new() }
    }

    // Submits a new value for a given monotonic metric
    //
    // Returns the difference with the last submitted value (ordered by timestamp),
    // whether this is a first point and whether it should be dropped.
    pub fn monotonic_diff(
        &mut self, name: &str, tags: &SharedTagSet, start_timestamp: u64, timestamp: u64, value: f64,
    ) -> (f64, bool, bool) {
        self.put_and_get_monotonic(name, tags, start_timestamp, timestamp, value, false)
    }

    // Submits a new value for a given monotonic metric
    //
    // Returns the rate per second since the last submitted value (ordered by timestamp),
    // whether this is a first point and whether it should be dropped.
    pub fn monotonic_rate(
        &mut self, name: &str, tags: &SharedTagSet, start_timestamp: u64, timestamp: u64, value: f64,
    ) -> (f64, bool, bool) {
        self.put_and_get_monotonic(name, tags, start_timestamp, timestamp, value, true)
    }

    fn put_and_get_monotonic(
        &mut self, name: &str, tags: &SharedTagSet, start_timestamp: u64, timestamp: u64, value: f64, rate: bool,
    ) -> (f64, bool, bool) {
        // dx, firstPoint, dropPoint
        let mut dx = 0.0;
        let mut first_point = true;
        let drop_point = false;

        let key = get_cache_key(name, tags);

        if let Some(prev_counter) = self.points.get(&key) {
            if prev_counter.timestamp >= timestamp {
                // We were given a point with a timestamp older or equal to the one in the cache. This point
                // should be dropped. We keep the current point in cache.
                return (0.0, false, true);
            }
            println!(
                "rz6300 name: {:?}, current: {:?}, prev_counter: {:?}",
                name, value, prev_counter.value
            );
            dx = value - prev_counter.value;
            if rate {
                let time_delta = Duration::from_nanos(timestamp - prev_counter.timestamp);
                dx /= time_delta.as_secs_f64();
            }
            // If !isNotFirstPoint or dx < 0, there has been a reset. We cache the new value, and firstPoint is true.
            first_point = !is_not_first_point(start_timestamp, timestamp, prev_counter.start_timestamp) || dx < 0.0;
        }

        self.points.insert(
            key,
            NumberCounter {
                value,
                timestamp,
                start_timestamp,
            },
        );

        (dx, first_point, drop_point)
    }
}

/// isNotFirstPoint determines if this is NOT the first point on a cumulative series:
/// https://github.com/open-telemetry/opentelemetry-specification/blob/v1.19.0/specification/metrics/data-model.md#resets-and-gaps
fn is_not_first_point(start_ts: u64, ts: u64, old_start_ts: u64) -> bool {
    if start_ts == 0 {
        // We don't know the start time, assume the sequence has not been restarted.
        return true;
    } else if start_ts != ts && start_ts == old_start_ts {
        // Since startTs != 0 we know the start time, thus we apply the following rules from the spec:
        //  - "When StartTimeUnixNano equals TimeUnixNano, a new unbroken sequence of observations begins with a reset at an unknown start time."
        //  - "[for cumulative series] the StartTimeUnixNano of each point matches the StartTimeUnixNano of the initial observation."
        return true;
    }
    false
}

/// Create a cache key
/// TODO: Add host and other stuff needed
/// https://github.com/DataDog/datadog-agent/blob/main/pkg/opentelemetry-mapping-go/otlp/metrics/dimensions.go#L135-L151
fn get_cache_key(name: &str, tags: &SharedTagSet) -> String {
    // Collect tags into a mutable Vec<String> to allow sorting.
    let mut dimensions: Vec<String> = tags.into_iter().map(|t| t.to_string()).collect();

    // Add the metric name as a dimension, just like the Go implementation.
    dimensions.push(format!("name:{}", name));

    // TODO: Add host and originID once they are available.
    // dimensions.push(format!("host:{}", host));
    // dimensions.push(format!("originID:{}", origin_id));

    // Sort the dimensions alphabetically to ensure a canonical key.
    dimensions.sort();

    // Join with a null character separator.
    dimensions.join("\0")
}
