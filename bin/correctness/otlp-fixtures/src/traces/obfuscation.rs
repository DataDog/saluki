//! The fixture of the `otlp-traces-obfuscation` case: one probe for each value that an implementation scrubs, or leaves
//! as it is.
//!
//! Every probe is a span of a service of its own, `probe-obf-<variant>`, and the only span of its trace. The service
//! names the probe: no scrubbed field can hide it, as the resource of a database span does. The values that a case
//! expects scrubbed are synthetic: `CANARY` stands for a secret, and `CARD` for a card number that no issuer holds.

use otlp_protos::opentelemetry::proto::{
    common::v1::KeyValue,
    trace::v1::span::{Event, SpanKind},
};

use super::{probe_request, ProbeResource, ProbeSpan, BASE_TIME_NANOS};
use crate::common::{boolean, double, int, kv, string};

/// The seed of the traces of `obfuscation_fixture`: trace `n`, counted from 1, has the seed `OBFUSCATION_SEED + n`.
const OBFUSCATION_SEED: u32 = 0x0c00;

/// A synthetic secret. A case expects it scrubbed from every field that it names.
pub const CANARY: &str = "canary-7f3a";

/// A synthetic card number: sixteen digits that start with the Visa prefix, and that pass the Luhn check. No issuer
/// holds it.
pub const CARD: &str = "4111111111111111";

/// The OTLP name of a span of a database. The service is the identity of a probe, so the names repeat.
const DATABASE_NAME: &str = "otlpprobe.obf.database";

/// The OTLP name of a span of an HTTP client.
const HTTP_NAME: &str = "otlpprobe.obf.http";

/// The OTLP name of a span with attributes that look like a card number.
const CARD_NAME: &str = "otlpprobe.obf.card";

/// A probe of a database: a `CLIENT` span with the string attributes `attributes`.
fn database(service: &'static str, attributes: &[(&str, &str)]) -> Vec<ProbeSpan> {
    vec![ProbeSpan::new(service, DATABASE_NAME, SpanKind::Client).with_strings(attributes)]
}

/// A probe of a database, with the legacy attributes `db.system` and `db.statement`.
fn legacy_database(service: &'static str, system: &str, statement: &str) -> Vec<ProbeSpan> {
    database(service, &[("db.system", system), ("db.statement", statement)])
}

/// A probe of a database, with the current attributes `db.system.name` and `db.query.text`.
fn current_database(service: &'static str, system: &str, query: &str) -> Vec<ProbeSpan> {
    database(service, &[("db.system.name", system), ("db.query.text", query)])
}

/// A probe of an HTTP client with the string attributes `attributes`.
fn http(service: &'static str, attributes: &[(&str, &str)]) -> Vec<ProbeSpan> {
    vec![ProbeSpan::new(service, HTTP_NAME, SpanKind::Client).with_strings(attributes)]
}

/// A probe with attributes that look like a card number: an `INTERNAL` span with the attributes `attributes`.
fn card(service: &'static str, attributes: Vec<KeyValue>) -> ProbeSpan {
    attributes.into_iter().fold(
        ProbeSpan::new(service, CARD_NAME, SpanKind::Internal),
        |span, attribute| span.with_attribute(&attribute.key, attribute.value.unwrap()),
    )
}

/// A probe with the string attributes `attributes`, one for each of its values that looks like a card number.
fn card_strings(service: &'static str, attributes: &[(&str, &str)]) -> Vec<ProbeSpan> {
    vec![card(
        service,
        attributes.iter().map(|(key, value)| kv(key, string(value))).collect(),
    )]
}

/// The probes of SQL, in the legacy and in the current attributes.
fn sql_traces() -> Vec<Vec<ProbeSpan>> {
    vec![
        legacy_database(
            "probe-obf-sql-simple",
            "postgresql",
            &format!("SELECT id FROM obf_simple WHERE token = '{CANARY}' AND n = 42"),
        ),
        legacy_database(
            "probe-obf-sql-in-list",
            "postgresql",
            &format!("SELECT id FROM obf_in_list WHERE id IN (1, 2, 3) AND tag IN ('{CANARY}', 'b')"),
        ),
        legacy_database(
            "probe-obf-sql-comment",
            "postgresql",
            &format!("/* {CANARY} */ SELECT id FROM obf_comment WHERE n = 7 -- {CANARY}"),
        ),
        legacy_database(
            "probe-obf-sql-dollar",
            "postgresql",
            &format!("SELECT $${CANARY}$$ FROM obf_dollar"),
        ),
        legacy_database(
            "probe-obf-sql-insert",
            "postgresql",
            &format!("INSERT INTO obf_insert (id, token) VALUES (1, '{CANARY}')"),
        ),
        legacy_database(
            "probe-obf-sql-unterminated",
            "postgresql",
            &format!("SELECT '{CANARY} FROM obf_unterminated"),
        ),
        legacy_database(
            "probe-obf-sql-mysql",
            "mysql",
            &format!("SELECT id FROM obf_mysql WHERE token = \"{CANARY}\" AND n = 7"),
        ),
        current_database(
            "probe-obf-sql-current-simple",
            "postgresql",
            &format!("SELECT id FROM obf_current_simple WHERE token = '{CANARY}' AND n = 42"),
        ),
        current_database(
            "probe-obf-sql-current-in-list",
            "postgresql",
            &format!("SELECT id FROM obf_current_in_list WHERE id IN (1, 2, 3) AND tag IN ('{CANARY}', 'b')"),
        ),
        // The system of one convention, and the query of the other.
        database(
            "probe-obf-sql-mixed-legacy-system",
            &[
                ("db.system", "postgresql"),
                (
                    "db.query.text",
                    &format!("SELECT id FROM obf_mixed_legacy_system WHERE token = '{CANARY}'"),
                ),
            ],
        ),
        database(
            "probe-obf-sql-mixed-current-system",
            &[
                ("db.system.name", "postgresql"),
                (
                    "db.statement",
                    &format!("SELECT id FROM obf_mixed_current_system WHERE token = '{CANARY}'"),
                ),
            ],
        ),
        // Both attributes, with different queries.
        database(
            "probe-obf-sql-both-attributes",
            &[
                ("db.system", "postgresql"),
                (
                    "db.statement",
                    &format!("SELECT id FROM obf_both_attributes WHERE token = '{CANARY}'"),
                ),
                (
                    "db.query.text",
                    &format!("SELECT id FROM obf_both_attributes_text WHERE token = '{CANARY}'"),
                ),
            ],
        ),
        // The query that the tracer sent as `sql.query` too.
        database(
            "probe-obf-sql-query-tag",
            &[
                ("db.system", "postgresql"),
                (
                    "db.statement",
                    &format!("SELECT id FROM obf_query_tag WHERE token = '{CANARY}'"),
                ),
                (
                    "sql.query",
                    &format!("SELECT id FROM obf_query_tag WHERE token = '{CANARY}'"),
                ),
            ],
        ),
    ]
}

/// The probes of the commands of a cache.
fn cache_traces() -> Vec<Vec<ProbeSpan>> {
    vec![
        legacy_database("probe-obf-redis-statement", "redis", &format!("SET obf_key {CANARY}")),
        current_database("probe-obf-redis-query-text", "redis", &format!("SET obf_key {CANARY}")),
        database(
            "probe-obf-redis-raw-command",
            &[
                ("db.system", "redis"),
                ("db.statement", "SET obf_key ?"),
                ("redis.raw_command", &format!("SET obf_key {CANARY}")),
            ],
        ),
        database(
            "probe-obf-redis-many-commands",
            &[
                ("db.system", "redis"),
                (
                    "db.statement",
                    &format!("SET obf_a {CANARY}\nGET obf_a\nDEL obf_a\nGET obf_b"),
                ),
                (
                    "redis.raw_command",
                    &format!("SET obf_a {CANARY}\nGET obf_a\nDEL obf_a\nGET obf_b"),
                ),
            ],
        ),
        // `db.system` has no value for Valkey that both implementations map to the span type, so the probe sets the type.
        database(
            "probe-obf-valkey-raw-command",
            &[
                ("span.type", "valkey"),
                ("db.system", "valkey"),
                ("db.statement", "SET obf_key ?"),
                ("valkey.raw_command", &format!("SET obf_key {CANARY}")),
            ],
        ),
        database(
            "probe-obf-memcached-command",
            &[
                ("db.system", "memcached"),
                ("db.statement", &format!("set obf_key 0 0 {CANARY}")),
                ("memcached.command", &format!("set obf_key 0 0 {CANARY}")),
            ],
        ),
    ]
}

/// The probes of the JSON bodies of document stores.
fn json_traces() -> Vec<Vec<ProbeSpan>> {
    vec![
        database(
            "probe-obf-mongodb-query",
            &[
                ("db.system", "mongodb"),
                (
                    "mongodb.query",
                    &format!(r#"{{"find": "obf_mongo", "filter": {{"token": "{CANARY}", "n": 5}}}}"#),
                ),
            ],
        ),
        database(
            "probe-obf-mongodb-malformed",
            &[
                ("db.system", "mongodb"),
                (
                    "mongodb.query",
                    &format!(r#"{{"find": "obf_mongo_malformed", "filter": {{"token": "{CANARY}""#),
                ),
            ],
        ),
        database(
            "probe-obf-elasticsearch-body",
            &[
                ("db.system", "elasticsearch"),
                (
                    "elasticsearch.body",
                    &format!(r#"{{"query": {{"match": {{"token": "{CANARY}", "n": 5}}}}}}"#),
                ),
            ],
        ),
        database(
            "probe-obf-elasticsearch-malformed",
            &[
                ("db.system", "elasticsearch"),
                (
                    "elasticsearch.body",
                    &format!(r#"{{"query": {{"match": {{"token": "{CANARY}""#),
                ),
            ],
        ),
        database(
            "probe-obf-opensearch-body",
            &[
                ("db.system", "opensearch"),
                (
                    "opensearch.body",
                    &format!(r#"{{"query": {{"match": {{"token": "{CANARY}", "n": 5}}}}}}"#),
                ),
            ],
        ),
        database(
            "probe-obf-opensearch-malformed",
            &[
                ("db.system", "opensearch"),
                (
                    "opensearch.body",
                    &format!(r#"{{"query": {{"match": {{"token": "{CANARY}""#),
                ),
            ],
        ),
    ]
}

/// The probes of URLs.
fn url_traces() -> Vec<Vec<ProbeSpan>> {
    vec![
        http(
            "probe-obf-url-full-userinfo",
            &[(
                "url.full",
                &format!("https://user:{CANARY}@example.com/obf/full?page=2"),
            )],
        ),
        http(
            "probe-obf-url-legacy-userinfo",
            &[(
                "http.url",
                &format!("https://user:{CANARY}@example.com/obf/legacy?page=2"),
            )],
        ),
        // A query string is left as it is unless `http.remove_query_string` is on, so this one holds no secret.
        http(
            "probe-obf-url-query-string",
            &[("url.full", "https://example.com/obf/query?page=2&sort=name")],
        ),
        // The path digits are left as they are unless `http.remove_paths_with_digits` is on.
        http(
            "probe-obf-url-path-digits",
            &[("url.full", "https://example.com/obf/path/42/items")],
        ),
    ]
}

/// The event of a span that has the string attribute `key`.
fn event(name: &str, attributes: Vec<KeyValue>) -> Event {
    Event {
        time_unix_nano: BASE_TIME_NANOS + 1_000_000,
        name: name.to_string(),
        attributes,
        dropped_attributes_count: 0,
    }
}

/// The probes of values that look like a card number.
fn card_traces() -> Vec<Vec<ProbeSpan>> {
    vec![
        card_strings("probe-obf-card-sixteen", &[("payment.card", CARD)]),
        card_strings("probe-obf-card-spaced", &[("payment.card", "4111 1111 1111 1111")]),
        card_strings("probe-obf-card-dashed", &[("payment.card", "4111-1111-1111-1111")]),
        // The Luhn check is off by default, so a number that fails it is scrubbed too.
        card_strings("probe-obf-card-invalid-luhn", &[("payment.card", "4111111111111112")]),
        // A number with more than sixteen digits, or fewer than twelve, is not a card number to the Agent.
        card_strings("probe-obf-card-seventeen", &[("payment.card", "41111111111111111")]),
        card_strings("probe-obf-card-nineteen", &[("payment.card", "4111111111111111111")]),
        card_strings("probe-obf-card-eleven", &[("payment.card", "41111111111")]),
        card_strings("probe-obf-card-twelve", &[("payment.card", "411111111111")]),
        // A number inside text is not a card number: the check needs a value of digits, spaces and dashes only.
        card_strings(
            "probe-obf-card-in-text",
            &[("payment.note", &format!("card {CARD} declined"))],
        ),
        // An integer attribute, and a boolean that no card number can be.
        vec![card(
            "probe-obf-card-integer",
            vec![
                kv("payment.card", int(4_111_111_111_111_111)),
                kv("payment.valid", boolean(true)),
            ],
        )],
        vec![card(
            "probe-obf-card-double",
            vec![kv("payment.card", double(4_111_111_111_111_111.0))],
        )],
        // The keys that a card scan skips.
        card_strings(
            "probe-obf-card-exempt-keys",
            &[
                ("aws.account.id", "411111111111"),
                ("aws.target.account.id", "411111111111"),
                ("cloud.account.id", "411111111111"),
                ("account_id", "411111111111"),
                ("_obf_hidden", CARD),
            ],
        ),
        // A card in a span event, and a card in a span that has an event with none.
        vec![
            card("probe-obf-card-event", vec![kv("payment.note", string("event below"))])
                .with_event(event("payment", vec![kv("payment.card", string(CARD))])),
        ],
    ]
}

/// The resources of the fixture: one for each service of a probe, with only a `service.name`.
fn resources(traces: &[Vec<ProbeSpan>]) -> Vec<ProbeResource> {
    let mut services: Vec<&'static str> = Vec::new();
    for span in traces.iter().flatten() {
        if !services.contains(&span.service) {
            services.push(span.service);
        }
    }
    services.into_iter().map(ProbeResource::new).collect()
}

/// The spans of the services `probe-obf-*`.
///
/// Every probe is the only span of its trace.
pub fn obfuscation_fixture() -> Vec<u8> {
    let traces: Vec<Vec<ProbeSpan>> = [sql_traces(), cache_traces(), json_traces(), url_traces(), card_traces()]
        .into_iter()
        .flatten()
        .collect();
    probe_request(&resources(&traces), OBFUSCATION_SEED, &traces)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use otlp_protos::opentelemetry::proto::{
        collector::trace::v1::ExportTraceServiceRequest,
        common::v1::{any_value::Value, KeyValue},
        trace::v1::Span,
    };
    use prost::Message as _;

    use super::*;

    /// Returns each span of `obfuscation_fixture` and the `service.name` of its resource.
    fn spans() -> Vec<(String, Span)> {
        let request = ExportTraceServiceRequest::decode(obfuscation_fixture().as_slice()).unwrap();
        request
            .resource_spans
            .iter()
            .flat_map(|resource_spans| {
                let service = resource_spans
                    .resource
                    .as_ref()
                    .unwrap()
                    .attributes
                    .iter()
                    .find(|attribute| attribute.key == "service.name")
                    .and_then(|attribute| match &attribute.value.as_ref()?.value {
                        Some(Value::StringValue(service)) => Some(service.clone()),
                        _ => None,
                    })
                    .expect("a resource has a service.name");
                resource_spans.scope_spans[0]
                    .spans
                    .iter()
                    .map(move |span| (service.clone(), span.clone()))
            })
            .collect()
    }

    /// The string values of the attributes of `span` and of its events.
    fn string_values(span: &Span) -> Vec<String> {
        let attributes = span
            .attributes
            .iter()
            .chain(span.events.iter().flat_map(|event| event.attributes.iter()));
        attributes
            .filter_map(|KeyValue { value, .. }| match &value.as_ref()?.value {
                Some(Value::StringValue(value)) => Some(value.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn every_probe_is_the_only_span_of_a_trace_and_a_service_of_its_own() {
        let spans = spans();
        assert_eq!(spans.len(), 42);

        let services: HashSet<&str> = spans.iter().map(|(service, _)| service.as_str()).collect();
        assert_eq!(services.len(), spans.len(), "a service holds more than one span");
        assert!(services.iter().all(|service| service.starts_with("probe-obf-")));

        let traces: HashSet<&[u8]> = spans.iter().map(|(_, span)| span.trace_id.as_slice()).collect();
        assert_eq!(traces.len(), spans.len(), "a trace holds more than one span");
    }

    #[test]
    fn the_only_secrets_are_the_canary_and_numbers_that_no_issuer_holds() {
        // The numbers of the card probes: the card, one that fails the Luhn check, and the lengths that a scan treats
        // differently. All have the prefix of a test card.
        let numbers: HashSet<&str> = [
            CARD,
            "4111111111111112",
            "4111 1111 1111 1111",
            "4111-1111-1111-1111",
            "41111111111",
            "411111111111",
            "41111111111111111",
            "4111111111111111111",
        ]
        .into_iter()
        .collect();

        for (service, span) in spans() {
            for value in string_values(&span) {
                if value.contains("canary") {
                    assert!(
                        value.contains(CANARY) && value.matches("canary").count() == value.matches(CANARY).count(),
                        "{service}: '{value}' holds a secret that is not the canary"
                    );
                }

                // A run of eleven or more digits, spaces and dashes, or a longer one in text, must be one of the numbers.
                let digits: String = value.chars().filter(char::is_ascii_digit).collect();
                if digits.len() >= 11 && !value.starts_with("card ") {
                    assert!(
                        numbers.contains(value.as_str()),
                        "{service}: '{value}' holds a number that is not a test number"
                    );
                }
            }
        }
    }

    #[test]
    fn the_probes_that_keep_a_url_hold_no_secret() {
        for (service, span) in spans() {
            if matches!(
                service.as_str(),
                "probe-obf-url-query-string" | "probe-obf-url-path-digits"
            ) {
                assert!(string_values(&span).iter().all(|value| !value.contains(CANARY)));
            }
        }
    }

    #[test]
    fn the_numeric_card_probes_hold_a_number_and_no_string() {
        let spans = spans();
        let span = |service: &str| spans.iter().find(|(name, _)| name == service).unwrap().1.clone();

        let integer = span("probe-obf-card-integer");
        assert!(integer.attributes.iter().any(|attribute| matches!(
            &attribute.value.as_ref().unwrap().value,
            Some(Value::IntValue(4_111_111_111_111_111))
        )));
        let double = span("probe-obf-card-double");
        assert!(double
            .attributes
            .iter()
            .any(|attribute| matches!(&attribute.value.as_ref().unwrap().value, Some(Value::DoubleValue(value)) if *value == 4_111_111_111_111_111.0)));
        assert!(string_values(&double).is_empty());
    }

    #[test]
    fn the_event_probe_keeps_its_card_in_an_event() {
        let (_, span) = spans()
            .into_iter()
            .find(|(service, _)| service == "probe-obf-card-event")
            .unwrap();
        assert_eq!(span.events.len(), 1);
        assert!(span.attributes.iter().all(|attribute| !matches!(
            &attribute.value.as_ref().unwrap().value,
            Some(Value::StringValue(value)) if value.contains(CARD)
        )));
    }
}
