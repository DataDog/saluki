//! `intake_from`: selected telemetry reached an intake from the expected sender, and from no other, the expected number
//! of times. Or, with `count: 0`, no selected telemetry reached it from anyone.
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
//! An intake that several keys deliver to can tell the deliveries apart by the `DD-API-KEY` header. With `api_keys`,
//! the step lists every key that may deliver selected items. A request with any other key, or with no key, fails the
//! step, just like a request of another sender. Give a map from each key to its exact number of items. Or give a list of
//! keys, which expects at least one item of each. Use a list for `trace_stats`, which takes no count. A step without
//! `api_keys` accepts any key.
//!
//! The reports, the kept files and the `Debug` form of the step never show the value of a key. A report names a key by
//! its place in the sorted keys of the step, such as `key 2`. The case file, the output of `panoramic plan` and the
//! `plan.json` that a failed case keeps do show the keys, so use only synthetic keys:
//!
//! ```yaml
//! - step: intake_from
//!   on: intake-extra
//!   telemetry: spans
//!   select: {service: otlp-routing-endpoints-split-http}
//!   sender: adp
//!   api_keys: {"11111111111111111111111111111111": 3, "22222222222222222222222222222222": 3}
//!   timeout: 2m
//! ```
//!
//! With `count: 0`, the step checks that nothing arrives. It passes when no selected item reached the intake from any
//! sender for the whole `settle`, and fails when one does. It takes no `sender` or `path`, because no item may arrive
//! at all. It needs an explicit `settle`. The step cannot see a request that is still in flight, so `settle` is the
//! time that a wrong delivery has to arrive.
//!
//! An absence that follows no delivery proves nothing, because the payload may have gone nowhere. The plan therefore
//! rejects the step unless an earlier procedure entry has a step that expects the same items from a sender. That step
//! can be at any intake. Both steps need the same `telemetry` and the same `select`. The earlier step must not use
//! `count: 0`. A step in the same `parallel` entry does not count, because those steps run together. So a typo in
//! `select`, or an absence in the first entry, fails the plan:
//!
//! ```yaml
//! - step: intake_from
//!   on: intake-main
//!   telemetry: spans
//!   select: {service: otlp-routing-endpoints-split-http}
//!   count: 0
//!   settle: 30s
//!   timeout: 1m
//! ```
//!
//! The options of the step:
//!
//! - `on`: the intake participant.
//! - `telemetry`: `metrics`, `spans`, `trace_stats` or `logs`.
//! - `select`: `{name_prefix: <prefix>}` for `metrics`, and `{service: <name>}` for the others.
//! - `sender`: `adp`, `agent` or `trace-agent`. Required, except with `count: 0`, where it is not allowed.
//! - `path`: the path of the requests that carry the selected items, such as `/api/intake/metrics/v3/series`. It starts
//!   with `/`. Without it, the path does not matter. Not allowed with `count: 0`.
//! - `api_keys`: every value of the `DD-API-KEY` header that may deliver selected items. A list expects at least one
//!   item of each key. A map gives the exact number of items of each key, and is not allowed for `trace_stats`. A
//!   request with another key, or with no key, fails the step. Without `api_keys`, the key does not matter. Not allowed
//!   with `count`, which counts the items of every key together. Not allowed with `count: 0`, which allows no key.
//! - `count`: the exact number of items. Not allowed for `trace_stats`, whose buckets vary, so the step only checks
//!   that statistics arrived from the sender. `0` is allowed for every telemetry, and means that no item arrives.
//! - `settle`: how long to keep watching after the result first holds. Defaults to `20s`, and is required with
//!   `count: 0`.
//! - `timeout`: how long to wait for the result to first hold. With `count: 0` the result holds at the first read, so
//!   `timeout` only bounds a read that fails.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Write as _};
use std::future::Future;
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

/// The header that carries the API key of a request, in lower case.
const DD_API_KEY: &str = "dd-api-key";

/// What a failure message calls a request that no sender claims.
const UNATTRIBUTABLE: &str = "unattributable";

/// Most requests a failure message quotes. The kept file holds all of them.
const MAX_QUOTED: usize = 10;

/// Headers that carry a secret, in lower case. A failure shows [`REDACTED`] as their value.
const SECRET_HEADERS: [&str; 3] = [DD_API_KEY, "dd-application-key", "authorization"];

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
    #[serde(default)]
    sender: Option<Sender>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    api_keys: Option<ApiKeys>,
    #[serde(default)]
    count: Option<u64>,
    #[serde(default)]
    settle: Option<Timeout>,
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

/// The `api_keys` field as the case writes it: a list of keys, or a map from each key to its number of items.
#[derive(Deserialize)]
#[serde(
    untagged,
    expecting = "a list of API keys, or a map from each API key to its number of items"
)]
enum ApiKeys {
    Listed(Vec<String>),
    Counted(BTreeMap<String, u64>),
}

/// An API key that selected requests carry. Nothing that the step reports or keeps shows its value, and its `Debug`
/// form does not either. The fields of the step in the plan do show it, because they hold what the case wrote.
struct ApiKey(String);

impl ApiKey {
    /// Whether `headers` hold this key as the value of the `DD-API-KEY` header, whatever the case of the name.
    fn is_in(&self, headers: &BTreeMap<String, String>) -> bool {
        headers
            .iter()
            .any(|(name, value)| name.eq_ignore_ascii_case(DD_API_KEY) && *value == self.0)
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ApiKey({})", REDACTED)
    }
}

/// What the step expects of the selected items that carry one API key. A step without `api_keys` has one bound, for any
/// key.
#[derive(Debug)]
struct KeyBound {
    /// The key that the requests carry, or `None` for any key.
    key: Option<ApiKey>,
    /// The exact number of items from the expected sender, or `None` for at least one.
    count: Option<u64>,
}

impl KeyBound {
    /// How the step describes the expectation, such as `exactly 3`.
    fn expected(&self) -> String {
        match self.count {
            Some(count) => format!("exactly {}", count),
            None => "at least 1".to_string(),
        }
    }
}

/// Which bound of the step the API key of a request falls under.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum KeyMatch {
    /// The step names no key, so every request falls under its one bound.
    Any,
    /// The key is the one of the bound at this index.
    Listed(usize),
    /// The request carries a key that no bound names.
    Unlisted,
    /// The request carries no key.
    Missing,
}

impl KeyMatch {
    /// Finds the bound that the key in `headers` falls under.
    fn of(bounds: &[KeyBound], headers: &BTreeMap<String, String>) -> Self {
        if let [KeyBound { key: None, .. }] = bounds {
            return Self::Any;
        }
        let position = bounds
            .iter()
            .position(|bound| bound.key.as_ref().is_some_and(|key| key.is_in(headers)));
        match position {
            Some(index) => Self::Listed(index),
            None if headers.keys().any(|name| name.eq_ignore_ascii_case(DD_API_KEY)) => Self::Unlisted,
            None => Self::Missing,
        }
    }

    /// The index of the bound that the request falls under, or `None` if no bound names its key.
    fn bound(self) -> Option<usize> {
        match self {
            Self::Any => Some(0),
            Self::Listed(index) => Some(index),
            Self::Unlisted | Self::Missing => None,
        }
    }

    /// What a report calls the key. It never shows the value: a listed key is its place among the keys of the step.
    fn label(self) -> String {
        match self {
            Self::Any => "any".to_string(),
            Self::Listed(index) => format!("key {}", index + 1),
            Self::Unlisted => "unlisted".to_string(),
            Self::Missing => "none".to_string(),
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

    /// The claim that the selected items reach an intake. Steps compare claims by their text, so two steps share a claim
    /// if they have the same telemetry and the same selector.
    fn reach_claim(&self) -> String {
        format!("{} reach an intake", self.describe())
    }

    /// A file name that is safe on every host. Two steps share it if they have the same telemetry, expected sender and
    /// selector, or selectors that differ only in replaced characters or after the first 60. The API key of a step does
    /// not change the name, because the name must not show it.
    fn file_name(&self, expected: &str) -> String {
        let selector: String = self
            .selector
            .chars()
            .take(60)
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' })
            .collect();
        format!("intake_from-{}-{}-{}.txt", self.telemetry.name(), expected, selector)
    }

    /// Decodes the answer of the intake at the route of the telemetry, and keeps the requests that carried selected
    /// items, each with the bound of `bounds` that its API key falls under.
    ///
    /// # Errors
    ///
    /// Returns an error if the answer does not decode as the records of the telemetry.
    fn selected(&self, body: &[u8], bounds: &[KeyBound]) -> Result<Tally, String> {
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
                Tally::of(
                    bounds,
                    requests.iter().map(|request| {
                        let points = request
                            .items()
                            .iter()
                            .filter(|metric| metric.context().name().starts_with(selector))
                            .map(|metric| metric.values().len() as u64)
                            .sum::<u64>();
                        (request.path(), request.headers(), points)
                    }),
                )
            }
            Telemetry::Spans => {
                let requests: Vec<IntakeRequest<Vec<Span>>> = decode(body)?;
                Tally::of(
                    bounds,
                    requests.iter().map(|request| {
                        let spans = request.items().iter().filter(|span| span.service() == selector).count();
                        (request.path(), request.headers(), spans as u64)
                    }),
                )
            }
            Telemetry::TraceStats => {
                let requests: Vec<IntakeRequest<ClientStatisticsAggregator>> = decode(body)?;
                Tally::of(
                    bounds,
                    requests.iter().map(|request| {
                        let groups = request
                            .items()
                            .groups()
                            .values()
                            .flat_map(|group| group.buckets())
                            .filter(|(_, statistics)| statistics.service() == selector)
                            .count();
                        (request.path(), request.headers(), groups as u64)
                    }),
                )
            }
            Telemetry::Logs => {
                let requests: Vec<LogsRequest> = decode(body)?;
                Tally::of(
                    bounds,
                    requests.iter().map(|request| {
                        let logs = request
                            .logs()
                            .iter()
                            .filter(|log| log.service().and_then(|value| value.as_str()) == Some(selector))
                            .count();
                        (request.path(), request.headers(), logs as u64)
                    }),
                )
            }
        })
    }
}

/// A request that carried selected items.
#[derive(Debug)]
struct Selected {
    path: String,
    /// Which bound of the step the key of the request falls under.
    key: KeyMatch,
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

    /// One line for a failure message: the sender, the path, the `User-Agent`, the key if the step names keys, and every
    /// other header.
    fn line(&self) -> String {
        let headers: Vec<String> = self
            .headers
            .iter()
            .filter(|(name, _)| name.as_str() != USER_AGENT)
            .map(|(name, value)| format!("{}: {}", name, value))
            .collect();
        let key = match self.key {
            KeyMatch::Any => String::new(),
            key => format!("; API key: {}", key.label()),
        };
        format!(
            "{} {}: {} item(s), user-agent {}{}; headers: {}",
            self.sender_name(),
            self.path,
            self.items,
            self.user_agent()
                .map_or("none".to_string(), |agent| format!("'{}'", agent)),
            key,
            headers.join(", ")
        )
    }
}

/// The requests that carried selected items, in the order the intake received them.
#[derive(Debug)]
struct Tally(Vec<Selected>);

impl Tally {
    /// Keeps the requests that carried items, from each request's path, headers, and number of selected items. It finds
    /// the bound of `bounds` that the API key of each request falls under, and then replaces the value of each secret
    /// header, so nothing that the step reports can show it.
    fn of<'a>(
        bounds: &[KeyBound], requests: impl IntoIterator<Item = (&'a str, &'a BTreeMap<String, String>, u64)>,
    ) -> Self {
        Self(
            requests
                .into_iter()
                .filter(|(_, _, items)| *items > 0)
                .map(|(path, headers, items)| Selected {
                    path: path.to_string(),
                    key: KeyMatch::of(bounds, headers),
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

    /// The items that requests of `sender` carried under the bound at `index`.
    fn items_from_under(&self, sender: Sender, index: usize) -> u64 {
        self.0
            .iter()
            .filter(|request| request.sender() == Some(sender) && request.key.bound() == Some(index))
            .map(|request| request.items)
            .sum()
    }

    /// Whether a request carried a key that no bound names, or no key.
    fn has_stray_key(&self) -> bool {
        self.0.iter().any(|request| request.key.bound().is_none())
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

    /// The items of each key, such as `key 1 3, unlisted 3`.
    fn key_counts(&self) -> String {
        let mut counts: BTreeMap<KeyMatch, u64> = BTreeMap::new();
        for request in &self.0 {
            *counts.entry(request.key).or_default() += request.items;
        }
        if counts.is_empty() {
            return "none".to_string();
        }
        counts
            .iter()
            .map(|(key, items)| format!("{} {}", key.label(), items))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// How the items that arrived compare with what the step expects.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Check {
    /// Fewer items than expected arrived from the sender under the bound at this index.
    Short(usize),
    /// The expected sender delivered the expected number of items, and nobody else delivered any.
    Met,
    /// Another sender, or an unattributable request, delivered selected items. For a step that expects no item, every
    /// sender is another sender.
    Foreign,
    /// A request carried an API key that the step does not list, or no key.
    Stray,
    /// The expected sender delivered selected items on a path other than `path`.
    Misrouted,
    /// The expected sender delivered more items than the bound at this index allows.
    Duplicate(usize),
}

/// What the step expects of the selected items.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Expect {
    /// Items from `sender` and from no other sender. Each bound of the step says how many.
    From { sender: Sender },
    /// No item, from any sender.
    Nothing,
}

impl Expect {
    /// The expected sender, or `none` if the step expects no item.
    fn sender_name(self) -> &'static str {
        match self {
            Self::From { sender, .. } => sender.name(),
            Self::Nothing => "none",
        }
    }
}

/// What a watch of the intake found.
#[derive(Debug)]
struct Watched {
    check: Check,
    tally: Tally,
    /// Whether the check was `Met` earlier, so the watch was in its settle.
    settling: bool,
}

/// Passes once selected items arrived from the expected sender, and nothing else arrived for the settle. Or, if it
/// expects nothing, passes when no selected item arrived for the settle.
#[derive(Debug)]
struct IntakeFrom {
    on: String,
    target: Target,
    expect: Expect,
    /// The path of the requests that carry the selected items, or `None` for any path.
    path: Option<String>,
    /// The keys that the requests may carry, each with its count. One bound for any key if the step names none.
    bounds: Vec<KeyBound>,
    settle: Timeout,
    timeout: Timeout,
}

impl IntakeFrom {
    /// Checks that the fields of the step agree.
    ///
    /// # Errors
    ///
    /// Returns an error if the selection does not suit the telemetry, `count` is not `0` for `trace_stats`, `path` does
    /// not start with `/`, `api_keys` is empty, holds an empty or repeated key, gives a count of `0` or a count for
    /// `trace_stats`, or comes with `count`, `sender` is missing or given with `count: 0`, or `path` is given or
    /// `settle` is missing with `count: 0`.
    fn new(fields: Fields) -> Result<Self, String> {
        let Fields {
            on,
            telemetry,
            select,
            sender,
            path,
            api_keys,
            count,
            settle,
            timeout,
        } = fields;
        if telemetry == Telemetry::TraceStats && count.is_some_and(|count| count != 0) {
            return Err(
                "'count' is not an option for trace_stats, except 0: the buckets of statistics vary, so the step \
                 checks only that statistics arrive from the sender"
                    .to_string(),
            );
        }
        if let Some(path) = path.as_deref().filter(|path| !path.starts_with('/')) {
            return Err(format!("'path' must start with '/', and '{}' does not", path));
        }
        let bounds = Self::bounds(api_keys, telemetry, count)?;
        let expect = match (sender, count) {
            (None, Some(0)) => Expect::Nothing,
            (Some(_), Some(0)) => {
                return Err(
                    "'sender' is not an option with 'count: 0': no sender may deliver a selected item".to_string(),
                )
            }
            (None, _) => return Err("'sender' is required, except with 'count: 0'".to_string()),
            (Some(sender), _) => Expect::From { sender },
        };
        if expect == Expect::Nothing && path.is_some() {
            return Err("'path' is not an option with 'count: 0': no selected item may arrive on any path".to_string());
        }
        let settle = match (expect, settle) {
            (_, Some(settle)) => settle,
            (Expect::From { .. }, None) => default_settle(),
            (Expect::Nothing, None) => {
                return Err(
                    "'settle' is required with 'count: 0': it is how long the step watches for a selected item"
                        .to_string(),
                )
            }
        };
        Ok(Self {
            on,
            target: Target::new(telemetry, select)?,
            expect,
            path,
            bounds,
            settle,
            timeout,
        })
    }

    /// The bounds of the step: one for each key of `api_keys`, in sorted order, or one for any key without `api_keys`.
    ///
    /// # Errors
    ///
    /// Returns an error if `api_keys` does not suit `telemetry` and `count`, or does not name a key, or names one twice
    /// or with a count of `0`.
    fn bounds(api_keys: Option<ApiKeys>, telemetry: Telemetry, count: Option<u64>) -> Result<Vec<KeyBound>, String> {
        let Some(api_keys) = api_keys else {
            return Ok(vec![KeyBound { key: None, count }]);
        };
        match count {
            Some(0) => {
                return Err("'api_keys' is not an option with 'count: 0': no item may arrive with any key".to_string())
            }
            Some(_) => return Err(
                "'api_keys' is not an option with 'count': a map gives the count of each key, and a list expects at \
                     least one item of each key"
                    .to_string(),
            ),
            None => {}
        }
        let keys: BTreeMap<String, Option<u64>> =
            match api_keys {
                ApiKeys::Listed(keys) => {
                    let distinct: BTreeSet<&String> = keys.iter().collect();
                    if distinct.len() != keys.len() {
                        return Err("'api_keys' lists a key twice".to_string());
                    }
                    keys.into_iter().map(|key| (key, None)).collect()
                }
                ApiKeys::Counted(_) if telemetry == Telemetry::TraceStats => return Err(
                    "'api_keys' is a list for trace_stats: the buckets of statistics vary, so the step has no count \
                     for a key"
                        .to_string(),
                ),
                ApiKeys::Counted(keys) => keys.into_iter().map(|(key, count)| (key, Some(count))).collect(),
            };
        if keys.is_empty() {
            return Err("'api_keys' is empty, so it names no key, and every request fails the step".to_string());
        }
        if keys.contains_key("") {
            return Err("'api_keys' holds an empty key, which no request carries".to_string());
        }
        if keys.values().any(|count| *count == Some(0)) {
            return Err(
                "'api_keys' gives a key a count of 0: leave the key out, because a request with a key that the list \
                 does not name fails the step"
                    .to_string(),
            );
        }
        Ok(keys
            .into_iter()
            .map(|(key, count)| KeyBound {
                key: Some(ApiKey(key)),
                count,
            })
            .collect())
    }

    fn check(&self, tally: &Tally) -> Check {
        let Expect::From { sender } = self.expect else {
            return if tally.total() == 0 { Check::Met } else { Check::Foreign };
        };
        if tally.total() > tally.items_from(sender) {
            return Check::Foreign;
        }
        if tally.has_stray_key() {
            return Check::Stray;
        }
        if self.path.as_deref().is_some_and(|path| tally.reached_other_than(path)) {
            return Check::Misrouted;
        }
        let have = |index: usize| tally.items_from_under(sender, index);
        let bounds = || self.bounds.iter().enumerate();
        if let Some((index, _)) = bounds().find(|(index, bound)| bound.count.is_some_and(|count| have(*index) > count))
        {
            return Check::Duplicate(index);
        }
        match bounds().find(|(index, bound)| have(*index) < bound.count.unwrap_or(1)) {
            Some((index, _)) => Check::Short(index),
            None => Check::Met,
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
                    let tally = self.target.selected(&body, &self.bounds).map_err(|why| {
                        HandleError::Harness(format!(
                            "the records that '{}' answers at {} do not decode: {}",
                            self.on,
                            self.target.telemetry.route(),
                            why
                        ))
                    })?;
                    let check = self.check(&tally);
                    match check {
                        Check::Foreign | Check::Stray | Check::Misrouted | Check::Duplicate(_) => {
                            break Ok((check, tally))
                        }
                        Check::Met => {
                            held_since.get_or_insert_with(Instant::now);
                        }
                        Check::Short(_) => {}
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
            return StepReport {
                status: StepStatus::Passed,
                message: self.success(&tally),
            };
        };

        if settling {
            let _ = write!(
                message,
                " It happened after the result first held, within the settle of {}.",
                self.settle.text
            );
        }
        let _ = write!(message, "\nSelection: {}.", self.target.describe());
        if let Expect::From { sender } = self.expect {
            let _ = write!(message, " Expected sender: {}.", sender.name());
        }
        if let Some(path) = &self.path {
            let _ = write!(message, " Expected path: {}.", path);
        }
        if self.is_keyed() {
            let _ = write!(message, " Expected API keys: {}.", self.expected_keys());
        }
        let _ = write!(message, "\nItems by sender: {}.", tally.counts());
        if self.is_keyed() {
            let _ = write!(message, "\nItems by API key: {}.", tally.key_counts());
        }
        let lines: Vec<String> = tally.0.iter().map(Selected::line).collect();
        if !lines.is_empty() {
            let file = self.target.file_name(self.expect.sender_name());
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

    /// Whether the step names the API keys that may deliver.
    fn is_keyed(&self) -> bool {
        self.bounds.iter().any(|bound| bound.key.is_some())
    }

    /// What the step expects of each key, such as `key 1 exactly 3, key 2 at least 1`.
    fn expected_keys(&self) -> String {
        self.bounds
            .iter()
            .enumerate()
            .map(|(index, bound)| format!("{} {}", KeyMatch::Listed(index).label(), bound.expected()))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// What a message says of the key that the items of the bound at `index` carry, or nothing if the step names no key.
    fn with_key(&self, index: usize) -> String {
        if self.is_keyed() {
            format!(" with {}", KeyMatch::Listed(index).label())
        } else {
            String::new()
        }
    }

    /// The message of a step that passed.
    fn success(&self, tally: &Tally) -> String {
        let (on, settle) = (&self.on, &self.settle.text);
        match self.expect {
            Expect::From { sender } => {
                let path = self.path.as_ref().map_or(String::new(), |path| format!(" on {}", path));
                let keys = if self.is_keyed() {
                    " with the listed API keys"
                } else {
                    ""
                };
                format!(
                    "{} selected item(s) reached '{}' from {}{}{} and from no other sender, and the result held for {}.",
                    tally.total(),
                    on,
                    sender.name(),
                    path,
                    keys,
                    settle
                )
            }
            Expect::Nothing => format!("No selected item reached '{}' within the settle of {}.", on, settle),
        }
    }

    /// The first sentence of the message for a check that failed, or `None` if the check held.
    fn failure(&self, check: Check, tally: &Tally) -> Option<String> {
        let on = &self.on;
        let Expect::From { sender } = self.expect else {
            return (check != Check::Met).then(|| {
                format!(
                    "{} selected item(s) reached '{}', and the step expects none.",
                    tally.total(),
                    on
                )
            });
        };
        let (have, sender) = (|index| tally.items_from_under(sender, index), sender.name());
        Some(match check {
            Check::Met => return None,
            Check::Foreign => format!("Selected items reached '{}' from a sender other than {}.", on, sender),
            Check::Stray => format!(
                "Selected items reached '{}' with an API key that the step does not list, or with no key.",
                on
            ),
            Check::Misrouted => format!(
                "Selected items reached '{}' on a path other than '{}'.",
                on,
                self.path.as_deref().unwrap_or_default()
            ),
            Check::Duplicate(index) => format!(
                "{} selected item(s) reached '{}' from {}{}, and the step expects {}.",
                have(index),
                on,
                sender,
                self.with_key(index),
                self.bounds[index].expected()
            ),
            Check::Short(index) => format!(
                "{} selected item(s) reached '{}' from {}{} within {}, and the step expects {}.",
                have(index),
                on,
                sender,
                self.with_key(index),
                self.timeout.text,
                self.bounds[index].expected()
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

    /// That the selected items reach an intake, if the step saw them arrive from a sender.
    fn establishes(&self) -> Option<String> {
        matches!(self.expect, Expect::From { .. }).then(|| self.target.reach_claim())
    }

    /// That the selected items reach an intake, if the step expects them at none. Without it, the absence holds when the
    /// items went nowhere.
    fn presumes(&self) -> Option<String> {
        (self.expect == Expect::Nothing).then(|| self.target.reach_claim())
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

    /// Synthetic keys.
    const KEY_ONE: &str = "11111111111111111111111111111111";
    const KEY_TWO: &str = "22222222222222222222222222222222";

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

    /// `request` with the header `DD-API-KEY` set to `key`.
    fn keyed(mut request: Value, key: &str) -> Value {
        request["headers"]["dd-api-key"] = json!(key);
        request
    }

    fn step(fields: &str) -> IntakeFrom {
        let fields = StepFields::of(serde_saphyr::from_str(fields).unwrap());
        IntakeFrom::new(fields.decode().unwrap()).unwrap()
    }

    /// The step that selects the fixtures of `telemetry` and expects them from ADP, three times unless `telemetry` is
    /// `trace_stats`.
    fn adp_step(telemetry: Telemetry) -> IntakeFrom {
        let count = match telemetry {
            Telemetry::TraceStats => "",
            Telemetry::Metrics | Telemetry::Spans | Telemetry::Logs => ", count: 3",
        };
        step(&format!(
            "{{on: intake, telemetry: {}, select: {}, sender: adp, settle: 2s, timeout: 10s{}}}",
            telemetry.name(),
            selection(telemetry),
            count
        ))
    }

    /// The `select` of the steps below, for the fixtures of `telemetry`.
    fn selection(telemetry: Telemetry) -> &'static str {
        match telemetry {
            Telemetry::Metrics => "{name_prefix: 'fx.'}",
            Telemetry::Spans | Telemetry::TraceStats | Telemetry::Logs => "{service: fx}",
        }
    }

    /// The step that selects the fixtures of `telemetry` and expects none of them from anyone.
    fn absent_step(telemetry: Telemetry) -> IntakeFrom {
        step(&format!(
            "{{on: intake, telemetry: {}, select: {}, count: 0, settle: 2s, timeout: 10s}}",
            telemetry.name(),
            selection(telemetry)
        ))
    }

    /// The step that selects the fixtures of `telemetry` and expects them from ADP, with the keys `api_keys`, a list or
    /// a map in YAML.
    fn keyed_step(telemetry: Telemetry, api_keys: &str) -> IntakeFrom {
        step(&format!(
            "{{on: intake, telemetry: {}, select: {}, sender: adp, api_keys: {}, settle: 2s, timeout: 10s}}",
            telemetry.name(),
            selection(telemetry),
            api_keys
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
            assert_eq!(watched.check, Check::Duplicate(0), "honors_limit: {}", honors_limit);
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

    #[tokio::test(start_paused = true)]
    async fn an_intake_without_a_selected_item_passes_a_step_that_expects_none_after_the_settle() {
        for telemetry in TELEMETRIES {
            // Background items, and requests of any sender that hold nothing selected, are not fixtures.
            let background = records(vec![
                request(telemetry, Some(ADP), 0, 2),
                request(telemetry, Some(AGENT), 0, 4),
                request(telemetry, None, 0, 1),
            ]);
            for answer in [records(Vec::new()), background] {
                let outcome = watch(&absent_step(telemetry), vec![answer]).await;
                assert_status(&outcome, StepStatus::Passed);
                assert_mentions(
                    &outcome,
                    &["No selected item reached 'intake' within the settle of 2s."],
                );
                assert_eq!(outcome.elapsed, Duration::from_secs(2), "{}", telemetry.name());
                assert!(outcome.kept.is_empty(), "{}", telemetry.name());
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_selected_item_fails_a_step_that_expects_none_whoever_sent_it() {
        for user_agent in [Some(ADP), Some(AGENT), Some(TRACE_AGENT), Some("curl/8.0"), None] {
            let answer = records(vec![request(Telemetry::Metrics, user_agent, 1, 3)]);
            let outcome = watch(&absent_step(Telemetry::Metrics), vec![answer]).await;
            assert_status(&outcome, StepStatus::Failed);
            assert_mentions(
                &outcome,
                &[
                    "1 selected item(s) reached 'intake', and the step expects none.",
                    "Selection: metric data points with a name that starts with 'fx.'.",
                    "all kept as intake_from-metrics-none-fx_.txt of 'intake'",
                ],
            );
            assert!(
                !outcome.report.message.contains("Expected sender"),
                "{}",
                outcome.report.message
            );
            assert_eq!(outcome.elapsed, Duration::ZERO);
            assert_eq!(outcome.kept.len(), 1);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_selected_item_that_arrives_during_the_settle_fails_a_step_that_expects_none() {
        let late = records(vec![request(Telemetry::Spans, Some(ADP), 1, 0)]);
        let answers = vec![records(Vec::new()), records(Vec::new()), late];
        let outcome = watch(&absent_step(Telemetry::Spans), answers).await;
        assert_status(&outcome, StepStatus::Failed);
        assert_mentions(
            &outcome,
            &[
                "1 selected item(s) reached 'intake', and the step expects none.",
                "after the result first held, within the settle of 2s",
            ],
        );
        assert_eq!(outcome.reads, 3);
        assert!(outcome.elapsed < Duration::from_secs(2), "{:?}", outcome.elapsed);
    }

    #[tokio::test(start_paused = true)]
    async fn a_step_that_expects_none_fails_when_the_last_read_fails() {
        let answers = vec![records(Vec::new()), Err(HandleError::Software("closed".to_string()))];
        let outcome = watch(&absent_step(Telemetry::Spans), answers).await;
        assert_status(&outcome, StepStatus::Failed);
        assert_mentions(
            &outcome,
            &["the last read of /traces/dump_span_requests from 'intake' failed: closed"],
        );
    }

    /// The `api_keys` of a step that lists `KEY_ONE`, which is the first of its keys, and expects `count` items of it.
    fn counted_keys(count: u64) -> String {
        format!("{{'{}': {}}}", KEY_ONE, count)
    }

    #[tokio::test(start_paused = true)]
    async fn each_listed_api_key_has_its_own_count_for_every_telemetry() {
        for telemetry in [Telemetry::Metrics, Telemetry::Spans, Telemetry::Logs] {
            // The request without a key holds no selected item, so the step ignores it.
            let answer = records(vec![
                keyed(request(telemetry, Some(ADP), 3, 1), KEY_ONE),
                keyed(request(telemetry, Some(ADP), 2, 0), KEY_TWO),
                request(telemetry, Some(ADP), 0, 2),
            ]);
            let keys = format!("{{'{}': 3, '{}': 2}}", KEY_ONE, KEY_TWO);
            let outcome = watch(&keyed_step(telemetry, &keys), vec![answer]).await;
            assert_status(&outcome, StepStatus::Passed);
            assert_mentions(
                &outcome,
                &["5 selected item(s) reached 'intake' from adp with the listed API keys and from no other sender"],
            );
            assert_eq!(outcome.elapsed, Duration::from_secs(2), "{}", telemetry.name());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_listed_api_key_with_more_or_fewer_items_than_its_count_fails() {
        let answer = records(vec![
            keyed(request(Telemetry::Spans, Some(ADP), 3, 0), KEY_ONE),
            keyed(request(Telemetry::Spans, Some(ADP), 3, 0), KEY_TWO),
        ]);
        let keys = |second| format!("{{'{}': 3, '{}': {}}}", KEY_ONE, KEY_TWO, second);

        let outcome = watch(&keyed_step(Telemetry::Spans, &keys(2)), vec![answer.clone()]).await;
        assert_status(&outcome, StepStatus::Failed);
        assert_mentions(
            &outcome,
            &[
                "3 selected item(s) reached 'intake' from adp with key 2, and the step expects exactly 2.",
                "Expected API keys: key 1 exactly 3, key 2 exactly 2.",
                "Items by API key: key 1 3, key 2 3.",
            ],
        );
        assert_eq!(outcome.elapsed, Duration::ZERO);

        let outcome = watch(&keyed_step(Telemetry::Spans, &keys(4)), vec![answer]).await;
        assert_status(&outcome, StepStatus::Failed);
        assert_mentions(
            &outcome,
            &["3 selected item(s) reached 'intake' from adp with key 2 within 10s, and the step expects exactly 4."],
        );
        assert_eq!(outcome.elapsed, Duration::from_secs(10));
    }

    #[tokio::test(start_paused = true)]
    async fn a_list_of_api_keys_expects_at_least_one_item_of_each_for_every_telemetry() {
        let keys = format!("['{}', '{}']", KEY_ONE, KEY_TWO);
        for telemetry in TELEMETRIES {
            let both = records(vec![
                keyed(request(telemetry, Some(ADP), 2, 0), KEY_ONE),
                keyed(request(telemetry, Some(ADP), 1, 0), KEY_TWO),
            ]);
            let outcome = watch(&keyed_step(telemetry, &keys), vec![both]).await;
            assert_status(&outcome, StepStatus::Passed);
            assert_eq!(outcome.elapsed, Duration::from_secs(2), "{}", telemetry.name());

            let first = records(vec![keyed(request(telemetry, Some(ADP), 2, 0), KEY_ONE)]);
            let outcome = watch(&keyed_step(telemetry, &keys), vec![first]).await;
            assert_status(&outcome, StepStatus::Failed);
            assert_mentions(
                &outcome,
                &["0 selected item(s) reached 'intake' from adp with key 2 within 10s, and the step expects at least 1."],
            );
            assert_eq!(outcome.elapsed, Duration::from_secs(10), "{}", telemetry.name());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_item_with_an_unlisted_api_key_fails_at_once_for_every_telemetry() {
        for telemetry in TELEMETRIES {
            // The step lists the first key, which delivers what it expects. A second key delivers one more item.
            let answer = records(vec![
                keyed(request(telemetry, Some(ADP), 3, 0), KEY_ONE),
                keyed(request(telemetry, Some(ADP), 1, 0), KEY_TWO),
            ]);
            // Statistics take no count, so a list stands for them.
            let keys = if telemetry == Telemetry::TraceStats {
                format!("['{}']", KEY_ONE)
            } else {
                counted_keys(3)
            };
            let outcome = watch(&keyed_step(telemetry, &keys), vec![answer]).await;
            assert_status(&outcome, StepStatus::Failed);
            assert_mentions(
                &outcome,
                &[
                    "with an API key that the step does not list, or with no key.",
                    "Items by API key: key 1 3, unlisted 1.",
                    "; API key: key 1;",
                    "; API key: unlisted;",
                ],
            );
            assert_eq!(outcome.elapsed, Duration::ZERO, "{}", telemetry.name());
            assert_eq!(outcome.reads, 1, "{}", telemetry.name());
            let [(_, kept)] = &outcome.kept[..] else {
                panic!("{:?}", outcome.kept)
            };
            assert!(kept.contains("; API key: unlisted;"), "{}", kept);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_request_without_an_api_key_fails_a_step_that_lists_keys() {
        let answer = records(vec![
            keyed(request(Telemetry::Spans, Some(ADP), 3, 0), KEY_ONE),
            request(Telemetry::Spans, Some(ADP), 1, 0),
        ]);
        let outcome = watch(&keyed_step(Telemetry::Spans, &counted_keys(3)), vec![answer]).await;
        assert_status(&outcome, StepStatus::Failed);
        assert_mentions(
            &outcome,
            &["does not list, or with no key", "Items by API key: key 1 3, none 1."],
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_item_with_an_unlisted_api_key_that_arrives_during_the_settle_fails() {
        let held = || records(vec![keyed(request(Telemetry::Spans, Some(ADP), 3, 0), KEY_ONE)]);
        let stray = records(vec![
            keyed(request(Telemetry::Spans, Some(ADP), 3, 0), KEY_ONE),
            keyed(request(Telemetry::Spans, Some(ADP), 1, 0), KEY_TWO),
        ]);
        let outcome = watch(
            &keyed_step(Telemetry::Spans, &counted_keys(3)),
            vec![held(), held(), stray],
        )
        .await;
        assert_status(&outcome, StepStatus::Failed);
        assert_mentions(
            &outcome,
            &["does not list", "after the result first held, within the settle of 2s"],
        );
        assert!(outcome.elapsed < Duration::from_secs(2), "{:?}", outcome.elapsed);
    }

    #[tokio::test(start_paused = true)]
    async fn a_listed_api_key_does_not_excuse_another_sender() {
        let answer = records(vec![
            keyed(request(Telemetry::Spans, Some(ADP), 3, 0), KEY_ONE),
            keyed(request(Telemetry::Spans, Some(AGENT), 1, 0), KEY_ONE),
        ]);
        let outcome = watch(&keyed_step(Telemetry::Spans, &counted_keys(3)), vec![answer]).await;
        assert_status(&outcome, StepStatus::Failed);
        assert_mentions(
            &outcome,
            &["from a sender other than adp", "Items by sender: adp 3, agent 1."],
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_key_of_a_request_is_matched_exactly_and_by_a_header_name_in_any_case() {
        let key = "abababababababababababababababab";
        let keys = format!("{{'{}': 3}}", key);
        let mut upper = request(Telemetry::Spans, Some(ADP), 3, 0);
        upper["headers"]["DD-API-KEY"] = json!(key);
        let outcome = watch(&keyed_step(Telemetry::Spans, &keys), vec![records(vec![upper])]).await;
        assert_status(&outcome, StepStatus::Passed);

        for near in [
            key.to_uppercase(),
            format!("{} ", key),
            key[1..].to_string(),
            format!("{}b", key),
        ] {
            let answer = records(vec![keyed(request(Telemetry::Spans, Some(ADP), 3, 0), &near)]);
            let outcome = watch(&keyed_step(Telemetry::Spans, &keys), vec![answer]).await;
            assert_status(&outcome, StepStatus::Failed);
            assert_mentions(&outcome, &["Items by API key: unlisted 3."]);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_step_without_api_keys_accepts_every_key_and_no_key() {
        let answer = records(vec![
            keyed(request(Telemetry::Spans, Some(ADP), 1, 0), KEY_ONE),
            keyed(request(Telemetry::Spans, Some(ADP), 1, 0), KEY_TWO),
            request(Telemetry::Spans, Some(ADP), 1, 0),
        ]);
        let outcome = watch(&adp_step(Telemetry::Spans), vec![answer]).await;
        assert_status(&outcome, StepStatus::Passed);
        assert!(
            !outcome.report.message.contains("API key"),
            "{}",
            outcome.report.message
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_api_key_is_in_no_message_kept_file_or_debug_form() {
        let one = |telemetry| records(vec![keyed(request(telemetry, Some(ADP), 4, 0), KEY_ONE)]);
        let both = |telemetry| {
            records(vec![
                keyed(request(telemetry, Some(ADP), 3, 0), KEY_ONE),
                keyed(request(telemetry, Some(ADP), 1, 0), KEY_TWO),
            ])
        };
        let counted = keyed_step(Telemetry::Spans, &counted_keys(3));
        let listed = keyed_step(Telemetry::TraceStats, &format!("['{}']", KEY_ONE));
        let runs = [
            (&counted, one(Telemetry::Spans)),
            (&counted, both(Telemetry::Spans)),
            (&listed, both(Telemetry::TraceStats)),
        ];
        for (step, answer) in runs {
            let outcome = watch(step, vec![answer]).await;
            assert_status(&outcome, StepStatus::Failed);
            assert_mentions(&outcome, &["Expected API keys: key 1", "dd-api-key: <redacted>"]);
            let [(_, kept)] = &outcome.kept[..] else {
                panic!("{:?}", outcome.kept)
            };
            assert!(kept.contains("dd-api-key: <redacted>"), "{}", kept);
            for key in [KEY_ONE, KEY_TWO] {
                for shown in [&outcome.report.message, kept, &format!("{:?}", step)] {
                    assert!(!shown.contains(key), "the key is in: {}", shown);
                }
            }
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
                "'count' is not an option for trace_stats, except 0",
            ),
            (
                format!("on: i, telemetry: logs, count: -1, {}", select),
                "invalid value: integer `-1`",
            ),
            (
                format!("on: i, telemetry: logs, count: 0, {}", select),
                "'sender' is not an option with 'count: 0'",
            ),
            (
                "on: i, telemetry: logs, select: {service: fx}, timeout: 1m".to_string(),
                "'sender' is required, except with 'count: 0'",
            ),
            (
                "on: i, telemetry: logs, select: {service: fx}, count: 0, timeout: 1m".to_string(),
                "'settle' is required with 'count: 0'",
            ),
            (
                "on: i, telemetry: logs, select: {service: fx}, count: 0, path: /a, settle: 1s, timeout: 1m"
                    .to_string(),
                "'path' is not an option with 'count: 0'",
            ),
            (
                format!("on: i, telemetry: logs, api_key: k, {}", select),
                "unknown field `api_key`",
            ),
            (
                format!("on: i, telemetry: logs, api_keys: k, {}", select),
                "a list of API keys, or a map from each API key to its number of items",
            ),
            (
                format!("on: i, telemetry: logs, api_keys: [], {}", select),
                "'api_keys' is empty",
            ),
            (
                format!("on: i, telemetry: logs, api_keys: {{}}, {}", select),
                "'api_keys' is empty",
            ),
            (
                format!("on: i, telemetry: logs, api_keys: [''], {}", select),
                "'api_keys' holds an empty key",
            ),
            (
                format!("on: i, telemetry: logs, api_keys: {{'': 1}}, {}", select),
                "'api_keys' holds an empty key",
            ),
            (
                format!("on: i, telemetry: logs, api_keys: [k, k], {}", select),
                "'api_keys' lists a key twice",
            ),
            (
                format!("on: i, telemetry: logs, api_keys: {{k: 0}}, {}", select),
                "'api_keys' gives a key a count of 0",
            ),
            (
                format!("on: i, telemetry: logs, api_keys: {{k: 1}}, count: 1, {}", select),
                "'api_keys' is not an option with 'count'",
            ),
            (
                format!("on: i, telemetry: logs, api_keys: [k], count: 1, {}", select),
                "'api_keys' is not an option with 'count'",
            ),
            (
                "on: i, telemetry: logs, select: {service: fx}, api_keys: [k], count: 0, settle: 1s, timeout: 1m"
                    .to_string(),
                "'api_keys' is not an option with 'count: 0'",
            ),
            (
                format!("on: i, telemetry: trace_stats, api_keys: {{k: 1}}, {}", select),
                "'api_keys' is a list for trace_stats",
            ),
            (
                format!("on: i, telemetry: logs, settle: 0s, {}", select),
                "needs a whole number above zero",
            ),
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
            let fields = StepFields::of(serde_saphyr::from_str(&format!("{{{}}}", fields)).unwrap());
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
  elsewhere:
    type: datadog-intake
  sender:
    subject: true
    image: alpine:3.20
procedure:
  - {step: intake_from, on: intake, telemetry: metrics, select: {name_prefix: "fx."}, sender: adp, count: 3, settle: 1ms, timeout: 1ms}
  - {step: intake_from, on: intake, telemetry: spans, select: {service: fx}, sender: adp, settle: 1ms, timeout: 1ms}
  - {step: intake_from, on: intake, telemetry: logs, select: {service: fx}, sender: adp, settle: 1ms, timeout: 1ms}
  - {step: intake_from, on: elsewhere, telemetry: trace_stats, select: {service: fx}, sender: adp, settle: 1ms, timeout: 1ms}
  - {step: intake_from, on: intake, telemetry: trace_stats, select: {service: fx}, count: 0, settle: 1ms, timeout: 1ms}
"#;

    #[tokio::test]
    async fn the_planned_step_reads_an_intake_through_its_published_port() {
        let plan = plan_case(CASE).unwrap_or_else(|errors| panic!("{}", errors.join("\n")));
        let planned: Vec<_> = plan.procedure.iter().flat_map(|node| node.steps()).collect();
        for step in &planned {
            let on = step.fields.0["on"].as_str().unwrap();
            assert_eq!(
                step.needs,
                [Need::Endpoint(
                    on.to_string(),
                    API.to_string(),
                    EndpointOperation::Request
                )]
            );
        }
        // The step on `elsewhere` only shows the plan that the delivery of the statistics happens. The test reads
        // `intake` alone.
        let steps: Vec<_> = planned
            .into_iter()
            .filter(|step| step.fields.0["on"] == "intake")
            .collect();

        // The client of the handle needs the process-wide crypto provider, which `main` installs at startup.
        let _ = crate::default_crypto_provider().install_default();
        let port = serve(BTreeMap::from([
            (
                "/metrics/dump_requests",
                (200, json!([request(Telemetry::Metrics, Some(ADP), 3, 1)]).to_string()),
            ),
            ("/traces/dump_span_requests", (500, "broken".to_string())),
            ("/traces/dump_stats_requests", (200, "[]".to_string())),
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
            [
                StepStatus::Passed,
                StepStatus::Failed,
                StepStatus::Failed,
                StepStatus::Passed
            ],
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

    /// A case with two intakes, whose procedure is `procedure`.
    fn presumption_case(procedure: &str) -> String {
        format!(
            "timeout: 5m\nsubject_os: [linux]\nparticipants:\n  here: {{type: datadog-intake}}\n  there: {{type: \
             datadog-intake}}\n  sender: {{subject: true, image: 'alpine:3.20'}}\nprocedure:\n{}",
            procedure
        )
    }

    /// A step that expects the fixtures of `telemetry` from ADP at `on`.
    fn delivery(on: &str, telemetry: Telemetry) -> String {
        format!(
            "{{step: intake_from, on: {}, telemetry: {}, select: {}, sender: adp, settle: 1ms, timeout: 1ms}}",
            on,
            telemetry.name(),
            selection(telemetry)
        )
    }

    /// A step that expects none of the fixtures of `telemetry` at `on`.
    fn absence(on: &str, telemetry: Telemetry) -> String {
        format!(
            "{{step: intake_from, on: {}, telemetry: {}, select: {}, count: 0, settle: 1ms, timeout: 1ms}}",
            on,
            telemetry.name(),
            selection(telemetry)
        )
    }

    #[test]
    fn an_absence_plans_after_a_delivery_of_the_same_items_in_an_earlier_entry() {
        for telemetry in TELEMETRIES {
            let (delivered, absent) = (delivery("here", telemetry), absence("there", telemetry));
            let procedures = [
                format!("  - {}\n  - {}\n", delivered, absent),
                format!("  - parallel:\n      - {}\n  - {}\n", delivered, absent),
                format!("  - {}\n  - parallel:\n      - {}\n", delivered, absent),
                // A step that expects the items from a sender establishes them wherever it is. The absence at the same
                // intake would fail when it runs, but it does not hold vacuously.
                format!("  - {}\n  - {}\n", delivered, absence("here", telemetry)),
            ];
            for procedure in procedures {
                if let Err(errors) = plan_case(&presumption_case(&procedure)) {
                    panic!("{}: {}", telemetry.name(), errors.join("\n"));
                }
            }
        }
    }

    #[test]
    fn an_absence_of_items_that_no_earlier_step_expects_does_not_plan() {
        let spans = delivery("here", Telemetry::Spans);
        let typo = absence("there", Telemetry::Spans).replace("service: fx", "service: fx-typo");
        let prefix = absence("there", Telemetry::Metrics).replace("'fx.'", "'fx.a.'");
        let cases = [
            // The select differs.
            (
                format!("  - {}\n  - {}\n", spans, typo),
                "step 2",
                "spans of service 'fx-typo' reach an intake",
            ),
            (
                format!("  - {}\n  - {}\n", delivery("here", Telemetry::Metrics), prefix),
                "step 2",
                "metric data points with a name that starts with 'fx.a.' reach an intake",
            ),
            // The telemetry differs.
            (
                format!("  - {}\n  - {}\n", spans, absence("there", Telemetry::Logs)),
                "step 2",
                "logs of service 'fx' reach an intake",
            ),
            (
                format!("  - {}\n  - {}\n", spans, absence("there", Telemetry::TraceStats)),
                "step 2",
                "groups of trace statistics of service 'fx' reach an intake",
            ),
            // No step comes before it.
            (
                format!("  - {}\n  - {}\n", absence("there", Telemetry::Spans), spans),
                "step 1",
                "spans of service 'fx' reach an intake",
            ),
            // The steps of one entry run together, so the delivery does not come before the absence.
            (
                format!(
                    "  - parallel:\n      - {}\n      - {}\n",
                    spans,
                    absence("there", Telemetry::Spans)
                ),
                "step 1.2",
                "spans of service 'fx' reach an intake",
            ),
            (
                format!(
                    "  - parallel:\n      - {}\n      - {}\n",
                    absence("there", Telemetry::Spans),
                    spans
                ),
                "step 1.1",
                "spans of service 'fx' reach an intake",
            ),
            // An absence shows no delivery.
            (
                format!(
                    "  - {}\n  - {}\n",
                    absence("here", Telemetry::Spans),
                    absence("there", Telemetry::Spans)
                ),
                "step 1",
                "spans of service 'fx' reach an intake",
            ),
        ];
        for (procedure, at, claim) in cases {
            let errors = plan_case(&presumption_case(&procedure)).unwrap_err();
            let message = format!(
                "intake_from: the step presumes that {}, and no step in an earlier procedure entry shows it",
                claim
            );
            assert!(
                errors
                    .iter()
                    .any(|error| error.contains(&format!(", {}: {}", at, message))),
                "'{}' at {} is not in:\n{}\nfor:\n{}",
                message,
                at,
                errors.join("\n"),
                procedure
            );
        }
    }

    #[test]
    fn a_step_that_does_not_build_does_not_make_an_absence_fail_the_plan_as_well() {
        // The delivery has no `sender`, so it does not build and cannot establish anything.
        let broken = delivery("here", Telemetry::Spans).replace("sender: adp, ", "");
        let procedure = format!("  - {}\n  - {}\n", broken, absence("there", Telemetry::Spans));
        let errors = plan_case(&presumption_case(&procedure)).unwrap_err();
        assert!(
            errors.iter().any(|error| error.contains("'sender' is required")),
            "{:#?}",
            errors
        );
        assert!(!errors.iter().any(|error| error.contains("presumes")), "{:#?}", errors);
    }
}
