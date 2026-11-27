//! `http_request`: one HTTP request to an endpoint of a participant, and a check of its one reply.
//!
//! The step sends the request once. It does not retry, poll or follow a redirect, and it compares the reply body as it
//! arrived, without decoding it. Wait for the endpoint with an earlier step.
//!
//! `expect` holds the predicates that the reply must meet. A `known` entry names a gap and its issue, and replaces some
//! of those predicates with what the software answers today. The step passes as known when the reply meets the
//! predicates with the replacements, and meets none of the replaced predicates of `expect`. A reply that meets one of
//! them shows that the gap is fixed there, so the entry is stale and the step fails.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_compression::tokio::write::GzipEncoder;
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, CONTENT_ENCODING, CONTENT_LENGTH, HOST, TRANSFER_ENCODING};
use reqwest::Method;
use serde::Deserialize;
use tokio::io::AsyncWriteExt as _;
use tokio::time::Instant;

use super::stopped;
use crate::core::case::{self, Timeout};
use crate::core::handle::{HandleError, HttpReply, HttpRequestSpec};
use crate::core::record::StepStatus;
use crate::core::step::{EndpointOperation, Need, Step, StepContext, StepFactory, StepFields, StepPurpose, StepReport};

/// The most bytes of a body that a message shows.
const MAX_SHOWN_BODY_BYTES: usize = 200;

/// The headers that frame a message. The client sets them, so a case cannot.
const FRAMING_HEADERS: [HeaderName; 3] = [HOST, CONTENT_LENGTH, TRANSFER_ENCODING];

pub(super) struct HttpRequestFactory;

impl StepFactory for HttpRequestFactory {
    fn name(&self) -> &'static str {
        "http_request"
    }

    fn purpose(&self) -> StepPurpose {
        StepPurpose::Action
    }

    fn build(&self, fields: &StepFields) -> Result<Box<dyn Step>, String> {
        Ok(Box::new(HttpRequest::new(fields.decode()?, &fields.dir)?))
    }
}

/// The fields as the case writes them.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HttpRequestFields {
    on: String,
    endpoint: String,
    request: RequestFields,
    expect: ExpectFields,
    known: Option<KnownFields>,
    timeout: Timeout,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestFields {
    method: String,
    path: String,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    body: Option<BytesFields>,
    content_encoding: Option<ContentEncoding>,
}

/// How the step encodes the request body before it sends it.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ContentEncoding {
    Gzip,
}

/// Bytes from exactly one source: UTF-8 text, base64, or a file relative to the case directory.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BytesFields {
    text: Option<String>,
    base64: Option<String>,
    file: Option<PathBuf>,
}

impl BytesFields {
    /// The bytes of the field `field`. A file is read from `dir`, and `None` allows no file.
    fn read(self, field: &str, dir: Option<&Path>) -> Result<Vec<u8>, String> {
        match (self.text, self.base64, self.file, dir) {
            (Some(text), None, None, _) => Ok(text.into_bytes()),
            (None, Some(encoded), None, _) => STANDARD
                .decode(&encoded)
                .map_err(|e| format!("{}.base64: {}", field, e)),
            (None, None, Some(file), Some(dir)) => {
                case::read_step_source(dir, &file).map_err(|e| format!("{}.file: {}", field, e))
            }
            (_, _, _, Some(_)) => Err(format!("{} takes exactly one of 'text', 'base64' and 'file'", field)),
            (_, _, _, None) => Err(format!("{} takes exactly one of 'text' and 'base64'", field)),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpectFields {
    status: Option<u16>,
    /// The value of each header, or `None` for a header that must be absent.
    #[serde(default)]
    headers: BTreeMap<String, Option<String>>,
    body: Option<BytesFields>,
    body_contains: Option<BytesFields>,
}

impl ExpectFields {
    /// The predicates of the field `field`, with each file read from `dir`.
    fn predicates(self, field: &str, dir: &Path) -> Result<Vec<Predicate>, String> {
        let mut predicates = Vec::new();
        if let Some(status) = self.status {
            predicates.push(Predicate::Status(status));
        }
        let mut names = BTreeMap::new();
        for (written, value) in self.headers {
            let name = HeaderName::from_bytes(written.as_bytes())
                .map_err(|_| format!("{}.headers: '{}' is not a header name", field, written))?;
            if let Some(first) = names.insert(name.to_string(), written.clone()) {
                return Err(format!(
                    "{}.headers: '{}' and '{}' name one header",
                    field, first, written
                ));
            }
            predicates.push(Predicate::Header(name, value));
        }
        match (self.body, self.body_contains) {
            (Some(_), Some(_)) => return Err(format!("{} takes 'body' or 'body_contains', not both", field)),
            (Some(body), None) => predicates.push(Predicate::Body(body.read(&format!("{}.body", field), Some(dir))?)),
            (None, Some(part)) => predicates.push(Predicate::BodyContains(
                part.read(&format!("{}.body_contains", field), None)?,
            )),
            (None, None) => {}
        }
        Ok(predicates)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KnownFields {
    gap: String,
    issue: u64,
    reason: String,
    expect: ExpectFields,
}

/// One condition on a reply.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Predicate {
    Status(u16),
    /// The value of a header, or `None` when the reply must not have it.
    Header(HeaderName, Option<String>),
    Body(Vec<u8>),
    BodyContains(Vec<u8>),
}

impl Predicate {
    /// Returns `true` if `self` and `other` check the same part of a reply: the status, one header, or the body.
    fn checks_same_part(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Status(_), Self::Status(_)) => true,
            (Self::Header(name, _), Self::Header(other, _)) => name == other,
            (Self::Body(_) | Self::BodyContains(_), Self::Body(_) | Self::BodyContains(_)) => true,
            _ => false,
        }
    }

    /// Returns `true` if `reply` meets the predicate.
    fn holds(&self, reply: &HttpReply) -> bool {
        match self {
            Self::Status(status) => reply.status == *status,
            Self::Header(name, expected) => match (header_value(&reply.headers, name), expected) {
                (None, None) => true,
                (Some(value), Some(expected)) => value == expected.as_bytes(),
                _ => false,
            },
            Self::Body(body) => reply.body == *body,
            Self::BodyContains(part) => part.is_empty() || reply.body.windows(part.len()).any(|window| window == part),
        }
    }
}

impl fmt::Display for Predicate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Status(status) => write!(f, "status {}", status),
            Self::Header(name, Some(value)) => write!(f, "header {}: {}", name, shown(value.as_bytes())),
            Self::Header(name, None) => write!(f, "no header {}", name),
            Self::Body(body) => write!(f, "the body {}", shown(body)),
            Self::BodyContains(part) => write!(f, "a body that contains {}", shown(part)),
        }
    }
}

/// The value of the header `name` in `headers`, with the values of repeated fields joined by `, `, or `None` if
/// there is no such header.
fn header_value(headers: &HeaderMap, name: &HeaderName) -> Option<Vec<u8>> {
    let mut values = headers.get_all(name).iter();
    let mut joined = values.next()?.as_bytes().to_vec();
    for value in values {
        joined.extend_from_slice(b", ");
        joined.extend_from_slice(value.as_bytes());
    }
    Some(joined)
}

/// `bytes` for a message: escaped, quoted, and cut to the first [`MAX_SHOWN_BODY_BYTES`] bytes.
fn shown(bytes: &[u8]) -> String {
    let cut = bytes.len().min(MAX_SHOWN_BODY_BYTES);
    let mut text = format!("\"{}\"", bytes[..cut].escape_ascii());
    if cut < bytes.len() {
        text.push_str(&format!(" (the first {} of {} bytes)", cut, bytes.len()));
    }
    text
}

/// The predicates of `expect`, each replaced by the predicate of `overlay` that checks the same part of a reply.
fn overlaid(expect: &[Predicate], overlay: &[Predicate]) -> Vec<Predicate> {
    expect
        .iter()
        .map(|predicate| {
            overlay
                .iter()
                .find(|replacement| replacement.checks_same_part(predicate))
                .unwrap_or(predicate)
                .clone()
        })
        .collect()
}

/// The predicates that `reply` does not meet.
fn failing<'a>(predicates: &'a [Predicate], reply: &HttpReply) -> Vec<&'a Predicate> {
    predicates.iter().filter(|predicate| !predicate.holds(reply)).collect()
}

/// Joins predicates for a message.
fn listed(predicates: &[&Predicate]) -> String {
    let listed: Vec<String> = predicates.iter().map(ToString::to_string).collect();
    listed.join("; ")
}

/// An issue-linked reply that the software gives today in place of the expected one.
#[derive(Debug)]
struct Known {
    gap: String,
    issue: u64,
    reason: String,
    /// The predicates that replace those of `expect` that check the same parts of a reply.
    overlay: Vec<Predicate>,
}

impl Known {
    /// Checks the entry against `expect`, and returns it.
    fn new(fields: KnownFields, expect: &[Predicate], dir: &Path) -> Result<Self, String> {
        let name = fields.gap.strip_prefix("gap/").unwrap_or_default();
        let kebab = name
            .split('-')
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()));
        if !kebab {
            return Err(format!(
                "known.gap: '{}' is not of the form gap/<name>, where the name is lowercase words joined by '-'",
                fields.gap
            ));
        }
        if fields.issue == 0 {
            return Err("known.issue: an issue number is above zero".to_string());
        }
        let overlay = fields.expect.predicates("known.expect", dir)?;
        if overlay.is_empty() {
            return Err("known.expect: a known entry replaces at least one predicate of expect".to_string());
        }
        for replacement in &overlay {
            match expect.iter().find(|predicate| predicate.checks_same_part(replacement)) {
                None => {
                    return Err(format!(
                        "known.expect: {} checks a part of the reply that expect does not check",
                        replacement
                    ))
                }
                Some(predicate) if predicate == replacement => {
                    return Err(format!(
                        "known.expect: {} is the same in expect, so the entry does not replace it",
                        replacement
                    ))
                }
                Some(_) => {}
            }
        }
        Ok(Self {
            gap: fields.gap,
            issue: fields.issue,
            reason: fields.reason,
            overlay,
        })
    }
}

/// Sends `request` to the endpoint once, and checks the reply against `expect`, or against `known`, within `timeout`.
#[derive(Debug)]
struct HttpRequest {
    on: String,
    endpoint: String,
    /// The request, with its body before any content encoding.
    request: HttpRequestSpec,
    content_encoding: Option<ContentEncoding>,
    expect: Vec<Predicate>,
    known: Option<Known>,
    timeout: Timeout,
}

impl HttpRequest {
    /// Checks the fields, with each file read from the case directory `dir`, and returns the step.
    fn new(fields: HttpRequestFields, dir: &Path) -> Result<Self, String> {
        let request = fields.request;
        let method = Method::from_bytes(request.method.as_bytes())
            .map_err(|_| format!("request.method: '{}' is not an HTTP method", request.method))?;
        let mut headers = HeaderMap::new();
        for (written, value) in &request.headers {
            let name = HeaderName::from_bytes(written.as_bytes())
                .map_err(|_| format!("request.headers: '{}' is not a header name", written))?;
            if FRAMING_HEADERS.contains(&name) {
                return Err(format!(
                    "request.headers: the client sets '{}' itself, so a case cannot",
                    written
                ));
            }
            if name == CONTENT_ENCODING && request.content_encoding.is_some() {
                return Err(format!(
                    "request.headers: '{}' and request.content_encoding both set the content encoding",
                    written
                ));
            }
            let value = HeaderValue::from_str(value)
                .map_err(|_| format!("request.headers: the value of '{}' is not a header value", written))?;
            if headers.insert(name, value).is_some() {
                return Err(format!(
                    "request.headers: '{}' names a header that another key names too",
                    written
                ));
            }
        }
        if let Some(ContentEncoding::Gzip) = request.content_encoding {
            headers.insert(CONTENT_ENCODING, HeaderValue::from_static("gzip"));
        }
        let body = match request.body {
            Some(body) => body.read("request.body", Some(dir))?,
            None => Vec::new(),
        };

        let spec = HttpRequestSpec {
            method,
            path: request.path,
            headers,
            body,
        };
        // The port does not matter: the check is of the path.
        spec.url(0).map_err(|e| format!("request.path: {}", e))?;

        let expect = fields.expect.predicates("expect", dir)?;
        if !expect.iter().any(|predicate| matches!(predicate, Predicate::Status(_))) {
            return Err("expect: needs a status".to_string());
        }
        let known = fields.known.map(|known| Known::new(known, &expect, dir)).transpose()?;
        Ok(Self {
            on: fields.on,
            endpoint: fields.endpoint,
            request: spec,
            content_encoding: request.content_encoding,
            expect,
            known,
            timeout: fields.timeout,
        })
    }

    /// The request as the step sends it, with its body encoded.
    async fn encoded(&self) -> Result<HttpRequestSpec, HandleError> {
        let mut request = self.request.clone();
        if let Some(ContentEncoding::Gzip) = self.content_encoding {
            let mut encoder = GzipEncoder::new(Vec::new());
            let encoded = async {
                encoder.write_all(&request.body).await?;
                encoder.shutdown().await
            };
            encoded
                .await
                .map_err(|e| HandleError::Harness(format!("the request body cannot be gzipped: {}", e)))?;
            request.body = encoder.into_inner();
        }
        Ok(request)
    }

    /// The request, for a message.
    fn described(&self) -> String {
        format!(
            "{} {} at endpoint '{}' of '{}'",
            self.request.method, self.request.path, self.endpoint, self.on
        )
    }

    /// The reply, for a message: the status, the value of each header that the step checks, and the start of the body.
    fn seen(&self, reply: &HttpReply) -> String {
        let mut seen = format!("{} answered status {}", self.described(), reply.status);
        for predicate in &self.expect {
            if let Predicate::Header(name, _) = predicate {
                match header_value(&reply.headers, name) {
                    Some(value) => seen.push_str(&format!(", {}: {}", name, shown(&value))),
                    None => seen.push_str(&format!(", no {}", name)),
                }
            }
        }
        seen.push_str(&format!(" and the body {}", shown(&reply.body)));
        seen
    }

    /// Judges `reply` against `expect` and the known entry.
    fn judge(&self, reply: &HttpReply) -> StepReport {
        let seen = self.seen(reply);
        let expect_fails = failing(&self.expect, reply);
        let report = |status, message| StepReport { status, message };
        let Some(known) = &self.known else {
            if expect_fails.is_empty() {
                return report(StepStatus::Passed, format!("{}.", seen));
            }
            return report(
                StepStatus::Failed,
                format!("{}. It does not meet {}.", seen, listed(&expect_fails)),
            );
        };

        let entry = format!("the known entry for {} (#{})", known.gap, known.issue);
        // The replaced predicates of expect that the reply meets: the gap is fixed there.
        let fixed: Vec<&Predicate> = self
            .expect
            .iter()
            .filter(|predicate| {
                known
                    .overlay
                    .iter()
                    .any(|replacement| replacement.checks_same_part(predicate))
                    && predicate.holds(reply)
            })
            .collect();
        if !fixed.is_empty() {
            let mut message = format!(
                "{}. It meets {} of expect, so {} is stale: narrow or remove it.",
                seen,
                listed(&fixed),
                entry
            );
            if !expect_fails.is_empty() {
                message.push_str(&format!(" It does not meet {} of expect.", listed(&expect_fails)));
            }
            return report(StepStatus::Failed, message);
        }
        let replaced = overlaid(&self.expect, &known.overlay);
        let known_fails = failing(&replaced, reply);
        if known_fails.is_empty() {
            return report(
                StepStatus::Passed,
                format!(
                    "{}, as known for {} (#{}): {}",
                    seen, known.gap, known.issue, known.reason
                ),
            );
        }
        report(
            StepStatus::Failed,
            format!(
                "{}. It does not meet {} of expect, nor {} of {}.",
                seen,
                listed(&expect_fails),
                listed(&known_fails),
                entry
            ),
        )
    }
}

#[async_trait]
impl Step for HttpRequest {
    fn name(&self) -> &'static str {
        "http_request"
    }

    fn needs(&self) -> Vec<Need> {
        vec![Need::Endpoint(
            self.on.clone(),
            self.endpoint.clone(),
            EndpointOperation::Exchange,
        )]
    }

    fn budget(&self) -> Duration {
        self.timeout.duration
    }

    async fn run(&self, cx: &StepContext<'_>) -> StepReport {
        let exchanged = async {
            let handle = cx.handle(&self.on)?;
            let request = self.encoded().await?;
            let limit = cx.deadline.saturating_duration_since(Instant::now());
            handle.http_request(&self.endpoint, &request, limit).await
        };
        let no_reply = |message| format!("{} got no reply: {}", self.described(), message);
        match exchanged.await {
            Ok(reply) => self.judge(&reply),
            Err(HandleError::Software(message)) => stopped(HandleError::Software(no_reply(message))),
            Err(HandleError::Harness(message)) => stopped(HandleError::Harness(no_reply(message))),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use async_compression::tokio::write::GzipDecoder;
    use async_trait::async_trait;
    use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWrite, BufReader};
    use tokio::net::TcpListener;

    use super::*;
    use crate::core::backend::{Exec, Realized};
    use crate::core::case::CASE_FILE;
    use crate::core::handle::Handle;
    use crate::core::observe::{Kept, Observation};
    use crate::core::participant::Os;
    use crate::core::reference::Values;

    /// The fields of a step with `request`, `expect` and, when set, `known`, each a YAML flow map.
    fn fields(request: &str, expect: &str, known: Option<&str>) -> String {
        let mut fields = format!(
            "{{on: app, endpoint: api, request: {}, expect: {}, timeout: 5s",
            request, expect
        );
        if let Some(known) = known {
            fields.push_str(&format!(", known: {}", known));
        }
        fields.push('}');
        fields
    }

    /// Builds the step from `fields` in the case directory `dir`.
    fn build_in(fields: &str, dir: &Path) -> Result<HttpRequest, String> {
        let fields = StepFields {
            values: serde_saphyr::from_str(fields).unwrap(),
            dir: dir.to_path_buf(),
        };
        HttpRequest::new(fields.decode()?, &fields.dir)
    }

    /// Builds the step from `fields`, which name no file.
    fn build(fields: &str) -> Result<HttpRequest, String> {
        build_in(fields, Path::new(""))
    }

    /// A reply with `status`, `headers` and `body`.
    fn reply(status: u16, headers: &[(&'static str, &'static str)], body: &[u8]) -> HttpReply {
        let mut map = HeaderMap::new();
        for (name, value) in headers {
            map.append(HeaderName::from_static(name), HeaderValue::from_static(value));
        }
        HttpReply {
            status,
            headers: map,
            body: body.to_vec(),
        }
    }

    const POST: &str = "{method: POST, path: /v1/metrics}";

    #[test]
    fn fields_that_cannot_be_honored_do_not_build() {
        let known = |expect: &str| format!("{{gap: gap/otlp-format, issue: 3482, reason: r, expect: {}}}", expect);
        let cases = [
            (
                format!("{{extra: 1, {}", &fields(POST, "{status: 200}", None)[1..]),
                "unknown field `extra`",
            ),
            (
                fields("{method: POST, path: /, verb: x}", "{status: 200}", None),
                "unknown field `verb`",
            ),
            (fields(POST, "{status: 200, length: 2}", None), "unknown field `length`"),
            (
                fields("{method: 'PO ST', path: /}", "{status: 200}", None),
                "is not an HTTP method",
            ),
            (
                fields("{method: GET, path: v1}", "{status: 200}", None),
                "does not start with '/'",
            ),
            (
                fields(
                    "{method: POST, path: /, body: {text: a, base64: YQ==}}",
                    "{status: 200}",
                    None,
                ),
                "request.body takes exactly one of 'text', 'base64' and 'file'",
            ),
            (
                fields("{method: POST, path: /, body: {}}", "{status: 200}", None),
                "request.body takes exactly one of",
            ),
            (
                fields(POST, "{status: 200, body_contains: {file: a.txt}}", None),
                "expect.body_contains takes exactly one of 'text' and 'base64'",
            ),
            (
                fields(POST, "{status: 200, body: {base64: '!!'}}", None),
                "expect.body.base64",
            ),
            (
                fields("{method: GET, path: /, headers: {Host: a}}", "{status: 200}", None),
                "the client sets 'Host' itself",
            ),
            (
                fields(
                    "{method: GET, path: /, headers: {content-length: '1'}}",
                    "{status: 200}",
                    None,
                ),
                "the client sets 'content-length' itself",
            ),
            (
                fields(
                    "{method: POST, path: /, headers: {Content-Encoding: gzip}, content_encoding: gzip}",
                    "{status: 200}",
                    None,
                ),
                "'Content-Encoding' and request.content_encoding both set the content encoding",
            ),
            (
                fields("{method: POST, path: /, content_encoding: br}", "{status: 200}", None),
                "unknown variant `br`",
            ),
            (
                fields(
                    "{method: POST, path: /, headers: {Content-Type: a, content-type: b}}",
                    "{status: 200}",
                    None,
                ),
                "names a header that another key names too",
            ),
            (
                fields(POST, "{status: 200, headers: {Content-Type: a, content-type: b}}", None),
                "'Content-Type' and 'content-type' name one header",
            ),
            (
                fields(POST, "{headers: {content-type: a}}", None),
                "expect: needs a status",
            ),
            (
                fields(POST, "{status: 200, body: {text: a}, body_contains: {text: a}}", None),
                "takes 'body' or 'body_contains', not both",
            ),
            (
                fields(POST, "{status: 200}", Some(&known("{}"))),
                "replaces at least one predicate",
            ),
            (
                fields(POST, "{status: 200}", Some(&known("{status: 200}"))),
                "status 200 is the same in expect",
            ),
            (
                fields(
                    POST,
                    "{status: 200, headers: {content-type: a}}",
                    Some(&known("{headers: {Content-Type: a}}")),
                ),
                "header content-type: \"a\" is the same in expect",
            ),
            (
                fields(POST, "{status: 200}", Some(&known("{headers: {content-type: a}}"))),
                "header content-type: \"a\" checks a part of the reply that expect does not check",
            ),
            (
                fields(POST, "{status: 200}", Some(&known("{body: {text: OK}}"))),
                "checks a part of the reply that expect does not check",
            ),
            (
                fields(POST, "{status: 200}", Some(&known("{status: 500, extra: 1}"))),
                "unknown field `extra`",
            ),
            (
                fields(
                    POST,
                    "{status: 200}",
                    Some("{gap: gap/a, issue: 1, reason: r, expect: {status: 500}, extra: 1}"),
                ),
                "unknown field `extra`",
            ),
        ];
        let gaps = [
            "otlp-format",
            "gap/",
            "gap/Otlp",
            "gap/otlp--format",
            "gap/otlp_format",
            "gap/-otlp",
        ];
        let cases = cases.into_iter().chain(gaps.iter().map(|gap| {
            (
                fields(
                    POST,
                    "{status: 200}",
                    Some(&format!(
                        "{{gap: '{}', issue: 1, reason: r, expect: {{status: 500}}}}",
                        gap
                    )),
                ),
                "is not of the form gap/<name>",
            )
        }));
        let cases = cases.chain([
            (
                fields(
                    POST,
                    "{status: 200}",
                    Some("{gap: gap/a, issue: 0, reason: r, expect: {status: 500}}"),
                ),
                "an issue number is above zero",
            ),
            (
                fields(
                    POST,
                    "{status: 200}",
                    Some("{gap: gap/a, issue: -1, reason: r, expect: {status: 500}}"),
                ),
                "invalid value",
            ),
        ]);
        for (fields, part) in cases {
            let error = HttpRequestFactory
                .build(&StepFields::of(serde_saphyr::from_str(&fields).unwrap()))
                .unwrap_err();
            assert!(error.contains(part), "{}: {}", fields, error);
        }
        assert!(build(&fields(
            POST,
            "{status: 200}",
            Some("{gap: gap/otlp-format-2, issue: 1, reason: r, expect: {status: 500}}")
        ))
        .is_ok());
    }

    #[test]
    fn a_path_that_the_url_parser_would_rewrite_does_not_build() {
        let path = |path: &str| {
            build(&fields(
                &format!("{{method: GET, path: '{}'}}", path),
                "{status: 200}",
                None,
            ))
        };
        let rewritten = [
            ("/v1/../metrics", "/metrics"),
            ("/v1/./metrics", "/v1/metrics"),
            ("/a b", "/a%20b"),
            ("/v1#frag", "/v1"),
            ("/v1\\x", "/v1/x"),
            ("/v1?a=b c", "/v1?a=b%20c"),
            ("/\u{e9}", "/%C3%A9"),
        ];
        for (written, sent) in rewritten {
            let error = path(written).unwrap_err();
            assert_eq!(
                error,
                format!("request.path: the path '{}' would be sent as '{}'", written, sent)
            );
        }
        for written in [
            "/",
            "//a//b/",
            "/v1/metrics?a=b&c=%20d",
            "/v1?",
            "/a%2Fb",
            "/v1/..%2Fmetrics",
        ] {
            let step = path(written).unwrap();
            assert_eq!(step.request.path, written);
        }
    }

    #[test]
    fn a_body_file_is_read_from_the_case_directory_when_the_case_is_planned() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("suite").join("case");
        std::fs::create_dir_all(dir.join("bodies")).unwrap();
        std::fs::write(dir.join("bodies").join("metrics.pb"), [0x0a, 0x00, 0xff]).unwrap();
        std::fs::write(root.path().join("outside.pb"), [0x01]).unwrap();
        let step = build_in(
            &fields(
                "{method: POST, path: /, body: {file: bodies/metrics.pb}}",
                "{status: 200, body: {file: bodies/metrics.pb}}",
                None,
            ),
            &dir,
        )
        .unwrap();
        assert_eq!(step.request.body, [0x0a, 0x00, 0xff]);
        assert_eq!(step.expect[1], Predicate::Body(vec![0x0a, 0x00, 0xff]));

        let missing = build_in(
            &fields(
                "{method: POST, path: /, body: {file: bodies/missing.pb}}",
                "{status: 200}",
                None,
            ),
            &dir,
        )
        .unwrap_err();
        assert!(
            missing.contains("request.body.file: file source 'bodies/missing.pb' cannot be read"),
            "{}",
            missing
        );
        let outside = build_in(
            &fields(
                "{method: POST, path: /, body: {file: ../../outside.pb}}",
                "{status: 200}",
                None,
            ),
            &dir,
        )
        .unwrap_err();
        assert!(outside.contains("is outside the suite directory"), "{}", outside);
    }

    /// A step that expects an empty protobuf reply, and knows a plain-text reply for one gap.
    fn known_step() -> HttpRequest {
        build(&fields(
            POST,
            "{status: 200, headers: {content-type: application/x-protobuf, content-encoding: null}, body: {base64: CgA=}}",
            Some(
                "{gap: gap/otlp-format, issue: 3482, reason: ADP answers OK, expect: {headers: {content-type: text/plain}, \
                 body: {text: OK}}}",
            ),
        ))
        .unwrap()
    }

    #[test]
    fn a_reply_is_judged_against_expect_and_the_known_entry() {
        let expected = reply(200, &[("content-type", "application/x-protobuf")], b"\n\0");
        let known = reply(200, &[("content-type", "text/plain")], b"OK");
        let half_fixed = reply(200, &[("content-type", "application/x-protobuf")], b"OK");
        let neither = reply(500, &[("content-type", "text/plain")], b"OK");

        let plain = build(&fields(
            POST,
            "{status: 200, headers: {content-type: application/x-protobuf, content-encoding: null}, body: {base64: CgA=}}",
            None,
        ))
        .unwrap();
        let report = plain.judge(&expected);
        assert_eq!(report.status, StepStatus::Passed, "{}", report.message);
        assert_eq!(
            report.message,
            "POST /v1/metrics at endpoint 'api' of 'app' answered status 200, no content-encoding, \
             content-type: \"application/x-protobuf\" and the body \"\\n\\x00\"."
        );
        let report = plain.judge(&known);
        assert_eq!(report.status, StepStatus::Failed);
        assert!(
            report
                .message
                .ends_with("It does not meet header content-type: \"application/x-protobuf\"; the body \"\\n\\x00\"."),
            "{}",
            report.message
        );

        let step = known_step();
        let report = step.judge(&known);
        assert_eq!(report.status, StepStatus::Passed, "{}", report.message);
        assert!(
            report
                .message
                .ends_with(", as known for gap/otlp-format (#3482): ADP answers OK"),
            "{}",
            report.message
        );

        let report = step.judge(&expected);
        assert_eq!(report.status, StepStatus::Failed);
        assert!(
            report.message.contains(
                "It meets header content-type: \"application/x-protobuf\"; the body \"\\n\\x00\" of expect, so the known \
                 entry for gap/otlp-format (#3482) is stale: narrow or remove it."
            ),
            "{}",
            report.message
        );

        let report = step.judge(&half_fixed);
        assert_eq!(report.status, StepStatus::Failed);
        assert!(
            report.message.contains(
                "It meets header content-type: \"application/x-protobuf\" of expect, so the known entry for \
                 gap/otlp-format (#3482) is stale"
            ),
            "{}",
            report.message
        );
        assert!(
            report
                .message
                .ends_with("It does not meet the body \"\\n\\x00\" of expect."),
            "{}",
            report.message
        );

        let report = step.judge(&neither);
        assert_eq!(report.status, StepStatus::Failed);
        assert!(
            report.message.ends_with(
                "It does not meet status 200; header content-type: \"application/x-protobuf\"; the body \"\\n\\x00\" of \
                 expect, nor status 200 of the known entry for gap/otlp-format (#3482)."
            ),
            "{}",
            report.message
        );
    }

    #[test]
    fn a_header_predicate_reads_the_header_by_name_and_tells_absent_from_present() {
        let absent = Predicate::Header(HeaderName::from_static("content-encoding"), None);
        let identity = Predicate::Header(
            HeaderName::from_static("content-encoding"),
            Some("identity".to_string()),
        );
        assert!(absent.holds(&reply(200, &[], b"")));
        assert!(!absent.holds(&reply(200, &[("content-encoding", "")], b"")));
        assert!(!absent.holds(&reply(200, &[("content-encoding", "identity")], b"")));
        assert!(identity.holds(&reply(200, &[("content-encoding", "identity")], b"")));
        assert!(!identity.holds(&reply(200, &[], b"")));
        assert!(!identity.holds(&reply(200, &[("content-encoding", "Identity")], b"")));

        // The case names a header in any letter case, and the reply's repeated fields are one value.
        let step = build(&fields(POST, "{status: 200, headers: {Vary: 'Accept, Origin'}}", None)).unwrap();
        let report = step.judge(&reply(200, &[("vary", "Accept"), ("vary", "Origin")], b""));
        assert_eq!(report.status, StepStatus::Passed, "{}", report.message);
        assert!(
            report.message.contains("vary: \"Accept, Origin\""),
            "{}",
            report.message
        );
    }

    #[test]
    fn body_contains_finds_bytes_anywhere_in_the_body() {
        let step = build(&fields(POST, "{status: 400, body_contains: {base64: AP8=}}", None)).unwrap();
        assert_eq!(step.judge(&reply(400, &[], b"a\0\xffb")).status, StepStatus::Passed);
        assert_eq!(step.judge(&reply(400, &[], b"\0\xff")).status, StepStatus::Passed);
        let report = step.judge(&reply(400, &[], b"a\0b\xff"));
        assert_eq!(report.status, StepStatus::Failed);
        assert!(
            report
                .message
                .ends_with("It does not meet a body that contains \"\\x00\\xff\"."),
            "{}",
            report.message
        );
        let text = build(&fields(
            POST,
            "{status: 400, body_contains: {text: 'Content-Encoding: br'}}",
            None,
        ))
        .unwrap();
        let body = b"unsupported Content-Encoding: br";
        assert_eq!(text.judge(&reply(400, &[], body)).status, StepStatus::Passed);
    }

    #[test]
    fn a_long_body_is_cut_in_a_message() {
        let body = vec![b'a'; MAX_SHOWN_BODY_BYTES + 5];
        let report = build(&fields(POST, "{status: 200}", None))
            .unwrap()
            .judge(&reply(500, &[], &body));
        let shown = format!("\"{}\" (the first 200 of 205 bytes)", "a".repeat(MAX_SHOWN_BODY_BYTES));
        assert!(report.message.contains(&shown), "{}", report.message);
    }

    /// A participant whose endpoints are all at `port` of this host.
    #[derive(Debug)]
    struct Served(u16);

    #[async_trait]
    impl Realized for Served {
        async fn exec(
            &self, _exec: Exec, _stdout: &mut (dyn AsyncWrite + Send + Unpin),
            _stderr: &mut (dyn AsyncWrite + Send + Unpin), _deadline: Instant,
        ) -> Result<i64, HandleError> {
            Err(HandleError::Harness("not scripted".to_string()))
        }

        async fn has_exited(&self) -> bool {
            false
        }

        async fn read_file(&self, _path: &str, _deadline: Instant) -> Result<Option<Vec<u8>>, HandleError> {
            Ok(None)
        }

        fn host_port(&self, _endpoint: &str) -> Option<u16> {
            Some(self.0)
        }
    }

    /// The requests a server received, each as its head and its body.
    type Received = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

    /// Serves `replies`, raw HTTP responses by path, on a port of this host, one connection at a time. Returns the port
    /// and what the server receives.
    async fn serve(replies: BTreeMap<&'static str, Vec<u8>>) -> (u16, Received) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let received = Received::default();
        let log = Arc::clone(&received);
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let mut stream = BufReader::new(stream);
                let mut head = String::new();
                loop {
                    let mut line = String::new();
                    if stream.read_line(&mut line).await.unwrap() == 0 || line == "\r\n" {
                        break;
                    }
                    head.push_str(&line);
                }
                let length = head
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                let mut body = vec![0; length];
                stream.read_exact(&mut body).await.unwrap();
                let path = head.split_whitespace().nth(1).unwrap_or_default().to_string();
                log.lock().unwrap().push((head, body));
                let reply = replies.get(path.as_str()).cloned().unwrap_or_else(|| {
                    b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_vec()
                });
                stream.get_mut().write_all(&reply).await.unwrap();
                stream.get_mut().shutdown().await.unwrap();
            }
        });
        (port, received)
    }

    /// A raw HTTP response with `status`, `headers` and `body`.
    fn response(status: &str, headers: &str, body: &[u8]) -> Vec<u8> {
        let mut response = format!(
            "HTTP/1.1 {}\r\n{}content-length: {}\r\nconnection: close\r\n\r\n",
            status,
            headers,
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(body);
        response
    }

    /// Plans a case whose host-process participant `app` has the HTTP endpoint `api`, with `procedure`, and runs each
    /// step against `realized`.
    async fn run_planned(procedure: &str, realized: &dyn Realized) -> Vec<StepReport> {
        let suite = tempfile::tempdir().unwrap();
        let dir = suite.path().join("case");
        std::fs::create_dir(&dir).unwrap();
        let config = format!(
            "
timeout: 1m
subject_os: [linux]
participants:
  app:
    subject: true
    realization: {{enclosure: host-process-group}}
    programs: [{{name: main, path: sh}}]
    endpoints:
      api: {{protocol: http, port: 8080}}
procedure:
{}",
            procedure
        );
        std::fs::write(dir.join(CASE_FILE), config).unwrap();
        let plans = crate::load_and_plan(&dir, &BTreeMap::new()).unwrap_or_else(|errors| {
            panic!(
                "{}",
                errors.iter().map(ToString::to_string).collect::<Vec<_>>().join("\n")
            )
        });
        let plan = &plans[&Os::Linux];
        // The client of the handle needs the process-wide crypto provider, which `main` installs at startup.
        let _ = crate::default_crypto_provider().install_default();
        let values = Values::of_plan(&plan.participants);
        let observation = Observation::new([]);
        let handles = BTreeMap::from([(
            "app".to_string(),
            Handle::new("app", &plan.participants["app"], realized, &values, &observation),
        )]);
        let kept = Kept::new(&suite.path().join("logs"));
        let mut reports = Vec::new();
        for step in plan.procedure.iter().flat_map(|node| node.steps()) {
            assert_eq!(step.reach.get("app.api"), Some(&crate::core::wiring::Reach::InRunner));
            let cx = StepContext::new(&handles, &values, &kept, Instant::now() + step.step.budget());
            reports.push(step.step.run(&cx).await);
        }
        reports
    }

    /// The requests in `received` for `path`.
    fn received_for(received: &Received, path: &str) -> Vec<(String, Vec<u8>)> {
        let prefix = format!(" {} HTTP/1.1\r\n", path);
        received
            .lock()
            .unwrap()
            .iter()
            .filter(|(head, _)| {
                head.split_once("\r\n")
                    .is_some_and(|(line, _)| format!("{}\r\n", line).ends_with(&prefix))
            })
            .cloned()
            .collect()
    }

    async fn gzipped(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = GzipEncoder::new(Vec::new());
        encoder.write_all(bytes).await.unwrap();
        encoder.shutdown().await.unwrap();
        encoder.into_inner()
    }

    #[tokio::test]
    async fn the_step_sends_one_request_as_written_and_judges_the_raw_reply() {
        let compressed = gzipped(b"hello").await;
        let (port, received) = serve(BTreeMap::from([
            ("/plain", response("200 OK", "", b"")),
            ("/gzipped", response("200 OK", "", b"")),
            ("/broken", response("500 Internal Server Error", "", b"broken")),
            ("/moved", response("302 Found", "location: /plain\r\n", b"")),
            (
                "/compressed",
                response("200 OK", "content-encoding: gzip\r\n", &compressed),
            ),
        ]))
        .await;
        let procedure = format!(
            "
  - {{step: http_request, on: app, endpoint: api, request: {{method: POST, path: /plain, body: {{text: hello}}}}, expect: {{status: 200}}, timeout: 5s}}
  - step: http_request
    on: app
    endpoint: api
    request:
      method: POST
      path: /gzipped
      headers: {{Content-Type: application/x-protobuf}}
      body: {{text: hello}}
      content_encoding: gzip
    expect: {{status: 200}}
    timeout: 5s
  - {{step: http_request, on: app, endpoint: api, request: {{method: POST, path: /broken}}, expect: {{status: 200}}, timeout: 5s}}
  - {{step: http_request, on: app, endpoint: api, request: {{method: GET, path: /moved}}, expect: {{status: 302, headers: {{location: /plain}}}}, timeout: 5s}}
  - step: http_request
    on: app
    endpoint: api
    request: {{method: GET, path: /compressed, headers: {{Accept-Encoding: gzip}}}}
    expect: {{status: 200, headers: {{content-encoding: gzip}}, body: {{base64: '{}'}}}}
    timeout: 5s
",
            STANDARD.encode(&compressed)
        );
        let reports = run_planned(&procedure, &Served(port)).await;
        let statuses: Vec<StepStatus> = reports.iter().map(|report| report.status).collect();
        assert_eq!(
            statuses,
            [
                StepStatus::Passed,
                StepStatus::Passed,
                StepStatus::Failed,
                StepStatus::Passed,
                StepStatus::Passed
            ],
            "{:#?}",
            reports
        );

        // The client adds no representation header of its own.
        let plain = received_for(&received, "/plain");
        assert_eq!(plain.len(), 1, "the redirect to /plain is not followed: {:?}", plain);
        let (head, body) = &plain[0];
        assert_eq!(body, b"hello");
        let head = head.to_ascii_lowercase();
        for name in ["content-type:", "content-encoding:", "accept-encoding:"] {
            assert!(!head.contains(name), "{}", head);
        }

        let gzip = received_for(&received, "/gzipped");
        assert_eq!(gzip.len(), 1);
        let (head, body) = &gzip[0];
        let head = head.to_ascii_lowercase();
        assert!(head.contains("\r\ncontent-encoding: gzip\r\n"), "{}", head);
        assert!(
            head.contains("\r\ncontent-type: application/x-protobuf\r\n"),
            "{}",
            head
        );
        assert!(!head.contains("accept-encoding:"), "{}", head);
        let mut decoder = GzipDecoder::new(Vec::new());
        decoder.write_all(body).await.unwrap();
        decoder.shutdown().await.unwrap();
        assert_eq!(decoder.into_inner(), b"hello");

        assert_eq!(received_for(&received, "/broken").len(), 1);
        assert!(
            reports[2]
                .message
                .contains("answered status 500 and the body \"broken\". It does not meet status 200."),
            "{}",
            reports[2].message
        );
        assert_eq!(received_for(&received, "/moved").len(), 1);
        let compressed = received_for(&received, "/compressed");
        assert!(
            compressed[0].0.contains("\r\naccept-encoding: gzip\r\n"),
            "{}",
            compressed[0].0
        );
    }

    #[tokio::test]
    async fn a_request_that_gets_no_reply_fails_and_is_never_known() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let procedure = "
  - step: http_request
    on: app
    endpoint: api
    request: {method: POST, path: /v1/metrics}
    expect: {status: 200}
    known: {gap: gap/otlp-format, issue: 3482, reason: r, expect: {status: 500}}
    timeout: 5s
";
        let reports = run_planned(procedure, &Served(port)).await;
        assert_eq!(reports[0].status, StepStatus::Failed, "{}", reports[0].message);
        assert!(
            reports[0]
                .message
                .starts_with("POST /v1/metrics at endpoint 'api' of 'app' got no reply: "),
            "{}",
            reports[0].message
        );
        assert!(!reports[0].message.contains("known"), "{}", reports[0].message);
    }

    /// Plans a case for `subject_os` whose participant `app` has the container image `image` and the HTTP endpoint
    /// `api`, which `endpoint` extends, with the one step `procedure`. Returns the problems the plan finds.
    fn plan_with(subject_os: &str, image: &str, endpoint: &str, procedure: &str) -> Result<(), Vec<String>> {
        let suite = tempfile::tempdir().unwrap();
        let dir = suite.path().join("case");
        std::fs::create_dir(&dir).unwrap();
        let config = format!(
            "
timeout: 1m
subject_os: [{}]
participants:
  app:
    subject: true
    image: {}
    endpoints:
      api: {{protocol: http, port: 8080{}}}
procedure:
  - {}
",
            subject_os, image, endpoint, procedure
        );
        std::fs::write(dir.join(CASE_FILE), config).unwrap();
        crate::load_and_plan(&dir, &BTreeMap::new())
            .map(|_| ())
            .map_err(|errors| errors.iter().map(ToString::to_string).collect::<Vec<_>>())
    }

    const HTTP_CHECK: &str = "{step: http_check, on: app, endpoint: api, path: /, status: {equal: 200}, timeout: 5s}";
    const HTTP_REQUEST: &str = "{step: http_request, on: app, endpoint: api, request: {method: GET, path: /}, \
                                expect: {status: 200}, timeout: 5s}";

    #[test]
    fn a_windows_container_cannot_take_an_http_request_and_can_take_an_http_check() {
        let image = "mcr.microsoft.com/windows/nanoserver:ltsc2022";
        assert_eq!(plan_with("windows", image, "", HTTP_CHECK), Ok(()));
        let errors = plan_with("windows", image, "", HTTP_REQUEST).unwrap_err();
        assert_eq!(errors.len(), 1, "{:#?}", errors);
        assert!(
            errors[0].contains(
                "step 1: http_request: endpoint 'api' of participant 'app': an HTTP request to a Windows container goes \
                 through the courier"
            ),
            "{}",
            errors[0]
        );
    }

    #[test]
    fn an_http_request_cannot_reach_a_tls_endpoint_and_an_http_check_can() {
        let tls = ", tls: true";
        assert_eq!(plan_with("linux", "alpine:3.20", tls, HTTP_CHECK), Ok(()));
        let errors = plan_with("linux", "alpine:3.20", tls, HTTP_REQUEST).unwrap_err();
        assert_eq!(errors.len(), 1, "{:#?}", errors);
        assert!(
            errors[0].contains(
                "step 1: http_request: endpoint 'api' of participant 'app': an HTTP request to a TLS endpoint is not \
                 built"
            ),
            "{}",
            errors[0]
        );
        assert_eq!(plan_with("linux", "alpine:3.20", "", HTTP_REQUEST), Ok(()));
    }

    #[test]
    fn an_http_request_cannot_run_in_parallel() {
        let suite = tempfile::tempdir().unwrap();
        let dir = suite.path().join("case");
        std::fs::create_dir(&dir).unwrap();
        let config = "
timeout: 1m
subject_os: [linux]
participants:
  app:
    subject: true
    image: alpine:3.20
    endpoints:
      api: {protocol: http, port: 8080}
procedure:
  - parallel:
      - {step: http_request, on: app, endpoint: api, request: {method: GET, path: /}, expect: {status: 200}, timeout: 5s}
";
        std::fs::write(dir.join(CASE_FILE), config).unwrap();
        let errors = crate::load_and_plan(&dir, &BTreeMap::new()).unwrap_err();
        assert!(
            errors[0]
                .to_string()
                .contains("'http_request' is an action, and only assertions run in parallel"),
            "{}",
            errors[0]
        );
    }
}
