//! Checks an HTTP response's status and optional body pattern through a participant endpoint.

use std::{fmt, time::Duration};

use async_trait::async_trait;
use panoramic_core::{
    case::CaseDuration,
    endpoint::EndpointOperation,
    probe::{AttemptLimits, POLL_INTERVAL},
    record::{AssertionStatus, StepReport},
    step::{decode_fields, HandleError, Need, Step, StepContext, StepFactory, StepFields},
};
use serde::Deserialize;
use tokio::time::{sleep_until, Instant};

use crate::{attempts::Attempts, Matcher};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fields {
    /// Required participant serving the endpoint; has no default.
    on: String,
    /// Required endpoint name; has no default.
    endpoint: String,
    /// Required request path beginning with `/`; has no default.
    path: String,
    /// Required status comparison, either `equal` or `not_equal`; has no default.
    status: StatusMatcher,
    /// Optional body pattern; omitted accepts any body.
    pattern: Option<String>,
    /// Whether the body pattern is a regex. Defaults to false; true requires `pattern`.
    #[serde(default)]
    regex: bool,
    /// Required, positive time to wait for a match; has no default.
    timeout: CaseDuration,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StatusMatcher {
    Equal(u16),
    NotEqual(u16),
}

impl StatusMatcher {
    fn matches(&self, actual: u16) -> bool {
        match self {
            Self::Equal(expected) => actual == *expected,
            Self::NotEqual(forbidden) => actual != *forbidden,
        }
    }
}

impl fmt::Display for StatusMatcher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Equal(status) => write!(f, "equal to {status}"),
            Self::NotEqual(status) => write!(f, "not equal to {status}"),
        }
    }
}

/// Waits for a response whose status and body satisfy the configured criteria.
#[derive(Debug)]
pub(crate) struct HttpCheck {
    on: String,
    endpoint: String,
    path: String,
    status: StatusMatcher,
    matcher: Option<Matcher>,
    timeout: Duration,
}

impl HttpCheck {
    /// Registry name used in case procedures.
    pub(crate) const NAME: &'static str = "http_check";
}

#[async_trait]
impl Step for HttpCheck {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn needs(&self) -> Vec<Need> {
        vec![Need::Endpoint(
            self.on.clone(),
            self.endpoint.clone(),
            EndpointOperation::Request,
        )]
    }

    fn budget(&self) -> Duration {
        self.timeout
    }

    async fn run(&self, cx: &StepContext<'_>) -> StepReport {
        let started = Instant::now();
        let deadline = started + self.timeout;
        let Some(handle) = cx.handle(&self.on) else {
            return StepReport::new(
                started,
                AssertionStatus::Errored,
                format!("participant '{}' has not started", self.on),
            );
        };
        let subject = format!(
            "HTTP GET '{}' at endpoint '{}' of participant '{}'",
            self.path, self.endpoint, self.on
        );
        let mut attempts = Attempts::default();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match handle
                .http_get(&self.endpoint, &self.path, &AttemptLimits::new(remaining))
                .await
            {
                Ok(response) => {
                    if !self.status.matches(response.status) {
                        attempts.observed(format!("status {} did not satisfy {}", response.status, self.status));
                    } else if let Some(matcher) = self
                        .matcher
                        .as_ref()
                        .filter(|matcher| !matcher.is_match(&String::from_utf8_lossy(&response.body)))
                    {
                        attempts.observed(format!(
                            "status {}; body did not match pattern '{}'",
                            response.status,
                            matcher.pattern()
                        ));
                    } else {
                        return StepReport::new(
                            started,
                            AssertionStatus::Passed,
                            format!("{subject} matched with status {}", response.status),
                        );
                    }
                }
                Err(HandleError::Product { message }) => attempts.observed(message),
                Err(HandleError::Harness { message }) => attempts.errored(message),
            }
            if Instant::now() >= deadline {
                break;
            }
            sleep_until(deadline.min(Instant::now() + POLL_INTERVAL)).await;
        }
        StepReport::new(
            started,
            attempts.status(),
            attempts.explain_expired(&format!("{subject} did not match within {:?}", self.timeout)),
        )
    }
}

/// Builds `http_check` and compiles its optional body pattern.
pub(crate) struct HttpCheckFactory;

impl StepFactory for HttpCheckFactory {
    fn name(&self) -> &'static str {
        HttpCheck::NAME
    }

    fn build(&self, fields: &StepFields) -> Result<Box<dyn Step>, String> {
        let fields = decode_fields::<Fields>(fields)?;
        if !fields.path.starts_with('/') {
            return Err("path must start with '/'".into());
        }
        if fields.regex && fields.pattern.is_none() {
            return Err("regex requires pattern".into());
        }
        let matcher = fields
            .pattern
            .as_deref()
            .map(|pattern| Matcher::build(pattern, fields.regex))
            .transpose()?;
        Ok(Box::new(HttpCheck {
            on: fields.on,
            endpoint: fields.endpoint,
            path: fields.path,
            status: fields.status,
            matcher,
            timeout: fields.timeout.into(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use panoramic_core::step::test_support::{run_step_reaching, run_step_with, FakeParticipant};
    use serde_json::{json, Value};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    use super::*;
    use crate::test_support::valid_fields;

    fn response(status: u16, body: &[u8]) -> Vec<u8> {
        let mut response = format!(
            "HTTP/1.1 {status} Status\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(body);
        response
    }

    async fn check_responses(fields: &StepFields, responses: &[Vec<u8>]) -> (StepReport, Vec<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let fake = FakeParticipant::default();
        let mut requests = Vec::new();
        let report = tokio::select! {
            report = run_step_reaching(&HttpCheckFactory, fields, &fake, port) => report,
            () = async {
                loop {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut request = Vec::new();
                    loop {
                        request.push(stream.read_u8().await.unwrap());
                        if request.ends_with(b"\r\n\r\n") {
                            break;
                        }
                    }
                    let response = &responses[requests.len().min(responses.len() - 1)];
                    requests.push(request);
                    stream.write_all(response).await.unwrap();
                }
            } => unreachable!(),
        };
        (report, requests)
    }

    #[tokio::test]
    async fn status_and_optional_body_criteria_match_the_same_response() {
        for (body, pattern, regex) in [
            (b"ready: 7".as_slice(), Some("ready: 7"), false),
            (b"ready: 7".as_slice(), Some(r"^ready: [1-9]$"), true),
            (b"ready: \xff".as_slice(), Some("ready: \u{fffd}"), false),
            (b"anything".as_slice(), None, false),
        ] {
            let mut fields = valid_fields(HttpCheck::NAME);
            fields.insert("pattern".into(), json!(pattern));
            fields.insert("regex".into(), json!(regex));
            let (report, requests) = check_responses(&fields, &[response(200, body)]).await;
            assert_eq!(report.status, AssertionStatus::Passed, "{}", report.message);
            assert_eq!(requests.len(), 1);
            assert!(requests[0].starts_with(b"GET /metrics HTTP/1.1\r\n"));
            assert_eq!(
                report.message,
                "HTTP GET '/metrics' at endpoint 'http' of participant 'subject' matched with status 200"
            );
        }
    }

    #[tokio::test]
    async fn status_and_body_mismatches_are_retried_until_both_match() {
        let mut fields = valid_fields(HttpCheck::NAME);
        fields.insert("pattern".into(), json!("ready"));
        let (report, requests) = check_responses(
            &fields,
            &[
                response(503, b"ready"),
                response(200, b"waiting"),
                response(200, b"ready"),
            ],
        )
        .await;
        assert_eq!(report.status, AssertionStatus::Passed, "{}", report.message);
        assert_eq!(requests.len(), 3);
    }

    #[tokio::test]
    async fn a_status_mismatch_is_retried_and_reports_the_last_status_at_timeout() {
        let mut fields = valid_fields(HttpCheck::NAME);
        fields.insert("timeout".into(), json!("3s"));
        let (report, requests) = check_responses(&fields, &[response(503, b""), response(502, b"")]).await;
        assert_eq!(report.status, AssertionStatus::Failed, "{}", report.message);
        assert!(requests.len() >= 2);
        assert!(
            report
                .message
                .ends_with("last attempt: status 502 did not satisfy equal to 200"),
            "{}",
            report.message
        );
    }

    #[tokio::test]
    async fn not_equal_accepts_any_other_status_and_retries_the_forbidden_one() {
        for status in [201, 404, 503] {
            let mut fields = valid_fields(HttpCheck::NAME);
            fields.insert("status".into(), json!({"not_equal": 200}));
            let (report, requests) = check_responses(&fields, &[response(200, b""), response(status, b"")]).await;
            assert_eq!(report.status, AssertionStatus::Passed, "{}", report.message);
            assert_eq!(requests.len(), 2);
            assert!(
                report.message.ends_with(&format!("matched with status {status}")),
                "{}",
                report.message
            );
        }
    }

    #[tokio::test]
    async fn a_body_that_never_matches_fails_at_timeout_without_quoting_the_body() {
        let mut fields = valid_fields(HttpCheck::NAME);
        fields.insert("timeout".into(), json!("3s"));
        fields.insert("pattern".into(), json!("ready"));
        let (report, requests) = check_responses(&fields, &[response(200, b"private response")]).await;
        assert_eq!(report.status, AssertionStatus::Failed);
        assert!(requests.len() >= 2);
        assert!(
            report
                .message
                .ends_with("last attempt: status 200; body did not match pattern 'ready'"),
            "{}",
            report.message
        );
        assert!(!report.message.contains("private response"));
    }

    #[tokio::test]
    async fn connection_failures_are_retried_and_fail_at_timeout() {
        let mut fields = valid_fields(HttpCheck::NAME);
        fields.insert("timeout".into(), json!("3s"));
        let (report, requests) = check_responses(&fields, &[Vec::new()]).await;
        assert_eq!(report.status, AssertionStatus::Failed, "{}", report.message);
        assert!(requests.len() >= 2);
        assert!(report.message.contains("last attempt:"), "{}", report.message);
    }

    #[tokio::test(start_paused = true)]
    async fn only_harness_failures_error_at_timeout() {
        let fake = FakeParticipant::default();
        let mut fields = valid_fields(HttpCheck::NAME);
        fields.insert("path".into(), json!("/invalid\npath"));
        let report = run_step_with(&HttpCheckFactory, &fields, Some(&fake)).await;
        assert_eq!(report.status, AssertionStatus::Errored, "{}", report.message);
        assert_eq!(report.duration, Duration::from_secs(30));
        assert!(report.message.contains("last attempt:"), "{}", report.message);
    }

    #[test]
    fn a_path_must_start_with_a_slash_and_a_regex_must_compile() {
        for path in ["", "metrics", "http://example/metrics", "https://example/metrics"] {
            let mut fields = valid_fields(HttpCheck::NAME);
            fields.insert("path".into(), json!(path));
            assert_eq!(HttpCheckFactory.build(&fields).unwrap_err(), "path must start with '/'");
        }
        let mut fields = valid_fields(HttpCheck::NAME);
        fields.insert("regex".into(), json!(true));
        assert_eq!(HttpCheckFactory.build(&fields).unwrap_err(), "regex requires pattern");
        fields.insert("pattern".into(), Value::Null);
        assert_eq!(HttpCheckFactory.build(&fields).unwrap_err(), "regex requires pattern");
        fields.insert("pattern".into(), json!("("));
        assert!(HttpCheckFactory
            .build(&fields)
            .unwrap_err()
            .starts_with("pattern '(' is not a valid regex:"));
    }

    #[test]
    fn the_http_step_requires_its_fields_and_rejects_tls_options() {
        for field in ["on", "endpoint", "path", "status", "timeout"] {
            let mut fields = valid_fields(HttpCheck::NAME);
            fields.remove(field).unwrap();
            let message = HttpCheckFactory.build(&fields).unwrap_err();
            assert!(message.contains(&format!("missing field `{field}`")), "{message}");
        }
        for field in ["https", "insecure_skip_verify"] {
            let mut fields = valid_fields(HttpCheck::NAME);
            fields.insert(field.into(), json!(true));
            let message = HttpCheckFactory.build(&fields).unwrap_err();
            assert!(message.contains(&format!("unknown field `{field}`")), "{message}");
        }
        for status in [
            Value::Null,
            json!(200),
            json!({"equal": "200"}),
            json!({"greater": 200}),
            json!({"equal": 200, "not_equal": 503}),
        ] {
            let mut fields = valid_fields(HttpCheck::NAME);
            fields.insert("status".into(), status);
            assert!(HttpCheckFactory.build(&fields).is_err());
        }
        assert!(HttpCheckFactory.template_fields().is_empty());
    }
}
