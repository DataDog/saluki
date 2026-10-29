use std::collections::{BTreeMap, BTreeSet};

use saluki_error::{generic_error, GenericError};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Top-level keys that the logs intake reads as envelope fields when `DD-PROTOCOL: agent-json` is set.
pub const LOG_ENVELOPE_FIELDS: &[&str] = &[
    "message",
    "status",
    "timestamp",
    "hostname",
    "service",
    "ddsource",
    "ddtags",
];

/// A single logs intake request, as captured by the fake intake.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LogsRequest {
    path: String,
    headers: BTreeMap<String, String>,
    logs: Vec<Log>,
}

impl LogsRequest {
    /// Creates a new `LogsRequest`.
    ///
    /// Header names are lowercased. When a header appears more than once, its values are joined with `, `.
    pub fn new<'a, I>(path: impl Into<String>, headers: I, logs: Vec<Log>) -> Self
    where
        I: IntoIterator<Item = (&'a str, &'a str)>,
    {
        let mut header_map = BTreeMap::<String, String>::new();
        for (name, value) in headers {
            header_map
                .entry(name.to_ascii_lowercase())
                .and_modify(|existing| {
                    existing.push_str(", ");
                    existing.push_str(value);
                })
                .or_insert_with(|| value.to_string());
        }

        Self {
            path: path.into(),
            headers: header_map,
            logs,
        }
    }

    /// Returns the request path.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Returns the request headers, keyed by lowercased name.
    pub fn headers(&self) -> &BTreeMap<String, String> {
        &self.headers
    }

    /// Returns the value of the given header, matched case-insensitively.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(&name.to_ascii_lowercase()).map(String::as_str)
    }

    /// Returns the logs carried by the request.
    pub fn logs(&self) -> &[Log] {
        &self.logs
    }
}

/// A simplified log representation that keeps track of where each field was found.
///
/// A log on the wire is a JSON object with a handful of envelope fields (`message`, `status`, `hostname`, `service`,
/// `ddsource`, `timestamp`, and `ddtags`), optionally other top-level keys, and a `message` that may itself be a JSON
/// object serialized as a string. The Agent puts all log properties inside that string, while other producers may put
/// them at the top level. Keeping the two locations apart means that a property that moved from one to the other shows
/// up as a difference, rather than being normalized away.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Log {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    message: Option<Value>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    status: Option<Value>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    hostname: Option<Value>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    service: Option<Value>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    ddsource: Option<Value>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    timestamp: Option<Value>,
    ddtags: BTreeSet<String>,
    envelope_properties: BTreeMap<String, Value>,
    content_properties: Option<BTreeMap<String, Value>>,
}

impl Log {
    /// Parses a decompressed logs intake payload, which is a JSON array of log objects.
    ///
    /// # Errors
    ///
    /// If the payload isn't valid JSON, isn't an array of objects, or has a `ddtags` field that isn't a string, an
    /// error is returned.
    pub fn try_from_payload(payload: &[u8]) -> Result<Vec<Self>, GenericError> {
        let items = serde_json::from_slice::<Vec<Value>>(payload)
            .map_err(|e| generic_error!("Failed to parse logs payload as a JSON array: {}", e))?;

        items
            .into_iter()
            .enumerate()
            .map(|(idx, item)| match item {
                Value::Object(object) => Self::try_from_object(object.into_iter().collect()),
                other => Err(generic_error!(
                    "Log at index {} is not a JSON object: {}",
                    idx,
                    value_kind(&other)
                )),
            })
            .collect()
    }

    fn try_from_object(mut object: BTreeMap<String, Value>) -> Result<Self, GenericError> {
        let ddtags = match object.remove("ddtags") {
            None | Some(Value::Null) => BTreeSet::new(),
            Some(Value::String(raw)) => split_ddtags(&raw),
            Some(other) => {
                return Err(generic_error!(
                    "Log `ddtags` field is not a string: {}",
                    value_kind(&other)
                ))
            }
        };

        let message = object.remove("message");
        let content_properties = match &message {
            Some(Value::String(content)) => match serde_json::from_str::<Value>(content) {
                Ok(Value::Object(content_object)) => Some(content_object.into_iter().collect()),
                _ => None,
            },
            _ => None,
        };

        Ok(Self {
            message,
            status: object.remove("status"),
            hostname: object.remove("hostname"),
            service: object.remove("service"),
            ddsource: object.remove("ddsource"),
            timestamp: object.remove("timestamp"),
            ddtags,
            envelope_properties: object,
            content_properties,
        })
    }

    /// Returns the raw envelope `message` field.
    pub fn message(&self) -> Option<&Value> {
        self.message.as_ref()
    }

    /// Returns the raw envelope `status` field.
    pub fn status(&self) -> Option<&Value> {
        self.status.as_ref()
    }

    /// Returns the raw envelope `hostname` field.
    pub fn hostname(&self) -> Option<&Value> {
        self.hostname.as_ref()
    }

    /// Returns the raw envelope `service` field.
    pub fn service(&self) -> Option<&Value> {
        self.service.as_ref()
    }

    /// Returns the raw envelope `ddsource` field.
    pub fn ddsource(&self) -> Option<&Value> {
        self.ddsource.as_ref()
    }

    /// Returns the raw envelope `timestamp` field.
    pub fn timestamp(&self) -> Option<&Value> {
        self.timestamp.as_ref()
    }

    /// Returns the envelope `ddtags` field as a set.
    ///
    /// Like the logs intake, the raw string is split on `,`, each element is trimmed, and empty elements and duplicates
    /// are dropped.
    pub fn ddtags(&self) -> &BTreeSet<String> {
        &self.ddtags
    }

    /// Returns top-level keys other than the envelope fields.
    pub fn envelope_properties(&self) -> &BTreeMap<String, Value> {
        &self.envelope_properties
    }

    /// Returns the properties parsed from the envelope `message`, if it's a string holding a JSON object.
    pub fn content_properties(&self) -> Option<&BTreeMap<String, Value>> {
        self.content_properties.as_ref()
    }

    /// Returns the envelope field with the given name, if it's one of the fields held as a raw value.
    ///
    /// `ddtags` isn't held as a raw value, so it isn't returned here; use [`Log::ddtags`] instead.
    pub fn envelope_field(&self, name: &str) -> Option<&Value> {
        match name {
            "message" => self.message(),
            "status" => self.status(),
            "hostname" => self.hostname(),
            "service" => self.service(),
            "ddsource" => self.ddsource(),
            "timestamp" => self.timestamp(),
            _ => None,
        }
    }

    /// Returns the displayed message: the content's `message` when the content is a JSON object, or else the envelope
    /// `message`.
    pub fn display_message(&self) -> Option<&Value> {
        match &self.content_properties {
            Some(content) => content.get("message"),
            None => self.message.as_ref(),
        }
    }

    /// Returns a location-independent view of the log, used to identify the same log on both sides of a comparison.
    ///
    /// Envelope fields and other top-level keys come first, then content properties overwrite them. When the content
    /// is a JSON object, the raw envelope `message` is left out, since its properties are already present. `ddtags`
    /// is included as a sorted array.
    pub fn semantic_view(&self) -> BTreeMap<String, Value> {
        let mut view = BTreeMap::new();

        for name in LOG_ENVELOPE_FIELDS {
            if *name == "message" && self.content_properties.is_some() {
                continue;
            }
            if let Some(value) = self.envelope_field(name) {
                view.insert(name.to_string(), value.clone());
            }
        }
        if !self.ddtags.is_empty() {
            let tags = self.ddtags.iter().cloned().map(Value::String).collect();
            view.insert("ddtags".to_string(), Value::Array(tags));
        }

        view.extend(self.envelope_properties.iter().map(|(k, v)| (k.clone(), v.clone())));
        if let Some(content) = &self.content_properties {
            view.extend(content.iter().map(|(k, v)| (k.clone(), v.clone())));
        }

        view
    }
}

/// Deserializes a field that is present as `Some`, even when its value is `null`.
///
/// Serde reads a `null` into `None` for an `Option`, which would lose the difference between an envelope field that's
/// absent and one that's explicitly `null`. Absent fields get `None` from `#[serde(default)]`, and `None` isn't
/// serialized at all, so both states survive a round trip.
fn deserialize_present<'de, D>(deserializer: D) -> Result<Option<Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Value::deserialize(deserializer).map(Some)
}

fn split_ddtags(raw: &str) -> BTreeSet<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
        .map(str::to_string)
        .collect()
}

fn value_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn props(value: Value) -> BTreeMap<String, Value> {
        match value {
            Value::Object(object) => object.into_iter().collect(),
            _ => panic!("test properties must be a JSON object"),
        }
    }

    #[test]
    fn agent_payload_keeps_properties_in_content() {
        // Shaped like the Agent's logs agent output for an OTLP log: every property lives inside `message`.
        let payload = br#"[{
            "message": "{\"@timestamp\":\"2026-10-29T10:00:00.000Z\",\"hostname\":\"host-a\",\"message\":\"hello\",\"otel.severity_number\":\"9\",\"service\":\"svc\",\"status\":\"info\"}",
            "status": "info",
            "timestamp": 1793268000123,
            "hostname": "host-a",
            "service": "svc",
            "ddsource": "otlp_log_ingestion",
            "ddtags": " env:prod ,,otel_source:datadog_agent,env:prod,"
        }]"#;

        let logs = Log::try_from_payload(payload).expect("payload should parse");
        assert_eq!(logs.len(), 1);
        let log = &logs[0];

        assert_eq!(log.status(), Some(&json!("info")));
        assert_eq!(log.timestamp(), Some(&json!(1793268000123u64)));
        assert_eq!(log.hostname(), Some(&json!("host-a")));
        assert_eq!(log.service(), Some(&json!("svc")));
        assert_eq!(log.ddsource(), Some(&json!("otlp_log_ingestion")));
        assert_eq!(
            log.ddtags().iter().map(String::as_str).collect::<Vec<_>>(),
            vec!["env:prod", "otel_source:datadog_agent"]
        );
        assert!(log.envelope_properties().is_empty());
        assert_eq!(
            log.content_properties(),
            Some(&props(json!({
                "@timestamp": "2026-10-29T10:00:00.000Z",
                "hostname": "host-a",
                "message": "hello",
                "otel.severity_number": "9",
                "service": "svc",
                "status": "info",
            })))
        );
        assert_eq!(log.display_message(), Some(&json!("hello")));
    }

    #[test]
    fn adp_payload_keeps_properties_at_top_level() {
        // Shaped like ADP's current logs encoder output: properties are top-level keys.
        let payload = br#"[{
            "message": "{\"message\":\"hello\",\"service\":\"svc\"}",
            "status": "Info",
            "hostname": "host-a",
            "service": "svc",
            "ddsource": "otlp_log_ingestion",
            "ddtags": "otel_source:datadog_agent",
            "@timestamp": "2026-10-29T10:00:00.000Z",
            "otel.severity_number": "9",
            "nested": {"key": [1, 2]}
        }]"#;

        let logs = Log::try_from_payload(payload).expect("payload should parse");
        let log = &logs[0];

        assert_eq!(log.timestamp(), None);
        assert_eq!(
            log.envelope_properties(),
            &props(json!({
                "@timestamp": "2026-10-29T10:00:00.000Z",
                "otel.severity_number": "9",
                "nested": {"key": [1, 2]},
            }))
        );
        assert_eq!(
            log.content_properties(),
            Some(&props(json!({"message": "hello", "service": "svc"})))
        );
    }

    #[test]
    fn semantic_view_merges_locations_with_content_winning() {
        let payload = br#"[{
            "message": "{\"message\":\"hello\",\"shared\":\"from-content\"}",
            "status": "info",
            "ddtags": "b:2,a:1",
            "shared": "from-envelope",
            "top": 1
        }]"#;

        let logs = Log::try_from_payload(payload).expect("payload should parse");
        assert_eq!(
            logs[0].semantic_view(),
            props(json!({
                "ddtags": ["a:1", "b:2"],
                "message": "hello",
                "shared": "from-content",
                "status": "info",
                "top": 1,
            }))
        );
    }

    #[test]
    fn non_object_message_is_not_parsed_as_content() {
        let payload = br#"[
            {"message": "plain text"},
            {"message": "[1,2,3]"},
            {"message": "{not json"},
            {"message": 42},
            {}
        ]"#;

        let logs = Log::try_from_payload(payload).expect("payload should parse");
        assert_eq!(logs.len(), 5);
        for log in &logs {
            assert_eq!(log.content_properties(), None);
        }
        assert_eq!(logs[0].display_message(), Some(&json!("plain text")));
        assert_eq!(logs[1].display_message(), Some(&json!("[1,2,3]")));
        assert_eq!(logs[3].display_message(), Some(&json!(42)));
        assert_eq!(logs[4].display_message(), None);

        // Without parsed content, the raw message stays in the semantic view.
        assert_eq!(logs[0].semantic_view(), props(json!({"message": "plain text"})));
    }

    #[test]
    fn duplicate_keys_keep_the_last_value() {
        let payload = br#"[{"message": "{\"k\":1,\"k\":2}", "status": "warn", "status": "error"}]"#;

        let logs = Log::try_from_payload(payload).expect("payload should parse");
        assert_eq!(logs[0].status(), Some(&json!("error")));
        assert_eq!(logs[0].content_properties(), Some(&props(json!({"k": 2}))));
    }

    #[test]
    fn invalid_payloads_are_rejected() {
        let cases: &[(&str, &[u8])] = &[
            ("invalid JSON", b"[{"),
            ("not an array", br#"{"message": "x"}"#),
            ("array of non-objects", br#"["x"]"#),
            ("non-string ddtags", br#"[{"ddtags": ["a"]}]"#),
        ];

        for (name, payload) in cases {
            assert!(Log::try_from_payload(payload).is_err(), "{} should be rejected", name);
        }
    }

    #[test]
    fn empty_ddtags_and_null_ddtags_are_empty_sets() {
        let payload = br#"[{"ddtags": ""}, {"ddtags": " , ,"}, {"ddtags": null}]"#;

        let logs = Log::try_from_payload(payload).expect("payload should parse");
        for log in &logs {
            assert!(log.ddtags().is_empty());
        }
    }

    #[test]
    fn logs_request_normalizes_header_names() {
        let request = LogsRequest::new(
            "/api/v2/logs",
            [("DD-PROTOCOL", "agent-json"), ("X-Multi", "a"), ("x-multi", "b")],
            Vec::new(),
        );

        assert_eq!(request.path(), "/api/v2/logs");
        assert_eq!(request.header("dd-protocol"), Some("agent-json"));
        assert_eq!(request.header("Dd-Protocol"), Some("agent-json"));
        assert_eq!(request.header("x-multi"), Some("a, b"));
        assert!(request.logs().is_empty());
    }

    #[test]
    fn logs_request_round_trips_through_json() {
        let logs =
            Log::try_from_payload(br#"[{"message": "{\"a\":1}", "ddtags": "x", "extra": true}, {"message": null}]"#)
                .expect("payload should parse");
        // An explicit `null` is kept apart from an absent field.
        assert_eq!(logs[1].message(), Some(&Value::Null));
        assert_eq!(logs[1].status(), None);
        let request = LogsRequest::new("/v1/input", [("content-encoding", "zstd")], logs);

        let encoded = serde_json::to_string(&request).expect("request should serialize");
        let decoded: LogsRequest = serde_json::from_str(&encoded).expect("request should deserialize");
        assert_eq!(decoded, request);
        assert_eq!(decoded.logs()[1].message(), Some(&Value::Null));
        assert_eq!(decoded.logs()[1].status(), None);
    }
}
