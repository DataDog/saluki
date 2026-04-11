use std::hash::{BuildHasher, Hash, Hasher};

use saluki_common::collections::FastHashMap;
use saluki_common::hash::{FastBuildHasher, get_fast_build_hasher};
use tracing::Metadata;

// -- Token types --

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
enum TokenType {
    /// ERROR, WARN, INFO, DEBUG, TRACE -- never wildcarded.
    SeverityLevel = 0,
    /// Pure alphabetic/symbol token -- potential wildcard.
    Word = 1,
    /// Contains at least one ASCII digit -- always wildcarded on different value.
    Numeric = 2,
}

struct InputToken<'a> {
    ty: TokenType,
    value: &'a str,
    start: usize,
    end: usize,
}

fn classify_token(token: &str) -> TokenType {
    match token {
        "ERROR" | "WARN" | "INFO" | "DEBUG" | "TRACE" => TokenType::SeverityLevel,
        _ => {
            if token.bytes().any(|b| b.is_ascii_digit()) {
                TokenType::Numeric
            } else {
                TokenType::Word
            }
        }
    }
}

// -- Signature --

/// Compute a structural signature over the token type sequence.
///
/// The first Word token's value is embedded in the hash (first-word protection), so messages with
/// different leading words hash to different clusters even if their type sequences match.
fn compute_signature(tokens: &[InputToken<'_>], hasher_state: &impl BuildHasher) -> u64 {
    let mut h = hasher_state.build_hasher();
    tokens.len().hash(&mut h);
    let mut first_word_done = false;
    for tok in tokens {
        tok.ty.hash(&mut h);
        if !first_word_done && tok.ty == TokenType::Word {
            tok.value.hash(&mut h);
            first_word_done = true;
        }
    }
    h.finish()
}

// -- Pattern --

struct PatternSlot {
    ty: TokenType,
    /// `None` means this position is a wildcard.
    value: Option<String>,
}

struct Pattern {
    slots: Vec<PatternSlot>,
    wildcard_count: usize,
    merge_count: u32,
    saturated: bool,
}

impl Pattern {
    fn from_tokens(tokens: &[InputToken<'_>]) -> Self {
        let mut wildcard_count = 0;
        let slots = tokens
            .iter()
            .map(|tok| {
                // Numeric tokens start as wildcards immediately (same as old heuristic).
                if tok.ty == TokenType::Numeric {
                    wildcard_count += 1;
                    PatternSlot {
                        ty: tok.ty,
                        value: None,
                    }
                } else {
                    PatternSlot {
                        ty: tok.ty,
                        value: Some(tok.value.to_owned()),
                    }
                }
            })
            .collect();
        Self {
            slots,
            wildcard_count,
            merge_count: 0,
            saturated: false,
        }
    }
}

/// Read-only compatibility check. Returns `false` on first conflict.
fn can_merge(pattern: &Pattern, tokens: &[InputToken<'_>]) -> bool {
    if pattern.slots.len() != tokens.len() {
        return false;
    }
    for (slot, tok) in pattern.slots.iter().zip(tokens.iter()) {
        if slot.ty != tok.ty {
            return false;
        }
        // SeverityLevel tokens conflict on different values (never wildcarded).
        if slot.ty == TokenType::SeverityLevel {
            if let Some(ref sv) = slot.value {
                if sv != tok.value {
                    return false;
                }
            }
        }
    }
    true
}

/// Single-pass merge. Returns `true` if the template changed (new wildcards introduced).
fn try_merge(pattern: &mut Pattern, tokens: &[InputToken<'_>]) -> bool {
    debug_assert_eq!(pattern.slots.len(), tokens.len());
    let mut changed = false;
    for (slot, tok) in pattern.slots.iter_mut().zip(tokens.iter()) {
        debug_assert_eq!(slot.ty, tok.ty);
        match &slot.value {
            None => {} // Already wildcard.
            Some(existing) => {
                if existing != tok.value {
                    // Same type, different value -> wildcard.
                    slot.value = None;
                    pattern.wildcard_count += 1;
                    changed = true;
                }
            }
        }
    }
    changed
}

// -- Cluster --

struct Cluster {
    patterns: Vec<Pattern>,
    /// Hot-pattern cache: index of the last matched pattern in `patterns`.
    last_matched: Option<usize>,
}

impl Cluster {
    fn new(pattern: Pattern) -> Self {
        Self {
            patterns: vec![pattern],
            last_matched: Some(0),
        }
    }
}

/// Try to merge tokens into an existing pattern in the cluster.
/// Returns `Some(pattern_index)` on success, `None` if no pattern matched.
fn find_and_merge(cluster: &mut Cluster, tokens: &[InputToken<'_>], saturation_threshold: u32) -> Option<usize> {
    // Try hot-pattern cache first.
    if let Some(cached_idx) = cluster.last_matched {
        if cached_idx < cluster.patterns.len() {
            let pattern = &cluster.patterns[cached_idx];
            if pattern.saturated {
                // Saturated: skip can_merge, go straight to try_merge.
                if pattern.slots.len() == tokens.len() {
                    let pattern = &mut cluster.patterns[cached_idx];
                    let changed = try_merge(pattern, tokens);
                    if !changed {
                        pattern.merge_count += 1;
                        return Some(cached_idx);
                    } else {
                        // Template evolved -- desaturate.
                        pattern.saturated = false;
                        pattern.merge_count = 0;
                        return Some(cached_idx);
                    }
                }
            } else if can_merge(pattern, tokens) {
                let pattern = &mut cluster.patterns[cached_idx];
                let changed = try_merge(pattern, tokens);
                if changed {
                    pattern.merge_count = 0;
                } else {
                    pattern.merge_count += 1;
                    if pattern.merge_count >= saturation_threshold {
                        pattern.saturated = true;
                    }
                }
                return Some(cached_idx);
            }
        }
    }

    // Full scan of all patterns.
    for (i, pattern) in cluster.patterns.iter_mut().enumerate() {
        if Some(i) == cluster.last_matched {
            continue; // Already tried above.
        }
        if can_merge(pattern, tokens) {
            let changed = try_merge(pattern, tokens);
            if changed {
                pattern.merge_count = 0;
            } else {
                pattern.merge_count += 1;
                if pattern.merge_count >= saturation_threshold {
                    pattern.saturated = true;
                }
            }
            cluster.last_matched = Some(i);
            return Some(i);
        }
    }

    None
}

// -- Callsite cache --

const CALLSITE_LEARNING_THRESHOLD: u32 = 10;
const CALLSITE_INSTABILITY_RATE_PERCENT: u32 = 30;
const CALLSITE_REVALIDATION_INTERVAL: u32 = 1000;

enum CallsiteState {
    /// Route directly to cached cluster via stored signature. Still do full merge.
    Learning {
        signature: u64,
        hits: u32,
        misses: u32,
    },
    /// Pattern is saturated and callsite consistently produces the same structure.
    /// Only whitespace-split + positional extraction needed.
    Converged {
        skeleton: String,
        wildcard_positions: Vec<usize>,
        expected_token_count: usize,
        fast_path_hits: u32,
    },
    /// Too dynamic to cache.
    Unstable,
}

// -- ClusterManager --

const DEFAULT_MAX_PATTERNS: usize = 10_000;
const DEFAULT_SATURATION_THRESHOLD: u32 = 50;

/// A Drain-inspired pattern clustering manager for log template extraction.
///
/// Replaces `SkeletonParser` with a stateful approach that learns to wildcard pure-word positions
/// by observing value variation across messages with the same token-type structure.
pub struct ClusterManager {
    clusters: FastHashMap<u64, Cluster>,
    callsite_cache: FastHashMap<usize, CallsiteState>,
    hasher_state: FastBuildHasher,
    /// Reusable buffer for tokenization output. Reset each call.
    token_buf: Vec<InputToken<'static>>,
    /// Byte ranges of wildcard values from the last extract() call.
    variable_ranges: Vec<(usize, usize)>,
    /// Rendered skeleton from the last extract() call.
    skeleton_buf: String,
    max_patterns: usize,
    saturation_threshold: u32,
    total_patterns: usize,
}

impl Default for ClusterManager {
    fn default() -> Self {
        Self {
            clusters: FastHashMap::default(),
            callsite_cache: FastHashMap::default(),
            hasher_state: get_fast_build_hasher(),
            token_buf: Vec::new(),
            variable_ranges: Vec::new(),
            skeleton_buf: String::new(),
            max_patterns: DEFAULT_MAX_PATTERNS,
            saturation_threshold: DEFAULT_SATURATION_THRESHOLD,
            total_patterns: 0,
        }
    }
}

impl ClusterManager {
    /// Process a message through the clustering pipeline. Returns `(skeleton, var_count)`.
    ///
    /// The skeleton uses `\x00` as the wildcard placeholder (same format as `SkeletonParser`).
    /// After calling this, use [`variable_tokens`] to iterate over the wildcard values.
    pub fn extract(&mut self, message: &str, metadata: &'static Metadata<'static>) -> (&str, usize) {
        self.variable_ranges.clear();

        let callsite_key = metadata as *const Metadata<'static> as usize;

        // Try the converged fast path first.
        if let Some(CallsiteState::Converged {
            skeleton,
            wildcard_positions,
            expected_token_count,
            fast_path_hits,
        }) = self.callsite_cache.get_mut(&callsite_key)
        {
            // Periodic re-validation: every N hits, fall through to the full path.
            let needs_revalidation = *fast_path_hits > 0 && *fast_path_hits % CALLSITE_REVALIDATION_INTERVAL == 0;
            if !needs_revalidation {
                // Quick check: split by whitespace and count.
                let tokens: Vec<&str> = message.split_ascii_whitespace().collect();
                if tokens.len() == *expected_token_count {
                    for &pos in wildcard_positions.iter() {
                        let tok = tokens[pos];
                        // Find the byte range in the original message.
                        let start = tok.as_ptr() as usize - message.as_ptr() as usize;
                        self.variable_ranges.push((start, start + tok.len()));
                    }
                    *fast_path_hits += 1;
                    // Copy the cached skeleton into our output buffer.
                    self.skeleton_buf.clear();
                    self.skeleton_buf.push_str(skeleton);
                    let var_count = self.variable_ranges.len();
                    return (&self.skeleton_buf, var_count);
                }
            }
            // Token count mismatch or revalidation needed -- demote to Learning.
            // We need the signature, so we'll fall through to the full path below.
            // But first, extract what we need before mutating the map.
        }

        // Full path: tokenize, compute signature, cluster lookup, merge.
        self.tokenize(message);
        let signature = compute_signature(&self.token_buf, &self.hasher_state);

        // Update callsite cache.
        match self.callsite_cache.get_mut(&callsite_key) {
            Some(CallsiteState::Learning {
                signature: ref mut cached_sig,
                ref mut hits,
                ref mut misses,
            }) => {
                if signature == *cached_sig {
                    *hits += 1;
                } else {
                    *misses += 1;
                    let total = *hits + *misses;
                    if total >= CALLSITE_LEARNING_THRESHOLD
                        && (*misses * 100 / total) > CALLSITE_INSTABILITY_RATE_PERCENT
                    {
                        self.callsite_cache
                            .insert(callsite_key, CallsiteState::Unstable);
                    } else {
                        *cached_sig = signature;
                    }
                }
            }
            Some(CallsiteState::Converged { .. }) => {
                // Re-validation path or token count mismatch: demote to Learning.
                self.callsite_cache.insert(
                    callsite_key,
                    CallsiteState::Learning {
                        signature,
                        hits: 1,
                        misses: 0,
                    },
                );
            }
            Some(CallsiteState::Unstable) => {}
            None => {
                self.callsite_cache.insert(
                    callsite_key,
                    CallsiteState::Learning {
                        signature,
                        hits: 1,
                        misses: 0,
                    },
                );
            }
        }

        // Cluster lookup and merge.
        let sat_threshold = self.saturation_threshold;
        let cluster_exists = self.clusters.contains_key(&signature);

        let pattern_idx = if let Some(cluster) = self.clusters.get_mut(&signature) {
            find_and_merge(cluster, &self.token_buf, sat_threshold)
        } else {
            None
        };

        let pattern_idx = match pattern_idx {
            Some(idx) => idx,
            None if !cluster_exists => {
                // Create new cluster with initial pattern.
                if self.total_patterns < self.max_patterns {
                    let pattern = Pattern::from_tokens(&self.token_buf);
                    self.total_patterns += 1;
                    self.clusters.insert(signature, Cluster::new(pattern));
                    0
                } else {
                    self.token_buf.clear();
                    return self.naive_extract(message);
                }
            }
            None => {
                // Cluster exists but no pattern matched -- create new pattern in cluster.
                if self.total_patterns < self.max_patterns {
                    let pattern = Pattern::from_tokens(&self.token_buf);
                    self.total_patterns += 1;
                    let cluster = self.clusters.get_mut(&signature).unwrap();
                    cluster.patterns.push(pattern);
                    let idx = cluster.patterns.len() - 1;
                    cluster.last_matched = Some(idx);
                    idx
                } else {
                    self.token_buf.clear();
                    return self.naive_extract(message);
                }
            }
        };

        // Render skeleton and record variable positions. We need to read the pattern's slots while
        // also writing to skeleton_buf/variable_ranges, so do the render inline to satisfy the borrow
        // checker.
        {
            let cluster = self.clusters.get(&signature).unwrap();
            let pattern = &cluster.patterns[pattern_idx];

            self.skeleton_buf.clear();
            self.variable_ranges.clear();
            let mut first = true;
            for (slot, tok) in pattern.slots.iter().zip(self.token_buf.iter()) {
                if !first {
                    self.skeleton_buf.push(' ');
                }
                first = false;
                if slot.value.is_none() {
                    self.skeleton_buf.push('\x00');
                    self.variable_ranges.push((tok.start, tok.end));
                } else {
                    self.skeleton_buf.push_str(slot.value.as_ref().unwrap());
                }
            }

            // Check if we should promote callsite to Converged.
            if pattern.saturated {
                if let Some(CallsiteState::Learning { hits, .. }) = self.callsite_cache.get(&callsite_key) {
                    if *hits >= CALLSITE_LEARNING_THRESHOLD {
                        let skeleton = self.skeleton_buf.clone();
                        let wildcard_positions: Vec<usize> = pattern
                            .slots
                            .iter()
                            .enumerate()
                            .filter(|(_, s)| s.value.is_none())
                            .map(|(i, _)| i)
                            .collect();
                        let expected_token_count = pattern.slots.len();
                        self.callsite_cache.insert(
                            callsite_key,
                            CallsiteState::Converged {
                                skeleton,
                                wildcard_positions,
                                expected_token_count,
                                fast_path_hits: 0,
                            },
                        );
                    }
                }
            }
        }

        self.token_buf.clear();
        let var_count = self.variable_ranges.len();
        (&self.skeleton_buf, var_count)
    }

    /// Returns an iterator over the variable token values from the last `extract()` call.
    pub fn variable_tokens<'a>(&self, message: &'a str) -> impl Iterator<Item = &'a str> {
        let ranges = self.variable_ranges.clone();
        ranges.into_iter().map(move |(start, end)| &message[start..end])
    }

    /// Tokenize a message into `self.token_buf`.
    fn tokenize(&mut self, message: &str) {
        self.token_buf.clear();
        for token in message.split_ascii_whitespace() {
            let start = token.as_ptr() as usize - message.as_ptr() as usize;
            let end = start + token.len();
            let ty = classify_token(token);
            // SAFETY: We store references to `message` with a fake 'static lifetime. The token_buf
            // is always cleared before the borrow of `message` ends (at end of extract()).
            let value: &'static str = unsafe { std::mem::transmute::<&str, &'static str>(token) };
            self.token_buf.push(InputToken {
                ty,
                value,
                start,
                end,
            });
        }
    }

    /// Naive fallback: same heuristic as `SkeletonParser` (tokens with digits = variables).
    fn naive_extract(&mut self, message: &str) -> (&str, usize) {
        self.skeleton_buf.clear();
        self.variable_ranges.clear();
        let mut first = true;
        for token in message.split_ascii_whitespace() {
            if !first {
                self.skeleton_buf.push(' ');
            }
            first = false;
            if token.bytes().any(|b| b.is_ascii_digit()) {
                self.skeleton_buf.push('\x00');
                let start = token.as_ptr() as usize - message.as_ptr() as usize;
                self.variable_ranges.push((start, start + token.len()));
            } else {
                self.skeleton_buf.push_str(token);
            }
        }
        let var_count = self.variable_ranges.len();
        (&self.skeleton_buf, var_count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_metadata() -> &'static Metadata<'static> {
        use tracing::callsite;
        struct TestCallsite;
        static META: Metadata<'static> = Metadata::new(
            "test",
            "test::target",
            tracing::Level::INFO,
            Some("test.rs"),
            Some(1),
            Some("test::target"),
            tracing::field::FieldSet::new(&[], tracing::callsite::Identifier(&TestCallsite)),
            tracing::metadata::Kind::EVENT,
        );
        impl callsite::Callsite for TestCallsite {
            fn set_interest(&self, _: tracing::subscriber::Interest) {}
            fn metadata(&self) -> &Metadata<'_> {
                &META
            }
        }
        &META
    }

    // Create distinct metadata for each "callsite" in multi-callsite tests.
    macro_rules! make_metadata {
        ($name:ident) => {
            fn $name() -> &'static Metadata<'static> {
                use tracing::callsite;
                struct CS;
                static META: Metadata<'static> = Metadata::new(
                    stringify!($name),
                    "test",
                    tracing::Level::INFO,
                    Some("test.rs"),
                    Some(1),
                    Some("test"),
                    tracing::field::FieldSet::new(&[], tracing::callsite::Identifier(&CS)),
                    tracing::metadata::Kind::EVENT,
                );
                impl callsite::Callsite for CS {
                    fn set_interest(&self, _: tracing::subscriber::Interest) {}
                    fn metadata(&self) -> &Metadata<'_> {
                        &META
                    }
                }
                &META
            }
        };
    }

    #[test]
    fn empty_message() {
        let mut cm = ClusterManager::default();
        let meta = test_metadata();
        let (skeleton, var_count) = cm.extract("", meta);
        assert_eq!(skeleton, "");
        assert_eq!(var_count, 0);
    }

    #[test]
    fn all_static_tokens() {
        let mut cm = ClusterManager::default();
        let meta = test_metadata();
        let (skeleton, var_count) = cm.extract("hello world foo", meta);
        assert_eq!(skeleton, "hello world foo");
        assert_eq!(var_count, 0);
    }

    #[test]
    fn numeric_tokens_are_immediately_wildcarded() {
        let mut cm = ClusterManager::default();
        let meta = test_metadata();
        let (skeleton, var_count) = cm.extract("Processed 1234 events in 56ms", meta);
        assert_eq!(skeleton, "Processed \x00 events in \x00");
        assert_eq!(var_count, 2);
        let vars: Vec<&str> = cm.variable_tokens("Processed 1234 events in 56ms").collect();
        assert_eq!(vars, vec!["1234", "56ms"]);
    }

    #[test]
    fn word_tokens_wildcard_on_different_values() {
        let mut cm = ClusterManager::default();
        let meta = test_metadata();

        // First message: all static.
        let (s1, _) = cm.extract("Forwarding to dogstatsd", meta);
        assert_eq!(s1, "Forwarding to dogstatsd");

        // Second message: "dogstatsd" vs "topology_runner" -> position 2 becomes wildcard.
        let (s2, v2) = cm.extract("Forwarding to topology_runner", meta);
        assert_eq!(s2, "Forwarding to \x00");
        assert_eq!(v2, 1);
        let vars: Vec<&str> = cm.variable_tokens("Forwarding to topology_runner").collect();
        assert_eq!(vars, vec!["topology_runner"]);
    }

    #[test]
    fn severity_levels_never_wildcard() {
        make_metadata!(meta_a);
        make_metadata!(meta_b);
        let mut cm = ClusterManager::default();

        let (s1, _) = cm.extract("ERROR something failed", meta_a());
        assert_eq!(s1, "ERROR something failed");

        // Different severity -> different cluster (first-word protection + SeverityLevel type).
        let (s2, _) = cm.extract("WARN something failed", meta_b());
        assert_eq!(s2, "WARN something failed");
    }

    #[test]
    fn mixed_numeric_and_word_wildcards() {
        let mut cm = ClusterManager::default();
        let meta = test_metadata();

        let (s1, v1) = cm.extract("Forwarding 5000 metric(s) to dogstatsd", meta);
        assert_eq!(s1, "Forwarding \x00 metric(s) to dogstatsd");
        assert_eq!(v1, 1);

        // Second message: numeric changes AND word changes.
        let (s2, v2) = cm.extract("Forwarding 3000 metric(s) to topology_runner", meta);
        assert_eq!(s2, "Forwarding \x00 metric(s) to \x00");
        assert_eq!(v2, 2);
        let vars: Vec<&str> = cm.variable_tokens("Forwarding 3000 metric(s) to topology_runner").collect();
        assert_eq!(vars, vec!["3000", "topology_runner"]);
    }

    #[test]
    fn different_token_counts_create_separate_patterns() {
        make_metadata!(meta_a);
        make_metadata!(meta_b);
        let mut cm = ClusterManager::default();

        let (s1, _) = cm.extract("hello world", meta_a());
        assert_eq!(s1, "hello world");

        let (s2, _) = cm.extract("hello world foo", meta_b());
        assert_eq!(s2, "hello world foo");
    }

    #[test]
    fn saturation_after_threshold() {
        let mut cm = ClusterManager::default();
        let meta = test_metadata();

        // Feed the same structure many times to reach saturation.
        for i in 0..60 {
            let msg = format!("Processed {} events", i);
            cm.extract(&msg, meta);
        }

        // Verify the cluster's pattern is saturated.
        cm.tokenize("Processed 0 events");
        let sig = compute_signature(&cm.token_buf, &cm.hasher_state);
        cm.token_buf.clear();

        let cluster = cm.clusters.get(&sig).unwrap();
        assert!(cluster.patterns[0].saturated);
    }

    #[test]
    fn callsite_converged_fast_path() {
        let mut cm = ClusterManager::default();
        let meta = test_metadata();

        // Feed enough messages to saturate (50) + reach callsite learning threshold (10).
        for i in 0..70 {
            let msg = format!("Request {} completed in {}ms", i, i * 10);
            cm.extract(&msg, meta);
        }

        // Verify callsite is now Converged.
        let key = meta as *const Metadata<'static> as usize;
        assert!(matches!(
            cm.callsite_cache.get(&key),
            Some(CallsiteState::Converged { .. })
        ));

        // Next call should use the fast path.
        let (skeleton, var_count) = cm.extract("Request 999 completed in 9990ms", meta);
        assert_eq!(skeleton, "Request \x00 completed in \x00");
        assert_eq!(var_count, 2);
    }

    #[test]
    fn variable_tokens_returns_correct_values() {
        let mut cm = ClusterManager::default();
        let meta = test_metadata();

        let msg = "Processed 1234 events in 56ms";
        cm.extract(msg, meta);
        let vars: Vec<&str> = cm.variable_tokens(msg).collect();
        assert_eq!(vars, vec!["1234", "56ms"]);
    }

    #[test]
    fn naive_fallback_when_at_capacity() {
        let mut cm = ClusterManager::default();
        cm.max_patterns = 1; // Very low cap for testing.
        let meta = test_metadata();

        // First message creates a pattern.
        cm.extract("hello world", meta);
        assert_eq!(cm.total_patterns, 1);

        // Different structure that can't merge -- should fallback to naive.
        make_metadata!(meta2);
        let (skeleton, var_count) = cm.extract("goodbye 123 world", meta2());
        assert_eq!(skeleton, "goodbye \x00 world");
        assert_eq!(var_count, 1);
    }
}
