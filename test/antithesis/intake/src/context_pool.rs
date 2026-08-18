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
//!
//! A kind serves an existing context for one of two reasons and the pool tells them apart. Reaching the
//! cap is the configured end state. Spending every mint try on a duplicate is a crowded alphabet, which
//! is counted per kind and reported so a cap that fills slowly is visible. It is never latched: one
//! unlucky streak must not pin a kind below its cap for the rest of the run. A budget too small to hold
//! a kind's smallest identity is neither of those, and errors.
//!
//! One pull in `NON_UTF8_PULL_RATE` leads with a context carrying an invalid UTF-8 byte. A driver pulls
//! once per invocation and leads every datagram it sends with that context, so a carrying pull puts the
//! byte in all of its datagrams and any other pull in none. How many datagrams an invocation sends is
//! sampled independently of what its pull holds, so across a run the fraction of datagrams carrying the
//! byte is the fraction of pulls that do. Deciding it here rather than in the driver is what makes the
//! rate independent of how many contexts a timeline sampled: a driver holding one context gets the same
//! 1% as one holding a thousand. Such a context is a pool member like any other, minted against the same
//! budget, deduplicated, and counted against its kind's cap, so bounded cardinality is unaffected.

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
    metric: KindPool,
    /// Minted event contexts.
    event: KindPool,
    /// Minted service-check contexts.
    service_check: KindPool,
    /// Requests per kind that spent every mint try on a duplicate, by [`Kind`] order. A streak says the
    /// budget's alphabet is crowded against the cap, not that it is spent, so the kind keeps minting.
    collision_streaks: [u64; 3],
    /// Hashes of every context held, so a duplicate mint does not spend a cap slot. Hashes rather
    /// than the contexts themselves, since the pool already holds up to a million of them and a
    /// second copy would double that. A hash collision rejects a distinct identity, which costs one
    /// remint and nothing else.
    seen: HashSet<u64>,
}

impl PoolState {
    /// Mint one context of each sort for every kind, before any pull is served. A pull that owes the
    /// invalid byte must find a context of the right kind carrying it, and a pull that does not must find
    /// one without it, so neither half may be empty once the cap is spent. Every cap is at least two,
    /// which is what makes room for both, and these seeds count against the cap like any other context.
    ///
    /// # Errors
    ///
    /// Returns an error when the budget cannot hold a kind's smallest identity.
    fn seed_halves<R: Rng + ?Sized>(&mut self, caps: ContextSourceConfig, rng: &mut R) -> anyhow::Result<()> {
        let budget = caps.datagram_byte_limit.saturating_sub(1);
        // Minted into locals and committed only once all six succeed. Pushing as it goes would leave a
        // half-seeded pool behind on a failure, and the caller retries, so every retry would stack
        // another partial seeding against the cap.
        let mut seeded = Vec::with_capacity(6);
        for kind in [Kind::Metric, Kind::Event, Kind::ServiceCheck] {
            for non_utf8 in [false, true] {
                let context = if non_utf8 {
                    Context::mint_non_utf8_within(kind, rng, budget)
                } else {
                    Context::mint_within(kind, rng, budget)
                }
                .with_context(|| {
                    format!(
                        "no {kind:?} identity could be minted within the datagram budget {budget}, so this \
                         timeline's context source and datagram limit contradict each other"
                    )
                })?;
                seeded.push((kind, non_utf8, context));
            }
        }
        for (kind, non_utf8, context) in seeded {
            let pool = match kind {
                Kind::Metric => &mut self.metric,
                Kind::Event => &mut self.event,
                Kind::ServiceCheck => &mut self.service_check,
            };
            self.seen.insert(digest(&context));
            pool.half(non_utf8).push(context);
        }
        Ok(())
    }
}

/// One kind's minted contexts, split by whether they carry an invalid UTF-8 byte. The split is storage,
/// not policy: a pull needs to reach one of each without scanning up to a million members.
#[derive(Debug, Default)]
struct KindPool {
    /// Contexts whose every field is valid UTF-8.
    utf8: Vec<Context>,
    /// Contexts carrying an invalid UTF-8 byte in a name or a tag.
    non_utf8: Vec<Context>,
}

impl KindPool {
    /// Contexts held, both halves, which is what a cap counts.
    fn len(&self) -> usize {
        self.utf8.len() + self.non_utf8.len()
    }

    /// Every context held, both halves.
    #[cfg(test)]
    fn iter(&self) -> impl Iterator<Item = &Context> {
        self.utf8.iter().chain(self.non_utf8.iter())
    }

    /// The half a slot of this sort draws from.
    fn half(&mut self, non_utf8: bool) -> &mut Vec<Context> {
        if non_utf8 {
            &mut self.non_utf8
        } else {
            &mut self.utf8
        }
    }
}

/// How many times a duplicate mint is retried before the slot is left for a later request. A small
/// alphabet under a tight budget collides often, and spinning here would stall the serve.
const MINT_TRIES: usize = 8;

/// One pull in this many leads with a context carrying an invalid UTF-8 byte, which makes one datagram in
/// this many carry one.
const NON_UTF8_PULL_RATE: u32 = 100;

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
            state.seed_halves(resolved, rng)?;
            state.caps = Some(resolved);
            resolved
        };

        let mut out = Vec::with_capacity(n);
        let mut served_existing = false;
        // Settled once for the whole pull, and led with, so the byte cannot be lost to a full datagram.
        let non_utf8_pull = rng.random_range(0..NON_UTF8_PULL_RATE) == 0;
        for slot in 0..n {
            let non_utf8 = non_utf8_pull && slot == 0;
            let kind = Kind::sample(rng);
            let PoolState {
                metric,
                event,
                service_check,
                seen,
                collision_streaks,
                ..
            } = &mut *state;
            let (pool, cap, streaks) = match kind {
                Kind::Metric => (metric, caps.metric_contexts, &mut collision_streaks[0]),
                Kind::Event => (event, caps.event_contexts, &mut collision_streaks[1]),
                Kind::ServiceCheck => (service_check, caps.service_check_contexts, &mut collision_streaks[2]),
            };
            // The identity is minted against this timeline's real datagram budget less the newline
            // every packed line costs, so every served context has a rendering the driver can pack.
            // A duplicate identity would spend a cap slot without adding a distinct context, so the
            // kind would stop minting early and the run would explore fewer identities than configured.
            // Remint on a duplicate instead of pushing it.
            let mut minted: Option<Context> = None;
            let budget = caps.datagram_byte_limit.saturating_sub(1);
            if pool.len() < cap {
                for try_index in 0..MINT_TRIES {
                    // The byte is minted into the identity. Editing a rendered datagram instead would
                    // mint an identity the pool never issued and the cap never counted, one per edited
                    // datagram, which is how bounded cardinality leaks.
                    let candidate = if non_utf8 {
                        Context::mint_non_utf8_within(kind, rng, budget)
                    } else {
                        Context::mint_within(kind, rng, budget)
                    };
                    // A mint yields nothing either because the budget cannot hold the kind's smallest
                    // identity, which is a contradiction between this timeline's context source and its
                    // datagram limit, or because its own probe loop ran out of tries on content. Only the
                    // first is a config fault. The second is the same bad luck as a duplicate streak and
                    // gets the same treatment: count it and let the slot recur.
                    let Some(context) = candidate else {
                        anyhow::ensure!(
                            Context::mint_within(kind, rng, budget).is_some(),
                            "datagram budget {budget} cannot hold the smallest {kind:?} identity, so this \
                             timeline's context source and datagram limit contradict each other"
                        );
                        *streaks += 1;
                        break;
                    };
                    if seen.insert(digest(&context)) {
                        minted = Some(context);
                        break;
                    }
                    // Every try collided. That is a crowded alphabet, not a spent one, so the kind stays
                    // free to mint on the next request. Latching it here would let one unlucky streak
                    // pin a kind below its cap for the rest of the run. Counted so a cap that fills
                    // slowly is visible rather than a mystery.
                    if try_index + 1 == MINT_TRIES {
                        *streaks += 1;
                    }
                }
            }
            if let Some(context) = minted {
                pool.half(non_utf8).push(context.clone());
                out.push(context);
            } else {
                // A kind at its cap, or one whose tries all collided, recurs, which is what gives the
                // oracle its curves. Both halves were seeded when the caps resolved, so neither is ever
                // empty and a slot owed the byte is never served a context without it.
                let half = pool.half(non_utf8);
                let context = half
                    .get(rng.random_range(0..half.len().max(1)))
                    .with_context(|| {
                        format!(
                            "{kind:?} pool holds no {} context to serve",
                            if non_utf8 { "non-UTF-8" } else { "UTF-8" }
                        )
                    })?
                    .clone();
                served_existing = true;
                out.push(context);
            }
        }
        let collision_streaks = state.collision_streaks;
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
                "caps": { "metric": caps.metric_contexts, "event": caps.event_contexts, "service_check": caps.service_check_contexts },
                // Requests that spent every mint try on a duplicate. Reported rather than asserted on: a
                // cap near what the budget's alphabet can express collides legitimately, so a run that
                // hits it is not faulty, but a cap that fills slowly has to say why.
                "collision_streaks": { "metric": collision_streaks[0], "event": collision_streaks[1], "service_check": collision_streaks[2] }
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
        temp_config_limited(metric, event, service_check, 8_192)
    }

    /// The same, for a timeline whose datagram limit is `datagram_byte_limit`. A pool mints against that
    /// limit, so a test that packs must use the same one.
    fn temp_config_limited(metric: usize, event: usize, service_check: usize, datagram_byte_limit: usize) -> PathBuf {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "ctxpool-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("create temp config dir");
        let config = ContextSourceConfig {
            datagram_byte_limit,
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

    // A collision streak must not pin a kind below its cap. A crowded alphabet collides often, and a
    // latched flag would stop the kind minting for the rest of the run on one unlucky request.
    #[test]
    fn a_collision_streak_does_not_stop_minting() {
        let mut rng = SmallRng::seed_from_u64(31);
        let pool = Pool::new(temp_config(4_096, 4_096, 4_096));
        for _ in 0..40 {
            pool.serve(64, &mut rng).expect("serve");
        }
        let state = pool.state.lock().expect("lock");
        let streaks: u64 = state.collision_streaks.iter().sum();
        let minted = state.metric.len() + state.event.len() + state.service_check.len();
        assert!(
            minted > 64,
            "the pool stopped minting after {streaks} collision streaks, holding {minted}"
        );
    }

    // The rate the whole design rests on. One pull in a hundred carries a context with an invalid UTF-8
    // byte, and a pull is one driver invocation, so this is the datagram rate too: the harness pins that
    // a carrying pull puts the byte in every datagram it packs and a non-carrying one in none.
    #[test]
    fn one_pull_in_a_hundred_carries_non_utf8() {
        use harness::payload::dogstatsd::Pull;

        let mut rng = SmallRng::seed_from_u64(9);
        let pool = Pool::new(temp_config(1_000_000, 1_000_000, 1_000_000));
        let pulls = 5_000usize;
        let mut carrying = 0usize;
        for _ in 0..pulls {
            let contexts = pool.serve(1, &mut rng).expect("serve");
            if Pull::new(contexts).expect("non-empty").carries_non_utf8() {
                carrying += 1;
            }
        }
        // 0.25% to 1.25%, as integers so the bound carries no rounding of its own.
        assert!(
            carrying * 400 >= pulls && carrying * 80 <= pulls,
            "pulls carrying non-UTF-8 {carrying}/{pulls}, expected ~1%"
        );
    }

    // Once a kind is at its cap, every pull recurs rather than mints, and the half selection in that
    // branch is the only thing that still makes a carrying pull carry. A run spends nearly all of its
    // life there, so the rate has to survive the cap being spent.
    #[test]
    fn the_rate_holds_after_the_caps_fill() {
        use harness::payload::dogstatsd::Pull;

        let mut rng = SmallRng::seed_from_u64(21);
        let pool = Pool::new(temp_config(4, 4, 4));
        // Spend the caps first, so nothing below mints.
        for _ in 0..50 {
            pool.serve(8, &mut rng).expect("serve");
        }
        let pulls = 5_000usize;
        let mut carrying = 0usize;
        for _ in 0..pulls {
            let contexts = pool.serve(1, &mut rng).expect("serve");
            if Pull::new(contexts).expect("non-empty").carries_non_utf8() {
                carrying += 1;
            }
        }
        assert!(
            carrying * 400 >= pulls && carrying * 80 <= pulls,
            "after the caps filled, pulls carrying non-UTF-8 {carrying}/{pulls}, expected ~1%"
        );
        let state = pool.state.lock().expect("lock");
        for held in [&state.metric, &state.event, &state.service_check] {
            assert!(!held.non_utf8.is_empty(), "a kind holds no non-UTF-8 context");
            assert!(!held.utf8.is_empty(), "a kind holds no UTF-8 context");
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
