use std::time::Duration;

use saluki_common::cache::{Cache, CacheBuilder};

use super::config::OtlpTranslatorConfig;
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
pub struct PointsCache {
    number_points: Cache<String, NumberCounter>,
    extrema_points: Cache<String, Extrema>,
}

impl PointsCache {
    pub fn from_config(config: OtlpTranslatorConfig) -> Self {
        let ttl = config.delta_ttl;
        let interval = config.sweep_interval;

        let number_points = CacheBuilder::from_identifier("otlp/metrics/number_points")
            .expect("identifier cannot be invalid")
            .with_time_to_idle(Some(ttl))
            .with_expiration_interval(interval)
            .build();
        let extrema_points = CacheBuilder::from_identifier("otlp/metrics/extrema_points")
            .expect("identifier cannot be invalid")
            .with_time_to_idle(Some(ttl))
            .with_expiration_interval(interval)
            .build();

        Self {
            number_points,
            extrema_points,
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

    /// Creates a new `PointsCache` for tests.
    pub fn for_tests() -> Self {
        Self {
            number_points: CacheBuilder::for_tests().build(),
            extrema_points: CacheBuilder::for_tests().build(),
        }
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

#[cfg(test)]
mod tests {
    // TODO: Port `TestPutAndGetExtrema` from the Go `ttlcache_test.go` to test the logic
    // for `put_and_check_min` and `put_and_check_max` when histogram support is added.

    use super::*;
    use crate::sources::otlp::metrics::dimensions::Dimensions;

    struct Point {
        start_ts: u64,
        ts: u64,
        val: f64,
        expect_first_point: bool,
        expect_drop_point: bool,
        message: &'static str,
    }

    #[test]
    fn test_monotonic_diff_unknown_start() {
        let points = vec![
            Point {
                start_ts: 0,
                ts: 1,
                val: 5.0,
                expect_first_point: true,
                expect_drop_point: false,
                message: "first point",
            },
            Point {
                start_ts: 0,
                ts: 1,
                val: 6.0,
                expect_first_point: false,
                expect_drop_point: true,
                message: "new ts == old ts",
            },
            Point {
                start_ts: 0,
                ts: 0,
                val: 0.0,
                expect_first_point: false,
                expect_drop_point: true,
                message: "new ts < old ts",
            },
            Point {
                start_ts: 0,
                ts: 2,
                val: 2.0,
                expect_first_point: true,
                expect_drop_point: false,
                message: "new < old => there has been a reset: first point",
            },
            Point {
                start_ts: 0,
                ts: 4,
                val: 6.0,
                expect_first_point: false,
                expect_drop_point: false,
                message: "valid point",
            },
        ];

        let mut prev_pts = PointsCache::for_tests();
        let dims = Dimensions {
            name: "test".to_string(),
            tags: Default::default(),
            host: None,
            origin_id: None,
        };
        let mut dx = 0.0;

        for point in points {
            let (result_dx, first_point, drop_point) =
                prev_pts.monotonic_diff(&dims, point.start_ts, point.ts, point.val);
            assert_eq!(point.expect_first_point, first_point, "{}", point.message);
            assert_eq!(point.expect_drop_point, drop_point, "{}", point.message);
            if !drop_point {
                dx = result_dx;
            }
        }
        assert_eq!(4.0, dx, "expected diff 4.0");
    }

    #[test]
    fn test_monotonic_diff_known_start() {
        let initial_points = vec![
            Point {
                start_ts: 1,
                ts: 1,
                val: 5.0,
                expect_first_point: true,
                expect_drop_point: false,
                message: "first point",
            },
            Point {
                start_ts: 1,
                ts: 1,
                val: 6.0,
                expect_first_point: false,
                expect_drop_point: true,
                message: "new ts == old ts",
            },
            Point {
                start_ts: 1,
                ts: 0,
                val: 0.0,
                expect_first_point: false,
                expect_drop_point: true,
                message: "new ts < old ts",
            },
            Point {
                start_ts: 1,
                ts: 2,
                val: 2.0,
                expect_first_point: true,
                expect_drop_point: false,
                message: "new < old => there has been a reset: first point",
            },
            Point {
                start_ts: 1,
                ts: 3,
                val: 6.0,
                expect_first_point: false,
                expect_drop_point: false,
                message: "valid point",
            },
        ];

        let points_after_reset = vec![
            Point {
                start_ts: 4,
                ts: 4,
                val: 8.0,
                expect_first_point: true,
                expect_drop_point: false,
                message: "first point: startTs = ts, there has been a reset",
            },
            Point {
                start_ts: 4,
                ts: 6,
                val: 12.0,
                expect_first_point: false,
                expect_drop_point: false,
                message: "same startTs, old >= new",
            },
        ];

        let points_after_second_reset = vec![
            Point {
                start_ts: 8,
                ts: 9,
                val: 1.0,
                expect_first_point: true,
                expect_drop_point: false,
                message: "first point",
            },
            Point {
                start_ts: 8,
                ts: 12,
                val: 10.0,
                expect_first_point: false,
                expect_drop_point: false,
                message: "same startTs, old >= new",
            },
        ];

        let mut prev_pts = PointsCache::for_tests();
        let dims = Dimensions {
            name: "test".to_string(),
            tags: Default::default(),
            host: None,
            origin_id: None,
        };
        let mut dx = 0.0;

        for point in initial_points {
            let (result_dx, first_point, drop_point) =
                prev_pts.monotonic_diff(&dims, point.start_ts, point.ts, point.val);
            assert_eq!(point.expect_first_point, first_point, "{}", point.message);
            assert_eq!(point.expect_drop_point, drop_point, "{}", point.message);
            if !drop_point {
                dx = result_dx;
            }
        }
        assert_eq!(4.0, dx, "expected diff 4.0");

        // reset
        for point in points_after_reset {
            let (result_dx, first_point, drop_point) =
                prev_pts.monotonic_diff(&dims, point.start_ts, point.ts, point.val);
            assert_eq!(point.expect_first_point, first_point, "{}", point.message);
            assert_eq!(point.expect_drop_point, drop_point, "{}", point.message);
            if !drop_point {
                dx = result_dx;
            }
        }
        assert_eq!(4.0, dx, "expected diff 4.0");

        // second reset
        for point in points_after_second_reset {
            let (result_dx, first_point, drop_point) =
                prev_pts.monotonic_diff(&dims, point.start_ts, point.ts, point.val);
            assert_eq!(point.expect_first_point, first_point, "{}", point.message);
            assert_eq!(point.expect_drop_point, drop_point, "{}", point.message);
            if !drop_point {
                dx = result_dx;
            }
        }
        assert_eq!(9.0, dx, "expected diff 9.0");
    }

    #[test]
    fn test_diff_unknown_start() {
        let start_ts = 0;
        let mut prev_pts = PointsCache::for_tests();
        let dims = Dimensions {
            name: "test".to_string(),
            tags: Default::default(),
            host: None,
            origin_id: None,
        };

        let (_, ok) = prev_pts.diff(&dims, start_ts, 1, 5.0);
        assert!(!ok, "expected no diff: first point");

        let (_, ok) = prev_pts.diff(&dims, start_ts, 0, 0.0);
        assert!(!ok, "expected no diff: old point");

        let (dx, ok) = prev_pts.diff(&dims, start_ts, 2, 2.0);
        assert!(ok, "expected diff: no startTs, not monotonic");
        assert_eq!(-3.0, dx, "expected diff -3.0 with (0,1,5) value");

        let (dx, ok) = prev_pts.diff(&dims, start_ts, 3, 4.0);
        assert!(ok, "expected diff: no startTs, old >= new");
        assert_eq!(2.0, dx, "expected diff 2.0 with (0,2,2) value");
    }

    #[test]
    fn test_diff_known_start() {
        let mut start_ts = 1;
        let mut prev_pts = PointsCache::for_tests();
        let dims = Dimensions {
            name: "test".to_string(),
            tags: Default::default(),
            host: None,
            origin_id: None,
        };

        let (_, ok) = prev_pts.diff(&dims, start_ts, 1, 5.0);
        assert!(!ok, "expected no diff: first point");

        let (_, ok) = prev_pts.diff(&dims, start_ts, 0, 0.0);
        assert!(!ok, "expected no diff: old point");

        let (dx, ok) = prev_pts.diff(&dims, start_ts, 2, 2.0);
        assert!(ok, "expected diff: same startTs, not monotonic");
        assert_eq!(-3.0, dx, "expected diff -3.0 with (1,1,5) point");

        let (dx, ok) = prev_pts.diff(&dims, start_ts, 3, 4.0);
        assert!(ok, "expected diff: same startTs, not monotonic");
        assert_eq!(2.0, dx, "expected diff 2.0 with (0,2,2) value");

        // simulate reset with startTs = ts
        start_ts = 4;
        let (_, ok) = prev_pts.diff(&dims, start_ts, start_ts, 8.0);
        assert!(!ok, "expected no diff: reset with unknown start");
        let (dx, ok) = prev_pts.diff(&dims, start_ts, 5, 9.0);
        assert!(ok, "expected diff: same startTs, not monotonic");
        assert_eq!(1.0, dx, "expected diff 1.0 with (4,4,8) value");

        // simulate reset with known start
        start_ts = 6;
        let (_, ok) = prev_pts.diff(&dims, start_ts, 7, 1.0);
        assert!(!ok, "expected no diff: reset with known start");
        let (dx, ok) = prev_pts.diff(&dims, start_ts, 8, 10.0);
        assert!(ok, "expected diff: same startTs, not monotonic");
        assert_eq!(9.0, dx, "expected diff 9.0 with (6,7,1) value");
    }
}
