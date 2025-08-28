use std::collections::HashMap;
use std::time::Duration;

use super::dimensions::Dimensions;

/// The state we store for each unique time series.
#[derive(Clone, Debug)]
pub struct NumberCounter {
    pub value: f64,
    pub timestamp: u64,
    pub start_timestamp: u64,
}

/// The state we store for min/max values.
#[derive(Clone, Debug)]
pub struct Extrema {
    pub timestamp: u64,
    pub start_timestamp: u64,
    pub stored_extrema: f64,
}

/// A cache for storing previous data points to calculate deltas for cumulative metrics.
pub struct Cache {
    number_points: HashMap<String, NumberCounter>,
    extrema_points: HashMap<String, Extrema>,
}

impl Cache {
    pub fn new() -> Self {
        Self {
            number_points: HashMap::new(),
            extrema_points: HashMap::new(),
        }
    }

    // Submits a new value for a given monotonic metric
    //
    // Returns the difference with the last submitted value (ordered by timestamp),
    // whether this is a first point and whether it should be dropped.
    pub fn monotonic_diff(
        &mut self, dims: &Dimensions, start_timestamp: u64, timestamp: u64, value: f64,
    ) -> (f64, bool, bool) {
        self.put_and_get_monotonic(dims, start_timestamp, timestamp, value, false)
    }

    // Submits a new value for a given monotonic metric
    //
    // Returns the rate per second since the last submitted value (ordered by timestamp),
    // whether this is a first point and whether it should be dropped.
    pub fn monotonic_rate(
        &mut self, dims: &Dimensions, start_timestamp: u64, timestamp: u64, value: f64,
    ) -> (f64, bool, bool) {
        self.put_and_get_monotonic(dims, start_timestamp, timestamp, value, true)
    }

    // Diff submits a new value for a given non-monotonic metric and returns the difference with the
    // last submitted value (ordered by timestamp). The diff value is only valid if `ok` is true.
    pub fn diff(&mut self, dims: &Dimensions, start_timestamp: u64, timestamp: u64, value: f64) -> (f64, bool) {
        self.put_and_get_diff(dims, start_timestamp, timestamp, value)
    }

    pub fn put_and_check_min(&mut self, dims: &Dimensions, start_timestamp: u64, timestamp: u64, value: f64) -> bool {
        self.put_and_check_extrema(dims, start_timestamp, timestamp, value, true)
    }

    pub fn put_and_check_max(&mut self, dims: &Dimensions, start_timestamp: u64, timestamp: u64, value: f64) -> bool {
        self.put_and_check_extrema(dims, start_timestamp, timestamp, value, false)
    }

    fn put_and_get_diff(&mut self, dims: &Dimensions, start_timestamp: u64, timestamp: u64, value: f64) -> (f64, bool) {
        let key = dims.get_cache_key();

        let mut dx = 0.0;
        let mut ok = false;

        if let Some(prev_counter) = self.number_points.get(&key) {
            if prev_counter.timestamp > timestamp {
                // Point is older than the one in memory, drop it.
                return (0.0, false);
            }
            dx = value - prev_counter.value;
            ok = is_not_first_point(start_timestamp, timestamp, prev_counter.start_timestamp);
        }

        self.number_points.insert(
            key,
            NumberCounter {
                value,
                timestamp,
                start_timestamp,
            },
        );

        (dx, ok)
    }

    fn put_and_get_monotonic(
        &mut self, dims: &Dimensions, start_timestamp: u64, timestamp: u64, value: f64, rate: bool,
    ) -> (f64, bool, bool) {
        // dx, firstPoint, dropPoint
        let mut dx = 0.0;
        let mut first_point = true;
        let drop_point = false;

        let key = dims.get_cache_key();

        if let Some(prev_counter) = self.number_points.get(&key) {
            if prev_counter.timestamp >= timestamp {
                // We were given a point with a timestamp older or equal to the one in the cache. This point
                // should be dropped. We keep the current point in cache.
                return (0.0, false, true);
            }
            dx = value - prev_counter.value;
            if rate {
                let time_delta = Duration::from_nanos(timestamp - prev_counter.timestamp);
                dx /= time_delta.as_secs_f64();
            }
            // If !isNotFirstPoint or dx < 0, there has been a reset. We cache the new value, and firstPoint is true.
            first_point = !is_not_first_point(start_timestamp, timestamp, prev_counter.start_timestamp) || dx < 0.0;
        }

        self.number_points.insert(
            key,
            NumberCounter {
                value,
                timestamp,
                start_timestamp,
            },
        );

        (dx, first_point, drop_point)
    }

    fn put_and_check_extrema(
        &mut self, dims: &Dimensions, start_timestamp: u64, timestamp: u64, current_extrema: f64, is_min: bool,
    ) -> bool {
        let key = dims.get_cache_key();
        let mut from_last_window = false;

        if let Some(prev_extrema) = self.extrema_points.get(&key) {
            if prev_extrema.timestamp > timestamp {
                return false;
            }

            let is_not_first = is_not_first_point(start_timestamp, timestamp, prev_extrema.start_timestamp);
            if is_min {
                from_last_window = (is_not_first && current_extrema < prev_extrema.stored_extrema)
                    || (current_extrema > prev_extrema.stored_extrema);
            } else {
                from_last_window = (is_not_first && current_extrema > prev_extrema.stored_extrema)
                    || (current_extrema < prev_extrema.stored_extrema);
            }
        }

        self.extrema_points.insert(
            key,
            Extrema {
                stored_extrema: current_extrema,
                timestamp,
                start_timestamp,
            },
        );

        from_last_window
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
