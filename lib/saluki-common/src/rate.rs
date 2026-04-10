//! Rate limiting primitives.

use std::time::Instant;

/// Token bucket rate limiter.
///
/// Provides a `rate` tokens-per-second refill up to `capacity`, and allows consuming one token at a
/// time via [`allow`][TokenBucket::allow]. Mirrors `golang.org/x/time/rate.Limiter`.
pub struct TokenBucket {
    capacity: f64,
    tokens: f64,
    last_refill: Instant,
    rate: f64,
}

impl TokenBucket {
    /// Creates a new `TokenBucket` with the given rate (tokens per second) and burst capacity.
    ///
    /// The bucket starts full.
    pub fn new(rate: f64, burst: usize) -> Self {
        Self {
            capacity: burst as f64,
            tokens: burst as f64,
            last_refill: Instant::now(),
            rate,
        }
    }

    /// Attempt to consume one token. Returns `true` if a token was available.
    pub fn allow(&mut self) -> bool {
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::TokenBucket;

    #[test]
    fn full_bucket_allows_up_to_burst() {
        let burst = 5;
        let mut bucket = TokenBucket::new(1.0, burst);
        for _ in 0..burst {
            assert!(bucket.allow());
        }
        assert!(!bucket.allow());
    }

    #[test]
    fn empty_bucket_refills_over_time() {
        let mut bucket = TokenBucket::new(100.0, 1);
        assert!(bucket.allow()); // consume the initial token
        assert!(!bucket.allow()); // empty

        std::thread::sleep(Duration::from_millis(20)); // ~2 tokens at 100 TPS
        assert!(bucket.allow());
    }

    #[test]
    fn refill_does_not_exceed_capacity() {
        let burst = 3;
        let mut bucket = TokenBucket::new(1000.0, burst);
        assert!(bucket.allow());
        assert!(bucket.allow());
        assert!(bucket.allow());
        assert!(!bucket.allow());

        std::thread::sleep(Duration::from_millis(50)); // would add 50 tokens at 1000 TPS, capped at burst
        for _ in 0..burst {
            assert!(bucket.allow());
        }
        assert!(!bucket.allow());
    }

    #[test]
    fn zero_rate_never_refills() {
        let mut bucket = TokenBucket::new(0.0, 1);
        assert!(bucket.allow()); // initial token
        assert!(!bucket.allow());
        std::thread::sleep(Duration::from_millis(20));
        assert!(!bucket.allow()); // still empty, no refill
    }
}
