//! A small model of how the logs intake parses a log, used to tell which differences users would notice.
//!
//! The model covers what matters for comparing Agent and ADP payloads: which top-level keys the parser keeps, how the
//! JSON content in `message` becomes attributes, which attributes the default remappers read to set the date, host,
//! service, and status, and that `ddtags` is a set. It doesn't model the rest of log processing.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};
use stele::{Log, LOG_ENVELOPE_FIELDS};

/// Attributes read by the default date remapper, in precedence order.
const DATE_SOURCES: &[&str] = &["@timestamp", "timestamp", "date"];

/// Attributes read by the default host remapper, in precedence order.
const HOST_SOURCES: &[&str] = &["host", "hostname"];

/// Attributes read by the default service remapper, in precedence order.
const SERVICE_SOURCES: &[&str] = &["service", "dd.service"];

/// Attributes read by the default status remapper, in precedence order.
const STATUS_SOURCES: &[&str] = &["status", "severity", "level"];

/// Status used when an attribute status can't be mapped, and when the envelope has none.
const DEFAULT_STATUS: &str = "info";

/// Whether users would notice a difference after intake processing.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Visibility {
    /// The stored log differs.
    IntakeVisible,

    /// Only the payload differs; the intake stores the same log for both sides.
    WireOnly,
}

impl Visibility {
    /// Returns a short label for reports.
    pub const fn label(self) -> &'static str {
        match self {
            Self::IntakeVisible => "intake-visible",
            Self::WireOnly => "wire-only",
        }
    }
}

/// The log as the intake would store it.
#[derive(Debug, Default, Eq, PartialEq)]
pub struct IntakeLog {
    /// The displayed message.
    pub message: Option<Value>,

    /// The date, as the raw value it was taken from.
    pub date: Option<Value>,

    /// The host.
    pub host: Option<Value>,

    /// The service.
    pub service: Option<Value>,

    /// The normalized status.
    pub status: String,

    /// The source.
    pub ddsource: Option<Value>,

    /// The tags.
    pub ddtags: BTreeSet<String>,

    /// Every other attribute, with dotted keys expanded into nested objects.
    pub attributes: Map<String, Value>,

    /// Properties consumed to fill the stored fields above.
    consumed: BTreeMap<String, &'static str>,

    /// Whether the log was parsed with `DD-PROTOCOL: agent-json`.
    agent_json: bool,
}

impl IntakeLog {
    /// Models how the intake parses `log`.
    ///
    /// With `DD-PROTOCOL: agent-json` (`agent_json`), only the envelope fields are read, and the JSON content in
    /// `message` becomes the attributes. Without it, every top-level key is read and the JSON content is merged over
    /// them, `ddtags` and `ddsource` are taken from the merged object, and the remaining envelope fields are attributes
    /// like any other.
    pub fn from_log(log: &Log, agent_json: bool) -> Self {
        let content = log.content_properties().cloned();
        let mut intake = Self {
            ddtags: log.ddtags().clone(),
            agent_json,
            ..Self::default()
        };
        let envelope_status;

        let mut attributes = if agent_json {
            intake.date = log.timestamp().cloned();
            intake.host = log.hostname().cloned();
            intake.service = log.service().cloned();
            intake.ddsource = log.ddsource().cloned();
            envelope_status = log.status().cloned();
            content.unwrap_or_default()
        } else {
            let mut merged = log.envelope_properties().clone();
            for field in LOG_ENVELOPE_FIELDS {
                if let Some(value) = log.envelope_field(field) {
                    merged.insert(field.to_string(), value.clone());
                }
            }
            // The message is stored separately, whether it's plain or parsed content.
            merged.remove("message");
            if let Some(content) = content {
                merged.extend(content);
            }
            // Inner `ddtags` are merged as a raw string, and replace the envelope's. Every other envelope field is an
            // ordinary attribute here, which the remappers below read.
            if let Some(Value::String(raw)) = merged.remove("ddtags") {
                intake.ddtags = split_ddtags(&raw);
            }
            intake.ddsource = merged.remove("ddsource");
            intake.consumed.insert("ddtags".to_string(), "ddtags");
            intake.consumed.insert("ddsource".to_string(), "ddsource");
            envelope_status = None;
            merged
        };

        intake.message = match log.content_properties() {
            Some(_) => attributes.remove("message"),
            None => log.message().cloned(),
        };

        // The default remappers overwrite envelope values with the first matching attribute, which they consume.
        if let Some(date) = intake.take_attribute(&mut attributes, DATE_SOURCES, "date") {
            intake.date = Some(date);
        }
        if let Some(host) = intake.take_attribute(&mut attributes, HOST_SOURCES, "host") {
            intake.host = Some(host);
        }
        if let Some(service) = intake.take_attribute(&mut attributes, SERVICE_SOURCES, "service") {
            intake.service = Some(service);
        }
        intake.status = match intake.take_attribute(&mut attributes, STATUS_SOURCES, "status") {
            Some(status) => normalize_status(&status).to_string(),
            // The envelope status is stored as it is.
            None => match envelope_status {
                Some(Value::String(status)) if !status.is_empty() => status,
                _ => DEFAULT_STATUS.to_string(),
            },
        };

        attributes.retain(|_, value| !value.is_null());
        intake.attributes = expand_dotted_keys(attributes);
        intake
    }

    /// Removes and returns the first non-null attribute among `sources`, recording that it fills `field`.
    fn take_attribute(
        &mut self, attributes: &mut BTreeMap<String, Value>, sources: &[&str], field: &'static str,
    ) -> Option<Value> {
        let source = sources
            .iter()
            .find(|source| attributes.get(**source).is_some_and(|value| !value.is_null()))?;
        self.consumed.insert(source.to_string(), field);
        attributes.remove(*source)
    }

    fn stored_field(&self, field: &str) -> Value {
        match field {
            "message" => self.message.clone().unwrap_or(Value::Null),
            "date" | "timestamp" => self.date.clone().unwrap_or(Value::Null),
            "host" | "hostname" => self.host.clone().unwrap_or(Value::Null),
            "service" => self.service.clone().unwrap_or(Value::Null),
            "status" => Value::String(self.status.clone()),
            "ddsource" => self.ddsource.clone().unwrap_or(Value::Null),
            "ddtags" => Value::Array(self.ddtags.iter().cloned().map(Value::String).collect()),
            _ => Value::Null,
        }
    }

    /// Returns where the field at `path` ends up: a stored field, or an attribute.
    ///
    /// `path` is a location-qualified diff path. With agent-json, envelope fields map to the stored field they feed.
    /// Otherwise they're ordinary attributes, except `message`, `ddtags`, and `ddsource`. A property maps to the stored
    /// field it was consumed into, if any, and otherwise to the attribute at the same path. Paths below a field, such
    /// as `content.message.k`, are resolved inside the value that field ends up in.
    fn destination<'p>(&self, path: &'p str) -> Destination<'p> {
        if let Some(field) = path.strip_prefix("envelope.") {
            let (name, rest) = split_first_segment(field);
            if self.agent_json || matches!(name, "message" | "ddtags" | "ddsource") {
                return Destination::Stored {
                    field: stored_field_name(name),
                    rest,
                    property: None,
                };
            }
            return self.property_destination(field);
        }

        let key = path
            .strip_prefix("content.")
            .or_else(|| path.strip_prefix("envelope_properties."))
            .unwrap_or(path);
        self.property_destination(key)
    }

    fn property_destination<'p>(&self, key: &'p str) -> Destination<'p> {
        // The property key may itself contain dots, so find the longest consumed property the path starts with.
        let owner = std::iter::once(("message", "message"))
            .chain(self.consumed.iter().map(|(name, field)| (name.as_str(), *field)))
            .filter_map(|(name, field)| path_below(key, name).map(|rest| (name.len(), field, rest)))
            .max_by_key(|(len, _, _)| *len);
        match owner {
            Some((_, field, rest)) => Destination::Stored {
                field,
                rest,
                property: Some(key),
            },
            None => Destination::Attribute(key),
        }
    }

    fn stored_value(&self, field: &str, rest: &str) -> Value {
        walk(&self.stored_field(field), rest)
    }

    fn attribute_value(&self, key: &str) -> Value {
        lookup(&self.attributes, key)
    }
}

/// Where a field ends up in the stored log.
enum Destination<'p> {
    /// A stored field, such as the message or the status, and the path inside it. `property` is the property path
    /// that fed it, if the field came from a property.
    Stored {
        field: &'static str,
        rest: &'p str,
        property: Option<&'p str>,
    },

    /// An attribute at the given path.
    Attribute(&'p str),
}

fn stored_field_name(name: &str) -> &'static str {
    match name {
        "message" => "message",
        "status" => "status",
        "hostname" => "host",
        "service" => "service",
        "ddsource" => "ddsource",
        "timestamp" => "date",
        "ddtags" => "ddtags",
        _ => "",
    }
}

/// Returns the rest of `path` below `name`, if `path` is `name` itself or a path inside it.
fn path_below<'a>(path: &'a str, name: &str) -> Option<&'a str> {
    let rest = path.strip_prefix(name)?;
    (rest.is_empty() || rest.starts_with('.') || rest.starts_with('[')).then_some(rest)
}

/// Splits a path into its first `.`-separated segment, without array indexes, and the rest.
fn split_first_segment(path: &str) -> (&str, &str) {
    let end = path.find(['.', '[']).unwrap_or(path.len());
    (&path[..end], &path[end..])
}

/// Returns the value at `rest` inside `value`, where `rest` is empty, or starts with `.key` or `[index]`.
fn walk(value: &Value, rest: &str) -> Value {
    let mut current = Some(value);
    let mut rest = rest;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix('[') {
            let end = after.find(']').unwrap_or(after.len());
            let index = after[..end].parse::<usize>().ok();
            current = current
                .and_then(Value::as_array)
                .zip(index)
                .and_then(|(array, index)| array.get(index));
            rest = after.get(end + 1..).unwrap_or("");
        } else {
            let after = rest.strip_prefix('.').unwrap_or(rest);
            let (name, remaining) = split_first_segment(after);
            current = current.and_then(Value::as_object).and_then(|object| object.get(name));
            rest = remaining;
        }
        if current.is_none() {
            return Value::Null;
        }
    }
    current.cloned().unwrap_or(Value::Null)
}

/// Classifies a difference at `path` by comparing what the intake would store for each side.
pub fn classify(path: &str, baseline: &IntakeLog, comparison: &IntakeLog) -> Visibility {
    // Both sides are compared at the same destination: if either side stores the field, compare that stored field on
    // both, and also the attribute, which only a side that didn't consume the property keeps.
    // The sides can resolve differently, for example when only one was parsed with agent-json, so keep the stored
    // field and the attribute from whichever side has them.
    let destinations = [baseline.destination(path), comparison.destination(path)];
    let stored = destinations.iter().find_map(|destination| match destination {
        Destination::Stored { field, rest, .. } => Some((*field, *rest)),
        Destination::Attribute(_) => None,
    });
    let attribute = destinations.iter().find_map(|destination| match destination {
        Destination::Stored { property, .. } => *property,
        Destination::Attribute(key) => Some(*key),
    });

    let stored_differs =
        stored.is_some_and(|(field, rest)| baseline.stored_value(field, rest) != comparison.stored_value(field, rest));
    let attribute_differs =
        attribute.is_some_and(|key| baseline.attribute_value(key) != comparison.attribute_value(key));
    if stored_differs || attribute_differs {
        Visibility::IntakeVisible
    } else {
        Visibility::WireOnly
    }
}

/// Expands dotted attribute keys into nested objects, as the intake does.
///
/// Keys are applied in sorted order, so when a scalar and an object land on the same key, the later one wins. The intake
/// applies them in wire order, which `stele::Log` doesn't keep, so such collisions are only approximated.
fn expand_dotted_keys(attributes: BTreeMap<String, Value>) -> Map<String, Value> {
    let mut expanded = Map::new();
    for (key, value) in attributes {
        let mut segments = key.split('.').peekable();
        let mut target = &mut expanded;
        while let Some(segment) = segments.next() {
            if segments.peek().is_none() {
                target.insert(segment.to_string(), value);
                break;
            }
            let entry = target
                .entry(segment.to_string())
                .or_insert_with(|| Value::Object(Map::new()));
            if !entry.is_object() {
                *entry = Value::Object(Map::new());
            }
            target = entry.as_object_mut().expect("entry was just made an object");
        }
    }
    expanded
}

/// Returns the value at a diff path inside expanded attributes, such as `attr.list[1].k`.
fn lookup(attributes: &Map<String, Value>, path: &str) -> Value {
    let mut current = None::<&Value>;
    for segment in path.split('.') {
        let object = match current {
            None => Some(attributes),
            Some(value) => value.as_object(),
        };
        // An attribute name may contain brackets itself, so try the literal name before reading array indexes.
        if let Some(value) = object.and_then(|object| object.get(segment)) {
            current = Some(value);
            continue;
        }
        let (name, indexes) = match segment.find('[') {
            Some(start) => (&segment[..start], &segment[start..]),
            None => (segment, ""),
        };
        current = object.and_then(|object| object.get(name));
        for index in indexes.split('[').filter(|index| !index.is_empty()) {
            let index = index.trim_end_matches(']').parse::<usize>().ok();
            current = current
                .and_then(Value::as_array)
                .zip(index)
                .and_then(|(array, index)| array.get(index));
        }
        if current.is_none() {
            return Value::Null;
        }
    }
    current.cloned().unwrap_or(Value::Null)
}

/// Normalizes an attribute status like the intake's status remapper: on its first character for strings, and by
/// syslog severity for numbers. Values that can't be mapped become `info`.
fn normalize_status(value: &Value) -> &'static str {
    match value {
        Value::Number(number) => match number.as_i64() {
            Some(0) => "emergency",
            Some(1) => "alert",
            Some(2) => "critical",
            Some(3) => "error",
            Some(4) => "warn",
            Some(5) => "notice",
            Some(6) => "info",
            Some(7) => "debug",
            Some(8) => "ok",
            _ => DEFAULT_STATUS,
        },
        Value::String(status) => {
            let lower = status.to_ascii_lowercase();
            match lower.chars().next() {
                Some('i') => "info",
                Some('e') if lower == "e" || lower.starts_with("err") => "error",
                Some('e') if lower.starts_with("emerg") => "emergency",
                Some('w') => "warn",
                Some('d' | 't' | 'v') => "debug",
                Some('o' | 's') => "ok",
                Some('f') => "emergency",
                Some('a') => "alert",
                Some('c') => "critical",
                Some('n') => "notice",
                _ => DEFAULT_STATUS,
            }
        }
        _ => DEFAULT_STATUS,
    }
}

fn split_ddtags(raw: &str) -> BTreeSet<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn log(value: Value) -> Log {
        let payload = Value::Array(vec![value]).to_string();
        Log::try_from_payload(payload.as_bytes())
            .expect("test log should parse")
            .remove(0)
    }

    #[test]
    fn agent_json_drops_top_level_properties() {
        let log = log(json!({
            "message": "{\"message\":\"hello\",\"attr\":1}",
            "status": "warn",
            "top": "dropped",
        }));

        let intake = IntakeLog::from_log(&log, true);
        assert_eq!(intake.message, Some(json!("hello")));
        assert_eq!(Value::Object(intake.attributes), json!({"attr": 1}));
        // No status attribute, so the envelope status is stored as it is.
        assert_eq!(intake.status, "warn");

        let default = IntakeLog::from_log(&log, false);
        assert_eq!(default.attributes.get("top"), Some(&json!("dropped")));
        assert_eq!(default.attributes.get("attr"), Some(&json!(1)));
        // Without agent-json, the envelope status is an attribute and gets normalized.
        assert_eq!(
            IntakeLog::from_log(&self::log(json!({"message": "m", "status": "Warning"})), false).status,
            "warn"
        );
    }

    #[test]
    fn inner_fields_take_precedence_over_the_envelope() {
        let log = log(json!({
            "message": "{\"message\":\"inner\",\"hostname\":\"inner-host\",\"service\":\"inner-svc\",\"ddtags\":\"inner:1\"}",
            "hostname": "outer-host",
            "service": "outer-svc",
            "ddtags": "outer:1",
        }));

        let agent_json = IntakeLog::from_log(&log, true);
        assert_eq!(agent_json.message, Some(json!("inner")));
        assert_eq!(agent_json.host, Some(json!("inner-host")));
        assert_eq!(agent_json.service, Some(json!("inner-svc")));
        // With agent-json, inner `ddtags` stay a plain attribute.
        assert_eq!(agent_json.ddtags, BTreeSet::from(["outer:1".to_string()]));
        assert_eq!(agent_json.attributes.get("ddtags"), Some(&json!("inner:1")));
        // The remapped sources are consumed.
        assert_eq!(agent_json.attributes.get("hostname"), None);

        let default = IntakeLog::from_log(&log, false);
        assert_eq!(default.host, Some(json!("inner-host")));
        assert_eq!(default.ddtags, BTreeSet::from(["inner:1".to_string()]));
    }

    #[test]
    fn attribute_statuses_are_normalized_and_override_the_envelope() {
        let cases = [
            (json!("Error"), "error"),
            (json!("e"), "error"),
            (json!("emerg"), "emergency"),
            (json!("eh"), "info"),
            (json!("WARNING"), "warn"),
            (json!("trace"), "debug"),
            (json!("fatal"), "emergency"),
            (json!(""), "info"),
            (json!("?"), "info"),
            (json!(3), "error"),
            (json!(9), "info"),
        ];

        for (status, expected) in cases {
            let content = json!({"message": "m", "status": status}).to_string();
            let log = log(json!({"message": content, "status": "critical"}));
            assert_eq!(IntakeLog::from_log(&log, true).status, expected, "{}", status);
        }

        // Without any status, it defaults to info.
        assert_eq!(IntakeLog::from_log(&log(json!({"message": "m"})), true).status, "info");
    }

    #[test]
    fn a_property_moved_to_the_top_level_is_wire_only_without_agent_json() {
        // The Agent keeps properties in the content, while ADP currently puts them at the top level and sends no
        // `DD-PROTOCOL` header, so the default parser merges both into the same attributes.
        let agent = log(json!({"message": "{\"message\":\"a\",\"attr\":1}", "status": "info"}));
        let adp = log(json!({"message": "{\"message\":\"a\"}", "status": "info", "attr": 1}));

        let agent_view = IntakeLog::from_log(&agent, true);
        assert_eq!(
            classify("content.attr", &agent_view, &IntakeLog::from_log(&adp, false)),
            Visibility::WireOnly
        );
        // With agent-json, the moved property would be dropped.
        assert_eq!(
            classify("content.attr", &agent_view, &IntakeLog::from_log(&adp, true)),
            Visibility::IntakeVisible
        );
    }

    #[test]
    fn envelope_differences_hidden_by_remappers_are_wire_only() {
        // The envelope status differs, but both sides have the same content status, which wins.
        let baseline = log(json!({"message": "{\"message\":\"a\",\"status\":\"warn\"}", "status": "warn"}));
        let comparison = log(json!({"message": "{\"message\":\"a\",\"status\":\"warn\"}", "status": "Warning"}));
        let (baseline, comparison) = (
            IntakeLog::from_log(&baseline, true),
            IntakeLog::from_log(&comparison, true),
        );
        assert_eq!(
            classify("envelope.status", &baseline, &comparison),
            Visibility::WireOnly
        );

        // So are content statuses that normalize to the same value.
        let baseline = log(json!({"message": "{\"message\":\"a\",\"status\":\"warn\"}"}));
        let comparison = log(json!({"message": "{\"message\":\"a\",\"status\":\"Warning\"}"}));
        let (baseline, comparison) = (
            IntakeLog::from_log(&baseline, true),
            IntakeLog::from_log(&comparison, true),
        );
        assert_eq!(classify("content.status", &baseline, &comparison), Visibility::WireOnly);

        // A differing content value is visible.
        let baseline = log(json!({"message": "{\"message\":\"a\",\"attr\":{\"k\":1}}"}));
        let comparison = log(json!({"message": "{\"message\":\"a\",\"attr\":{\"k\":2}}"}));
        let (baseline, comparison) = (
            IntakeLog::from_log(&baseline, true),
            IntakeLog::from_log(&comparison, true),
        );
        assert_eq!(
            classify("content.attr.k", &baseline, &comparison),
            Visibility::IntakeVisible
        );

        // Dotted keys are expanded, so they're equivalent to nested objects.
        let baseline = log(json!({"message": "{\"message\":\"a\",\"attr.k\":1,\"list\":[{\"x\":1}]}"}));
        let comparison = log(json!({"message": "{\"message\":\"a\",\"attr\":{\"k\":1},\"list\":[{\"x\":2}]}"}));
        let (baseline, comparison) = (
            IntakeLog::from_log(&baseline, true),
            IntakeLog::from_log(&comparison, true),
        );
        assert_eq!(classify("content.attr.k", &baseline, &comparison), Visibility::WireOnly);
        assert_eq!(classify("content.attr", &baseline, &comparison), Visibility::WireOnly);
        assert_eq!(
            classify("content.list[0].x", &baseline, &comparison),
            Visibility::IntakeVisible
        );

        // A lower-precedence remapper source isn't consumed, so it stays a visible attribute.
        let baseline = log(json!({"message": "{\"message\":\"a\",\"host\":\"h\",\"hostname\":\"x\"}"}));
        let comparison = log(json!({"message": "{\"message\":\"a\",\"host\":\"h\",\"hostname\":\"y\"}"}));
        let (baseline, comparison) = (
            IntakeLog::from_log(&baseline, true),
            IntakeLog::from_log(&comparison, true),
        );
        assert_eq!(classify("content.host", &baseline, &comparison), Visibility::WireOnly);
        assert_eq!(
            classify("content.hostname", &baseline, &comparison),
            Visibility::IntakeVisible
        );

        // Without agent-json, an envelope field is an ordinary attribute, so a differing envelope `hostname` stays
        // visible when the content's `host` fills the stored host instead.
        let baseline = log(json!({"message": "{\"message\":\"a\",\"host\":\"h\"}", "hostname": "x"}));
        let comparison = log(json!({"message": "{\"message\":\"a\",\"host\":\"h\"}", "hostname": "y"}));
        let (default_baseline, default_comparison) = (
            IntakeLog::from_log(&baseline, false),
            IntakeLog::from_log(&comparison, false),
        );
        assert_eq!(
            classify("envelope.hostname", &default_baseline, &default_comparison),
            Visibility::IntakeVisible
        );
        // With agent-json, the envelope hostname is overridden by the content's `host`.
        let (baseline, comparison) = (
            IntakeLog::from_log(&baseline, true),
            IntakeLog::from_log(&comparison, true),
        );
        assert_eq!(
            classify("envelope.hostname", &baseline, &comparison),
            Visibility::WireOnly
        );

        // A difference inside a structured message is visible through the stored message.
        let baseline = log(json!({"message": "{\"message\":{\"k\":1}}"}));
        let comparison = log(json!({"message": "{\"message\":{\"k\":2}}"}));
        let (baseline, comparison) = (
            IntakeLog::from_log(&baseline, true),
            IntakeLog::from_log(&comparison, true),
        );
        assert_eq!(
            classify("content.message.k", &baseline, &comparison),
            Visibility::IntakeVisible
        );

        // Removing a redundant remapper source changes nothing stored.
        let baseline = log(json!({"message": "{\"message\":\"m\",\"status\":\"warn\"}", "status": "warn"}));
        let comparison = log(json!({"message": "{\"message\":\"m\"}", "status": "warn"}));
        let (baseline, comparison) = (
            IntakeLog::from_log(&baseline, true),
            IntakeLog::from_log(&comparison, true),
        );
        assert_eq!(classify("content.status", &baseline, &comparison), Visibility::WireOnly);

        // A dotted attribute name with brackets is a literal nested key, not an array index.
        let baseline = log(json!({"message": "{\"message\":\"m\",\"http.header[x]\":1}"}));
        let comparison = log(json!({"message": "{\"message\":\"m\",\"http.header[x]\":2}"}));
        let (baseline, comparison) = (
            IntakeLog::from_log(&baseline, true),
            IntakeLog::from_log(&comparison, true),
        );
        assert_eq!(
            classify("content.http.header[x]", &baseline, &comparison),
            Visibility::IntakeVisible
        );

        // With mixed parser modes, the attribute kept by the default parser still counts.
        let baseline = log(json!({"message": "{\"message\":\"a\",\"host\":\"h\"}", "hostname": "x"}));
        let comparison = log(json!({"message": "{\"message\":\"a\",\"host\":\"h\"}", "hostname": "y"}));
        let (baseline, comparison) = (
            IntakeLog::from_log(&baseline, true),
            IntakeLog::from_log(&comparison, false),
        );
        assert_eq!(
            classify("envelope.hostname", &baseline, &comparison),
            Visibility::IntakeVisible
        );

        // A plain message under the default parser is stored as the message, not kept as an attribute too.
        let baseline = IntakeLog::from_log(&log(json!({"message": "{\"message\":\"m\"}"})), true);
        let comparison = IntakeLog::from_log(&log(json!({"message": "m"})), false);
        assert!(comparison.attributes.is_empty(), "{:?}", comparison.attributes);
        assert_eq!(
            classify("content.message", &baseline, &comparison),
            Visibility::WireOnly
        );

        // Tags are a set, so their order and whitespace aren't visible.
        let baseline = IntakeLog::from_log(&log(json!({"message": "a", "ddtags": "a:1,b:2"})), true);
        let comparison = IntakeLog::from_log(&log(json!({"message": "a", "ddtags": "b:2 , a:1,"})), true);
        assert_eq!(
            classify("envelope.ddtags", &baseline, &comparison),
            Visibility::WireOnly
        );
    }
}
