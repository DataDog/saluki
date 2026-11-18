//! `http_check`: an HTTP endpoint of a participant answers a `GET` as expected.

use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::time::{sleep_until, Instant};

use super::{stopped, ATTEMPT, POLL};
use crate::core::case::Timeout;
use crate::core::handle::HandleError;
use crate::core::record::StepStatus;
use crate::core::step::{EndpointOperation, Need, Step, StepContext, StepFactory, StepFields, StepReport};

/// The most of a response body a message quotes.
const MAX_QUOTED_BODY_BYTES: usize = 1024;

pub(super) struct HttpCheckFactory;

impl StepFactory for HttpCheckFactory {
    fn name(&self) -> &'static str {
        "http_check"
    }

    fn build(&self, fields: &StepFields) -> Result<Box<dyn Step>, String> {
        Ok(Box::new(fields.decode::<HttpCheck>()?))
    }
}

/// What the response's status must be.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    Equal(u16),
    NotEqual(u16),
}

impl Status {
    fn matches(&self, status: u16) -> bool {
        match self {
            Self::Equal(expected) => status == *expected,
            Self::NotEqual(unexpected) => status != *unexpected,
        }
    }

    fn describe(&self) -> String {
        match self {
            Self::Equal(expected) => format!("status {}", expected),
            Self::NotEqual(unexpected) => format!("a status other than {}", unexpected),
        }
    }
}

/// What the response's body must be.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Body {
    Equal(String),
    Contains(String),
}

impl Body {
    fn matches(&self, body: &str) -> bool {
        match self {
            Self::Equal(expected) => body == expected,
            Self::Contains(part) => body.contains(part),
        }
    }

    fn describe(&self) -> String {
        match self {
            Self::Equal(expected) => format!("a body equal to {:?}", expected),
            Self::Contains(part) => format!("a body containing {:?}", part),
        }
    }
}

/// Passes once a `GET` of `path` at the endpoint answers with the status and, if set, the body, within `timeout`. With
/// `insecure_skip_verify`, the certificate of a TLS endpoint is not checked.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HttpCheck {
    on: String,
    endpoint: String,
    path: String,
    status: Status,
    body: Option<Body>,
    #[serde(default)]
    insecure_skip_verify: bool,
    timeout: Timeout,
}

impl HttpCheck {
    /// What the check expects of a response, for a message.
    fn expected(&self) -> String {
        match &self.body {
            Some(body) => format!("{} and {}", self.status.describe(), body.describe()),
            None => self.status.describe(),
        }
    }
}

#[async_trait]
impl Step for HttpCheck {
    fn name(&self) -> &'static str {
        "http_check"
    }

    fn needs(&self) -> Vec<Need> {
        vec![Need::Endpoint(
            self.on.clone(),
            self.endpoint.clone(),
            EndpointOperation::Request,
        )]
    }

    fn budget(&self) -> Duration {
        self.timeout.duration
    }

    async fn run(&self, cx: &StepContext<'_>) -> StepReport {
        let requested = async {
            let handle = cx.handle(&self.on)?;
            let last = loop {
                let limit = ATTEMPT.min(cx.deadline.saturating_duration_since(Instant::now()));
                let seen = match handle
                    .http_get(&self.endpoint, &self.path, self.insecure_skip_verify, limit)
                    .await
                {
                    Ok(response) => {
                        let body = String::from_utf8_lossy(&response.body);
                        if !self.status.matches(response.status) {
                            format!("status {}", response.status)
                        } else if self.body.as_ref().is_none_or(|expected| expected.matches(&body)) {
                            let message = format!(
                                "GET {} at endpoint '{}' of '{}' answered status {}.",
                                self.path, self.endpoint, self.on, response.status
                            );
                            return Ok(StepReport {
                                status: StepStatus::Passed,
                                message,
                            });
                        } else {
                            let shown = &body[..body.floor_char_boundary(MAX_QUOTED_BODY_BYTES)];
                            let cut = if shown.len() < body.len() { " (truncated)" } else { "" };
                            format!("status {} with body {:?}{}", response.status, shown, cut)
                        }
                    }
                    Err(HandleError::Software(message)) => message,
                    Err(e) => return Err(e),
                };
                if Instant::now() >= cx.deadline {
                    break seen;
                }
                sleep_until((Instant::now() + POLL).min(cx.deadline)).await;
            };
            let message = format!(
                "GET {} at endpoint '{}' of '{}' did not answer {} within {}: last {}",
                self.path,
                self.endpoint,
                self.on,
                self.expected(),
                self.timeout.text,
                last
            );
            Ok::<_, HandleError>(StepReport {
                status: StepStatus::Failed,
                message,
            })
        };
        requested.await.unwrap_or_else(stopped)
    }
}
