//! The context pool: a bounded, lazily-filled set of `DogStatsD` contexts the
//! differential drivers draw from so contexts recur across flushes.
//!
//! Every driver invocation is a fresh process; without coordination each would
//! mint its own contexts and the space would grow without bound and never recur.
//! The pool holds one shared set behind a hard cap `T`: it mints a new context per
//! requested slot until the set holds `T`, then draws an existing one at random.
//! Once cumulative requests exceed `T` the set is exhausted and contexts recur
//! across flushes, which is what gives the differential oracle multi-point curves
//! to align.
//!
//! The cap is read from `context_source.yaml` on the first [`Pool::serve`] call.
//! The intake starts before `first_sample_config` samples that file (see the boot
//! note in the intake binary), but the first `/contexts` request only ever comes
//! from a driver, which runs after `first_sample_config` — so the config is always
//! present by then and the pool never serves without it.

use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};

use antithesis_sdk::prelude::*;
use anyhow::Context as _;
use harness::config::ContextSourceConfig;
use harness::context::Context;
use rand::{Rng, RngExt};
use serde_json::json;

/// A bounded, lazily-filled pool of contexts.
#[derive(Debug)]
pub struct Pool {
    /// Directory holding `context_source.yaml`, read once on the first serve.
    config_dir: PathBuf,
    /// The cap (once resolved) and the minted contexts, behind one lock.
    state: Mutex<PoolState>,
}

/// The pool's mutable state: the resolved cap and the minted contexts.
#[derive(Debug, Default)]
struct PoolState {
    /// The hard cap `T`, resolved from the config on the first serve.
    cap: Option<usize>,
    /// The minted contexts, grown to `cap` then drawn from.
    contexts: Vec<Context>,
}

impl Pool {
    /// A pool that resolves its cap from `context_source.yaml` in `config_dir` on
    /// the first serve.
    #[must_use]
    pub fn new(config_dir: PathBuf) -> Self {
        Self {
            config_dir,
            state: Mutex::new(PoolState::default()),
        }
    }

    /// Serve `n` contexts: mint a fresh one while under the cap, else draw an
    /// existing one at random. The whole operation holds the lock, so concurrent
    /// requests never race on `rng` or overshoot the cap.
    ///
    /// # Errors
    ///
    /// Returns an error if `context_source.yaml` cannot be read on the first call.
    pub fn serve<R: Rng + ?Sized>(&self, n: usize, rng: &mut R) -> anyhow::Result<Vec<Context>> {
        // A poisoned lock still holds a valid pool; recover it rather than panic.
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);

        let cap = if let Some(cap) = state.cap {
            cap
        } else {
            let resolved = ContextSourceConfig::read(&self.config_dir)
                .context("read context source config for the pool cap")?
                .context_cap;
            state.cap = Some(resolved);
            resolved
        };

        let mut out = Vec::with_capacity(n);
        let mut served_existing = false;
        for _ in 0..n {
            if state.contexts.len() < cap {
                let context = Context::mint(rng);
                state.contexts.push(context.clone());
                out.push(context);
            } else {
                let idx = rng.random_range(0..state.contexts.len());
                if let Some(context) = state.contexts.get(idx) {
                    served_existing = true;
                    out.push(context.clone());
                }
            }
        }
        let len = state.contexts.len();
        drop(state);

        assert_always!(
            len <= cap,
            "context pool never exceeds its cap",
            &json!({ "len": len, "cap": cap })
        );
        assert_sometimes!(
            served_existing,
            "context source served an existing context",
            &json!({ "len": len, "cap": cap })
        );
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use harness::config::ContextSourceConfig;
    use harness::context::{decode_response, encode_response};
    use rand::rngs::SmallRng;
    use rand::SeedableRng;

    use super::Pool;

    /// Write a `context_source.yaml` with `cap` into a fresh temp dir and return
    /// it, so tests exercise the real cap-read path.
    fn temp_config(cap: usize) -> PathBuf {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!("ctxpool-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&dir).expect("create temp config dir");
        let yaml = ContextSourceConfig { context_cap: cap }.to_yaml().expect("render config");
        std::fs::write(dir.join("context_source.yaml"), yaml).expect("write config");
        dir
    }

    #[test]
    fn fills_to_cap_then_repeats() {
        let mut rng = SmallRng::seed_from_u64(0);
        let pool = Pool::new(temp_config(4));

        let mut seen = BTreeSet::new();
        let mut total = 0;
        for _ in 0..10 {
            let batch = pool.serve(5, &mut rng).expect("serve");
            total += batch.len();
            for context in batch {
                let mut wire = Vec::new();
                context.encode(&mut wire);
                seen.insert(wire);
            }
        }

        assert!(seen.len() <= 4, "distinct {} exceeds cap 4", seen.len());
        assert!(total > seen.len(), "expected repeats: {total} served, {} distinct", seen.len());
    }

    #[test]
    fn serves_exactly_n_and_round_trips_the_wire() {
        let mut rng = SmallRng::seed_from_u64(7);
        let pool = Pool::new(temp_config(1_000));
        let contexts = pool.serve(9, &mut rng).expect("serve");
        assert_eq!(contexts.len(), 9);

        let wire = encode_response(&contexts);
        assert_eq!(decode_response(&wire).as_deref(), Some(contexts.as_slice()));
    }

    #[test]
    fn cap_of_one_serves_a_single_context_forever() {
        let mut rng = SmallRng::seed_from_u64(3);
        let pool = Pool::new(temp_config(1));
        let first = pool.serve(1, &mut rng).expect("serve");
        for _ in 0..20 {
            assert_eq!(pool.serve(1, &mut rng).expect("serve"), first);
        }
    }

    #[test]
    fn missing_config_is_an_error() {
        let mut rng = SmallRng::seed_from_u64(1);
        let pool = Pool::new(std::env::temp_dir().join("ctxpool-does-not-exist"));
        assert!(pool.serve(1, &mut rng).is_err());
    }
}
