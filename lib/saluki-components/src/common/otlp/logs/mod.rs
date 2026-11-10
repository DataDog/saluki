//! OTLP logs helpers shared between the OTLP logs path's components.

use std::mem;

use saluki_core::data_model::event::log::Log;
use saluki_error::{generic_error, GenericError};
use serde_json::Value as JsonValue;
use stringtheory::MetaString;

/// The most serialization buffer capacity that a [`ContentRenderer`] keeps between logs.
///
/// A renderer keeps the capacity of the largest content it has written, so one enormous log would otherwise stay held
/// for the renderer's lifetime. The limit sits well above the content of an ordinary log: such a log keeps its reuse,
/// and a log past it has the excess released before the renderer is used again.
pub const CONTENT_BUFFER_RETENTION_LIMIT_BYTES: usize = 64 * 1024;

/// The most field list capacity, in fields, that a [`ContentRenderer`] keeps between logs.
///
/// Like the serialization buffer, the field list keeps the capacity of the log with the most fields, so it's bounded
/// too. The limit sits well above the fields of an ordinary log.
pub const CONTENT_FIELDS_RETENTION_LIMIT: usize = 1024;

/// The memory that the most field list capacity a [`ContentRenderer`] keeps between logs takes.
pub const CONTENT_FIELDS_RETENTION_LIMIT_BYTES: usize =
    CONTENT_FIELDS_RETENTION_LIMIT * mem::size_of::<(&str, ContentField<'_>)>();

/// A field of a log's content.
enum ContentField<'a> {
    String(&'a str),
    Value(&'a JsonValue),
}

/// Renders translated OTLP logs into the content of their log messages.
///
/// The content is the log as a `datadogV2.HTTPLogItem` without its `ddtags`, serialized by `HTTPLogItem.MarshalJSON`.
/// It holds:
///
/// - `hostname` and `service`, when the log has them, and `message`.
/// - Every additional property, overwriting a top-level field of the same name. A `datadog.log.source` property, and a
///   `ddtags` property from a resource attribute, stay in the content.
/// - `status`, the log's status, even when it's empty. It's set after every other property, so it overwrites a
///   `status` property.
///
/// The content is serialized with `serde_json`, with its keys sorted by their bytes, as Go sorts map keys. Its escaping
/// and number formatting differ from Go's `encoding/json` (`gap/json-encoding`): `<`, `>`, `&`, U+2028, and U+2029
/// aren't escaped, and floats use `serde_json`'s layout, such as `1.0` and `1e21` where Go writes `1` and `1e+21`.
///
/// A renderer keeps its serialization buffer between logs, up to [`CONTENT_BUFFER_RETENTION_LIMIT_BYTES`], and its
/// field list, up to [`CONTENT_FIELDS_RETENTION_LIMIT`], so rendering many logs reuses their allocations.
#[derive(Default)]
pub struct ContentRenderer {
    buffer: Vec<u8>,
    // Always empty between logs. It borrows nothing, and only holds the allocation of the field list.
    fields: Vec<(&'static str, ContentField<'static>)>,
}

impl std::fmt::Debug for ContentRenderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContentRenderer")
            .field("buffer_capacity", &self.buffer.capacity())
            .field("fields_capacity", &self.fields.capacity())
            .finish()
    }
}

impl ContentRenderer {
    /// Renders a log's content.
    ///
    /// # Errors
    ///
    /// If a property can't be serialized, an error is returned, and the log should be dropped.
    pub fn render(&mut self, log: &Log) -> Result<MetaString, GenericError> {
        let content = self.write_content(log).and_then(|()| {
            simdutf8::basic::from_utf8(&self.buffer)
                .map(MetaString::from)
                .map_err(|e| generic_error!("Rendered log content isn't valid UTF-8: {}", e))
        });

        if self.buffer.capacity() > CONTENT_BUFFER_RETENTION_LIMIT_BYTES {
            self.buffer.clear();
            self.buffer.shrink_to(CONTENT_BUFFER_RETENTION_LIMIT_BYTES);
        }

        content
    }

    /// Writes a log's content into the buffer.
    fn write_content(&mut self, log: &Log) -> Result<(), GenericError> {
        // The fields with each key once. `HTTPLogItem.MarshalJSON` writes the top-level fields, then every property,
        // overwriting a top-level field of the same name, and the status is set after every property. So a property
        // wins over `hostname`, `message`, and `service`, and `status` wins over a property. Properties already have
        // unique keys.
        let properties = log.additional_properties();
        let mut fields = reuse_empty(mem::take(&mut self.fields));
        fields.reserve(properties.len() + 4);
        let (mut hostname_overwritten, mut message_overwritten, mut service_overwritten) = (false, false, false);
        for (key, value) in properties {
            match key.as_ref() {
                "status" => continue,
                "hostname" => hostname_overwritten = true,
                "message" => message_overwritten = true,
                "service" => service_overwritten = true,
                _ => {}
            }
            fields.push((key.as_ref(), ContentField::Value(value)));
        }
        if !hostname_overwritten && !log.hostname().is_empty() {
            fields.push(("hostname", ContentField::String(log.hostname())));
        }
        if !message_overwritten {
            fields.push(("message", ContentField::String(log.message())));
        }
        if !service_overwritten && !log.service().is_empty() {
            fields.push(("service", ContentField::String(log.service())));
        }
        fields.push(("status", ContentField::String(log.status())));

        // Go sorts map keys by their bytes, as `str` orders them. Every key is unique, so an unstable sort, which
        // doesn't allocate, gives the same order as a stable one.
        fields.sort_unstable_by_key(|(key, _)| *key);

        let written = self.write_fields(&fields);

        fields.clear();
        fields.shrink_to(CONTENT_FIELDS_RETENTION_LIMIT);
        self.fields = reuse_empty(fields);
        written
    }

    /// Writes a log's fields into the buffer, as a JSON object.
    fn write_fields(&mut self, fields: &[(&str, ContentField<'_>)]) -> Result<(), GenericError> {
        self.buffer.clear();
        self.buffer.push(b'{');
        let mut first = true;
        for (key, field) in fields {
            if !first {
                self.buffer.push(b',');
            }
            first = false;

            let buffer = &mut self.buffer;
            let written = serde_json::to_writer(&mut *buffer, key).and_then(|()| {
                buffer.push(b':');
                match field {
                    ContentField::String(value) => serde_json::to_writer(&mut *buffer, value),
                    ContentField::Value(value) => serde_json::to_writer(&mut *buffer, value),
                }
            });
            written.map_err(|e| generic_error!("Failed to serialize log property '{}': {}", key, e))?;
        }
        self.buffer.push(b'}');
        Ok(())
    }
}

/// Returns an empty vector with the allocation of `fields`, for fields that borrow for a different lifetime.
///
/// The element types differ only in their lifetimes, so collecting the empty vector's iterator reuses its allocation in
/// place.
fn reuse_empty<'a, 'b>(mut fields: Vec<(&'a str, ContentField<'a>)>) -> Vec<(&'b str, ContentField<'b>)> {
    fields.clear();
    fields
        .into_iter()
        .map(|_| unreachable!("the fields are empty"))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use saluki_core::data_model::event::log::Log;
    use saluki_core::data_model::tags::{Tag, TagSet};
    use serde_json::{json, Value as JsonValue};
    use stringtheory::MetaString;

    use super::{ContentRenderer, CONTENT_BUFFER_RETENTION_LIMIT_BYTES, CONTENT_FIELDS_RETENTION_LIMIT};

    /// A render case: a translated log, and the content that Go writes for it, which is the target.
    ///
    /// The logs are written for these tests, each to exercise a rule of the content's serialization. Each target is
    /// what `datadogV2.HTTPLogItem.MarshalJSON` wrote for the log, with `datadog-api-client-go` v2.66.0 and Go 1.26.9:
    /// the item holds the hostname, service, message, and properties, then `status` is set as the last property, and
    /// the item has no `ddtags`.
    struct RenderCase {
        name: &'static str,
        log: fn() -> Log,
        target: &'static str,
        gap: Option<RenderGap>,
    }

    /// Why a case's current content differs from its target, and the current content, which the test checks exactly.
    /// Remove it in the change that fixes the gap.
    ///
    /// `gap/json-encoding`: Go escapes `<`, `>`, `&`, U+2028, and U+2029, and writes whole-number floats without a
    /// fraction and floats under 1e21 in decimal, where `serde_json` doesn't escape them and writes `3.0` and `1e+20`.
    struct RenderGap {
        gap: &'static str,
        current: &'static str,
    }

    fn log_with_properties(message: &str, properties: JsonValue) -> Log {
        let properties = match properties {
            JsonValue::Object(properties) => properties
                .into_iter()
                .map(|(key, value)| (MetaString::from(key), value))
                .collect::<HashMap<_, _>>(),
            other => panic!("properties must be a JSON object, got {}", other),
        };
        Log::new(message).with_additional_properties(properties)
    }

    fn translated_log(message: &str, status: &str, properties: JsonValue) -> Log {
        log_with_properties(message, properties).with_status(status)
    }

    const RENDER_CASES: &[RenderCase] = &[
        RenderCase {
            name: "minimal",
            log: || translated_log("", "", json!({})),
            target: r#"{"message":"","status":""}"#,
            gap: None,
        },
        RenderCase {
            name: "message-and-status",
            log: || translated_log("hello", "debug", json!({})),
            target: r#"{"message":"hello","status":"debug"}"#,
            gap: None,
        },
        RenderCase {
            name: "hostname-and-service",
            log: || {
                translated_log("hello", "info", json!({}))
                    .with_hostname(MetaString::from_static("host-a"))
                    .with_service(MetaString::from_static("svc"))
            },
            target: r#"{"hostname":"host-a","message":"hello","service":"svc","status":"info"}"#,
            gap: None,
        },
        RenderCase {
            name: "properties",
            log: || {
                translated_log(
                    "hello",
                    "warn",
                    json!({
                        "app": "test",
                        "otel.severity_number": "13",
                        "@timestamp": "2023-11-20T16:55:03.397Z",
                        "dd.trace_id": "167772160",
                        "Zebra": "upper case sorts first",
                        "a.b.c": "dotted",
                    }),
                )
            },
            target: r#"{"@timestamp":"2023-11-20T16:55:03.397Z","Zebra":"upper case sorts first","a.b.c":"dotted","app":"test","dd.trace_id":"167772160","message":"hello","otel.severity_number":"13","status":"warn"}"#,
            gap: None,
        },
        // Properties overwrite the top-level fields of the item.
        RenderCase {
            name: "collision-message",
            log: || translated_log("from body", "info", json!({"message": "from resource"})),
            target: r#"{"message":"from resource","status":"info"}"#,
            gap: None,
        },
        RenderCase {
            name: "collision-hostname",
            log: || {
                translated_log("m", "info", json!({"hostname": "from attribute"}))
                    .with_hostname(MetaString::from_static("host-a"))
            },
            target: r#"{"hostname":"from attribute","message":"m","status":"info"}"#,
            gap: None,
        },
        RenderCase {
            name: "collision-service",
            log: || {
                translated_log("m", "info", json!({"service": "from attribute"}))
                    .with_service(MetaString::from_static("svc"))
            },
            target: r#"{"message":"m","service":"from attribute","status":"info"}"#,
            gap: None,
        },
        // The status is set after every other property, so it overwrites a `status` resource attribute.
        RenderCase {
            name: "collision-status",
            log: || translated_log("m", "error", json!({"status": "from resource"})),
            target: r#"{"message":"m","status":"error"}"#,
            gap: None,
        },
        // Only the item's own tags are removed; a `ddtags` property stays in the content.
        RenderCase {
            name: "collision-ddtags",
            log: || translated_log("m", "info", json!({"ddtags": "from:resource"})),
            target: r#"{"ddtags":"from:resource","message":"m","status":"info"}"#,
            gap: None,
        },
        RenderCase {
            name: "collision-log-source",
            log: || translated_log("m", "info", json!({"datadog.log.source": "custom_source"})),
            target: r#"{"datadog.log.source":"custom_source","message":"m","status":"info"}"#,
            gap: None,
        },
        RenderCase {
            name: "empty-status",
            log: || translated_log("m", "", json!({"k": "v"})).with_hostname(MetaString::from_static("host-a")),
            target: r#"{"hostname":"host-a","k":"v","message":"m","status":""}"#,
            gap: None,
        },
        RenderCase {
            name: "html-characters",
            log: || translated_log(r#"<a href="x">&amp;</a>"#, "info", json!({"<key>": "a & b > c"})),
            target: r#"{"\u003ckey\u003e":"a \u0026 b \u003e c","message":"\u003ca href=\"x\"\u003e\u0026amp;\u003c/a\u003e","status":"info"}"#,
            gap: Some(RenderGap {
                gap: "gap/json-encoding",
                current: r#"{"<key>":"a & b > c","message":"<a href=\"x\">&amp;</a>","status":"info"}"#,
            }),
        },
        RenderCase {
            name: "line-separators",
            log: || translated_log("one\u{2028}two\u{2029}three", "info", json!({})),
            target: r#"{"message":"one\u2028two\u2029three","status":"info"}"#,
            gap: Some(RenderGap {
                gap: "gap/json-encoding",
                current: "{\"message\":\"one\u{2028}two\u{2029}three\",\"status\":\"info\"}",
            }),
        },
        RenderCase {
            name: "control-characters",
            log: || {
                translated_log(
                    "tab\tnew\nline\rcr\0nul\x1fus\x7fdel\"quote\\backslash/slash",
                    "info",
                    json!({}),
                )
            },
            // DEL isn't escaped.
            target: "{\"message\":\"tab\\tnew\\nline\\rcr\\u0000nul\\u001fus\x7fdel\\\"quote\\\\backslash/slash\",\"status\":\"info\"}",
            gap: None,
        },
        RenderCase {
            name: "non-ascii",
            log: || translated_log("café ☕ 日本語 😀", "info", json!({"ключ": "значение"})),
            target: r#"{"message":"café ☕ 日本語 😀","status":"info","ключ":"значение"}"#,
            gap: None,
        },
        RenderCase {
            name: "numbers",
            log: || {
                translated_log(
                    "m",
                    "info",
                    json!({
                        "int": 1_i64,
                        "negative": -42_i64,
                        "int64-min": i64::MIN,
                        "int64-max": i64::MAX,
                        "zero-float": 0.0_f64,
                        "integral": 3.0_f64,
                        "fraction": 0.1_f64,
                        "third": 1.0_f64 / 3.0,
                        "small": 1e-7_f64,
                        "small-edge": 1e-6_f64,
                        "large": 1e21_f64,
                        "large-edge": 1e20_f64,
                        "max-float": f64::MAX,
                        "min-positive": 5e-324_f64,
                        "bool": true,
                        "null": null,
                    }),
                )
            },
            target: r#"{"bool":true,"fraction":0.1,"int":1,"int64-max":9223372036854775807,"int64-min":-9223372036854775808,"integral":3,"large":1e+21,"large-edge":100000000000000000000,"max-float":1.7976931348623157e+308,"message":"m","min-positive":5e-324,"negative":-42,"null":null,"small":1e-7,"small-edge":0.000001,"status":"info","third":0.3333333333333333,"zero-float":0}"#,
            gap: Some(RenderGap {
                gap: "gap/json-encoding",
                current: r#"{"bool":true,"fraction":0.1,"int":1,"int64-max":9223372036854775807,"int64-min":-9223372036854775808,"integral":3.0,"large":1e+21,"large-edge":1e+20,"max-float":1.7976931348623157e+308,"message":"m","min-positive":5e-324,"negative":-42,"null":null,"small":1e-7,"small-edge":1e-6,"status":"info","third":0.3333333333333333,"zero-float":0.0}"#,
            }),
        },
        RenderCase {
            name: "nested",
            log: || {
                translated_log(
                    "m",
                    "debug",
                    json!({
                        "array": ["value1", 2_i64, 3.5_f64, false, null],
                        "array_with_maps": [
                            {"item_name": "item1", "value": 100_i64},
                            {"value": 200_i64, "item_name": "<item2>"},
                        ],
                        "matrix": [[{"b": 1_i64, "a": 0_i64}], []],
                        "empty": {},
                    }),
                )
            },
            target: r#"{"array":["value1",2,3.5,false,null],"array_with_maps":[{"item_name":"item1","value":100},{"item_name":"\u003citem2\u003e","value":200}],"empty":{},"matrix":[[{"a":0,"b":1}],[]],"message":"m","status":"debug"}"#,
            gap: Some(RenderGap {
                gap: "gap/json-encoding",
                current: r#"{"array":["value1",2,3.5,false,null],"array_with_maps":[{"item_name":"item1","value":100},{"item_name":"<item2>","value":200}],"empty":{},"matrix":[[{"a":0,"b":1}],[]],"message":"m","status":"debug"}"#,
            }),
        },
    ];

    #[test]
    fn matches_the_reference_byte_for_byte_except_known_gaps() {
        // One renderer for every case, as the transform uses one for every log.
        let mut renderer = ContentRenderer::default();
        let mut failures = Vec::new();
        for case in RENDER_CASES {
            let content = renderer
                .render(&(case.log)())
                .unwrap_or_else(|e| panic!("{}: render failed: {}", case.name, e));
            let content = &*content;
            match &case.gap {
                None if content != case.target => failures.push(format!(
                    "{}: content differs from the target with no known gap\n  target:  {}\n  current: {}",
                    case.name, case.target, content
                )),
                Some(gap) if content == case.target => failures.push(format!(
                    "{}: content now matches the target; remove its entry for {}",
                    case.name, gap.gap
                )),
                Some(gap) if content != gap.current => failures.push(format!(
                    "{}: current content changed\n  expected: {}\n  current:  {}",
                    case.name, gap.current, content
                )),
                _ => {}
            }
        }
        assert!(failures.is_empty(), "render case failures:\n{}", failures.join("\n"));
    }

    #[test]
    fn properties_overwrite_top_level_fields_except_status() {
        let log = log_with_properties(
            "from body",
            json!({
                "message": "from resource",
                "hostname": "from attribute",
                "service": "from attribute",
                "status": "from resource",
                "ddtags": "from:resource",
                "datadog.log.source": "custom_source",
            }),
        )
        .with_hostname(MetaString::from_static("host-a"))
        .with_service(MetaString::from_static("svc"))
        .with_status("error");

        let content: JsonValue =
            serde_json::from_str(&ContentRenderer::default().render(&log).expect("log should render"))
                .expect("content should be JSON");
        assert_eq!(
            content,
            json!({
                "message": "from resource",
                "hostname": "from attribute",
                "service": "from attribute",
                "status": "error",
                "ddtags": "from:resource",
                "datadog.log.source": "custom_source",
            })
        );
    }

    #[test]
    fn leaves_out_empty_hostname_and_service_but_not_status() {
        let content = ContentRenderer::default()
            .render(&Log::new("m"))
            .expect("log should render");
        assert_eq!(&*content, r#"{"message":"m","status":""}"#);
    }

    #[test]
    fn leaves_the_tags_out_of_the_content() {
        let log = Log::new("m").with_tags([Tag::from_static("env:prod")].into_iter().collect::<TagSet>());
        let content = ContentRenderer::default().render(&log).expect("log should render");
        assert_eq!(&*content, r#"{"message":"m","status":""}"#);
    }

    #[test]
    fn reuses_its_buffer_without_leaking_earlier_content() {
        let mut renderer = ContentRenderer::default();
        let long = log_with_properties("first", json!({"padding": "x".repeat(1024)}));
        renderer.render(&long).expect("log should render");

        let content = renderer.render(&Log::new("second")).expect("log should render");
        assert_eq!(&*content, r#"{"message":"second","status":""}"#);
    }

    #[test]
    fn releases_the_excess_of_an_oversized_buffer() {
        let mut renderer = ContentRenderer::default();
        let ordinary = log_with_properties("m", json!({"padding": "x".repeat(1024)}));
        renderer.render(&ordinary).expect("log should render");
        let ordinary_capacity = renderer.buffer.capacity();
        assert!(ordinary_capacity <= CONTENT_BUFFER_RETENTION_LIMIT_BYTES);

        // An ordinary log keeps the buffer it grew.
        renderer.render(&Log::new("m")).expect("log should render");
        assert_eq!(renderer.buffer.capacity(), ordinary_capacity);

        // An oversized log renders in full, then has the excess released.
        let oversized = "x".repeat(2 * CONTENT_BUFFER_RETENTION_LIMIT_BYTES);
        let content = renderer
            .render(&Log::new(oversized.as_str()))
            .expect("log should render");
        assert!(content.contains(&oversized));
        assert!(renderer.buffer.capacity() <= CONTENT_BUFFER_RETENTION_LIMIT_BYTES);
    }

    #[test]
    fn reuses_and_bounds_the_field_list() {
        let properties =
            |count: usize| JsonValue::Object((0..count).map(|i| (format!("key{:05}", i), json!(i))).collect());
        let mut renderer = ContentRenderer::default();
        renderer
            .render(&log_with_properties("m", properties(100)))
            .expect("log should render");
        let ordinary_capacity = renderer.fields.capacity();
        assert!(ordinary_capacity >= 104);

        // A log with as many fields reuses the field list.
        renderer
            .render(&log_with_properties("m", properties(100)))
            .expect("log should render");
        assert_eq!(renderer.fields.capacity(), ordinary_capacity);

        // A log with more fields than the limit renders in full, then has the excess released.
        let content = renderer
            .render(&log_with_properties(
                "m",
                properties(2 * CONTENT_FIELDS_RETENTION_LIMIT),
            ))
            .expect("log should render");
        assert!(content.contains(&format!(r#""key{:05}":"#, 2 * CONTENT_FIELDS_RETENTION_LIMIT - 1)));
        assert!(renderer.fields.capacity() <= CONTENT_FIELDS_RETENTION_LIMIT);
    }
}
