//! Rare sampler for V1 trace chunks.
//!
//! Mirrors the logic of the OTLP-path `RareSampler` but operates directly on
//! `V1TraceChunk` and `V1Span` types rather than the legacy `Trace`/`Span` types.

use std::time::{Duration, Instant};

use saluki_common::{collections::FastHashMap, rate::TokenBucket};
use saluki_core::data_model::event::trace::Span;

const RARE_SAMPLER_BURST: usize = 50;
const TTL_RENEWAL_PERIOD: Duration = Duration::from_secs(60);

// FNV-1a 32-bit constants.
const OFFSET_32: u32 = 2166136261;
const PRIME_32: u32 = 16777619;

fn write_hash(mut hash: u32, bytes: &[u8]) -> u32 {
    for &b in bytes {
        hash ^= b as u32;
        hash = hash.wrapping_mul(PRIME_32);
    }
    hash
}

/// Compute FNV-1a 32-bit hash of a span's (service, name, resource, error) tuple.
fn span_hash(span: &Span) -> u32 {
    let mut h = OFFSET_32;
    h = write_hash(h, span.service().as_bytes());
    h = write_hash(h, span.name().as_bytes());
    h = write_hash(h, span.resource().as_bytes());
    h = write_hash(h, &[u8::from(span.error() != 0)]);
    h
}

/// Compute a shard key for a span based on its service name.
fn shard_key(span: &Span) -> u32 {
    write_hash(OFFSET_32, span.service().as_bytes())
}

/// Tracks span signatures seen within a shard, with per-signature TTL expiry.
struct SeenSpans {
    expires: FastHashMap<u32, Instant>,
    shrunk: bool,
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

    fn sign(&self, span_hash: u32) -> u32 {
        if self.shrunk {
            span_hash % self.cardinality as u32
        } else {
            span_hash
        }
    }

    fn add(&mut self, now: Instant, expire: Instant, span_hash: u32) {
        let sig = self.sign(span_hash);
        if let Some(&stored) = self.expires.get(&sig) {
            if stored > now && expire.duration_since(stored) < TTL_RENEWAL_PERIOD {
                return;
            }
        }
        self.expires.insert(sig, expire);
        if self.expires.len() > self.cardinality {
            self.shrink();
        }
    }

    fn get_expire(&self, sig: u32) -> Option<&Instant> {
        self.expires.get(&sig)
    }

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

/// Rare sampler for V1 trace chunks.
///
/// Keeps chunks whose span signatures haven't been seen within the cooldown TTL and whose
/// rate stays below the token-bucket limit.
pub(super) struct V1RareSampler {
    enabled: bool,
    token_bucket: TokenBucket,
    ttl: Duration,
    cardinality: usize,
    seen: FastHashMap<u32, SeenSpans>,
}

impl V1RareSampler {
    pub(super) fn new(enabled: bool, tps: f64, ttl: Duration, cardinality: usize) -> Self {
        Self {
            enabled,
            token_bucket: TokenBucket::new(tps, RARE_SAMPLER_BURST),
            ttl,
            cardinality,
            seen: FastHashMap::default(),
        }
    }

    /// Returns `true` if the spans should be kept by the rare sampler.
    pub(super) fn sample(&mut self, spans: &[Span]) -> bool {
        if !self.enabled {
            return false;
        }

        let now = Instant::now();
        let expire = now + self.ttl;

        let found_rare = spans.iter().any(|span| {
            let key = shard_key(span);
            let hash = span_hash(span);
            let seen = self.seen.entry(key).or_insert_with(|| SeenSpans::new(self.cardinality));
            let sig = seen.sign(hash);
            seen.get_expire(sig).is_none_or(|e| now > *e)
        });

        if !found_rare || !self.token_bucket.allow() {
            return false;
        }

        for span in spans {
            let key = shard_key(span);
            let hash = span_hash(span);
            let seen = self.seen.entry(key).or_insert_with(|| SeenSpans::new(self.cardinality));
            seen.add(now, expire, hash);
        }

        true
    }
}
