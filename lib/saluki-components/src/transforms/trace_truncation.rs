//! Trace truncation transform.
//!
//! Caps span resources, attribute keys and values, and the `env`, `component`, and `version` fields, like the
//! trace-agent's `Agent.TruncateV1` (Datadog traces) and `Agent.Truncate` (OTLP traces) in
//! `pkg/trace/agent/truncator.go`. It runs where the trace-agent truncates: after obfuscation, before tag replacement.

use std::sync::Arc;

use async_trait::async_trait;
use saluki_common::collections::FastHashMap;
use saluki_core::accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_core::{
    components::{transforms::*, BuildContext},
    data_model::event::trace::{AttributeValue, Span, Trace, TraceIngestInfo},
    topology::EventsBuffer,
};
use saluki_error::GenericError;
use stringtheory::MetaString;

use crate::common::datadog::traces::normalize::{
    is_structured_meta_key, truncate_utf8_str, truncate_with_ellipses, MAX_META_KEY_LEN, MAX_META_VAL_LEN,
    MAX_METRICS_KEY_LEN,
};

/// Trace truncation configuration.
pub struct TraceTruncationConfiguration {
    /// Maximum length of a span resource, in bytes.
    pub max_resource_len: usize,
}

#[async_trait]
impl SynchronousTransformBuilder for TraceTruncationConfiguration {
    async fn build(&self, _context: BuildContext) -> Result<Box<dyn SynchronousTransform + Send>, GenericError> {
        Ok(Box::new(TraceTruncation {
            max_resource_len: self.max_resource_len,
        }))
    }
}

impl MemoryBounds for TraceTruncationConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            .with_single_value::<TraceTruncation>("component struct");
    }
}

/// The truncation transform that processes traces.
pub struct TraceTruncation {
    max_resource_len: usize,
}

impl SynchronousTransform for TraceTruncation {
    fn transform_buffer(&mut self, buffer: &mut EventsBuffer) {
        let mut memo = Memo::default();
        for trace in buffer.events_mut::<Trace>() {
            let legacy = matches!(trace.ingest_info(), TraceIngestInfo::Otlp { .. });
            truncate_spans(trace.spans_mut(), self.max_resource_len, legacy, &mut memo);
        }
    }
}

/// Truncates `spans`, sharing truncated strings through `memo`.
pub(crate) fn truncate_spans(spans: &mut [Span], max_resource_len: usize, legacy: bool, memo: &mut Memo) {
    for span in spans {
        truncate_span(span, max_resource_len, legacy, memo);
    }
}

/// Shares truncated strings across the spans of one buffer.
///
/// Spans often share one allocation for a long value, such as an obfuscated SQL query from the obfuscation cache.
/// Truncating each copy separately would turn that one allocation into one per span, so each distinct source string
/// is truncated once and every span gets the same shared result.
#[derive(Default)]
pub(crate) struct Memo {
    /// Results by the source string's address, length, limit, and whether an ellipsis is appended.
    results: FastHashMap<(usize, usize, usize, bool), MetaString>,
    /// One source string per entry, kept alive until the buffer is done so that no address is reused.
    sources: Vec<MetaString>,
}

/// Strings this short may be stored inline, at an address that isn't theirs alone, so they are never memoized.
const MIN_MEMO_LEN: usize = 64;

impl Memo {
    /// Replaces `s` by its truncation to `limit` bytes, with an ellipsis when `ellipsis` is set, sharing the result
    /// with every other copy of the same allocation.
    fn replace(&mut self, s: &mut MetaString, limit: usize, ellipsis: bool) {
        let cut = |s: &str| {
            if ellipsis {
                truncate_with_ellipses(s, limit).into_owned()
            } else {
                truncate_utf8_str(s, limit).to_owned()
            }
        };
        if s.len() < MIN_MEMO_LEN {
            *s = cut(s).into();
            return;
        }
        let key = (s.as_ptr() as usize, s.len(), limit, ellipsis);
        if let Some(result) = self.results.get(&key) {
            // The entry's source keeps this address alive, so this handle can go now.
            *s = result.clone();
            return;
        }
        let cut = cut(s);
        let result = if cut.is_empty() {
            MetaString::empty()
        } else {
            MetaString::from(Arc::<str>::from(cut))
        };
        self.results.insert(key, result.clone());
        self.sources.push(std::mem::replace(s, result));
    }

    #[cfg(test)]
    pub(crate) fn entries_and_sources(&self) -> (usize, usize) {
        (self.results.len(), self.sources.len())
    }
}

/// How an attribute is truncated.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Rule {
    /// Left alone.
    Skip,
    /// Key capped at `MAX_META_KEY_LEN`, value at `MAX_META_VAL_LEN`.
    Meta,
    /// Key capped at `MAX_METRICS_KEY_LEN`.
    Key,
}

/// Returns the rule for an attribute.
///
/// `TruncateV1` (`legacy` off) truncates string attributes like Meta, skips bytes attributes (legacy meta_struct),
/// and truncates the key of every other attribute without exempting structured keys. `Truncate` (`legacy` on, the
/// path OTLP traces take in the trace-agent) sees integers and floats as Metrics and everything else as Meta, whose
/// structured keys are exempt. ADP holds no string form of a non-string Meta value, so only its key is truncated.
fn rule(key: &str, value: &AttributeValue, legacy: bool) -> Rule {
    match value {
        AttributeValue::String(_) if is_structured_meta_key(key) => Rule::Skip,
        AttributeValue::String(_) => Rule::Meta,
        AttributeValue::Int(_) | AttributeValue::Float(_) => Rule::Key,
        AttributeValue::Bytes(_) if !legacy => Rule::Skip,
        _ if legacy && is_structured_meta_key(key) => Rule::Skip,
        _ => Rule::Key,
    }
}

/// Returns `true` when `s` is over `limit` and must be cut.
///
/// OTLP attributes were already truncated, with an ellipsis, when they were translated. Cutting them again would
/// append a second ellipsis, so on OTLP spans a string within `limit + 3` bytes that ends in `...` is taken as already
/// truncated. ponytail: this misreads an untruncated value of that exact shape; drop it once translation stops
/// truncating attributes itself.
fn over(s: &str, limit: usize, legacy: bool) -> bool {
    s.len() > limit && !(legacy && s.len() <= limit + ELLIPSIS.len() && s.ends_with(ELLIPSIS))
}

const ELLIPSIS: &str = "...";

fn truncate_span(span: &mut Span, max_resource_len: usize, legacy: bool, memo: &mut Memo) {
    // The resource gets no ellipsis.
    if span.resource().len() > max_resource_len {
        memo.replace(span.resource_mut(), max_resource_len, false);
    }

    let mut rekeyed = Vec::new();
    for (key, value) in span.attributes.iter_mut() {
        let rule = rule(key, value, legacy);
        if rule == Rule::Skip {
            continue;
        }
        if let AttributeValue::String(s) = value {
            if over(s, MAX_META_VAL_LEN, legacy) {
                memo.replace(s, MAX_META_VAL_LEN, true);
            }
        }
        let max_key_len = if rule == Rule::Meta {
            MAX_META_KEY_LEN
        } else {
            MAX_METRICS_KEY_LEN
        };
        if over(key, max_key_len, legacy) {
            rekeyed.push((rule == Rule::Meta, key.clone(), max_key_len));
        }
    }
    // Rekey after iterating so that new keys are never revisited. String attributes go first, as Go maps them before
    // the other attributes' keys, so a non-string attribute wins a collision.
    rekeyed.sort_by_key(|(is_string, _, _)| !is_string);
    for (_, key, max_key_len) in rekeyed {
        if let Some(value) = span.attributes.remove(&key) {
            let new_key: MetaString = truncate_with_ellipses(&key, max_key_len).into_owned().into();
            span.attributes.insert(new_key, value);
        }
    }

    for field in [&mut span.env, &mut span.component, &mut span.version] {
        if over(field, MAX_META_VAL_LEN, legacy) {
            memo.replace(field, MAX_META_VAL_LEN, true);
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn span() -> Span {
        Span::new("svc", "op", "res", "web", 1, 0, 0, 0, 0)
    }

    fn string(s: impl Into<MetaString>) -> AttributeValue {
        AttributeValue::String(s.into())
    }

    fn get<'a>(span: &'a Span, key: &str) -> Option<&'a AttributeValue> {
        span.attributes.get(key)
    }

    // TestTruncateResourcePassThru / TestTruncateLongResource in truncator_test.go.
    #[test]
    fn resource_is_cut_without_ellipsis() {
        for limit in [5000, 15_000] {
            let mut s = span().with_resource("a".repeat(limit));
            truncate_span(&mut s, limit, false, &mut Memo::default());
            assert_eq!(s.resource().len(), limit, "at the limit is untouched");

            let mut s = span().with_resource("a".repeat(limit + 1));
            truncate_span(&mut s, limit, false, &mut Memo::default());
            assert_eq!(s.resource(), "a".repeat(limit));
        }
        // A 3-byte char straddling the limit is dropped whole.
        let mut s = span().with_resource(format!("{}€", "a".repeat(4998)));
        truncate_span(&mut s, 5000, false, &mut Memo::default());
        assert_eq!(s.resource(), "a".repeat(4998));
    }

    // TestTruncateMetaKeyTooLong / TestTruncateMetaValueTooLong / TestTruncateMetricsKeyTooLong.
    #[test]
    fn meta_and_metric_keys_and_values_get_an_ellipsis() {
        for legacy in [false, true] {
            let long_key = "k".repeat(MAX_META_KEY_LEN + 1);
            let mut s = span();
            s.attributes.insert(long_key.as_str().into(), string("v"));
            s.attributes
                .insert("val".into(), string(format!("{}é", "v".repeat(MAX_META_VAL_LEN - 1))));
            s.attributes
                .insert(format!("m{long_key}").into(), AttributeValue::Float(1.0));
            truncate_span(&mut s, 5000, legacy, &mut Memo::default());

            let cut_key = format!("{}...", "k".repeat(MAX_META_KEY_LEN));
            assert_eq!(get(&s, &cut_key), Some(&string("v")));
            assert_eq!(
                get(&s, "val"),
                Some(&string(format!("{}...", "v".repeat(MAX_META_VAL_LEN - 1))))
            );
            let cut_metric = format!("m{}...", "k".repeat(MAX_METRICS_KEY_LEN - 1));
            assert_eq!(get(&s, &cut_metric), Some(&AttributeValue::Float(1.0)));
            assert_eq!(s.attributes.len(), 3);
        }
    }

    #[test]
    fn structured_and_bytes_attributes_follow_each_path() {
        let structured = format!("_dd.{}.json", "x".repeat(MAX_META_KEY_LEN));
        let long_value = "v".repeat(MAX_META_VAL_LEN + 1);
        let long_bytes = format!("b{}", "x".repeat(MAX_META_KEY_LEN));
        let build = || {
            let mut s = span();
            s.attributes
                .insert(structured.as_str().into(), string(long_value.as_str()));
            s.attributes
                .insert(long_bytes.as_str().into(), AttributeValue::Bytes(vec![1]));
            s.attributes.insert(
                format!("_dd.i{}.json", "x".repeat(MAX_META_KEY_LEN)).into(),
                AttributeValue::Int(1),
            );
            s.attributes.insert(
                format!("_dd.b{}.json", "x".repeat(MAX_META_KEY_LEN)).into(),
                AttributeValue::Bool(true),
            );
            s
        };

        // V1: structured strings and bytes untouched; numbers and bools lose their key regardless of shape.
        let mut s = build();
        truncate_span(&mut s, 5000, false, &mut Memo::default());
        assert_eq!(get(&s, &structured), Some(&string(long_value.as_str())));
        assert!(get(&s, &long_bytes).is_some());
        assert!(s.attributes.keys().filter(|k| k.ends_with("...")).count() == 2);

        // Legacy: bytes keys are truncated, structured bools are exempt, numbers are not.
        let mut s = build();
        truncate_span(&mut s, 5000, true, &mut Memo::default());
        assert_eq!(get(&s, &structured), Some(&string(long_value.as_str())));
        assert!(get(&s, &long_bytes).is_none());
        assert!(get(&s, &format!("_dd.b{}.json", "x".repeat(MAX_META_KEY_LEN))).is_some());
        assert!(s.attributes.keys().filter(|k| k.ends_with("...")).count() == 2);
    }

    // Go maps string attributes first and other keys afterwards, so the integer wins this collision.
    #[test]
    fn a_non_string_attribute_wins_a_truncated_key_collision() {
        let prefix = "k".repeat(MAX_META_KEY_LEN);
        for legacy in [false, true] {
            let mut s = span();
            s.attributes.insert(format!("{prefix}a").into(), string("str"));
            s.attributes.insert(format!("{prefix}b").into(), AttributeValue::Int(7));
            truncate_span(&mut s, 5000, legacy, &mut Memo::default());
            assert_eq!(s.attributes.len(), 1);
            assert_eq!(get(&s, &format!("{prefix}...")), Some(&AttributeValue::Int(7)));
        }
    }

    // The OTLP translator truncates attributes first; the final pass must not append a second ellipsis.
    #[test]
    fn otlp_attributes_truncated_at_translation_are_left_alone() {
        use crate::common::otlp::traces::transform::truncate_attributes;

        let value = format!("{}é", "v".repeat(MAX_META_VAL_LEN - 1));
        let key = format!("{}é", "k".repeat(MAX_META_KEY_LEN - 1));
        let mut s = span();
        s.attributes.insert(key.as_str().into(), string(value.as_str()));
        truncate_attributes(&mut s.attributes);
        let translated = s.clone();
        let cut_key = format!("{}...", "k".repeat(MAX_META_KEY_LEN - 1));
        let cut_value = format!("{}...", "v".repeat(MAX_META_VAL_LEN - 1));
        assert_eq!(get(&s, &cut_key), Some(&string(cut_value.as_str())));

        truncate_span(&mut s, 5000, true, &mut Memo::default());
        assert_eq!(s, translated);
    }

    #[test]
    fn spans_sharing_a_long_value_share_the_truncated_result() {
        let long = MetaString::from(std::sync::Arc::<str>::from("v".repeat(MAX_META_VAL_LEN + 100)));
        let resource = MetaString::from(std::sync::Arc::<str>::from("r".repeat(6_000)));
        let mut spans: Vec<Span> = (0..1_000)
            .map(|_| {
                let mut s = span().with_resource(resource.clone());
                s.attributes.insert("v".into(), AttributeValue::String(long.clone()));
                s
            })
            .collect();
        truncate_spans(&mut spans, 5000, false, &mut Memo::default());
        let value_ptr = |s: &Span| get(s, "v").and_then(AttributeValue::as_string).unwrap().as_ptr();
        for s in &spans {
            assert_eq!(s.resource().len(), 5000);
            assert_eq!(s.resource().as_ptr(), spans[0].resource().as_ptr());
            assert_eq!(value_ptr(s), value_ptr(&spans[0]));
        }
    }

    #[test]
    fn env_component_and_version_are_truncated_like_meta_values() {
        let long = "e".repeat(MAX_META_VAL_LEN + 1);
        let mut s = span()
            .with_env(long.as_str())
            .with_component(long.as_str())
            .with_version(long.as_str());
        truncate_span(&mut s, 5000, false, &mut Memo::default());
        let expected = format!("{}...", "e".repeat(MAX_META_VAL_LEN));
        for field in [&s.env, &s.component, &s.version] {
            assert_eq!(&**field, expected);
        }
    }

    #[test]
    fn truncation_is_idempotent_and_keeps_untouched_values_shared() {
        let shared = MetaString::from(std::sync::Arc::<str>::from("short value"));
        let mut s = span().with_resource("r".repeat(6000));
        s.attributes.insert("ok".into(), AttributeValue::String(shared.clone()));
        s.attributes
            .insert("k".repeat(300).into(), string("v".repeat(MAX_META_VAL_LEN + 10)));
        truncate_span(&mut s, 5000, false, &mut Memo::default());
        let once = s.clone();
        truncate_span(&mut s, 5000, false, &mut Memo::default());
        assert_eq!(s, once);
        let kept = get(&s, "ok").and_then(AttributeValue::as_string).unwrap();
        assert_eq!(kept.as_ptr(), shared.as_ptr());
    }

    /// A direct port of Go's `normalize.TruncateUTF8` (pkg/trace/traceutil/normalize/truncate.go), on bytes.
    fn go_truncate_utf8(s: &str, limit: usize) -> &[u8] {
        let mut s = s.as_bytes();
        if s.len() <= limit {
            return s;
        }
        s = &s[..limit];
        while !s.is_empty() {
            if (1..=4)
                .rev()
                .any(|n| s.len() >= n && simdutf8::basic::from_utf8(&s[s.len() - n..]).is_ok())
            {
                break;
            }
            s = &s[..s.len() - 1];
        }
        s
    }

    proptest! {
        #[test]
        fn property_test_cuts_match_go_and_stay_on_char_boundaries(s in "\\PC{0,64}", limit in 0usize..80) {
            let cut = truncate_with_ellipses(&s, limit);
            if s.len() > limit {
                let body = cut.strip_suffix("...").expect("ellipsis");
                prop_assert!(body.len() <= limit && s.starts_with(body));
                prop_assert_eq!(body.as_bytes(), go_truncate_utf8(&s, limit));
            } else {
                prop_assert_eq!(&*cut, s.as_str());
            }
        }
    }
}
