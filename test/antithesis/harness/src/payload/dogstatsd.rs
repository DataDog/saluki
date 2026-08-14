//! `DogStatsD` payload generation from a pooled context working set.
//!
//! A driver no longer samples a fresh identity per line. It fetches a working set of [`Context`]s
//! from the shared intake pool, [`crate::contexts`], and renders a fresh per-occurrence payload
//! against a sampled context each line, so identities recur while their load varies. Every rendered
//! line is repaired by [`Context::render_wellformed_within`] to one the Datadog Agent forwards, so a packed
//! datagram holds only whole lines the Agent keeps. There is no clean/feral/mixed configuration. The
//! legal space is exactly the set of payloads [`crate::dogstatsd::is_malformed`] accepts.
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
pub const PAYLOAD_BYTE_LIMIT: usize = 8_192;

/// What a generated payload holds, for anchoring assertions.
#[derive(Clone, Copy, Debug, Default)]
pub struct Payload {
    /// Lines packed into the buffer.
    pub lines: usize,
    /// Largest packed multi-value run among those lines. Zero when none.
    pub max_packed: usize,
}

/// How many affordable contexts a line tries before the datagram ends. A context whose occurrence
/// payloads keep landing on the drop side is one to move past, not a reason to stop packing.
const RENDER_PICKS: usize = 4;

/// A driver's working set, ordered by how many bytes each identity forces on every render.
///
/// The order is computed once per fetch. Packing a line then finds the affordable contexts by
/// partition point rather than filtering the whole set, which matters because one invocation packs
/// thousands of datagrams from a set of up to a thousand identities.
#[derive(Clone, Debug)]
pub struct WorkingSet {
    /// Contexts by ascending floor.
    contexts: Vec<Context>,
    /// `contexts[i].floor()`, same order, so the affordable contexts are a prefix.
    floors: Vec<usize>,
}

impl WorkingSet {
    /// Order `contexts` by floor and record each one.
    #[must_use]
    pub fn new(mut contexts: Vec<Context>) -> Self {
        contexts.sort_by_key(Context::floor);
        let floors = contexts.iter().map(Context::floor).collect();
        Self { contexts, floors }
    }

    /// Whether the set holds nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.contexts.is_empty()
    }

    /// The smallest floor in the set, or `None` when the set is empty.
    #[must_use]
    pub fn smallest_floor(&self) -> Option<usize> {
        self.floors.first().copied()
    }

    /// Pick uniformly among the contexts whose identity fits `budget`, or `None` when none does.
    fn pick(&self, rng: &mut (impl Rng + ?Sized), budget: usize) -> Option<&Context> {
        let affordable = self.floors.partition_point(|&floor| floor <= budget);
        (affordable > 0).then(|| &self.contexts[rng.random_range(0..affordable)])
    }
}

/// Pack whole `\n`-terminated lines into `buf`, each a fresh render of a context that fits the room
/// left under `limit_bytes`. A context is picked from those the remaining budget can hold and rendered
/// against that budget, so the datagram fills instead of ending on the first oversized render and
/// nothing is built and thrown away. A context whose occurrence payloads all land on the drop side is
/// passed over for another, up to [`RENDER_PICKS`], rather than ending the datagram. Clears `buf`
/// first. An empty working set, or a budget too small for any identity, yields an empty payload.
pub fn write_payload(
    rng: &mut (impl Rng + ?Sized), contexts: &WorkingSet, buf: &mut Vec<u8>, limit_bytes: usize,
) -> Payload {
    buf.clear();
    let mut payload = Payload::default();
    let mut line = Vec::new();
    loop {
        // `\n` is part of what a line costs.
        let Some(budget) = limit_bytes.checked_sub(buf.len() + 1) else {
            break;
        };
        let mut rendered = None;
        for _ in 0..RENDER_PICKS {
            let Some(context) = contexts.pick(rng, budget) else {
                break;
            };
            line.clear();
            if let Some(packed) = context.render_wellformed_within(rng, &mut line, budget) {
                rendered = Some(packed);
                break;
            }
        }
        let Some(packed) = rendered else {
            break;
        };
        buf.extend_from_slice(&line);
        buf.push(b'\n');
        payload.lines += 1;
        payload.max_packed = payload.max_packed.max(packed);
    }
    payload
}

#[cfg(test)]
mod test {
    use proptest::prelude::*;
    use rand::rngs::SmallRng;
    use rand::SeedableRng;

    use super::{write_payload, WorkingSet, PAYLOAD_BYTE_LIMIT};
    use crate::contexts::{Context, Kind};
    use crate::dogstatsd::is_malformed;

    /// A working set of minted contexts across all three kinds.
    fn pool(rng: &mut SmallRng, n: usize) -> WorkingSet {
        WorkingSet::new(
            (0..n)
                .filter_map(|_| Context::mint_within(Kind::sample(rng), rng, PAYLOAD_BYTE_LIMIT))
                .collect(),
        )
    }

    /// Lines carry no interior newline and each is `\n`-terminated, so the line count equals the
    /// newline count.
    #[allow(clippy::naive_bytecount)]
    fn newline_count(buf: &[u8]) -> usize {
        buf.iter().filter(|&&b| b == b'\n').count()
    }

    proptest! {
        /// A rendered metric carries exactly the tag set its identity holds. The Agent assigns the tag
        /// set from every `#`-prefixed optional field it sees, last one winning, so a second such field
        /// would swap the identity's tags for whatever an occurrence body happened to contain. That
        /// would make one pooled identity render as several, putting cardinality past its cap and one
        /// point in each of many series. Occurrence bodies do carry `|` and `#`, but segments are
        /// always joined by a separator, so the two never land adjacent. This pins that.
        #[test]
        fn property_test_metric_carries_one_tag_field(seed: u64) {
            let mut rng = SmallRng::seed_from_u64(seed);
            let contexts = pool(&mut rng, 8);
            let mut buf = Vec::new();
            for _ in 0..4 {
                write_payload(&mut rng, &contexts, &mut buf, PAYLOAD_BYTE_LIMIT);
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

        /// Every datagram the driver packs from pooled contexts is one the Agent forwards. A packed
        /// datagram must be entirely well-formed.
        #[test]
        fn property_test_every_payload_is_well_formed(seed: u64) {
            let mut rng = SmallRng::seed_from_u64(seed);
            let contexts = pool(&mut rng, 8);
            let mut buf = Vec::new();
            for _ in 0..8 {
                write_payload(&mut rng, &contexts, &mut buf, PAYLOAD_BYTE_LIMIT);
                prop_assert_eq!(is_malformed(&buf), Ok(()), "emitted a droppable datagram: {:?}", String::from_utf8_lossy(&buf));
            }
        }

        /// A budget that can hold some context's identity yields load. An empty datagram spends one of
        /// the driver's configured sends without reaching the SUT, so the packer must fill the budget it
        /// was given rather than give up on the first context that does not fit. The fallback line is
        /// the floor below which a render that keeps landing on the drop side has nowhere to go.
        #[test]
        fn property_test_payload_is_never_empty_when_a_context_fits(seed: u64, limit_bytes in 1..8_192usize) {
            let mut rng = SmallRng::seed_from_u64(seed);
            let contexts = pool(&mut rng, 8);
            let mut buf = Vec::new();
            let payload = write_payload(&mut rng, &contexts, &mut buf, limit_bytes);

            let smallest = contexts.smallest_floor().expect("pool is non-empty");
            if limit_bytes > smallest {
                prop_assert!(!buf.is_empty(), "empty datagram at limit {limit_bytes}, smallest floor {smallest}");
                prop_assert!(payload.lines > 0);
            }
        }

        #[test]
        fn property_test_payload_stays_within_its_limit(seed: u64, limit_bytes: u16) {
            let mut rng = SmallRng::seed_from_u64(seed);
            let contexts = pool(&mut rng, 8);
            let limit_bytes = usize::from(limit_bytes);
            let mut buf = Vec::new();
            let payload = write_payload(&mut rng, &contexts, &mut buf, limit_bytes);

            prop_assert!(buf.len() <= limit_bytes);
            prop_assert_eq!(newline_count(&buf), payload.lines);
            if !buf.is_empty() {
                prop_assert_eq!(buf[buf.len() - 1], b'\n');
            }
        }

        /// An empty working set yields no load, not a panic.
        #[test]
        fn property_test_empty_pool_yields_empty_payload(seed: u64) {
            let mut rng = SmallRng::seed_from_u64(seed);
            let mut buf = Vec::new();
            let payload = write_payload(&mut rng, &WorkingSet::new(Vec::new()), &mut buf, PAYLOAD_BYTE_LIMIT);
            prop_assert_eq!(payload.lines, 0);
            prop_assert!(buf.is_empty());
        }
    }
}
