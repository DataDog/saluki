use saluki_common::collections::FastHashMap;
use stringtheory::MetaString;

/// Size of each bucket in the retry queue capacity traffic-rate window.
pub const RETRY_QUEUE_CAPACITY_BUCKET_DURATION_SECS: u64 = 10;

/// A per-domain retry queue capacity estimate.
#[derive(Clone, Copy)]
pub(super) struct DomainCapacityStats {
    /// The current byte rate for this domain.
    pub bytes_per_sec: f64,
    /// The estimated available retry queue capacity for this domain, in bytes.
    pub capacity_bytes: f64,
    /// The estimated available retry queue capacity for this domain, in seconds at the current byte rate.
    pub capacity_secs: f64,
}

/// A rolling traffic-rate window for retry queue capacity telemetry.
pub(super) struct TrafficRateWindow {
    bucket_sum: Vec<u64>,
    current_index: usize,
    current_index_time_secs: Option<u64>,
    start_index_time_secs: Option<u64>,
    sum: u64,
    bucket_duration_secs: u64,
}

impl TrafficRateWindow {
    pub(super) fn new(history_duration_secs: u64, bucket_duration_secs: u64) -> Self {
        assert!(bucket_duration_secs > 0, "bucket duration must be at least one second");
        assert!(
            history_duration_secs >= bucket_duration_secs,
            "history duration must be greater than or equal to bucket duration"
        );

        Self {
            bucket_sum: vec![0; (history_duration_secs / bucket_duration_secs) as usize],
            current_index: 0,
            current_index_time_secs: None,
            start_index_time_secs: None,
            sum: 0,
            bucket_duration_secs,
        }
    }

    pub(super) fn record(&mut self, now_secs: u64, bytes: u64) -> f64 {
        if self.start_index_time_secs.is_none() {
            self.start_index_time_secs = Some(now_secs);
            self.current_index_time_secs = Some(now_secs);
        }

        while now_secs
            >= self.current_index_time_secs.expect("current index time should be set") + self.bucket_duration_secs
        {
            self.current_index = (self.current_index + 1) % self.bucket_sum.len();
            self.sum -= self.bucket_sum[self.current_index];
            self.bucket_sum[self.current_index] = 0;

            let current_index_time_secs =
                self.current_index_time_secs.expect("current index time should be set") + self.bucket_duration_secs;
            self.current_index_time_secs = Some(current_index_time_secs);

            let start_index_time_secs = self.start_index_time_secs.expect("start index time should be set");
            if current_index_time_secs
                >= start_index_time_secs + self.bucket_sum.len() as u64 * self.bucket_duration_secs
            {
                self.start_index_time_secs = Some(start_index_time_secs + self.bucket_duration_secs);
            }
        }

        self.bucket_sum[self.current_index] += bytes;
        self.sum += bytes;
        self.bytes_per_sec(now_secs)
    }

    fn bytes_per_sec(&self, now_secs: u64) -> f64 {
        let Some(start_index_time_secs) = self.start_index_time_secs else {
            return 0.0;
        };

        let duration_secs = now_secs.saturating_sub(start_index_time_secs) + 1;
        self.sum as f64 / duration_secs as f64
    }
}

/// Tracks endpoint retry queue capacity inputs and computes per-domain capacity estimates.
pub(super) struct RetryQueueCapacityAggregator {
    per_endpoint: FastHashMap<MetaString, EndpointCapacityStats>,
    per_domain: FastHashMap<String, DomainCapacityStats>,
}

impl RetryQueueCapacityAggregator {
    pub(super) fn new() -> Self {
        Self {
            per_endpoint: FastHashMap::default(),
            per_domain: FastHashMap::default(),
        }
    }

    pub(super) fn update_endpoint(
        &mut self, endpoint_id: MetaString, domain: MetaString, bytes_per_sec: f64, in_memory_capacity_bytes: u64,
        disk_available_capacity_bytes: u64,
    ) {
        self.per_endpoint.insert(
            endpoint_id,
            EndpointCapacityStats {
                domain,
                bytes_per_sec,
                in_memory_capacity_bytes,
                disk_available_capacity_bytes,
            },
        );
        self.recompute_domain_stats();
    }

    pub(super) fn total_bytes_per_sec(&self) -> f64 {
        self.per_endpoint.values().map(|stats| stats.bytes_per_sec).sum::<f64>()
    }

    pub(super) fn domain_stats(&self) -> impl Iterator<Item = (&str, DomainCapacityStats)> {
        self.per_domain.iter().map(|(domain, stats)| (domain.as_str(), *stats))
    }

    fn recompute_domain_stats(&mut self) {
        let mut per_domain = FastHashMap::<String, (f64, u64)>::default();
        let mut total_bytes_per_sec = 0.0;
        let mut shared_disk_available_capacity_bytes = 0;

        for endpoint_stats in self.per_endpoint.values() {
            let domain = endpoint_stats.domain.to_string();
            let (domain_bytes_per_sec, domain_in_memory_capacity_bytes) = per_domain.entry(domain).or_insert((0.0, 0));
            *domain_bytes_per_sec += endpoint_stats.bytes_per_sec;
            *domain_in_memory_capacity_bytes =
                (*domain_in_memory_capacity_bytes).max(endpoint_stats.in_memory_capacity_bytes);

            total_bytes_per_sec += endpoint_stats.bytes_per_sec;
            shared_disk_available_capacity_bytes =
                shared_disk_available_capacity_bytes.max(endpoint_stats.disk_available_capacity_bytes);
        }

        self.per_domain.clear();
        for (domain, (domain_bytes_per_sec, domain_in_memory_capacity_bytes)) in per_domain {
            if domain_bytes_per_sec <= 0.0 || total_bytes_per_sec <= 0.0 {
                continue;
            }

            let relative_rate = domain_bytes_per_sec / total_bytes_per_sec;
            let domain_disk_capacity_bytes = shared_disk_available_capacity_bytes as f64 * relative_rate;
            let capacity_bytes = domain_in_memory_capacity_bytes as f64 + domain_disk_capacity_bytes;
            let capacity_secs = capacity_bytes / domain_bytes_per_sec;

            self.per_domain.insert(
                domain,
                DomainCapacityStats {
                    bytes_per_sec: domain_bytes_per_sec,
                    capacity_bytes,
                    capacity_secs,
                },
            );
        }
    }
}

struct EndpointCapacityStats {
    domain: MetaString,
    bytes_per_sec: f64,
    in_memory_capacity_bytes: u64,
    disk_available_capacity_bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn domain_stats(aggregator: &RetryQueueCapacityAggregator, domain: &str) -> Option<DomainCapacityStats> {
        aggregator
            .domain_stats()
            .find_map(|(stats_domain, stats)| (stats_domain == domain).then_some(stats))
    }

    #[test]
    fn traffic_rate_window_matches_agent_windowed_rate() {
        let mut traffic_rate = TrafficRateWindow::new(10, 1);

        assert_eq!(traffic_rate.record(1, 5), 5.0);
        assert_eq!(traffic_rate.record(2, 15), 10.0);
    }

    #[test]
    fn capacity_aggregator_computes_memory_only_capacity() {
        let mut aggregator = RetryQueueCapacityAggregator::new();

        aggregator.update_endpoint(
            MetaString::from_static("endpoint"),
            MetaString::from_static("domain"),
            10.0,
            20,
            0,
        );

        let stats = domain_stats(&aggregator, "domain").expect("domain should have stats");
        assert_eq!(stats.bytes_per_sec, 10.0);
        assert_eq!(stats.capacity_bytes, 20.0);
        assert_eq!(stats.capacity_secs, 2.0);
    }

    #[test]
    fn capacity_aggregator_computes_memory_and_disk_capacity() {
        let mut aggregator = RetryQueueCapacityAggregator::new();

        aggregator.update_endpoint(
            MetaString::from_static("endpoint"),
            MetaString::from_static("domain"),
            10.0,
            20,
            50,
        );

        let stats = domain_stats(&aggregator, "domain").expect("domain should have stats");
        assert_eq!(stats.bytes_per_sec, 10.0);
        assert_eq!(stats.capacity_bytes, 70.0);
        assert_eq!(stats.capacity_secs, 7.0);
    }

    #[test]
    fn capacity_aggregator_splits_shared_disk_by_domain_rate() {
        let mut aggregator = RetryQueueCapacityAggregator::new();

        aggregator.update_endpoint(
            MetaString::from_static("one"),
            MetaString::from_static("domain1"),
            2.5,
            20,
            50,
        );
        aggregator.update_endpoint(
            MetaString::from_static("two"),
            MetaString::from_static("domain2"),
            1.5,
            20,
            50,
        );

        let domain1 = domain_stats(&aggregator, "domain1").expect("domain1 should have stats");
        let domain2 = domain_stats(&aggregator, "domain2").expect("domain2 should have stats");

        assert_eq!(domain1.bytes_per_sec, 2.5);
        assert_eq!(domain1.capacity_bytes, 51.25);
        assert_eq!(domain1.capacity_secs, 20.5);
        assert_eq!(domain2.bytes_per_sec, 1.5);
        assert_eq!(domain2.capacity_bytes, 38.75);
        assert_eq!(domain2.capacity_secs, 25.833333333333332);
    }

    #[test]
    fn capacity_aggregator_skips_zero_rate_domains() {
        let mut aggregator = RetryQueueCapacityAggregator::new();

        aggregator.update_endpoint(
            MetaString::from_static("endpoint"),
            MetaString::from_static("domain"),
            0.0,
            20,
            50,
        );

        assert!(domain_stats(&aggregator, "domain").is_none());
    }
}
