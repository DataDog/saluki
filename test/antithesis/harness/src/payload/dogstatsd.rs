//! `DogStatsD` datagram packing from one pull of pooled contexts.
//!
//! A driver pulls a set of [`Context`]s from the shared intake pool, [`crate::contexts`], once per
//! invocation and reuses it for every datagram it sends. Each datagram is a fresh per-occurrence render
//! of as many of those contexts as fit, and the rest are dropped. Identities recur because the pool is
//! bounded, while their load varies per render. There is no clean/feral/mixed configuration. The legal
//! space is exactly the set of payloads [`crate::dogstatsd::is_malformed`] accepts.
//!
//! Whether a datagram carries a non-UTF-8 byte is settled by the pull, before the driver sees the
//! contexts. The intake puts such a context in a fixed fraction of pulls, and a pull holding one leads
//! every datagram with it, so the fraction of datagrams carrying the byte is the fraction of pulls that
//! do. Nothing here depends on how many contexts a timeline sampled, which is what makes the rate hold
//! for a pull of one as well as a pull of a thousand.
//!
//! ```text
//! metric:        <NAME>:<VALUE>(:<VALUE>)*|<TYPE>[|@<RATE>][|#<TAGS>][|c:..][|e:..][|card:..]
//! event:         _e{<TITLE_LEN>,<TEXT_LEN>}:<TITLE>|<TEXT>[|opt...]|d:<TS>[|#<TAGS>]
//! service check: _sc|<NAME>|<STATUS>[|opt...]|d:<TS>[|#<TAGS>]|m:<MESSAGE>
//! ```

use rand::{Rng, RngExt};

use crate::contexts::Context;

pub(crate) mod common;

/// Ceiling on a generated datagram, the Datadog Agent's default `dogstatsd_buffer_size`. A run caps
/// each datagram to the smaller of this and the SUT's sampled receive buffer, so a packed datagram
/// always fits one read and the SUT never truncates a line mid-token.
pub const DATAGRAM_BYTE_LIMIT: usize = 8_192;

/// What a generated datagram holds, for anchoring assertions.
#[derive(Clone, Copy, Debug, Default)]
pub struct DatagramStats {
    /// Lines packed into the datagram.
    pub lines: usize,
    /// Largest packed multi-value run among those lines. Zero when none.
    pub max_packed: usize,
}

/// One pull of contexts, held for a driver invocation and packed into every datagram it sends.
#[derive(Clone, Debug)]
pub struct Pull {
    /// The contexts as served.
    contexts: Vec<Context>,
    /// The context carrying an invalid UTF-8 byte, if the pull holds one. It leads every datagram, so
    /// the intake's per-pull decision reaches every datagram the invocation sends rather than a
    /// sampling-dependent share of them.
    lead: Option<usize>,
    /// The smallest floor in the pull. Packing stops once the room left is under it, which is what keeps
    /// a pull far larger than a datagram from costing a scan per datagram.
    min_floor: usize,
}

impl Pull {
    /// Take a pull of contexts. Returns `None` for an empty one, which the pool never serves.
    #[must_use]
    pub fn new(contexts: Vec<Context>) -> Option<Self> {
        let min_floor = contexts.iter().map(Context::floor).min()?;
        let lead = contexts.iter().position(Context::has_non_utf8);
        Some(Self {
            contexts,
            lead,
            min_floor,
        })
    }

    /// Whether the pull carries a context bearing an invalid UTF-8 byte.
    #[must_use]
    pub fn carries_non_utf8(&self) -> bool {
        self.lead.is_some()
    }
}

/// Pack one `\n`-terminated line per context into `buf`, within `limit_bytes`, until the room left
/// cannot hold the smallest context in the pull. Each line is a fresh per-occurrence render against the
/// room left, so nothing is built and then thrown away, and a context whose line will not fit is passed
/// over. Clears `buf` first.
///
/// A pull carrying an invalid UTF-8 byte renders that context first, where the whole budget is free, so
/// the byte cannot be lost to a full datagram. The rest are walked from a fresh offset each datagram, so
/// a pull larger than one datagram still puts every context it holds on the wire across the invocation
/// rather than only the ones that happen to sort first.
///
/// # Panics
///
/// Panics when a context whose floor fits the room left produces no line the Agent forwards, and when a
/// pull carrying an invalid UTF-8 byte cannot pack it. Both are generator bugs rather than SUT behaviour,
/// and a quieter datagram or a thinner non-UTF-8 rate would hide them.
pub fn write_datagram(
    rng: &mut (impl Rng + ?Sized), pull: &Pull, buf: &mut Vec<u8>, limit_bytes: usize,
) -> DatagramStats {
    buf.clear();
    let mut stats = DatagramStats::default();
    let mut line = Vec::new();
    if let Some(lead) = pull.lead {
        let context = &pull.contexts[lead];
        // The pull's decision has to reach this datagram. The pool mints every context against the same
        // datagram limit less its newline, so the first line of an empty datagram always has room for it.
        // Skipping it here would drop the run's non-UTF-8 rate below the rate the pool set, silently.
        assert!(
            pack(rng, context, buf, &mut line, limit_bytes, &mut stats),
            "a pull carrying an invalid UTF-8 byte could not pack it into a {limit_bytes}-byte datagram, \
             so the context was minted against a larger limit than it is packed at: {context:?}"
        );
    }
    let count = pull.contexts.len();
    let start = rng.random_range(0..count);
    for step in 0..count {
        let index = (start + step) % count;
        if Some(index) == pull.lead {
            continue;
        }
        // `\n` is part of what a line costs.
        let Some(room) = limit_bytes.checked_sub(buf.len() + 1) else {
            break;
        };
        if room < pull.min_floor {
            break;
        }
        pack(rng, &pull.contexts[index], buf, &mut line, limit_bytes, &mut stats);
    }
    stats
}

/// Render `context` into `buf` when the room left holds it, and count what it packed. Reports whether a
/// line was written.
fn pack(
    rng: &mut (impl Rng + ?Sized), context: &Context, buf: &mut Vec<u8>, line: &mut Vec<u8>, limit_bytes: usize,
    stats: &mut DatagramStats,
) -> bool {
    let Some(room) = limit_bytes.checked_sub(buf.len() + 1) else {
        return false;
    };
    if context.floor() > room {
        return false;
    }
    line.clear();
    // A context whose floor fits renders, always: the repair loop retries the per-occurrence content the
    // Agent would drop, and its last try renders at the floor, where the occurrence is the shortest the
    // identity admits. Passing over the context instead would hide a generator that builds one it cannot
    // render, so this fails where it breaks.
    let Some(packed) = context.render_wellformed_within(rng, line, room) else {
        panic!("context floor fits the {room}-byte room left but no repaired render forwarded: {context:?}")
    };
    buf.extend_from_slice(line);
    buf.push(b'\n');
    stats.lines += 1;
    stats.max_packed = stats.max_packed.max(packed);
    true
}

#[cfg(test)]
mod test {
    use proptest::prelude::*;
    use rand::rngs::SmallRng;
    use rand::SeedableRng;

    use super::{write_datagram, Pull, DATAGRAM_BYTE_LIMIT};
    use crate::contexts::{Context, Kind};
    use crate::dogstatsd::is_malformed;

    /// A pull built against the budget it will be packed at, as the intake builds one. The pool mints
    /// against the timeline's datagram limit less the newline for exactly this reason: a context minted
    /// against a larger budget may not fit a smaller datagram.
    fn pull_of(rng: &mut SmallRng, n: usize, datagram_limit: usize, non_utf8: bool) -> Pull {
        let budget = datagram_limit.saturating_sub(1);
        let contexts = (0..n)
            .filter_map(|slot| {
                let kind = Kind::sample(rng);
                if non_utf8 && slot == 0 {
                    Context::mint_non_utf8_within(kind, rng, budget)
                } else {
                    Context::mint_within(kind, rng, budget)
                }
            })
            .collect();
        Pull::new(contexts).expect("minted no context")
    }

    /// The identity bytes every render of a context puts on the wire, for spotting it in a datagram.
    fn identity_bytes(context: &Context) -> &[u8] {
        match context {
            Context::Metric(c) => &c.name,
            Context::Event(c) => &c.title,
            Context::ServiceCheck(c) => &c.name,
        }
    }

    /// Lines carry no interior newline and each is `\n`-terminated, so the line count equals the
    /// newline count.
    #[allow(clippy::naive_bytecount)]
    fn newline_count(buf: &[u8]) -> usize {
        buf.iter().filter(|&&b| b == b'\n').count()
    }

    // The rate is the pull's, so a pull carrying the byte must put it in EVERY datagram it packs. If it
    // reached only some of them the datagram rate would fall below the pull rate by a factor nobody
    // configured.
    #[test]
    fn a_pull_carrying_non_utf8_puts_it_in_every_datagram() {
        for seed in 0..8u64 {
            for limit_bytes in [128usize, 512, 8_192] {
                let mut rng = SmallRng::seed_from_u64(seed);
                let pull = pull_of(&mut rng, 8, limit_bytes, true);
                assert!(pull.carries_non_utf8(), "the pull was expected to carry the byte");
                let mut buf = Vec::new();
                for _ in 0..200 {
                    write_datagram(&mut rng, &pull, &mut buf, limit_bytes);
                    assert!(
                        simdutf8::basic::from_utf8(&buf).is_err(),
                        "a datagram from a non-UTF-8 pull carried no invalid byte at limit {limit_bytes}"
                    );
                    assert_eq!(is_malformed(&buf), Ok(()), "the datagram was droppable");
                }
            }
        }
    }

    // The converse. A pull without the byte must never manufacture one, or the rate exceeds the pull
    // rate and the intake is no longer the only thing deciding it.
    #[test]
    fn a_pull_without_non_utf8_never_emits_it() {
        for seed in 0..8u64 {
            let mut rng = SmallRng::seed_from_u64(seed);
            let limit_bytes = 1024;
            let pull = pull_of(&mut rng, 8, limit_bytes, false);
            assert!(!pull.carries_non_utf8());
            let mut buf = Vec::new();
            for _ in 0..200 {
                write_datagram(&mut rng, &pull, &mut buf, limit_bytes);
                assert!(
                    simdutf8::basic::from_utf8(&buf).is_ok(),
                    "a datagram from a UTF-8 pull carried an invalid byte"
                );
            }
        }
    }

    // The byte must reach a metric tag as well as a metric name. The v3 intake splits on exactly that
    // axis, coercing non-UTF-8 in the tag dictionary and rejecting the whole payload on the name
    // dictionary, so a byte pinned to one position can only ever drive half of it. Counting any poisoned
    // event or service-check line as the other side would let a handful of those satisfy this while the
    // metric tag path stayed unreachable.
    #[test]
    fn non_utf8_reaches_more_than_the_metric_name() {
        let mut in_name = 0;
        let mut outside_name = 0;
        for seed in 0..32u64 {
            let mut rng = SmallRng::seed_from_u64(seed);
            let pull = pull_of(&mut rng, 8, 512, true);
            let mut buf = Vec::new();
            for _ in 0..50 {
                write_datagram(&mut rng, &pull, &mut buf, 512);
                for line in buf.split(|&b| b == b'\n') {
                    if line.is_empty() || simdutf8::basic::from_utf8(line).is_ok() {
                        continue;
                    }
                    if line.starts_with(b"_e{") || line.starts_with(b"_sc") {
                        continue;
                    }
                    let field0 = line.split(|&b| b == b'|').next().unwrap_or(line);
                    let name = field0.split(|&b| b == b':').next().unwrap_or(field0);
                    if simdutf8::basic::from_utf8(name).is_err() {
                        in_name += 1;
                    } else {
                        outside_name += 1;
                    }
                }
            }
        }
        assert!(in_name > 0, "no non-UTF-8 byte ever landed in a metric name");
        assert!(
            outside_name > 0,
            "every non-UTF-8 byte landed in a metric name, so the tag-dictionary path is unreachable"
        );
    }

    // A pull far larger than a datagram must still put every context it holds on the wire across the
    // invocation. Packing from a fixed end would leave the tail of a thousand-context pull unsent, so the
    // pool would mint identities the SUT never sees.
    #[test]
    fn a_pull_larger_than_a_datagram_still_reaches_every_context() {
        let mut rng = SmallRng::seed_from_u64(13);
        let limit_bytes = 512;
        let pull = pull_of(&mut rng, 64, limit_bytes, false);
        let mut seen = vec![false; pull.contexts.len()];
        let mut buf = Vec::new();
        for _ in 0..2_000 {
            write_datagram(&mut rng, &pull, &mut buf, limit_bytes);
            for (index, context) in pull.contexts.iter().enumerate() {
                let name = identity_bytes(context);
                if !name.is_empty() && buf.windows(name.len()).any(|window| window == name) {
                    seen[index] = true;
                }
            }
        }
        let unseen = seen.iter().filter(|&&hit| !hit).count();
        assert_eq!(
            unseen,
            0,
            "{unseen} of {} contexts never reached a datagram",
            pull.contexts.len()
        );
    }

    proptest! {
        /// Every context the room affords renders a line the Agent forwards. The packer panics rather
        /// than passing over one, so this is the invariant that keeps it from firing.
        #[test]
        fn property_test_an_affordable_identity_always_renders(seed: u64, limit_bytes in 128..=8_192usize) {
            let mut rng = SmallRng::seed_from_u64(seed);
            let pull = pull_of(&mut rng, 8, limit_bytes, seed % 2 == 0);
            for context in &pull.contexts {
                for budget in [context.floor(), limit_bytes - 1, limit_bytes / 2] {
                    if context.floor() > budget {
                        continue;
                    }
                    let mut line = Vec::new();
                    let rendered = context.render_wellformed_within(&mut rng, &mut line, budget);
                    prop_assert!(rendered.is_some(), "affordable identity never forwarded: {context:?}");
                }
            }
        }

        /// A rendered metric carries exactly the tag set its identity holds. The Agent assigns the tag
        /// set from every `#`-prefixed optional field it sees, last one winning, so a second such field
        /// would swap the identity's tags for whatever an occurrence body happened to contain. That
        /// would make one pooled identity render as several, putting cardinality past its cap and one
        /// point in each of many series. Occurrence bodies do carry `|` and `#`, but segments are
        /// always joined by a separator, so the two never land adjacent. This pins that.
        #[test]
        fn property_test_metric_carries_one_tag_field(seed: u64) {
            let mut rng = SmallRng::seed_from_u64(seed);
            let pull = pull_of(&mut rng, 8, DATAGRAM_BYTE_LIMIT, seed % 2 == 0);
            let mut buf = Vec::new();
            for _ in 0..4 {
                write_datagram(&mut rng, &pull, &mut buf, DATAGRAM_BYTE_LIMIT);
                for line in buf.split(|&b| b == b'\n') {
                    if line.is_empty() || line.starts_with(b"_e{") || line.starts_with(b"_sc") {
                        continue;
                    }
                    // Field 0 is `name:value` and field 1 the type. The Agent reads a tag set only from
                    // the optional fields after those, so a `#` opening either of the first two is not
                    // one.
                    let tag_fields = line
                        .split(|&b| b == b'|')
                        .skip(2)
                        .filter(|field| field.first() == Some(&b'#'))
                        .count();
                    prop_assert!(
                        tag_fields <= 1,
                        "{tag_fields} tag fields in {:?}",
                        String::from_utf8_lossy(line)
                    );
                }
            }
        }

        /// Every datagram the driver packs from a pull is one the Agent forwards.
        #[test]
        fn property_test_every_payload_is_well_formed(seed: u64) {
            let mut rng = SmallRng::seed_from_u64(seed);
            let pull = pull_of(&mut rng, 8, DATAGRAM_BYTE_LIMIT, seed % 2 == 0);
            let mut buf = Vec::new();
            for _ in 0..8 {
                write_datagram(&mut rng, &pull, &mut buf, DATAGRAM_BYTE_LIMIT);
                prop_assert_eq!(is_malformed(&buf), Ok(()), "emitted a droppable datagram: {:?}", String::from_utf8_lossy(&buf));
            }
        }

        /// A pull whose smallest context fits yields load. An empty datagram spends one of the driver's
        /// configured sends without reaching the SUT.
        #[test]
        fn property_test_payload_is_never_empty_when_a_context_fits(seed: u64, limit_bytes in 128..=8_192usize) {
            let mut rng = SmallRng::seed_from_u64(seed);
            let pull = pull_of(&mut rng, 8, limit_bytes, seed % 2 == 0);
            let mut buf = Vec::new();
            let stats = write_datagram(&mut rng, &pull, &mut buf, limit_bytes);

            let smallest = pull.min_floor;
            if limit_bytes > smallest {
                prop_assert!(!buf.is_empty(), "empty datagram at limit {limit_bytes}, smallest floor {smallest}");
                prop_assert!(stats.lines > 0);
            }
        }

        #[test]
        fn property_test_payload_stays_within_its_limit(seed: u64, limit_bytes in 128..=8_192usize) {
            let mut rng = SmallRng::seed_from_u64(seed);
            let pull = pull_of(&mut rng, 8, limit_bytes, seed % 2 == 0);
            let mut buf = Vec::new();
            let stats = write_datagram(&mut rng, &pull, &mut buf, limit_bytes);

            prop_assert!(buf.len() <= limit_bytes);
            prop_assert_eq!(newline_count(&buf), stats.lines);
            if !buf.is_empty() {
                prop_assert_eq!(buf[buf.len() - 1], b'\n');
            }
        }
    }
}
