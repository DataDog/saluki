use std::collections::HashMap;

use datadog_protos::stateful::{datum, dynamic_value, Datum, DictEntryDefine, DynamicValue, FlatLog, PatternDefine};

use crate::{StatefulDatum, StatefulMessage};

const FLAT_LOG_EMPTY_DICT_INDEX: u64 = 1;
const DEFAULT_STATUS: &str = "info";
const DYNAMIC_STRING_DICTIONARY_THRESHOLD: u16 = 2;
const MAX_PENDING_DYNAMIC_STRINGS: usize = 1_000_000;
const MIN_DYNAMIC_STRING_LEN: usize = 2;
const MAX_DYNAMIC_STRING_LEN: usize = 128;

/// A log event that can be translated into stateful log datums.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LogRecord {
    /// The raw log body before stateful encoding.
    pub body: Vec<u8>,
    /// The timestamp to place on the emitted `FlatLog`, in Unix milliseconds.
    pub timestamp_millis: i64,
    /// The top-level service, encoded separately from the tag set.
    pub service: Option<String>,
    /// The severity level. If absent, the translator emits `info`.
    pub status: Option<String>,
    /// The log source, emitted as the `ddsource:<source>` tag.
    pub source: Option<String>,
    /// The hostname, emitted as the `hostname:<hostname>` tag.
    pub hostname: Option<String>,
    /// Origin tags to join into the dictionary-backed tag set.
    pub tags: Vec<String>,
    /// Processing tags that participate in cache identity but are not emitted in the tag set.
    pub processing_tags: Vec<String>,
    /// Optional correlation identifier shared with another sender path.
    pub uuid: Option<String>,
}

impl LogRecord {
    /// Creates a new log record from a body and Unix millisecond timestamp.
    pub fn new(body: impl Into<Vec<u8>>, timestamp_millis: i64) -> Self {
        Self {
            body: body.into(),
            timestamp_millis,
            ..Self::default()
        }
    }
}

/// A pattern definition produced by a pluggable pattern extractor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtractedPattern {
    /// The server-visible pattern identifier.
    pub pattern_id: u64,
    /// The template string with wildcard holes.
    pub template: String,
    /// Character positions where dynamic values are inserted.
    pub pos_list: Vec<u32>,
    /// Values captured for this log instance.
    pub dynamic_values: Vec<String>,
    /// Whether the pattern definition should be emitted before this log.
    pub define: bool,
}

impl ExtractedPattern {
    fn param_count(&self) -> u32 {
        self.dynamic_values.len().try_into().unwrap_or(u32::MAX)
    }
}

/// Extracts a stateful pattern from a log body.
pub trait PatternExtractor {
    /// Returns a pattern extraction for the given log body, or `None` to use raw-log encoding.
    fn extract(&mut self, body: &str) -> Option<ExtractedPattern>;
}

/// A pattern extractor that always falls back to raw-log encoding.
#[derive(Clone, Debug, Default)]
pub struct NoopPatternExtractor;

impl PatternExtractor for NoopPatternExtractor {
    fn extract(&mut self, _body: &str) -> Option<ExtractedPattern> {
        None
    }
}

/// Translates log records into stateful log messages.
#[derive(Clone, Debug)]
pub struct StatefulLogTranslator<P = NoopPatternExtractor> {
    dictionary: Dictionary,
    tag_cache: Option<TagCache>,
    pattern_extractor: P,
}

impl Default for StatefulLogTranslator<NoopPatternExtractor> {
    fn default() -> Self {
        Self::new()
    }
}

impl StatefulLogTranslator<NoopPatternExtractor> {
    /// Creates a translator that emits raw `FlatLog` datums.
    pub fn new() -> Self {
        Self::with_pattern_extractor(NoopPatternExtractor)
    }
}

impl<P> StatefulLogTranslator<P>
where
    P: PatternExtractor,
{
    /// Creates a translator with a custom pattern extractor.
    pub fn with_pattern_extractor(pattern_extractor: P) -> Self {
        Self {
            dictionary: Dictionary::default(),
            tag_cache: None,
            pattern_extractor,
        }
    }

    /// Translates a single log record into ordered stateful messages.
    ///
    /// Dictionary definitions are emitted before the log datum that references them.
    pub fn translate(&mut self, record: &LogRecord) -> Vec<StatefulMessage> {
        if record.body.is_empty() {
            return Vec::new();
        }

        let mut messages = Vec::new();
        let body = String::from_utf8_lossy(&record.body).into_owned();
        let service_id = self.define_optional_field(record.service.as_deref(), &mut messages);
        let status = record.status.as_deref().unwrap_or(DEFAULT_STATUS);
        let status_id = self.define_field(status, &mut messages);
        let tags_id = self.define_tag_set(record, &mut messages);

        if let Some(pattern) = self.pattern_extractor.extract(&body) {
            self.emit_patterned_log(record, pattern, service_id, status_id, tags_id, &mut messages);
        } else {
            messages.push(StatefulMessage::new(StatefulDatum::new(Datum {
                data: Some(datum::Data::FlatLog(FlatLog {
                    timestamp: record.timestamp_millis,
                    status: flat_log_dict_index(status_id),
                    service: flat_log_dict_index(service_id),
                    tags: flat_log_dict_index(tags_id),
                    raw_log: body,
                    json_schema_id: FLAT_LOG_EMPTY_DICT_INDEX,
                    uuid: record.uuid.clone(),
                    ..FlatLog::default()
                })),
            })));
        }

        messages
    }

    /// Encodes a dynamic string with the same lossless numeric/bool preference used by the Agent.
    pub fn encode_dynamic_value(&mut self, value: &str) -> (DynamicValue, Option<StatefulMessage>) {
        if let Some(int_value) = parse_lossless_i64(value) {
            return (
                DynamicValue {
                    value: Some(dynamic_value::Value::IntValue(int_value)),
                    render_as_string: true,
                },
                None,
            );
        }
        if let Some(float_value) = parse_lossless_f64(value) {
            return (
                DynamicValue {
                    value: Some(dynamic_value::Value::FloatValue(float_value)),
                    render_as_string: true,
                },
                None,
            );
        }
        if let Some(bool_value) = parse_lossless_bool(value) {
            return (
                DynamicValue {
                    value: Some(dynamic_value::Value::BoolValue(bool_value)),
                    render_as_string: true,
                },
                None,
            );
        }

        match self.dictionary.observe_dynamic_string(value) {
            DynamicStringEncoding::Dict { id, is_new } => {
                let definition = is_new.then(|| Self::dict_entry_define_message(id, value));
                (
                    DynamicValue {
                        value: Some(dynamic_value::Value::DictIndex(id)),
                        ..DynamicValue::default()
                    },
                    definition,
                )
            }
            DynamicStringEncoding::Inline => (
                DynamicValue {
                    value: Some(dynamic_value::Value::StringValue(value.to_owned())),
                    ..DynamicValue::default()
                },
                None,
            ),
        }
    }

    fn emit_patterned_log(
        &mut self, record: &LogRecord, pattern: ExtractedPattern, service_id: u64, status_id: u64, tags_id: u64,
        messages: &mut Vec<StatefulMessage>,
    ) {
        if pattern.define {
            messages.push(StatefulMessage::new(StatefulDatum::new(Datum {
                data: Some(datum::Data::PatternDefine(PatternDefine {
                    pattern_id: pattern.pattern_id,
                    template: pattern.template.clone(),
                    param_count: pattern.param_count(),
                    pos_list: pattern.pos_list.clone(),
                })),
            })));
        }

        let mut dynamic_values = Vec::with_capacity(pattern.dynamic_values.len());
        for value in &pattern.dynamic_values {
            let (dynamic_value, maybe_definition) = self.encode_dynamic_value(value);
            if let Some(definition) = maybe_definition {
                messages.push(definition);
            }
            dynamic_values.push(dynamic_value);
        }

        messages.push(StatefulMessage::new(StatefulDatum::new(Datum {
            data: Some(datum::Data::FlatLog(FlatLog {
                timestamp: record.timestamp_millis,
                status: flat_log_dict_index(status_id),
                service: flat_log_dict_index(service_id),
                tags: flat_log_dict_index(tags_id),
                pattern_id: pattern.pattern_id,
                dynamic_values,
                json_schema_id: FLAT_LOG_EMPTY_DICT_INDEX,
                uuid: record.uuid.clone(),
                ..FlatLog::default()
            })),
        })));
    }

    fn define_optional_field(&mut self, value: Option<&str>, messages: &mut Vec<StatefulMessage>) -> u64 {
        value.map_or(0, |value| self.define_field(value, messages))
    }

    fn define_field(&mut self, value: &str, messages: &mut Vec<StatefulMessage>) -> u64 {
        if value.is_empty() {
            return 0;
        }

        let (id, is_new) = self.dictionary.add_string(value);
        if is_new {
            messages.push(Self::dict_entry_define_message(id, value));
        }
        id
    }

    fn define_tag_set(&mut self, record: &LogRecord, messages: &mut Vec<StatefulMessage>) -> u64 {
        if let Some(cache) = self.tag_cache.as_ref() {
            if cache.matches(record) && self.dictionary.has_id(cache.dict_id) {
                return cache.dict_id;
            }
        }

        let tag_string = build_tag_string(record);
        if tag_string.is_empty() {
            self.tag_cache = None;
            return 0;
        }

        let (dict_id, is_new) = self.dictionary.add_string(&tag_string);
        if is_new {
            messages.push(Self::dict_entry_define_message(dict_id, &tag_string));
        }
        self.tag_cache = Some(TagCache::new(record, tag_string, dict_id));
        dict_id
    }

    fn dict_entry_define_message(id: u64, value: &str) -> StatefulMessage {
        StatefulMessage::new(StatefulDatum::new(Datum {
            data: Some(datum::Data::DictEntryDefine(DictEntryDefine {
                id,
                value: value.to_owned(),
            })),
        }))
    }
}

#[derive(Clone, Debug, Default)]
struct Dictionary {
    string_to_id: HashMap<String, u64>,
    id_to_string: HashMap<u64, String>,
    pending_dynamic: HashMap<String, u16>,
    next_id: u64,
}

impl Dictionary {
    fn add_string(&mut self, value: &str) -> (u64, bool) {
        if let Some(id) = self.string_to_id.get(value) {
            return (*id, false);
        }

        let id = self.allocate_id();
        self.string_to_id.insert(value.to_owned(), id);
        self.id_to_string.insert(id, value.to_owned());
        self.pending_dynamic.remove(value);
        (id, true)
    }

    fn observe_dynamic_string(&mut self, value: &str) -> DynamicStringEncoding {
        if let Some(id) = self.string_to_id.get(value) {
            return DynamicStringEncoding::Dict { id: *id, is_new: false };
        }
        if !is_dynamic_string_dictionary_candidate(value) {
            return DynamicStringEncoding::Inline;
        }

        let count = match self.pending_dynamic.get_mut(value) {
            Some(count) => {
                if *count < DYNAMIC_STRING_DICTIONARY_THRESHOLD {
                    *count += 1;
                }
                *count
            }
            None => {
                if self.pending_dynamic.len() >= MAX_PENDING_DYNAMIC_STRINGS {
                    return DynamicStringEncoding::Inline;
                }
                self.pending_dynamic.insert(value.to_owned(), 1);
                1
            }
        };

        if count < DYNAMIC_STRING_DICTIONARY_THRESHOLD {
            return DynamicStringEncoding::Inline;
        }

        let (id, _) = self.add_string(value);
        DynamicStringEncoding::Dict { id, is_new: true }
    }

    fn has_id(&self, id: u64) -> bool {
        self.id_to_string.contains_key(&id)
    }

    fn allocate_id(&mut self) -> u64 {
        self.next_id = self.next_id.max(FLAT_LOG_EMPTY_DICT_INDEX) + 1;
        self.next_id
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum DynamicStringEncoding {
    Dict { id: u64, is_new: bool },
    Inline,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TagCache {
    hostname: Option<String>,
    source: Option<String>,
    tags: Vec<String>,
    processing_tags: Vec<String>,
    tag_string: String,
    dict_id: u64,
}

impl TagCache {
    fn new(record: &LogRecord, tag_string: String, dict_id: u64) -> Self {
        Self {
            hostname: record.hostname.clone(),
            source: record.source.clone(),
            tags: record.tags.clone(),
            processing_tags: record.processing_tags.clone(),
            tag_string,
            dict_id,
        }
    }

    fn matches(&self, record: &LogRecord) -> bool {
        self.hostname == record.hostname
            && self.source == record.source
            && self.tags == record.tags
            && self.processing_tags == record.processing_tags
            && self.tag_string == build_tag_string(record)
    }
}

fn build_tag_string(record: &LogRecord) -> String {
    let mut tags = record.tags.clone();
    if let Some(hostname) = non_empty(record.hostname.as_deref()) {
        tags.push(format!("hostname:{hostname}"));
    }
    if let Some(source) = non_empty(record.source.as_deref()) {
        tags.push(format!("ddsource:{source}"));
    }
    tags.join(",")
}

fn flat_log_dict_index(id: u64) -> u64 {
    if id == 0 {
        FLAT_LOG_EMPTY_DICT_INDEX
    } else {
        id
    }
}

fn is_dynamic_string_dictionary_candidate(value: &str) -> bool {
    (MIN_DYNAMIC_STRING_LEN..=MAX_DYNAMIC_STRING_LEN).contains(&value.len())
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.is_empty())
}

fn parse_lossless_i64(value: &str) -> Option<i64> {
    if !could_be_integer(value) {
        return None;
    }

    let parsed = value.parse::<i64>().ok()?;
    (parsed.to_string() == value).then_some(parsed)
}

fn parse_lossless_f64(value: &str) -> Option<f64> {
    if !could_be_float(value) {
        return None;
    }

    let parsed = value.parse::<f64>().ok()?;
    let rendered = serde_json::Number::from_f64(parsed)?.to_string();
    (rendered == value).then_some(parsed)
}

fn parse_lossless_bool(value: &str) -> Option<bool> {
    match value {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

fn could_be_integer(value: &str) -> bool {
    let rest = value.strip_prefix('-').unwrap_or(value);
    !rest.is_empty() && rest.bytes().all(|byte| byte.is_ascii_digit())
}

fn could_be_float(value: &str) -> bool {
    if value.is_empty() || !value.bytes().any(|byte| matches!(byte, b'.' | b'e' | b'E')) {
        return false;
    }

    value.bytes().enumerate().all(|(index, byte)| match byte {
        b'0'..=b'9' | b'.' | b'e' | b'E' => true,
        b'+' | b'-' => index == 0 || matches!(value.as_bytes()[index - 1], b'e' | b'E'),
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use datadog_protos::stateful::{datum, dynamic_value};

    use super::*;

    fn dict_define(message: &StatefulMessage) -> Option<(u64, String)> {
        match message.datum().datum().data.as_ref()? {
            datum::Data::DictEntryDefine(define) => Some((define.id, define.value.clone())),
            _ => None,
        }
    }

    fn flat_log(message: &StatefulMessage) -> &FlatLog {
        match message.datum().datum().data.as_ref().unwrap() {
            datum::Data::FlatLog(log) => log,
            other => panic!("expected FlatLog, got {other:?}"),
        }
    }

    #[test]
    fn translator_emits_dictionary_definitions_before_raw_log() {
        let mut translator = StatefulLogTranslator::new();
        let mut record = LogRecord::new("hello world", 1234);
        record.service = Some("payments".to_owned());
        record.hostname = Some("host-a".to_owned());
        record.source = Some("java".to_owned());
        record.tags = vec!["env:test".to_owned(), "team:core".to_owned()];
        record.uuid = Some("uuid-1".to_owned());

        let messages = translator.translate(&record);

        assert_eq!(messages.len(), 4);
        assert_eq!(dict_define(&messages[0]), Some((2, "payments".to_owned())));
        assert_eq!(dict_define(&messages[1]), Some((3, "info".to_owned())));
        assert_eq!(
            dict_define(&messages[2]),
            Some((4, "env:test,team:core,hostname:host-a,ddsource:java".to_owned()))
        );

        let log = flat_log(&messages[3]);
        assert_eq!(log.timestamp, 1234);
        assert_eq!(log.service, 2);
        assert_eq!(log.status, 3);
        assert_eq!(log.tags, 4);
        assert_eq!(log.raw_log, "hello world");
        assert_eq!(log.json_schema_id, FLAT_LOG_EMPTY_DICT_INDEX);
        assert_eq!(log.uuid.as_deref(), Some("uuid-1"));
    }

    #[test]
    fn translator_reuses_dictionary_state_for_repeated_metadata() {
        let mut translator = StatefulLogTranslator::new();
        let mut first = LogRecord::new("first", 1);
        first.service = Some("payments".to_owned());
        first.status = Some("warn".to_owned());
        first.tags = vec!["env:test".to_owned()];

        let mut second = first.clone();
        second.body = b"second".to_vec();
        second.timestamp_millis = 2;

        assert_eq!(translator.translate(&first).len(), 4);
        let messages = translator.translate(&second);

        assert_eq!(messages.len(), 1);
        let log = flat_log(&messages[0]);
        assert_eq!(log.service, 2);
        assert_eq!(log.status, 3);
        assert_eq!(log.tags, 4);
        assert_eq!(log.raw_log, "second");
    }

    #[test]
    fn translator_replaces_invalid_utf8_before_proto_string_encoding() {
        let mut translator = StatefulLogTranslator::new();
        let record = LogRecord::new(vec![b'o', b'k', 0xff], 7);

        let messages = translator.translate(&record);

        let log = flat_log(messages.last().unwrap());
        assert_eq!(log.raw_log, "ok\u{fffd}");
    }

    #[test]
    fn dynamic_values_preserve_lossless_scalar_strings() {
        let mut translator = StatefulLogTranslator::new();

        let (int_value, definition) = translator.encode_dynamic_value("42");
        assert!(definition.is_none());
        assert_eq!(int_value.value, Some(dynamic_value::Value::IntValue(42)));
        assert!(int_value.render_as_string);

        let (bool_value, definition) = translator.encode_dynamic_value("false");
        assert!(definition.is_none());
        assert_eq!(bool_value.value, Some(dynamic_value::Value::BoolValue(false)));
        assert!(bool_value.render_as_string);
    }

    #[test]
    fn dynamic_strings_are_defined_after_repetition() {
        let mut translator = StatefulLogTranslator::new();

        let (first, definition) = translator.encode_dynamic_value("request-a");
        assert_eq!(
            first.value,
            Some(dynamic_value::Value::StringValue("request-a".to_owned()))
        );
        assert!(definition.is_none());

        let (second, definition) = translator.encode_dynamic_value("request-a");
        assert_eq!(second.value, Some(dynamic_value::Value::DictIndex(2)));
        assert_eq!(dict_define(&definition.unwrap()), Some((2, "request-a".to_owned())));
    }

    #[derive(Clone, Debug, Default)]
    struct StubPatternExtractor;

    impl PatternExtractor for StubPatternExtractor {
        fn extract(&mut self, _body: &str) -> Option<ExtractedPattern> {
            Some(ExtractedPattern {
                pattern_id: 12,
                template: "user {} logged in".to_owned(),
                pos_list: vec![5],
                dynamic_values: vec!["alice".to_owned()],
                define: true,
            })
        }
    }

    #[test]
    fn translator_can_use_pattern_extractor_for_flat_log_patterns() {
        let mut translator = StatefulLogTranslator::with_pattern_extractor(StubPatternExtractor);
        let record = LogRecord::new("user alice logged in", 9);

        let messages = translator.translate(&record);

        assert!(matches!(
            messages[1].datum().datum().data.as_ref(),
            Some(datum::Data::PatternDefine(define)) if define.pattern_id == 12 && define.param_count == 1
        ));
        let log = flat_log(messages.last().unwrap());
        assert_eq!(log.pattern_id, 12);
        assert!(log.raw_log.is_empty());
        assert_eq!(log.dynamic_values.len(), 1);
    }
}
