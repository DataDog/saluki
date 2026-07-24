//! The context pool: per-kind bounded, lazily-filled sets the differential drivers draw from so
//! contexts recur across flushes.
//!
//! Every driver invocation is a fresh process; without coordination each would mint its own contexts
//! and the space would grow without bound and never recur. The pool holds one shared set per kind
//! behind a hard per-kind cap: it mints a new context of a sampled kind while that kind is under its
//! cap, then draws an existing one of that kind at random. Once a kind's cumulative requests exceed
//! its cap the set is exhausted and its contexts recur across flushes, which is what gives the
//! differential oracle multi-point curves to align.
//!
//! The caps are read from `context_source.yaml` on the first [`Pool::serve`] call. The intake starts
//! before `first_sample_config` samples that file, but the first `/contexts` request only ever comes
//! from a driver, which runs after `first_sample_config` — so the config is present by then.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};

use antithesis_sdk::prelude::*;
use anyhow::Context as _;
use harness::config::ContextSourceConfig;
use harness::contexts::{Context, Kind};
use rand::{Rng, RngExt};
use serde_json::json;

/// A per-kind bounded, lazily-filled pool of contexts.
#[derive(Debug)]
pub struct Pool {
    /// Directory holding `context_source.yaml`, read once on the first serve.
    config_dir: PathBuf,
    /// The resolved caps and the minted contexts, behind one lock.
    state: Mutex<PoolState>,
}

/// The pool's mutable state: the resolved per-kind caps and the minted contexts.
#[derive(Debug, Default)]
struct PoolState {
    /// The per-kind caps, resolved from the config on the first serve.
    caps: Option<ContextSourceConfig>,
    /// Minted metric contexts, grown to the metric cap then drawn from.
    metric: Vec<Context>,
    /// Minted event contexts.
    event: Vec<Context>,
    /// Minted service-check contexts.
    service_check: Vec<Context>,
    /// Hashes of every context held, so a duplicate mint does not spend a cap slot. Hashes rather
    /// than the contexts themselves, since the pool already holds up to a million of them and a
    /// second copy would double that. A hash collision rejects a distinct identity, which costs one
    /// remint and nothing else.
    seen: HashSet<u64>,
}

/// How many times a duplicate mint is retried before the slot is left for a later request. A small
/// alphabet under a tight budget collides often, and spinning here would stall the serve.
const MINT_TRIES: usize = 8;

/// The hash a pooled identity is deduplicated by.
fn digest(context: &Context) -> u64 {
    let mut hasher = DefaultHasher::new();
    context.hash(&mut hasher);
    hasher.finish()
}

impl Pool {
    /// A pool that resolves its caps from `context_source.yaml` in `config_dir` on the first serve.
    #[must_use]
    pub fn new(config_dir: PathBuf) -> Self {
        Self {
            config_dir,
            state: Mutex::new(PoolState::default()),
        }
    }

    /// Serve `n` contexts: for each slot sample a kind, mint a fresh one while that kind is under its
    /// cap, else draw an existing one of that kind. The whole operation holds the lock, so concurrent
    /// requests never race on `rng` or overshoot a cap.
    ///
    /// # Errors
    ///
    /// Returns an error if `context_source.yaml` cannot be read on the first call.
    pub fn serve<R: Rng + ?Sized>(&self, n: usize, rng: &mut R) -> anyhow::Result<Vec<Context>> {
        // A poisoned lock still holds a valid pool; recover it rather than panic.
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);

        let caps = if let Some(caps) = state.caps {
            caps
        } else {
            let resolved =
                ContextSourceConfig::read(&self.config_dir).context("read context source config for the pool caps")?;
            state.caps = Some(resolved);
            resolved
        };

        let mut out = Vec::with_capacity(n);
        let mut served_existing = false;
        for _ in 0..n {
            let kind = Kind::sample(rng);
            let PoolState {
                metric,
                event,
                service_check,
                seen,
                ..
            } = &mut *state;
            let (pool, cap) = match kind {
                Kind::Metric => (metric, caps.metric_contexts),
                Kind::Event => (event, caps.event_contexts),
                Kind::ServiceCheck => (service_check, caps.service_check_contexts),
            };
            // The identity is minted against this timeline's real datagram budget, so every served
            // context has a rendering the driver can pack. A budget too small for this kind, or an
            // alphabet that keeps landing on the drop side, yields nothing rather than a context of
            // another kind, so a kind's working set never fills with identities it did not ask for.
            // A duplicate identity would spend a cap slot without adding a distinct context, so the
            // kind would stop minting early and the run would explore fewer identities than configured.
            // Remint on a duplicate instead of pushing it.
            let mut minted = None;
            if pool.len() < cap {
                for _ in 0..MINT_TRIES {
                    let Some(context) = Context::mint_within(kind, rng, caps.payload_byte_limit.saturating_sub(1))
                    else {
                        break;
                    };
                    if seen.insert(digest(&context)) {
                        minted = Some(context);
                        break;
                    }
                }
            }
            if let Some(context) = minted {
                pool.push(context.clone());
                out.push(context);
            } else if !pool.is_empty() {
                let context = pool[rng.random_range(0..pool.len())].clone();
                served_existing = true;
                out.push(context);
            }
        }
        let metric = state.metric.len();
        let event = state.event.len();
        let service_check = state.service_check.len();
        drop(state);

        assert_always!(
            metric <= caps.metric_contexts
                && event <= caps.event_contexts
                && service_check <= caps.service_check_contexts,
            "context pool never exceeds its per-kind caps",
            &json!({
                "metric": metric,
                "event": event,
                "service_check": service_check,
                "caps": { "metric": caps.metric_contexts, "event": caps.event_contexts, "service_check": caps.service_check_contexts }
            })
        );
        assert_sometimes!(served_existing, "context source served an existing context", &json!({}));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use harness::config::ContextSourceConfig;
    use harness::contexts::{decode_response, encode_response, Context};
    use rand::rngs::SmallRng;
    use rand::SeedableRng;

    use super::Pool;

    /// Write a `context_source.yaml` with the given per-kind caps into a fresh temp dir.
    fn temp_config(metric: usize, event: usize, service_check: usize) -> PathBuf {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "ctxpool-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("create temp config dir");
        let config = ContextSourceConfig {
            payload_byte_limit: 8_192,
            metric_contexts: metric,
            event_contexts: event,
            service_check_contexts: service_check,
        };
        std::fs::write(
            dir.join("context_source.yaml"),
            config.to_yaml().expect("render config"),
        )
        .expect("write config");
        dir
    }

    #[test]
    fn fills_to_caps_then_repeats() {
        let mut rng = SmallRng::seed_from_u64(0);
        // Small metric cap so metric contexts recur within the run.
        let pool = Pool::new(temp_config(4, 1_000, 1_000));

        let mut metric_wire = BTreeSet::new();
        let mut metric_total = 0;
        for _ in 0..50 {
            for context in pool.serve(5, &mut rng).expect("serve") {
                if let Context::Metric(_) = context {
                    let mut wire = Vec::new();
                    context.encode(&mut wire);
                    metric_wire.insert(wire);
                    metric_total += 1;
                }
            }
        }
        assert!(
            metric_wire.len() <= 4,
            "distinct metric {} exceeds cap 4",
            metric_wire.len()
        );
        assert!(metric_total > metric_wire.len(), "expected metric repeats");
    }

    // A cap counts distinct identities. A duplicate mint that consumed a slot would stop the kind
    // minting early and leave the run exploring fewer identities than configured.
    #[test]
    fn pooled_contexts_are_distinct() {
        let mut rng = SmallRng::seed_from_u64(11);
        let pool = Pool::new(temp_config(64, 64, 64));
        let mut all = Vec::new();
        for _ in 0..16 {
            all.extend(pool.serve(32, &mut rng).expect("serve"));
        }
        let state = pool.state.lock().expect("lock");
        for held in [&state.metric, &state.event, &state.service_check] {
            let distinct: HashSet<&Context> = held.iter().collect();
            assert_eq!(distinct.len(), held.len(), "a kind holds a duplicate identity");
        }
    }

    #[test]
    fn serves_exactly_n_and_round_trips_the_wire() {
        let mut rng = SmallRng::seed_from_u64(7);
        let pool = Pool::new(temp_config(1_000, 1_000, 1_000));
        let contexts = pool.serve(9, &mut rng).expect("serve");
        assert_eq!(contexts.len(), 9);

        let wire = encode_response(&contexts);
        let decoded = decode_response(&wire);
        assert_eq!(decoded.as_deref(), Some(contexts.as_slice()));
    }

    #[test]
    fn missing_config_is_an_error() {
        let mut rng = SmallRng::seed_from_u64(1);
        let pool = Pool::new(std::env::temp_dir().join("ctxpool-does-not-exist"));
        assert!(pool.serve(1, &mut rng).is_err());
    }
}
