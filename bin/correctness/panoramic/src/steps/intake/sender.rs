//! `intake_from`: selected telemetry reached an intake from the expected sender, and from no other, the expected number
//! of times.
//!
//! The step reads the requests that a `datadog-intake` participant recorded. A record holds the path and headers of one
//! request, and the items that the request carried. The step reads them with `GET` requests to the `api` endpoint of
//! the intake, so the participant needs no operation for it. A `telemetry` value picks the route and the item to count:
//!
//! - `metrics`: `/metrics/dump_requests`. An item is a data point of a metric whose name starts with `name_prefix`.
//! - `spans`: `/traces/dump_span_requests`. An item is a span of the `service`.
//! - `trace_stats`: `/traces/dump_stats_requests`. An item is a group of statistics of the `service`.
//! - `logs`: `/logs/dump`. An item is a log of the `service`.
//!
//! The `User-Agent` header of a request names its sender: `agent-data-plane/` is `adp`, `datadog-agent/` is `agent`,
//! and `Datadog Trace Agent/` is `trace-agent`. A request with any other `User-Agent`, or none, is unattributable.
//! A request that carries no selected item does not count, so background telemetry does not matter.
//!
//! ```yaml
//! - step: intake_from
//!   on: intake
//!   telemetry: metrics
//!   select: {name_prefix: "otlp.routing.mixed-grpc.grpc."}
//!   sender: adp
//!   count: 3
//!   settle: 20s
//!   timeout: 2m
//! ```
//!
//! The step fails when any selected item came from another sender, or from an unattributable request, even if the
//! expected sender also delivered. It fails when more than `count` selected items came from the expected sender. It
//! passes once `count` items arrived and the result held for `settle`, so a late duplicate or a second sender still
//! fails the step. Without `count`, one item is enough, and extra items are fine. The step fails when fewer than `count`
//! items arrived within `timeout`. A failure keeps the requests that carried selected items, with their headers, as a
//! file of the intake. The value of a `DD-API-KEY`, `DD-APPLICATION-KEY` or `Authorization` header is shown as
//! `<redacted>`.
//!
//! Two encodings of the same metrics decode to the same items, and only the path of the request tells them apart. With
//! `path`, the step also fails when any selected item came on another path, even if `count` items arrived on `path`:
//!
//! ```yaml
//! - step: intake_from
//!   on: intake-agent
//!   telemetry: metrics
//!   select: {name_prefix: "otlpprobe.v3.series."}
//!   sender: agent
//!   path: /api/intake/metrics/v3/series
//!   count: 3
//!   timeout: 1m
//! ```
//!
//! The options of the step:
//!
//! - `on`: the intake participant.
//! - `telemetry`: `metrics`, `spans`, `trace_stats` or `logs`.
//! - `select`: `{name_prefix: <prefix>}` for `metrics`, and `{service: <name>}` for the others.
//! - `sender`: `adp`, `agent` or `trace-agent`.
//! - `path`: the path of the requests that carry the selected items, such as `/api/intake/metrics/v3/series`. It starts
//!   with `/`. Without it, the path does not matter.
//! - `count`: the exact number of items. Not allowed for `trace_stats`, whose buckets vary, so the step only checks that
//!   statistics arrived from the sender.
//! - `settle`: how long to keep watching after the result first holds. Defaults to `20s`.
//! - `timeout`: how long to wait for the result to first hold.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::future::Future;
use std::num::NonZeroU64;
use std::time::Duration;

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use stele::{ClientStatisticsAggregator, IntakeRequest, LogsRequest, Metric, Span};
use tokio::time::{sleep_until, Instant};

use crate::core::case::Timeout;
use crate::core::handle::{Handle, HandleError};
use crate::core::probe::{POLL_INTERVAL, PROBE_TIMEOUT};
use crate::core::record::StepStatus;
use crate::core::spec::Keep;
use crate::core::step::{EndpointOperation, Need, Step, StepContext, StepFactory, StepFields, StepReport};
use crate::steps::stopped;

/// The endpoint of the intake that the step reads.
const API: &str = "api";

/// The `User-Agent` prefix of each sender.
const USER_AGENTS: [(&str, Sender); 3] = [
    ("agent-data-plane/", Sender::Adp),
    ("datadog-agent/", Sender::Agent),
    ("Datadog Trace Agent/", Sender::TraceAgent),
];

/// The header that names the sender of a request, in lower case.
const USER_AGENT: &str = "user-agent";

/// What a failure message calls a request that no sender claims.
const UNATTRIBUTABLE: &str = "unattributable";

/// Most requests a failure message quotes. The kept file holds all of them.
const MAX_QUOTED: usize = 10;

/// Headers that carry a secret, in lower case. A failure shows [`REDACTED`] as their value.
const SECRET_HEADERS: [&str; 3] = ["dd-api-key", "dd-application-key", "authorization"];

/// What a failure shows as the value of a header in [`SECRET_HEADERS`].
const REDACTED: &str = "<redacted>";

pub(crate) struct IntakeFromFactory;

impl StepFactory for IntakeFromFactory {
    fn name(&self) -> &'static str {
        "intake_from"
    }

    fn build(&self, fields: &StepFields) -> Result<Box<dyn Step>, String> {
        Ok(Box::new(IntakeFrom::new(fields.decode()?)?))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Fields {
    on: String,
    telemetry: Telemetry,
    select: Select,
    sender: Sender,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    count: Option<NonZeroU64>,
    #[serde(default = "default_settle")]
    settle: Timeout,
    timeout: Timeout,
}

fn default_settle() -> Timeout {
    Timeout {
        text: "20s".to_string(),
        duration: Duration::from_secs(20),
    }
}

/// The telemetry that the step reads.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Telemetry {
    Metrics,
    Spans,
    TraceStats,
    Logs,
}

impl Telemetry {
    /// The name of the telemetry, as a case writes it.
    fn name(self) -> &'static str {
        match self {
            Self::Metrics => "metrics",
            Self::Spans => "spans",
            Self::TraceStats => "trace_stats",
            Self::Logs => "logs",
        }
    }

    /// The route of the intake that answers with the requests it recorded.
    fn route(self) -> &'static str {
        match self {
            Self::Metrics => "/metrics/dump_requests",
            Self::Spans => "/traces/dump_span_requests",
            Self::TraceStats => "/traces/dump_stats_requests",
            Self::Logs => "/logs/dump",
        }
    }
}

/// How the case selects the items of its fixtures.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Select {
    NamePrefix(String),
    Service(String),
}

/// The software that sends telemetry to an intake.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "kebab-case")]
enum Sender {
    Adp,
    Agent,
    TraceAgent,
}

impl Sender {
    /// The sender whose `User-Agent` prefix `user_agent` starts with, or `None` if no sender claims it.
    fn of(user_agent: &str) -> Option<Self> {
        USER_AGENTS
            .iter()
            .find(|(prefix, _)| user_agent.starts_with(prefix))
            .map(|(_, sender)| *sender)
    }

    fn name(self) -> &'static str {
        match self {
            Self::Adp => "adp",
            Self::Agent => "agent",
            Self::TraceAgent => "trace-agent",
        }
    }
}

/// The telemetry that the step reads, with the selection of its items.
#[derive(Debug)]
struct Target {
    telemetry: Telemetry,
    /// The name prefix of metrics, or the service of spans, statistics and logs, that selects the items.
    selector: String,
}

impl Target {
    /// Pairs `telemetry` with its selection.
    ///
    /// # Errors
    ///
    /// Returns an error if `select` is not the selection of `telemetry`, or selects every item.
    fn new(telemetry: Telemetry, select: Select) -> Result<Self, String> {
        let selector = match (telemetry, select) {
            (Telemetry::Metrics, Select::NamePrefix(prefix)) => prefix,
            (Telemetry::Spans | Telemetry::TraceStats | Telemetry::Logs, Select::Service(service)) => service,
            (Telemetry::Metrics, Select::Service(_)) => {
                return Err("metrics are selected by 'name_prefix', not 'service'".to_string())
            }
            (telemetry, Select::NamePrefix(_)) => {
                return Err(format!(
                    "{} are selected by 'service', not 'name_prefix'",
                    telemetry.name()
                ))
            }
        };
        if selector.is_empty() {
            return Err("'select' is empty, so it selects every item, not only the fixtures".to_string());
        }
        Ok(Self { telemetry, selector })
    }

    fn describe(&self) -> String {
        let selector = &self.selector;
        match self.telemetry {
            Telemetry::Metrics => format!("metric data points with a name that starts with '{}'", selector),
            Telemetry::Spans => format!("spans of service '{}'", selector),
            Telemetry::TraceStats => format!("groups of trace statistics of service '{}'", selector),
            Telemetry::Logs => format!("logs of service '{}'", selector),
        }
    }

    /// A file name that is safe on every host. Two steps share it if they have the same telemetry, sender and selector,
    /// or selectors that differ only in replaced characters or after the first 60.
    fn file_name(&self, sender: Sender) -> String {
        let selector: String = self
            .selector
            .chars()
            .take(60)
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' })
            .collect();
        format!(
            "intake_from-{}-{}-{}.txt",
            self.telemetry.name(),
            sender.name(),
            selector
        )
    }

    /// Decodes the answer of the intake at the route of the telemetry, and keeps the requests that carried selected
    /// items.
    ///
    /// # Errors
    ///
    /// Returns an error if the answer does not decode as the records of the telemetry.
    fn selected(&self, body: &[u8]) -> Result<Tally, String> {
        fn decode<T: DeserializeOwned>(body: &[u8]) -> Result<T, String> {
            serde_json::from_slice(body).map_err(|e| {
                if body.is_empty() {
                    "the intake answered with an empty body (check that it serves this route)".to_string()
                } else {
                    e.to_string()
                }
            })
        }

        let selector = self.selector.as_str();
        Ok(match self.telemetry {
            Telemetry::Metrics => {
                let requests: Vec<IntakeRequest<Vec<Metric>>> = decode(body)?;
                Tally::of(requests.iter().map(|request| {
                    let points = request
                        .items()
                        .iter()
                        .filter(|metric| metric.context().name().starts_with(selector))
                        .map(|metric| metric.values().len() as u64)
                        .sum::<u64>();
                    (request.path(), request.headers(), points)
                }))
            }
            Telemetry::Spans => {
                let requests: Vec<IntakeRequest<Vec<Span>>> = decode(body)?;
                Tally::of(requests.iter().map(|request| {
                    let spans = request.items().iter().filter(|span| span.service() == selector).count();
                    (request.path(), request.headers(), spans as u64)
                }))
            }
            Telemetry::TraceStats => {
                let requests: Vec<IntakeRequest<ClientStatisticsAggregator>> = decode(body)?;
                Tally::of(requests.iter().map(|request| {
                    let groups = request
                        .items()
                        .groups()
                        .values()
                        .flat_map(|group| group.buckets())
                        .filter(|(_, statistics)| statistics.service() == selector)
                        .count();
                    (request.path(), request.headers(), groups as u64)
                }))
            }
            Telemetry::Logs => {
                let requests: Vec<LogsRequest> = decode(body)?;
                Tally::of(requests.iter().map(|request| {
                    let logs = request
                        .logs()
                        .iter()
                        .filter(|log| log.service().and_then(|value| value.as_str()) == Some(selector))
                        .count();
                    (request.path(), request.headers(), logs as u64)
                }))
            }
        })
    }
}

/// A request that carried selected items.
#[derive(Debug)]
struct Selected {
    path: String,
    /// The headers of the request, with the value of each secret header replaced.
    headers: BTreeMap<String, String>,
    items: u64,
}

impl Selected {
    fn user_agent(&self) -> Option<&str> {
        self.headers.get(USER_AGENT).map(String::as_str)
    }

    fn sender(&self) -> Option<Sender> {
        self.user_agent().and_then(Sender::of)
    }

    fn sender_name(&self) -> &'static str {
        self.sender().map_or(UNATTRIBUTABLE, Sender::name)
    }

    /// One line for a failure message: the sender, the path, the `User-Agent` and every other header.
    fn line(&self) -> String {
        let headers: Vec<String> = self
            .headers
            .iter()
            .filter(|(name, _)| name.as_str() != USER_AGENT)
            .map(|(name, value)| format!("{}: {}", name, value))
            .collect();
        format!(
            "{} {}: {} item(s), user-agent {}; headers: {}",
            self.sender_name(),
            self.path,
            self.items,
            self.user_agent()
                .map_or("none".to_string(), |agent| format!("'{}'", agent)),
            headers.join(", ")
        )
    }
}

/// The requests that carried selected items, in the order the intake received them.
#[derive(Debug)]
struct Tally(Vec<Selected>);

impl Tally {
    /// Keeps the requests that carried items, from each request's path, headers, and number of selected items. The
    /// value of each secret header is replaced here, so nothing that the step reports can show it.
    fn of<'a>(requests: impl IntoIterator<Item = (&'a str, &'a BTreeMap<String, String>, u64)>) -> Self {
        Self(
            requests
                .into_iter()
                .filter(|(_, _, items)| *items > 0)
                .map(|(path, headers, items)| Selected {
                    path: path.to_string(),
                    headers: headers
                        .iter()
                        .map(|(name, value)| {
                            let secret = SECRET_HEADERS.iter().any(|secret| name.eq_ignore_ascii_case(secret));
                            (name.clone(), if secret { REDACTED } else { value.as_str() }.to_string())
                        })
                        .collect(),
                    items,
                })
                .collect(),
        )
    }

    /// The items that requests of `sender` carried.
    fn items_from(&self, sender: Sender) -> u64 {
        self.0
            .iter()
            .filter(|request| request.sender() == Some(sender))
            .map(|request| request.items)
            .sum()
    }

    fn total(&self) -> u64 {
        self.0.iter().map(|request| request.items).sum()
    }

    /// Whether a request that carried selected items arrived on a path other than `path`.
    fn reached_other_than(&self, path: &str) -> bool {
        self.0.iter().any(|request| request.path != path)
    }

    /// The items of each sender, such as `adp 3, agent 1`.
    fn counts(&self) -> String {
        let mut counts: BTreeMap<&str, u64> = BTreeMap::new();
        for request in &self.0 {
            *counts.entry(request.sender_name()).or_default() += request.items;
        }
        if counts.is_empty() {
            return "none".to_string();
        }
        counts
            .iter()
            .map(|(sender, items)| format!("{} {}", sender, items))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// How the items that arrived compare with what the step expects.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Check {
    /// Fewer items than expected arrived from the sender.
    Short,
    /// The expected sender delivered the expected number of items, and nobody else delivered any.
    Met,
    /// Another sender, or an unattributable request, delivered selected items.
    Foreign,
    /// The expected sender delivered selected items on a path other than `path`.
    Misrouted,
    /// The expected sender delivered more items than `count`.
    Duplicate,
}

/// What a watch of the intake found.
#[derive(Debug)]
struct Watched {
    check: Check,
    tally: Tally,
    /// Whether the check was `Met` earlier, so the watch was in its settle.
    settling: bool,
}

/// Passes once selected items arrived from the expected sender, and nothing else arrived for the settle.
#[derive(Debug)]
struct IntakeFrom {
    on: String,
    target: Target,
    sender: Sender,
    /// The path of the requests that carry the selected items, or `None` for any path.
    path: Option<String>,
    /// The exact number of items, or `None` for at least one.
    count: Option<u64>,
    settle: Timeout,
    timeout: Timeout,
}

impl IntakeFrom {
    /// Checks that the fields of the step agree.
    ///
    /// # Errors
    ///
    /// Returns an error if the selection does not suit the telemetry, `count` is set for `trace_stats`, or `path` does
    /// not start with `/`.
    fn new(fields: Fields) -> Result<Self, String> {
        let Fields {
            on,
            telemetry,
            select,
            sender,
            path,
            count,
            settle,
            timeout,
        } = fields;
        if telemetry == Telemetry::TraceStats && count.is_some() {
            return Err(
                "'count' is not an option for trace_stats: the buckets of statistics vary, so the step checks only \
                 that statistics arrive from the sender"
                    .to_string(),
            );
        }
        if let Some(path) = path.as_deref().filter(|path| !path.starts_with('/')) {
            return Err(format!("'path' must start with '/', and '{}' does not", path));
        }
        Ok(Self {
            on,
            target: Target::new(telemetry, select)?,
            sender,
            path,
            count: count.map(NonZeroU64::get),
            settle,
            timeout,
        })
    }

    fn check(&self, tally: &Tally) -> Check {
        let have = tally.items_from(self.sender);
        if tally.total() > have {
            return Check::Foreign;
        }
        if self.path.as_deref().is_some_and(|path| tally.reached_other_than(path)) {
            return Check::Misrouted;
        }
        match self.count {
            Some(count) if have > count => Check::Duplicate,
            Some(count) if have < count => Check::Short,
            None if have == 0 => Check::Short,
            _ => Check::Met,
        }
    }

    /// Reads the intake with `fetch` until the check fails, or until the settle that follows its first success ends,
    /// or until the timeout ends without a success.
    ///
    /// The intake answers with every request it recorded so far. A read that fails does not lose a request, so the step
    /// goes on after one. Only the last read must succeed.
    ///
    /// The intake clones its records before it answers, so a read can miss a request that arrives while the read is in
    /// flight. The settle therefore ends with a read that starts after the settle ends. A read that starts earlier
    /// stops at the end of the settle, so the last read still fits in the budget.
    ///
    /// # Errors
    ///
    /// Returns a harness error if an answer does not decode, and the error of the last read if it failed.
    async fn watch<F, Fut>(&self, deadline: Instant, mut fetch: F) -> Result<Watched, HandleError>
    where
        F: FnMut(Duration) -> Fut,
        Fut: Future<Output = Result<Vec<u8>, HandleError>>,
    {
        let give_up = Instant::now() + self.timeout.duration;
        let mut held_since = None;
        let last = loop {
            let started = Instant::now();
            let mut limit = PROBE_TIMEOUT.min(deadline.saturating_duration_since(started));
            if let Some(since) = held_since {
                let end = since + self.settle.duration;
                if started < end {
                    limit = limit.min(end.saturating_duration_since(started));
                }
            }
            let read = match fetch(limit).await {
                Ok(body) => {
                    let tally = self.target.selected(&body).map_err(|why| {
                        HandleError::Harness(format!(
                            "the records that '{}' answers at {} do not decode: {}",
                            self.on,
                            self.target.telemetry.route(),
                            why
                        ))
                    })?;
                    let check = self.check(&tally);
                    match check {
                        Check::Foreign | Check::Misrouted | Check::Duplicate => break Ok((check, tally)),
                        Check::Met => {
                            held_since.get_or_insert_with(Instant::now);
                        }
                        Check::Short => {}
                    }
                    Ok((check, tally))
                }
                Err(e) => Err(e),
            };
            let now = Instant::now();
            let end = held_since.map_or(give_up, |since| since + self.settle.duration);
            // A read that started before the settle ended can have missed a request, so it does not end the settle.
            if now >= end && (held_since.is_none() || started >= end) {
                break read;
            }
            sleep_until((now + POLL_INTERVAL).min(end)).await;
        };
        match last {
            Ok((check, tally)) => Ok(Watched {
                check,
                tally,
                settling: held_since.is_some(),
            }),
            Err(error) => {
                let context = |why: String| {
                    format!(
                        "the last read of {} from '{}' failed: {}",
                        self.target.telemetry.route(),
                        self.on,
                        why
                    )
                };
                Err(match error {
                    HandleError::Harness(why) => HandleError::Harness(context(why)),
                    HandleError::Software(why) => HandleError::Software(context(why)),
                })
            }
        }
    }

    /// Watches the intake, with `fetch` reading it, and reports what it found. A failure keeps the requests that
    /// carried selected items.
    async fn run_with<F, Fut>(&self, cx: &StepContext<'_>, fetch: F) -> StepReport
    where
        F: FnMut(Duration) -> Fut,
        Fut: Future<Output = Result<Vec<u8>, HandleError>>,
    {
        let Watched { check, tally, settling } = match self.watch(cx.deadline, fetch).await {
            Ok(watched) => watched,
            Err(e) => return stopped(e),
        };
        let Some(mut message) = self.failure(check, &tally) else {
            let path = self.path.as_ref().map_or(String::new(), |path| format!(" on {}", path));
            return StepReport {
                status: StepStatus::Passed,
                message: format!(
                    "{} selected item(s) reached '{}' from {}{} and from no other sender, and the result held for {}.",
                    tally.total(),
                    self.on,
                    self.sender.name(),
                    path,
                    self.settle.text
                ),
            };
        };

        if settling {
            let _ = write!(
                message,
                " It happened after the result first held, within the settle of {}.",
                self.settle.text
            );
        }
        let _ = write!(
            message,
            "\nSelection: {}. Expected sender: {}.",
            self.target.describe(),
            self.sender.name()
        );
        if let Some(path) = &self.path {
            let _ = write!(message, " Expected path: {}.", path);
        }
        let _ = write!(message, "\nItems by sender: {}.", tally.counts());
        let lines: Vec<String> = tally.0.iter().map(Selected::line).collect();
        if !lines.is_empty() {
            let file = self.target.file_name(self.sender);
            cx.keep(&self.on, &file, lines.join("\n").as_bytes(), Keep::Always)
                .await;
            let _ = write!(
                message,
                "\n{} request(s) carried selected items, {} quoted, all kept as {} of '{}':",
                lines.len(),
                lines.len().min(MAX_QUOTED),
                file,
                self.on
            );
            for line in lines.iter().take(MAX_QUOTED) {
                let _ = write!(message, "\n  {}", line);
            }
        }
        StepReport {
            status: StepStatus::Failed,
            message,
        }
    }

    /// The first sentence of the message for a check that failed, or `None` if the check held.
    fn failure(&self, check: Check, tally: &Tally) -> Option<String> {
        let (on, sender) = (&self.on, self.sender.name());
        let have = tally.items_from(self.sender);
        let expected = match self.count {
            Some(count) => format!("exactly {}", count),
            None => "at least 1".to_string(),
        };
        Some(match check {
            Check::Met => return None,
            Check::Foreign => format!("Selected items reached '{}' from a sender other than {}.", on, sender),
            Check::Misrouted => format!(
                "Selected items reached '{}' on a path other than '{}'.",
                on,
                self.path.as_deref().unwrap_or_default()
            ),
            Check::Duplicate => format!(
                "{} selected item(s) reached '{}' from {}, and the step expects {}.",
                have, on, sender, expected
            ),
            Check::Short => format!(
                "{} selected item(s) reached '{}' from {} within {}, and the step expects {}.",
                have, on, sender, self.timeout.text, expected
            ),
        })
    }
}

#[async_trait]
impl Step for IntakeFrom {
    fn name(&self) -> &'static str {
        "intake_from"
    }

    fn needs(&self) -> Vec<Need> {
        vec![Need::Endpoint(
            self.on.clone(),
            API.to_string(),
            EndpointOperation::Request,
        )]
    }

    /// The wait for the first success, the read that can end after it, the settle, and the read that ends the watch.
    fn budget(&self) -> Duration {
        self.timeout.duration + self.settle.duration + PROBE_TIMEOUT * 2
    }

    async fn run(&self, cx: &StepContext<'_>) -> StepReport {
        match cx.handle(&self.on) {
            Ok(handle) => {
                self.run_with(cx, |limit| read(handle, self.target.telemetry.route(), limit))
                    .await
            }
            Err(e) => stopped(e),
        }
    }
}

/// Reads the answer of the intake at `route`. This is the only request the step makes.
async fn read(handle: &Handle<'_>, route: &str, limit: Duration) -> Result<Vec<u8>, HandleError> {
    let response = handle.http_get(API, route, false, true, limit).await?;
    if response.status != 200 {
        return Err(HandleError::Software(format!(
            "the intake answered {} with status {}",
            route, response.status
        )));
    }
    Ok(response.body)
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use serde_json::{json, Value};
    use tokio::io::{AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
    use tokio::net::TcpListener;

    use super::*;
    use crate::core::backend::{Exec, Realized};
    use crate::core::case::CASE_FILE;
    use crate::core::observe::{Kept, Observation};
    use crate::core::participant::Os;
    use crate::core::reference::Values;

    const ADP: &str = "agent-data-plane/1.9.0";
    const AGENT: &str = "datadog-agent/7.87.0";
    const TRACE_AGENT: &str = "Datadog Trace Agent/7.87.0/0123abc";

    const V3_SERIES: &str = "/api/intake/metrics/v3/series";
    const V2_SERIES: &str = "/api/v2/series";

    const TELEMETRIES: [Telemetry; 4] = [
        Telemetry::Metrics,
        Telemetry::Spans,
        Telemetry::TraceStats,
        Telemetry::Logs,
    ];

    fn span(service: &str) -> Value {
        json!({
            "agent_metadata": {
                "hostname": "", "env": "", "tags": {}, "agent_version": "", "target_tps": 1.0, "error_tps": 1.0,
                "rare_sampler_enabled": false,
            },
            "tracer_metadata": {
                "container_id": "", "language_name": "", "language_version": "", "tracer_version": "",
                "runtime_id": "", "tags": {}, "env": "", "hostname": "", "app_version": "",
            },
            "trace_chunk_metadata": {"priority": 1, "origin": "", "tags": {}, "dropped_trace": false},
            "service": service, "name": "op", "resource": "res", "trace_id": 1, "span_id": 2, "parent_id": 0,
            "start": 0, "duration": 1, "error": 0, "meta": {}, "metrics": {}, "type_": "", "meta_struct": {},
            "span_links": [], "span_events": [],
        })
    }

    /// A group of statistics of `service`, with the sketches of an empty bucket.
    fn statistics(service: &str) -> Value {
        json!({
            "agent_metadata": {"hostname": "agent", "env": "", "agent_version": "", "client_computed": false},
            "tracer_metadata": {
                "hostname": "host", "env": "", "version": "", "service": "", "language": "", "tracer_version": "",
                "runtime_id": "", "sequence": 0, "agent_aggregation": "", "container_id": "", "tags": [],
                "git_commit_sha": "", "image_tag": "", "process_tags_hash": 0, "process_tags": "",
            },
            "buckets": {"[0,10]": {
                "service": service, "name": "op", "resource": "res", "type_": "", "hits": 1, "errors": 0,
                "duration_ns": 0, "ok_summary": "CgkJ/UqBWr9S8D8SChIIAAAAAAAA8D8=",
                "error_summary": "CgkJ/UqBWr9S8D8SChIIAAAAAAAA8D8=", "synthetics": false, "top_level_hits": 0,
                "span_kind": "", "peer_tags": [], "is_trace_root": null, "db_type": "", "grpc_status_code": "",
                "http_status_code": 0, "http_method": "", "http_endpoint": "",
            }},
        })
    }

    /// The path that a request of `telemetry` has unless a test gives another.
    fn default_path(telemetry: Telemetry) -> &'static str {
        match telemetry {
            Telemetry::Metrics => "/api/v2/series",
            Telemetry::Spans => "/api/v0.2/traces",
            Telemetry::TraceStats => "/api/v0.2/stats",
            Telemetry::Logs => "/api/v2/logs",
        }
    }

    /// A recorded request of `telemetry` from `user_agent`, with `selected` items of the fixtures that the steps below
    /// select, and `other` items that they do not.
    fn request(telemetry: Telemetry, user_agent: Option<&str>, selected: usize, other: usize) -> Value {
        request_at(telemetry, default_path(telemetry), user_agent, selected, other)
    }

    /// As [`request`], with the request at `path`.
    fn request_at(telemetry: Telemetry, path: &str, user_agent: Option<&str>, selected: usize, other: usize) -> Value {
        let mut headers = json!({"content-type": "application/x-protobuf"});
        if let Some(user_agent) = user_agent {
            headers["user-agent"] = json!(user_agent);
        }
        let point = json!([0, {"mtype": "Count", "value": 1.0}]);
        let (key, items) = match telemetry {
            Telemetry::Metrics => (
                "items",
                json!([
                    {"context": {"name": "fx.count", "tags": []}, "values": vec![point.clone(); selected]},
                    {"context": {"name": "datadog.agent.otlp.metrics", "tags": []}, "values": vec![point; other]},
                ]),
            ),
            Telemetry::Spans => (
                "items",
                json!((0..selected)
                    .map(|_| span("fx"))
                    .chain((0..other).map(|_| span("other")))
                    .collect::<Vec<_>>()),
            ),
            Telemetry::TraceStats => {
                let services = (0..selected).map(|_| "fx").chain((0..other).map(|_| "other"));
                let groups: serde_json::Map<String, Value> = services
                    .enumerate()
                    .map(|(key, service)| (key.to_string(), statistics(service)))
                    .collect();
                ("items", json!({"groups": groups}))
            }
            Telemetry::Logs => {
                let log = |service| json!({"service": service, "ddtags": [], "envelope_properties": {}});
                (
                    "logs",
                    json!((0..selected)
                        .map(|_| log("fx"))
                        .chain((0..other).map(|_| log("other")))
                        .collect::<Vec<_>>()),
                )
            }
        };
        json!({"path": path, "headers": headers, key: items})
    }

    /// The answer of an intake that recorded `requests`.
    fn records(requests: Vec<Value>) -> Result<Vec<u8>, HandleError> {
        Ok(Value::Array(requests).to_string().into_bytes())
    }

    fn step(fields: &str) -> IntakeFrom {
        let fields = StepFields(serde_saphyr::from_str(fields).unwrap());
        IntakeFrom::new(fields.decode().unwrap()).unwrap()
    }

    /// The step that selects the fixtures of `telemetry` and expects them from ADP, three times unless `telemetry` is
    /// `trace_stats`.
    fn adp_step(telemetry: Telemetry) -> IntakeFrom {
        let (select, count) = match telemetry {
            Telemetry::Metrics => ("{name_prefix: 'fx.'}", ", count: 3"),
            Telemetry::TraceStats => ("{service: fx}", ""),
            Telemetry::Spans | Telemetry::Logs => ("{service: fx}", ", count: 3"),
        };
        step(&format!(
            "{{on: intake, telemetry: {}, select: {}, sender: adp, settle: 2s, timeout: 10s{}}}",
            telemetry.name(),
            select,
            count
        ))
    }

    /// The step on metrics, to expect `count` items of ADP.
    fn metrics_step(count: Option<u64>) -> IntakeFrom {
        let count = count.map(|count| format!(", count: {}", count)).unwrap_or_default();
        step(&format!(
            "{{on: intake, telemetry: metrics, select: {{name_prefix: 'fx.'}}, sender: adp, settle: 2s, timeout: 10s{}}}",
            count
        ))
    }

    /// The step on metrics that expects the three items of ADP on `path`.
    fn path_step(path: &str) -> IntakeFrom {
        step(&format!(
            "{{on: intake, telemetry: metrics, select: {{name_prefix: 'fx.'}}, sender: adp, path: {}, count: 3, settle: 2s, \
             timeout: 10s}}",
            path
        ))
    }

    struct Outcome {
        report: StepReport,
        /// The name and contents of each file the step kept.
        kept: Vec<(String, String)>,
        elapsed: Duration,
        reads: usize,
    }

    /// Runs `step` against an intake whose reads answer with `answers` in turn, and with the last one again after
    /// them.
    async fn watch(step: &IntakeFrom, answers: Vec<Result<Vec<u8>, HandleError>>) -> Outcome {
        let suite = tempfile::tempdir().unwrap();
        let kept = Kept::new(&suite.path().join("logs"));
        let handles = BTreeMap::new();
        let values = Values::of_plan(&BTreeMap::new());
        let started = Instant::now();
        let cx = StepContext::new(&handles, &values, &kept, started + step.budget());
        let mut reads = 0;
        let report = step
            .run_with(&cx, |_| {
                let answer = answers[reads.min(answers.len() - 1)].clone();
                reads += 1;
                async move { answer }
            })
            .await;
        let kept = kept
            .files()
            .iter()
            .map(|file| {
                let name = file.path.file_name().unwrap().to_string_lossy().into_owned();
                (name, std::fs::read_to_string(&file.path).unwrap())
            })
            .collect();
        Outcome {
            report,
            kept,
            elapsed: started.elapsed(),
            reads,
        }
    }

    #[track_caller]
    fn assert_status(outcome: &Outcome, status: StepStatus) {
        assert_eq!(outcome.report.status, status, "{}", outcome.report.message);
    }

    #[track_caller]
    fn assert_mentions(outcome: &Outcome, parts: &[&str]) {
        for part in parts {
            assert!(
                outcome.report.message.contains(part),
                "'{}' is not in: {}",
                part,
                outcome.report.message
            );
        }
    }

    #[test]
    fn a_user_agent_names_its_sender_by_prefix() {
        let cases = [
            (ADP, Some(Sender::Adp)),
            (AGENT, Some(Sender::Agent)),
            (TRACE_AGENT, Some(Sender::TraceAgent)),
            ("agent-data-plane", None),
            ("Go-http-client/1.1", None),
            ("", None),
        ];
        for (user_agent, sender) in cases {
            assert_eq!(Sender::of(user_agent), sender, "{}", user_agent);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_selected_items_of_the_expected_sender_pass_after_the_settle_for_every_telemetry() {
        for telemetry in TELEMETRIES {
            // Background items in the same request, and a request of another sender that holds nothing selected,
            // are not fixtures.
            let answer = records(vec![
                request(telemetry, Some(ADP), 3, 2),
                request(telemetry, Some(AGENT), 0, 4),
                request(telemetry, None, 0, 1),
            ]);
            let outcome = watch(&adp_step(telemetry), vec![answer]).await;
            assert_status(&outcome, StepStatus::Passed);
            assert_eq!(outcome.elapsed, Duration::from_secs(2), "{}", telemetry.name());
            assert!(outcome.kept.is_empty(), "{}", telemetry.name());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_selected_items_on_the_path_pass_after_the_settle() {
        let answer = records(vec![
            request_at(Telemetry::Metrics, V3_SERIES, Some(ADP), 2, 1),
            request_at(Telemetry::Metrics, V3_SERIES, Some(ADP), 1, 0),
        ]);
        let outcome = watch(&path_step(V3_SERIES), vec![answer]).await;
        assert_status(&outcome, StepStatus::Passed);
        assert_eq!(outcome.elapsed, Duration::from_secs(2));
        assert_mentions(
            &outcome,
            &["from adp on /api/intake/metrics/v3/series and from no other sender"],
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_selected_item_on_another_path_fails_at_once_although_the_path_holds_the_count() {
        // A fallback that sends the same items again on the older path.
        let answer = records(vec![
            request_at(Telemetry::Metrics, V3_SERIES, Some(ADP), 3, 0),
            request_at(Telemetry::Metrics, V2_SERIES, Some(ADP), 3, 0),
        ]);
        let outcome = watch(&path_step(V3_SERIES), vec![answer]).await;
        assert_status(&outcome, StepStatus::Failed);
        assert_mentions(
            &outcome,
            &[
                "Selected items reached 'intake' on a path other than '/api/intake/metrics/v3/series'.",
                "Expected path: /api/intake/metrics/v3/series.",
                "Items by sender: adp 6.",
                "adp /api/intake/metrics/v3/series: 3 item(s)",
                "adp /api/v2/series: 3 item(s)",
            ],
        );
        assert_eq!(outcome.elapsed, Duration::ZERO);
        assert_eq!(outcome.reads, 1);
        assert_eq!(outcome.kept.len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn items_on_the_wrong_path_fail_when_none_arrive_on_the_right_one() {
        let answer = records(vec![request_at(Telemetry::Metrics, V2_SERIES, Some(ADP), 3, 0)]);
        let outcome = watch(&path_step(V3_SERIES), vec![answer]).await;
        assert_status(&outcome, StepStatus::Failed);
        assert_mentions(&outcome, &["on a path other than '/api/intake/metrics/v3/series'"]);
        assert_eq!(outcome.elapsed, Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn a_late_selected_item_on_another_path_fails_during_the_settle() {
        let held = || records(vec![request_at(Telemetry::Metrics, V3_SERIES, Some(ADP), 3, 0)]);
        let fallback = records(vec![
            request_at(Telemetry::Metrics, V3_SERIES, Some(ADP), 3, 0),
            request_at(Telemetry::Metrics, V2_SERIES, Some(ADP), 1, 0),
        ]);
        let outcome = watch(&path_step(V3_SERIES), vec![held(), held(), fallback]).await;
        assert_status(&outcome, StepStatus::Failed);
        assert_mentions(&outcome, &["on a path other than", "after the result first held"]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_sender_other_than_the_expected_one_fails_before_the_path_is_checked() {
        let answer = records(vec![
            request_at(Telemetry::Metrics, V3_SERIES, Some(ADP), 3, 0),
            request_at(Telemetry::Metrics, V2_SERIES, Some(AGENT), 1, 0),
        ]);
        let outcome = watch(&path_step(V3_SERIES), vec![answer]).await;
        assert_status(&outcome, StepStatus::Failed);
        assert_mentions(&outcome, &["from a sender other than adp"]);
    }

    #[tokio::test(start_paused = true)]
    async fn unselected_items_on_another_path_are_ignored() {
        let answer = records(vec![
            request_at(Telemetry::Metrics, V3_SERIES, Some(ADP), 3, 0),
            request_at(Telemetry::Metrics, V2_SERIES, Some(ADP), 0, 4),
            request_at(Telemetry::Metrics, V2_SERIES, Some(AGENT), 0, 4),
        ]);
        let outcome = watch(&path_step(V3_SERIES), vec![answer]).await;
        assert_status(&outcome, StepStatus::Passed);
    }

    #[tokio::test(start_paused = true)]
    async fn the_path_does_not_matter_without_a_path() {
        let answer = records(vec![
            request_at(Telemetry::Metrics, V3_SERIES, Some(ADP), 2, 0),
            request_at(Telemetry::Metrics, V2_SERIES, Some(ADP), 1, 0),
        ]);
        let outcome = watch(&adp_step(Telemetry::Metrics), vec![answer]).await;
        assert_status(&outcome, StepStatus::Passed);
    }

    #[tokio::test(start_paused = true)]
    async fn each_sender_is_expected_by_its_user_agent() {
        for (sender, user_agent) in [("agent", AGENT), ("trace-agent", TRACE_AGENT)] {
            let step = step(&format!(
                "{{on: intake, telemetry: spans, select: {{service: fx}}, sender: {}, settle: 1s, timeout: 10s}}",
                sender
            ));
            let answer = records(vec![request(Telemetry::Spans, Some(user_agent), 1, 0)]);
            let outcome = watch(&step, vec![answer]).await;
            assert_status(&outcome, StepStatus::Passed);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn items_that_arrive_in_several_requests_and_polls_add_up() {
        let answers = vec![
            records(vec![request(Telemetry::Metrics, Some(ADP), 2, 0)]),
            records(vec![
                request(Telemetry::Metrics, Some(ADP), 2, 0),
                request(Telemetry::Metrics, Some(ADP), 1, 0),
            ]),
        ];
        let outcome = watch(&metrics_step(Some(3)), answers).await;
        assert_status(&outcome, StepStatus::Passed);
        // The first read does not meet the count, so the settle starts at the second.
        assert_eq!(outcome.elapsed, Duration::from_millis(2500));
    }

    #[tokio::test(start_paused = true)]
    async fn without_a_count_one_item_is_enough_and_more_are_fine() {
        let answer = records(vec![request(Telemetry::Metrics, Some(ADP), 5, 0)]);
        let outcome = watch(&metrics_step(None), vec![answer]).await;
        assert_status(&outcome, StepStatus::Passed);
    }

    #[tokio::test(start_paused = true)]
    async fn items_from_another_sender_fail_at_once_and_the_failure_is_kept() {
        let answer = records(vec![request(Telemetry::Metrics, Some(AGENT), 3, 0)]);
        let outcome = watch(&adp_step(Telemetry::Metrics), vec![answer]).await;
        assert_status(&outcome, StepStatus::Failed);
        assert_mentions(
            &outcome,
            &[
                "from a sender other than adp",
                "Items by sender: agent 3.",
                "/api/v2/series",
                "user-agent 'datadog-agent/7.87.0'",
                "all kept as intake_from-metrics-adp-fx_.txt of 'intake'",
            ],
        );
        assert!(
            !outcome.report.message.contains("after the result first held"),
            "{}",
            outcome.report.message
        );
        assert_eq!(outcome.elapsed, Duration::ZERO);
        assert_eq!(outcome.reads, 1);
        let [(name, contents)] = &outcome.kept[..] else {
            panic!("{:?}", outcome.kept)
        };
        assert_eq!(name, "intake_from-metrics-adp-fx_.txt");
        assert!(
            contents.contains("content-type: application/x-protobuf"),
            "{}",
            contents
        );
        // The `User-Agent` has its own field, so the list of headers leaves it out.
        assert_eq!(contents.matches(AGENT).count(), 1, "{}", contents);
    }

    #[tokio::test(start_paused = true)]
    async fn delivery_from_both_senders_fails_although_the_expected_sender_delivered() {
        let answer = records(vec![
            request(Telemetry::Metrics, Some(ADP), 3, 0),
            request(Telemetry::Metrics, Some(AGENT), 1, 0),
        ]);
        let outcome = watch(&adp_step(Telemetry::Metrics), vec![answer]).await;
        assert_status(&outcome, StepStatus::Failed);
        assert_mentions(&outcome, &["Items by sender: adp 3, agent 1."]);
    }

    #[tokio::test(start_paused = true)]
    async fn an_unattributable_request_fails_and_names_its_user_agent_and_path() {
        for (user_agent, shown) in [(Some("curl/8.0"), "user-agent 'curl/8.0'"), (None, "user-agent none")] {
            let answer = records(vec![
                request(Telemetry::Metrics, Some(ADP), 3, 0),
                request(Telemetry::Metrics, user_agent, 1, 0),
            ]);
            let outcome = watch(&adp_step(Telemetry::Metrics), vec![answer]).await;
            assert_status(&outcome, StepStatus::Failed);
            assert_mentions(
                &outcome,
                &["adp 3, unattributable 1", shown, "unattributable /api/v2/series"],
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn more_items_than_the_count_fail() {
        let answer = records(vec![request(Telemetry::Metrics, Some(ADP), 4, 0)]);
        let outcome = watch(&adp_step(Telemetry::Metrics), vec![answer]).await;
        assert_status(&outcome, StepStatus::Failed);
        assert_mentions(
            &outcome,
            &["4 selected item(s) reached 'intake' from adp", "expects exactly 3"],
        );
        assert_eq!(outcome.elapsed, Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn a_duplicate_that_arrives_during_the_settle_fails() {
        let held = || records(vec![request(Telemetry::Metrics, Some(ADP), 3, 0)]);
        let duplicated = records(vec![
            request(Telemetry::Metrics, Some(ADP), 3, 0),
            request(Telemetry::Metrics, Some(ADP), 1, 0),
        ]);
        let outcome = watch(&adp_step(Telemetry::Metrics), vec![held(), held(), held(), duplicated]).await;
        assert_status(&outcome, StepStatus::Failed);
        assert_mentions(
            &outcome,
            &[
                "4 selected item(s)",
                "after the result first held, within the settle of 2s",
            ],
        );
        assert_eq!(outcome.reads, 4);
        assert!(outcome.elapsed < Duration::from_secs(2), "{:?}", outcome.elapsed);
    }

    #[tokio::test(start_paused = true)]
    async fn a_second_sender_that_delivers_during_the_settle_fails() {
        let held = || records(vec![request(Telemetry::Spans, Some(ADP), 3, 0)]);
        let both = records(vec![
            request(Telemetry::Spans, Some(ADP), 3, 0),
            request(Telemetry::Spans, Some(TRACE_AGENT), 1, 0),
        ]);
        let outcome = watch(&adp_step(Telemetry::Spans), vec![held(), held(), both]).await;
        assert_status(&outcome, StepStatus::Failed);
        assert_mentions(
            &outcome,
            &["Items by sender: adp 3, trace-agent 1.", "after the result first held"],
        );
    }

    /// Watches an intake for the step on metrics, and returns the watch with the start and limit of each read.
    ///
    /// The intake holds the three items that the step expects. If `late` is set, one more item arrives then. The read
    /// that starts 1.5 seconds into the watch, just before the 2-second settle ends, takes 0.6 seconds. The answer shows
    /// the items that the intake held when the read started. If `honors_limit` is set, a read stops at its limit with an
    /// error. Otherwise, it overruns its limit.
    async fn watch_slow_read(
        honors_limit: bool, late: Option<Duration>,
    ) -> (Result<Watched, HandleError>, Vec<(Duration, Duration)>) {
        let step = adp_step(Telemetry::Metrics);
        let begun = Instant::now();
        let mut reads = Vec::new();
        let watched = step
            .watch(begun + step.budget(), |limit| {
                let offset = begun.elapsed();
                reads.push((offset, limit));
                let mut items = vec![request(Telemetry::Metrics, Some(ADP), 3, 0)];
                if late.is_some_and(|late| offset >= late) {
                    items.push(request(Telemetry::Metrics, Some(ADP), 1, 0));
                }
                let latency = if offset == Duration::from_millis(1500) {
                    Duration::from_millis(600)
                } else {
                    Duration::ZERO
                };
                async move {
                    if honors_limit && latency > limit {
                        sleep_until(Instant::now() + limit).await;
                        return Err(HandleError::Software("the read timed out".to_string()));
                    }
                    sleep_until(Instant::now() + latency).await;
                    records(items)
                }
            })
            .await;
        (watched, reads)
    }

    #[tokio::test(start_paused = true)]
    async fn a_duplicate_that_a_slow_read_of_the_settle_misses_still_fails() {
        let late = Some(Duration::from_millis(1800));
        for honors_limit in [true, false] {
            let (watched, reads) = watch_slow_read(honors_limit, late).await;
            let watched = watched.unwrap();
            assert_eq!(watched.check, Check::Duplicate, "honors_limit: {}", honors_limit);
            assert!(watched.settling);
            // The last read starts after the settle ends, so it sees the duplicate.
            let (offset, _) = *reads.last().unwrap();
            assert!(offset >= Duration::from_secs(2), "{:?}", reads);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_settle_ends_with_a_read_that_starts_after_it_ends() {
        for honors_limit in [true, false] {
            let (watched, reads) = watch_slow_read(honors_limit, None).await;
            assert_eq!(watched.unwrap().check, Check::Met, "honors_limit: {}", honors_limit);
            let (offset, limit) = *reads.last().unwrap();
            assert!(offset >= Duration::from_secs(2), "{:?}", reads);
            // The last read gets a whole probe. One more read is enough, so the watch does not loop.
            assert_eq!(limit, PROBE_TIMEOUT);
            assert!(reads.len() <= 6, "{:?}", reads);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_read_that_starts_in_the_settle_stops_when_it_ends() {
        let (_, reads) = watch_slow_read(true, None).await;
        // The reads start every 0.5s from the first success. The read at 1.5s gets the 0.5s that are left.
        let limits: Vec<_> = reads
            .iter()
            .map(|(offset, limit)| (offset.as_millis(), limit.as_millis()))
            .collect();
        assert_eq!(
            limits,
            [(0, 10_000), (500, 1500), (1000, 1000), (1500, 500), (2000, 10_000)]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn fewer_items_than_the_count_fail_at_the_timeout() {
        let answer = records(vec![request(Telemetry::Metrics, Some(ADP), 2, 0)]);
        let outcome = watch(&adp_step(Telemetry::Metrics), vec![answer]).await;
        assert_status(&outcome, StepStatus::Failed);
        assert_mentions(
            &outcome,
            &[
                "2 selected item(s) reached 'intake' from adp within 10s",
                "expects exactly 3",
            ],
        );
        assert_eq!(outcome.elapsed, Duration::from_secs(10));
    }

    #[tokio::test(start_paused = true)]
    async fn an_intake_with_nothing_selected_fails_at_the_timeout() {
        let outcome = watch(
            &metrics_step(None),
            vec![records(vec![request(Telemetry::Metrics, Some(ADP), 0, 7)])],
        )
        .await;
        assert_status(&outcome, StepStatus::Failed);
        assert_mentions(
            &outcome,
            &[
                "0 selected item(s) reached 'intake' from adp within 10s",
                "expects at least 1",
                "Items by sender: none.",
            ],
        );
        assert!(outcome.kept.is_empty(), "{:?}", outcome.kept);
    }

    #[tokio::test(start_paused = true)]
    async fn a_failure_quotes_a_few_requests_and_keeps_them_all() {
        let requests = (0..12)
            .map(|_| request(Telemetry::Metrics, Some(AGENT), 1, 0))
            .collect();
        let outcome = watch(&adp_step(Telemetry::Metrics), vec![records(requests)]).await;
        assert_mentions(&outcome, &["12 request(s) carried selected items, 10 quoted"]);
        assert_eq!(
            outcome.report.message.matches("\n  agent /api/v2/series").count(),
            MAX_QUOTED
        );
        assert_eq!(outcome.kept[0].1.lines().count(), 12);
    }

    #[tokio::test(start_paused = true)]
    async fn a_failure_redacts_the_secret_headers_in_the_message_and_the_kept_file() {
        let mut leaked = request(Telemetry::Metrics, Some(AGENT), 1, 0);
        leaked["headers"]["dd-api-key"] = json!("api-key-0123456789abcdef");
        leaked["headers"]["DD-Application-Key"] = json!("application-key-0123456789abcdef");
        leaked["headers"]["authorization"] = json!("Bearer token-0123456789abcdef");
        let outcome = watch(&adp_step(Telemetry::Metrics), vec![records(vec![leaked])]).await;

        assert_status(&outcome, StepStatus::Failed);
        assert_mentions(
            &outcome,
            &[
                "authorization: <redacted>",
                "dd-api-key: <redacted>",
                "DD-Application-Key: <redacted>",
                "content-type: application/x-protobuf",
                &format!("user-agent '{}'", AGENT),
            ],
        );
        let (_, kept) = &outcome.kept[0];
        for shown in [&outcome.report.message, kept] {
            for secret in [
                "api-key-0123456789abcdef",
                "application-key-0123456789abcdef",
                "token-0123456789abcdef",
            ] {
                assert!(!shown.contains(secret), "'{}' is in: {}", secret, shown);
            }
        }
        assert!(kept.contains("dd-api-key: <redacted>"), "{}", kept);
    }

    #[tokio::test(start_paused = true)]
    async fn a_read_that_fails_is_tried_again_but_the_last_read_must_work() {
        let held = records(vec![request(Telemetry::Metrics, Some(ADP), 3, 0)]);
        let refused = |why: &str| Err(HandleError::Software(why.to_string()));

        let outcome = watch(&adp_step(Telemetry::Metrics), vec![refused("refused"), held.clone()]).await;
        assert_status(&outcome, StepStatus::Passed);

        let answers = vec![held.clone(), refused("closed")];
        let outcome = watch(&adp_step(Telemetry::Metrics), answers).await;
        assert_status(&outcome, StepStatus::Failed);
        assert_mentions(
            &outcome,
            &["the last read of /metrics/dump_requests from 'intake' failed: closed"],
        );

        let docker = Err(HandleError::Harness("Docker refused the exec".to_string()));
        let outcome = watch(&adp_step(Telemetry::Metrics), vec![held, docker]).await;
        assert_status(&outcome, StepStatus::Errored);
        assert_mentions(&outcome, &["Docker refused the exec"]);
    }

    #[tokio::test(start_paused = true)]
    async fn an_answer_that_does_not_decode_errors_at_once() {
        let answers = [Ok(b"{}".to_vec()), Ok(Vec::new())];
        for (answer, why) in answers.into_iter().zip(["invalid type", "answered with an empty body"]) {
            let outcome = watch(&adp_step(Telemetry::Metrics), vec![answer]).await;
            assert_status(&outcome, StepStatus::Errored);
            assert_mentions(
                &outcome,
                &["the records that 'intake' answers at /metrics/dump_requests", why],
            );
            assert_eq!(outcome.reads, 1);
        }
    }

    #[test]
    fn a_step_that_cannot_be_honored_is_rejected_when_it_is_built() {
        let select = "select: {service: fx}, sender: adp, timeout: 1m";
        let cases = [
            (
                format!("on: i, telemetry: events, {}", select),
                "unknown variant `events`",
            ),
            (
                format!("on: i, telemetry: trace_stats, count: 2, {}", select),
                "'count' is not an option for trace_stats",
            ),
            (format!("on: i, telemetry: logs, count: 0, {}", select), "nonzero"),
            (
                "on: i, telemetry: logs, select: {service: fx}, sender: collector, timeout: 1m".to_string(),
                "unknown variant `collector`",
            ),
            (
                format!("on: i, telemetry: logs, extra: 1, {}", select),
                "unknown field `extra`",
            ),
            (
                format!("on: i, telemetry: metrics, {}", select),
                "metrics are selected by 'name_prefix', not 'service'",
            ),
            (
                "on: i, telemetry: spans, select: {name_prefix: fx}, sender: adp, timeout: 1m".to_string(),
                "spans are selected by 'service', not 'name_prefix'",
            ),
            (
                "on: i, telemetry: logs, select: {service: ''}, sender: adp, timeout: 1m".to_string(),
                "'select' is empty",
            ),
            (
                "on: i, telemetry: logs, select: {service: fx, name_prefix: fx}, sender: adp, timeout: 1m".to_string(),
                "expected map with a single key",
            ),
            (
                "on: i, telemetry: logs, select: {service: fx}, sender: adp".to_string(),
                "missing field `timeout`",
            ),
            (
                format!("on: i, telemetry: logs, path: api/v2/series, {}", select),
                "'path' must start with '/', and 'api/v2/series' does not",
            ),
        ];
        for (fields, error) in cases {
            let fields = StepFields(serde_saphyr::from_str(&format!("{{{}}}", fields)).unwrap());
            let rejected = IntakeFromFactory.build(&fields).unwrap_err();
            assert!(rejected.contains(error), "'{}' is not in: {}", error, rejected);
        }
    }

    #[test]
    fn the_budget_covers_the_timeout_the_settle_and_two_reads() {
        let default_settle = step("{on: i, telemetry: logs, select: {service: fx}, sender: adp, timeout: 2m}");
        assert_eq!(
            default_settle.budget(),
            Duration::from_secs(120 + 20) + PROBE_TIMEOUT * 2
        );
        assert_eq!(
            adp_step(Telemetry::Logs).budget(),
            Duration::from_secs(12) + PROBE_TIMEOUT * 2
        );
    }

    /// A participant whose `api` endpoint is published at a port of this host.
    #[derive(Debug)]
    struct Published(u16);

    #[async_trait]
    impl Realized for Published {
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

        fn host_port(&self, endpoint: &str) -> Option<u16> {
            (endpoint == API).then_some(self.0)
        }
    }

    /// Serves `routes`, as a status and a body for each path, on a port of this host, and returns the port.
    async fn serve(routes: BTreeMap<&'static str, (u16, String)>) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut head = Vec::new();
                while !head.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    if stream.read(&mut byte).await.unwrap() == 0 {
                        break;
                    }
                    head.push(byte[0]);
                }
                let head = String::from_utf8_lossy(&head);
                let path = head.split_whitespace().nth(1).unwrap_or_default();
                let (status, body) = routes.get(path).cloned().unwrap_or((404, String::new()));
                let response = format!(
                    "HTTP/1.1 {} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    status,
                    body.len(),
                    body
                );
                stream.write_all(response.as_bytes()).await.unwrap();
                stream.shutdown().await.unwrap();
            }
        });
        port
    }

    /// Plans the case `config`, as `panoramic plan` does, for Linux.
    fn plan_case(config: &str) -> Result<crate::core::plan::CasePlan, Vec<String>> {
        let suite = tempfile::tempdir().unwrap();
        let dir = suite.path().join("case");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join(CASE_FILE), config).unwrap();
        let mut plans = crate::load_and_plan(&dir, &BTreeMap::new())
            .map_err(|errors| errors.iter().map(ToString::to_string).collect::<Vec<_>>())?;
        Ok(plans.remove(&Os::Linux).unwrap())
    }

    const CASE: &str = r#"
timeout: 5m
subject_os: [linux]
participants:
  intake:
    type: datadog-intake
  sender:
    subject: true
    image: alpine:3.20
procedure:
  - {step: intake_from, on: intake, telemetry: metrics, select: {name_prefix: "fx."}, sender: adp, count: 3, settle: 1ms, timeout: 1ms}
  - {step: intake_from, on: intake, telemetry: spans, select: {service: fx}, sender: adp, settle: 1ms, timeout: 1ms}
  - {step: intake_from, on: intake, telemetry: logs, select: {service: fx}, sender: adp, settle: 1ms, timeout: 1ms}
"#;

    #[tokio::test]
    async fn the_planned_step_reads_an_intake_through_its_published_port() {
        let plan = plan_case(CASE).unwrap_or_else(|errors| panic!("{}", errors.join("\n")));
        let steps: Vec<_> = plan.procedure.iter().flat_map(|node| node.steps()).collect();
        for step in &steps {
            assert_eq!(
                step.needs,
                [Need::Endpoint(
                    "intake".to_string(),
                    API.to_string(),
                    EndpointOperation::Request
                )]
            );
        }

        // The client of the handle needs the process-wide crypto provider, which `main` installs at startup.
        let _ = crate::default_crypto_provider().install_default();
        let port = serve(BTreeMap::from([
            (
                "/metrics/dump_requests",
                (200, json!([request(Telemetry::Metrics, Some(ADP), 3, 1)]).to_string()),
            ),
            ("/traces/dump_span_requests", (500, "broken".to_string())),
            (
                "/logs/dump",
                (200, json!([request(Telemetry::Logs, Some(AGENT), 2, 0)]).to_string()),
            ),
        ]))
        .await;
        let realized = Published(port);
        let observation = Observation::new([]);
        let values = Values::of_plan(&plan.participants);
        let handles = BTreeMap::from([(
            "intake".to_string(),
            Handle::new("intake", &plan.participants["intake"], &realized, &values, &observation),
        )]);
        let suite = tempfile::tempdir().unwrap();
        let kept = Kept::new(&suite.path().join("logs"));
        let mut reports = Vec::new();
        for step in &steps {
            let cx = StepContext::new(&handles, &values, &kept, Instant::now() + step.step.budget());
            reports.push(step.step.run(&cx).await);
        }

        let statuses: Vec<StepStatus> = reports.iter().map(|report| report.status).collect();
        let messages: Vec<&str> = reports.iter().map(|report| report.message.as_str()).collect();
        assert_eq!(
            statuses,
            [StepStatus::Passed, StepStatus::Failed, StepStatus::Failed],
            "{:#?}",
            messages
        );
        assert!(
            messages[1].contains("the intake answered /traces/dump_span_requests with status 500"),
            "{}",
            messages[1]
        );
        assert!(messages[2].contains("Items by sender: agent 2."), "{}", messages[2]);
    }

    #[test]
    fn a_step_on_a_participant_without_an_api_endpoint_does_not_plan() {
        let config = CASE.replace("on: intake", "on: sender");
        let errors = plan_case(&config).unwrap_err();
        assert!(
            errors
                .iter()
                .any(|error| error.contains("participant 'sender' has no endpoint 'api'")),
            "{:#?}",
            errors
        );
    }
}
