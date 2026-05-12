//! Shared sampling-rate state between the APM receiver source and the V1 trace sampler.

use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use saluki_common::collections::FastHashMap;

/// Per-service sampling rates computed by the V1 priority sampler.
struct V1SamplingRates {
    /// Map from `"service:<name>,env:<env>"` to a rate in `[0.0, 1.0]`.
    rates: FastHashMap<String, f64>,
    /// Opaque version token in the form `"<unix_hex>-<counter_hex>"`.
    ///
    /// Mirrors the Go Trace Agent's `newVersion()`:
    /// `strconv.FormatInt(time.Now().Unix(), 16) + "-" + strconv.FormatInt(localVersion.Inc(), 16)`
    ///
    /// The timestamp prefix makes the token time-anchored and opaque to clients; the
    /// counter suffix ensures uniqueness within the same second.
    version: String,
    /// Monotonic counter incremented on each `set_all` call.
    generation: u64,
}

impl Default for V1SamplingRates {
    fn default() -> Self {
        Self {
            rates: FastHashMap::default(),
            version: String::new(),
            generation: 0,
        }
    }
}

impl V1SamplingRates {
    fn set_all(&mut self, new_rates: FastHashMap<String, f64>) {
        if new_rates == self.rates {
            return;
        }
        self.rates = new_rates;
        self.generation = self.generation.wrapping_add(1);
        self.version = new_version(self.generation);
    }
}

/// Builds a version token matching the Go agent's `newVersion()`.
///
/// Format: `"<unix_secs_hex>-<generation_hex>"`, e.g. `"67f4a2b1-3"`.
fn new_version(generation: u64) -> String {
    let unix_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{:x}-{:x}", unix_secs, generation)
}

/// Response produced by [`V1SamplingRatesHandle::get_response`].
pub enum RateResponse {
    /// Rates are unchanged since the client's last-known version.
    /// Respond with `{}` and set the version header.
    Unchanged {
        /// Current version token.
        version: String,
    },
    /// Rates have been updated.
    /// Respond with the full `{"rate_by_service": {...}}` payload.
    Updated {
        /// Current per-service rates.
        rates: FastHashMap<String, f64>,
        /// Current version token.
        version: String,
    },
}

/// Cheap-clone handle to the shared APM priority-sampler rate table.
///
/// The [`crate::transforms::V1TraceSamplerConfiguration`] holds one clone (writer).
/// The APM receiver source holds another (reader). Cloning is O(1) — just an Arc
/// refcount increment.
#[derive(Clone)]
pub struct V1SamplingRatesHandle {
    inner: Arc<RwLock<V1SamplingRates>>,
}

impl V1SamplingRatesHandle {
    /// Creates a new handle backed by an empty rate table.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(V1SamplingRates::default())),
        }
    }

    /// Replaces the current rate table with `new_rates`.
    ///
    /// Called by the V1 trace sampler transform whenever the core sampler's
    /// sliding window advances and produces new per-service rates.
    pub fn set_all(&self, new_rates: FastHashMap<String, f64>) {
        // Recover from lock poisoning consistently with the read side — if another
        // thread panicked holding the lock, the data inside is still valid to update.
        let mut guard = self.inner.write().unwrap_or_else(|e| e.into_inner());
        guard.set_all(new_rates);
    }

    /// Returns the appropriate response for a tracer's `/v1.0/traces` request.
    ///
    /// `client_version` is the value of the `Datadog-Rates-Payload-Version` request
    /// header, or an empty string if the header was absent.
    pub fn get_response(&self, client_version: &str) -> RateResponse {
        let guard = self.inner.read().unwrap_or_else(|e| e.into_inner());
        let current_version = guard.version.clone();
        // An empty version means no rates have been computed yet — always send Updated
        // so the tracer gets an explicit empty map rather than a stale "unchanged" reply.
        // This matches the Go agent's treatment of version="" as a "no rates" sentinel.
        let version_matches = !current_version.is_empty()
            && !client_version.is_empty()
            && client_version == current_version;
        if version_matches {
            RateResponse::Unchanged { version: current_version }
        } else {
            RateResponse::Updated {
                rates: guard.rates.clone(),
                version: current_version,
            }
        }
    }
}

impl Default for V1SamplingRatesHandle {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_rates(pairs: &[(&str, f64)]) -> FastHashMap<String, f64> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn version_empty_on_new() {
        let handle = V1SamplingRatesHandle::new();
        assert!(handle.inner.read().unwrap().version.is_empty());
    }

    #[test]
    fn version_changes_when_rates_change() {
        let handle = V1SamplingRatesHandle::new();
        handle.set_all(make_rates(&[("service:foo,env:prod", 0.5)]));
        let v1 = handle.inner.read().unwrap().version.clone();
        // Let at least 1 µs pass so the timestamp portion can't collide.
        std::thread::sleep(std::time::Duration::from_millis(1));
        handle.set_all(make_rates(&[("service:foo,env:prod", 0.3)]));
        let v2 = handle.inner.read().unwrap().version.clone();
        assert_ne!(v1, v2, "version must change when rates change");
    }

    #[test]
    fn version_unchanged_when_rates_unchanged() {
        let handle = V1SamplingRatesHandle::new();
        handle.set_all(make_rates(&[("service:foo,env:prod", 0.5)]));
        let v1 = handle.inner.read().unwrap().version.clone();
        handle.set_all(make_rates(&[("service:foo,env:prod", 0.5)]));
        let v2 = handle.inner.read().unwrap().version.clone();
        assert_eq!(v1, v2, "version must not change when rates are identical");
    }

    #[test]
    fn version_format_matches_go_agent() {
        // Expected: "<hex_unix_secs>-<hex_generation>", e.g. "67f4a2b1-1"
        let handle = V1SamplingRatesHandle::new();
        handle.set_all(make_rates(&[("service:,env:", 1.0)]));
        let version = handle.inner.read().unwrap().version.clone();
        let parts: Vec<&str> = version.splitn(2, '-').collect();
        assert_eq!(parts.len(), 2, "version must contain exactly one '-'");
        u64::from_str_radix(parts[0], 16).expect("timestamp part must be hex");
        u64::from_str_radix(parts[1], 16).expect("counter part must be hex");
    }

    #[test]
    fn unchanged_response_when_version_matches() {
        let handle = V1SamplingRatesHandle::new();
        handle.set_all(make_rates(&[("service:foo,env:prod", 0.5)]));
        let current_version = handle.inner.read().unwrap().version.clone();

        let response = handle.get_response(&current_version);
        assert!(matches!(response, RateResponse::Unchanged { .. }));
    }

    #[test]
    fn updated_response_when_version_differs() {
        let handle = V1SamplingRatesHandle::new();
        handle.set_all(make_rates(&[("service:foo,env:prod", 0.5)]));

        let response = handle.get_response("stale-version");
        match response {
            RateResponse::Updated { rates, .. } => {
                assert_eq!(rates.get("service:foo,env:prod"), Some(&0.5));
            }
            _ => panic!("expected Updated response"),
        }
    }

    #[test]
    fn empty_client_version_always_gets_updated() {
        let handle = V1SamplingRatesHandle::new();
        handle.set_all(make_rates(&[("service:,env:", 1.0)]));
        let response = handle.get_response("");
        assert!(matches!(response, RateResponse::Updated { .. }));
    }

    #[test]
    fn updated_response_before_any_set_all() {
        // Before the sampler calls set_all, version is empty. A tracer that also
        // has an empty version should still receive Updated (not Unchanged), matching
        // the Go agent's treatment of version="" as "no rates computed yet".
        let handle = V1SamplingRatesHandle::new();
        let response = handle.get_response("");
        assert!(
            matches!(response, RateResponse::Updated { .. }),
            "should return Updated before any set_all even when client version is also empty"
        );
    }
}
