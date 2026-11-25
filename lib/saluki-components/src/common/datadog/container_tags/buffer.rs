use std::{sync::Arc, time::Instant};

use saluki_common::collections::FastIndexMap;
use stringtheory::MetaString;

use super::{
    telemetry::BufferTelemetry, ContainerTagLookup, ContainerTagsResolver, Released, Resolution, Shared,
    MAX_BUFFER_DURATION,
};

/// A resolver that holds items until their container has a `kube_` tag, or until the wait runs out.
pub(super) struct BufferingResolver<T> {
    lookup: ContainerTagLookup,
    shared: Arc<Shared>,
    pending: FastIndexMap<MetaString, PendingContainer<T>>,
    pending_items: usize,
    telemetry: BufferTelemetry,
}

impl<T> Drop for BufferingResolver<T> {
    fn drop(&mut self) {
        // An error or cancellation can bypass the consumer's final flush.
        for container in self.pending.values() {
            for item in &container.items {
                self.shared.release(item.size);
            }
        }
        self.telemetry.set_pending(0, self.shared.pending_bytes());
    }
}

/// The items held for one container, in submission order.
struct PendingContainer<T> {
    expires_at: Instant,
    items: Vec<PendingItem<T>>,
}

struct PendingItem<T> {
    item: T,
    size: usize,
}

impl<T> BufferingResolver<T> {
    pub fn new(lookup: ContainerTagLookup, shared: Arc<Shared>, telemetry: BufferTelemetry) -> Self {
        Self {
            lookup,
            shared,
            pending: FastIndexMap::default(),
            pending_items: 0,
            telemetry,
        }
    }

    /// Releases the held items of a container with the given resolution.
    fn release_container(
        &mut self, container_id: &MetaString, resolution: &Resolution, released: &mut Vec<Released<T>>,
    ) {
        if let Some(container) = self.pending.shift_remove(container_id) {
            self.pending_items = self.pending_items.saturating_sub(container.items.len());
            release_items(&self.shared, container.items, resolution, released);
        }
    }

    fn report_pending(&self) {
        self.telemetry
            .set_pending(self.pending_items, self.shared.pending_bytes());
    }
}

/// Moves the items of a container into `released`, and gives back their bytes.
fn release_items<T>(
    shared: &Shared, items: impl IntoIterator<Item = PendingItem<T>>, resolution: &Resolution,
    released: &mut Vec<Released<T>>,
) {
    for pending in items {
        shared.release(pending.size);
        released.push(Released {
            item: pending.item,
            resolution: resolution.clone(),
        });
    }
}

impl<T> ContainerTagsResolver<T> for BufferingResolver<T>
where
    T: Send,
{
    fn submit(
        &mut self, container_id: &MetaString, item: T, size: usize, now: Instant, released: &mut Vec<Released<T>>,
    ) {
        let lookup = self.lookup.lookup(container_id);
        let complete = lookup.is_complete();
        let resolution = lookup.into_resolution();

        // Held items of the container go first, so the new item never overtakes them.
        if complete {
            self.release_container(container_id, &resolution, released);
        } else if self.shared.denied().is_denied(now, container_id) {
            self.telemetry.track_denied_by_denylist();
            self.release_container(container_id, &resolution, released);
        } else if !self.shared.try_reserve(size) {
            self.telemetry.track_denied_by_memory_limit();
            self.release_container(container_id, &resolution, released);
        } else {
            let container = self
                .pending
                .entry(container_id.clone())
                .or_insert_with(|| PendingContainer {
                    expires_at: now + MAX_BUFFER_DURATION,
                    items: Vec::new(),
                });
            container.items.push(PendingItem { item, size });
            self.pending_items += 1;
            self.telemetry.track_buffered();
            self.report_pending();
            return;
        }

        released.push(Released { item, resolution });
        self.report_pending();
    }

    fn tick(&mut self, now: Instant, released: &mut Vec<Released<T>>) {
        let expired = self.shared.denied().evict_expired(now);
        self.telemetry.track_denylist_expired(expired);

        let Self {
            lookup,
            shared,
            pending,
            pending_items,
            ..
        } = self;
        let mut still_pending = 0;
        pending.retain(|container_id, container| {
            let found = lookup.lookup(container_id);
            let resolution = if found.is_complete() {
                found.into_resolution()
            } else if now >= container.expires_at {
                // The deny list learns of the container before the items leave, so a new item doesn't wait again.
                shared.denied().deny(now, container_id);
                found.into_expired_resolution()
            } else {
                still_pending += container.items.len();
                return true;
            };
            release_items(shared, container.items.drain(..), &resolution, released);
            false
        });
        *pending_items = still_pending;

        // Report the deny list after the retry, which can deny the containers that ran out of time.
        let denylist_size = self.shared.denied().len();
        self.telemetry.set_denylist_size(denylist_size);
        self.report_pending();
    }

    fn flush_all(&mut self, released: &mut Vec<Released<T>>) {
        for (container_id, container) in self.pending.drain(..) {
            let resolution = self.lookup.lookup(&container_id).into_resolution();
            release_items(&self.shared, container.items, &resolution, released);
        }
        self.pending_items = 0;
        self.report_pending();
    }

    fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use metrics::Key;
    use saluki_core::data_model::origin::OriginTagCardinality;
    use saluki_metrics::{test::TestRecorder, MetricsBuilder};

    use super::*;
    use crate::common::datadog::container_tags::{
        resolver, test_util::ScriptedProvider, ContainerTagsBuffer, DENY_DURATION, RETRY_INTERVAL,
    };

    const SECOND: Duration = Duration::from_secs(1);

    /// A buffering resolver over a scripted provider, with a clock that the test advances.
    struct Harness {
        provider: Arc<ScriptedProvider>,
        resolver: Box<dyn ContainerTagsResolver<&'static str>>,
        start: Instant,
        released: Vec<Released<&'static str>>,
    }

    impl Harness {
        fn new() -> Self {
            Self::with_buffer(&ContainerTagsBuffer::new(1_000_000))
        }

        fn with_buffer(buffer: &ContainerTagsBuffer) -> Self {
            let provider = Arc::new(ScriptedProvider::default());
            Self {
                resolver: resolver(Some(provider.clone()), Some(buffer), &MetricsBuilder::default()),
                provider,
                start: Instant::now(),
                released: Vec::new(),
            }
        }

        fn at(&self, seconds: u64) -> Instant {
            self.start + SECOND * seconds as u32
        }

        fn submit(&mut self, container_id: &'static str, item: &'static str, seconds: u64) {
            self.submit_sized(container_id, item, 10, seconds);
        }

        fn submit_sized(&mut self, container_id: &'static str, item: &'static str, size: usize, seconds: u64) {
            let now = self.at(seconds);
            self.resolver
                .submit(&MetaString::from(container_id), item, size, now, &mut self.released);
        }

        /// Ticks once a second until `seconds`, as a consumer does while items are pending.
        fn tick_until(&mut self, from: u64, seconds: u64) {
            for second in from..=seconds {
                let now = self.at(second);
                self.resolver.tick(now, &mut self.released);
            }
        }

        fn take_released(&mut self) -> Vec<(&'static str, String)> {
            self.released
                .drain(..)
                .map(|released| (released.item, describe(&released.resolution)))
                .collect()
        }
    }

    fn describe(resolution: &Resolution) -> String {
        match resolution {
            Resolution::Tags(tags) => {
                let mut tags: Vec<_> = tags.into_iter().map(|tag| tag.as_str().to_string()).collect();
                tags.sort();
                format!("tags[{}]", tags.join(","))
            }
            Resolution::Failed => "failed".to_string(),
        }
    }

    #[test]
    fn a_container_with_a_kube_tag_resolves_at_once() {
        let mut harness = Harness::new();
        harness.provider.set_tags("c1", &["kube_namespace:prod", "image_tag:1"]);

        harness.submit("c1", "t1", 0);

        assert_eq!(
            vec![("t1", "tags[image_tag:1,kube_namespace:prod]".to_string())],
            harness.take_released()
        );
        assert!(!harness.resolver.has_pending());
        assert_eq!(Some(OriginTagCardinality::High), harness.provider.last_cardinality());
    }

    #[test]
    fn a_container_without_a_kube_tag_is_retried_every_second_until_one_appears() {
        let mut harness = Harness::new();
        harness.provider.set_tags("c1", &["image_tag:1"]);

        harness.submit("c1", "t1", 0);
        assert!(harness.take_released().is_empty(), "the tags have no kube_ tag yet");
        assert!(harness.resolver.has_pending());
        assert_eq!(1, harness.provider.lookups());

        // One lookup for each tick, and the item stays held.
        harness.tick_until(1, 5);
        assert!(harness.take_released().is_empty());
        assert_eq!(6, harness.provider.lookups());

        harness.provider.set_tags("c1", &["image_tag:1", "kube_namespace:prod"]);
        harness.tick_until(6, 6);
        assert_eq!(
            vec![("t1", "tags[image_tag:1,kube_namespace:prod]".to_string())],
            harness.take_released()
        );
        assert!(!harness.resolver.has_pending());

        // A resolved container doesn't enter the deny list: the next incomplete item waits again.
        harness.provider.set_tags("c2", &[]);
        harness.submit("c2", "t2", 7);
        assert!(harness.resolver.has_pending());
    }

    #[test]
    fn a_container_that_never_resolves_is_released_after_twelve_seconds_with_the_tags_it_has() {
        let mut harness = Harness::new();
        harness.provider.set_tags("c1", &["image_tag:1"]);

        harness.submit("c1", "t1", 0);
        harness.tick_until(1, 11);
        assert!(harness.take_released().is_empty(), "the wait lasts 12 seconds");

        harness.tick_until(12, 12);
        assert_eq!(vec![("t1", "tags[image_tag:1]".to_string())], harness.take_released());
        assert!(!harness.resolver.has_pending());
    }

    #[test]
    fn a_container_the_provider_never_learns_expires_with_no_tags_rather_than_failing() {
        // As in the Agent, a wait that ran out reports no error, so stats keep the container ID.
        let mut harness = Harness::new();

        harness.submit("unknown", "t1", 0);
        harness.tick_until(1, 12);

        assert_eq!(vec![("t1", "tags[]".to_string())], harness.take_released());
    }

    #[test]
    fn an_expired_container_is_denied_for_two_minutes_and_then_waits_again() {
        let mut harness = Harness::new();
        harness.provider.set_tags("c1", &["image_tag:1"]);
        harness.submit("c1", "t1", 0);
        harness.tick_until(1, 12);
        harness.take_released();

        // The container expired at 12s. While denied, an item doesn't wait.
        harness.submit("c1", "denied", 13);
        assert_eq!(
            vec![("denied", "tags[image_tag:1]".to_string())],
            harness.take_released()
        );
        assert!(!harness.resolver.has_pending());

        // A denied container that the resolver doesn't see for two minutes leaves the list.
        harness.submit("c1", "waits", 13 + DENY_DURATION.as_secs() + 1);
        assert!(harness.take_released().is_empty());
        assert!(harness.resolver.has_pending());
    }

    #[test]
    fn a_denied_container_the_provider_does_not_know_fails_the_lookup() {
        let mut harness = Harness::new();
        harness.submit("unknown", "t1", 0);
        harness.tick_until(1, 12);
        harness.take_released();

        harness.submit("unknown", "t2", 13);

        assert_eq!(vec![("t2", "failed".to_string())], harness.take_released());
    }

    #[test]
    fn the_deny_list_is_shared_between_resolvers_of_one_buffer() {
        let buffer = ContainerTagsBuffer::new(1_000_000);
        let mut first = Harness::with_buffer(&buffer);
        let mut second = Harness::with_buffer(&buffer);
        first.provider.set_tags("c1", &["image_tag:1"]);
        second.provider.set_tags("c1", &["image_tag:1"]);

        first.submit("c1", "t1", 0);
        first.tick_until(1, 12);

        // The second resolver started its clock at its own start, so submit near the first one's expiry time.
        let now = first.at(13);
        second
            .resolver
            .submit(&MetaString::from("c1"), "t2", 10, now, &mut second.released);
        assert_eq!(
            1,
            second.released.len(),
            "the second resolver doesn't wait for a denied container"
        );
        assert!(!second.resolver.has_pending());
    }

    #[test]
    fn the_pending_bytes_cap_makes_an_item_skip_the_wait() {
        let buffer = ContainerTagsBuffer::new(100);
        let mut harness = Harness::with_buffer(&buffer);
        harness.provider.set_tags("c1", &["image_tag:1"]);
        harness.provider.set_tags("c2", &["image_tag:2"]);

        harness.submit_sized("c1", "fits", 60, 0);
        harness.submit_sized("c2", "over the cap", 60, 0);
        assert_eq!(
            vec![("over the cap", "tags[image_tag:2]".to_string())],
            harness.take_released()
        );

        // The held item gives its bytes back when it leaves, and the cap admits the next one.
        harness.tick_until(1, 12);
        assert_eq!(vec![("fits", "tags[image_tag:1]".to_string())], harness.take_released());
        harness.submit_sized("c3", "fits now", 100, 13);
        assert!(harness.resolver.has_pending());
    }

    #[test]
    fn an_item_over_the_cap_releases_the_held_items_of_its_container_first() {
        let mut harness = Harness::with_buffer(&ContainerTagsBuffer::new(100));
        harness.provider.set_tags("c1", &["image_tag:1"]);

        harness.submit_sized("c1", "first", 60, 0);
        harness.submit_sized("c1", "second", 60, 1);

        let released = harness.take_released();
        assert_eq!(
            vec!["first", "second"],
            released.iter().map(|r| r.0).collect::<Vec<_>>()
        );
        assert!(!harness.resolver.has_pending());
    }

    #[test]
    fn a_new_item_never_overtakes_the_held_items_of_its_container() {
        let mut harness = Harness::new();
        harness.provider.set_tags("c1", &["image_tag:1"]);
        harness.submit("c1", "first", 0);
        harness.submit("c1", "second", 1);
        assert!(harness.take_released().is_empty());

        harness.provider.set_tags("c1", &["kube_namespace:prod"]);
        harness.submit("c1", "third", 2);

        let released = harness.take_released();
        assert_eq!(
            vec!["first", "second", "third"],
            released.iter().map(|r| r.0).collect::<Vec<_>>()
        );
        assert!(!harness.resolver.has_pending());
    }

    #[test]
    fn the_items_of_a_container_share_its_deadline() {
        let mut harness = Harness::new();
        harness.provider.set_tags("c1", &["image_tag:1"]);
        harness.submit("c1", "first", 0);
        harness.submit("c1", "second", 8);

        harness.tick_until(1, 12);

        let released = harness.take_released();
        assert_eq!(
            vec!["first", "second"],
            released.iter().map(|r| r.0).collect::<Vec<_>>()
        );
    }

    #[test]
    fn flush_all_releases_every_pending_item_with_the_current_lookup() {
        let mut harness = Harness::new();
        harness.provider.set_tags("c1", &["image_tag:1"]);
        harness.submit("c1", "a1", 0);
        harness.submit("unknown", "b1", 0);
        harness.submit("c1", "a2", 1);

        harness.resolver.flush_all(&mut harness.released);

        // The final lookup reports an unknown container as failed, as the Agent's forced flush does.
        let mut released = harness.take_released();
        released.sort();
        assert_eq!(
            vec![
                ("a1", "tags[image_tag:1]".to_string()),
                ("a2", "tags[image_tag:1]".to_string()),
                ("b1", "failed".to_string()),
            ],
            released
        );
        assert!(!harness.resolver.has_pending());
    }

    #[test]
    fn flush_all_gives_the_pending_bytes_back() {
        let buffer = ContainerTagsBuffer::new(100);
        let mut harness = Harness::with_buffer(&buffer);
        harness.submit_sized("c1", "a", 100, 0);
        harness.resolver.flush_all(&mut harness.released);
        harness.take_released();

        harness.submit_sized("c1", "b", 100, 1);

        assert!(harness.resolver.has_pending(), "the cap has room again");
    }

    #[test]
    fn dropping_a_resolver_releases_its_shared_reservations() {
        let buffer = ContainerTagsBuffer::new(100);
        let mut first = Harness::with_buffer(&buffer);
        first.submit_sized("c1", "first", 100, 0);
        assert!(first.resolver.has_pending());

        drop(first);
        let mut second = Harness::with_buffer(&buffer);
        second.submit_sized("c2", "second", 100, 0);

        assert!(second.take_released().is_empty());
        assert!(
            second.resolver.has_pending(),
            "the first resolver returned the shared reservation"
        );
    }

    #[test]
    fn ticking_an_idle_resolver_evicts_expired_denials() {
        let buffer = ContainerTagsBuffer::new(100);
        let mut harness = Harness::with_buffer(&buffer);
        harness.submit("unknown", "item", 0);
        harness.tick_until(1, 12);
        harness.take_released();
        assert!(!harness.resolver.has_pending());
        assert_eq!(1, buffer.shared.denied().len());

        harness.tick_until(13, 12 + DENY_DURATION.as_secs() + 10);

        assert_eq!(0, buffer.shared.denied().len());
    }

    #[test]
    fn the_no_op_resolver_never_waits() {
        let provider = Arc::new(ScriptedProvider::default());
        provider.set_tags("c1", &["image_tag:1"]);
        let mut resolver = resolver::<&'static str>(Some(provider), None, &MetricsBuilder::default());
        let mut released = Vec::new();
        let now = Instant::now();

        // An incomplete container and an unknown one both resolve at once.
        resolver.submit(&MetaString::from("c1"), "known", 10, now, &mut released);
        resolver.submit(&MetaString::from("unknown"), "unknown", 10, now, &mut released);

        let described: Vec<_> = released.iter().map(|r| (r.item, describe(&r.resolution))).collect();
        assert_eq!(
            vec![
                ("known", "tags[image_tag:1]".to_string()),
                ("unknown", "failed".to_string())
            ],
            described
        );
        assert!(!resolver.has_pending());
    }

    #[test]
    fn a_resolver_without_a_provider_adds_no_tags_and_does_not_fail() {
        let mut resolver =
            resolver::<&'static str>(None, Some(&ContainerTagsBuffer::new(100)), &MetricsBuilder::default());
        let mut released = Vec::new();

        resolver.submit(&MetaString::from("c1"), "item", 10, Instant::now(), &mut released);

        assert_eq!(
            vec![("item", "tags[]".to_string())],
            released
                .iter()
                .map(|r| (r.item, describe(&r.resolution)))
                .collect::<Vec<_>>()
        );
        assert!(!resolver.has_pending());
    }

    #[test]
    fn the_cap_follows_the_memory_limit() {
        for (limit, expected) in [
            (5e8, 50_000_000),
            (1e6, 100_000),
            (0.0, 7_000_000),
            (-1.0, 7_000_000),
            (f64::NAN, 7_000_000),
        ] {
            assert_eq!(
                expected,
                ContainerTagsBuffer::from_memory_limit(limit).max_pending_bytes(),
                "limit {limit}"
            );
        }
    }

    #[test]
    fn telemetry_reports_buffering_denials_and_pending_state() {
        let recorder = TestRecorder::default();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let mut harness = Harness::with_buffer(&ContainerTagsBuffer::new(100));
        let counter = |name: &'static str, tags: Vec<metrics::Label>| recorder.counter(Key::from_parts(name, tags));
        let gauge = |name: &'static str| recorder.gauge(Key::from_name(name));
        let reason = |reason: &'static str| vec![metrics::Label::new("reason", reason)];

        harness.submit_sized("c1", "held", 60, 0);
        assert_eq!(Some(1), counter("tag_buffer_buffered_payloads_total", vec![]));
        assert_eq!(Some(1.0), gauge("tag_buffer_pending_payloads"));
        assert_eq!(Some(60.0), gauge("tag_buffer_memory_usage_bytes"));

        harness.submit_sized("c2", "over the cap", 60, 0);
        assert_eq!(
            Some(1),
            counter("tag_buffer_denied_payloads_total", reason("memory_limit"))
        );

        harness.tick_until(1, 12);
        assert_eq!(Some(0.0), gauge("tag_buffer_pending_payloads"));
        assert_eq!(Some(0.0), gauge("tag_buffer_memory_usage_bytes"));
        assert_eq!(Some(1.0), gauge("tag_buffer_denylist_size"));

        harness.submit("c1", "denied", 13);
        assert_eq!(Some(1), counter("tag_buffer_denied_payloads_total", reason("denylist")));

        harness.tick_until(14, 14 + DENY_DURATION.as_secs() + 20);
        assert_eq!(Some(1), counter("tag_buffer_denylist_expired_total", vec![]));
        assert_eq!(Some(0.0), gauge("tag_buffer_denylist_size"));
    }

    #[test]
    fn the_retry_interval_is_one_second() {
        assert_eq!(SECOND, RETRY_INTERVAL);
    }
}
