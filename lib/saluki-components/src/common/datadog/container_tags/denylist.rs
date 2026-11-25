use std::time::{Duration, Instant};

use saluki_common::collections::FastHashMap;
use stringtheory::MetaString;

use super::DENY_DURATION;

/// How often a container that the resolver sees again has its last-seen time refreshed.
const DENY_REFRESH_INTERVAL: Duration = Duration::from_secs(10);

/// How often the deny list drops the containers that expired.
const EVICTION_INTERVAL: Duration = Duration::from_secs(10);

/// The containers whose wait for tags ran out.
///
/// A container stays denied while the resolver keeps seeing it. It leaves the list `DENY_DURATION` after the resolver
/// last saw it.
#[derive(Default)]
pub(super) struct DeniedContainers {
    last_seen: FastHashMap<MetaString, Instant>,
    last_eviction: Option<Instant>,
}

impl DeniedContainers {
    /// Denies a container from `now`.
    pub fn deny(&mut self, now: Instant, container_id: &MetaString) {
        self.last_seen.insert(container_id.clone(), now);
    }

    /// Returns `true` if the container is denied, and refreshes the time that the resolver last saw it.
    ///
    /// An expired entry leaves the list.
    pub fn is_denied(&mut self, now: Instant, container_id: &MetaString) -> bool {
        let Some(last_seen) = self.last_seen.get_mut(container_id) else {
            return false;
        };
        let age = now.saturating_duration_since(*last_seen);
        if age > DENY_DURATION {
            self.last_seen.remove(container_id);
            return false;
        }
        if age > DENY_REFRESH_INTERVAL {
            *last_seen = now;
        }
        true
    }

    /// Removes the expired containers, at most once every `EVICTION_INTERVAL`, and returns how many it removed.
    pub fn evict_expired(&mut self, now: Instant) -> usize {
        if let Some(last_eviction) = self.last_eviction {
            if now.saturating_duration_since(last_eviction) < EVICTION_INTERVAL {
                return 0;
            }
        }
        self.last_eviction = Some(now);

        let before = self.last_seen.len();
        self.last_seen
            .retain(|_, last_seen| now.saturating_duration_since(*last_seen) <= DENY_DURATION);
        before - self.last_seen.len()
    }

    pub fn len(&self) -> usize {
        self.last_seen.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(value: &'static str) -> MetaString {
        MetaString::from_static(value)
    }

    #[test]
    fn a_denied_container_stays_denied_for_two_minutes_then_leaves_the_list() {
        let start = Instant::now();
        let mut denied = DeniedContainers::default();
        denied.deny(start, &id("a"));

        assert!(!denied.is_denied(start, &id("other")));
        assert!(denied.is_denied(start, &id("a")));
        assert!(denied.is_denied(start + DENY_DURATION, &id("a")));

        // The resolver refreshed the entry at the previous check, so it counts from there.
        let after = start + DENY_DURATION + DENY_DURATION + Duration::from_secs(1);
        assert!(!denied.is_denied(after, &id("a")));
        assert_eq!(0, denied.len(), "the check removes the expired entry");
    }

    #[test]
    fn seeing_a_container_again_extends_the_denial() {
        let start = Instant::now();
        let mut denied = DeniedContainers::default();
        denied.deny(start, &id("a"));

        // Each check after the refresh interval moves the last-seen time forward.
        let mut now = start;
        for _ in 0..6 {
            now += Duration::from_secs(60);
            assert!(denied.is_denied(now, &id("a")));
        }
        assert!(!denied.is_denied(now + DENY_DURATION + Duration::from_secs(1), &id("a")));
    }

    #[test]
    fn eviction_removes_only_the_expired_containers() {
        let start = Instant::now();
        let mut denied = DeniedContainers::default();
        denied.deny(start, &id("old"));
        denied.deny(start + Duration::from_secs(100), &id("recent"));

        // Too early: nothing expired, and the pass sets the eviction time.
        assert_eq!(0, denied.evict_expired(start + Duration::from_secs(60)));
        assert_eq!(2, denied.len());

        let now = start + DENY_DURATION + Duration::from_secs(10);
        assert_eq!(1, denied.evict_expired(now));
        assert_eq!(1, denied.len());
        assert!(denied.is_denied(now, &id("recent")));

        // A second pass inside the eviction interval does nothing.
        assert_eq!(0, denied.evict_expired(now + Duration::from_secs(1)));
    }
}
