//! The client's observability telemetry.
//!
//! Subscribers see only their snapshots and their own decoding errors, so every other failure (a poll that fails, a
//! response that cannot be applied, a product whose assignment never builds) is visible only through these metrics
//! and the worker's logs.

use saluki_metrics::MetricsBuilder;

/// Counts completed polls, tagged by how they ended.
const POLLS_TOTAL: &str = "remote_config_polls_total";

/// Counts rejected configurations, tagged by the product and by which rule rejected them.
const CONFIGURATIONS_REJECTED_TOTAL: &str = "remote_config_configurations_rejected_total";

/// Counts published snapshots, tagged by the product.
const SNAPSHOTS_PUBLISHED_TOTAL: &str = "remote_config_snapshots_published_total";

/// Measures how long the client has gone without a successful poll, in seconds.
const SECONDS_SINCE_SUCCESSFUL_POLL: &str = "remote_config_seconds_since_last_successful_poll";

/// How one completed poll ended.
///
/// The variants are the `outcome` tag of the polls counter. Every poll ends in exactly one of them: a response the
/// client could not apply, including a payload that fails its hash, counts as an invalid response rather than a
/// success.
#[derive(Clone, Copy)]
pub(crate) enum PollOutcome {
    /// The Agent answered a response the client applied.
    Ok,

    /// The Agent answered but reported its configuration expired, which withdraws every assignment.
    ///
    /// The poll itself succeeded, so it does not count as a failure of the transport.
    Expired,

    /// The RPC failed, including a poll the Agent did not answer within the request timeout.
    RpcError,

    /// The Agent does not implement Remote Configuration or has it disabled.
    Unimplemented,

    /// The Agent answered a response the client could not apply.
    InvalidResponse,
}

impl PollOutcome {
    fn tag(&self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Expired => "expired",
            Self::RpcError => "rpc_error",
            Self::Unimplemented => "unimplemented",
            Self::InvalidResponse => "invalid_response",
        }
    }
}

/// Which of the client's rules rejected a configuration.
///
/// The variants are the `stage` tag of the rejected-configurations counter. The hash check that aborts a whole poll is
/// not a stage: it rejects no configuration, it counts as a poll outcome.
#[derive(Clone, Copy)]
pub(crate) enum RejectionStage {
    /// The product's decoder rejected this configuration's payload.
    Decode,

    /// The configuration decoded, but the product's snapshot as a whole failed to build.
    Build,

    /// The product's decoder panicked, which rejects its whole assignment.
    ///
    /// A panic over an empty assignment rejects no configuration but still counts once, so every panic is counted.
    Panic,

    /// This configuration shares its ID with another, so none of them reached a decoder.
    Collision,
}

impl RejectionStage {
    fn tag(&self) -> &'static str {
        match self {
            Self::Decode => "decode",
            Self::Build => "build",
            Self::Panic => "panic",
            Self::Collision => "collision",
        }
    }
}

/// The client's metrics.
///
/// The `product` tag is bounded to the subscribed products, so the counters it tags cannot grow without bound while
/// the client runs.
pub(crate) struct Metrics(MetricsBuilder);

impl Metrics {
    pub(crate) fn new() -> Self {
        Self(MetricsBuilder::default())
    }

    /// Counts one completed poll.
    pub(crate) fn count_poll(&self, outcome: PollOutcome) {
        self.0
            .register_counter_with_tags(POLLS_TOTAL, [("outcome", outcome.tag())])
            .increment(1);
    }

    /// Counts `count` configurations rejected at `stage` while evaluating `product`.
    pub(crate) fn count_rejected(&self, product: &str, stage: RejectionStage, count: u64) {
        self.0
            .register_counter_with_tags(
                CONFIGURATIONS_REJECTED_TOTAL,
                [("product", product.to_owned()), ("stage", stage.tag().to_owned())],
            )
            .increment(count);
    }

    /// Counts one snapshot published to a product's subscriptions.
    pub(crate) fn count_snapshot_published(&self, product: &str) {
        self.0
            .register_counter_with_tags(SNAPSHOTS_PUBLISHED_TOTAL, [("product", product.to_owned())])
            .increment(1);
    }

    /// Records how long the client has gone without a successful poll.
    ///
    /// Until the first success, `seconds` measures from when the worker started, which is how long the client has been
    /// without one.
    pub(crate) fn set_seconds_since_successful_poll(&self, seconds: u64) {
        self.0.register_gauge(SECONDS_SINCE_SUCCESSFUL_POLL).set(seconds as f64);
    }
}
