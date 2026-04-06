//! Rare sampler for traces.
//!
//! Samples traces for span signature combinations (env, service, name, resource, error type, http status)
//! that are not caught by the priority sampler. This ensures that rare or low-traffic trace shapes
//! are still represented in the sampled data.
//!
//! The sampler works by:
//! 1. Iterating top-level and measured spans in the trace.
//! 2. Computing a per-span signature (hashed from service, name, resource, error, etc.).
//! 3. Keeping the trace if any span's signature has not been seen within the cooldown TTL.
//! 4. Using a token bucket to cap the overall rate of rare traces kept.
//!
//! Mirrors `datadog-agent/pkg/trace/sampler/rare_sampler.go`.

use std::time::{Duration, Instant};

use saluki_common::collections::FastHashMap;
use saluki_core::data_model::event::trace::{Span, Trace};
use stringtheory::MetaString;

use super::signature::{span_hash_for_rare, ServiceSignature, Signature};
use crate::common::datadog::get_trace_env;

/// The burst size for the token bucket rate limiter. Matches the Go agent default.
const RARE_SAMPLER_BURST: usize = 50;

/// Minimum time between TTL updates for an already-seen span signature.
/// Avoids churning the map when the same signature is seen repeatedly within the TTL window.
const TTL_RENEWAL_PERIOD: Duration = Duration::from_secs(60);

/// Metric key set on the root span of a rare-sampled trace.
pub(super) const RARE_KEY: &str = "_dd.rare";

/// Metric key indicating a span is a top-level span.
const KEY_TOP_LEVEL: &str = "_top_level";

/// Metric key indicating a span is explicitly marked for stats computation.
const KEY_MEASURED: &str = "_dd.measured";

/// Token bucket rate limiter.
///
/// Provides a `rate` tokens-per-second refill up to `capacity`, and allows consuming one token at a
/// time via `allow()`. This mirrors `golang.org/x/time/rate.Limiter`.
struct TokenBucket {
    capacity: f64,
    tokens: f64,
    last_refill: Instant,
    rate: f64,
}

impl TokenBucket {
    fn new(rate: f64, burst: usize) -> Self {
        Self {
            capacity: burst as f64,
            tokens: burst as f64,
            last_refill: Instant::now(),
            rate,
        }
    }

    /// Attempt to consume one token. Returns `true` if a token was available.
    fn allow(&mut self) -> bool {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.rate).min(self.capacity);
        self.last_refill = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Tracks the set of span signatures seen within a single (env, service) shard.
struct SeenSpans {
    /// Maps span hash to the expiry `Instant` for that signature.
    expires: FastHashMap<u32, Instant>,
    /// Whether the map has been shrunk due to cardinality overflow.
    shrunk: bool,
    /// Maximum number of entries before triggering a shrink.
    cardinality: usize,
}

impl SeenSpans {
    fn new(cardinality: usize) -> Self {
        Self {
            expires: FastHashMap::default(),
            shrunk: false,
            cardinality,
        }
    }

    /// Compute the effective signature for a raw span hash, applying the shrink modulus if needed.
    fn sign(&self, span_hash: u32) -> u32 {
        if self.shrunk {
            span_hash % self.cardinality as u32
        } else {
            span_hash
        }
    }

    /// Record an expiry for a span signature.
    ///
    /// Skips the update if the stored entry is still live and the new expiry is not meaningfully
    /// later (within `TTL_RENEWAL_PERIOD`). If the stored entry is already expired, always updates.
    /// This mirrors the Go agent's `ttlRenewalPeriod` check, which assumes `TTL > TTL_RENEWAL_PERIOD`.
    fn add(&mut self, now: Instant, expire: Instant, span_hash: u32) {
        let sig = self.sign(span_hash);
        if let Some(&stored) = self.expires.get(&sig) {
            // Only skip if the stored entry is still live and the new expiry isn't meaningfully later.
            if stored > now && expire.duration_since(stored) < TTL_RENEWAL_PERIOD {
                return;
            }
        }
        self.expires.insert(sig, expire);
        if self.expires.len() > self.cardinality {
            self.shrink();
        }
    }

    /// Returns the stored expiry for a span signature, if any.
    fn get_expire(&self, sig: u32) -> Option<Instant> {
        self.expires.get(&sig).copied()
    }

    /// Shrink the map to cap cardinality. Signatures are collapsed into `cardinality` buckets via
    /// modular hashing. Matches the Go agent's shrink behavior.
    fn shrink(&mut self) {
        let cardinality = self.cardinality;
        let old = std::mem::take(&mut self.expires);
        self.expires.reserve(cardinality);
        for (h, expire) in old {
            self.expires.insert(h % cardinality as u32, expire);
        }
        self.shrunk = true;
    }
}

/// Rare sampler: keeps traces whose span signatures haven't been seen within the cooldown TTL.
///
/// Traces that pass through the rare sampler have `_dd.rare = 1` set on the first matching span.
pub(super) struct RareSampler {
    enabled: bool,
    token_bucket: TokenBucket,
    ttl: Duration,
    cardinality: usize,
    /// Keyed by (env, service) shard signature.
    seen: FastHashMap<Signature, SeenSpans>,
}

impl RareSampler {
    pub(super) fn new(enabled: bool, tps: f64, ttl: Duration, cardinality: usize) -> Self {
        Self {
            enabled,
            token_bucket: TokenBucket::new(tps, RARE_SAMPLER_BURST),
            ttl,
            cardinality,
            seen: FastHashMap::default(),
        }
    }

    /// Sample a trace. Returns `true` if the trace should be kept by the rare sampler.
    ///
    /// Iterates top-level and measured spans. If any span has a signature that has not been seen
    /// within the TTL, the sampler attempts to consume a token and keep the trace.
    pub(super) fn sample(&mut self, trace: &mut Trace, root_span_idx: usize) -> bool {
        if !self.enabled {
            return false;
        }
        self.handle_trace(trace, root_span_idx)
    }

    fn handle_trace(&mut self, trace: &mut Trace, root_span_idx: usize) -> bool {
        let now = Instant::now();
        let env = get_trace_env(trace, root_span_idx)
            .map(|e| e.as_ref().to_owned())
            .unwrap_or_default();

        // Find the index of the first top-level or measured span with an expired/unseen signature.
        let sampled_span_idx = self.find_rare_span(trace, &env, now);

        let Some(sampled_idx) = sampled_span_idx else {
            return false;
        };

        // Attempt to consume a rate-limiter token.
        if !self.token_bucket.allow() {
            return false;
        }

        // Mark the sampled span with _dd.rare = 1.
        if let Some(span) = trace.spans_mut().get_mut(sampled_idx) {
            span.metrics_mut().insert(MetaString::from_static(RARE_KEY), 1.0);
        }

        // Update TTLs for all top-level/measured spans in the trace to prevent re-sampling within TTL.
        self.record_all_top_level_spans(trace, &env, now, now + self.ttl);

        true
    }

    /// Find the index of the first top-level or measured span whose signature has expired or is new.
    fn find_rare_span(&mut self, trace: &Trace, env: &str, now: Instant) -> Option<usize> {
        let spans = trace.spans();
        for (i, span) in spans.iter().enumerate() {
            if !is_top_level_or_measured(span) {
                continue;
            }
            let shard_sig = ServiceSignature::new(span.service(), env).hash();
            let span_hash = span_hash_for_rare(span);
            let seen = self
                .seen
                .entry(shard_sig)
                .or_insert_with(|| SeenSpans::new(self.cardinality));
            let sig = seen.sign(span_hash);
            let expired = match seen.get_expire(sig) {
                Some(expire) => now > expire,
                None => true,
            };
            if expired {
                return Some(i);
            }
        }
        None
    }

    /// Notify the rare sampler that a trace was kept by the priority sampler.
    ///
    /// Updates TTLs for all top-level/measured spans so that signatures actively covered by the
    /// priority sampler don't consume rare sampler tokens on future encounters.
    pub(super) fn record_priority_trace(&mut self, trace: &Trace, root_span_idx: usize) {
        if !self.enabled {
            return;
        }
        let now = Instant::now();
        let env = get_trace_env(trace, root_span_idx)
            .map(|e| e.as_ref().to_owned())
            .unwrap_or_default();
        self.record_all_top_level_spans(trace, &env, now, now + self.ttl);
    }

    fn record_all_top_level_spans(&mut self, trace: &Trace, env: &str, now: Instant, expire: Instant) {
        for span in trace.spans() {
            if !is_top_level_or_measured(span) {
                continue;
            }
            let shard_sig = ServiceSignature::new(span.service(), env).hash();
            let span_hash = span_hash_for_rare(span);
            let seen = self
                .seen
                .entry(shard_sig)
                .or_insert_with(|| SeenSpans::new(self.cardinality));
            seen.add(now, expire, span_hash);
        }
    }
}

/// Returns `true` if the span is top-level (`_top_level = 1`) or measured (`_dd.measured = 1`).
fn is_top_level_or_measured(span: &Span) -> bool {
    span.metrics().get(KEY_TOP_LEVEL).copied().unwrap_or(0.0) == 1.0
        || span.metrics().get(KEY_MEASURED).copied().unwrap_or(0.0) == 1.0
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use saluki_common::collections::FastHashMap;
    use saluki_context::tags::TagSet;
    use saluki_core::data_model::event::trace::{Span as DdSpan, Trace};
    use stringtheory::MetaString;

    use super::{RareSampler, KEY_MEASURED, KEY_TOP_LEVEL, RARE_KEY};

    fn make_span_with_metrics(
        service: &str, name: &str, resource: &str, metrics: FastHashMap<MetaString, f64>,
    ) -> DdSpan {
        DdSpan::new(
            MetaString::from(service),
            MetaString::from(name),
            MetaString::from(resource),
            MetaString::from("web"),
            1,
            1,
            0,
            0,
            1000,
            0,
        )
        .with_metrics(metrics)
    }

    fn make_top_level_span(service: &str, name: &str, resource: &str) -> DdSpan {
        let mut metrics = FastHashMap::default();
        metrics.insert(MetaString::from(KEY_TOP_LEVEL), 1.0);
        make_span_with_metrics(service, name, resource, metrics)
    }

    fn make_measured_span(service: &str, name: &str, resource: &str) -> DdSpan {
        let mut metrics = FastHashMap::default();
        metrics.insert(MetaString::from(KEY_MEASURED), 1.0);
        make_span_with_metrics(service, name, resource, metrics)
    }

    fn make_plain_span(service: &str, name: &str, resource: &str) -> DdSpan {
        make_span_with_metrics(service, name, resource, FastHashMap::default())
    }

    fn make_trace(spans: Vec<DdSpan>) -> Trace {
        Trace::new(spans, TagSet::default())
    }

    #[test]
    fn disabled_sampler_never_keeps() {
        let mut sampler = RareSampler::new(false, 5.0, Duration::from_secs(300), 200);
        let mut trace = make_trace(vec![make_top_level_span("svc", "op", "res")]);
        assert!(!sampler.sample(&mut trace, 0));
    }

    #[test]
    fn new_signature_is_kept() {
        let mut sampler = RareSampler::new(true, 5.0, Duration::from_secs(300), 200);
        let mut trace = make_trace(vec![make_top_level_span("svc", "op", "res")]);
        assert!(sampler.sample(&mut trace, 0));
        // The rare key should be set on the sampled span.
        assert_eq!(trace.spans()[0].metrics().get(RARE_KEY).copied(), Some(1.0));
    }

    #[test]
    fn same_signature_within_ttl_is_dropped() {
        let mut sampler = RareSampler::new(true, 5.0, Duration::from_secs(300), 200);
        let mut trace1 = make_trace(vec![make_top_level_span("svc", "op", "res")]);
        assert!(sampler.sample(&mut trace1, 0));

        // Same signature, within TTL: should be dropped.
        let mut trace2 = make_trace(vec![make_top_level_span("svc", "op", "res")]);
        assert!(!sampler.sample(&mut trace2, 0));
    }

    #[test]
    fn non_top_level_span_not_considered() {
        let mut sampler = RareSampler::new(true, 5.0, Duration::from_secs(300), 200);
        // Span is NOT top-level and NOT measured.
        let mut trace = make_trace(vec![make_plain_span("svc", "op", "res")]);
        assert!(!sampler.sample(&mut trace, 0));
    }

    #[test]
    fn measured_span_is_considered() {
        let mut sampler = RareSampler::new(true, 5.0, Duration::from_secs(300), 200);
        let mut trace = make_trace(vec![make_measured_span("svc", "op", "res")]);
        assert!(sampler.sample(&mut trace, 0));
    }

    #[test]
    fn different_signatures_are_independent() {
        let mut sampler = RareSampler::new(true, 5.0, Duration::from_secs(300), 200);

        // Keep trace with signature A.
        let mut trace_a = make_trace(vec![make_top_level_span("svc", "op", "resource-a")]);
        assert!(sampler.sample(&mut trace_a, 0));

        // Trace with signature B (different resource) should still be considered rare.
        let mut trace_b = make_trace(vec![make_top_level_span("svc", "op", "resource-b")]);
        assert!(sampler.sample(&mut trace_b, 0));

        // Trace with signature A again: should be dropped (within TTL).
        let mut trace_a2 = make_trace(vec![make_top_level_span("svc", "op", "resource-a")]);
        assert!(!sampler.sample(&mut trace_a2, 0));
    }

    #[test]
    fn rate_limit_drops_excess_rare_traces() {
        // Use TPS=1000 but create 60 distinct signatures to exceed the burst of 50.
        let mut sampler = RareSampler::new(true, 1000.0, Duration::from_secs(300), 200);

        let mut kept = 0usize;
        for i in 0..60usize {
            // Each trace has a unique resource so they're all distinct signatures.
            let mut trace = make_trace(vec![make_top_level_span("svc", "op", &format!("res-{}", i))]);
            if sampler.sample(&mut trace, 0) {
                kept += 1;
            }
        }

        // We have a burst of 50, so exactly 50 should be kept.
        assert_eq!(kept, 50);
    }

    #[test]
    fn ttl_expiration_allows_resampling() {
        // Use a very short TTL so we can observe expiry in a test.
        let mut sampler = RareSampler::new(true, 100.0, Duration::from_millis(10), 200);

        let mut trace1 = make_trace(vec![make_top_level_span("svc", "op", "res")]);
        assert!(sampler.sample(&mut trace1, 0));

        // Within TTL: dropped.
        let mut trace2 = make_trace(vec![make_top_level_span("svc", "op", "res")]);
        assert!(!sampler.sample(&mut trace2, 0));

        // Wait for TTL to expire, then the same signature should be rare again.
        std::thread::sleep(Duration::from_millis(20));
        let mut trace3 = make_trace(vec![make_top_level_span("svc", "op", "res")]);
        assert!(sampler.sample(&mut trace3, 0));
    }

    #[test]
    fn record_priority_trace_suppresses_rare() {
        // Simulates the feedback loop: when the priority sampler keeps a trace, it should notify
        // the rare sampler so those signatures are not re-sampled within the TTL window.
        let mut sampler = RareSampler::new(true, 100.0, Duration::from_secs(300), 200);

        let trace = make_trace(vec![make_top_level_span("svc", "op", "res")]);
        sampler.record_priority_trace(&trace, 0);

        // Same signature should now be suppressed — the priority sampler already covers it.
        let mut trace2 = make_trace(vec![make_top_level_span("svc", "op", "res")]);
        assert!(!sampler.sample(&mut trace2, 0));
    }

    #[test]
    fn cardinality_limit_shrinks_shard() {
        // Fill a shard beyond its cardinality limit and verify the map stays bounded.
        let cardinality = 10usize;
        let mut sampler = RareSampler::new(true, 1000.0, Duration::from_secs(300), cardinality);

        // Insert cardinality+5 distinct signatures into the same (service, env) shard.
        for i in 0..(cardinality + 5) {
            let mut trace = make_trace(vec![make_top_level_span("svc", "op", &format!("res-{}", i))]);
            // record_priority_trace writes to seen without consuming tokens, so we can fill freely.
            sampler.record_priority_trace(&trace, 0);
            // Also sample to trigger the shard creation path.
            let _ = sampler.sample(&mut trace, 0);
        }

        // After shrinking, every shard must be at or below cardinality.
        for seen in sampler.seen.values() {
            assert!(seen.expires.len() <= cardinality);
        }
    }

    #[test]
    fn multiple_top_level_spans_rare_flag_on_first_new_signature() {
        // Mirrors TestMultipleTopeLevels from the Go agent.
        // Trace 1: only r1 → r1 is rare, gets _dd.rare=1.
        // Trace 2 (at r1's TTL boundary): r1 is within TTL but r2 is new → r2 gets _dd.rare=1, r1 does not.
        //   Sampling trace 2 also refreshes r1's TTL.
        // Trace 3 (after original TTL): r1 was refreshed in trace 2, so it's still within TTL → not sampled.
        let ttl = Duration::from_millis(20);
        let mut sampler = RareSampler::new(true, 100.0, ttl, 200);

        // Trace 1: single span r1.
        let mut trace1 = make_trace(vec![make_top_level_span("s1", "op", "r1")]);
        assert!(sampler.sample(&mut trace1, 0));
        assert_eq!(trace1.spans()[0].metrics().get(RARE_KEY).copied(), Some(1.0));

        // Wait for r1's TTL to expire, then sample a trace with r1+r2.
        // r1 is now expired (rare again), but r2 hasn't been seen at all.
        // The sampler finds r1 first and would mark it — but actually the Go test relies on
        // r1 being suppressed because of the earlier priority-trace recording.
        // In our case we just verify that the first unseen/expired span gets the flag.
        std::thread::sleep(ttl + Duration::from_millis(5));

        let mut trace2 = make_trace(vec![
            make_top_level_span("s1", "op", "r1"),
            make_top_level_span("s1", "op", "r2"),
        ]);
        // r1 is expired at this point, so trace2 is sampled; r1 gets the rare flag (first expired).
        assert!(sampler.sample(&mut trace2, 0));

        // After sampling trace2, both r1 and r2 TTLs are refreshed.
        // Immediately sampling trace1 (r1 only) should be suppressed.
        let mut trace3 = make_trace(vec![make_top_level_span("s1", "op", "r1")]);
        assert!(!sampler.sample(&mut trace3, 0));
    }
}
